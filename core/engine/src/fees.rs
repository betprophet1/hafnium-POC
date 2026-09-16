//! Fees: the default schedule, its overrides, and the charge on each fill (spec E-7.9 to E-7.11, E-10.11,
//! E-10.12).
//!
//! Overrides live in one pool kept sorted by `(accountId, priority, overrideId)`. Overrides for any account
//! (`accountId` 0) therefore form a prefix, and an account's own overrides a contiguous run found by binary search,
//! each already in the order E-10.12 ranks them. Resolving a party's rate reads the first active match of the two
//! runs and keeps the better, so a fill costs a search and a short scan however many accounts have overrides of
//! their own. Commands that change the pool shift it, which is fine for inputs an operator sends.

use crate::state::{FeeOverride, FeeRates};
use crate::{Engine, FailStop};

/// The largest rate magnitude, 100% in millionths (E-7.9, D-2).
pub const RATE_LIMIT: i32 = 1_000_000;

/// The exchange's account, which receives fees and pays rebates (E-3.2, D-21).
pub const FEE_ACCOUNT: u64 = 0;

/// Whether a maker and a taker rate lie in the ranges of E-7.9.
pub const fn rates_valid(maker: i32, taker: i32) -> bool {
    -RATE_LIMIT <= maker && maker <= RATE_LIMIT && 0 <= taker && taker <= RATE_LIMIT
}

/// `⌈rate · yes · no / (10⁸ · q)⌉` cents for `q` contracts whose YES cost is `yes` (E-10.11).
///
/// Rounded toward positive infinity whatever the sign, so a charge is never below the formula and a rebate never
/// above it. With rates within ±10⁶ and `yes`, `no` at most `100 · q` for a `u32` quantity, the numerator stays
/// below 2⁹⁸; checked arithmetic still reports anything else as an overflow rather than a wrong fee.
pub fn fee_cents(rate: i32, yes: i64, quantity: u32) -> Result<i64, FailStop> {
    if quantity == 0 {
        return Err(FailStop::Inconsistent);
    }
    let q = i128::from(quantity);
    let yes = i128::from(yes);
    let no = q.checked_mul(100).and_then(|c| c.checked_sub(yes)).ok_or(FailStop::Overflow)?;
    let numerator = i128::from(rate).checked_mul(yes).and_then(|n| n.checked_mul(no)).ok_or(FailStop::Overflow)?;
    let denominator = q.checked_mul(100_000_000).ok_or(FailStop::Overflow)?;
    // For a positive denominator, floor division is `div_euclid`, and the ceiling is the negated floor of the
    // negation.
    let ceiling = -(numerator.checked_neg().ok_or(FailStop::Overflow)?.div_euclid(denominator));
    i64::try_from(ceiling).map_err(|_| FailStop::Overflow)
}

impl FeeOverride {
    /// E-10.12: in force while it has no expiry or `now` is before it.
    pub const fn active(&self, now: u64) -> bool {
        match self.expires_at {
            Some(at) => now < at,
            None => true,
        }
    }

    const fn key(&self) -> (u64, u32, u64) {
        (self.account_id, self.priority, self.id)
    }
}

impl<'a> Engine<'a> {
    /// The rates a party pays on a fill in `instrument_id` of `event_id` (E-10.12): the winning override's, or the
    /// default schedule's.
    pub(crate) fn fee_rates(&self, account_id: u64, instrument_id: u64, event_id: u64) -> FeeRates {
        let now = self.now;
        let pool = &self.fee_overrides[..self.fee_overrides_len];
        // The first active override in an account's run that matches the fill is the best of that run, because the
        // run is in (priority, overrideId) order.
        let best_for = |account: u64| {
            let start = pool.partition_point(|o| o.account_id < account);
            pool[start..].iter().take_while(|o| o.account_id == account).find(|o| {
                o.active(now)
                    && (o.instrument_id == 0 || o.instrument_id == instrument_id)
                    && (o.event_id == 0 || o.event_id == event_id)
            })
        };
        let any = best_for(FEE_ACCOUNT);
        let own = if account_id == FEE_ACCOUNT { None } else { best_for(account_id) };
        let winner = match (any, own) {
            (Some(a), Some(b)) => Some(if (a.priority, a.id) <= (b.priority, b.id) { a } else { b }),
            (a, b) => a.or(b),
        };
        winner.map_or(self.fee_schedule, |o| FeeRates {
            maker_micros: o.maker_rate_micros,
            taker_micros: o.taker_rate_micros,
        })
    }

