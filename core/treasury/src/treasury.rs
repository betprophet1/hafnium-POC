//! The treasury: the account of record for money moving in and out of the venue (blueprint §5, §11).
//!
//! A deposit is sent to the bank and, once the bank settles it, granted to the account's sleeve in the sequencer group.
//! A withdrawal is reclaimed from the sleeve first — the sequencer group decides whether the account has the cash — and
//! only then sent to the bank; if the bank cannot pay it out, or the client cancels it while it waits, it is granted
//! back. Every movement is a balanced posting in the ledger, written down with the transfer's new stage in one line of
//! the book before anything is done that depends on it.
//!
//! Nothing here waits on the network while holding the lock: `step` takes one piece of work out, does it, and puts
//! the outcome back.

use crate::bank::FakeBank;
use crate::book::Book;
use crate::ledger::{self, Ledger, Posting};
use crate::transfer::{Kind, Method, Reason, Stage, State, Transfer};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// Transfers a client may list at once.
pub const MAX_LIST: usize = 500;

/// Updates kept for streams that have fallen behind. A stream further behind than this skips ahead: updates are a
/// convenience, and a client that misses some reads the list again (interface 1 README, deposits and withdrawals).
const UPDATES: usize = 10_000;

/// How long a request waits for a transfer to get past the stages that take no time — a reclaim or a grant, or
/// handing it to the bank — so that its answer already says whether a withdrawal was refused, or an instant deposit
/// settled. A sequencer group that is slower than this is answered `pending`.
pub const QUICK: Duration = Duration::from_secs(5);

/// How soon work that could not be done — a sequencer group that did not answer — is tried again.
const RETRY_MS: u64 = 1000;

/// Transfer IDs fit in the six hex digits a command ID leaves them (`command_id`).
const MAX_TRANSFERS: u64 = 0xff_ffff;

/// What the sequencer group said to a sleeve adjustment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Adjusted {
    /// Applied at this input sequence.
    Applied(u64),
    /// Rejected, with the engine's reason in lower snake case.
    Rejected(String),
    /// Answered as a duplicate of a command whose outcome is not known yet: try again later.
    Unknown,
}

/// The sequencer group, as far as the treasury needs it: sleeve grants and reclaims (spec E-7.2).
pub trait Sequencer {
    fn adjust(&mut self, command_id: &str, account: u64, cents: u64, reason: &str) -> Result<Adjusted, String>;
}

/// A refusal at the treasury's edge, in interface 1's `Error` form. Nothing was created or changed.
#[derive(Debug, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into() }
    }

    pub fn bad(message: impl Into<String>) -> Self {
        Self::new(400, "BAD_REQUEST", message)
    }

    pub fn not_found(message: &str) -> Self {
        Self::new(404, "NOT_FOUND", message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(503, "UNAVAILABLE", message)
    }

    pub fn body(&self) -> Value {
        json!({"error": {"code": self.code, "message": self.message}})
    }
}

pub struct Config {
    pub bank: FakeBank,
    /// The largest single transfer, in cents.
    pub max_cents: u64,
    /// The accounts transfers may be made for, each with the cents the environment's seed grants it.
    pub accounts: BTreeMap<u64, u64>,
}

struct Inner {
    /// Unique to this book, so that its command IDs never meet those of an earlier book on the same sequencer group.
    nonce: u32,
    transfers: BTreeMap<u64, Transfer>,
    by_client: BTreeMap<(u64, String), u64>,
    ledger: Ledger,
    book: Option<Book>,
    /// Transfers that changed, oldest first, each numbered, for streams.
    updates: VecDeque<(u64, u64, Value)>,
    next_update: u64,
    /// When work that failed may be tried again, by transfer.
    retry_at: BTreeMap<u64, u64>,
    /// Counts every change, so that whoever waits for one cannot miss one made just before it started waiting.
    generation: u64,
}

pub struct Treasury {
    config: Config,
    inner: Mutex<Inner>,
    /// Signalled whenever anything changes: a new transfer for `step`, a new stage for a request waiting on one, a new
    /// update for a stream.
    pub changed: Condvar,
}

/// One piece of work that needs the sequencer group.
struct Command {
    transfer: u64,
    command_id: String,
    account: u64,
    cents: u64,
    reason: &'static str,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn nonce() -> Result<u32, String> {
    use std::io::Read;
    let mut bytes = [0u8; 4];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|e| format!("drawing a nonce: {e}"))?;
    Ok(u32::from_le_bytes(bytes))
}

fn stage_json(stage: Stage) -> Value {
    match stage {
        Stage::Reclaiming => json!({"at": "reclaiming"}),
        Stage::ToBank => json!({"at": "to_bank"}),
        Stage::AtBank { due, settles } => json!({"at": "at_bank", "due": due, "settles": settles}),
        Stage::Granting { then, reason } => {
            json!({"at": "granting", "then": then.name(), "reason": reason.map(Reason::name)})
        }
        Stage::Done => json!({"at": "done"}),
    }
}

