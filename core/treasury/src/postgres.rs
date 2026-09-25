//! The treasury's records in PostgreSQL: the schema is `migrations/`, and what it enforces is in
//! `core/treasury/README.md` (the ledger) and `core/store/decisions.md`.
//!
//! Double entry is the database's to enforce, not only this code's: a deferred constraint trigger refuses, at commit, a
//! posting that has fewer than two lines or does not sum to zero. Append-only is too: triggers refuse an update,
//! delete or truncate of every table of record, and of a transfer's terms. A transfer's current state is the one row
//! that changes, and each change is also a row of `transfer_events`.

use crate::bank::Receipt;
use crate::ledger::{self, Posting};
use crate::recon::{Break, Resolution};
use crate::records::{posting_key, BankLoaded, Error, Loaded, Records};
use crate::transfer::{Kind, Method, Reason, Stage, State, Transfer};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::SystemTime;
use venue_store::postgres::{Row, Transaction};
use venue_store::{big, millis, time, unsigned, Database, Migration};

/// The treasury's schema, in order. A released migration is never edited: a change is a new one.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: "20260917000001",
        name: "ledger",
        sql: include_str!("../migrations/20260917000001_ledger.sql"),
    },
    Migration {
        version: "20260925000001",
        name: "internal_transfers",
        sql: include_str!("../migrations/20260925000001_internal_transfers.sql"),
    },
];

pub struct Postgres {
    db: Database,
}

impl Postgres {
    pub const fn new(db: Database) -> Self {
        Self { db }
    }
}

fn stage_columns(stage: Stage) -> (&'static str, Option<SystemTime>, Option<&'static str>, Option<&'static str>) {
    match stage {
        Stage::Reclaiming => ("reclaiming", None, None, None),
        Stage::ToBank => ("to_bank", None, None, None),
        Stage::AtBank { due } => ("at_bank", Some(time(due)), None, None),
        Stage::Granting { then, reason } => ("granting", None, Some(then.name()), reason.map(Reason::name)),
        Stage::Done => ("done", None, None, None),
    }
}

fn transfer_from(row: &Row) -> Result<Transfer, Error> {
    let bad = |what: &str| Error::refused(format!("a transfer row with {what}"));
    let text = |name: &str| row.try_get::<_, String>(name).map_err(|e| Error::refused(e.to_string()));
    let stage = match text("stage")?.as_str() {
        "reclaiming" => Stage::Reclaiming,
        "to_bank" => Stage::ToBank,
        "at_bank" => {
            Stage::AtBank { due: millis(row.get::<_, Option<SystemTime>>("due_at").ok_or_else(|| bad("no due_at"))?) }
        }
        "granting" => Stage::Granting {
            then: row
                .get::<_, Option<String>>("then_state")
                .as_deref()
                .and_then(State::parse)
                .ok_or_else(|| bad("no then_state"))?,
            reason: row.get::<_, Option<String>>("then_reason").as_deref().and_then(Reason::parse),
        },
        "done" => Stage::Done,
        other => return Err(bad(&format!("stage {other}"))),
    };
    Ok(Transfer {
        id: unsigned(row.get("transfer_id"))?,
        account: unsigned(row.get("account_id"))?,
        client_id: text("client_transfer_id")?,
        kind: Kind::parse(&text("kind")?).ok_or_else(|| bad("an unknown kind"))?,
        amount: unsigned(row.get("amount_cents"))?,
        method: row
            .get::<_, Option<String>>("method")
            .map(|m| Method::parse(&m).ok_or_else(|| bad("an unknown method")))
            .transpose()?,
        state: State::parse(&text("state")?).ok_or_else(|| bad("an unknown state"))?,
        reason: row.get::<_, Option<String>>("reason").as_deref().and_then(Reason::parse),
        sequence: row.get::<_, Option<i64>>("sequence").map(unsigned).transpose()?,
        created: millis(row.get("created_at")),
        updated: millis(row.get("updated_at")),
        stage,
        to_account: row.get::<_, Option<i64>>("to_account_id").map(unsigned).transpose()?,
        in_doubt: row.get::<_, Option<SystemTime>>("in_doubt_since").map(millis),
    })
}

