//! The fake bank: the dummy provider of blueprint §16, which moves no money and decides every transfer the way a bank
//! might — after a while, and now and then not in the customer's favour.
//!
//! Its decisions are a function of its seed and the transfer alone, never of the clock or of the order transfers
//! arrive in, so a run can be repeated exactly and a failure found in one can be found again.
//!
//! `Bank` is the bank's side of the story: the transfers it received, what it decided and when, and the FBO omnibus
//! balance that follows. It keeps its own book (tables of its own in the treasury's records), apart from the
//! treasury's, because it stands for another company's system: the treasury learns what the bank did only from its notices, which can go missing (`faults`), and checks
//! its ledger against the bank's statement (`recon`).

use crate::clock;
use crate::faults::Faults;
use crate::records::Records;
use crate::transfer::{Kind, Method, Transfer};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub struct FakeBank {
    pub seed: u64,
    /// How long `dummy_bank` takes to decide, from a transfer reaching it: somewhere in this range.
    pub min_delay_ms: u64,
    pub max_delay_ms: u64,
    /// How often `dummy_bank` fails a transfer, in thousandths.
    pub failure_per_mille: u64,
}

/// What the bank will do with a transfer, and when.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Decision {
    pub delay_ms: u64,
    pub settles: bool,
}

/// SplitMix64's output function: a well-mixed 64 bits from any input, so neighbouring transfer IDs draw unrelated
/// numbers.
pub(crate) const fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

impl FakeBank {
    pub fn decide(&self, transfer: u64, kind: Kind, method: Method) -> Decision {
        let salt = match kind {
            Kind::Deposit => 1,
            Kind::Withdrawal => 2,
        };
        let draw = mix(mix(self.seed ^ mix(transfer)) ^ salt);
        let span = self.max_delay_ms.saturating_sub(self.min_delay_ms).saturating_add(1);
        let delay_ms = self.min_delay_ms.saturating_add((draw >> 16) % span);
        match method {
            Method::DummyInstant => Decision { delay_ms: 0, settles: true },
            Method::DummyFailing => Decision { delay_ms, settles: false },
            Method::DummyBank => Decision { delay_ms, settles: (draw & 0xffff) % 1000 >= self.failure_per_mille },
        }
    }
}

/// A transfer as the bank holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Receipt {
    pub transfer: u64,
    pub account: u64,
    pub kind: Kind,
    pub amount: u64,
    pub received: u64,
    /// When the bank decides it, and which way. Fixed on receipt: the bank's own business, not the treasury's.
    pub decided: u64,
    pub settles: bool,
    pub cancelled: Option<u64>,
    /// When each copy of its notice reaches the treasury: none if it is lost. Drawn on receipt and written down with
    /// it, so a restart with other faults injected neither brings a lost notice back nor loses one on its way.
    pub arrivals: Vec<u64>,
}

/// Where a transfer stands at the bank at some moment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Pending,
    Settled,
    Failed,
    Cancelled,
}

impl Outcome {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Settled => "settled",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

impl Receipt {
    pub fn outcome(&self, at: u64) -> Outcome {
        if self.cancelled.is_some_and(|c| c <= at) {
            Outcome::Cancelled
        } else if self.decided > at {
            Outcome::Pending
        } else if self.settles {
            Outcome::Settled
        } else {
            Outcome::Failed
        }
    }

    /// How much this transfer has moved the omnibus by `at`: a settled deposit brings money in, a settled withdrawal
    /// pays it out, anything else moves nothing.
    pub fn movement(&self, at: u64) -> i64 {
        let amount = i64::try_from(self.amount).unwrap_or(i64::MAX);
        match (self.outcome(at), self.kind) {
            (Outcome::Settled, Kind::Deposit) => amount,
            (Outcome::Settled, Kind::Withdrawal) => -amount,
            _ => 0,
        }
    }
}

/// The bank telling the treasury what it decided about a transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Notice {
    pub transfer: u64,
    pub settles: bool,
}

/// The bank's account of a period, as a statement file or a statement API would give it.
pub struct Statement {
    pub from: u64,
    pub to: u64,
    pub opening: i64,
    pub closing: i64,
    /// Every transfer the bank had received by `to` that was still pending then or was decided or cancelled in the
    /// period, with its outcome as of `to`.
    pub lines: Vec<(Receipt, Outcome)>,
}

