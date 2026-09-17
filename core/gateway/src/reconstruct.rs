//! Following the audit log's references into the journal (interfaces/audit/README.md, frames derived from the
//! journal).
//!
//! What the venue told an account about what the engine did — an execution, an account snapshot, a payout, a release
//! — is recorded as what it was, which sequenced input it came from and the hash of the bytes sent, not as the bytes.
//! This renders those frames again from the journal and checks each one against its hash. A frame that does not match
//! means the journal is not the one that gateway was serving, or the renderers no longer render as they did, which is
//! why `core/ci.sh` runs this over a recorded session: a change that would strand old references fails the build
//! instead of being found in a dispute.

use crate::hub::{Hub, Outbox, Queued};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::mpsc;
use std::sync::Arc;
use venue_link::catalogue::Catalogue;
use venue_link::wire::{self, Error};

/// What a frame was, for an account, at one input: the frames the gateway would send are gathered under this, and a
/// reference is looked up by it.
type Frame = (u64, u64, String);

/// One frame the log kept a reference to instead of a copy.
struct Reference {
    file: String,
    line: usize,
    account: u64,
    /// Every session of an account is sent the same frames, so each session is followed through them on its own.
    session: u64,
    sequence: u64,
    kind: String,
    length: usize,
    sha256: String,
}

/// What came of following every reference in `files`.
pub struct Outcome {
    pub checked: usize,
    pub broken: Vec<String>,
}

/// Renders every referenced frame again and checks it against the hash the log kept. `journal` and `results` are a
/// node's journal and the result log of replaying it (`venue-node replay`), which together are what the feed carried;
/// the engine itself stays on its own side of interface 7, and this reads only its files.
pub fn reconstruct(journal: &Path, results: &Path, catalogue: &Path, files: &[String]) -> Result<Outcome, Error> {
    let references = read_references(files)?;
    if references.is_empty() {
        return Ok(Outcome { checked: 0, broken: Vec::new() });
    }
    let accounts: Vec<u64> = {
        let mut seen: Vec<u64> = references.iter().map(|r| r.account).collect();
        seen.sort_unstable();
        seen.dedup();
        seen
    };
    let rendered = render(journal, results, catalogue, &accounts)?;
    // How far each session has been followed through the frames of one input: the file keeps a session's frames in
    // the order they were sent, so the nth reference of a kind at a sequence is the nth frame rendered for it.
    let mut taken: BTreeMap<(u64, Frame), usize> = BTreeMap::new();
    let mut outcome = Outcome { checked: 0, broken: Vec::new() };
    for reference in &references {
        outcome.checked += 1;
        let key = (reference.account, reference.sequence, reference.kind.clone());
        let at = taken.entry((reference.session, key.clone())).or_default();
        let frame = rendered.get(&key).and_then(|frames| frames.get(*at)).cloned();
        *at += 1;
        let Some(frame) = frame else {
            outcome.broken.push(format!(
                "{}: line {}: the journal has no {} for account {} at sequence {}",
                reference.file, reference.line, reference.kind, reference.account, reference.sequence
            ));
            continue;
        };
        let digest = crate::audit::sha256_hex(frame.as_bytes());
        if digest != reference.sha256 || frame.len() != reference.length {
            outcome.broken.push(format!(
                "{}: line {}: the {} rendered for account {} at sequence {} is {} bytes hashing to {}, and the log \
                 says {} bytes hashing to {}",
                reference.file,
                reference.line,
                reference.kind,
                reference.account,
                reference.sequence,
                frame.len(),
                digest,
                reference.length,
                reference.sha256
            ));
        }
    }
    Ok(outcome)
}

/// Every reference in the files, in the order they were written.
fn read_references(files: &[String]) -> Result<Vec<Reference>, Error> {
    let mut out = Vec::new();
    for file in files {
        let text = fs::read_to_string(file)?;
        for (index, line) in text.lines().enumerate() {
            let record: Value = match serde_json::from_str(line) {
                Ok(record) => record,
                Err(e) => return Err(format!("{file}: line {}: {e}", index + 1).into()),
            };
            // A frame recorded by reference: everything else in the file holds what it is about.
            if record["kind"] != "ws-frame" || !record["type"].is_string() {
                continue;
            }
            let number = |v: &Value| v.as_str().and_then(|s| s.parse::<u64>().ok());
            let (Some(account), Some(sequence)) = (number(&record["account"]), number(&record["sequence"])) else {
                return Err(format!("{file}: line {}: a reference without an account or a sequence", index + 1).into());
            };
            out.push(Reference {
                file: file.clone(),
                line: index + 1,
                account,
                session: number(&record["session"]).unwrap_or(0),
                sequence,
                kind: record["type"].as_str().unwrap_or_default().to_string(),
                length: record["length"].as_u64().unwrap_or(0) as usize,
                sha256: record["sha256"].as_str().unwrap_or_default().to_string(),
            });
        }
    }
    Ok(out)
}

