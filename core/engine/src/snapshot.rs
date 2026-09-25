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

use crate::emit::{AtEventStart, MarketState, Side};
use crate::fixed::FixedMap;
use crate::state::{
    Account, Duplicate, Event, FeeOverride, FeeRates, Holding, Instrument, Level, MakerSetting, Order, OrderGroup,
    RuleLimits, TickRow, TickTable, WindowCounter, NONE,
};
use crate::{Engine, FailStop};
use venue_journal::allocation_policy::AllocationPolicy;
use venue_journal::instrument_class::InstrumentClass;
use venue_journal::self_match_prevention::SelfMatchPrevention;
use venue_journal::time_in_force::TimeInForce;
use venue_journal::venue_mode::VenueMode;

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
        self.write_instruments(&mut w)?;
        self.write_accounts(&mut w)?;
        self.write_orders(&mut w)?;
        self.write_books(&mut w)?;
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

    /// Instruments ascending by ID. Their books follow later, once the orders they name exist: see
    /// [`Self::write_books`].
    fn write_instruments(&mut self, w: &mut Writer<'_>) -> Result<(), FailStop> {
        let Self { instrument_index, instruments, tick_tables, scratch, .. } = self;
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
        }
        Ok(())
    }

    /// Each instrument's book, in the same instrument order: `BUY` before `SELL`, ticks ascending, and the orders
    /// at a tick in queue order, head first — the order in which they would match.
    ///
    /// Written after the orders, not with the instruments, because a book names orders and a decoder reading one
    /// has to have them already. Time priority is the one ordering here that no key can carry (E-20.4).
    fn write_books(&mut self, w: &mut Writer<'_>) -> Result<(), FailStop> {
        let Self { instrument_index, instruments, levels, orders, scratch, .. } = self;
        let mut n = 0;
        instrument_index.for_each(|&id, &slot| {
            scratch[n] = (id, u64::from(slot));
            n += 1;
        });
        scratch[..n].sort_unstable();
        for &(_, slot) in scratch[..n].iter() {
            let inst: &Instrument = instruments.get(slot as usize).ok_or(FailStop::Inconsistent)?;
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
                && a.master_account_id == 0
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
            w.u64(a.master_account_id)?;
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

/// Why a snapshot was refused.
///
/// Refusing is not a fail-stop (E-20.8): nothing has been processed, the engine is untouched, and an operator may
/// try again with a larger engine or an older release. This is deliberately not a `FailStop` — a fail-stop says the
/// venue has reached a state it must not continue from, and declining to read a file is not that.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// The engine has already processed something. A snapshot is restored into a fresh engine, never over the top
    /// of state that is already there.
    NotFresh,
    /// The bytes ran out part-way.
    Truncated,
    /// A value the encoding cannot hold, or a reference to something the snapshot does not contain.
    Malformed,
    /// An encoding this build does not implement (E-20.8). The journal around it is still readable.
    Version(u16),
    /// The contents do not fit this engine's tables.
    TooLarge,
}

/// Reads what [`Writer`] wrote, refusing to run past the end.
struct Reader<'b> {
    buf: &'b [u8],
    at: usize,
}

