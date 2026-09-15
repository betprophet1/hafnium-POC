//! Books, the order slab, holdings, locks and reservations (spec §2, §10, §11; blueprint §6).
//!
//! Every write a walk can reach goes through the accessors here. During a dry run (E-10.9) they log the value
//! they overwrite and [`Engine::rollback`] restores it; outside a dry run they only write. Writes a unit
//! reports (positions, sleeves, book levels) also record the old value through the `touch_*` methods, so
//! blocks 3 to 5 of E-5.2 come out right whatever path changed them.

use crate::emit::Side;
use crate::fixed::{FixedMap, MapKey};
use crate::state::*;
use crate::{Engine, FailStop};

#[inline]
pub(crate) const fn opposite(side: Side) -> Side {
    match side {
        Side::BUY => Side::SELL,
        _ => Side::BUY,
    }
}

/// The largest multiple of `step` not above `value`.
#[inline]
pub(crate) const fn floor_to(value: u64, step: u64) -> u64 {
    match value.checked_div(step) {
        Some(lots) => lots * step,
        None => 0,
    }
}

/// Whether an order of `side` with limit `limit` crosses an opposite order at `tick` (E-10.1).
#[inline]
pub(crate) fn crosses(side: Side, tick: u16, limit: u16) -> bool {
    if side == Side::BUY {
        tick <= limit
    } else {
        tick >= limit
    }
}

#[inline]
fn capacity<T>(r: Result<T, crate::fixed::Full>) -> Result<T, FailStop> {
    r.map_err(|_| FailStop::Capacity)
}

fn restore<K: MapKey + Copy + Eq, V: Copy>(
    map: &mut FixedMap<'_, K, V>,
    key: K,
    old: Option<V>,
) -> Result<(), FailStop> {
    match old {
        Some(v) => capacity(map.insert(key, v)).map(|_| ()),
        None => {
            map.remove(&key);
            Ok(())
        }
    }
}

impl<'a> Engine<'a> {
    // ------------------------------------------------------------------ logged writes and the dry run

    fn push_undo(&mut self, entry: Undo) -> Result<(), FailStop> {
        let slot = self.undo.get_mut(self.undo_len).ok_or(FailStop::Capacity)?;
        *slot = entry;
        self.undo_len += 1;
        Ok(())
    }

    pub(crate) fn order_w(&mut self, slot: u32) -> Result<&mut Order, FailStop> {
        if self.dry {
            let old = self.orders[slot as usize];
            self.push_undo(Undo::Order(slot, old))?;
        }
        Ok(&mut self.orders[slot as usize])
    }

    fn level_w(&mut self, index: usize) -> Result<&mut Level, FailStop> {
        if self.dry {
            let old = self.levels[index];
            self.push_undo(Undo::Level(index as u32, old))?;
        }
        Ok(&mut self.levels[index])
    }

    fn bits_w(&mut self, index: usize) -> Result<&mut u64, FailStop> {
        if self.dry {
            let old = self.level_bits[index];
            self.push_undo(Undo::Bits(index as u32, old))?;
        }
        Ok(&mut self.level_bits[index])
    }

    pub(crate) fn account_w(&mut self, slot: u32) -> Result<&mut Account, FailStop> {
        if self.dry {
            let old = self.accounts[slot as usize];
            self.push_undo(Undo::Account(slot, old))?;
        }
        Ok(&mut self.accounts[slot as usize])
    }

    fn holding_w(&mut self, key: HoldingKey) -> Result<&mut Holding, FailStop> {
        let old = self.holdings.get(&key).copied();
        if self.dry {
            self.push_undo(Undo::Holding(key, old))?;
        }
        if old.is_none() {
            capacity(self.holdings.insert(key, Holding::default()))?;
        }
        self.holdings.get_mut(&key).ok_or(FailStop::Capacity)
    }

    fn index_insert(&mut self, id: u64, slot: u32) -> Result<(), FailStop> {
        let old = capacity(self.order_index.insert(id, slot))?;
        if self.dry {
            self.push_undo(Undo::OrderIndex(id, old))?;
        }
        Ok(())
    }