/// The frames the gateway would send each of `accounts`, by the account, input and kind they belong to, in the order
/// the hub sent them. A session is opened for every account before the first input, so nothing depends on when the
/// real session happened to be connected: a reference is matched by what it says it is, not by where it sits.
fn render(
    journal: &Path,
    results: &Path,
    catalogue: &Path,
    accounts: &[u64],
) -> Result<BTreeMap<Frame, Vec<String>>, Error> {
    let mut hub = Hub::new(Catalogue::load(&catalogue.display().to_string())?);
    let mut queues = Vec::new();
    for &account in accounts {
        let (tx, rx) = mpsc::channel();
        let key = hub.add_session(Outbox::new(tx, Arc::new(|| {})));
        hub.log_in(key, account);
        queues.push((account, rx));
    }
    for unit in wire::replayed_units(journal, results)? {
        hub.apply(&unit);
    }
    let mut out: BTreeMap<Frame, Vec<String>> = BTreeMap::new();
    for (account, rx) in queues {
        for message in rx.try_iter() {
            let Queued { text, derived } = message;
            if let Some(derived) = derived {
                out.entry((account, derived.sequence, derived.kind.to_string())).or_default().push(text);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{self, eventually, CATALOGUE, MAKER_TOKEN, MARKET};
    use serde_json::json;
    use std::time::Duration;
    use tungstenite::stream::MaybeTlsStream;
    use tungstenite::{Message, WebSocket};

    /// Trades, then renders from the node's own journal every frame the log kept a reference to. This is what makes a
    /// reference worth keeping instead of the frame: a renderer whose output changes fails here, rather than stranding
    /// the references in files that have already been written.
    #[test]
    fn every_reference_renders_again_from_the_journal() {
        let venue = testing::venue("reconstruct");
        let (mut client, _) = tungstenite::connect(&venue.ws_private).expect("a WebSocket");
        if let MaybeTlsStream::Plain(stream) = client.get_ref() {
            stream.set_read_timeout(Some(Duration::from_secs(10))).expect("a read timeout");
        }
        send(&mut client, json!({"op": "login", "id": "l", "token": MAKER_TOKEN}));
        await_type(&mut client, "welcome");
        for (n, tick) in [(1, 4), (2, 5), (3, 6)] {
            send(
                &mut client,
                json!({"op": "order", "id": format!("o{n}"), "clientOrderId": format!("r{n}"),
                       "instrumentId": MARKET.to_string(), "action": "buy", "outcome": "yes",
                       "price": {"tick": tick}, "quantity": 2}),
            );
            await_type(&mut client, "execution");
        }

        // The log is written as the frames go out; wait for the last order's reference to reach the file.
        let files: Vec<String> = eventually_referenced(&venue.audit_dir, 3);
        let results = venue.data_dir.join("replayed.log");
        venue_node::node::replay(&venue.data_dir.join("journal.sbe"), &results).expect("a replay of the journal");
        let outcome =
            reconstruct(&venue.data_dir.join("journal.sbe"), &results, Path::new(CATALOGUE), &files).expect("checked");
        assert!(outcome.checked >= 6, "too few references to be a test: {}", outcome.checked);
        assert!(outcome.broken.is_empty(), "{}", outcome.broken.join("\n"));
    }

    /// A session an earlier build recorded, kept in the repository with the journal it came from: every frame it
    /// derived must still render the same way, byte for byte. The test above cannot make this check — there the same
    /// code writes the log and renders it again — and this is what stands behind keeping a reference rather than the
    /// frame. A renderer whose output changes fails here: raise `audit::RENDERER`, record a new session beside the old
    /// one, and reconstruct each under the rules it names.
    #[test]
    fn a_recorded_session_still_reconstructs() {
        let recorded = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../interfaces/audit/recorded"));
        let results = std::env::temp_dir().join(format!("venue-recorded-{}.log", std::process::id()));
        venue_node::node::replay(&recorded.join("journal.sbe"), &results).expect("a replay of the recorded journal");
        let files = vec![recorded.join("gateway.jsonl").display().to_string()];
        let outcome = reconstruct(&recorded.join("journal.sbe"), &results, Path::new(CATALOGUE), &files);
        let _ = fs::remove_file(&results);
        let outcome = outcome.expect("checked");
        assert!(outcome.checked >= 14, "the recorded session holds too little: {}", outcome.checked);
        assert!(outcome.broken.is_empty(), "{}", outcome.broken.join("\n"));
    }

    fn send(client: &mut WebSocket<MaybeTlsStream<std::net::TcpStream>>, message: serde_json::Value) {
        client.send(Message::text(message.to_string())).expect("sent");
    }

    fn await_type(client: &mut WebSocket<MaybeTlsStream<std::net::TcpStream>>, kind: &str) {
        loop {
            if let Message::Text(text) = client.read().expect("a message") {
                let value: Value = serde_json::from_str(text.as_str()).expect("JSON");
                if value["type"] == kind {
                    return;
                }
            }
        }
    }

    /// The audit files, once they hold at least `wanted` references.
    fn eventually_referenced(dir: &Path, wanted: usize) -> Vec<String> {
        let files = || -> Vec<String> {
            fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
                .map(|p| p.display().to_string())
                .collect()
        };
        eventually("the references reach the audit log", || {
            read_references(&files()).is_ok_and(|found| found.len() >= wanted)
        });
        files()
    }
}
