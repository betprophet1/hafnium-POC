//! The treasury: the account of record for money moving in and out of the venue (blueprint §5, §11).
//!
//! A deposit is sent to the bank and, once the bank settles it, granted to the account's sleeve in the sequencer group.
//! A withdrawal is reclaimed from the sleeve first — the sequencer group decides whether the account has the cash — and
//! only then sent to the bank; if the bank cannot pay it out, or the client cancels it while it waits, it is granted
//! back. Every movement is a balanced posting in the ledger, written down with the transfer's new stage in one write
//! to the records (`records.rs`) before anything is done that depends on it.
//!
//! Nothing here waits on the network while holding the lock: `step` takes one piece of work out, does it, and puts
//! the outcome back.
//!
//! Messages can be lost on the way: a bank's notice, or the answer to a grant. `step` also reconciles, on a schedule
//! and when an operator asks (`recon`), against the bank's statement and what the sequencer group applied.

use crate::bank::{Bank, FakeBank, Notice};
use crate::ledger::{self, Ledger, Posting};
use crate::recon;
use crate::records::{Loaded, Records};
use crate::transfer::{Kind, Method, Reason, Stage, State, Transfer};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
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

/// How soon work that could not be done — a sequencer group that did not answer, records that could not be written —
/// is tried again.
pub(crate) const RETRY_MS: u64 = 1000;

/// Transfer IDs fit in the six hex digits a command ID leaves them (`command_id`).
const MAX_TRANSFERS: u64 = 0xff_ffff;

/// What the sequencer group said to a sleeve adjustment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Adjusted {
    /// Applied at this input sequence.
    Applied(u64),
    /// Rejected, with the engine's reason in lower snake case.
    Rejected(String),
    /// The sequencer group answered `DUPLICATE` — it decided this command before, and E-7.1 does not have it say
    /// how — and the feed has not carried that first outcome yet. `after` is the sequence of the *re-send*, which
    /// is itself a journaled input: the original must lie before it, so once the feed has read past `after` and
    /// the command is still not there, it never will be (`Treasury::strand`).
    Unknown { after: u64 },
    /// Sent, and no answer came back. An error says the command may never have left and is tried again shortly; this
    /// is what a relay sees when an answer goes missing without a failure to show for it, as `faults::Lossy` makes
    /// happen. The transfer waits, in doubt, until reconciliation reads what the sequencer group applied.
    NoAnswer,
}

/// A treasury command as the sequencer group's journal records it, with its first outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recorded {
    pub account: u64,
    pub cents: u64,
    /// `GRANT` or `RECLAIM`.
    pub reason: String,
    /// `Applied` or `Rejected`.
    pub outcome: Adjusted,
}

/// Every treasury command the journal holds so far, by command ID, and the range of the journal that is.
///
/// The range has two ends, and both matter. `through` is the obvious one. `from` is the sequence this reader was
/// founded at — the point below which it has read nothing and never will, because it was handed a starting
/// position rather than the whole history (`venue-view snapshot`, `core/link/src/pair.rs`). Without it a founded
/// reader would take "sequenced at or before `through`" to mean "in `commands` if it is anywhere", and report
/// every transfer older than its floor as a command the engine never applied.
///
/// It is deliberately *not* filled in from the treasury's own records. Reconciliation compares what the treasury
/// believes against what the journal independently says; sourcing the journal's side from the treasury's own
/// database would make the comparison vacuous. A reader that cannot check a range says so, rather than checking
/// it against itself.
#[derive(Clone, Debug, Default)]
pub struct Journal {
    pub commands: BTreeMap<String, Recorded>,
    /// The sequence below which nothing was read: 0 for a reader that started from the beginning, so the usual
    /// case is unchanged. A command at or before it is outside what this reader can speak to.
    pub from: u64,
    /// The highest input sequence read: a command sequenced after `from` and at or before this is in `commands`
    /// if it is anywhere.
    pub through: u64,
}

impl Journal {
    /// Whether this reader has actually read the stretch of journal a command at `sequence` would sit in, and so
    /// whether its absence from `commands` means anything.
    pub fn covers(&self, sequence: u64) -> bool {
        sequence > self.from && sequence <= self.through
    }
}

/// The sequencer group, as far as the treasury needs it: sleeve grants and reclaims (spec E-7.2), and the record of
/// those it has applied.
pub trait Sequencer {
    fn adjust(&mut self, command_id: &str, account: u64, cents: u64, reason: &str) -> Result<Adjusted, String>;
    fn journal(&self) -> Journal;
}

