//! Where the treasury writes down what it has done: its records of money (blueprint §11), kept in PostgreSQL
//! (`postgres.rs`) and, for unit tests, in memory (`Memory`).
//!
//! Everything is written before anything is done that depends on it, and a write that fails changes nothing the
//! treasury then acts on: a treasury that stops between two writes starts again from the first and repeats only what
//! is safe to repeat, because every command it sends carries a command ID fixed by the transfer, which the sequencer
//! group deduplicates (spec E-7.1). A write whose acknowledgement was lost is safe to repeat too: a transfer's state
//! that is already there is not recorded twice, and a posting carries an idempotency key (`posting_key`) that makes a
//! second write of it a no-op, or a refusal if its entries differ.
//!
//! The treasury keeps a working copy of its records in memory, so that reads and reconciliation need no round trip,
//! and rebuilds it from the records at start (`load`) and after a write whose outcome it could not learn.

use crate::bank::Receipt;
use crate::ledger::{self, Ledger, Posting};
use crate::recon::Break;
use crate::transfer::Transfer;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
pub use venue_store::Error;

/// A treasury's records as it starts from them.
pub struct Loaded {
    pub nonce: u32,
    pub created: u64,
    /// Every ledger account's balance, and how many postings made them.
    pub balances: BTreeMap<String, i64>,
    pub postings: u64,
    /// How much each transfer's postings have moved the FBO omnibus.
    pub fbo: BTreeMap<u64, i64>,
    pub transfers: Vec<Transfer>,
    /// Every break, each with its resolution if it has one.
    pub breaks: Vec<Break>,
    /// The newest runs' reports, oldest first.
    pub runs: Vec<Value>,
    /// The number the next run takes: runs are never numbered twice.
    pub next_run: u64,
}

/// The fake bank's records as it starts from them.
pub struct BankLoaded {
    pub opening: i64,
    pub opened: u64,
    /// The treasury book (its nonce) this bank book belongs to.
    pub treasury_book: u32,
    /// Every transfer the bank received, each with its cancellation if it had one.
    pub receipts: Vec<Receipt>,
    /// How many copies of each transfer's notice the bank has sent.
    pub sent: BTreeMap<u64, u8>,
}

/// The treasury's and the fake bank's records.
pub trait Records: Send + Sync {
    /// Everything but the newest `runs` runs' reports; `None` if no book has been created.
    fn load(&self, runs: usize) -> Result<Option<Loaded>, Error>;
    /// A new book, with the opening balances the environment's seed grants, in one write.
    fn create(&self, nonce: u32, now: u64, opening: &[(&str, Posting)]) -> Result<(), Error>;
    /// A transfer's new state, with the postings that took it there, in one write.
    fn commit(&self, transfer: &Transfer, postings: &[(&str, Posting)], now: u64) -> Result<(), Error>;
    /// A break as found or, once resolved, with its resolution. Each is written once.
    fn record_break(&self, found: &Break) -> Result<(), Error>;
    /// A reconciliation run's report.
    fn record_run(&self, run: u64, status: &str, report: &Value, now: u64) -> Result<(), Error>;
    /// The ledger of record's balances and number of postings, summed where the records are.
    fn trial_balance(&self) -> Result<(BTreeMap<String, i64>, u64), Error>;
    /// Whether the records can be written to now.
    fn ping(&self) -> Result<(), Error>;

    /// The fake bank's book; `None` if it has not been opened.
    fn load_bank(&self) -> Result<Option<BankLoaded>, Error>;
    fn open_bank(&self, opening: i64, now: u64, treasury_book: u32) -> Result<(), Error>;
    /// A transfer the bank received, and the receipt the records hold for it: the first one written, if the bank
    /// had already received it.
    fn receive(&self, receipt: &Receipt) -> Result<Receipt, Error>;
    fn cancel(&self, transfer: u64, at: u64) -> Result<(), Error>;
    /// One copy of a transfer's notice sent, the first being copy 0.
    fn sent(&self, transfer: u64, copy: u8, at: u64) -> Result<(), Error>;
}

/// A posting's idempotency key: the transfer it belongs to and the accounts it moves money between, which name the
/// step of the transfer it records (received at the bank, reclaimed, granted, paid out, returned), each of which
/// happens at most once. An opening balance has no transfer and moves the seed into one customer account.
pub fn posting_key(transfer: Option<u64>, posting: &Posting) -> String {
    match transfer {
        Some(t) => format!("transfer:{t}:{}", posting.accounts()),
        None => format!("opening:{}", posting.accounts()),
    }
}

/// Records held in memory, for tests: shared by every clone, so a treasury opened again on a clone is a restarted
/// one. They keep the rules the database keeps (a posting balances, is written once under its key, and a transfer's
/// terms never change), and can be told to be unreachable.
#[derive(Clone, Default)]
pub struct Memory {
    data: Arc<Mutex<Data>>,
    down: Arc<AtomicBool>,
}

#[derive(Default)]
struct Data {
    book: Option<(u32, u64)>,
    /// Key, transfer, posting.
    postings: Vec<(String, Option<u64>, Posting)>,
    transfers: BTreeMap<u64, Transfer>,
    breaks: BTreeMap<u64, Break>,
    runs: BTreeMap<u64, Value>,
    bank: Option<(i64, u64, u32)>,
    receipts: BTreeMap<u64, Receipt>,
    sent: BTreeMap<u64, u8>,
}

