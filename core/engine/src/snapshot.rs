//! Writing the engine's state as spec §20 defines it.
//!
//! The bytes here depend on the venue's state and on nothing else — not on the order rows were inserted in, not on
//! which slab slot an order landed in, not on how large the tables are, and not on the hash seed. Two engines that
//! the specification cannot tell apart produce identical bytes (E-20.2), which is the whole point: the equivalence
//! property (E-20.9) is a property only if that holds, and a coincidence of history otherwise.
//!
//! Three rules do that work, and every awkwardness below is one of them being obeyed:
//!
//!   - **Order by key, never by slot.** Every keyed table is gathered and sorted before it is written. The engine
//!     already does this for matching (`collect`), with the same scratch and the same destructuring to borrow it
//!     beside the map it is reading.
//!   - **Queue order where no key can carry it.** A book's time priority is state. It is the one ordering that is
//!     written as an order rather than recovered from a key (E-20.4).
//!   - **Nothing at its initial value.** E-3.2 makes an account of all-initial values indistinguishable from one
//!     never mentioned, so writing it would break the property it is here to keep (E-20.5).
//!
//! What is *not* written is as deliberate: no reserved totals or level aggregates (a decoder recomputes them,
//! E-20.6), no table sizes, and none of the residue a running engine carries — dead window records, expired fee
//! overrides, slots for accounts holding nothing. That residue is invisible to every rule but `FailStop::Capacity`,
//! and leaving it out runs the safe way round: a restored engine has more room than the one it came from, never
//! less (E-20.7, D-31).

use crate::fixed::FixedMap;
use crate::state::{Instrument, TickTable, NONE};
use crate::{Engine, FailStop};

/// The encoding this module writes, carried in `Snapshot.stateVersion`. A reader that does not implement it must
/// refuse the snapshot rather than guess at it; the journal around it stays readable either way.
pub const STATE_VERSION: u16 = 1;

/// Appends to a caller's buffer, refusing to run past it. `FailStop::Capacity` is the engine's own word for "this
/// did not fit", and a snapshot that does not fit is the same kind of answer as a table that is full.
struct Writer<'b> {
    buf: &'b mut [u8],
    at: usize,
}

impl<'b> Writer<'b> {
    fn new(buf: &'b mut [u8]) -> Self {
        Self { buf, at: 0 }
    }

    fn bytes(&mut self, b: &[u8]) -> Result<(), FailStop> {
        let end = self.at.checked_add(b.len()).ok_or(FailStop::Overflow)?;
        let room = self.buf.get_mut(self.at..end).ok_or(FailStop::Capacity)?;
        room.copy_from_slice(b);
        self.at = end;
        Ok(())
    }

    fn u8(&mut self, v: u8) -> Result<(), FailStop> {
        self.bytes(&[v])
    }

    fn bool(&mut self, v: bool) -> Result<(), FailStop> {
        self.u8(u8::from(v))
    }

    fn u16(&mut self, v: u16) -> Result<(), FailStop> {
        self.bytes(&v.to_le_bytes())
    }

    fn u32(&mut self, v: u32) -> Result<(), FailStop> {
        self.bytes(&v.to_le_bytes())
    }

    fn u64(&mut self, v: u64) -> Result<(), FailStop> {
        self.bytes(&v.to_le_bytes())
    }

    fn i32(&mut self, v: i32) -> Result<(), FailStop> {
        self.bytes(&v.to_le_bytes())
    }

    fn i64(&mut self, v: i64) -> Result<(), FailStop> {
        self.bytes(&v.to_le_bytes())
    }

    /// An optional number as a presence byte and, when present, the number. Absent writes a zero byte and nothing
    /// else, so `None` and `Some(0)` do not encode alike.
    fn opt_u64(&mut self, v: Option<u64>) -> Result<(), FailStop> {
        match v {
            None => self.u8(0),
            Some(n) => {
                self.u8(1)?;
                self.u64(n)
            }
        }
    }

    fn opt_i64(&mut self, v: Option<i64>) -> Result<(), FailStop> {
        match v {
            None => self.u8(0),
            Some(n) => {
                self.u8(1)?;
                self.i64(n)
            }
        }
    }