impl<'b> Reader<'b> {
    fn new(buf: &'b [u8]) -> Self {
        Self { buf, at: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'b [u8], Refused> {
        let end = self.at.checked_add(n).ok_or(Refused::Truncated)?;
        let got = self.buf.get(self.at..end).ok_or(Refused::Truncated)?;
        self.at = end;
        Ok(got)
    }

    fn u8(&mut self) -> Result<u8, Refused> {
        Ok(self.take(1)?[0])
    }

    fn bool(&mut self) -> Result<bool, Refused> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Refused::Malformed),
        }
    }

    fn u16(&mut self) -> Result<u16, Refused> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().map_err(|_| Refused::Truncated)?))
    }

    fn u32(&mut self) -> Result<u32, Refused> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_| Refused::Truncated)?))
    }

    fn u64(&mut self) -> Result<u64, Refused> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| Refused::Truncated)?))
    }

    fn i32(&mut self) -> Result<i32, Refused> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().map_err(|_| Refused::Truncated)?))
    }

    fn i64(&mut self) -> Result<i64, Refused> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().map_err(|_| Refused::Truncated)?))
    }

    fn opt_u64(&mut self) -> Result<Option<u64>, Refused> {
        if self.bool()? {
            Ok(Some(self.u64()?))
        } else {
            Ok(None)
        }
    }

    fn opt_i64(&mut self) -> Result<Option<i64>, Refused> {
        if self.bool()? {
            Ok(Some(self.i64()?))
        } else {
            Ok(None)
        }
    }

    fn opt_u32(&mut self) -> Result<Option<u32>, Refused> {
        if self.bool()? {
            Ok(Some(self.u32()?))
        } else {
            Ok(None)
        }
    }

    fn count(&mut self) -> Result<usize, Refused> {
        Ok(self.u32()? as usize)
    }

    fn limits(&mut self) -> Result<Option<RuleLimits>, Refused> {
        if !self.bool()? {
            return Ok(None);
        }
        Ok(Some(RuleLimits {
            max_resting_orders: self.u32()?,
            max_new_orders_per_second: self.u32()?,
            max_order_to_trade_ratio_milli: self.u32()?,
            ratio_free_orders_per_second: self.u32()?,
        }))
    }
}

/// An enum the encoding must round-trip: an unknown byte is a malformed snapshot rather than a null value, because
/// the engine's own generated codecs answer `NullVal` for anything they do not know and that would restore a venue
/// into a state no command could have put it in.
fn known<T: Copy + PartialEq>(v: T, null: T) -> Result<T, Refused> {
    if v == null {
        Err(Refused::Malformed)
    } else {
        Ok(v)
    }
}

