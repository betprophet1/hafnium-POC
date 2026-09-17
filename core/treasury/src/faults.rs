//! Messages lost on purpose, so that reconciliation has something to find (blueprint §11, §16: the fake provider
//! produces every outcome in the contract on demand, late returns included).
//!
//! Two paths lose messages. Between the bank and the treasury, a notice of the bank's decision can be lost outright,
//! arrive long after the bank decided, or arrive twice. Between the treasury and the sequencer group, a sleeve
//! adjustment can be applied with its answer lost, or lost before it arrives; either way the treasury is left waiting
//! for an outcome that never comes. Each loss is drawn from the seed and the transfer or command alone, like the
//! bank's own decisions, so a run with losses can be repeated exactly.
//!
//! All of it is off unless asked for: the `demo` profile turns every kind on at a few percent.

use crate::bank::mix;
use crate::treasury::{Adjusted, Journal, Sequencer};
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// How long after the first copy a duplicated notice arrives again.
pub const DUPLICATE_GAP_MS: u64 = 2_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Faults {
    /// Thousandths of the bank's notices that never reach the treasury.
    pub lost_notice_per_mille: u64,
    /// Thousandths that arrive `late_notice_ms` after the bank decided.
    pub late_notice_per_mille: u64,
    pub late_notice_ms: u64,
    /// Thousandths that arrive twice.
    pub duplicate_notice_per_mille: u64,
    /// Thousandths of sleeve adjustments that the sequencer group applies and whose answer is lost.
    pub lost_answer_per_mille: u64,
    /// Thousandths of sleeve adjustments lost before they reach the sequencer group.
    pub lost_command_per_mille: u64,
}

/// What becomes of the bank's notice of one transfer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    OnTime,
    Lost,
    Late,
    Twice,
}

/// What becomes of one sleeve adjustment the first time it is sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Loss {
    None,
    Answer,
    Command,
}

/// A draw in thousandths for `key`, independent of the bank's decisions (which salt with 1 and 2).
fn per_mille(seed: u64, key: u64, salt: u64) -> u64 {
    mix(mix(seed ^ mix(key)) ^ salt) % 1000
}

impl Faults {
    pub const NONE: Self = Self {
        lost_notice_per_mille: 0,
        late_notice_per_mille: 0,
        late_notice_ms: 0,
        duplicate_notice_per_mille: 0,
        lost_answer_per_mille: 0,
        lost_command_per_mille: 0,
    };

    /// Every kind of loss at a few percent, with a late notice a minute and a half behind: often enough to see in a
    /// demo that makes a few dozen transfers, and late enough that a scheduled reconciliation finds it first.
    pub const DEMO: Self = Self {
        lost_notice_per_mille: 40,
        late_notice_per_mille: 40,
        late_notice_ms: 90_000,
        duplicate_notice_per_mille: 40,
        lost_answer_per_mille: 40,
        lost_command_per_mille: 40,
    };

    pub fn profile(name: &str) -> Option<Self> {
        match name {
            "none" => Some(Self::NONE),
            "demo" => Some(Self::DEMO),
            _ => None,
        }
    }

    pub const fn any(&self) -> bool {
        self.lost_notice_per_mille > 0
            || self.late_notice_per_mille > 0
            || self.duplicate_notice_per_mille > 0
            || self.lost_answer_per_mille > 0
            || self.lost_command_per_mille > 0
    }

    /// One draw decides between the notice kinds, so their shares add up rather than overlap.
    pub fn notice(&self, seed: u64, transfer: u64) -> Delivery {
        let d = per_mille(seed, transfer, 3);
        let lost = self.lost_notice_per_mille;
        let late = lost + self.late_notice_per_mille;
        let twice = late + self.duplicate_notice_per_mille;
        if d < lost {
            Delivery::Lost
        } else if d < late {
            Delivery::Late
        } else if d < twice {
            Delivery::Twice
        } else {
            Delivery::OnTime
        }
    }

    /// When each copy of the notice of a decision made at `decided` arrives.
    pub fn arrivals(&self, seed: u64, transfer: u64, decided: u64) -> Vec<u64> {
        match self.notice(seed, transfer) {
            Delivery::OnTime => vec![decided],
            Delivery::Lost => vec![],
            Delivery::Late => vec![decided.saturating_add(self.late_notice_ms)],
            Delivery::Twice => vec![decided, decided.saturating_add(DUPLICATE_GAP_MS)],
        }
    }