    fn index_remove(&mut self, id: u64) -> Result<(), FailStop> {
        let old = self.order_index.remove(&id);
        if self.dry {
            self.push_undo(Undo::OrderIndex(id, old))?;
        }
        Ok(())
    }

    pub(crate) fn add_collateral(&mut self, event_id: u64, cents: i64) -> Result<(), FailStop> {
        let event = self.events.get_mut(&event_id).ok_or(FailStop::Inconsistent)?;
        let old = event.collateral;
        event.collateral = old.checked_add(cents).ok_or(FailStop::Overflow)?;
        if self.dry {
            self.push_undo(Undo::Collateral(event_id, old))?;
        }
        Ok(())
    }

    /// Adds to an account's balance and to the net cash the event has paid it (E-10.6, E-14.3; D-1).
    pub(crate) fn add_event_cash(&mut self, account_slot: u32, event_id: u64, cents: i64) -> Result<(), FailStop> {
        self.add_balance(account_slot, cents)?;
        let key = (self.accounts[account_slot as usize].id, event_id);
        let old = self.event_cash.get(&key).copied();
        let cash = old.unwrap_or(0).checked_add(cents).ok_or(FailStop::Overflow)?;
        capacity(self.event_cash.insert(key, cash))?;
        if self.dry {
            self.push_undo(Undo::EventCash(key, old))?;
        }
        Ok(())
    }

    pub(crate) fn maker_counter_w(&mut self, key: HoldingKey, value: WindowCounter) -> Result<(), FailStop> {
        let old = capacity(self.maker_counters.insert(key, value))?;
        if self.dry {
            self.push_undo(Undo::MakerCounter(key, old))?;
        }
        Ok(())
    }

    pub(crate) fn order_group_w(&mut self, key: (u64, u32), value: OrderGroup) -> Result<(), FailStop> {
        let old = capacity(self.order_groups.insert(key, value))?;
        if self.dry {
            self.push_undo(Undo::Group(key, old))?;
        }
        Ok(())
    }

    /// Starts a dry run: until [`Self::rollback`], writes are logged, and nothing is emitted, touched or marked.
    pub(crate) fn begin_dry_run(&mut self) -> Result<(), FailStop> {
        self.dry = true;
        self.undo_len = 0;
        let scalars = Undo::Scalars {
            next_trade: self.next_trade,
            free_order: self.free_order,
            resting_orders: self.resting_orders,
        };
        self.push_undo(scalars)
    }

    /// Ends a dry run, restoring every logged write in reverse order.
    pub(crate) fn rollback(&mut self) -> Result<(), FailStop> {
        while self.undo_len > 0 {
            self.undo_len -= 1;
            match self.undo[self.undo_len] {
                Undo::Order(slot, o) => self.orders[slot as usize] = o,
                Undo::Level(i, l) => self.levels[i as usize] = l,
                Undo::Bits(i, w) => self.level_bits[i as usize] = w,
                Undo::Account(slot, a) => self.accounts[slot as usize] = a,
                Undo::Holding(key, old) => restore(&mut self.holdings, key, old)?,
                Undo::OrderIndex(id, old) => restore(&mut self.order_index, id, old)?,
                Undo::Collateral(id, cents) => {
                    if let Some(e) = self.events.get_mut(&id) {
                        e.collateral = cents;
                    }
                }
                Undo::EventCash(key, old) => restore(&mut self.event_cash, key, old)?,
                Undo::MakerCounter(key, old) => restore(&mut self.maker_counters, key, old)?,
                Undo::Group(key, old) => restore(&mut self.order_groups, key, old)?,
                Undo::Scalars { next_trade, free_order, resting_orders } => {
                    self.next_trade = next_trade;
                    self.free_order = free_order;
                    self.resting_orders = resting_orders;
                }
            }
        }
        self.dry = false;
        Ok(())
    }

    // ------------------------------------------------------------------ prices (E-2.4, E-2.6)