impl Statement {
    pub fn to_json(&self) -> Value {
        let lines: Vec<Value> = self
            .lines
            .iter()
            .map(|(r, outcome)| {
                json!({
                    "transferId": r.transfer.to_string(), "accountId": r.account.to_string(), "kind": r.kind.name(),
                    "amountCents": r.amount, "receivedAt": clock::rfc3339(r.received), "outcome": outcome.name(),
                    "valueAt": match outcome {
                        Outcome::Pending => Value::Null,
                        Outcome::Cancelled => json!(r.cancelled.map(clock::rfc3339)),
                        _ => json!(clock::rfc3339(r.decided)),
                    },
                    "movementCents": r.movement(self.to)
                })
            })
            .collect();
        json!({
            "account": crate::ledger::FBO, "from": clock::rfc3339(self.from), "to": clock::rfc3339(self.to),
            "openingCents": self.opening, "closingCents": self.closing, "lines": lines
        })
    }
}

pub struct Bank {
    pub fake: FakeBank,
    faults: Faults,
    records: Arc<dyn Records>,
    /// The omnibus balance when the bank's book was opened: whatever the treasury's ledger said the bank held then.
    opening: i64,
    receipts: BTreeMap<u64, Receipt>,
    /// Notices still to arrive: when, for which transfer, and which copy.
    outbox: BTreeSet<(u64, u64, u8)>,
    /// Copies of each transfer's notice already sent, so a restart does not send them again.
    sent: BTreeMap<u64, u8>,
}

impl Bank {
    /// The bank whose book is in `records`, as it was left. A new book opens with `opening` in the omnibus, for the
    /// treasury book named by `nonce`; an existing one must be that book's. Says whether it is new.
    pub fn open(
        fake: FakeBank,
        faults: Faults,
        records: Arc<dyn Records>,
        nonce: u32,
        opening: i64,
        now: u64,
    ) -> Result<(Self, bool), String> {
        let loaded = records.load_bank()?;
        let fresh = loaded.is_none();
        let mut bank = Self {
            fake,
            faults,
            records,
            opening,
            receipts: BTreeMap::new(),
            outbox: BTreeSet::new(),
            sent: BTreeMap::new(),
        };
        match loaded {
            None => bank.records.open_bank(opening, now, nonce)?,
            Some(book) => {
                if book.treasury_book != nonce {
                    return Err(format!(
                        "the bank's book belongs to treasury book {}, not this one ({nonce})",
                        book.treasury_book
                    ));
                }
                bank.opening = book.opening;
                bank.receipts = book.receipts.into_iter().map(|r| (r.transfer, r)).collect();
                bank.sent = book.sent;
            }
        }
        let ids: Vec<u64> = bank.receipts.keys().copied().collect();
        for id in ids {
            bank.queue(id);
        }
        Ok((bank, fresh))
    }

    /// Reads the book again from the records, after a write whose outcome is not known.
    pub fn reload(&mut self) -> Result<(), String> {
        let book = self.records.load_bank()?.ok_or("the bank's book has gone from the records")?;
        self.opening = book.opening;
        self.receipts = book.receipts.into_iter().map(|r| (r.transfer, r)).collect();
        self.sent = book.sent;
        self.outbox.clear();
        let ids: Vec<u64> = self.receipts.keys().copied().collect();
        for id in ids {
            self.queue(id);
        }
        Ok(())
    }

    fn queue(&mut self, id: u64) {
        let Some(r) = self.receipts.get(&id) else { return };
        if r.cancelled.is_some() {
            return;
        }
        let sent = self.sent.get(&id).copied().unwrap_or(0);
        for (copy, &at) in r.arrivals.iter().enumerate() {
            let copy = u8::try_from(copy).unwrap_or(u8::MAX);
            if copy >= sent {
                self.outbox.insert((at, id, copy));
            }
        }
    }

    /// Takes a transfer in, once: a transfer handed over again, after a treasury stopped between the two, is the one
    /// already received.
    pub fn receive(&mut self, t: &Transfer, now: u64) -> Result<Receipt, String> {
        if let Some(r) = self.receipts.get(&t.id) {
            return Ok(r.clone());
        }
        let decision = self.fake.decide(t.id, t.kind, t.method);
        let receipt = Receipt {
            transfer: t.id,
            account: t.account,
            kind: t.kind,
            amount: t.amount,
            received: now,
            decided: now.saturating_add(decision.delay_ms),
            settles: decision.settles,
            cancelled: None,
            arrivals: self.faults.arrivals(self.fake.seed, t.id, now.saturating_add(decision.delay_ms)),
        };
        // The records keep the first receipt, should a write whose acknowledgement was lost be repeated.
        let receipt = self.records.receive(&receipt)?;
        self.receipts.insert(t.id, receipt.clone());
        self.queue(t.id);
        Ok(receipt)
    }