    fn opt_u32(&mut self, v: Option<u32>) -> Result<(), FailStop> {
        match v {
            None => self.u8(0),
            Some(n) => {
                self.u8(1)?;
                self.u32(n)
            }
        }
    }

    /// A count, written before the entries it counts. `u32` because no table the engine can hold is larger.
    fn count(&mut self, n: usize) -> Result<(), FailStop> {
        self.u32(u32::try_from(n).map_err(|_| FailStop::Overflow)?)
    }
}

/// The tick table an instrument uses, by its ID.
///
/// An instrument keeps a copy of the table's pool range rather than the ID it was listed with, so the ID has to be
/// found again. The range identifies it: rows are bump-allocated and never reclaimed, so no two tables share a
/// `start`. Writing the range instead would put a pool offset in the snapshot, which is the one thing E-20.2 says
/// it may not contain.
fn tick_table_id(tables: &FixedMap<'_, u32, TickTable>, wanted: TickTable) -> Option<u32> {
    let mut found = None;
    tables.for_each(|&id, &table| {
        if table == wanted {
            found = Some(id);
        }
    });
    found
}

impl Engine<'_> {
    /// Writes this engine's state into `out`, returning how many bytes it used.
    ///
    /// Taken between inputs and never during one, and never from an engine that has fail-stopped (E-20.1): its
    /// state is the state at which processing stopped, and nothing may continue from it.
    pub fn snapshot(&mut self, out: &mut [u8]) -> Result<usize, FailStop> {
        if self.stopped {
            return Err(FailStop::Stopped);
        }
        let mut w = Writer::new(out);
        self.write_scalars(&mut w)?;
        self.write_tick_tables(&mut w)?;
        self.write_events(&mut w)?;
        self.write_releases(&mut w)?;
        self.write_instruments_and_books(&mut w)?;
        self.write_orders(&mut w)?;
        self.write_accounts(&mut w)?;
        self.write_holdings(&mut w)?;
        self.write_event_cash(&mut w)?;
        self.write_order_groups(&mut w)?;
        self.write_maker(&mut w)?;
        self.write_duplicates(&mut w)?;
        self.write_commands(&mut w)?;
        self.write_gtd(&mut w)?;
        self.write_fee_overrides(&mut w)?;
        Ok(w.at)
    }

    fn write_scalars(&mut self, w: &mut Writer<'_>) -> Result<(), FailStop> {
        w.u64(self.last_sequence)?;
        w.u64(self.now)?;
        w.u16(self.protocol_version)?;
        w.u8(self.venue_mode as u8)?;
        w.u32(self.active_order_limit)?;
        w.u32(self.client_order_id_window_seconds)?;
        w.u32(self.settled_retention_seconds)?;
        w.u32(self.max_events)?;
        w.u32(self.max_instruments)?;
        w.u32(self.max_price_levels)?;
        w.u64(self.next_order)?;
        w.u64(self.next_trade)?;
        // The unit serial travels with each account's `feesUnit` and `feesCharged` below. Without it a stale unit
        // number could collide with the restored numbering and E-3.3's floor would be computed from fees charged
        // in a unit this engine never ran (E-20.3).
        w.u64(self.unit_serial)?;
        w.i32(self.fee_schedule.maker_micros)?;
        w.i32(self.fee_schedule.taker_micros)?;
        match self.default_limits {
            None => w.u8(0)?,
            Some(l) => {
                w.u8(1)?;
                w.u32(l.max_resting_orders)?;
                w.u32(l.max_new_orders_per_second)?;
                w.u32(l.max_order_to_trade_ratio_milli)?;
                w.u32(l.ratio_free_orders_per_second)?;
            }
        }
        Ok(())
    }

    fn write_tick_tables(&mut self, w: &mut Writer<'_>) -> Result<(), FailStop> {
        let Self { tick_tables, tick_rows, scratch, .. } = self;
        let mut n = 0;
        tick_tables.for_each(|&id, _| {
            scratch[n] = (u64::from(id), 0);
            n += 1;
        });
        scratch[..n].sort_unstable();
        w.count(n)?;
        for &(id, _) in scratch[..n].iter() {
            let id = id as u32;
            let table = *tick_tables.get(&id).ok_or(FailStop::Inconsistent)?;
            w.u32(id)?;
            w.u16(table.len)?;
            // Rows in their given order, which is the order that defines them (E-20.4). The pool range they sit
            // in is not written: it is where this engine happened to put them.
            let start = table.start as usize;
            for row in tick_rows.get(start..start + usize::from(table.len)).ok_or(FailStop::Inconsistent)? {
                w.u32(row.numerator)?;
                w.u32(row.denominator)?;
                w.u16(row.complement)?;
            }
        }
        Ok(())
    }

    fn write_events(&mut self, w: &mut Writer<'_>) -> Result<(), FailStop> {
        let Self { events, scratch, .. } = self;
        let mut n = 0;
        events.for_each(|&id, _| {
            scratch[n] = (id, 0);
            n += 1;
        });
        scratch[..n].sort_unstable();
        w.count(n)?;
        for &(id, _) in scratch[..n].iter() {
            let e = events.get(&id).ok_or(FailStop::Inconsistent)?;
            w.u64(id)?;
            w.u8(e.state_count)?;
            w.u64(e.scheduled_start)?;
            w.bool(e.started)?;
            w.bool(e.suspended)?;
            w.bool(e.ended)?;
            w.bool(e.resolved)?;
            w.u64(e.resolved_at)?;
            w.i64(e.collateral)?;
            // `first_instrument` and `instrument_count` are not written: the instruments name their event, and a
            // decoder rebuilds the chain in instrument order, which is the order this one is kept in.
        }
        Ok(())
    }

    /// The releases (E-8.4), in the order they will happen, which is the order they resolved. One release happens
    /// per tick, so the order is observable and has to be written as an order — `resolvedAt` cannot recover it.
    fn write_releases(&mut self, w: &mut Writer<'_>) -> Result<(), FailStop> {
        let Self { events, release_head, scratch, .. } = self;
        let mut n = 0;
        let mut at = *release_head;
        while let Some(id) = at {
            scratch[n] = (id, 0);
            n += 1;
            at = events.get(&id).ok_or(FailStop::Inconsistent)?.next_release;
        }
        w.count(n)?;
        for &(id, _) in scratch[..n].iter() {
            w.u64(id)?;
        }
        Ok(())
    }

    /// Instruments ascending by ID, each followed by its book: `BUY` before `SELL`, ticks ascending, and the
    /// orders at a tick in queue order, head first — the order in which they would match (E-20.4).
    fn write_instruments_and_books(&mut self, w: &mut Writer<'_>) -> Result<(), FailStop> {
        let Self { instrument_index, instruments, levels, orders, tick_tables, scratch, .. } = self;
        let mut n = 0;
        instrument_index.for_each(|&id, &slot| {
            scratch[n] = (id, u64::from(slot));
            n += 1;
        });
        scratch[..n].sort_unstable();
        w.count(n)?;
        for &(id, slot) in scratch[..n].iter() {
            let inst: &Instrument = instruments.get(slot as usize).ok_or(FailStop::Inconsistent)?;
            w.u64(id)?;
            w.u64(inst.event_id)?;
            w.u8(inst.class as u8)?;
            w.u32(tick_table_id(tick_tables, inst.table).ok_or(FailStop::Inconsistent)?)?;
            w.u32(inst.lot_step)?;
            w.u8(inst.policy as u8)?;
            w.u64(inst.mask)?;
            w.u32(inst.position_limit)?;
            w.u8(inst.market_state as u8)?;

            let ticks = usize::from(inst.table.len);
            for side in 0..2u8 {
                // Count the occupied ticks first: the count precedes the entries, and a level is occupied when it
                // has a head, whatever its aggregates say.
                let base = inst.levels_start as usize + usize::from(side) * ticks;
                let occupied = levels
                    .get(base..base + ticks)
                    .ok_or(FailStop::Inconsistent)?
                    .iter()
                    .filter(|l| l.head != NONE)
                    .count();
                w.count(occupied)?;
                for tick in 0..ticks {
                    let level = &levels[base + tick];
                    if level.head == NONE {
                        continue;
                    }
                    w.u16(u16::try_from(tick).map_err(|_| FailStop::Overflow)?)?;
                    // Walk the FIFO rather than trusting `count`, which is an aggregate a decoder rebuilds.
                    let mut queued = 0usize;
                    let mut at = level.head;
                    while at != NONE {
                        queued += 1;
                        at = orders.get(at as usize).ok_or(FailStop::Inconsistent)?.next;
                    }
                    w.count(queued)?;
                    let mut at = level.head;
                    while at != NONE {
                        let o = orders.get(at as usize).ok_or(FailStop::Inconsistent)?;
                        w.u64(o.id)?;
                        at = o.next;
                    }
                }
            }
        }
        Ok(())
    }

    fn write_orders(&mut self, w: &mut Writer<'_>) -> Result<(), FailStop> {
        let Self { order_index, orders, instruments, scratch, .. } = self;
        let mut n = 0;
        order_index.for_each(|&id, &slot| {
            scratch[n] = (id, u64::from(slot));
            n += 1;
        });
        scratch[..n].sort_unstable();
        w.count(n)?;
        for &(id, slot) in scratch[..n].iter() {
            let o = orders.get(slot as usize).ok_or(FailStop::Inconsistent)?;
            w.u64(id)?;
            w.u64(o.account_id)?;
            w.u64(o.session_id)?;
            w.bytes(&o.client_order_id)?;
            // The instrument by ID, never by slot.
            w.u64(instruments.get(o.instrument_slot as usize).ok_or(FailStop::Inconsistent)?.id)?;
            w.u8(o.side as u8)?;
            w.u16(o.tick)?;
            w.u32(o.remaining)?;
            w.u8(o.time_in_force as u8)?;
            w.opt_u64(o.expires_at)?;
            w.u8(o.at_event_start as u8)?;
            w.bool(o.post_only)?;
            w.bool(o.reduce_only)?;
            w.opt_i64(o.spend_cap_remaining)?;
            w.opt_u32(o.group)?;
            w.u64(o.accepted_at)?;
            w.u32(o.lock)?;
            w.i64(o.reserved)?;
        }
        Ok(())
    }

    fn write_accounts(&mut self, w: &mut Writer<'_>) -> Result<(), FailStop> {
        let Self { account_index, accounts, scratch, .. } = self;
        let mut n = 0;
        account_index.for_each(|&id, &slot| {
            scratch[n] = (id, u64::from(slot));
            n += 1;
        });
        scratch[..n].sort_unstable();
        // An account at every initial value is indistinguishable from one never mentioned (E-3.2), so writing it
        // would make two engines the specification cannot tell apart differ here (E-20.5). Its slot is the only
        // trace it leaves, and a slot is exactly what a snapshot does not carry.
        let mut kept = 0usize;
        for i in 0..n {
            let a = accounts.get(scratch[i].1 as usize).ok_or(FailStop::Inconsistent)?;
            let initial = a.balance == 0
                && a.reserved == 0
                && !a.closed_only
                && a.second.is_none()
                && a.limits.is_none()
                && a.fees_charged == 0
                && a.first_order == NONE
                && (a.self_match_prevention as u8) == (crate::state::Account::default().self_match_prevention as u8);
            if !initial {
                scratch[kept] = scratch[i];
                kept += 1;
            }
        }
        w.count(kept)?;
        for &(id, slot) in scratch[..kept].iter() {
            let a = accounts.get(slot as usize).ok_or(FailStop::Inconsistent)?;
            w.u64(id)?;
            w.i64(a.balance)?;
            w.bool(a.closed_only)?;
            w.u8(a.self_match_prevention as u8)?;
            w.opt_u64(a.second)?;
            w.u32(a.new_orders_in_second)?;
            w.u32(a.fills_in_second)?;
            match a.limits {
                None => w.u8(0)?,
                Some(l) => {
                    w.u8(1)?;
                    w.u32(l.max_resting_orders)?;
                    w.u32(l.max_new_orders_per_second)?;
                    w.u32(l.max_order_to_trade_ratio_milli)?;
                    w.u32(l.ratio_free_orders_per_second)?;
                }
            }
            w.i64(a.fees_charged)?;
            w.u64(a.fees_unit)?;
            // `reserved`, `resting_orders` and `first_order` are not written: a decoder recomputes the first two
            // from the orders and rebuilds the third in any order it likes (E-20.6). The account's own list is
            // kept in no useful order by design, so there is nothing in it to preserve.
        }
        Ok(())
    }

    fn write_holdings(&mut self, w: &mut Writer<'_>) -> Result<(), FailStop> {
        let Self { holdings, scratch, .. } = self;
        let mut n = 0;
        holdings.for_each(|&(account, instrument), h| {
            if h.position != 0 || h.yes_locked != 0 || h.no_locked != 0 {
                scratch[n] = (account, instrument);
                n += 1;
            }
        });
        scratch[..n].sort_unstable();
        w.count(n)?;
        for &key in scratch[..n].iter() {
            let h = holdings.get(&key).ok_or(FailStop::Inconsistent)?;
            w.u64(key.0)?;
            w.u64(key.1)?;
            w.i64(h.position)?;
            w.i64(h.yes_locked)?;
            w.i64(h.no_locked)?;
        }
        Ok(())
    }

    fn write_event_cash(&mut self, w: &mut Writer<'_>) -> Result<(), FailStop> {
        let Self { event_cash, scratch, .. } = self;
        let mut n = 0;
        event_cash.for_each(|&(account, event), &cents| {
            if cents != 0 {
                scratch[n] = (account, event);
                n += 1;
            }
        });
        scratch[..n].sort_unstable();
        w.count(n)?;
        for &key in scratch[..n].iter() {
            w.u64(key.0)?;
            w.u64(key.1)?;
            w.i64(*event_cash.get(&key).ok_or(FailStop::Inconsistent)?)?;
        }
        Ok(())
    }

    fn write_order_groups(&mut self, w: &mut Writer<'_>) -> Result<(), FailStop> {
        let Self { order_groups, scratch, .. } = self;
        let mut n = 0;
        order_groups.for_each(|&(account, group), _| {
            scratch[n] = (account, u64::from(group));
            n += 1;
        });
        scratch[..n].sort_unstable();
        w.count(n)?;
        for &(account, group) in scratch[..n].iter() {
            let group = group as u32;
            let g = order_groups.get(&(account, group)).ok_or(FailStop::Inconsistent)?;
            w.u64(account)?;
            w.u32(group)?;
            w.u32(g.threshold)?;
            w.u32(g.window_millis)?;
            w.u64(g.count)?;
            w.opt_u64(g.window_start)?;
            w.bool(g.triggered)?;
        }
        Ok(())
    }

    fn write_maker(&mut self, w: &mut Writer<'_>) -> Result<(), FailStop> {
        {
            let Self { maker_settings, scratch, .. } = self;
            let mut n = 0;
            maker_settings.for_each(|&key, _| {
                scratch[n] = key;
                n += 1;
            });
            scratch[..n].sort_unstable();
            w.count(n)?;
            for &key in scratch[..n].iter() {
                let m = maker_settings.get(&key).ok_or(FailStop::Inconsistent)?;
                w.u64(key.0)?;
                w.u64(key.1)?;
                w.u32(m.threshold)?;
                w.u32(m.window_millis)?;
            }
        }
        let Self { maker_counters, scratch, .. } = self;
        let mut n = 0;
        maker_counters.for_each(|&key, c| {
            if c.count != 0 || c.window_start.is_some() {
                scratch[n] = key;
                n += 1;
            }
        });
        scratch[..n].sort_unstable();
        w.count(n)?;
        for &key in scratch[..n].iter() {
            let c = maker_counters.get(&key).ok_or(FailStop::Inconsistent)?;
            w.u64(key.0)?;
            w.u64(key.1)?;
            w.u64(c.count)?;
            w.opt_u64(c.window_start)?;
        }
        Ok(())
    }

    /// The client-order-ID window (E-9.2), ascending by `(accountId, clientOrderId)`.
    ///
    /// The key is forty bytes and the engine's scratch holds pairs of eight, so the slots are gathered instead and
    /// sorted by the key each one names. The expiry records that index this map are not written at all: they carry
    /// entries that have already gone, and that residue is the engine's own (E-20.7). A decoder rebuilds them.
    fn write_duplicates(&mut self, w: &mut Writer<'_>) -> Result<(), FailStop> {
        let Self { duplicates, scratch, .. } = self;
        let mut n = 0;
        duplicates.for_each_slot(|slot, _, _| {
            scratch[n] = (slot as u64, 0);
            n += 1;
        });
        scratch[..n].sort_unstable_by(|a, b| {
            let left = duplicates.at(a.0 as usize).map(|(k, _)| k);
            let right = duplicates.at(b.0 as usize).map(|(k, _)| k);
            left.cmp(&right)
        });
        w.count(n)?;
        for &(slot, _) in scratch[..n].iter() {
            let (key, d) = duplicates.at(slot as usize).ok_or(FailStop::Inconsistent)?;
            w.u64(key.0)?;
            w.bytes(&key.1)?;
            w.u64(d.expires_at)?;
            w.opt_u64(d.order_id)?;
        }
        Ok(())
    }

    /// The command window (E-7.1), ascending by command ID. Sixteen bytes fits the scratch as two numbers, and
    /// big-endian halves sort in the same order as the bytes do.
    fn write_commands(&mut self, w: &mut Writer<'_>) -> Result<(), FailStop> {
        let Self { commands, scratch, .. } = self;
        let mut n = 0;
        commands.for_each(|id, _| {
            let (hi, lo) = id.split_at(8);
            scratch[n] = (
                u64::from_be_bytes(hi.try_into().unwrap_or_default()),
                u64::from_be_bytes(lo.try_into().unwrap_or_default()),
            );
            n += 1;
        });
        scratch[..n].sort_unstable();
        w.count(n)?;
        for &(hi, lo) in scratch[..n].iter() {
            let mut id = [0u8; 16];
            id[..8].copy_from_slice(&hi.to_be_bytes());
            id[8..].copy_from_slice(&lo.to_be_bytes());
            w.bytes(&id)?;
            w.u64(*commands.get(&id).ok_or(FailStop::Inconsistent)?)?;
        }
        Ok(())
    }

    /// The GTD expiries (E-8.1), ascending, and only for orders still resting: the heap keeps records for orders
    /// long gone, and that is residue rather than state (E-20.7).
    fn write_gtd(&mut self, w: &mut Writer<'_>) -> Result<(), FailStop> {
        let Self { gtd, gtd_len, order_index, orders, scratch, .. } = self;
        let mut n = 0;
        for &(at, order) in gtd[..*gtd_len].iter() {
            let live = order_index
                .get(&order)
                .and_then(|&slot| orders.get(slot as usize))
                .is_some_and(|o| o.expires_at == Some(at));
            if live {
                scratch[n] = (at, order);
                n += 1;
            }
        }
        scratch[..n].sort_unstable();
        w.count(n)?;
        for &(at, order) in scratch[..n].iter() {
            w.u64(at)?;
            w.u64(order)?;
        }
        Ok(())
    }

    /// The fee overrides (E-7.10), already kept sorted by `(accountId, priority, overrideId)`, which is the order
    /// E-20.4 asks for. Expired ones are left out: they occupy the pool until it fills, and that is residue.
    fn write_fee_overrides(&mut self, w: &mut Writer<'_>) -> Result<(), FailStop> {
        let now = self.now;
        let live =
            self.fee_overrides[..self.fee_overrides_len].iter().filter(|o| o.expires_at.is_none_or(|at| at > now));
        let count = live.clone().count();
        w.count(count)?;
        for o in live {
            w.u64(o.account_id)?;
            w.u32(o.priority)?;
            w.u64(o.id)?;
            w.u64(o.instrument_id)?;
            w.u64(o.event_id)?;
            w.i32(o.maker_rate_micros)?;
            w.i32(o.taker_rate_micros)?;
            w.opt_u64(o.expires_at)?;
        }
        Ok(())
    }
}