fn stage_from(v: &Value) -> Result<Stage, String> {
    Ok(match v["at"].as_str().unwrap_or("") {
        "reclaiming" => Stage::Reclaiming,
        "to_bank" => Stage::ToBank,
        "at_bank" => Stage::AtBank {
            due: v["due"].as_u64().ok_or("an at_bank stage needs due")?,
            settles: v["settles"].as_bool().ok_or("an at_bank stage needs settles")?,
        },
        "granting" => Stage::Granting {
            then: v["then"].as_str().and_then(State::parse).ok_or("a granting stage needs then")?,
            reason: v["reason"].as_str().and_then(Reason::parse),
        },
        "done" => Stage::Done,
        other => return Err(format!("unknown stage {other:?}")),
    })
}

/// A transfer as the book records it: everything, including the stage a client never sees.
fn record(t: &Transfer) -> Value {
    json!({
        "id": t.id, "account": t.account, "clientTransferId": t.client_id, "kind": t.kind.name(),
        "amountCents": t.amount, "method": t.method.name(), "state": t.state.name(),
        "reason": t.reason.map(Reason::name), "sequence": t.sequence, "created": t.created, "updated": t.updated,
        "stage": stage_json(t.stage)
    })
}

fn from_record(v: &Value) -> Result<Transfer, String> {
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
        stage: stage_from(&v["stage"])?,
    })
}

/// The command ID of a transfer's reclaim (`r`) or grant (`g`): fixed by the transfer, so that sending it again after
/// a restart is answered as a duplicate rather than moving the money twice (spec E-7.1). Sixteen bytes exactly.
fn command_id(nonce: u32, transfer: u64, what: char) -> String {
    format!("T{nonce:08x}{transfer:06x}{what}")
}

/// The ledger account that holds a transfer's money between the bank and the sequencer group.
const fn in_transit(kind: Kind) -> &'static str {
    match kind {
        Kind::Deposit => ledger::DEPOSITS_IN_TRANSIT,
        Kind::Withdrawal => ledger::WITHDRAWALS_IN_TRANSIT,
    }
}

fn posting_json(memo: &str, posting: &Posting) -> Value {
    json!({"memo": memo, "entries": posting.to_json()})
}

fn text<'a>(fields: &'a Map<String, Value>, name: &str) -> Result<&'a str, Refusal> {
    fields.get(name).and_then(Value::as_str).ok_or_else(|| Refusal::bad(format!("{name} is required, as a string")))
}

impl Treasury {
    /// A treasury whose book is the file at `path`, replayed if it exists. A new book records the seed's grants as
    /// opening balances held at the fake bank, so the ledger starts where the sequencer group does and the omnibus
    /// covers every customer's cash from the first posting (blueprint §11): a withdrawal of seeded money is paid out
    /// of money the bank holds, not out of nothing.
    pub fn open(config: Config, path: &Path, fsync: bool, now: u64) -> Result<Self, String> {
        let (book, records) = Book::open(path, fsync)?;
        let mut inner = Inner {
            nonce: 0,
            transfers: BTreeMap::new(),
            by_client: BTreeMap::new(),
            ledger: Ledger::default(),
            book: None,
            updates: VecDeque::new(),
            next_update: 0,
            retry_at: BTreeMap::new(),
            generation: 0,
        };
        let mut records = records.into_iter();
        match records.next() {
            Some(first) => {
                let nonce = first["book"]["nonce"].as_u64().ok_or("the ledger's first line must name its book")?;
                inner.nonce = u32::try_from(nonce).map_err(|_| "a book nonce out of range")?;
            }
            None => inner.nonce = nonce()?,
        }
        for r in records {
            for p in r["postings"].as_array().into_iter().flatten() {
                inner.ledger.post(&Posting::from_json(&p["entries"])?)?;
            }
            if !r["transfer"].is_null() {
                let t = from_record(&r["transfer"])?;
                inner.by_client.insert((t.account, t.client_id.clone()), t.id);
                inner.transfers.insert(t.id, t);
            }
        }
        let fresh = inner.transfers.is_empty() && inner.ledger.trial_balance()["postings"] == 0;
        inner.book = Some(book);
        let treasury = Self { config, inner: Mutex::new(inner), changed: Condvar::new() };
        if fresh {
            let mut inner = locked(&treasury.inner);
            let nonce = inner.nonce;
            Self::write(&mut inner, json!({"book": {"nonce": nonce, "created": now}}))?;
            let mut postings = Vec::new();
            for (&account, &cents) in treasury.config.accounts.iter().filter(|(_, c)| **c > 0) {
                postings.push(Posting::moving(cents, ledger::FBO, &ledger::customer(account))?);
            }
            if !postings.is_empty() {
                let memo = "opening balance: the environment's seed grant, held at the fake bank";
                let written: Vec<Value> = postings.iter().map(|p| posting_json(memo, p)).collect();
                Self::write(&mut inner, json!({"postings": written}))?;
                for posting in &postings {
                    inner.ledger.post(posting)?;
                }
            }
        }
        Ok(treasury)
    }

