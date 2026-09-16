//! E-10.9 without writing the book: how much of an order the walk would fill, found by walking a view of the
//! state rather than the state itself.
//!
//! The dry run was the real walk with every write logged and rolled back. That is exact by construction, but it
//! logs twenty-odd writes for every fill into an undo log of fixed size, so one fill-or-kill order against a few
//! thousand resting orders ran out of log and fail-stopped the engine — and a fail-stop replays, so the group
//! stayed down.
//!
//! What the answer depends on is small. The incoming order's remaining quantity and spend cap; which resting
//! orders are still there and how much of each remains; and each account's position and locks in the one
//! instrument, because a fill can make lock restoration (E-11.4) shrink or remove a reduce-only order further
//! along the walk (scenario 18). Cash, reservations, collateral and counters cannot change it, and protection
//! triggers act only at the end of the unit (E-13.4). So the check keeps exactly that view, in two tables indexed
//! by slot and stamped per check — clearing them costs nothing and they hold every order and account there can be —
//! and follows E-10.3 to E-10.8 step for step over it.
//!
//! Two walks must now agree. With the `dry-run-oracle` feature the engine runs the logged dry run as well and
//! fail-stops if the answers differ, which is how the scenarios and differential vectors test this module.

use crate::book::{crosses, floor_to, opposite};
use crate::emit::Side;
use crate::orders::EXTRA_LOT;
use crate::state::*;
use crate::{Engine, FailStop};
use venue_journal::allocation_policy::AllocationPolicy;
use venue_journal::self_match_prevention::SelfMatchPrevention;

/// The incoming order as the check walks it.
struct Taker {
    account_slot: u32,
    account_id: u64,
    inst_slot: u32,
    side: Side,
    limit: u16,
    remaining: u32,
    lock: u32,
    cap: Option<i64>,
}