/// A refusal at the treasury's edge, in interface 1's `Error` form. Nothing was created or changed.
#[derive(Debug, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    pub(crate) fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
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
    /// Lost messages to inject, and when to reconcile.
    pub recon: recon::Options,
}

pub(crate) struct Inner {
    /// Unique to this book, so that its command IDs never meet those of an earlier book on the same sequencer group.
    pub(crate) nonce: u32,
    pub(crate) transfers: BTreeMap<u64, Transfer>,
    by_client: BTreeMap<(u64, String), u64>,
    pub(crate) ledger: Ledger,
    pub(crate) bank: Bank,
    /// A write to the records failed, so they may hold something this working copy does not: it is read again from
    /// them before anything else is written.
    pub(crate) stale: bool,
    /// The bank's notices that have arrived and are not yet acted on.
    inbox: VecDeque<Notice>,
    /// How much each transfer's postings have moved the FBO omnibus, to match against the bank's statement line by
    /// line.
    pub(crate) fbo: BTreeMap<u64, i64>,
    pub(crate) recon: recon::State,
    /// Transfers that changed, oldest first, each numbered, for streams.
    updates: VecDeque<(u64, u64, Value)>,
    next_update: u64,
    /// When work that failed may be tried again, by transfer.
    pub(crate) retry_at: BTreeMap<u64, u64>,
    /// Transfers whose command the sequencer group deduplicated and whose first outcome the feed will never
    /// carry, with what is known about why: retrying one of these forever would never learn anything, so it is
    /// stopped and reported instead (`strand`, and `duplicate_unresolvable` in `recon`).
    ///
    /// Held in memory rather than in the records on purpose. A restart re-sends the command, is answered
    /// `DUPLICATE` again, and works the same conclusion out again once the feed has caught up — so this rebuilds
    /// itself, and needs no migration to hold something derived.
    pub(crate) unresolvable: BTreeMap<u64, String>,
    /// Counts every change, so that whoever waits for one cannot miss one made just before it started waiting.
    pub(crate) generation: u64,
}

pub struct Treasury {
    pub(crate) config: Config,
    pub(crate) records: Arc<dyn Records>,
    pub(crate) inner: Mutex<Inner>,
    /// Signalled whenever anything changes: a new transfer for `step`, a new stage for a request waiting on one, a new
    /// update for a stream.
    pub changed: Condvar,
}

/// One piece of work that needs the sequencer group.
pub(crate) struct Command {
    pub(crate) transfer: u64,
    pub(crate) command_id: String,
    account: u64,
    cents: u64,
    reason: &'static str,
}

impl Command {
    /// The reclaim or grant a transfer's stage waits on, if it waits on one.
    pub(crate) fn for_transfer(nonce: u32, t: &Transfer) -> Option<Self> {
        let (what, reason) = match t.stage {
            Stage::Reclaiming => ('r', "RECLAIM"),
            Stage::Granting { .. } => ('g', "GRANT"),
            _ => return None,
        };
        Some(Self {
            transfer: t.id,
            command_id: command_id(nonce, t.id, what),
            account: t.account,
            cents: t.amount,
            reason,
        })
    }
}

/// The ledger, transfers by ID and by client transfer ID, and each transfer's omnibus movement.
type WorkingCopy = (Ledger, BTreeMap<u64, Transfer>, BTreeMap<(u64, String), u64>, BTreeMap<u64, i64>);

pub(crate) fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
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

/// The command ID of a transfer's reclaim (`r`) or grant (`g`): fixed by the transfer, so that sending it again after
/// a restart is answered as a duplicate rather than moving the money twice (spec E-7.1). Sixteen bytes exactly.
pub(crate) fn command_id(nonce: u32, transfer: u64, what: char) -> String {
    format!("T{nonce:08x}{transfer:06x}{what}")
}

/// The ledger account that holds a transfer's money between the bank and the sequencer group.
const fn in_transit(kind: Kind) -> &'static str {
    match kind {
        Kind::Deposit => ledger::DEPOSITS_IN_TRANSIT,
        Kind::Withdrawal => ledger::WITHDRAWALS_IN_TRANSIT,
    }
}

fn text<'a>(fields: &'a Map<String, Value>, name: &str) -> Result<&'a str, Refusal> {
    fields.get(name).and_then(Value::as_str).ok_or_else(|| Refusal::bad(format!("{name} is required, as a string")))
}