impl Memory {
    /// Makes every call fail as an unreachable database would, or succeed again.
    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    fn data(&self) -> Result<MutexGuard<'_, Data>, Error> {
        if self.down.load(Ordering::SeqCst) {
            return Err(Error::Unavailable("the records are down (a test said so)".into()));
        }
        Ok(self.data.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

fn post(data: &mut Data, transfer: Option<u64>, posting: &Posting) -> Result<(), Error> {
    let key = posting_key(transfer, posting);
    if let Some((_, _, existing)) = data.postings.iter().find(|(k, _, _)| *k == key) {
        return if existing == posting {
            Ok(())
        } else {
            Err(Error::refused(format!("posting {key} was written with other entries")))
        };
    }
    Ledger::default().post(posting).map_err(Error::Refused)?;
    data.postings.push((key, transfer, posting.clone()));
    Ok(())
}

impl Records for Memory {
    fn load(&self, runs: usize) -> Result<Option<Loaded>, Error> {
        let data = self.data()?;
        let Some((nonce, created)) = data.book else { return Ok(None) };
        let mut ledger = Ledger::default();
        let mut fbo: BTreeMap<u64, i64> = BTreeMap::new();
        for (_, transfer, posting) in &data.postings {
            ledger.post(posting).map_err(Error::Refused)?;
            let moved = posting.cents(ledger::FBO);
            if let (Some(t), true) = (transfer, moved != 0) {
                *fbo.entry(*t).or_default() += moved;
            }
        }
        let skip = data.runs.len().saturating_sub(runs);
        let last_run = data.runs.keys().next_back().copied().unwrap_or(0);
        let last_found = data.breaks.values().map(|b| b.found_by).max().unwrap_or(0);
        Ok(Some(Loaded {
            nonce,
            created,
            balances: ledger.balances().clone(),
            postings: ledger.postings(),
            fbo,
            transfers: data.transfers.values().cloned().collect(),
            breaks: data.breaks.values().cloned().collect(),
            runs: data.runs.values().skip(skip).cloned().collect(),
            next_run: last_run.max(last_found) + 1,
        }))
    }

    fn create(&self, nonce: u32, now: u64, opening: &[(&str, Posting)]) -> Result<(), Error> {
        let mut data = self.data()?;
        if data.book.is_some() {
            return Err(Error::refused("a book already exists"));
        }
        for (_, posting) in opening {
            post(&mut data, None, posting)?;
        }
        data.book = Some((nonce, now));
        Ok(())
    }

    fn commit(&self, transfer: &Transfer, postings: &[(&str, Posting)], _now: u64) -> Result<(), Error> {
        let mut data = self.data()?;
        if let Some(t) = data.transfers.get(&transfer.id) {
            let terms = |t: &Transfer| (t.account, t.client_id.clone(), t.kind, t.amount, t.method, t.created);
            if terms(t) != terms(transfer) {
                return Err(Error::refused(format!("transfer {}: its terms cannot change", t.id)));
            }
        } else if data.transfers.values().any(|t| t.account == transfer.account && t.client_id == transfer.client_id) {
            return Err(Error::refused("a client transfer ID used twice"));
        }
        for (_, posting) in postings {
            post(&mut data, Some(transfer.id), posting)?;
        }
        data.transfers.insert(transfer.id, transfer.clone());
        Ok(())
    }

    fn record_break(&self, found: &Break) -> Result<(), Error> {
        let mut data = self.data()?;
        let entry = data.breaks.entry(found.id).or_insert_with(|| found.clone());
        if entry.resolution.is_none() {
            entry.resolution.clone_from(&found.resolution);
        }
        Ok(())
    }

    fn record_run(&self, run: u64, _status: &str, report: &Value, _now: u64) -> Result<(), Error> {
        self.data()?.runs.entry(run).or_insert_with(|| report.clone());
        Ok(())
    }

    fn trial_balance(&self) -> Result<(BTreeMap<String, i64>, u64), Error> {
        let data = self.data()?;
        let mut ledger = Ledger::default();
        for (_, _, posting) in &data.postings {
            ledger.post(posting).map_err(Error::Refused)?;
        }
        Ok((ledger.balances().clone(), ledger.postings()))
    }

    fn ping(&self) -> Result<(), Error> {
        self.data().map(|_| ())
    }

    fn load_bank(&self) -> Result<Option<BankLoaded>, Error> {
        let data = self.data()?;
        Ok(data.bank.map(|(opening, opened, treasury_book)| BankLoaded {
            opening,
            opened,
            treasury_book,
            receipts: data.receipts.values().cloned().collect(),
            sent: data.sent.clone(),
        }))
    }

    fn open_bank(&self, opening: i64, now: u64, treasury_book: u32) -> Result<(), Error> {
        let mut data = self.data()?;
        if data.bank.is_some() {
            return Err(Error::refused("the bank's book already exists"));
        }
        data.bank = Some((opening, now, treasury_book));
        Ok(())
    }

    fn receive(&self, receipt: &Receipt) -> Result<Receipt, Error> {
        Ok(self.data()?.receipts.entry(receipt.transfer).or_insert_with(|| receipt.clone()).clone())
    }

    fn cancel(&self, transfer: u64, at: u64) -> Result<(), Error> {
        let mut data = self.data()?;
        let receipt = data.receipts.get_mut(&transfer).ok_or_else(|| Error::refused("no such receipt"))?;
        receipt.cancelled.get_or_insert(at);
        Ok(())
    }

    fn sent(&self, transfer: u64, copy: u8, _at: u64) -> Result<(), Error> {
        let mut data = self.data()?;
        let sent = data.sent.entry(transfer).or_default();
        *sent = (*sent).max(copy + 1);
        Ok(())
    }
}