    /// `yesCost(t, q) = 100 · q · numerator / denominator` cents (E-2.4).
    pub(crate) fn yes_cost(&self, instrument_slot: u32, tick: u16, quantity: u64) -> Result<i64, FailStop> {
        let inst = &self.instruments[instrument_slot as usize];
        let row = self.tick_rows[inst.table.start as usize + usize::from(tick)];
        let n = 100u128 * u128::from(quantity) * u128::from(row.numerator);
        let d = u128::from(row.denominator);
        if !n.is_multiple_of(d) {
            // Listing validation (E-6.3) makes this unreachable for quantities on the lot step.
            return Err(FailStop::Inconsistent);
        }
        i64::try_from(n / d).map_err(|_| FailStop::Overflow)
    }

    /// An order's cost on its own side for `quantity` contracts at `tick` (E-2.6).
    pub(crate) fn unit_cost(
        &self,
        instrument_slot: u32,
        side: Side,
        tick: u16,
        quantity: u64,
    ) -> Result<i64, FailStop> {
        let yes = self.yes_cost(instrument_slot, tick, quantity)?;
        if side == Side::BUY {
            return Ok(yes);
        }
        let whole = i64::try_from(100u128 * u128::from(quantity)).map_err(|_| FailStop::Overflow)?;
        whole.checked_sub(yes).ok_or(FailStop::Overflow)
    }

    /// The largest multiple of the lot step whose gross spend at `tick` does not exceed `cap`.
    pub(crate) fn affordable(&self, instrument_slot: u32, side: Side, tick: u16, cap: i64) -> Result<u64, FailStop> {
        let lot = u64::from(self.instruments[instrument_slot as usize].lot_step);
        let per_lot = self.unit_cost(instrument_slot, side, tick, lot)?;
        if per_lot <= 0 || cap <= 0 {
            return Ok(0);
        }
        Ok((cap / per_lot) as u64 * lot)
    }

    // ------------------------------------------------------------------ levels and occupancy

    pub(crate) fn level_idx(&self, instrument_slot: u32, side: Side, tick: u16) -> usize {
        crate::level_index(&self.instruments[instrument_slot as usize], side, tick)
    }

    pub(crate) fn level(&self, instrument_slot: u32, side: Side, tick: u16) -> Level {
        self.levels[self.level_idx(instrument_slot, side, tick)]
    }

    fn side_words(&self, instrument_slot: u32, side: Side) -> (usize, usize) {
        let inst = &self.instruments[instrument_slot as usize];
        let words = inst.words_per_side();
        (inst.bits_start as usize + if side == Side::BUY { 0 } else { words }, words)
    }

    fn word_index(&self, instrument_slot: u32, side: Side, tick: u16) -> (usize, u64) {
        let (base, _) = self.side_words(instrument_slot, side);
        (base + usize::from(tick) / 64, 1u64 << (tick % 64))
    }

    /// The best tick on one side: the highest bid or the lowest ask.
    pub(crate) fn best(&self, instrument_slot: u32, side: Side) -> Option<u16> {
        let (base, words) = self.side_words(instrument_slot, side);
        let bits = &self.level_bits[base..base + words];
        if side == Side::BUY {
            let (i, w) = bits.iter().enumerate().rev().find(|(_, w)| **w != 0)?;
            Some((i * 64 + 63 - w.leading_zeros() as usize) as u16)
        } else {
            let (i, w) = bits.iter().enumerate().find(|(_, w)| **w != 0)?;
            Some((i * 64 + w.trailing_zeros() as usize) as u16)
        }
    }

    /// The best opposite tick an order of `side` with limit `limit` crosses (E-10.1).
    pub(crate) fn next_crossing(&self, instrument_slot: u32, side: Side, limit: u16) -> Option<u16> {
        let best = self.best(instrument_slot, opposite(side))?;
        crosses(side, best, limit).then_some(best)
    }

    // ------------------------------------------------------------------ the slab

    pub(crate) fn alloc_order(&mut self) -> Result<u32, FailStop> {
        let slot = self.free_order;
        if slot == NONE {
            return Err(FailStop::Capacity);
        }
        self.free_order = self.orders[slot as usize].account_next;
        Ok(slot)
    }

    /// Returns a slot that no longer rests to the free list. `next` is kept, so a walk holding this slot as its
    /// next order can still step past it.
    pub(crate) fn free_slot(&mut self, slot: u32) -> Result<(), FailStop> {
        let (next, free) = (self.orders[slot as usize].next, self.free_order);
        *self.order_w(slot)? = Order { next, prev: NONE, account_next: free, ..Order::default() };
        self.free_order = slot;
        Ok(())
    }