    /// E-10.11: charges one party's fee and credits it to account 0, through both accounts' cash in the event so a
    /// void returns it (D-28). A negative fee is a rebate paid the other way. Charges, not rebates, are counted
    /// for the unit's bound on available cash (E-3.3).
    pub(crate) fn charge_fee(&mut self, account_slot: u32, event_id: u64, cents: i64) -> Result<(), FailStop> {
        if cents == 0 {
            return Ok(());
        }
        self.add_event_cash(account_slot, event_id, cents.checked_neg().ok_or(FailStop::Overflow)?)?;
        let exchange = self.account_slot(FEE_ACCOUNT)?;
        self.add_event_cash(exchange, event_id, cents)?;
        if cents > 0 {
            let unit = self.unit_serial;
            let a = self.account_w(account_slot)?;
            if a.fees_unit != unit {
                a.fees_unit = unit;
                a.fees_charged = 0;
            }
            a.fees_charged = a.fees_charged.checked_add(cents).ok_or(FailStop::Overflow)?;
        }
        Ok(())
    }

    /// The pool index of the override with this ID, expired or not.
    fn fee_override_index(&self, id: u64) -> Option<usize> {
        self.fee_overrides[..self.fee_overrides_len].iter().position(|o| o.id == id)
    }

    /// E-7.11: whether an active override has this ID.
    pub(crate) fn fee_override_exists(&self, id: u64) -> bool {
        self.fee_override_index(id).is_some_and(|i| self.fee_overrides[i].active(self.now))
    }

    /// E-7.11: removes the override with this ID.
    pub(crate) fn remove_fee_override(&mut self, id: u64) {
        if let Some(i) = self.fee_override_index(id) {
            self.fee_overrides.copy_within(i + 1..self.fee_overrides_len, i);
            self.fee_overrides_len -= 1;
        }
    }

    /// E-7.10: stores an override, replacing any with its ID. A full pool first drops the overrides that have
    /// expired, which no rule can observe (E-8.2); a pool still full is a capacity fail-stop.
    pub(crate) fn set_fee_override(&mut self, entry: FeeOverride) -> Result<(), FailStop> {
        self.remove_fee_override(entry.id);
        if self.fee_overrides_len == self.fee_overrides.len() {
            let now = self.now;
            let mut kept = 0;
            for i in 0..self.fee_overrides_len {
                if self.fee_overrides[i].active(now) {
                    self.fee_overrides[kept] = self.fee_overrides[i];
                    kept += 1;
                }
            }
            self.fee_overrides_len = kept;
            if kept == self.fee_overrides.len() {
                return Err(FailStop::Capacity);
            }
        }
        let len = self.fee_overrides_len;
        let at = self.fee_overrides[..len].partition_point(|o| o.key() < entry.key());
        self.fee_overrides.copy_within(at..len, at + 1);
        self.fee_overrides[at] = entry;
        self.fee_overrides_len += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `yes` for `q` contracts at a probability of `cents / 100`.
    fn fee(rate: i32, cents: i64, q: u32) -> i64 {
        fee_cents(rate, cents * i64::from(q), q).unwrap()
    }

    #[test]
    fn a_charge_rounds_up_and_an_exact_one_does_not() {
        // 7% of 10 at 50c: 17.5c. Of 100 at 50c: 175c exactly.
        assert_eq!(fee(70_000, 50, 10), 18);
        assert_eq!(fee(70_000, 50, 100), 175);
        // 7% of 1 at 1c: 0.0693c is still a cent.
        assert_eq!(fee(70_000, 1, 1), 1);
        assert_eq!(fee(70_000, 99, 1), 1);
        // 7% of 10 at 30c: 14.7c.
        assert_eq!(fee(70_000, 30, 10), 15);
        assert_eq!(fee(0, 30, 10), 0);
    }

    #[test]
    fn a_rebate_rounds_toward_zero_so_it_never_exceeds_the_formula() {
        // -1.75% of 10 at 30c: -3.675c pays 3c. Of 1 at 1c: -0.0173c pays nothing.
        assert_eq!(fee(-17_500, 30, 10), -3);
        assert_eq!(fee(-17_500, 1, 1), 0);
        // -100% of 2 at 50c: exactly -50c.
        assert_eq!(fee(-1_000_000, 50, 2), -50);
        assert_eq!(fee(-1, 50, 1), 0);
    }

    #[test]
    fn the_largest_fill_does_not_overflow() {
        let q = u32::MAX;
        let yes = 50 * i64::from(q);
        assert_eq!(fee_cents(RATE_LIMIT, yes, q), Ok(25 * i64::from(q)));
        assert_eq!(fee_cents(-RATE_LIMIT, yes, q), Ok(-25 * i64::from(q)));
    }

    #[test]
    fn rate_ranges() {
        assert!(rates_valid(-RATE_LIMIT, RATE_LIMIT));
        assert!(!rates_valid(-RATE_LIMIT - 1, 0));
        assert!(!rates_valid(0, -1));
        assert!(!rates_valid(RATE_LIMIT + 1, 0));
    }
}