/// Which kind of ledger account a name is, and the venue account a customer's belongs to.
fn classify(account: &str) -> Result<(&'static str, Option<i64>), Error> {
    if let Some(id) = account.strip_prefix("customer:").and_then(|rest| rest.strip_suffix(":cash")) {
        let id = id.parse::<u64>().map_err(|_| Error::refused(format!("a customer account {account}")))?;
        return Ok(("customer", Some(big(id)?)));
    }
    match account.split_once(':') {
        Some(("bank", _)) => Ok(("bank", None)),
        Some(("transit", _)) => Ok(("transit", None)),
        _ => Err(Error::refused(format!("no kind of ledger account is {account}"))),
    }
}

/// Writes a posting under its idempotency key, or finds it already written with the same entries.
fn post(tx: &mut Transaction<'_>, transfer: Option<u64>, memo: &str, posting: &Posting, now: u64) -> Result<(), Error> {
    let key = posting_key(transfer, posting);
    for (account, _) in posting.entries() {
        let (kind, account_id) = classify(account)?;
        tx.execute(
            "INSERT INTO ledger_accounts (account, kind, account_id) VALUES ($1, $2, $3) ON CONFLICT (account) DO NOTHING",
            &[account, &kind, &account_id],
        )?;
    }
    let transfer = transfer.map(big).transpose()?;
    let inserted = tx.query_opt(
        "INSERT INTO postings (idempotency_key, transfer_id, memo, posted_at) VALUES ($1, $2, $3, $4)
         ON CONFLICT (idempotency_key) DO NOTHING RETURNING posting_id",
        &[&key, &transfer, &memo, &time(now)],
    )?;
    match inserted {
        Some(row) => {
            let id: i64 = row.get(0);
            for (line, (account, cents)) in posting.entries().iter().enumerate() {
                let line = i16::try_from(line + 1).map_err(|_| Error::refused("a posting of too many lines"))?;
                tx.execute(
                    "INSERT INTO entries (posting_id, line, account, cents) VALUES ($1, $2, $3, $4)",
                    &[&id, &line, account, cents],
                )?;
            }
        }
        None => {
            let existing: Vec<(String, i64)> = tx
                .query(
                    "SELECT e.account, e.cents FROM entries e JOIN postings p USING (posting_id)
                     WHERE p.idempotency_key = $1 ORDER BY e.line",
                    &[&key],
                )?
                .iter()
                .map(|r| (r.get(0), r.get(1)))
                .collect();
            if existing != posting.entries() {
                return Err(Error::refused(format!("posting {key} was already written with other entries")));
            }
        }
    }
    Ok(())
}

fn receipt_from(row: &Row) -> Result<Receipt, Error> {
    let kind: String = row.get("kind");
    Ok(Receipt {
        transfer: unsigned(row.get("transfer_id"))?,
        account: unsigned(row.get("account_id"))?,
        kind: Kind::parse(&kind).ok_or_else(|| Error::refused(format!("a receipt of kind {kind}")))?,
        amount: unsigned(row.get("amount_cents"))?,
        received: millis(row.get("received_at")),
        decided: millis(row.get("decided_at")),
        settles: row.get("settles"),
        cancelled: row.get::<_, Option<SystemTime>>("cancelled_at").map(millis),
        arrivals: row.get::<_, Vec<SystemTime>>("arrivals").into_iter().map(millis).collect(),
    })
}

const RECEIPT: &str =
    "SELECT r.*, c.at AS cancelled_at FROM bank_receipts r LEFT JOIN bank_cancellations c USING (transfer_id)";