    fn write(inner: &mut Inner, record: Value) -> Result<(), String> {
        inner.book.as_mut().ok_or("the book is closed")?.append(&record)
    }

    /// Writes a transfer's new stage, with the postings that go with it, and only then applies them: a line that
    /// could not be written changes nothing. Tells streams and waiters if a client can see the difference.
    fn commit(
        &self,
        inner: &mut Inner,
        mut next: Transfer,
        postings: &[(&str, Posting)],
        now: u64,
    ) -> Result<(), String> {
        let before = inner.transfers.get(&next.id).map(Transfer::visible);
        if before.is_some_and(|b| b != next.visible()) {
            next.updated = now;
        }
        let written: Vec<Value> = postings.iter().map(|(memo, p)| posting_json(memo, p)).collect();
        Self::write(inner, json!({"transfer": record(&next), "postings": written}))?;
        for (_, posting) in postings {
            inner.ledger.post(posting)?;
        }
        if before != Some(next.visible()) {
            let n = inner.next_update;
            inner.next_update += 1;
            inner.updates.push_back((n, next.account, next.to_json()));
            while inner.updates.len() > UPDATES {
                inner.updates.pop_front();
            }
        }
        inner.by_client.insert((next.account, next.client_id.clone()), next.id);
        inner.transfers.insert(next.id, next);
        inner.generation += 1;
        self.changed.notify_all();
        Ok(())
    }

    fn known(&self, account: u64) -> Result<(), Refusal> {
        if self.config.accounts.contains_key(&account) {
            Ok(())
        } else {
            Err(Refusal::not_found("no such account"))
        }
    }