    /// Puts an order at the back of its tick's queue, on its account's list and in the ID index (E-9.9).
    pub(crate) fn rest(&mut self, slot: u32) -> Result<(), FailStop> {
        let o = self.orders[slot as usize];
        self.touch_level(o.instrument_slot, o.side, o.tick)?;
        let li = self.level_idx(o.instrument_slot, o.side, o.tick);
        let tail = self.levels[li].tail;
        {
            let w = self.order_w(slot)?;
            w.prev = tail;
            w.next = NONE;
            w.in_use = true;
        }
        if tail != NONE {
            self.order_w(tail)?.next = slot;
        }
        let l = self.level_w(li)?;
        if l.head == NONE {
            l.head = slot;
        }
        l.tail = slot;
        l.quantity = l.quantity.checked_add(u64::from(o.remaining)).ok_or(FailStop::Overflow)?;
        l.count = l.count.checked_add(1).ok_or(FailStop::Overflow)?;
        let (w, bit) = self.word_index(o.instrument_slot, o.side, o.tick);
        *self.bits_w(w)? |= bit;
        let account = self.account_w(o.account_slot)?;
        let old_head = account.first_order;
        account.first_order = slot;
        account.resting_orders = account.resting_orders.checked_add(1).ok_or(FailStop::Overflow)?;
        {
            let w = self.order_w(slot)?;
            w.account_prev = NONE;
            w.account_next = old_head;
        }
        if old_head != NONE {
            self.order_w(old_head)?.account_prev = slot;
        }
        self.index_insert(o.id, slot)?;
        if let Some(at) = o.expires_at {
            self.gtd_push((at, o.id))?;
        }
        self.resting_orders = self.resting_orders.checked_add(1).ok_or(FailStop::Overflow)?;
        Ok(())
    }

    /// Takes a resting order out of its queue, its account's list and the ID index. The slot stays allocated and
    /// keeps its `next`, so a walk can step past it.
    pub(crate) fn unlink(&mut self, slot: u32) -> Result<(), FailStop> {
        let o = self.orders[slot as usize];
        if !o.in_use {
            return Err(FailStop::Inconsistent);
        }
        self.touch_level(o.instrument_slot, o.side, o.tick)?;
        if o.prev != NONE {
            self.order_w(o.prev)?.next = o.next;
        }
        if o.next != NONE {
            self.order_w(o.next)?.prev = o.prev;
        }
        let li = self.level_idx(o.instrument_slot, o.side, o.tick);
        let l = self.level_w(li)?;
        if l.head == slot {
            l.head = o.next;
        }
        if l.tail == slot {
            l.tail = o.prev;
        }
        l.quantity = l.quantity.checked_sub(u64::from(o.remaining)).ok_or(FailStop::Inconsistent)?;
        l.count = l.count.checked_sub(1).ok_or(FailStop::Inconsistent)?;
        if l.count == 0 {
            let (w, bit) = self.word_index(o.instrument_slot, o.side, o.tick);
            *self.bits_w(w)? &= !bit;
        }
        if o.account_prev != NONE {
            self.order_w(o.account_prev)?.account_next = o.account_next;
        } else {
            self.account_w(o.account_slot)?.first_order = o.account_next;
        }
        if o.account_next != NONE {
            self.order_w(o.account_next)?.account_prev = o.account_prev;
        }
        let account = self.account_w(o.account_slot)?;
        account.resting_orders = account.resting_orders.checked_sub(1).ok_or(FailStop::Inconsistent)?;
        self.index_remove(o.id)?;
        self.resting_orders = self.resting_orders.checked_sub(1).ok_or(FailStop::Inconsistent)?;
        self.order_w(slot)?.in_use = false;
        Ok(())
    }

