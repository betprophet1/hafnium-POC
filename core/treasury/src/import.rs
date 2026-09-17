//! `venue-treasury import`: moves a book kept in the append-only JSON-lines files of earlier versions (the ledger file
//! and the bank's `.bank.jsonl` beside it) into the records, once, so a venue can migrate without losing its money
//! of record. Every line becomes what the records would have held had they been written then: each state a transfer
//! was in is a transfer event, each posting keeps its memo and idempotency key, breaks keep their numbers and runs
//! their reports. The records must hold no book yet.
//!
//! The file format is read here and nowhere else: the treasury no longer writes it.

use crate::bank::Receipt;
use crate::clock;
use crate::ledger::Posting;
use crate::recon::{Break, Resolution};
use crate::records::Records;
use crate::transfer::{Kind, Method, Reason, Stage, State, Transfer};
use serde_json::Value;
use std::path::Path;

/// What an import moved.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Imported {
    pub nonce: u32,
    pub transfer_lines: usize,
    pub postings: usize,
    pub breaks: usize,
    pub runs: usize,
    pub receipts: usize,
}

/// Every whole line of a JSON-lines file; a last line cut short by a stop mid-write never happened.
fn read_lines(path: &Path) -> Result<Vec<Value>, String> {
    let text = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let whole = text.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    if whole < text.len() {
        eprintln!("import: {}: ignoring a last line cut short ({} bytes)", path.display(), text.len() - whole);
    }
    text[..whole]
        .split(|b| *b == b'\n')
        .enumerate()
        .filter(|(_, l)| !l.is_empty())
        .map(|(n, line)| serde_json::from_slice(line).map_err(|e| format!("{} line {}: {e}", path.display(), n + 1)))
        .collect()
}

fn stage(v: &Value) -> Result<Stage, String> {
    Ok(match v["at"].as_str().unwrap_or("") {
        "reclaiming" => Stage::Reclaiming,
        "to_bank" => Stage::ToBank,
        "at_bank" => Stage::AtBank { due: v["due"].as_u64().ok_or("an at_bank stage needs due")? },
        "granting" => Stage::Granting {
            then: v["then"].as_str().and_then(State::parse).ok_or("a granting stage needs then")?,
            reason: v["reason"].as_str().and_then(Reason::parse),
        },
        "done" => Stage::Done,
        other => return Err(format!("unknown stage {other:?}")),
    })
}

fn transfer(v: &Value) -> Result<Transfer, String> {
    let number = |name: &str| v[name].as_u64().ok_or_else(|| format!("a transfer record needs {name}: {v}"));
    Ok(Transfer {
        id: number("id")?,
        account: number("account")?,
        client_id: v["clientTransferId"].as_str().ok_or("a transfer record needs clientTransferId")?.to_string(),
        kind: v["kind"].as_str().and_then(Kind::parse).ok_or("a transfer record needs kind")?,
        amount: number("amountCents")?,
        method: v["method"].as_str().and_then(Method::parse).ok_or("a transfer record needs method")?,
        state: v["state"].as_str().and_then(State::parse).ok_or("a transfer record needs state")?,
        reason: v["reason"].as_str().and_then(Reason::parse),
        sequence: v["sequence"].as_u64(),
        created: number("created")?,
        updated: number("updated")?,
        stage: stage(&v["stage"])?,
        in_doubt: v["inDoubt"].as_u64(),
    })
}

fn postings(v: &Value) -> Result<Vec<(String, Posting)>, String> {
    v.as_array()
        .into_iter()
        .flatten()
        .map(|p| Ok((p["memo"].as_str().unwrap_or("").to_string(), Posting::from_json(&p["entries"])?)))
        .collect()
}

fn found_break(v: &Value) -> Result<Break, String> {
    let number = |name: &str| v[name].as_u64().ok_or_else(|| format!("a break record needs {name}: {v}"));
    let r = &v["resolution"];
    Ok(Break {
        id: number("id")?,
        kind: v["kind"].as_str().ok_or("a break record needs kind")?.to_string(),
        transfer: v["transfer"].as_u64(),
        account: v["account"].as_u64(),
        detail: v["detail"].as_str().unwrap_or("").to_string(),
        found_at: number("foundAt")?,
        found_by: number("foundBy")?,
        resolution: (!r.is_null()).then(|| Resolution {
            at: r["at"].as_u64().unwrap_or(0),
            by: r["by"].as_str().unwrap_or("").to_string(),
            note: r["note"].as_str().unwrap_or("").to_string(),
        }),
    })
}