impl Treasury {
    /// A treasury whose book is in `records`, as it was left. A new book records the seed's grants as opening balances
    /// held at the fake bank, so the ledger starts where the sequencer group does and the omnibus covers every
    /// customer's cash from the first posting (blueprint §11): a withdrawal of seeded money is paid out of money the
    /// bank holds, not out of nothing.
    pub fn open(config: Config, records: Arc<dyn Records>, now: u64) -> Result<Self, String> {
        let loaded = match records.load(recon::RUNS)? {
            Some(loaded) => loaded,
            None => {
                let memo = "opening balance: the environment's seed grant, held at the fake bank";
                let mut opening = Vec::new();
                for (&account, &cents) in config.accounts.iter().filter(|(_, c)| **c > 0) {
                    opening.push((memo, Posting::moving(cents, ledger::FBO, &ledger::customer(account))?));
                }
                records.create(nonce()?, now, &opening)?;
                records.load(recon::RUNS)?.ok_or("the book just created is not in the records")?
            }
        };
        let nonce = loaded.nonce;
        let mut recon = recon::State::new(&config.recon, now);
        let (ledger, transfers, by_client, fbo) = Self::working_copy(loaded, &mut recon)?;
        // The bank opens holding what the ledger says it holds, so a bank added beside an older book starts level
        // with it rather than with nothing.
        let (mut bank, bank_fresh) = Bank::open(
            config.bank,
            config.recon.faults,
            Arc::clone(&records),
            nonce,
            ledger.balance(ledger::FBO),
            now,
        )?;
        if bank_fresh {
            for t in transfers.values().filter(|t| matches!(t.stage, Stage::AtBank { .. })) {
                bank.receive(t, now)?;
            }
        }
        let inner = Inner {
            nonce,
            transfers,
            by_client,
            ledger,
            bank,
            stale: false,
            inbox: VecDeque::new(),
            fbo,
            recon,
            updates: VecDeque::new(),
            next_update: 0,
            retry_at: BTreeMap::new(),
            unresolvable: BTreeMap::new(),
            generation: 0,
        };
        Ok(Self { config, records, inner: Mutex::new(inner), changed: Condvar::new() })
    }

    /// The working copy of what the records hold: the ledger, transfers by ID and by client transfer ID, and the
    /// omnibus movement of each transfer; breaks and runs go into `recon`.
    fn working_copy(loaded: Loaded, recon: &mut recon::State) -> Result<WorkingCopy, String> {
        let total: i128 = loaded.balances.values().map(|b| i128::from(*b)).sum();
        if total != 0 {
            return Err(format!("the ledger of record sums to {total} cents, not 0"));
        }
        let ledger = Ledger::restore(loaded.balances, loaded.postings);
        let by_client = loaded.transfers.iter().map(|t| ((t.account, t.client_id.clone()), t.id)).collect();
        let transfers = loaded.transfers.into_iter().map(|t| (t.id, t)).collect();
        recon.restore(loaded.breaks, loaded.runs, loaded.next_run);
        Ok((ledger, transfers, by_client, loaded.fbo))
    }

    /// Reads the working copy again from the records, after a write whose outcome is not known: whatever did reach
    /// them is taken up, and streams are told of every transfer that looks different.
    pub(crate) fn resync(&self, inner: &mut Inner) -> Result<(), String> {
        let loaded = self.records.load(recon::RUNS)?.ok_or("the book has gone from the records")?;
        if loaded.nonce != inner.nonce {
            return Err(format!("the records now hold another book ({:08x})", loaded.nonce));
        }
        inner.bank.reload()?;
        let (ledger, transfers, by_client, fbo) = Self::working_copy(loaded, &mut inner.recon)?;
        for t in transfers.values() {
            if inner.transfers.get(&t.id).map(Transfer::visible) != Some(t.visible()) {
                let n = inner.next_update;
                inner.next_update += 1;
                inner.updates.push_back((n, t.account, t.to_json()));
            }
        }
        while inner.updates.len() > UPDATES {
            inner.updates.pop_front();
        }
        (inner.ledger, inner.transfers, inner.by_client, inner.fbo) = (ledger, transfers, by_client, fbo);
        inner.stale = false;
        inner.generation += 1;
        self.changed.notify_all();
        eprintln!("treasury: the working copy was read again from the records");
        Ok(())
    }

    /// Reads the working copy again if a write failed since it was last read; an error if the records cannot be read.
    pub(crate) fn fresh(&self, inner: &mut Inner) -> Result<(), String> {
        if inner.stale {
            self.resync(inner)?;
        }
        Ok(())
    }