    /// Changes an order's remaining quantity, keeping its level's aggregate right if it rests.
    pub(crate) fn set_remaining(&mut self, slot: u32, remaining: u32) -> Result<(), FailStop> {
        let o = self.orders[slot as usize];
        if o.in_use {
            self.touch_level(o.instrument_slot, o.side, o.tick)?;
            let li = self.level_idx(o.instrument_slot, o.side, o.tick);
            let l = self.level_w(li)?;
            let without = l.quantity.checked_sub(u64::from(o.remaining)).ok_or(FailStop::Inconsistent)?;
            l.quantity = without.checked_add(u64::from(remaining)).ok_or(FailStop::Overflow)?;
        }
        self.order_w(slot)?.remaining = remaining;
        Ok(())
    }

    /// Resting orders passing `keep`, as `(key, orderId)` in ascending order, in `scratch[..n]`.
    pub(crate) fn collect(
        &mut self,
        mut keep: impl FnMut(&Order, &Instrument) -> bool,
        key: impl Fn(&Order) -> u64,
    ) -> usize {
        let Self { order_index, orders, instruments, scratch, .. } = self;
        let mut n = 0;
        order_index.for_each(|&id, &slot| {
            let o = &orders[slot as usize];
            if keep(o, &instruments[o.instrument_slot as usize]) {
                scratch[n] = (key(o), id);
                n += 1;
            }
        });
        scratch[..n].sort_unstable();
        n
    }

    // ------------------------------------------------------------------ orders that expire (E-8.1)

    /// Records that a resting order expires, compacting first if there is no room.
    pub(crate) fn gtd_push(&mut self, entry: (u64, u64)) -> Result<(), FailStop> {
        if self.gtd_len == self.gtd.len() {
            self.gtd_compact();
        }
        if heap_push(self.gtd, &mut self.gtd_len, entry) {
            Ok(())
        } else {
            Err(FailStop::Capacity)
        }
    }

    /// The earliest record, taken off the heap, if it has expired by `now`.
    pub(crate) fn gtd_pop_due(&mut self, now: u64) -> Option<(u64, u64)> {
        heap_pop_due(self.gtd, &mut self.gtd_len, now)
    }

    /// Drops records whose order has gone, or no longer expires when the record says, and keeps one record per
    /// order: an amendment that loses priority rests the order again (E-12.5), which records it again. What is left
    /// is one record per resting order that expires, so it can never outnumber the orders, which is half the heap,
    /// and so compacting a full heap always makes room.
    fn gtd_compact(&mut self) {
        let Self { gtd, gtd_len, order_index, orders, .. } = self;
        heap_compact(gtd, gtd_len, |(at, id)| {
            order_index.get(&id).is_some_and(|&slot| {
                let o = &orders[slot as usize];
                o.in_use && o.expires_at == Some(at)
            })
        });
    }

    // ------------------------------------------------------------------ holdings, locks and reservations (§11)

    pub(crate) fn set_position(&mut self, account_id: u64, instrument_id: u64, position: i64) -> Result<(), FailStop> {
        self.touch_position(account_id, instrument_id)?;
        self.holding_w((account_id, instrument_id))?.position = position;
        Ok(())
    }

    pub(crate) fn add_balance(&mut self, account_slot: u32, cents: i64) -> Result<(), FailStop> {
        self.touch_sleeve(account_slot)?;
        let a = self.account_w(account_slot)?;
        a.balance = a.balance.checked_add(cents).ok_or(FailStop::Overflow)?;
        Ok(())
    }

    fn add_reserved(&mut self, account_slot: u32, cents: i64) -> Result<(), FailStop> {
        self.touch_sleeve(account_slot)?;
        let a = self.account_w(account_slot)?;
        a.reserved = a.reserved.checked_add(cents).ok_or(FailStop::Overflow)?;
        Ok(())
    }

    /// The holding an order of `side` would reduce, less what resting orders already lock (E-9.5).
    pub(crate) fn unlocked_holding(&self, account_id: u64, instrument_id: u64, side: Side) -> i64 {
        let h = self.holding(account_id, instrument_id);
        if side == Side::SELL {
            h.position.max(0) - h.yes_locked
        } else {
            (-h.position).max(0) - h.no_locked
        }
    }

    /// Adds to the account's locked holding on the side an order of `side` reduces.
    pub(crate) fn add_locked(
        &mut self,
        account_id: u64,
        instrument_id: u64,
        side: Side,
        delta: i64,
    ) -> Result<(), FailStop> {
        let h = self.holding_w((account_id, instrument_id))?;
        let locked = if side == Side::SELL { &mut h.yes_locked } else { &mut h.no_locked };
        *locked = locked.checked_add(delta).ok_or(FailStop::Overflow)?;
        Ok(())
    }

