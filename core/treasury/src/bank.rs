//! The fake bank: the dummy provider of blueprint §16, which moves no money and decides every transfer the way a bank
//! might — after a while, and now and then not in the customer's favour.
//!
//! Its decisions are a function of its seed and the transfer alone, never of the clock or of the order transfers
//! arrive in, so a run can be repeated exactly and a failure found in one can be found again.

use crate::transfer::{Kind, Method};

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
const fn mix(mut z: u64) -> u64 {
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