    /// POST: a new transfer, or the one this client transfer ID already made. Returns its ID; the caller may wait
    /// for it to get past its quick stages (`wait_quick`) before answering.
    pub fn request(&self, account: u64, body: &Value, now: u64) -> Result<u64, Refusal> {
        self.known(account)?;
        let fields = body.as_object().ok_or_else(|| Refusal::bad("the body must be a JSON object"))?;
        const FIELDS: [&str; 4] = ["clientTransferId", "kind", "amountCents", "method"];
        if let Some(k) = fields.keys().find(|k| !FIELDS.contains(&k.as_str())) {
            return Err(Refusal::bad(format!("unknown field {k}")));
        }
        let client_id = text(fields, "clientTransferId")?;
        if !(1..=32).contains(&client_id.len()) || !client_id.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
            return Err(Refusal::bad("clientTransferId must be 1 to 32 printable ASCII characters"));
        }
        let kind = Kind::parse(text(fields, "kind")?)
            .ok_or_else(|| Refusal::bad(format!("kind must be one of {}", Kind::NAMES.join(", "))))?;
        let method = Method::parse(text(fields, "method")?)
            .ok_or_else(|| Refusal::bad(format!("method must be one of {}", Method::NAMES.join(", "))))?;
        let max = self.config.max_cents;
        let amount = fields
            .get("amountCents")
            .and_then(Value::as_u64)
            .filter(|a| (1..=max).contains(a))
            .ok_or_else(|| Refusal::bad(format!("amountCents must be an integer from 1 to {max}")))?;
        let mut inner = locked(&self.inner);
        if let Some(&id) = inner.by_client.get(&(account, client_id.to_string())) {
            let t = &inner.transfers[&id];
            return if (t.kind, t.amount, t.method) == (kind, amount, method) {
                Ok(id)
            } else {
                Err(Refusal::new(
                    409,
                    "CLIENT_TRANSFER_ID_REUSED",
                    format!("clientTransferId {client_id} was used for transfer {id}, with other details"),
                ))
            };
        }
        let id = inner.transfers.keys().next_back().map_or(1, |last| last + 1);
        if id > MAX_TRANSFERS {
            return Err(Refusal::unavailable("this treasury's book is full"));
        }
        let transfer = Transfer {
            id,
            account,
            client_id: client_id.to_string(),
            kind,
            amount,
            method,
            state: State::Pending,
            reason: None,
            sequence: None,
            created: now,
            updated: now,
            stage: match kind {
                Kind::Deposit => Stage::ToBank,
                Kind::Withdrawal => Stage::Reclaiming,
            },
        };
        self.commit(&mut inner, transfer, &[], now).map_err(|e| {
            eprintln!("treasury: {e}");
            Refusal::unavailable("the treasury could not record the transfer; nothing was done")
        })?;
        Ok(id)
    }

    /// DELETE: cancels a transfer still waiting on the bank. A withdrawal is granted back by `step`.
    pub fn cancel(&self, account: u64, id: u64, now: u64) -> Result<u64, Refusal> {
        let mut inner = locked(&self.inner);
        let mut t = inner
            .transfers
            .get(&id)
            .filter(|t| t.account == account)
            .cloned()
            .ok_or_else(|| Refusal::not_found("no such transfer"))?;
        if !matches!(t.stage, Stage::ToBank | Stage::AtBank { .. }) {
            return Err(Refusal::new(
                409,
                "TRANSFER_NOT_CANCELLABLE",
                format!("transfer {id} is {} and no longer waiting on the bank", t.state.name()),
            ));
        }
        match t.kind {
            Kind::Deposit => {
                (t.state, t.reason, t.stage) = (State::Cancelled, Some(Reason::CancelledByClient), Stage::Done);
            }
            Kind::Withdrawal => {
                t.stage = Stage::Granting { then: State::Cancelled, reason: Some(Reason::CancelledByClient) };
            }
        }
        self.commit(&mut inner, t, &[], now).map_err(|e| {
            eprintln!("treasury: {e}");
            Refusal::unavailable("the treasury could not record the cancellation; nothing was done")
        })?;
        Ok(id)
    }

    /// One of the account's transfers, as a client sees it.
    pub fn get(&self, account: u64, id: u64) -> Result<Value, Refusal> {
        self.known(account)?;
        let inner = locked(&self.inner);
        inner
            .transfers
            .get(&id)
            .filter(|t| t.account == account)
            .map(Transfer::to_json)
            .ok_or_else(|| Refusal::not_found("no such transfer"))
    }

    /// The account's newest transfers, newest first.
    pub fn list(&self, account: u64, limit: usize) -> Result<Value, Refusal> {
        self.known(account)?;
        let inner = locked(&self.inner);
        let transfers: Vec<Value> = inner
            .transfers
            .values()
            .rev()
            .filter(|t| t.account == account)
            .take(limit.min(MAX_LIST))
            .map(Transfer::to_json)
            .collect();
        Ok(json!({"transfers": transfers}))
    }

    /// Waits up to `limit` for a transfer to be waiting on the bank or finished, and returns it as a client sees it.
    pub fn wait_quick(&self, id: u64, limit: Duration) -> Value {
        let inner = locked(&self.inner);
        let quick = |i: &mut Inner| {
            i.transfers
                .get(&id)
                .is_some_and(|t| matches!(t.stage, Stage::Reclaiming | Stage::ToBank | Stage::Granting { .. }))
        };
        let (inner, _) = self.changed.wait_timeout_while(inner, limit, quick).unwrap_or_else(PoisonError::into_inner);
        inner.transfers.get(&id).map_or(Value::Null, Transfer::to_json)
    }

    /// Updates numbered `from` and after, each with its account, and the number to ask from next time. `None` asks
    /// from the next update to come.
    pub fn updates_from(&self, from: Option<u64>, wait: Duration) -> (Vec<(u64, Value)>, u64) {
        let inner = locked(&self.inner);
        let from = from.unwrap_or(inner.next_update);
        let (inner, _) = self
            .changed
            .wait_timeout_while(inner, wait, |i| i.next_update <= from)
            .unwrap_or_else(PoisonError::into_inner);
        let out = inner.updates.iter().filter(|(n, _, _)| *n >= from).map(|(_, a, t)| (*a, t.clone())).collect();
        (out, inner.next_update.max(from))
    }

    /// The start of every command ID this book sends, by which its commands are known on the feed.
    pub fn command_prefix(&self) -> String {
        format!("T{:08x}", locked(&self.inner).nonce)
    }

    /// How many changes there have been, for `wait_for_change`.
    pub fn generation(&self) -> u64 {
        locked(&self.inner).generation
    }

    /// Waits up to `limit` for a change after `seen` changes.
    pub fn wait_for_change(&self, seen: u64, limit: Duration) {
        let inner = locked(&self.inner);
        let _ = self.changed.wait_timeout_while(inner, limit, |i| i.generation == seen);
    }

    /// Every ledger balance, for an operator.
    pub fn trial_balance(&self) -> Value {
        locked(&self.inner).ledger.trial_balance()
    }

    #[cfg(test)]
    pub fn balance(&self, account: &str) -> i64 {
        locked(&self.inner).ledger.balance(account)
    }

    /// Does everything that can be done now: hands transfers to the bank, collects what the bank has decided, and sends
    /// reclaims and grants to `sequencer`. Returns how long until something else falls due, for the caller to wait at
    /// most that long for a change.
    pub fn step(&self, sequencer: &mut dyn Sequencer, now: u64) -> Duration {
        loop {
            let command = {
                let mut inner = locked(&self.inner);
                match self.local_work(&mut inner, now) {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(e) => {
                        eprintln!("treasury: {e}");
                        return Duration::from_millis(RETRY_MS);
                    }
                }
                match Self::next_command(&inner, now) {
                    Some(command) => command,
                    None => return Self::next_due(&inner, now),
                }
            };
            let outcome = sequencer.adjust(&command.command_id, command.account, command.cents, command.reason);
            let mut inner = locked(&self.inner);
            if let Err(e) = self.apply(&mut inner, &command, outcome, now) {
                eprintln!("treasury: transfer {}: {e}", command.transfer);
                inner.retry_at.insert(command.transfer, now + RETRY_MS);
            }
        }
    }

    /// One stage that needs nothing but the bank, if there is one due. Says whether it did anything.
    fn local_work(&self, inner: &mut Inner, now: u64) -> Result<bool, String> {
        let due = inner.transfers.values().find(|t| match t.stage {
            Stage::ToBank => true,
            Stage::AtBank { due, .. } => due <= now,
            _ => false,
        });
        let Some(mut t) = due.cloned() else { return Ok(false) };
        if t.stage == Stage::ToBank {
            let decision = self.config.bank.decide(t.id, t.kind, t.method);
            let due = now.saturating_add(decision.delay_ms);
            t.stage = Stage::AtBank { due, settles: decision.settles };
            // A bank that decides at once is decided in the same breath, so nobody waiting sees it at the bank.
            if due > now {
                self.commit(inner, t, &[], now)?;
                return Ok(true);
            }
        }
        let mut postings = Vec::new();
        match (t.stage, t.kind) {
            (Stage::AtBank { settles: true, .. }, Kind::Deposit) => {
                postings.push((
                    "deposit received at the bank",
                    Posting::moving(t.amount, ledger::FBO, ledger::DEPOSITS_IN_TRANSIT)?,
                ));
                t.stage = Stage::Granting { then: State::Settled, reason: None };
            }
            (Stage::AtBank { settles: false, .. }, Kind::Deposit) => {
                (t.state, t.reason, t.stage) = (State::Failed, Some(Reason::DeclinedByBank), Stage::Done);
            }
            (Stage::AtBank { settles: true, .. }, Kind::Withdrawal) => {
                postings.push((
                    "withdrawal paid out by the bank",
                    Posting::moving(t.amount, ledger::WITHDRAWALS_IN_TRANSIT, ledger::FBO)?,
                ));
                (t.state, t.stage) = (State::Settled, Stage::Done);
            }
            (Stage::AtBank { settles: false, .. }, Kind::Withdrawal) => {
                t.stage = Stage::Granting { then: State::Failed, reason: Some(Reason::ReturnedByBank) };
            }
            _ => return Ok(false),
        }
        self.commit(inner, t, &postings, now)?;
        Ok(true)
    }

    fn next_command(inner: &Inner, now: u64) -> Option<Command> {
        let ready = |t: &&Transfer| inner.retry_at.get(&t.id).is_none_or(|at| *at <= now);
        inner.transfers.values().filter(ready).find_map(|t| {
            let (what, reason) = match t.stage {
                Stage::Reclaiming => ('r', "RECLAIM"),
                Stage::Granting { .. } => ('g', "GRANT"),
                _ => return None,
            };
            Some(Command {
                transfer: t.id,
                command_id: command_id(inner.nonce, t.id, what),
                account: t.account,
                cents: t.amount,
                reason,
            })
        })
    }

    fn next_due(inner: &Inner, now: u64) -> Duration {
        let due = inner.transfers.values().filter_map(|t| match t.stage {
            Stage::AtBank { due, .. } => Some(due),
            Stage::Reclaiming | Stage::Granting { .. } => inner.retry_at.get(&t.id).copied(),
            _ => None,
        });
        Duration::from_millis(due.min().map_or(RETRY_MS, |d| d.saturating_sub(now).clamp(1, RETRY_MS)))
    }

    /// Puts what the sequencer group said about a command back into its transfer.
    fn apply(
        &self,
        inner: &mut Inner,
        command: &Command,
        outcome: Result<Adjusted, String>,
        now: u64,
    ) -> Result<(), String> {
        let Some(mut t) = inner.transfers.get(&command.transfer).cloned() else { return Ok(()) };
        let outcome = outcome.map_err(|e| format!("the sequencer group did not answer: {e}"))?;
        let customer = ledger::customer(t.account);
        let mut postings = Vec::new();
        match (t.stage, outcome) {
            (_, Adjusted::Unknown) => return Err("a duplicate whose first outcome is not known yet".into()),
            (Stage::Reclaiming, Adjusted::Applied(sequence)) => {
                postings.push((
                    "withdrawal reclaimed from the sleeve",
                    Posting::moving(t.amount, &customer, ledger::WITHDRAWALS_IN_TRANSIT)?,
                ));
                (t.sequence, t.stage) = (Some(sequence), Stage::ToBank);
            }
            (Stage::Reclaiming, Adjusted::Rejected(reason)) => {
                if reason != "insufficient_sleeve" {
                    eprintln!("treasury: transfer {}: reclaim rejected as {reason}", t.id);
                }
                (t.state, t.reason, t.stage) = (State::Failed, Some(Reason::InsufficientAvailableCash), Stage::Done);
            }
            (Stage::Granting { then, reason }, Adjusted::Applied(sequence)) => {
                let memo = match t.kind {
                    Kind::Deposit => "deposit granted to the sleeve",
                    Kind::Withdrawal => "withdrawal returned to the sleeve",
                };
                postings.push((memo, Posting::moving(t.amount, in_transit(t.kind), &customer)?));
                (t.state, t.reason, t.sequence, t.stage) = (then, reason, Some(sequence), Stage::Done);
            }
            (Stage::Granting { .. }, Adjusted::Rejected(reason)) => {
                // A grant of a positive amount has nothing to reject (E-7.2), so this is a fault to look at, and the
                // money is still owed: keep trying.
                return Err(format!("grant rejected as {reason}"));
            }
            _ => return Ok(()),
        }
        inner.retry_at.remove(&t.id);
        self.commit(inner, t, &postings, now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ledger::{customer, DEPOSITS_IN_TRANSIT, FBO, WITHDRAWALS_IN_TRANSIT};

    /// A sequencer group that keeps sleeves and remembers command IDs as E-7.1 and E-7.2 say, and can be told to
    /// stop answering.
    #[derive(Default)]
    struct Group {
        sleeves: BTreeMap<u64, i64>,
        outcomes: BTreeMap<String, Adjusted>,
        sequence: u64,
        down: bool,
        sent: usize,
    }

    impl Sequencer for Group {
        fn adjust(&mut self, command_id: &str, account: u64, cents: u64, reason: &str) -> Result<Adjusted, String> {
            if self.down {
                return Err("down".into());
            }
            self.sent += 1;
            self.sequence += 1;
            if let Some(first) = self.outcomes.get(command_id) {
                return Ok(first.clone());
            }
            let sleeve = self.sleeves.entry(account).or_default();
            let cents = i64::try_from(cents).map_err(|e| e.to_string())?;
            let outcome = match reason {
                "RECLAIM" if cents > *sleeve => Adjusted::Rejected("insufficient_sleeve".into()),
                "RECLAIM" => {
                    *sleeve -= cents;
                    Adjusted::Applied(self.sequence)
                }
                _ => {
                    *sleeve += cents;
                    Adjusted::Applied(self.sequence)
                }
            };
            self.outcomes.insert(command_id.to_string(), outcome.clone());
            Ok(outcome)
        }
    }

    const ACCOUNT: u64 = 2;

    fn path(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("venue-treasury-{name}-{}.jsonl", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    fn open(path: &Path) -> Treasury {
        let config = Config {
            bank: FakeBank { seed: 5, min_delay_ms: 100, max_delay_ms: 200, failure_per_mille: 0 },
            max_cents: 1_000_000,
            accounts: [(1, 0), (ACCOUNT, 50_000)].into_iter().collect(),
        };
        Treasury::open(config, path, false, 1_000).expect("a treasury")
    }

    fn body(client: &str, kind: &str, cents: u64, method: &str) -> Value {
        json!({"clientTransferId": client, "kind": kind, "amountCents": cents, "method": method})
    }

    fn state(treasury: &Treasury, id: u64) -> (String, Value) {
        let t = treasury.get(ACCOUNT, id).expect("the transfer");
        (t["state"].as_str().unwrap_or("").to_string(), t["reason"].clone())
    }

    /// Runs the treasury until nothing more is due before `until`.
    fn run(treasury: &Treasury, group: &mut Group, until: u64) {
        for now in (1_000..=until).step_by(50) {
            treasury.step(group, now);
        }
    }

    /// The ledger sums to zero, and the omnibus holds exactly the customers' cash plus what is in transit
    /// (blueprint §11: the sum of customer cash equals the FBO balance minus in-flight transfers).
    fn balanced(treasury: &Treasury) {
        let trial = treasury.trial_balance();
        assert_eq!(trial["total"], "0", "{trial}");
        let customers: i64 = [1, ACCOUNT].iter().map(|a| treasury.balance(&customer(*a))).sum();
        let transit = treasury.balance(DEPOSITS_IN_TRANSIT) + treasury.balance(WITHDRAWALS_IN_TRANSIT);
        assert_eq!(treasury.balance(FBO), -customers - transit, "{trial}");
    }

    #[test]
    fn a_settled_deposit_is_granted_once_and_the_ledger_balances() {
        let p = path("deposit");
        let treasury = open(&p);
        let mut group = Group::default();
        assert_eq!(treasury.balance(&customer(ACCOUNT)), -50_000, "the seed's opening balance");
        assert_eq!(treasury.balance(FBO), 50_000);
        let id = treasury.request(ACCOUNT, &body("d1", "deposit", 10_000, "dummy_bank"), 1_000).expect("requested");
        treasury.step(&mut group, 1_000);
        assert_eq!(state(&treasury, id).0, "pending");
        assert!(group.sleeves.is_empty(), "granted before the bank settled");
        run(&treasury, &mut group, 1_500);
        assert_eq!(state(&treasury, id), ("settled".into(), Value::Null));
        assert_eq!(group.sleeves[&ACCOUNT], 10_000);
        assert_eq!((treasury.balance(FBO), treasury.balance(&customer(ACCOUNT))), (60_000, -60_000));
        assert_eq!(treasury.balance(DEPOSITS_IN_TRANSIT), 0);
        assert!(treasury.get(ACCOUNT, id).expect("the transfer")["sequence"].is_string());
        balanced(&treasury);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_withdrawal_beyond_available_cash_is_refused_and_moves_nothing() {
        let p = path("refused");
        let treasury = open(&p);
        let mut group = Group::default();
        group.sleeves.insert(ACCOUNT, 300);
        let id = treasury.request(ACCOUNT, &body("w1", "withdrawal", 500, "dummy_instant"), 1_000).expect("requested");
        run(&treasury, &mut group, 1_500);
        assert_eq!(state(&treasury, id), ("failed".into(), json!("insufficient_available_cash")));
        assert_eq!(group.sleeves[&ACCOUNT], 300);
        assert_eq!(treasury.balance(FBO), 50_000);
        assert_eq!(treasury.balance(WITHDRAWALS_IN_TRANSIT), 0);
        assert!(treasury.get(ACCOUNT, id).expect("the transfer")["sequence"].is_null());
        balanced(&treasury);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_withdrawal_is_reclaimed_before_the_bank_and_paid_out_after() {
        let p = path("withdrawal");
        let treasury = open(&p);
        let mut group = Group::default();
        group.sleeves.insert(ACCOUNT, 1_000);
        let id = treasury.request(ACCOUNT, &body("w2", "withdrawal", 400, "dummy_bank"), 1_000).expect("requested");
        treasury.step(&mut group, 1_000);
        assert_eq!(group.sleeves[&ACCOUNT], 600, "reclaimed as soon as it was asked for");
        assert_eq!(state(&treasury, id).0, "pending");
        run(&treasury, &mut group, 1_500);
        assert_eq!(state(&treasury, id), ("settled".into(), Value::Null));
        assert_eq!(group.sleeves[&ACCOUNT], 600);
        assert_eq!((treasury.balance(FBO), treasury.balance(WITHDRAWALS_IN_TRANSIT)), (49_600, 0));
        balanced(&treasury);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn what_the_bank_fails_is_never_granted_or_is_granted_back() {
        let p = path("bank-fails");
        let treasury = open(&p);
        let mut group = Group::default();
        group.sleeves.insert(ACCOUNT, 1_000);
        let deposit = treasury.request(ACCOUNT, &body("d", "deposit", 700, "dummy_failing"), 1_000).expect("requested");
        let withdrawal =
            treasury.request(ACCOUNT, &body("w", "withdrawal", 900, "dummy_failing"), 1_000).expect("requested");
        run(&treasury, &mut group, 1_500);
        assert_eq!(state(&treasury, deposit), ("failed".into(), json!("declined_by_bank")));
        assert_eq!(state(&treasury, withdrawal), ("failed".into(), json!("returned_by_bank")));
        assert_eq!(group.sleeves[&ACCOUNT], 1_000, "the withdrawal was granted back, the deposit never granted");
        assert_eq!(treasury.balance(&customer(ACCOUNT)), -50_000);
        assert_eq!((treasury.balance(FBO), treasury.balance(WITHDRAWALS_IN_TRANSIT)), (50_000, 0));
        balanced(&treasury);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_client_transfer_id_makes_one_transfer_however_often_it_is_sent() {
        let p = path("idempotent");
        let mut group = Group::default();
        let id = {
            let treasury = open(&p);
            let id = treasury.request(ACCOUNT, &body("same", "deposit", 250, "dummy_instant"), 1_000).expect("first");
            assert_eq!(treasury.request(ACCOUNT, &body("same", "deposit", 250, "dummy_instant"), 1_010), Ok(id));
            let reused = treasury.request(ACCOUNT, &body("same", "deposit", 251, "dummy_instant"), 1_020);
            assert_eq!(reused.map_err(|r| (r.status, r.code)), Err((409, "CLIENT_TRANSFER_ID_REUSED")));
            // The same ID is another account's to use.
            assert!(treasury
                .request(1, &body("same", "deposit", 1, "dummy_instant"), 1_030)
                .is_ok_and(|other| other != id));
            run(&treasury, &mut group, 1_200);
            id
        };
        // And it holds after a restart, with the transfer as it was left.
        let treasury = open(&p);
        assert_eq!(treasury.request(ACCOUNT, &body("same", "deposit", 250, "dummy_instant"), 2_000), Ok(id));
        assert_eq!(state(&treasury, id).0, "settled");
        assert_eq!(group.sleeves[&ACCOUNT], 250);
        assert_eq!(treasury.list(ACCOUNT, 10).expect("a list")["transfers"].as_array().map(Vec::len), Some(1));
        balanced(&treasury);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_grant_sent_again_after_a_restart_moves_the_money_once() {
        let p = path("restart");
        let mut group = Group::default();
        {
            let treasury = open(&p);
            treasury.request(ACCOUNT, &body("r", "deposit", 800, "dummy_instant"), 1_000).expect("requested");
            // The group applies the grant, and the treasury stops before it hears back: its book still says granting.
            let mut applied_then_lost = LostAnswer(&mut group);
            treasury.step(&mut applied_then_lost, 1_000);
        }
        let treasury = open(&p);
        treasury.step(&mut group, 1_100);
        let t = treasury.list(ACCOUNT, 1).expect("a list")["transfers"][0].clone();
        assert_eq!(t["state"], "settled", "{t}");
        assert_eq!(group.sleeves[&ACCOUNT], 800, "granted twice");
        assert_eq!(treasury.balance(&customer(ACCOUNT)), -50_800, "posted twice");
        balanced(&treasury);
        let _ = std::fs::remove_file(&p);
    }

    /// Passes each command on and then loses the answer, as a treasury that stops at that moment would.
    struct LostAnswer<'a>(&'a mut Group);

    impl Sequencer for LostAnswer<'_> {
        fn adjust(&mut self, command_id: &str, account: u64, cents: u64, reason: &str) -> Result<Adjusted, String> {
            self.0.adjust(command_id, account, cents, reason)?;
            Err("the answer was lost".into())
        }
    }

    #[test]
    fn a_sequencer_group_that_does_not_answer_leaves_the_transfer_to_be_tried_again() {
        let p = path("down");
        let treasury = open(&p);
        let mut group = Group { down: true, ..Group::default() };
        group.sleeves.insert(ACCOUNT, 1_000);
        let id =
            treasury.request(ACCOUNT, &body("down", "withdrawal", 100, "dummy_instant"), 1_000).expect("requested");
        treasury.step(&mut group, 1_000);
        assert_eq!(state(&treasury, id).0, "pending");
        group.down = false;
        treasury.step(&mut group, 1_500);
        assert_eq!(state(&treasury, id).0, "pending", "retried before its time");
        treasury.step(&mut group, 2_000);
        assert_eq!(state(&treasury, id).0, "settled");
        assert_eq!(group.sleeves[&ACCOUNT], 900);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn only_a_transfer_waiting_on_the_bank_can_be_cancelled() {
        let p = path("cancel");
        let treasury = open(&p);
        let mut group = Group::default();
        group.sleeves.insert(ACCOUNT, 1_000);
        let deposit = treasury.request(ACCOUNT, &body("cd", "deposit", 100, "dummy_bank"), 1_000).expect("requested");
        let withdrawal =
            treasury.request(ACCOUNT, &body("cw", "withdrawal", 300, "dummy_bank"), 1_000).expect("requested");
        treasury.step(&mut group, 1_000);
        assert_eq!(group.sleeves[&ACCOUNT], 700);
        assert_eq!(treasury.cancel(1, deposit, 1_010).map_err(|r| r.status), Err(404), "another account's transfer");
        treasury.cancel(ACCOUNT, deposit, 1_010).expect("cancelled");
        treasury.cancel(ACCOUNT, withdrawal, 1_010).expect("cancelled");
        run(&treasury, &mut group, 1_500);
        assert_eq!(state(&treasury, deposit), ("cancelled".into(), json!("cancelled_by_client")));
        assert_eq!(state(&treasury, withdrawal), ("cancelled".into(), json!("cancelled_by_client")));
        assert_eq!(group.sleeves[&ACCOUNT], 1_000, "the withdrawal was granted back, the deposit never granted");
        let again = treasury.cancel(ACCOUNT, withdrawal, 1_600).map_err(|r| (r.status, r.code));
        assert_eq!(again, Err((409, "TRANSFER_NOT_CANCELLABLE")));
        balanced(&treasury);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn malformed_requests_create_nothing() {
        let p = path("malformed");
        let treasury = open(&p);
        let refused = |b: Value| treasury.request(ACCOUNT, &b, 1_000).map_err(|r| r.status);
        assert_eq!(refused(json!([])), Err(400));
        assert_eq!(refused(body("x", "gift", 1, "dummy_bank")), Err(400));
        assert_eq!(refused(body("x", "deposit", 0, "dummy_bank")), Err(400));
        assert_eq!(refused(body("x", "deposit", 1_000_001, "dummy_bank")), Err(400));
        assert_eq!(refused(body("x", "deposit", 1, "wire")), Err(400));
        assert_eq!(refused(body("", "deposit", 1, "dummy_bank")), Err(400));
        let mut extra = body("x", "deposit", 1, "dummy_bank");
        extra["note"] = json!("hi");
        assert_eq!(refused(extra), Err(400));
        assert_eq!(treasury.request(99, &body("x", "deposit", 1, "dummy_bank"), 1_000).map_err(|r| r.status), Err(404));
        assert_eq!(treasury.list(ACCOUNT, 10).expect("a list")["transfers"], json!([]));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_command_id_is_sixteen_bytes() {
        assert_eq!(command_id(u32::MAX, MAX_TRANSFERS, 'g').len(), 16);
        assert_eq!(command_id(0, 1, 'r'), "T00000000000001r");
    }
}