    /// Cancels a transfer the bank has not decided yet. Says whether it did: once decided, it is too late.
    pub fn cancel(&mut self, id: u64, now: u64) -> Result<bool, String> {
        match self.receipts.get(&id) {
            Some(r) if r.outcome(now) == Outcome::Pending => {}
            _ => return Ok(false),
        }
        self.records.cancel(id, now)?;
        if let Some(r) = self.receipts.get_mut(&id) {
            r.cancelled = Some(now);
        }
        self.outbox.retain(|(_, t, _)| *t != id);
        Ok(true)
    }

    /// The notices that have arrived by `now`, each written down as sent before it is handed over.
    pub fn deliver(&mut self, now: u64) -> Result<Vec<Notice>, String> {
        let mut notices = Vec::new();
        while let Some(&(at, id, copy)) = self.outbox.first() {
            if at > now {
                break;
            }
            self.records.sent(id, copy, now)?;
            self.outbox.pop_first();
            self.sent.insert(id, copy + 1);
            if let Some(r) = self.receipts.get(&id) {
                notices.push(Notice { transfer: id, settles: r.settles });
            }
        }
        Ok(notices)
    }

    /// When the next notice arrives, if one is on its way.
    pub fn next_delivery(&self) -> Option<u64> {
        self.outbox.first().map(|(at, _, _)| *at)
    }

    pub fn receipt(&self, id: u64) -> Option<&Receipt> {
        self.receipts.get(&id)
    }

    pub const fn receipts(&self) -> &BTreeMap<u64, Receipt> {
        &self.receipts
    }

    /// The omnibus balance at `at`.
    pub fn balance(&self, at: u64) -> i64 {
        self.opening.saturating_add(self.receipts.values().map(|r| r.movement(at)).sum())
    }

