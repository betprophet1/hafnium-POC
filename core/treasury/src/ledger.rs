//! The treasury's double-entry ledger (blueprint §11): integer cents, every posting balanced, nothing ever updated.
//! A correction is a further posting. Balances are debits positive and credits negative, so every posting sums to
//! zero and so does the whole ledger, whatever it has been through.

use serde_json::{json, Value};
use std::collections::BTreeMap;

/// The bank account that holds customer money: the FBO omnibus of blueprint §11, here the fake bank's.
pub const FBO: &str = "bank:fbo_omnibus";
/// Deposits the bank has settled and the sequencer group has not yet granted.
pub const DEPOSITS_IN_TRANSIT: &str = "transit:deposits";
/// Withdrawals taken from customers and not yet paid out by the bank, or not yet granted back.
pub const WITHDRAWALS_IN_TRANSIT: &str = "transit:withdrawals";

/// An account's cash as the treasury has moved it: what came in through deposits and the seed, less what went out
/// through withdrawals. Trading moves cash inside the sequencer group, which this ledger does not see yet.
pub fn customer(account: u64) -> String {
    format!("customer:{account}:cash")
}

/// One balanced movement of money between ledger accounts, in the order it is written down.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Posting(Vec<(String, i64)>);

impl Posting {
    /// `cents` from `credit` to `debit`: the debit account's balance rises and the credit account's falls.
    pub fn moving(cents: u64, debit: &str, credit: &str) -> Result<Self, String> {
        let cents = i64::try_from(cents).map_err(|_| "an amount beyond what a ledger entry holds".to_string())?;
        Ok(Self(vec![(debit.to_string(), cents), (credit.to_string(), -cents)]))
    }

    /// A posting of these entries, as the records hold it. `Ledger::post` checks that it balances.
    pub const fn from_entries(entries: Vec<(String, i64)>) -> Self {
        Self(entries)
    }

    /// Its entries, in the order they are written down.
    pub fn entries(&self) -> &[(String, i64)] {
        &self.0
    }

    /// The accounts it moves money between, in order: `debit>credit` for a posting made by `moving`. With the
    /// transfer it belongs to, this names the step of the transfer it records, which happens at most once.
    pub fn accounts(&self) -> String {
        self.0.iter().map(|(a, _)| a.as_str()).collect::<Vec<_>>().join(">")
    }

    /// What this posting moves into `account` (negative: out of it).
    pub fn cents(&self, account: &str) -> i64 {
        self.0.iter().filter(|(a, _)| a == account).map(|(_, c)| *c).sum()
    }

    pub fn to_json(&self) -> Value {
        Value::Array(self.0.iter().map(|(account, cents)| json!([account, cents])).collect())
    }

    pub fn from_json(value: &Value) -> Result<Self, String> {
        let entries = value.as_array().ok_or("a posting is a list of entries")?;
        entries
            .iter()
            .map(|entry| match (entry[0].as_str(), entry[1].as_i64()) {
                (Some(account), Some(cents)) => Ok((account.to_string(), cents)),
                _ => Err(format!("a posting entry is [account, cents], not {entry}")),
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }
}

#[derive(Default)]
pub struct Ledger {
    balances: BTreeMap<String, i64>,
    postings: u64,
}

impl Ledger {
    /// A ledger whose postings the records have already summed.
    pub const fn restore(balances: BTreeMap<String, i64>, postings: u64) -> Self {
        Self { balances, postings }
    }

    pub const fn postings(&self) -> u64 {
        self.postings
    }

    /// Applies a posting whole, or not at all if it does not balance or a balance would overflow.
    pub fn post(&mut self, posting: &Posting) -> Result<(), String> {
        if posting.0.len() < 2 {
            return Err("a posting needs a debit and a credit".into());
        }
        let sum = posting.0.iter().try_fold(0i64, |sum, (_, cents)| sum.checked_add(*cents));
        if sum != Some(0) {
            return Err(format!("a posting that does not balance: {}", posting.to_json()));
        }
        let mut next = Vec::with_capacity(posting.0.len());
        for (account, cents) in &posting.0 {
            let balance = self.balance(account).checked_add(*cents).ok_or("a ledger balance overflowed")?;
            next.push((account.clone(), balance));
        }
        self.balances.extend(next);
        self.postings += 1;
        Ok(())
    }

    pub const fn balances(&self) -> &BTreeMap<String, i64> {
        &self.balances
    }

    pub fn balance(&self, account: &str) -> i64 {
        self.balances.get(account).copied().unwrap_or(0)
    }

    /// Every balance, and the sum of them all, which is zero unless something is badly wrong.
    pub fn trial_balance(&self) -> Value {
        let total: i128 = self.balances.values().map(|b| i128::from(*b)).sum();
        json!({"balances": self.balances, "postings": self.postings, "total": total.to_string()})
    }

    pub fn total(&self) -> i128 {
        self.balances.values().map(|b| i128::from(*b)).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_posting_moves_money_and_the_ledger_still_sums_to_zero() {
        let mut ledger = Ledger::default();
        ledger.post(&Posting::moving(500, FBO, &customer(1)).expect("a posting")).expect("posted");
        ledger.post(&Posting::moving(200, &customer(1), WITHDRAWALS_IN_TRANSIT).expect("a posting")).expect("posted");
        assert_eq!((ledger.balance(FBO), ledger.balance(&customer(1))), (500, -300));
        assert_eq!(ledger.balance(WITHDRAWALS_IN_TRANSIT), -200);
        assert_eq!(ledger.total(), 0);
    }

    #[test]
    fn a_posting_that_does_not_balance_changes_nothing() {
        let mut ledger = Ledger::default();
        let lopsided = Posting::from_json(&json!([[FBO, 100], [customer(1), -99]])).expect("well formed");
        assert!(ledger.post(&lopsided).is_err());
        let alone = Posting::from_json(&json!([[FBO, 0]])).expect("well formed");
        assert!(ledger.post(&alone).is_err());
        assert_eq!(ledger.balance(FBO), 0);
        assert_eq!(ledger.trial_balance()["postings"], 0);
    }

    #[test]
    fn a_posting_survives_its_written_form() {
        let posting = Posting::moving(12_345, FBO, &customer(7)).expect("a posting");
        assert_eq!(Posting::from_json(&posting.to_json()), Ok(posting));
    }
}