    /// Sets an order's lock, keeping the account's locked total the sum of its orders' locks.
    pub(crate) fn set_lock(&mut self, slot: u32, lock: u32) -> Result<(), FailStop> {
        let o = self.orders[slot as usize];
        if lock != o.lock {
            let instrument_id = self.instruments[o.instrument_slot as usize].id;
            self.add_locked(o.account_id, instrument_id, o.side, i64::from(lock) - i64::from(o.lock))?;
            self.order_w(slot)?.lock = lock;
        }
        Ok(())
    }

    /// E-11.2: the unit cost at the order's limit for `remaining − lock`, capped at its spend cap.
    pub(crate) fn reservation_of(&self, o: &Order) -> Result<i64, FailStop> {
        let uncovered = o.remaining.checked_sub(o.lock).ok_or(FailStop::Inconsistent)?;
        let cost = self.unit_cost(o.instrument_slot, o.side, o.tick, u64::from(uncovered))?;
        Ok(o.spend_cap_remaining.map_or(cost, |cap| cost.min(cap)))
    }

    /// Recomputes an order's reservation and moves the difference in its account's reserved total.
    pub(crate) fn refresh_reserved(&mut self, slot: u32) -> Result<(), FailStop> {
        let o = self.orders[slot as usize];
        let new = self.reservation_of(&o)?;
        if new != o.reserved {
            self.add_reserved(o.account_slot, new - o.reserved)?;
            self.order_w(slot)?.reserved = new;
        }
        Ok(())
    }

    /// E-11.5, E-12.1: releases an order's lock and reservation.
    pub(crate) fn release(&mut self, slot: u32) -> Result<(), FailStop> {
        self.set_lock(slot, 0)?;
        let o = self.orders[slot as usize];
        if o.reserved != 0 {
            self.add_reserved(o.account_slot, -o.reserved)?;
            self.order_w(slot)?.reserved = 0;
        }
        Ok(())
    }
}

// -------------------------------------------------------------------------- the expiry heap (E-8.1)
//
// A min-heap of `(expiresAt, orderId)` in `heap[..len]`. Ordering by the pair is what makes the expiries happen in
// the order the rules require, and the smallest is always at the front, so a tick pays for what expired rather
// than for every order that rests.

/// Adds `entry`. False if the heap is full, which is the caller's signal to compact.
fn heap_push(heap: &mut [(u64, u64)], len: &mut usize, entry: (u64, u64)) -> bool {
    if *len == heap.len() {
        return false;
    }
    heap[*len] = entry;
    *len += 1;
    let mut i = *len - 1;
    while i > 0 {
        let parent = (i - 1) / 2;
        if heap[parent] <= heap[i] {
            break;
        }
        heap.swap(parent, i);
        i = parent;
    }
    true
}

/// The smallest entry, taken off the heap, if it has expired by `now`.
fn heap_pop_due(heap: &mut [(u64, u64)], len: &mut usize, now: u64) -> Option<(u64, u64)> {
    if *len == 0 {
        return None;
    }
    let first = heap[0];
    if first.0 > now {
        return None;
    }
    *len -= 1;
    heap[0] = heap[*len];
    heap_sift_down(heap, *len, 0);
    Some(first)
}

fn heap_sift_down(heap: &mut [(u64, u64)], len: usize, mut i: usize) {
    loop {
        let (left, right) = (2 * i + 1, 2 * i + 2);
        let mut smallest = i;
        if left < len && heap[left] < heap[smallest] {
            smallest = left;
        }
        if right < len && heap[right] < heap[smallest] {
            smallest = right;
        }
        if smallest == i {
            return;
        }
        heap.swap(i, smallest);
        i = smallest;
    }
}