fn receipt(v: &Value) -> Result<Receipt, String> {
    let number = |name: &str| v[name].as_u64().ok_or_else(|| format!("a bank receipt needs {name}: {v}"));
    Ok(Receipt {
        transfer: number("transfer")?,
        account: number("account")?,
        kind: v["kind"].as_str().and_then(Kind::parse).ok_or("a bank receipt needs kind")?,
        amount: number("amountCents")?,
        received: number("received")?,
        decided: number("decided")?,
        settles: v["settles"].as_bool().ok_or("a bank receipt needs settles")?,
        cancelled: None,
        arrivals: v["arrivals"]
            .as_array()
            .ok_or("a bank receipt needs arrivals")?
            .iter()
            .map(|a| a.as_u64().ok_or("an arrival is a time"))
            .collect::<Result<_, _>>()?,
    })
}

/// Imports the book at `ledger`, and the bank's book beside it if there is one, into `records`.
pub fn import(ledger: &Path, records: &dyn Records) -> Result<Imported, String> {
    if records.load(0)?.is_some() || records.load_bank()?.is_some() {
        return Err("the records already hold a treasury book: an import only goes into empty ones".into());
    }
    let mut lines = read_lines(ledger)?.into_iter().peekable();
    let first = lines.next().ok_or_else(|| format!("{} is empty", ledger.display()))?;
    let nonce = first["book"]["nonce"].as_u64().ok_or("the ledger's first line must name its book")?;
    let nonce = u32::try_from(nonce).map_err(|_| "a book nonce out of range")?;
    let created = first["book"]["created"].as_u64().unwrap_or(0);
    let mut imported = Imported { nonce, ..Imported::default() };
    // The opening balances are the line after the book's, with postings and no transfer.
    let opening = match lines.peek() {
        Some(r) if r["transfer"].is_null() && !r["postings"].is_null() => {
            postings(&lines.next().unwrap_or_default()["postings"])?
        }
        _ => Vec::new(),
    };
    let with_memos: Vec<(&str, Posting)> = opening.iter().map(|(m, p)| (m.as_str(), p.clone())).collect();
    records.create(nonce, created, &with_memos)?;
    imported.postings += opening.len();
    let mut last = created;
    for (n, r) in lines.enumerate() {
        let at = |e: String| format!("{} record {}: {e}", ledger.display(), n + 3);
        if !r["transfer"].is_null() {
            let t = transfer(&r["transfer"]).map_err(at)?;
            let moved = postings(&r["postings"]).map_err(at)?;
            let with_memos: Vec<(&str, Posting)> = moved.iter().map(|(m, p)| (m.as_str(), p.clone())).collect();
            last = last.max(t.updated);
            records.commit(&t, &with_memos, t.updated).map_err(|e| at(e.into()))?;
            imported.transfer_lines += 1;
            imported.postings += moved.len();
        } else if !r["postings"].is_null() {
            return Err(at("postings without a transfer after the opening balances".into()));
        }
        if !r["break"].is_null() {
            let b = found_break(&r["break"]).map_err(at)?;
            records.record_break(&b).map_err(|e| at(e.into()))?;
            if b.resolution.is_none() {
                imported.breaks += 1;
            }
        }
        if let Some(run) = r["run"]["runId"].as_str().and_then(|id| id.parse::<u64>().ok()) {
            let report = &r["run"];
            let when = report["at"].as_str().and_then(clock::parse_rfc3339).unwrap_or(last);
            let status = report["status"].as_str().unwrap_or("clean");
            records.record_run(run, status, report, when).map_err(|e| at(e.into()))?;
            imported.runs += 1;
        }
    }

    let bank = ledger.with_extension("bank.jsonl");
    if bank.exists() {
        for (n, r) in read_lines(&bank)?.into_iter().enumerate() {
            let at = |e: String| format!("{} line {}: {e}", bank.display(), n + 1);
            if let Some(opening) = r["bank"]["openingCents"].as_i64() {
                if r["bank"]["treasuryBook"].as_u64() != Some(u64::from(nonce)) {
                    return Err(at("the bank's book belongs to another treasury book".into()));
                }
                let opened = r["bank"]["opened"].as_u64().unwrap_or(created);
                records.open_bank(opening, opened, nonce).map_err(|e| at(e.into()))?;
            } else if !r["receipt"].is_null() {
                let receipt = receipt(&r["receipt"]).map_err(at)?;
                records.receive(&receipt).map_err(|e| at(e.into()))?;
                if let Some(when) = r["receipt"]["cancelled"].as_u64() {
                    records.cancel(receipt.transfer, when).map_err(|e| at(e.into()))?;
                }
                imported.receipts += 1;
            } else if let Some(id) = r["cancelled"].as_u64() {
                records.cancel(id, r["at"].as_u64().unwrap_or(last)).map_err(|e| at(e.into()))?;
            } else if let Some(id) = r["sent"].as_u64() {
                let copies = u8::try_from(r["copies"].as_u64().unwrap_or(0)).unwrap_or(u8::MAX);
                // The file kept how many copies were sent, not when: each is taken to have gone when it was due.
                let arrivals = records.load_bank()?.and_then(|b| b.receipts.into_iter().find(|x| x.transfer == id));
                for copy in 0..copies {
                    let when =
                        arrivals.as_ref().and_then(|x| x.arrivals.get(usize::from(copy)).copied()).unwrap_or(last);
                    records.sent(id, copy, when).map_err(|e| at(e.into()))?;
                }
            }
        }
    }
    let (balances, _) = records.trial_balance()?;
    let total: i128 = balances.values().map(|b| i128::from(*b)).sum();
    if total != 0 {
        return Err(format!("the imported ledger sums to {total} cents, not 0: the file was not a whole book"));
    }
    Ok(imported)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::records::Memory;

    #[test]
    fn a_book_of_lines_is_imported_whole_and_only_into_empty_records() {
        let dir = std::env::temp_dir().join(format!("venue-treasury-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a directory");
        let ledger = dir.join("ledger.jsonl");
        std::fs::write(&ledger, SAMPLE_BOOK).expect("written");
        std::fs::write(dir.join("ledger.bank.jsonl"), SAMPLE_BANK).expect("written");
        let records = Memory::default();
        let imported = import(&ledger, &records).expect("imported");
        assert_eq!(
            imported,
            Imported { nonce: 7, transfer_lines: 3, postings: 3, breaks: 1, runs: 1, receipts: 1 },
            "{imported:?}"
        );
        let loaded = records.load(10).expect("readable").expect("a book");
        assert_eq!(loaded.balances["customer:2:cash"], -50_700);
        assert_eq!(loaded.transfers[0].state, State::Settled);
        assert!(loaded.breaks[0].resolution.is_some());
        assert_eq!(loaded.next_run, 2);
        let bank = records.load_bank().expect("readable").expect("a bank");
        assert_eq!((bank.opening, bank.sent.get(&1)), (50_000, Some(&1)));
        assert!(import(&ledger, &records).is_err(), "imported twice");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A book as earlier versions wrote it: the book, the opening balance, one deposit's three states, a break found
    /// and resolved, and a run.
    pub(crate) const SAMPLE_BOOK: &str = r#"{"book":{"created":1000,"nonce":7}}
{"postings":[{"entries":[["bank:fbo_omnibus",50000],["customer:2:cash",-50000]],"memo":"opening balance"}]}
{"postings":[],"transfer":{"account":2,"amountCents":700,"clientTransferId":"d1","created":1000,"id":1,"inDoubt":null,"kind":"deposit","method":"dummy_bank","reason":null,"sequence":null,"stage":{"at":"to_bank"},"state":"pending","updated":1000}}
{"postings":[{"entries":[["bank:fbo_omnibus",700],["transit:deposits",-700]],"memo":"deposit received at the bank"}],"transfer":{"account":2,"amountCents":700,"clientTransferId":"d1","created":1000,"id":1,"inDoubt":null,"kind":"deposit","method":"dummy_bank","reason":null,"sequence":null,"stage":{"at":"granting","reason":null,"then":"settled"},"state":"pending","updated":1000}}
{"postings":[{"entries":[["transit:deposits",700],["customer:2:cash",-700]],"memo":"deposit granted to the sleeve"}],"transfer":{"account":2,"amountCents":700,"clientTransferId":"d1","created":1000,"id":1,"inDoubt":null,"kind":"deposit","method":"dummy_bank","reason":null,"sequence":42,"stage":{"at":"done"},"state":"settled","updated":2000}}
{"break":{"account":2,"detail":"late","foundAt":2500,"foundBy":1,"id":1,"kind":"bank_late_notice","resolution":null,"transfer":1}}
{"break":{"account":2,"detail":"late","foundAt":2500,"foundBy":1,"id":1,"kind":"bank_late_notice","resolution":{"at":2500,"by":"reconciliation","note":"ignored"},"transfer":1}}
{"run":{"at":"1970-01-01T00:00:02.500Z","runId":"1","status":"resolved"}}
"#;

    pub(crate) const SAMPLE_BANK: &str = r#"{"bank":{"opened":1000,"openingCents":50000,"treasuryBook":7}}
{"receipt":{"account":2,"amountCents":700,"arrivals":[1500],"cancelled":null,"decided":1500,"kind":"deposit","received":1000,"settles":true,"transfer":1}}
{"copies":1,"sent":1}
"#;
}