impl Engine<'_> {
    /// E-10.9: the quantity the walk would fill for the order in `slot`, as it would be accepted with its lock
    /// taken. Writes nothing but the check's own views.
    pub(crate) fn fill_check(&mut self, slot: u32) -> Result<u32, FailStop> {
        self.check_stamp = match self.check_stamp.checked_add(1) {
            Some(stamp) => stamp,
            None => {
                // Once in four billion checks: forget every view, so no old stamp can match the new ones.
                self.check_orders.fill(CheckOrder::default());
                self.check_holdings.fill(CheckHolding::default());
                1
            }
        };
        let o = self.orders[slot as usize];
        let mut t = Taker {
            account_slot: o.account_slot,
            account_id: o.account_id,
            inst_slot: o.instrument_slot,
            side: o.side,
            limit: o.tick,
            remaining: o.remaining,
            lock: o.lock,
            cap: o.spend_cap_remaining,
        };
        self.add_check_locked(t.account_slot, t.inst_slot, t.side, i64::from(o.lock))?;
        self.check_walk(&mut t)?;
        Ok(o.remaining - t.remaining)
    }

    // ------------------------------------------------------------------ the views

    fn check_order(&self, slot: u32) -> CheckOrder {
        let view = self.check_orders[slot as usize];
        if view.stamp == self.check_stamp {
            return view;
        }
        let o = &self.orders[slot as usize];
        CheckOrder { stamp: self.check_stamp, remaining: o.remaining, lock: o.lock, resting: o.in_use }
    }

    fn set_check_order(&mut self, slot: u32, view: CheckOrder) {
        self.check_orders[slot as usize] = CheckOrder { stamp: self.check_stamp, ..view };
    }

    /// Still on the book in the check's view: what the real walk would find linked into a level.
    fn check_live(&self, slot: u32) -> bool {
        let view = self.check_order(slot);
        view.resting && view.remaining > 0
    }

    fn check_holding(&self, account_slot: u32, inst_slot: u32) -> Holding {
        let view = self.check_holdings[account_slot as usize];
        if view.stamp == self.check_stamp {
            return view.holding;
        }
        self.holding(self.accounts[account_slot as usize].id, self.instruments[inst_slot as usize].id)
    }

    fn check_holding_w(&mut self, account_slot: u32, inst_slot: u32) -> &mut Holding {
        let holding = self.check_holding(account_slot, inst_slot);
        let stamp = self.check_stamp;
        let view = &mut self.check_holdings[account_slot as usize];
        if view.stamp != stamp {
            *view = CheckHolding { stamp, holding };
        }
        &mut view.holding
    }

    /// As `add_locked`: the locked holding on the side an order of `side` reduces.
    fn add_check_locked(&mut self, account_slot: u32, inst_slot: u32, side: Side, delta: i64) -> Result<(), FailStop> {
        let h = self.check_holding_w(account_slot, inst_slot);
        let locked = if side == Side::SELL { &mut h.yes_locked } else { &mut h.no_locked };
        *locked = locked.checked_add(delta).ok_or(FailStop::Overflow)?;
        Ok(())
    }

    /// As `remove`, for what it changes that the walk can see: the order leaves, releasing its lock (E-11.5).
    fn check_remove(&mut self, slot: u32) -> Result<(), FailStop> {
        let o = self.orders[slot as usize];
        let mut view = self.check_order(slot);
        if view.lock != 0 {
            self.add_check_locked(o.account_slot, o.instrument_slot, o.side, -i64::from(view.lock))?;
        }
        view.lock = 0;
        view.resting = false;
        self.set_check_order(slot, view);
        Ok(())
    }

    // ------------------------------------------------------------------ the walk (E-10.3 to E-10.8)

    fn check_walk(&mut self, t: &mut Taker) -> Result<(), FailStop> {
        let inst = self.instruments[t.inst_slot as usize];
        let lot = u64::from(inst.lot_step);
        // Nothing joins the book during a walk, so a tick passed over never becomes the best again.
        let mut from = None;
        loop {
            if t.remaining == 0 {
                return Ok(());
            }
            let Some(tick) = self.check_next_crossing(t, from) else { return Ok(()) };
            from = Some(tick);
            let mut q = u64::from(t.remaining);
            if let Some(cap) = t.cap {
                q = q.min(self.affordable(t.inst_slot, t.side, tick, cap)?);
            }
            let q = floor_to(q, lot) as u32;
            if q == 0 {
                return Ok(());
            }
            let stopped = if inst.policy == AllocationPolicy::PRO_RATA {
                self.check_pro_rata(t, tick, q)?
            } else {
                self.check_price_time(t, tick, q)?
            };
            if stopped {
                return Ok(());
            }
        }
    }

    /// The best opposite tick, from `from` onward, that crosses and still holds an order in the check's view.
    fn check_next_crossing(&self, t: &Taker, from: Option<u16>) -> Option<u16> {
        let maker_side = opposite(t.side);
        let len = self.instruments[t.inst_slot as usize].table.len;
        let mut tick = match from {
            Some(tick) => tick,
            None => self.best(t.inst_slot, maker_side)?,
        };
        loop {
            if !crosses(t.side, tick, t.limit) {
                return None;
            }
            let mut cur = self.level(t.inst_slot, maker_side, tick).head;
            while cur != NONE {
                if self.check_live(cur) {
                    return Some(tick);
                }
                cur = self.orders[cur as usize].next;
            }
            // Asks are walked upward from the lowest, bids downward from the highest.
            tick = if maker_side == Side::SELL {
                tick.checked_add(1).filter(|&next| next < len)?
            } else {
                tick.checked_sub(1)?
            };
        }
    }

    /// As `price_time` (E-10.4).
    fn check_price_time(&mut self, t: &mut Taker, tick: u16, q: u32) -> Result<bool, FailStop> {
        let mode = self.accounts[t.account_slot as usize].self_match_prevention;
        let mut left = q;
        let mut cur = self.level(t.inst_slot, opposite(t.side), tick).head;
        while cur != NONE && left > 0 {
            let r = self.orders[cur as usize];
            let next = r.next;
            if !self.check_live(cur) {
                cur = next;
                continue;
            }
            if r.account_id == t.account_id {
                // E-13.1
                match mode {
                    SelfMatchPrevention::CANCEL_RESTING => {
                        self.check_remove(cur)?;
                        cur = next;
                        continue;
                    }
                    SelfMatchPrevention::CANCEL_BOTH => {
                        self.check_remove(cur)?;
                        return Ok(true);
                    }
                    _ => return Ok(true),
                }
            }
            let f = left.min(self.check_order(cur).remaining);
            self.check_fill(t, cur, tick, f)?;
            left -= f;
            cur = next;
        }
        Ok(false)
    }

    /// As `pro_rata` (E-10.5).
    fn check_pro_rata(&mut self, t: &mut Taker, tick: u16, q: u32) -> Result<bool, FailStop> {
        let maker_side = opposite(t.side);
        let lot = self.instruments[t.inst_slot as usize].lot_step;
        let mode = self.accounts[t.account_slot as usize].self_match_prevention;
        let head = self.level(t.inst_slot, maker_side, tick).head;
        // E-13.1: the account's own orders at the tick, together, in ascending order ID.
        let mut own = 0;
        let mut cur = head;
        while cur != NONE {
            let r = self.orders[cur as usize];
            if self.check_live(cur) && r.account_id == t.account_id {
                self.scratch[own] = (r.id, u64::from(cur));
                own += 1;
            }
            cur = r.next;
        }
        if own > 0 {
            if mode == SelfMatchPrevention::CANCEL_INCOMING {
                return Ok(true);
            }
            self.scratch[..own].sort_unstable();
            for i in 0..own {
                let s = self.scratch[i].1 as u32;
                self.check_remove(s)?;
            }
            if mode == SelfMatchPrevention::CANCEL_BOTH {
                return Ok(true);
            }
        }
        // The other orders, oldest first, as (lots, slot).
        let (mut n, mut total) = (0, 0u64);
        cur = head;
        while cur != NONE {
            if self.check_live(cur) {
                let lots = u64::from(self.check_order(cur).remaining / lot);
                self.scratch[n] = (lots, u64::from(cur));
                total += lots;
                n += 1;
            }
            cur = self.orders[cur as usize].next;
        }
        let want = u64::from(q / lot);
        let share = |m: u64| (u128::from(want) * u128::from(m) / u128::from(total.max(1))) as u64;
        if total > want {
            let spare = want - self.scratch[..n].iter().map(|&(m, _)| share(m)).sum::<u64>();
            // The leftover lots go one each to the largest (Q · m) mod M, ties to the lower order ID.
            for _ in 0..spare {
                let mut best: Option<(u64, u64, usize)> = None;
                for (i, &(m, s)) in self.scratch[..n].iter().enumerate() {
                    if s & EXTRA_LOT != 0 {
                        continue;
                    }
                    let rem = (u128::from(want) * u128::from(m) % u128::from(total)) as u64;
                    let id = self.orders[s as usize].id;
                    if best.is_none_or(|(br, bid, _)| rem > br || (rem == br && id < bid)) {
                        best = Some((rem, id, i));
                    }
                }
                if let Some((_, _, i)) = best {
                    self.scratch[i].1 |= EXTRA_LOT;
                }
            }
        }
        for i in 0..n {
            let (m, s) = self.scratch[i];
            let maker = (s & !EXTRA_LOT) as u32;
            let lots = if total <= want { m } else { share(m) + u64::from(s & EXTRA_LOT != 0) };
            // Lock restoration may have shrunk or removed an order during the allocation.
            let view = self.check_order(maker);
            if lots > 0 && view.resting && view.remaining > 0 {
                let f = (lots * u64::from(lot)).min(u64::from(view.remaining)) as u32;
                self.check_fill(t, maker, tick, f)?;
            }
        }
        Ok(false)
    }

    /// As `fill` (E-10.6, E-10.7), for what it changes that the walk can see: positions, locks, what remains of
    /// both orders and the taker's spend cap, lock restoration, and a finished maker leaving the book.
    fn check_fill(&mut self, t: &mut Taker, maker_slot: u32, tick: u16, quantity: u32) -> Result<(), FailStop> {
        let maker = self.orders[maker_slot as usize];
        let inst_slot = t.inst_slot;
        let (buyer_slot, seller_slot) = if t.side == Side::BUY {
            (t.account_slot, maker.account_slot)
        } else {
            (maker.account_slot, t.account_slot)
        };
        let q = i64::from(quantity);
        let pb = self.check_holding(buyer_slot, inst_slot).position;
        let ps = self.check_holding(seller_slot, inst_slot).position;
        self.check_holding_w(buyer_slot, inst_slot).position = pb.checked_add(q).ok_or(FailStop::Overflow)?;
        self.check_holding_w(seller_slot, inst_slot).position = ps.checked_sub(q).ok_or(FailStop::Overflow)?;
        // E-11.3, taker then maker.
        let used = quantity.min(t.lock);
        if used > 0 {
            self.add_check_locked(t.account_slot, inst_slot, t.side, -i64::from(used))?;
        }
        if let Some(cap) = t.cap {
            t.cap = Some(cap - self.unit_cost(inst_slot, t.side, tick, u64::from(quantity))?);
        }
        t.remaining -= quantity;
        t.lock -= used;
        let mut m = self.check_order(maker_slot);
        let used = quantity.min(m.lock);
        if used > 0 {
            self.add_check_locked(maker.account_slot, inst_slot, maker.side, -i64::from(used))?;
        }
        m.remaining -= quantity;
        m.lock -= used;
        self.set_check_order(maker_slot, m);
        // E-11.4, for both accounts in ascending order.
        let (buyer_id, seller_id) = (self.accounts[buyer_slot as usize].id, self.accounts[seller_slot as usize].id);
        let (first, second) = if buyer_id <= seller_id { (buyer_slot, seller_slot) } else { (seller_slot, buyer_slot) };
        self.check_restore(first, inst_slot)?;
        if buyer_id != seller_id {
            self.check_restore(second, inst_slot)?;
        }
        // A resting order with nothing remaining leaves the book. Lock restoration may already have removed it.
        let m = self.check_order(maker_slot);
        if m.resting && m.remaining == 0 {
            self.check_remove(maker_slot)?;
        }
        Ok(())
    }

    /// As `restore_locks` (E-11.4) for one account on one instrument.
    fn check_restore(&mut self, account_slot: u32, inst_slot: u32) -> Result<(), FailStop> {
        for side in [Side::SELL, Side::BUY] {
            let h = self.check_holding(account_slot, inst_slot);
            let (held, locked) = if side == Side::SELL {
                (h.position.max(0), h.yes_locked)
            } else {
                ((-h.position).max(0), h.no_locked)
            };
            let mut excess = locked - held;
            // Victims most recently accepted first: each round takes the largest (acceptedAt, orderId) below the last.
            let mut below: Option<(u64, u64)> = None;
            while excess > 0 {
                let mut victim: Option<((u64, u64), u32)> = None;
                let mut cur = self.accounts[account_slot as usize].first_order;
                while cur != NONE {
                    let o = &self.orders[cur as usize];
                    let view = self.check_order(cur);
                    let key = (o.accepted_at, o.id);
                    if view.resting
                        && o.instrument_slot == inst_slot
                        && o.side == side
                        && view.lock > 0
                        && below.is_none_or(|b| key < b)
                        && victim.is_none_or(|(v, _)| key > v)
                    {
                        victim = Some((key, cur));
                    }
                    cur = o.account_next;
                }
                let Some((key, v)) = victim else { break };
                below = Some(key);
                let reduce_only = self.orders[v as usize].reduce_only;
                let mut view = self.check_order(v);
                let take = i64::from(view.lock).min(excess) as u32;
                self.add_check_locked(account_slot, inst_slot, side, -i64::from(take))?;
                view.lock -= take;
                excess -= i64::from(take);
                if reduce_only {
                    view.remaining -= take;
                }
                self.set_check_order(v, view);
                if reduce_only && view.remaining == 0 {
                    self.check_remove(v)?;
                }
            }
            if excess > 0 {
                return Err(FailStop::Inconsistent);
            }
        }
        Ok(())
    }
}