/// Keeps the entries `live` accepts, one copy of each.
fn heap_compact(heap: &mut [(u64, u64)], len: &mut usize, mut live: impl FnMut((u64, u64)) -> bool) {
    let mut n = 0;
    for i in 0..*len {
        let entry = heap[i];
        if live(entry) {
            heap[n] = entry;
            n += 1;
        }
    }
    // An order's expiry never changes, so its entries are all the same pair and sorting puts them side by side.
    heap[..n].sort_unstable();
    let mut m = 0;
    for i in 0..n {
        if m == 0 || heap[m - 1] != heap[i] {
            heap[m] = heap[i];
            m += 1;
        }
    }
    // Sorted is already a heap, so there is nothing to rebuild.
    *len = m;
}

#[cfg(test)]
mod tests {
    use super::{heap_compact, heap_pop_due, heap_push};

    #[test]
    fn entries_come_back_by_expiry_then_order_id() {
        let mut heap = [(0u64, 0u64); 8];
        let mut len = 0;
        for entry in [(30, 2), (10, 5), (30, 1), (20, 9)] {
            assert!(heap_push(&mut heap, &mut len, entry));
        }
        let mut got = [(0u64, 0u64); 4];
        for slot in &mut got {
            *slot = heap_pop_due(&mut heap, &mut len, 100).unwrap();
        }
        // The order a tick must expire them in, which is also what the table scan used to produce.
        assert_eq!(got, [(10, 5), (20, 9), (30, 1), (30, 2)]);
        assert_eq!(len, 0);
    }

    #[test]
    fn nothing_comes_back_before_its_time() {
        let mut heap = [(0u64, 0u64); 4];
        let mut len = 0;
        assert!(heap_push(&mut heap, &mut len, (100, 1)));
        assert!(heap_push(&mut heap, &mut len, (50, 2)));
        assert_eq!(heap_pop_due(&mut heap, &mut len, 49), None);
        assert_eq!(heap_pop_due(&mut heap, &mut len, 50), Some((50, 2)));
        assert_eq!(heap_pop_due(&mut heap, &mut len, 99), None);
        assert_eq!(heap_pop_due(&mut heap, &mut len, 100), Some((100, 1)));
        assert_eq!(len, 0);
    }

    #[test]
    fn compacting_drops_dead_entries_and_keeps_one_of_each() {
        // An amendment that loses priority rests the order again (E-12.5), recording it again, so the same pair
        // can appear several times; order 2 is one that has since been cancelled or filled.
        let mut heap = [(0u64, 0u64); 8];
        let mut len = 0;
        for entry in [(10, 1), (10, 1), (20, 2), (30, 3), (10, 1), (30, 3)] {
            assert!(heap_push(&mut heap, &mut len, entry));
        }
        heap_compact(&mut heap, &mut len, |(_, id)| id != 2);
        assert_eq!(len, 2);
        assert_eq!(heap_pop_due(&mut heap, &mut len, 100), Some((10, 1)));
        assert_eq!(heap_pop_due(&mut heap, &mut len, 100), Some((30, 3)));
        assert_eq!(len, 0);
    }

    #[test]
    fn a_full_heap_refuses_and_compacting_makes_room() {
        let mut heap = [(0u64, 0u64); 4];
        let mut len = 0;
        for id in 0..4u64 {
            assert!(heap_push(&mut heap, &mut len, (10, id)));
        }
        assert!(!heap_push(&mut heap, &mut len, (10, 4)));
        heap_compact(&mut heap, &mut len, |(_, id)| id == 0);
        assert_eq!(len, 1);
        assert!(heap_push(&mut heap, &mut len, (10, 4)));
        assert_eq!(heap_pop_due(&mut heap, &mut len, 10), Some((10, 0)));
        assert_eq!(heap_pop_due(&mut heap, &mut len, 10), Some((10, 4)));
    }

    #[test]
    fn compacting_all_live_distinct_entries_keeps_them_all() {
        // The engine sizes the heap at twice the orders, and one resting order leaves one live entry, so a
        // compaction can never be asked to fit more than half a heap.
        let mut heap = [(0u64, 0u64); 8];
        let mut len = 0;
        for id in 0..4u64 {
            assert!(heap_push(&mut heap, &mut len, (100 - id, id)));
        }
        heap_compact(&mut heap, &mut len, |_| true);
        assert_eq!(len, 4);
        assert_eq!(heap_pop_due(&mut heap, &mut len, 100), Some((97, 3)));
    }
}