impl Engine<'_> {
    /// Puts this engine into the state a snapshot describes.
    ///
    /// The engine must be fresh. Everything a running engine carries beyond the state — which slot an order sits
    /// in, where a book's levels are in the pool, which way round a hash table happens to be — is rebuilt here from
    /// the contents, so a restored engine's layout is its own and has nothing to do with the layout of the engine
    /// the snapshot was taken from. That is the point: it is what makes a snapshot a description of the venue.
    ///
    /// Derived state is recomputed rather than read (E-20.6): reserved totals, level aggregates, occupancy bitmaps,
    /// resting counts and the carried counts E-6.5 keeps. A snapshot that cannot produce them has a gap in it.
    pub fn restore(&mut self, state_version: u16, bytes: &[u8]) -> Result<(), Refused> {
        if state_version != STATE_VERSION {
            return Err(Refused::Version(state_version));
        }
        if self.last_sequence != 0 || self.now != 0 || self.accounts_used != 0 || self.instruments_used != 0 {
            return Err(Refused::NotFresh);
        }
        let mut r = Reader::new(bytes);
        self.read_scalars(&mut r)?;
        self.read_tick_tables(&mut r)?;
        self.read_events(&mut r)?;
        self.read_releases(&mut r)?;
        self.read_instruments(&mut r)?;
        self.read_accounts(&mut r)?;
        self.read_orders(&mut r)?;
        self.read_books(&mut r)?;
        self.read_holdings(&mut r)?;
        self.read_event_cash(&mut r)?;
        self.read_order_groups(&mut r)?;
        self.read_maker(&mut r)?;
        self.read_duplicates(&mut r)?;
        self.read_commands(&mut r)?;
        self.read_gtd(&mut r)?;
        self.read_fee_overrides(&mut r)?;
        if r.at != bytes.len() {
            return Err(Refused::Malformed);
        }
        Ok(())
    }

    fn read_scalars(&mut self, r: &mut Reader<'_>) -> Result<(), Refused> {
        self.last_sequence = r.u64()?;
        self.now = r.u64()?;
        self.protocol_version = r.u16()?;
        self.venue_mode = known(VenueMode::from(r.u8()?), VenueMode::NullVal)?;
        self.active_order_limit = r.u32()?;
        self.client_order_id_window_seconds = r.u32()?;
        self.settled_retention_seconds = r.u32()?;
        self.max_events = r.u32()?;
        self.max_instruments = r.u32()?;
        self.max_price_levels = r.u32()?;
        self.next_order = r.u64()?;
        self.next_trade = r.u64()?;
        self.unit_serial = r.u64()?;
        self.fee_schedule = FeeRates { maker_micros: r.i32()?, taker_micros: r.i32()? };
        self.default_limits = r.limits()?;
        Ok(())
    }

    fn read_tick_tables(&mut self, r: &mut Reader<'_>) -> Result<(), Refused> {
        for _ in 0..r.count()? {
            let id = r.u32()?;
            let len = r.u16()?;
            let start = self.tick_rows_used;
            let end = start.checked_add(usize::from(len)).ok_or(Refused::Malformed)?;
            if end > self.tick_rows.len() {
                return Err(Refused::TooLarge);
            }
            for row in start..end {
                self.tick_rows[row] = TickRow { numerator: r.u32()?, denominator: r.u32()?, complement: r.u16()? };
            }
            self.tick_rows_used = end;
            let table = TickTable { start: u32::try_from(start).map_err(|_| Refused::TooLarge)?, len };
            self.tick_tables.insert(id, table).map_err(|_| Refused::TooLarge)?;
        }
        Ok(())
    }

    fn read_events(&mut self, r: &mut Reader<'_>) -> Result<(), Refused> {
        for _ in 0..r.count()? {
            let id = r.u64()?;
            let event = Event {
                state_count: r.u8()?,
                scheduled_start: r.u64()?,
                started: r.bool()?,
                suspended: r.bool()?,
                ended: r.bool()?,
                resolved: r.bool()?,
                resolved_at: r.u64()?,
                collateral: r.i64()?,
                next_release: None,
                first_instrument: NONE,
                instrument_count: 0,
            };
            self.events.insert(id, event).map_err(|_| Refused::TooLarge)?;
        }
        Ok(())
    }

    /// The releases, in the order they will happen (E-8.4). The chain is rebuilt in the order it was written
    /// rather than sorted, because that order is the state.
    fn read_releases(&mut self, r: &mut Reader<'_>) -> Result<(), Refused> {
        let mut previous: Option<u64> = None;
        for _ in 0..r.count()? {
            let id = r.u64()?;
            if !self.events.contains(&id) {
                return Err(Refused::Malformed);
            }
            match previous {
                None => self.release_head = Some(id),
                Some(before) => {
                    self.events.get_mut(&before).ok_or(Refused::Malformed)?.next_release = Some(id);
                }
            }
            previous = Some(id);
        }
        self.release_tail = previous;
        Ok(())
    }

    fn read_instruments(&mut self, r: &mut Reader<'_>) -> Result<(), Refused> {
        for _ in 0..r.count()? {
            let id = r.u64()?;
            let event_id = r.u64()?;
            let class = known(InstrumentClass::from(r.u8()?), InstrumentClass::NullVal)?;
            let table_id = r.u32()?;
            let lot_step = r.u32()?;
            let policy = known(AllocationPolicy::from(r.u8()?), AllocationPolicy::NullVal)?;
            let mask = r.u64()?;
            let position_limit = r.u32()?;
            let market_state = known(MarketState::from(r.u8()?), MarketState::NullVal)?;
            let table = *self.tick_tables.get(&table_id).ok_or(Refused::Malformed)?;

            let per_side = usize::from(table.len);
            let words = per_side.div_ceil(64);
            let (levels_start, bits_start) = (self.levels_used, self.level_bits_used);
            if levels_start + 2 * per_side > self.levels.len() || bits_start + 2 * words > self.level_bits.len() {
                return Err(Refused::TooLarge);
            }
            let slot = self.instruments_used;
            if slot >= self.instruments.len() {
                return Err(Refused::TooLarge);
            }
            for l in &mut self.levels[levels_start..levels_start + 2 * per_side] {
                *l = Level { quantity: 0, count: 0, head: NONE, tail: NONE };
            }
            for w in &mut self.level_bits[bits_start..bits_start + 2 * words] {
                *w = 0;
            }
            self.instruments[slot] = Instrument {
                in_use: true,
                id,
                event_id,
                class,
                table,
                lot_step,
                policy,
                mask,
                position_limit,
                market_state,
                next_in_event: NONE,
                levels_start: u32::try_from(levels_start).map_err(|_| Refused::TooLarge)?,
                bits_start: u32::try_from(bits_start).map_err(|_| Refused::TooLarge)?,
            };
            self.instruments_used = slot + 1;
            self.levels_used = levels_start + 2 * per_side;
            self.level_bits_used = bits_start + 2 * words;
            // What E-6.5 counts against the listing limits, recomputed rather than carried (E-20.6).
            self.instruments_carried += 1;
            self.levels_carried += 2 * per_side;
            self.level_bits_carried += 2 * words;
            let slot32 = u32::try_from(slot).map_err(|_| Refused::TooLarge)?;
            self.instrument_index.insert(id, slot32).map_err(|_| Refused::TooLarge)?;

            // Append to the event's chain. Instruments arrive ascending by ID, which is the order the chain is
            // kept in, so appending keeps it without a sort.
            let event = self.events.get_mut(&event_id).ok_or(Refused::Malformed)?;
            event.instrument_count = event.instrument_count.checked_add(1).ok_or(Refused::Malformed)?;
            let mut at = event.first_instrument;
            if at == NONE {
                event.first_instrument = slot32;
            } else {
                while self.instruments[at as usize].next_in_event != NONE {
                    at = self.instruments[at as usize].next_in_event;
                }
                self.instruments[at as usize].next_in_event = slot32;
            }
        }
        Ok(())
    }

    fn read_accounts(&mut self, r: &mut Reader<'_>) -> Result<(), Refused> {
        for _ in 0..r.count()? {
            let id = r.u64()?;
            let slot = self.accounts_used;
            if slot >= self.accounts.len() {
                return Err(Refused::TooLarge);
            }
            self.accounts[slot] = Account {
                id,
                balance: r.i64()?,
                // Recomputed from the orders below (E-20.6).
                reserved: 0,
                closed_only: r.bool()?,
                self_match_prevention: known(SelfMatchPrevention::from(r.u8()?), SelfMatchPrevention::NullVal)?,
                second: r.opt_u64()?,
                new_orders_in_second: r.u32()?,
                fills_in_second: r.u32()?,
                limits: r.limits()?,
                first_order: NONE,
                resting_orders: 0,
                fees_charged: r.i64()?,
                fees_unit: r.u64()?,
                master_account_id: r.u64()?,
            };
            self.accounts_used = slot + 1;
            self.account_index
                .insert(id, u32::try_from(slot).map_err(|_| Refused::TooLarge)?)
                .map_err(|_| Refused::TooLarge)?;
        }
        Ok(())
    }

    /// The orders, each into a slot of this engine's choosing. Their queues are rebuilt from the books that
    /// follow; here they are only made to exist, indexed, and hung on their account's list — which is kept in no
    /// useful order by design, so any order will do.
    fn read_orders(&mut self, r: &mut Reader<'_>) -> Result<(), Refused> {
        for _ in 0..r.count()? {
            let id = r.u64()?;
            let account_id = r.u64()?;
            let session_id = r.u64()?;
            let client_order_id: [u8; 32] = r.take(32)?.try_into().map_err(|_| Refused::Truncated)?;
            let instrument_id = r.u64()?;
            let side = known(Side::from(r.u8()?), Side::NullVal)?;
            let tick = r.u16()?;
            let remaining = r.u32()?;
            let time_in_force = known(TimeInForce::from(r.u8()?), TimeInForce::NullVal)?;
            let expires_at = r.opt_u64()?;
            let at_event_start = known(AtEventStart::from(r.u8()?), AtEventStart::NullVal)?;
            let post_only = r.bool()?;
            let reduce_only = r.bool()?;
            let spend_cap_remaining = r.opt_i64()?;
            let group = r.opt_u32()?;
            let accepted_at = r.u64()?;
            let lock = r.u32()?;
            let reserved = r.i64()?;

            let account_slot = *self.account_index.get(&account_id).ok_or(Refused::Malformed)?;
            let instrument_slot = *self.instrument_index.get(&instrument_id).ok_or(Refused::Malformed)?;
            let slot = self.free_order;
            if slot == NONE {
                return Err(Refused::TooLarge);
            }
            self.free_order = self.orders[slot as usize].account_next;

            let first = self.accounts[account_slot as usize].first_order;
            self.orders[slot as usize] = Order {
                in_use: true,
                id,
                account_id,
                account_slot,
                session_id,
                client_order_id,
                instrument_slot,
                side,
                tick,
                remaining,
                time_in_force,
                expires_at,
                at_event_start,
                post_only,
                reduce_only,
                spend_cap_remaining,
                group,
                accepted_at,
                lock,
                reserved,
                prev: NONE,
                next: NONE,
                account_prev: NONE,
                account_next: first,
            };
            if first != NONE {
                self.orders[first as usize].account_prev = slot;
            }
            let account = &mut self.accounts[account_slot as usize];
            account.first_order = slot;
            account.resting_orders += 1;
            // E-3.1: an account's reserved is always the sum of its orders' (E-20.6).
            account.reserved = account.reserved.checked_add(reserved).ok_or(Refused::Malformed)?;
            self.resting_orders += 1;
            self.order_index.insert(id, slot).map_err(|_| Refused::TooLarge)?;
        }
        Ok(())
    }

    /// The queues, which is where an order's time priority comes back. The level's aggregates and the occupancy
    /// bitmap are counted as the queue is walked rather than read (E-20.6).
    fn read_books(&mut self, r: &mut Reader<'_>) -> Result<(), Refused> {
        for slot in 0..self.instruments_used {
            let inst = self.instruments[slot];
            let ticks = usize::from(inst.table.len);
            let words = inst.words_per_side();
            for side in 0..2usize {
                let base = inst.levels_start as usize + side * ticks;
                for _ in 0..r.count()? {
                    let tick = usize::from(r.u16()?);
                    if tick >= ticks {
                        return Err(Refused::Malformed);
                    }
                    let mut previous = NONE;
                    let (mut quantity, mut count) = (0u64, 0u32);
                    for _ in 0..r.count()? {
                        let id = r.u64()?;
                        let order = *self.order_index.get(&id).ok_or(Refused::Malformed)?;
                        let o = &mut self.orders[order as usize];
                        // A book may only name its own orders, on its own side, at its own tick.
                        if o.instrument_slot as usize != slot
                            || usize::from(o.tick) != tick
                            || (o.side as u8 as usize) != side
                            || o.prev != NONE
                            || o.next != NONE
                        {
                            return Err(Refused::Malformed);
                        }
                        o.prev = previous;
                        quantity = quantity.checked_add(u64::from(o.remaining)).ok_or(Refused::Malformed)?;
                        count = count.checked_add(1).ok_or(Refused::Malformed)?;
                        if previous == NONE {
                            self.levels[base + tick].head = order;
                        } else {
                            self.orders[previous as usize].next = order;
                        }
                        previous = order;
                    }
                    if previous == NONE {
                        // A level was named with nothing in it; the encoder only writes occupied ones.
                        return Err(Refused::Malformed);
                    }
                    let level = &mut self.levels[base + tick];
                    level.tail = previous;
                    level.quantity = quantity;
                    level.count = count;
                    let word = inst.bits_start as usize + side * words + tick / 64;
                    self.level_bits[word] |= 1u64 << (tick % 64);
                }
            }
        }
        Ok(())
    }

    fn read_holdings(&mut self, r: &mut Reader<'_>) -> Result<(), Refused> {
        for _ in 0..r.count()? {
            let key = (r.u64()?, r.u64()?);
            let holding = Holding { position: r.i64()?, yes_locked: r.i64()?, no_locked: r.i64()? };
            self.holdings.insert(key, holding).map_err(|_| Refused::TooLarge)?;
        }
        Ok(())
    }

    fn read_event_cash(&mut self, r: &mut Reader<'_>) -> Result<(), Refused> {
        for _ in 0..r.count()? {
            let key = (r.u64()?, r.u64()?);
            let cents = r.i64()?;
            self.event_cash.insert(key, cents).map_err(|_| Refused::TooLarge)?;
        }
        Ok(())
    }

    fn read_order_groups(&mut self, r: &mut Reader<'_>) -> Result<(), Refused> {
        for _ in 0..r.count()? {
            let key = (r.u64()?, r.u32()?);
            let group = OrderGroup {
                threshold: r.u32()?,
                window_millis: r.u32()?,
                count: r.u64()?,
                window_start: r.opt_u64()?,
                triggered: r.bool()?,
            };
            self.order_groups.insert(key, group).map_err(|_| Refused::TooLarge)?;
        }
        Ok(())
    }

    fn read_maker(&mut self, r: &mut Reader<'_>) -> Result<(), Refused> {
        for _ in 0..r.count()? {
            let key = (r.u64()?, r.u64()?);
            let setting = MakerSetting { threshold: r.u32()?, window_millis: r.u32()? };
            self.maker_settings.insert(key, setting).map_err(|_| Refused::TooLarge)?;
        }
        for _ in 0..r.count()? {
            let key = (r.u64()?, r.u64()?);
            let counter = WindowCounter { count: r.u64()?, window_start: r.opt_u64()? };
            self.maker_counters.insert(key, counter).map_err(|_| Refused::TooLarge)?;
        }
        Ok(())
    }

    /// The client-order-ID window, and the expiry records that index it rebuilt from it.
    ///
    /// The records were not written: they carry entries that have already gone, which is the engine's residue and
    /// not the venue's state (E-20.7). Rebuilt they are sorted by expiry, which is what the queue form means, so a
    /// restored engine is in queue mode with nothing stranded — `expiry_heap_until` is `None` by construction.
    fn read_duplicates(&mut self, r: &mut Reader<'_>) -> Result<(), Refused> {
        let n = r.count()?;
        if n > self.expiry.len() {
            return Err(Refused::TooLarge);
        }
        for i in 0..n {
            let account = r.u64()?;
            let client_order_id: [u8; 32] = r.take(32)?.try_into().map_err(|_| Refused::Truncated)?;
            let expires_at = r.u64()?;
            let order_id = r.opt_u64()?;
            let key = (account, client_order_id);
            self.duplicates.insert(key, Duplicate { expires_at, order_id }).map_err(|_| Refused::TooLarge)?;
            self.expiry[i] = (expires_at, key);
            self.expiry_latest = self.expiry_latest.max(expires_at);
        }
        self.expiry[..n].sort_unstable();
        self.expiry_head = 0;
        self.expiry_len = n;
        self.expiry_heap_until = None;
        Ok(())
    }

    fn read_commands(&mut self, r: &mut Reader<'_>) -> Result<(), Refused> {
        let n = r.count()?;
        if n > self.command_expiry.len() {
            return Err(Refused::TooLarge);
        }
        for i in 0..n {
            let id: [u8; 16] = r.take(16)?.try_into().map_err(|_| Refused::Truncated)?;
            let expires_at = r.u64()?;
            self.commands.insert(id, expires_at).map_err(|_| Refused::TooLarge)?;
            self.command_expiry[i] = (expires_at, id);
        }
        self.command_expiry[..n].sort_unstable();
        self.command_expiry_head = 0;
        self.command_expiry_len = n;
        Ok(())
    }

    /// The GTD expiries. Sorted is already a min-heap, which is the form the engine keeps them in.
    fn read_gtd(&mut self, r: &mut Reader<'_>) -> Result<(), Refused> {
        let n = r.count()?;
        if n > self.gtd.len() {
            return Err(Refused::TooLarge);
        }
        for i in 0..n {
            self.gtd[i] = (r.u64()?, r.u64()?);
        }
        self.gtd[..n].sort_unstable();
        self.gtd_len = n;
        Ok(())
    }

    fn read_fee_overrides(&mut self, r: &mut Reader<'_>) -> Result<(), Refused> {
        let n = r.count()?;
        if n > self.fee_overrides.len() {
            return Err(Refused::TooLarge);
        }
        for i in 0..n {
            self.fee_overrides[i] = FeeOverride {
                account_id: r.u64()?,
                priority: r.u32()?,
                id: r.u64()?,
                instrument_id: r.u64()?,
                event_id: r.u64()?,
                maker_rate_micros: r.i32()?,
                taker_rate_micros: r.i32()?,
                expires_at: r.opt_u64()?,
            };
        }
        self.fee_overrides_len = n;
        Ok(())
    }
}