impl Records for Postgres {
    fn load(&self, runs: usize) -> Result<Option<Loaded>, Error> {
        let limit = i64::try_from(runs).unwrap_or(i64::MAX);
        let mut conn = self.db.get()?;
        let mut tx = conn
            .build_transaction()
            .read_only(true)
            .isolation_level(venue_store::postgres::IsolationLevel::RepeatableRead)
            .start()?;
        let Some(book) = tx.query_opt("SELECT nonce, created_at FROM book", &[])? else { return Ok(None) };
        let nonce = u32::try_from(book.get::<_, i64>(0)).map_err(|_| Error::refused("a book nonce out of range"))?;
        let balances = tx
            .query("SELECT account, balance_cents FROM trial_balance", &[])?
            .iter()
            .map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1)))
            .collect();
        let postings = unsigned(tx.query_one("SELECT count(*) FROM postings", &[])?.get(0))?;
        let mut fbo = BTreeMap::new();
        for r in tx.query(
            "SELECT p.transfer_id, sum(e.cents)::bigint FROM entries e JOIN postings p USING (posting_id)
             WHERE e.account = $1 AND p.transfer_id IS NOT NULL GROUP BY p.transfer_id",
            &[&ledger::FBO],
        )? {
            let moved: i64 = r.get(1);
            if moved != 0 {
                fbo.insert(unsigned(r.get(0))?, moved);
            }
        }
        let transfers = tx
            .query("SELECT * FROM transfers ORDER BY transfer_id", &[])?
            .iter()
            .map(transfer_from)
            .collect::<Result<_, _>>()?;
        let mut breaks = Vec::new();
        for r in tx.query(
            "SELECT b.*, r.at, r.resolved_by, r.note FROM recon_breaks b LEFT JOIN recon_resolutions r USING (break_id)
             ORDER BY break_id",
            &[],
        )? {
            breaks.push(Break {
                id: unsigned(r.get("break_id"))?,
                kind: r.get("kind"),
                transfer: r.get::<_, Option<i64>>("transfer_id").map(unsigned).transpose()?,
                account: r.get::<_, Option<i64>>("account_id").map(unsigned).transpose()?,
                detail: r.get("detail"),
                found_at: millis(r.get("found_at")),
                found_by: unsigned(r.get("found_by_run"))?,
                resolution: r.get::<_, Option<SystemTime>>("at").map(|at| Resolution {
                    at: millis(at),
                    by: r.get("resolved_by"),
                    note: r.get("note"),
                }),
            });
        }
        let mut reports = Vec::new();
        for r in tx.query(
            "SELECT report::text FROM (SELECT run_id, report FROM recon_runs ORDER BY run_id DESC LIMIT $1) newest
             ORDER BY run_id",
            &[&limit],
        )? {
            let text: String = r.get(0);
            reports.push(serde_json::from_str(&text).map_err(|e| Error::refused(format!("a run's report: {e}")))?);
        }
        let next_run = unsigned(
            tx.query_one(
                "SELECT greatest((SELECT coalesce(max(run_id), 0) FROM recon_runs),
                                 (SELECT coalesce(max(found_by_run), 0) FROM recon_breaks)) + 1",
                &[],
            )?
            .get(0),
        )?;
        tx.commit()?;
        Ok(Some(Loaded {
            nonce,
            created: millis(book.get(1)),
            balances,
            postings,
            fbo,
            transfers,
            breaks,
            runs: reports,
            next_run,
        }))
    }

    fn create(&self, nonce: u32, now: u64, opening: &[(&str, Posting)]) -> Result<(), Error> {
        self.db.transaction(|tx| {
            tx.execute("INSERT INTO book (nonce, created_at) VALUES ($1, $2)", &[&i64::from(nonce), &time(now)])?;
            for (memo, posting) in opening {
                post(tx, None, memo, posting, now)?;
            }
            Ok(())
        })
    }

    fn commit(&self, t: &Transfer, postings: &[(&str, Posting)], now: u64) -> Result<(), Error> {
        let (stage, due, then_state, then_reason) = stage_columns(t.stage);
        let id = big(t.id)?;
        let account = big(t.account)?;
        let amount = big(t.amount)?;
        let sequence = t.sequence.map(big).transpose()?;
        let in_doubt = t.in_doubt.map(time);
        let (created, updated) = (time(t.created), time(t.updated));
        let (kind, method, state, reason) =
            (t.kind.name(), t.method.map(Method::name), t.state.name(), t.reason.map(Reason::name));
        let to_account = t.to_account.map(big).transpose()?;
        self.db.transaction(|tx| {
            // Inserted, or updated if where it stands changed: a repeat of a write already made changes nothing.
            let changed = tx.query_opt(
                "INSERT INTO transfers AS t (transfer_id, account_id, client_transfer_id, kind, amount_cents, method,
                   state, reason, sequence, stage, due_at, then_state, then_reason, in_doubt_since, created_at,
                   updated_at, to_account_id, version)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, 1)
                 ON CONFLICT (transfer_id) DO UPDATE SET state = EXCLUDED.state, reason = EXCLUDED.reason,
                   sequence = EXCLUDED.sequence, stage = EXCLUDED.stage, due_at = EXCLUDED.due_at,
                   then_state = EXCLUDED.then_state, then_reason = EXCLUDED.then_reason,
                   in_doubt_since = EXCLUDED.in_doubt_since, updated_at = EXCLUDED.updated_at,
                   account_id = EXCLUDED.account_id, client_transfer_id = EXCLUDED.client_transfer_id,
                   kind = EXCLUDED.kind, amount_cents = EXCLUDED.amount_cents, method = EXCLUDED.method,
                   created_at = EXCLUDED.created_at, to_account_id = EXCLUDED.to_account_id, version = t.version + 1
                 WHERE (t.state, t.reason, t.sequence, t.stage, t.due_at, t.then_state, t.then_reason,
                        t.in_doubt_since, t.updated_at, t.account_id, t.client_transfer_id, t.kind, t.amount_cents,
                        t.method, t.created_at, t.to_account_id)
                   IS DISTINCT FROM
                       (EXCLUDED.state, EXCLUDED.reason, EXCLUDED.sequence, EXCLUDED.stage, EXCLUDED.due_at,
                        EXCLUDED.then_state, EXCLUDED.then_reason, EXCLUDED.in_doubt_since, EXCLUDED.updated_at,
                        EXCLUDED.account_id, EXCLUDED.client_transfer_id, EXCLUDED.kind, EXCLUDED.amount_cents,
                        EXCLUDED.method, EXCLUDED.created_at, EXCLUDED.to_account_id)
                 RETURNING version",
                &[
                    &id,
                    &account,
                    &t.client_id,
                    &kind,
                    &amount,
                    &method,
                    &state,
                    &reason,
                    &sequence,
                    &stage,
                    &due,
                    &then_state,
                    &then_reason,
                    &in_doubt,
                    &created,
                    &updated,
                    &to_account,
                ],
            )?;
            if let Some(row) = changed {
                let version: i32 = row.get(0);
                tx.execute(
                    "INSERT INTO transfer_events (transfer_id, version, at, state, reason, sequence, stage, due_at,
                       then_state, then_reason, in_doubt_since)
                     SELECT transfer_id, version, $3, state, reason, sequence, stage, due_at, then_state, then_reason,
                       in_doubt_since
                     FROM transfers WHERE transfer_id = $1 AND version = $2",
                    &[&id, &version, &time(now)],
                )?;
            }
            for (memo, posting) in postings {
                post(tx, Some(t.id), memo, posting, now)?;
            }
            Ok(())
        })
    }

    fn record_break(&self, b: &Break) -> Result<(), Error> {
        let id = big(b.id)?;
        let transfer = b.transfer.map(big).transpose()?;
        let account = b.account.map(big).transpose()?;
        let found_by = big(b.found_by)?;
        self.db.transaction(|tx| {
            tx.execute(
                "INSERT INTO recon_breaks (break_id, kind, transfer_id, account_id, detail, found_at, found_by_run)
                 VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (break_id) DO NOTHING",
                &[&id, &b.kind, &transfer, &account, &b.detail, &time(b.found_at), &found_by],
            )?;
            if let Some(r) = &b.resolution {
                tx.execute(
                    "INSERT INTO recon_resolutions (break_id, at, resolved_by, note) VALUES ($1, $2, $3, $4)
                     ON CONFLICT (break_id) DO NOTHING",
                    &[&id, &time(r.at), &r.by, &r.note],
                )?;
            }
            Ok(())
        })
    }

    fn record_run(&self, run: u64, status: &str, report: &Value, now: u64) -> Result<(), Error> {
        let run = big(run)?;
        self.db.get()?.execute(
            "INSERT INTO recon_runs (run_id, at, status, report) VALUES ($1, $2, $3, $4::text::jsonb)
             ON CONFLICT (run_id) DO NOTHING",
            &[&run, &time(now), &status, &report.to_string()],
        )?;
        Ok(())
    }

    fn trial_balance(&self) -> Result<(BTreeMap<String, i64>, u64), Error> {
        let mut conn = self.db.get()?;
        let balances = conn
            .query("SELECT account, balance_cents FROM trial_balance", &[])?
            .iter()
            .map(|r| (r.get(0), r.get(1)))
            .collect();
        let postings = unsigned(conn.query_one("SELECT count(*) FROM postings", &[])?.get(0))?;
        Ok((balances, postings))
    }

    fn ping(&self) -> Result<(), Error> {
        self.db.ping()
    }

    fn load_bank(&self) -> Result<Option<BankLoaded>, Error> {
        let mut conn = self.db.get()?;
        let Some(book) = conn.query_opt("SELECT opening_cents, opened_at, treasury_book FROM bank_book", &[])? else {
            return Ok(None);
        };
        let receipts = conn
            .query(&format!("{RECEIPT} ORDER BY transfer_id"), &[])?
            .iter()
            .map(receipt_from)
            .collect::<Result<_, _>>()?;
        let mut sent = BTreeMap::new();
        for r in conn.query("SELECT transfer_id, max(copy) + 1 FROM bank_notices GROUP BY transfer_id", &[])? {
            let copies: i32 = r.get(1);
            sent.insert(unsigned(r.get(0))?, u8::try_from(copies).unwrap_or(u8::MAX));
        }
        Ok(Some(BankLoaded {
            opening: book.get(0),
            opened: millis(book.get(1)),
            treasury_book: u32::try_from(book.get::<_, i64>(2))
                .map_err(|_| Error::refused("a treasury book out of range"))?,
            receipts,
            sent,
        }))
    }

    fn open_bank(&self, opening: i64, now: u64, treasury_book: u32) -> Result<(), Error> {
        self.db.get()?.execute(
            "INSERT INTO bank_book (opening_cents, opened_at, treasury_book) VALUES ($1, $2, $3)",
            &[&opening, &time(now), &i64::from(treasury_book)],
        )?;
        Ok(())
    }

    fn receive(&self, r: &Receipt) -> Result<Receipt, Error> {
        let arrivals: Vec<SystemTime> = r.arrivals.iter().copied().map(time).collect();
        let (id, account, amount) = (big(r.transfer)?, big(r.account)?, big(r.amount)?);
        self.db.transaction(|tx| {
            tx.execute(
                "INSERT INTO bank_receipts (transfer_id, account_id, kind, amount_cents, received_at, decided_at, settles,
                   arrivals)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (transfer_id) DO NOTHING",
                &[&id, &account, &r.kind.name(), &amount, &time(r.received), &time(r.decided), &r.settles, &arrivals],
            )?;
            receipt_from(&tx.query_one(&format!("{RECEIPT} WHERE transfer_id = $1"), &[&id])?)
        })
    }

    fn cancel(&self, transfer: u64, at: u64) -> Result<(), Error> {
        self.db.get()?.execute(
            "INSERT INTO bank_cancellations (transfer_id, at) VALUES ($1, $2) ON CONFLICT (transfer_id) DO NOTHING",
            &[&big(transfer)?, &time(at)],
        )?;
        Ok(())
    }

    fn sent(&self, transfer: u64, copy: u8, at: u64) -> Result<(), Error> {
        self.db.get()?.execute(
            "INSERT INTO bank_notices (transfer_id, copy, sent_at) VALUES ($1, $2, $3)
             ON CONFLICT (transfer_id, copy) DO NOTHING",
            &[&big(transfer)?, &i16::from(copy), &time(at)],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recon;
    use crate::treasury::tests::{balanced, body, open_on, run, state, Group, ACCOUNT};
    use crate::treasury::Treasury;
    use serde_json::json;
    use std::sync::Arc;
    use venue_store::testing;

    /// The treasury's schema on a role of its own that is not a superuser, as a deployed treasury's is.
    fn records(name: &str) -> Option<(Database, Arc<Postgres>)> {
        let config = testing::owned_config(name)?;
        let db = Database::connect(&config).expect("the test database");
        db.migrate(&venue_store::migrations_dir!()).expect("migrated");
        Some((db.clone(), Arc::new(Postgres::new(db))))
    }

    fn open(records: &Arc<Postgres>) -> Treasury {
        open_on(records.clone(), recon::Options { every_ms: 0, ..recon::Options::default() })
    }

    #[test]
    fn the_migration_files_are_the_ones_the_build_embeds() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
        venue_store::check_files(&dir, MIGRATIONS).expect("the same migrations");
    }

    #[test]
    fn transfers_settle_into_postgres_and_a_restarted_treasury_carries_on_from_them() {
        let Some((db, records)) = records("treasury_restart") else { return };
        let mut group = Group::default();
        group.sleeves.insert(ACCOUNT, 1_000);
        let (deposit, withdrawal, failed) = {
            let treasury = open(&records);
            let d = treasury.request(ACCOUNT, &body("d", "deposit", 700, "dummy_bank"), 1_000).expect("requested");
            let w = treasury.request(ACCOUNT, &body("w", "withdrawal", 400, "dummy_bank"), 1_000).expect("requested");
            let f =
                treasury.request(ACCOUNT, &body("f", "withdrawal", 5_000, "dummy_instant"), 1_000).expect("requested");
            treasury.step(&mut group, 1_000);
            (d, w, f)
        };
        // Restarted while the deposit and the withdrawal are at the bank.
        let treasury = open(&records);
        run(&treasury, &mut group, 1_500);
        assert_eq!(state(&treasury, deposit), ("settled".into(), json!(null)));
        assert_eq!(state(&treasury, withdrawal), ("settled".into(), json!(null)));
        assert_eq!(state(&treasury, failed), ("failed".into(), json!("insufficient_available_cash")));
        assert_eq!(group.sleeves[&ACCOUNT], 1_300, "granted and reclaimed once each");
        balanced(&treasury);
        let mut conn = db.get().expect("a connection");
        let check =
            conn.query_one("SELECT total_cents::bigint, postings, balanced FROM ledger_check", &[]).expect("a row");
        assert_eq!((check.get::<_, i64>(0), check.get::<_, i64>(1), check.get::<_, bool>(2)), (0, 5, true));
        let fbo: i64 = conn
            .query_one("SELECT balance_cents FROM trial_balance WHERE account = 'bank:fbo_omnibus'", &[])
            .expect("a row")
            .get(0);
        assert_eq!(fbo, 50_300);
        let events: i64 = conn
            .query_one("SELECT count(*) FROM transfer_events WHERE transfer_id = $1", &[&big(deposit).expect("fits")])
            .expect("a row")
            .get(0);
        assert_eq!(events, 4, "requested, at the bank, granting, settled");
        let reopened = open(&records);
        assert_eq!(reopened.trial_balance(), treasury.trial_balance());
        assert_eq!(reopened.list(ACCOUNT, 10).expect("a list"), treasury.list(ACCOUNT, 10).expect("a list"));
    }

    #[test]
    fn the_database_refuses_an_unbalanced_posting_and_any_edit_of_a_record() {
        let Some((db, records)) = records("treasury_enforced") else { return };
        let treasury = open(&records);
        let mut group = Group::default();
        treasury.request(ACCOUNT, &body("d", "deposit", 700, "dummy_instant"), 1_000).expect("requested");
        run(&treasury, &mut group, 1_200);
        let mut conn = db.get().expect("a connection");
        let refused = |conn: &mut venue_store::Conn, sql: &str| {
            let outcome = conn.batch_execute(sql);
            assert!(outcome.is_err(), "the database accepted: {sql}");
            let _ = conn.batch_execute("ROLLBACK");
        };
        refused(
            &mut conn,
            "BEGIN; INSERT INTO postings (idempotency_key, memo, posted_at) VALUES ('x', 'lopsided', now());
             INSERT INTO entries (posting_id, line, account, cents)
               SELECT posting_id, 1, 'bank:fbo_omnibus', 100 FROM postings WHERE idempotency_key = 'x';
             INSERT INTO entries (posting_id, line, account, cents)
               SELECT posting_id, 2, 'transit:deposits', -99 FROM postings WHERE idempotency_key = 'x';
             COMMIT",
        );
        refused(
            &mut conn,
            "BEGIN; INSERT INTO postings (idempotency_key, memo, posted_at) VALUES ('y', 'alone', now()); COMMIT",
        );
        refused(&mut conn, "UPDATE entries SET cents = cents + 1");
        refused(&mut conn, "DELETE FROM entries");
        refused(&mut conn, "UPDATE postings SET memo = 'changed'");
        refused(&mut conn, "DELETE FROM postings");
        refused(&mut conn, "TRUNCATE entries, postings CASCADE");
        refused(&mut conn, "UPDATE transfer_events SET state = 'failed'");
        refused(&mut conn, "UPDATE transfers SET amount_cents = 1");
        refused(&mut conn, "DELETE FROM transfers");
        refused(&mut conn, "UPDATE ledger_accounts SET kind = 'bank'");
        refused(&mut conn, "DELETE FROM recon_runs");
        refused(&mut conn, "UPDATE bank_receipts SET settles = false");
        let check = conn.query_one("SELECT balanced, postings FROM ledger_check", &[]).expect("a row");
        assert_eq!(
            (check.get::<_, bool>(0), check.get::<_, i64>(1)),
            (true, 3),
            "the refused statements left no trace"
        );
        balanced(&treasury);
    }

    #[test]
    fn a_write_repeated_after_a_lost_acknowledgement_posts_nothing_twice() {
        let Some((db, records)) = records("treasury_idempotent") else { return };
        records.create(9, 1_000, &[]).expect("a book");
        let mut t = Transfer {
            id: 1,
            account: ACCOUNT,
            client_id: "same".into(),
            kind: Kind::Deposit,
            amount: 250,
            method: Some(Method::DummyInstant),
            state: State::Pending,
            reason: None,
            sequence: None,
            created: 1_000,
            updated: 1_000,
            stage: Stage::Granting { then: State::Settled, reason: None },
            to_account: None,
            in_doubt: None,
        };
        let received = Posting::moving(250, ledger::FBO, ledger::DEPOSITS_IN_TRANSIT).expect("a posting");
        records.commit(&t, &[("received", received.clone())], 1_000).expect("written");
        records.commit(&t, &[("received", received.clone())], 1_005).expect("written again, as a no-op");
        let (_, postings) = records.trial_balance().expect("sums");
        assert_eq!(postings, 1);
        let other = Posting::moving(251, ledger::FBO, ledger::DEPOSITS_IN_TRANSIT).expect("a posting");
        assert!(records.commit(&t, &[("received", other)], 1_010).is_err(), "one key, other entries");
        let mut conn = db.get().expect("a connection");
        let version: i32 = conn.query_one("SELECT version FROM transfers", &[]).expect("a row").get(0);
        assert_eq!(version, 1);
        t.stage = Stage::Done;
        t.state = State::Settled;
        records.commit(&t, &[], 1_020).expect("written");
        let version: i32 = conn.query_one("SELECT version FROM transfers", &[]).expect("a row").get(0);
        assert_eq!(version, 2);
        let twin = Transfer { id: 2, ..t.clone() };
        assert!(records.commit(&twin, &[], 1_030).is_err(), "a client transfer ID used for a second transfer");
        let changed = Transfer { amount: 999, ..t };
        assert!(records.commit(&changed, &[], 1_040).is_err(), "a transfer's terms changed");
    }

    #[test]
    fn reconciliation_breaks_and_runs_survive_a_restart_in_postgres() {
        use crate::faults::{Faults, Lossy};
        let Some((_, records)) = records("treasury_recon") else { return };
        let faults = Faults { lost_notice_per_mille: 1000, ..Faults::NONE };
        let options = recon::Options { faults, every_ms: 0, grace_ms: 100 };
        let treasury = open_on(records.clone(), options);
        let mut lossy = Lossy::new(Group::default(), faults, 5);
        let id = treasury.request(ACCOUNT, &body("lost", "deposit", 500, "dummy_bank"), 1_000).expect("requested");
        for now in (1_000..=2_000).step_by(50) {
            treasury.step(&mut lossy, now);
        }
        assert_eq!(state(&treasury, id).0, "pending", "the notice was lost");
        let run = treasury.request_run(std::time::Duration::from_millis(1));
        assert_eq!(run["requested"], true);
        treasury.step(&mut lossy, 2_100);
        treasury.step(&mut lossy, 2_150);
        assert_eq!(state(&treasury, id).0, "settled", "booked from the statement");
        let runs = treasury.runs(10);
        let breaks = treasury.breaks(None, 10).expect("breaks");
        assert_eq!(breaks["breaks"][0]["kind"], "bank_decision_unbooked");
        for check in runs["runs"][0]["invariants"].as_array().into_iter().flatten() {
            assert_eq!(check["ok"], true, "{check}");
        }
        let reopened = open_on(records, options);
        assert_eq!(reopened.breaks(None, 10).expect("breaks"), breaks);
        assert_eq!(reopened.runs(10), runs);
        balanced(&reopened);
    }

    #[test]
    fn a_book_of_lines_imports_into_postgres() {
        let Some((db, records)) = records("treasury_import") else { return };
        let dir = std::env::temp_dir().join(format!("venue-treasury-import-pg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a directory");
        let ledger = dir.join("ledger.jsonl");
        std::fs::write(&ledger, crate::import::tests::SAMPLE_BOOK).expect("written");
        std::fs::write(dir.join("ledger.bank.jsonl"), crate::import::tests::SAMPLE_BANK).expect("written");
        let imported = crate::import::import(&ledger, records.as_ref()).expect("imported");
        assert_eq!((imported.transfer_lines, imported.postings, imported.runs), (3, 3, 1));
        assert!(crate::import::import(&ledger, records.as_ref()).is_err(), "imported twice");
        let events: i64 = db
            .get()
            .expect("a connection")
            .query_one("SELECT count(*) FROM transfer_events", &[])
            .expect("a row")
            .get(0);
        assert_eq!(events, 3);
        let treasury = open(&records);
        assert_eq!(treasury.get(ACCOUNT, 1).expect("the transfer")["state"], "settled");
        assert_eq!(treasury.trial_balance()["balances"]["customer:2:cash"], -50_700);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