    /// Whether the records answer, for health checks.
    pub fn health(&self) -> Result<(), String> {
        self.records.ping().map_err(String::from)
    }

    /// Writes a transfer's new stage, with the postings that go with it, and only then applies them: a line that
    /// could not be written changes nothing. Tells streams and waiters if a client can see the difference.
    pub(crate) fn commit(
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
        self.fresh(inner)?;
        if let Err(e) = self.records.commit(&next, postings, now) {
            inner.stale = true;
            return Err(e.into());
        }
        for (_, posting) in postings {
            inner.ledger.post(posting)?;
            let moved = posting.cents(ledger::FBO);
            if moved != 0 {
                *inner.fbo.entry(next.id).or_default() += moved;
            }
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

    /// Whether the treasury may act for this account: one the catalogue named, or one it has already made a
    /// transfer for.
    ///
    /// An account the catalogue never named first appears by depositing (see `request`), and is known from then on
    /// — which is what lets its transfers be read back and withdrawn from afterwards. Asked of `by_client`, which
    /// is keyed by account and restored from the records, so this costs a lookup and keeps no second list that
    /// could disagree with them.
    fn known(&self, inner: &Inner, account: u64) -> Result<(), Refusal> {
        let transacted =
            inner.by_client.range((account, String::new())..).next().is_some_and(|((a, _), _)| *a == account);
        if self.config.accounts.contains_key(&account) || transacted {
            Ok(())
        } else {
            Err(Refusal::not_found("no such account"))
        }
    }

    /// POST: a new transfer, or the one this client transfer ID already made. Returns its ID; the caller may wait
    /// for it to get past its quick stages (`wait_quick`) before answering.
    pub fn request(&self, account: u64, body: &Value, now: u64) -> Result<u64, Refusal> {
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
        // A deposit is how an account the catalogue never named first appears. The member exists, the sequencer
        // group already knows the account — it refuses their orders for want of a sleeve, not for want of an
        // account — and this is the money arriving, recorded like any other deposit: whose it is, how much, by
        // which method, and every state it passed through.
        //
        // A withdrawal is not. Nothing can be taken out of an account that has never had anything put in, and
        // refusing here is what stops a mistyped account number opening one and being paid.
        if kind == Kind::Withdrawal {
            self.known(&inner, account)?;
        }
        self.fresh(&mut inner).map_err(|e| {
            eprintln!("treasury: {e}");
            Refusal::unavailable("the treasury's records are unavailable; nothing was done")
        })?;
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
            in_doubt: None,
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
        self.fresh(&mut inner).map_err(|e| {
            eprintln!("treasury: {e}");
            Refusal::unavailable("the treasury's records are unavailable; nothing was done")
        })?;
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
        // The bank has a say once it has the transfer: it cannot take back what it has already decided, whether or not
        // its notice has arrived.
        if matches!(t.stage, Stage::AtBank { .. }) {
            let cancelled = inner.bank.cancel(id, now).map_err(|e| {
                inner.stale = true;
                eprintln!("treasury: {e}");
                Refusal::unavailable("the bank could not record the cancellation; nothing was done")
            })?;
            if !cancelled {
                return Err(Refusal::new(
                    409,
                    "TRANSFER_NOT_CANCELLABLE",
                    format!("the bank has already decided transfer {id}"),
                ));
            }
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
        let inner = locked(&self.inner);
        self.known(&inner, account)?;
        inner
            .transfers
            .get(&id)
            .filter(|t| t.account == account)
            .map(Transfer::to_json)
            .ok_or_else(|| Refusal::not_found("no such transfer"))
    }

    /// The account's newest transfers, newest first.
    pub fn list(&self, account: u64, limit: usize) -> Result<Value, Refusal> {
        let inner = locked(&self.inner);
        self.known(&inner, account)?;
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
            i.transfers.get(&id).is_some_and(|t| {
                t.in_doubt.is_none() && matches!(t.stage, Stage::Reclaiming | Stage::ToBank | Stage::Granting { .. })
            })
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
            if let Err(e) = self.fresh(&mut locked(&self.inner)) {
                eprintln!("treasury: the records: {e}");
                return Duration::from_millis(RETRY_MS);
            }
            let due = locked(&self.inner).recon.due(now);
            if let Some(trigger) = due {
                // The journal is read before the lock is taken, so it is never newer than the transfers it is matched
                // against: a command the treasury has seen answered is on the feed or the feed is behind it.
                let journal = sequencer.journal();
                let mut inner = locked(&self.inner);
                if self.reconcile(&mut inner, &journal, trigger, now) {
                    continue;
                }
                return Duration::from_millis(RETRY_MS);
            }
            let command = {
                let mut inner = locked(&self.inner);
                match self.local_work(&mut inner, now) {
                    Ok(true) => continue,
                    Ok(false) => {}
                    Err(e) => {
                        inner.stale = true;
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
            // A duplicate whose first outcome the feed has already run past is never going to arrive, and
            // retrying it every second for ever would not learn anything. `after` is the re-send's own sequence,
            // so the original lies before it: once the feed has read that far and the command is still not
            // there, it is either below the floor this reader was founded at, or — for a reader that started at
            // the beginning, where nothing can lie below the floor — a real inconsistency worth an operator.
            if let Ok(Adjusted::Unknown { after }) = outcome {
                let journal = sequencer.journal();
                if journal.through >= after {
                    let mut inner = locked(&self.inner);
                    if let Err(e) = self.strand(&mut inner, &command, after, &journal, now) {
                        eprintln!("treasury: transfer {}: {e}", command.transfer);
                        inner.retry_at.insert(command.transfer, now + RETRY_MS);
                    }
                    continue;
                }
            }
            let mut inner = locked(&self.inner);
            if let Err(e) = self.apply(&mut inner, &command, outcome, now) {
                eprintln!("treasury: transfer {}: {e}", command.transfer);
                inner.retry_at.insert(command.transfer, now + RETRY_MS);
            }
        }
    }

    /// One stage that needs nothing but the bank, if there is one due. Says whether it did anything.
    fn local_work(&self, inner: &mut Inner, now: u64) -> Result<bool, String> {
        let arrived = inner.bank.deliver(now)?;
        inner.inbox.extend(arrived);
        if let Some(mut t) = inner.transfers.values().find(|t| t.stage == Stage::ToBank).cloned() {
            let receipt = inner.bank.receive(&t, now)?;
            t.stage = Stage::AtBank { due: receipt.decided };
            // A bank that decides at once is decided in the same breath, so nobody waiting sees it at the bank.
            let arrived = inner.bank.deliver(now)?;
            inner.inbox.extend(arrived);
            let mut postings = Vec::new();
            if let Some(i) = inner.inbox.iter().position(|n| n.transfer == t.id) {
                let notice = inner.inbox.remove(i).ok_or("a notice that vanished")?;
                postings.extend(Self::decide(&mut t, notice.settles)?);
            }
            self.commit(inner, t, &postings, now)?;
            return Ok(true);
        }
        let Some(notice) = inner.inbox.pop_front() else { return Ok(false) };
        let at_bank = inner.transfers.get(&notice.transfer).filter(|t| matches!(t.stage, Stage::AtBank { .. }));
        let Some(mut t) = at_bank.cloned() else {
            // Already past the bank: a duplicate, or a notice that came after reconciliation booked it. Acting on it
            // again would post it twice; reconciliation reports that it came.
            inner.recon.ignored.push((notice.transfer, now));
            return Ok(true);
        };
        let postings: Vec<_> = Self::decide(&mut t, notice.settles)?.into_iter().collect();
        if let Err(e) = self.commit(inner, t, &postings, now) {
            inner.inbox.push_front(notice);
            return Err(e);
        }
        Ok(true)
    }

    /// Moves a transfer at the bank past it, the way the bank decided, and returns the posting that goes with it.
    pub(crate) fn decide(t: &mut Transfer, settles: bool) -> Result<Option<(&'static str, Posting)>, String> {
        Ok(match (settles, t.kind) {
            (true, Kind::Deposit) => {
                t.stage = Stage::Granting { then: State::Settled, reason: None };
                Some((
                    "deposit received at the bank",
                    Posting::moving(t.amount, ledger::FBO, ledger::DEPOSITS_IN_TRANSIT)?,
                ))
            }
            (false, Kind::Deposit) => {
                (t.state, t.reason, t.stage) = (State::Failed, Some(Reason::DeclinedByBank), Stage::Done);
                None
            }
            (true, Kind::Withdrawal) => {
                (t.state, t.stage) = (State::Settled, Stage::Done);
                Some((
                    "withdrawal paid out by the bank",
                    Posting::moving(t.amount, ledger::WITHDRAWALS_IN_TRANSIT, ledger::FBO)?,
                ))
            }
            (false, Kind::Withdrawal) => {
                t.stage = Stage::Granting { then: State::Failed, reason: Some(Reason::ReturnedByBank) };
                None
            }
        })
    }

    fn next_command(inner: &Inner, now: u64) -> Option<Command> {
        let ready = |t: &&Transfer| t.in_doubt.is_none() && inner.retry_at.get(&t.id).is_none_or(|at| *at <= now);
        inner.transfers.values().filter(ready).find_map(|t| Command::for_transfer(inner.nonce, t))
    }

    fn next_due(inner: &Inner, now: u64) -> Duration {
        let retries = inner.transfers.values().filter_map(|t| match t.stage {
            Stage::Reclaiming | Stage::Granting { .. } => inner.retry_at.get(&t.id).copied(),
            _ => None,
        });
        let due = retries.chain(inner.bank.next_delivery()).chain([inner.recon.due_at]).min();
        Duration::from_millis(due.map_or(RETRY_MS, |d| d.saturating_sub(now).clamp(1, RETRY_MS)))
    }

    /// Stops retrying a command whose first outcome can never be learned, and leaves it for an operator.
    ///
    /// The transfer is left exactly where it stands, in doubt: nothing is guessed about which way the original
    /// went, because guessing wrong either pays a customer twice or strands their money. What changes is that the
    /// treasury stops asking a question that has no answer, and says so where reconciliation will report it —
    /// a stage like `Reclaiming` is `Expect::InFlight` to reconciliation, so without this the transfer would sit
    /// silently for ever, which is worse than the retry loop it replaces rather than better.
    pub(crate) fn strand(
        &self,
        inner: &mut Inner,
        command: &Command,
        after: u64,
        journal: &Journal,
        now: u64,
    ) -> Result<(), String> {
        let Some(mut t) = inner.transfers.get(&command.transfer).cloned() else { return Ok(()) };
        let detail = if journal.from > 0 {
            format!(
                "{} was deduplicated by the sequencer group when re-sent at sequence {after}, so its first \
                 outcome is before that — and before {}, which this treasury was founded at and has not read",
                command.command_id, journal.from
            )
        } else {
            format!(
                "{} was deduplicated by the sequencer group when re-sent at sequence {after}, so its first \
                 outcome is before that; the journal read over {}..{} does not carry it, which it must if the \
                 group decided it",
                command.command_id, journal.from, journal.through
            )
        };
        eprintln!("treasury: transfer {}: {detail}", t.id);
        inner.unresolvable.insert(t.id, detail);
        inner.retry_at.remove(&t.id);
        t.in_doubt = Some(now);
        self.commit(inner, t, &[], now)
    }

    /// Puts what the sequencer group said about a command back into its transfer.
    pub(crate) fn apply(
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
            // Retryable, and meant to be: the feed has not reached the first outcome yet. `step` takes the other
            // case — the feed having gone past it, so that it is never coming — before ever getting here.
            (_, Adjusted::Unknown { .. }) => return Err("a duplicate whose first outcome is not known yet".into()),
            (Stage::Reclaiming | Stage::Granting { .. }, Adjusted::NoAnswer) => {
                eprintln!("treasury: transfer {}: {} sent and no answer came back", t.id, command.command_id);
                t.in_doubt = Some(now);
                inner.retry_at.remove(&t.id);
                return self.commit(inner, t, &[], now);
            }
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
        t.in_doubt = None;
        inner.retry_at.remove(&t.id);
        self.commit(inner, t, &postings, now)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::ledger::{customer, DEPOSITS_IN_TRANSIT, FBO, WITHDRAWALS_IN_TRANSIT};
    use crate::records::Memory;

    /// A sequencer group that keeps sleeves and remembers command IDs as E-7.1 and E-7.2 say, and can be told to
    /// stop answering.
    #[derive(Default)]
    pub(crate) struct Group {
        pub(crate) sleeves: BTreeMap<u64, i64>,
        pub(crate) outcomes: BTreeMap<String, Adjusted>,
        pub(crate) journal: Journal,
        sequence: u64,
        pub(crate) down: bool,
        pub(crate) sent: usize,
        /// Answer everything as a duplicate whose first outcome this reader does not hold, as the real group
        /// does for a command it decided before the sequence a founded treasury was founded at.
        pub(crate) answer_unknown_after: Option<u64>,
    }

    impl Sequencer for Group {
        fn adjust(&mut self, command_id: &str, account: u64, cents: u64, reason: &str) -> Result<Adjusted, String> {
            if self.down {
                return Err("down".into());
            }
            self.sent += 1;
            self.sequence += 1;
            if let Some(after) = self.answer_unknown_after {
                return Ok(Adjusted::Unknown { after });
            }
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
            let recorded =
                Recorded { account, cents: cents.unsigned_abs(), reason: reason.to_string(), outcome: outcome.clone() };
            self.journal.commands.insert(command_id.to_string(), recorded);
            self.journal.through = self.sequence;
            Ok(outcome)
        }

        fn journal(&self) -> Journal {
            self.journal.clone()
        }
    }

    pub(crate) const ACCOUNT: u64 = 2;

    pub(crate) fn open(records: &Memory) -> Treasury {
        open_with(records, recon::Options { every_ms: 0, ..recon::Options::default() })
    }

    pub(crate) fn open_with(records: &Memory, recon: recon::Options) -> Treasury {
        open_on(Arc::new(records.clone()), recon)
    }

    pub(crate) fn open_on(records: Arc<dyn Records>, recon: recon::Options) -> Treasury {
        let config = Config {
            bank: FakeBank { seed: 5, min_delay_ms: 100, max_delay_ms: 200, failure_per_mille: 0 },
            max_cents: 1_000_000,
            accounts: [(1, 0), (ACCOUNT, 50_000)].into_iter().collect(),
            recon,
        };
        Treasury::open(config, records, 1_000).expect("a treasury")
    }

    pub(crate) fn body(client: &str, kind: &str, cents: u64, method: &str) -> Value {
        json!({"clientTransferId": client, "kind": kind, "amountCents": cents, "method": method})
    }

    pub(crate) fn state(treasury: &Treasury, id: u64) -> (String, Value) {
        let t = treasury.get(ACCOUNT, id).expect("the transfer");
        (t["state"].as_str().unwrap_or("").to_string(), t["reason"].clone())
    }

    /// Runs the treasury until nothing more is due before `until`.
    pub(crate) fn run(treasury: &Treasury, group: &mut Group, until: u64) {
        for now in (1_000..=until).step_by(50) {
            treasury.step(group, now);
        }
    }

    /// The ledger sums to zero, and the omnibus holds exactly the customers' cash plus what is in transit
    /// (blueprint §11: the sum of customer cash equals the FBO balance minus in-flight transfers).
    pub(crate) fn balanced(treasury: &Treasury) {
        let trial = treasury.trial_balance();
        assert_eq!(trial["total"], "0", "{trial}");
        let customers: i64 = [1, ACCOUNT].iter().map(|a| treasury.balance(&customer(*a))).sum();
        let transit = treasury.balance(DEPOSITS_IN_TRANSIT) + treasury.balance(WITHDRAWALS_IN_TRANSIT);
        assert_eq!(treasury.balance(FBO), -customers - transit, "{trial}");
    }

    #[test]
    fn a_settled_deposit_is_granted_once_and_the_ledger_balances() {
        let p = Memory::default();
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
    }

    #[test]
    fn a_withdrawal_beyond_available_cash_is_refused_and_moves_nothing() {
        let p = Memory::default();
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
    }

    #[test]
    fn a_withdrawal_is_reclaimed_before_the_bank_and_paid_out_after() {
        let p = Memory::default();
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
    }

    #[test]
    fn what_the_bank_fails_is_never_granted_or_is_granted_back() {
        let p = Memory::default();
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
    }

    #[test]
    fn a_client_transfer_id_makes_one_transfer_however_often_it_is_sent() {
        let p = Memory::default();
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
    }

    #[test]
    fn a_grant_sent_again_after_a_restart_moves_the_money_once() {
        let p = Memory::default();
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
    }

    /// Passes each command on and then loses the answer, as a treasury that stops at that moment would.
    struct LostAnswer<'a>(&'a mut Group);

    impl Sequencer for LostAnswer<'_> {
        fn adjust(&mut self, command_id: &str, account: u64, cents: u64, reason: &str) -> Result<Adjusted, String> {
            self.0.adjust(command_id, account, cents, reason)?;
            Err("the answer was lost".into())
        }

        fn journal(&self) -> Journal {
            self.0.journal()
        }
    }

    #[test]
    fn a_sequencer_group_that_does_not_answer_leaves_the_transfer_to_be_tried_again() {
        let p = Memory::default();
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
    }

    #[test]
    fn only_a_transfer_waiting_on_the_bank_can_be_cancelled() {
        let p = Memory::default();
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
    }

    #[test]
    fn an_account_the_catalogue_never_named_appears_by_depositing_and_cannot_withdraw_before_it_has() {
        let records = Memory::default();
        let treasury = open(&records);
        // A member's account: made at sign-up, in no catalogue, and so unknown here until it puts money in.
        const MEMBER: u64 = 100_002;

        // Nothing can be taken out of an account that has never had anything put in. This is what stops a mistyped
        // account number opening one and being paid out of the omnibus.
        let out = body("w1", "withdrawal", 100, "dummy_instant");
        assert_eq!(
            treasury.request(MEMBER, &out, 2_000).unwrap_err().code,
            "NOT_FOUND",
            "an account nobody has funded was allowed to withdraw"
        );
        assert!(treasury.list(MEMBER, 10).is_err(), "nor read back transfers it does not have");

        // A deposit opens it, and is recorded as any other deposit is.
        let id = treasury.request(MEMBER, &body("d1", "deposit", 100, "dummy_instant"), 2_000).expect("deposited");
        let made = treasury.get(MEMBER, id).expect("its own transfer");
        assert_eq!((&made["kind"], &made["amountCents"]), (&json!("deposit"), &json!(100)), "{made}");

        // And from then on it is an account the treasury knows: it can be read back, and withdrawn from — refused
        // now for want of money rather than for want of an account, which is a different answer.
        assert_eq!(treasury.list(MEMBER, 10).expect("listed")["transfers"].as_array().map(Vec::len), Some(1));
        let refused = treasury.request(MEMBER, &body("w2", "withdrawal", 100_000, "dummy_instant"), 2_000);
        assert_ne!(
            refused.as_ref().err().map(|e| e.code),
            Some("NOT_FOUND"),
            "having deposited, the account is known: {refused:?}"
        );

        // The catalogue's own accounts are unaffected.
        assert!(treasury.request(ACCOUNT, &body("d2", "deposit", 100, "dummy_instant"), 2_000).is_ok());
    }

    #[test]
    fn malformed_requests_create_nothing() {
        let p = Memory::default();
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
        // A withdrawal names an account that must already exist. A deposit no longer does — that is how an account
        // the catalogue never named opens — so the refusal this asserts is the one that is left.
        assert_eq!(
            treasury.request(99, &body("x", "withdrawal", 1, "dummy_bank"), 1_000).map_err(|r| r.status),
            Err(404)
        );
        assert_eq!(treasury.list(ACCOUNT, 10).expect("a list")["transfers"], json!([]));
    }

    #[test]
    fn records_that_cannot_be_reached_refuse_new_transfers_and_nothing_is_lost_when_they_return() {
        let p = Memory::default();
        let treasury = open(&p);
        let mut group = Group::default();
        group.sleeves.insert(ACCOUNT, 1_000);
        let before =
            treasury.request(ACCOUNT, &body("before", "deposit", 300, "dummy_bank"), 1_000).expect("requested");
        treasury.step(&mut group, 1_000);
        p.set_down(true);
        let refused = treasury.request(ACCOUNT, &body("during", "deposit", 100, "dummy_instant"), 1_010);
        assert_eq!(refused.map_err(|r| (r.status, r.code)), Err((503, "UNAVAILABLE")));
        assert!(treasury.health().is_err());
        // Reads are answered from the working copy; the bank's decision waits, and so does reconciliation.
        assert_eq!(state(&treasury, before).0, "pending");
        run(&treasury, &mut group, 1_500);
        assert_eq!(state(&treasury, before).0, "pending", "moved on while its records could not be written");
        locked(&treasury.inner).recon.requested = true;
        treasury.step(&mut group, 1_600);
        assert!(locked(&treasury.inner).recon.runs.is_empty(), "a run while the records were down");
        p.set_down(false);
        for now in (6_700..=7_500).step_by(50) {
            treasury.step(&mut group, now);
        }
        assert_eq!(state(&treasury, before), ("settled".into(), Value::Null));
        assert_eq!(locked(&treasury.inner).recon.runs.len(), 1, "the postponed run ran once the records were back");
        treasury.request(ACCOUNT, &body("after", "deposit", 100, "dummy_instant"), 7_600).expect("accepted again");
        balanced(&treasury);
        let reopened = open(&p);
        assert_eq!(state(&reopened, before).0, "settled");
    }

    #[test]
    fn a_command_id_is_sixteen_bytes() {
        assert_eq!(command_id(u32::MAX, MAX_TRANSFERS, 'g').len(), 16);
        assert_eq!(command_id(0, 1, 'r'), "T00000000000001r");
    }
}