    pub fn command(&self, seed: u64, command_id: &str) -> Loss {
        let key = command_id.bytes().fold(0u64, |h, b| mix(h ^ u64::from(b)));
        let d = per_mille(seed, key, 4);
        if d < self.lost_answer_per_mille {
            Loss::Answer
        } else if d < self.lost_answer_per_mille + self.lost_command_per_mille {
            Loss::Command
        } else {
            Loss::None
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "lostNoticePerMille": self.lost_notice_per_mille, "lateNoticePerMille": self.late_notice_per_mille,
            "lateNoticeMs": self.late_notice_ms, "duplicateNoticePerMille": self.duplicate_notice_per_mille,
            "lostAnswerPerMille": self.lost_answer_per_mille, "lostCommandPerMille": self.lost_command_per_mille
        })
    }
}

/// A sequencer group reached through a path that loses messages. Only the first send of a command can be lost: the
/// send that reconciliation makes again under the same command ID goes through, as a retry on a healthy path would.
pub struct Lossy<S> {
    inner: S,
    faults: Faults,
    seed: u64,
    spent: BTreeSet<String>,
}

impl<S> Lossy<S> {
    pub const fn new(inner: S, faults: Faults, seed: u64) -> Self {
        Self { inner, faults, seed, spent: BTreeSet::new() }
    }

    pub const fn inner(&self) -> &S {
        &self.inner
    }
}

impl<S: Sequencer> Sequencer for Lossy<S> {
    fn adjust(&mut self, command_id: &str, account: u64, cents: u64, reason: &str) -> Result<Adjusted, String> {
        if self.spent.contains(command_id) {
            return self.inner.adjust(command_id, account, cents, reason);
        }
        match self.faults.command(self.seed, command_id) {
            Loss::None => self.inner.adjust(command_id, account, cents, reason),
            Loss::Command => {
                self.spent.insert(command_id.to_string());
                eprintln!("treasury: fault injected: {reason} {command_id} lost before the sequencer group");
                Ok(Adjusted::NoAnswer)
            }
            Loss::Answer => {
                // A send that fails outright is an error the treasury sees and retries, not a loss.
                let answer = self.inner.adjust(command_id, account, cents, reason)?;
                self.spent.insert(command_id.to_string());
                eprintln!("treasury: fault injected: the answer to {reason} {command_id} ({answer:?}) lost");
                Ok(Adjusted::NoAnswer)
            }
        }
    }

    fn journal(&self) -> Journal {
        self.inner.journal()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_lost_unless_asked_for() {
        for t in 1..2000 {
            assert_eq!(Faults::NONE.notice(9, t), Delivery::OnTime);
            assert_eq!(Faults::NONE.command(9, &format!("T00000000{t:06x}g")), Loss::None);
        }
    }

    #[test]
    fn the_demo_profile_loses_each_kind_now_and_then_and_the_same_way_every_time() {
        let notices: Vec<Delivery> = (1..=4000).map(|t| Faults::DEMO.notice(9, t)).collect();
        for kind in [Delivery::Lost, Delivery::Late, Delivery::Twice] {
            let n = notices.iter().filter(|d| **d == kind).count();
            // 4% of 4000, give or take what a fair draw varies by.
            assert!((100..=220).contains(&n), "{kind:?}: {n} of 4000");
        }
        let again: Vec<Delivery> = (1..=4000).map(|t| Faults::DEMO.notice(9, t)).collect();
        assert_eq!(notices, again);
        let commands: Vec<Loss> = (1..=4000).map(|t| Faults::DEMO.command(9, &format!("T0000abcd{t:06x}g"))).collect();
        for kind in [Loss::Answer, Loss::Command] {
            let n = commands.iter().filter(|d| **d == kind).count();
            assert!((100..=220).contains(&n), "{kind:?}: {n} of 4000");
        }
    }

    #[test]
    fn a_late_notice_comes_late_and_a_duplicate_comes_twice() {
        let faults = Faults { late_notice_per_mille: 500, duplicate_notice_per_mille: 500, ..Faults::DEMO };
        let faults = Faults { lost_notice_per_mille: 0, ..faults };
        for t in 1..100 {
            let arrivals = faults.arrivals(1, t, 10_000);
            match faults.notice(1, t) {
                Delivery::Late => assert_eq!(arrivals, vec![100_000]),
                Delivery::Twice => assert_eq!(arrivals, vec![10_000, 12_000]),
                other => panic!("{other:?} drawn with no share"),
            }
        }
    }
}