    pub fn statement(&self, from: u64, to: u64) -> Statement {
        let lines = self
            .receipts
            .values()
            .filter(|r| r.received <= to)
            .filter_map(|r| {
                let outcome = r.outcome(to);
                let when = match outcome {
                    Outcome::Pending => return Some((r.clone(), outcome)),
                    Outcome::Cancelled => r.cancelled.unwrap_or(0),
                    _ => r.decided,
                };
                (when >= from).then(|| (r.clone(), outcome))
            })
            .collect();
        let opening = if from == 0 { self.opening } else { self.balance(from - 1) };
        Statement { from, to, opening, closing: self.balance(to), lines }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::records::Memory;

    fn bank(seed: u64) -> FakeBank {
        FakeBank { seed, min_delay_ms: 1000, max_delay_ms: 3000, failure_per_mille: 100 }
    }

    #[test]
    fn the_same_seed_decides_the_same_way_whatever_the_order() {
        let forwards: Vec<Decision> = (1..200).map(|t| bank(7).decide(t, Kind::Deposit, Method::DummyBank)).collect();
        let mut backwards: Vec<Decision> =
            (1..200).rev().map(|t| bank(7).decide(t, Kind::Deposit, Method::DummyBank)).collect();
        backwards.reverse();
        assert_eq!(forwards, backwards);
        let other: Vec<Decision> = (1..200).map(|t| bank(8).decide(t, Kind::Deposit, Method::DummyBank)).collect();
        assert_ne!(forwards, other, "another seed should decide differently");
    }

    #[test]
    fn delays_stay_in_range_and_failures_are_occasional() {
        let decisions: Vec<Decision> =
            (1..=2000).map(|t| bank(3).decide(t, Kind::Withdrawal, Method::DummyBank)).collect();
        assert!(decisions.iter().all(|d| (1000..=3000).contains(&d.delay_ms)));
        let failed = decisions.iter().filter(|d| !d.settles).count();
        // 10% of 2000, give or take what a fair draw varies by.
        assert!((120..=280).contains(&failed), "{failed} of 2000 failed");
    }

    #[test]
    fn the_instant_and_failing_methods_do_what_they_say() {
        for t in 1..50 {
            assert_eq!(bank(1).decide(t, Kind::Deposit, Method::DummyInstant), Decision { delay_ms: 0, settles: true });
            assert!(!bank(1).decide(t, Kind::Withdrawal, Method::DummyFailing).settles);
        }
    }

    fn transfer(id: u64, kind: Kind, amount: u64, method: Method) -> Transfer {
        Transfer {
            id,
            account: 2,
            client_id: format!("c{id}"),
            kind,
            amount,
            method,
            state: crate::transfer::State::Pending,
            reason: None,
            sequence: None,
            created: 0,
            updated: 0,
            stage: crate::transfer::Stage::ToBank,
            in_doubt: None,
        }
    }

    fn open(records: &Memory, faults: Faults) -> Bank {
        let fake = FakeBank { seed: 1, min_delay_ms: 1000, max_delay_ms: 1000, failure_per_mille: 0 };
        Bank::open(fake, faults, Arc::new(records.clone()), 7, 10_000, 0).expect("a bank").0
    }

    #[test]
    fn the_bank_takes_a_transfer_once_and_its_statement_follows_its_decisions() {
        let records = Memory::default();
        let mut b = open(&records, Faults::NONE);
        let first = b.receive(&transfer(1, Kind::Deposit, 500, Method::DummyBank), 100).expect("received");
        assert_eq!(b.receive(&transfer(1, Kind::Deposit, 500, Method::DummyBank), 900).expect("again"), first);
        b.receive(&transfer(2, Kind::Withdrawal, 200, Method::DummyBank), 100).expect("received");
        b.receive(&transfer(3, Kind::Withdrawal, 300, Method::DummyBank), 100).expect("received");
        assert!(b.cancel(3, 500).expect("cancelled"), "undecided, so cancellable");
        assert!(!b.cancel(2, 1_100).expect("refused"), "decided at 1100, so too late");
        assert_eq!(b.deliver(1_000).expect("nothing yet"), vec![]);
        let notices = b.deliver(1_100).expect("two notices");
        assert_eq!(notices, vec![Notice { transfer: 1, settles: true }, Notice { transfer: 2, settles: true }]);

        let statement = b.statement(0, 2_000);
        assert_eq!((statement.opening, statement.closing), (10_000, 10_300));
        let outcomes: Vec<(u64, Outcome)> = statement.lines.iter().map(|(r, o)| (r.transfer, *o)).collect();
        assert_eq!(outcomes, vec![(1, Outcome::Settled), (2, Outcome::Settled), (3, Outcome::Cancelled)]);
        let later = b.statement(1_500, 2_000);
        assert_eq!((later.opening, later.closing, later.lines.len()), (10_300, 10_300, 0));
        assert_eq!(b.balance(1_000), 10_000);

        // Reopened, it holds the same and sends nothing twice.
        drop(b);
        let mut b = open(&records, Faults::NONE);
        assert_eq!(b.balance(2_000), 10_300);
        assert_eq!(b.deliver(5_000).expect("nothing"), vec![]);
        assert!(b.receipt(3).is_some_and(|r| r.cancelled == Some(500)));
        let fake = b.fake;
        drop(b);
        let other = Bank::open(fake, Faults::NONE, Arc::new(records), 8, 10_000, 0).map(|_| ());
        assert!(other.is_err(), "a bank's book opened for another treasury book");
    }

    #[test]
    fn a_lost_notice_never_comes_and_a_duplicate_comes_twice() {
        let records = Memory::default();
        let mut b = open(&records, Faults { lost_notice_per_mille: 1000, ..Faults::NONE });
        b.receive(&transfer(1, Kind::Deposit, 500, Method::DummyInstant), 100).expect("received");
        assert_eq!(b.deliver(u64::MAX).expect("nothing"), vec![]);
        assert_eq!(b.balance(100), 10_500, "the bank settled it all the same");
        drop(b);
        let mut b = open(&records, Faults::NONE);
        assert_eq!(b.deliver(u64::MAX).expect("still nothing"), vec![], "a lost notice came back after a restart");
        drop(b);
        let mut b = open(&Memory::default(), Faults { duplicate_notice_per_mille: 1000, ..Faults::NONE });
        b.receive(&transfer(1, Kind::Deposit, 500, Method::DummyInstant), 100).expect("received");
        assert_eq!(b.deliver(100).expect("the first").len(), 1);
        assert_eq!(b.next_delivery(), Some(100 + crate::faults::DUPLICATE_GAP_MS));
        assert_eq!(b.deliver(100 + crate::faults::DUPLICATE_GAP_MS).expect("the second").len(), 1);
    }
}
