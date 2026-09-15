//! Client messages (spec §9, §12, §13) and the walk that matches them (§10).
//!
//! Every client message is one unit, or one unit per entry of `BatchOrders` (E-4.4). Checks come before any
//! write, so a rejected unit changes only what E-5.3 allows. The dry run of E-10.9 is the real walk with the
//! dry-run flag set: writes are logged and rolled back, and nothing is emitted, touched or marked.

use crate::book::{crosses, floor_to, opposite};
use crate::commands::header;
use crate::emit::{AtEventStart, CancelReason, ClientOrderId, MarketState, Out, RejectReason, ReleaseKind, Side};
use crate::state::*;
use crate::{Engine, FailStop, ResultSink};
use venue_journal::allocation_policy::AllocationPolicy;
use venue_journal::batch_entry_type::BatchEntryType;
use venue_journal::contract::Contract;
use venue_journal::mass_cancel_cause::MassCancelCause;
use venue_journal::mass_cancel_scope::MassCancelScope;
use venue_journal::self_match_prevention::SelfMatchPrevention;
use venue_journal::time_in_force::TimeInForce;
use venue_journal::venue_mode::VenueMode;
use venue_journal::{
    amend_order_codec, basket_mint_redeem_codec, batch_orders_codec, cancel_order_codec, mass_cancel_codec,
    new_order_codec, reset_order_group_codec, set_order_group_codec,
};

pub(crate) const NS_PER_SECOND: u64 = 1_000_000_000;
const NS_PER_MILLI: u64 = 1_000_000;
pub(crate) const NO_CLIENT_ORDER_ID: ClientOrderId = [0; 32];
const EXTRA_LOT: u64 = 1 << 63;

/// `(slot, tick, quantity, priorityKept)` for an amendment that passes its checks.
type AmendPlan = (u32, u16, u32, bool);

/// A `NewOrder`, or a `NEW` entry of `BatchOrders` with the batch's session and account (E-12.8).
#[derive(Clone, Copy)]
struct NewOrderFields {
    session_id: u64,
    account_id: u64,
    client_order_id: ClientOrderId,
    instrument_id: u64,
    side: Side,
    entered: Contract,
    tick: u16,
    quantity: u32,
    time_in_force: TimeInForce,
    expires_at: Option<u64>,
    at_event_start: AtEventStart,
    post_only: bool,
    reduce_only: bool,
    protection_ticks: u8,
    spend_cap: Option<i64>,
    group: Option<u32>,
}

macro_rules! new_order_fields {
    ($decoder:expr, $session:expr, $account:expr) => {{
        let d = &$decoder;
        let flags = d.flags();
        NewOrderFields {
            session_id: $session,
            account_id: $account,
            client_order_id: d.client_order_id(),
            instrument_id: d.instrument_id(),
            side: d.side(),
            entered: d.entered_contract(),
            tick: d.tick(),
            quantity: d.quantity(),
            time_in_force: d.time_in_force(),
            expires_at: d.expires_at(),
            at_event_start: d.at_event_start(),
            post_only: flags.get_post_only(),
            reduce_only: flags.get_reduce_only(),
            protection_ticks: d.protection_ticks(),
            spend_cap: d.spend_cap_cents(),
            group: d.client_order_group(),
        }
    }};
}

/// `(accountId, sessionId, clientOrderId, instrumentId)` for a client message's rejection, each 0 where the
/// message has no such field.
fn client_identity(template: u16, input: &[u8]) -> (u64, u64, ClientOrderId, u64) {
    let h = header(input);
    match template {
        new_order_codec::SBE_TEMPLATE_ID => {
            let d = new_order_codec::NewOrderDecoder::default().header(h, 0);
            (d.account_id(), d.session_id(), d.client_order_id(), d.instrument_id())
        }
        cancel_order_codec::SBE_TEMPLATE_ID => {
            let d = cancel_order_codec::CancelOrderDecoder::default().header(h, 0);
            (d.account_id(), d.session_id(), d.client_order_id(), d.instrument_id())
        }
        amend_order_codec::SBE_TEMPLATE_ID => {
            let d = amend_order_codec::AmendOrderDecoder::default().header(h, 0);
            (d.account_id(), d.session_id(), NO_CLIENT_ORDER_ID, d.instrument_id())
        }
        mass_cancel_codec::SBE_TEMPLATE_ID => {
            let d = mass_cancel_codec::MassCancelDecoder::default().header(h, 0);
            (d.account_id(), d.session_id(), NO_CLIENT_ORDER_ID, 0)
        }
        batch_orders_codec::SBE_TEMPLATE_ID => {
            let d = batch_orders_codec::BatchOrdersDecoder::default().header(h, 0);
            (d.account_id(), d.session_id(), NO_CLIENT_ORDER_ID, 0)
        }
        set_order_group_codec::SBE_TEMPLATE_ID => {
            let d = set_order_group_codec::SetOrderGroupDecoder::default().header(h, 0);
            (d.account_id(), d.session_id(), NO_CLIENT_ORDER_ID, 0)
        }
        reset_order_group_codec::SBE_TEMPLATE_ID => {
            let d = reset_order_group_codec::ResetOrderGroupDecoder::default().header(h, 0);
            (d.account_id(), d.session_id(), NO_CLIENT_ORDER_ID, 0)
        }
        basket_mint_redeem_codec::SBE_TEMPLATE_ID => {
            let d = basket_mint_redeem_codec::BasketMintRedeemDecoder::default().header(h, 0);
            (d.account_id(), d.session_id(), d.client_order_id(), 0)
        }
        _ => (0, 0, NO_CLIENT_ORDER_ID, 0),
    }
}

/// E-2.8: `(sequencerGroup << 48) + n`, advancing the counter.
fn next_id(group: u16, counter: &mut u64) -> Result<u64, FailStop> {
    let n = *counter;
    *counter = n.checked_add(1).ok_or(FailStop::Overflow)?;
    (u64::from(group) << 48).checked_add(n).ok_or(FailStop::Overflow)
}

/// The window rule of E-13.2 and E-13.3: restart an elapsed or unstarted window, then count `quantity`.
fn window_count(now: u64, c: &mut WindowCounter, window_millis: u32, quantity: u32) -> Result<u64, FailStop> {
    let elapsed = c.window_start.is_none_or(|start| now - start >= u64::from(window_millis) * NS_PER_MILLI);
    if elapsed {
        *c = WindowCounter { count: 0, window_start: Some(now) };
    }
    c.count = c.count.checked_add(u64::from(quantity)).ok_or(FailStop::Overflow)?;
    Ok(c.count)
}

const fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

impl<'a> Engine<'a> {
    pub(crate) fn on_client<S: ResultSink>(
        &mut self,
        template: u16,
        foreign: bool,
        input: &[u8],
        sink: &mut S,
    ) -> Result<(), FailStop> {
        if foreign {
            // E-4.3
            let (account_id, session_id, client_order_id, instrument_id) = client_identity(template, input);
            let reject = Out::OrderRejected {
                account_id,
                session_id,
                client_order_id,
                instrument_id,
                reason: RejectReason::MALFORMED,
                original_order_id: None,
            };
            self.out(sink, &reject)?;
            return self.end_unit(sink);
        }
        match template {
            new_order_codec::SBE_TEMPLATE_ID => {
                let d = new_order_codec::NewOrderDecoder::default().header(header(input), 0);
                let fields = new_order_fields!(d, d.session_id(), d.account_id());
                self.new_order(&fields, sink)?;
                self.finish_order_unit(sink)
            }
            cancel_order_codec::SBE_TEMPLATE_ID => {
                let d = cancel_order_codec::CancelOrderDecoder::default().header(header(input), 0);
                let (session_id, account_id, instrument_id) = (d.session_id(), d.account_id(), d.instrument_id());
                self.cancel(session_id, account_id, instrument_id, d.order_id(), d.client_order_id(), sink)?;
                self.end_unit(sink)
            }
            amend_order_codec::SBE_TEMPLATE_ID => self.amend(input, sink),
            mass_cancel_codec::SBE_TEMPLATE_ID => self.mass_cancel(input, sink),
            batch_orders_codec::SBE_TEMPLATE_ID => self.batch(input, sink),
            set_order_group_codec::SBE_TEMPLATE_ID | reset_order_group_codec::SBE_TEMPLATE_ID => {
                self.order_group(template, input, sink)
            }
            basket_mint_redeem_codec::SBE_TEMPLATE_ID => self.basket(input, sink),
            _ => Err(FailStop::MalformedCommand),
        }
    }

    // ------------------------------------------------------------------ helpers

    pub(crate) fn out<S: ResultSink>(&mut self, sink: &mut S, out: &Out) -> Result<(), FailStop> {
        if self.dry {
            Ok(())
        } else {
            self.emitter.emit(sink, out)
        }
    }

    fn mark(&mut self, mark: Mark) -> Result<(), FailStop> {
        if self.dry {
            return Ok(());
        }
        let slot = self.marks.get_mut(self.marks_len).ok_or(FailStop::Capacity)?;
        *slot = mark;
        self.marks_len += 1;
        Ok(())
    }

    fn event_suspended(&self, event_id: u64) -> bool {
        self.events.get(&event_id).is_some_and(|e| e.suspended)
    }

    fn limits_for(&self, account_slot: u32) -> RuleLimits {
        self.accounts[account_slot as usize].limits.or(self.default_limits).unwrap_or_default()
    }

    /// Starts the account's counters afresh in a new second of sequencer time (E-9.4).
    fn roll_second(&mut self, account_slot: u32) -> Result<(), FailStop> {
        let second = self.now / NS_PER_SECOND;
        if self.accounts[account_slot as usize].second != Some(second) {
            let a = self.account_w(account_slot)?;
            a.second = Some(second);
            a.new_orders_in_second = 0;
            a.fills_in_second = 0;
        }
        Ok(())
    }

    /// E-9.7, leaving out the order in slot `exclude` (`NONE` for none).
    pub(crate) fn position_limit_breached(
        &self,
        account_slot: u32,
        instrument_slot: u32,
        side: Side,
        quantity: u32,
        exclude: u32,
    ) -> bool {
        let inst = &self.instruments[instrument_slot as usize];
        if inst.position_limit == 0 {
            return false;
        }
        let account = &self.accounts[account_slot as usize];
        let mut resting = 0i64;
        let mut cur = account.first_order;
        while cur != NONE {
            let o = &self.orders[cur as usize];
            if cur != exclude && o.instrument_slot == instrument_slot && o.side == side {
                resting += i64::from(o.remaining);
            }
            cur = o.account_next;
        }
        let position = self.holding(account.id, inst.id).position;
        let signed = if side == Side::BUY { position } else { -position };
        signed + resting + i64::from(quantity) > i64::from(inst.position_limit)
    }

    pub(crate) fn duplicate_window(&self) -> u64 {
        u64::from(self.client_order_id_window_seconds) * NS_PER_SECOND
    }

    /// E-12.1: removes an order that rests or is being processed, releasing its lock and reservation, and
    /// emits `OrderCancelled`. The slot is freed.
    pub(crate) fn remove<S: ResultSink>(
        &mut self,
        slot: u32,
        reason: CancelReason,
        sink: &mut S,
    ) -> Result<(), FailStop> {
        let o = self.orders[slot as usize];
        if o.in_use {
            self.unlink(slot)?;
        }
        self.release(slot)?;
        let cancelled = Out::OrderCancelled {
            order_id: o.id,
            account_id: o.account_id,
            instrument_id: self.instruments[o.instrument_slot as usize].id,
            cancelled_quantity: o.remaining,
            reason,
        };
        self.out(sink, &cancelled)?;
        self.free_slot(slot)
    }

    /// Removes every resting order passing `keep`, in ascending order ID.
    pub(crate) fn remove_where<S: ResultSink>(
        &mut self,
        keep: impl FnMut(&Order, &Instrument) -> bool,
        reason: CancelReason,
        sink: &mut S,
    ) -> Result<(), FailStop> {
        let n = self.collect(keep, |o| o.id);
        for i in 0..n {
            let id = self.scratch[i].1;
            let slot = *self.order_index.get(&id).ok_or(FailStop::Inconsistent)?;
            self.remove(slot, reason, sink)?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------ new orders (§9)

    fn new_order<S: ResultSink>(&mut self, f: &NewOrderFields, sink: &mut S) -> Result<(), FailStop> {
        let reject = |reason, original_order_id| Out::OrderRejected {
            account_id: f.account_id,
            session_id: f.session_id,
            client_order_id: f.client_order_id,
            instrument_id: f.instrument_id,
            reason,
            original_order_id,
        };
        // Check 1, with E-4.3 for undefined enum values.
        let defined = f.side != Side::NullVal
            && f.entered != Contract::NullVal
            && f.time_in_force != TimeInForce::NullVal
            && f.at_event_start != AtEventStart::NullVal;
        let purchase = matches!((f.side, f.entered), (Side::BUY, Contract::YES) | (Side::SELL, Contract::NO));
        let malformed = !defined
            || f.account_id == 0
            || f.client_order_id == NO_CLIENT_ORDER_ID
            || f.quantity == 0
            || (f.time_in_force == TimeInForce::GTD) != f.expires_at.is_some()
            || (f.post_only && matches!(f.time_in_force, TimeInForce::IOC | TimeInForce::FOK))
            || f.spend_cap.is_some_and(|cap| cap <= 0 || !purchase);
        if malformed {
            return self.out(sink, &reject(RejectReason::MALFORMED, None));
        }
        // Check 2 (E-9.2).
        let key = (f.account_id, f.client_order_id);
        if let Some(entry) = self.duplicates.get(&key).copied() {
            if self.now < entry.expires_at {
                return self.out(sink, &reject(RejectReason::DUPLICATE_CLIENT_ORDER_ID, entry.order_id));
            }
        }
        let expires_at = self.now.checked_add(self.duplicate_window()).ok_or(FailStop::Overflow)?;
        let entry = Duplicate { expires_at, order_id: None };
        self.duplicates.insert(key, entry).map_err(|_| FailStop::Capacity)?;
        match self.check_and_build(f, sink)? {
            Err(reason) => self.out(sink, &reject(reason, None)),
            Ok(slot) => self.accept_and_match(slot, sink),
        }
    }

    /// E-9.3 checks 3 to 18. On success the order is built in a slab slot, not yet accepted.
    fn check_and_build<S: ResultSink>(
        &mut self,
        f: &NewOrderFields,
        sink: &mut S,
    ) -> Result<Result<u32, RejectReason>, FailStop> {
        use RejectReason as R;
        let account_slot = self.account_slot(f.account_id)?;
        if self.venue_mode != VenueMode::NORMAL {
            return Ok(Err(R::VENUE_MODE));
        }
        let Some(&instrument_slot) = self.instrument_index.get(&f.instrument_id) else {
            return Ok(Err(R::UNKNOWN_INSTRUMENT));
        };
        let inst = self.instruments[instrument_slot as usize];
        if inst.market_state != MarketState::OPEN || self.event_suspended(inst.event_id) {
            return Ok(Err(R::MARKET_NOT_OPEN));
        }
        if f.tick >= inst.table.len {
            return Ok(Err(R::TICK_NOT_ON_LADDER));
        }
        if !f.quantity.is_multiple_of(inst.lot_step) {
            return Ok(Err(R::QUANTITY_NOT_ON_LOT_STEP));
        }
        if f.time_in_force == TimeInForce::GTD && f.expires_at.is_some_and(|at| at <= self.now) {
            return Ok(Err(R::EXPIRY_IN_PAST));
        }
        if let Some(group) = f.group {
            if self.order_groups.get(&(f.account_id, group)).is_some_and(|g| g.triggered) {
                return Ok(Err(R::ORDER_GROUP_TRIGGERED));
            }
        }
        // Check 10 (E-9.4).
        let limits = self.limits_for(account_slot);
        self.roll_second(account_slot)?;
        let account = self.accounts[account_slot as usize];
        let (new_orders, fills) = (account.new_orders_in_second, account.fills_in_second);
        if limits.max_new_orders_per_second > 0 && new_orders >= limits.max_new_orders_per_second {
            return Ok(Err(R::RULE_LIMIT));
        }
        let (ratio, free) = (limits.max_order_to_trade_ratio_milli, limits.ratio_free_orders_per_second);
        if ratio > 0
            && new_orders >= free
            && u64::from(new_orders - free + 1) * 1000 > u64::from(ratio) * u64::from(fills)
        {
            return Ok(Err(R::RULE_LIMIT));
        }
        self.account_w(account_slot)?.new_orders_in_second = new_orders.checked_add(1).ok_or(FailStop::Overflow)?;
        // Check 11 (E-9.5).
        let lot = u64::from(inst.lot_step);
        let unlocked = self.unlocked_holding(f.account_id, inst.id, f.side).max(0) as u64;
        let mut quantity = f.quantity;
        if f.reduce_only || account.closed_only {
            quantity = floor_to(u64::from(quantity).min(unlocked), lot) as u32;
            if quantity == 0 {
                return Ok(Err(if f.reduce_only { R::REDUCE_ONLY_NO_POSITION } else { R::CLOSED_ONLY_MODE }));
            }
        }
        // Check 12 (E-9.6).
        let opposite_best = self.best(instrument_slot, opposite(f.side));
        let mut limit = f.tick;
        if let (true, Some(b)) = (f.protection_ticks > 0, opposite_best) {
            let n = u16::from(f.protection_ticks);
            limit = if f.side == Side::BUY { f.tick.min(b.saturating_add(n)) } else { f.tick.max(b.saturating_sub(n)) };
        }
        if let Some(cap) = f.spend_cap {
            if cap < self.unit_cost(instrument_slot, f.side, limit, lot)? {
                return Ok(Err(R::SPEND_CAP_BELOW_ONE_CONTRACT));
            }
        }
        // Check 13.
        if f.post_only && opposite_best.is_some_and(|b| crosses(f.side, b, limit)) {
            return Ok(Err(R::POST_ONLY_WOULD_TAKE));
        }
        // E-11.1: the lock the order will take.
        let lock = floor_to(u64::from(quantity).min(unlocked), lot) as u32;
        let slot = self.alloc_order()?;
        *self.order_w(slot)? = Order {
            in_use: false,
            id: 0,
            account_id: f.account_id,
            account_slot,
            session_id: f.session_id,
            client_order_id: f.client_order_id,
            instrument_slot,
            side: f.side,
            tick: limit,
            remaining: quantity,
            time_in_force: f.time_in_force,
            expires_at: f.expires_at,
            at_event_start: f.at_event_start,
            post_only: f.post_only,
            reduce_only: f.reduce_only || account.closed_only,
            spend_cap_remaining: f.spend_cap,
            group: f.group,
            accepted_at: self.last_sequence,
            lock,
            reserved: 0,
            prev: NONE,
            next: NONE,
            account_prev: NONE,
            account_next: NONE,
        };
        match self.checks_after_build(slot, &limits, sink)? {
            None => Ok(Ok(slot)),
            Some(reason) => {
                self.free_slot(slot)?;
                Ok(Err(reason))
            }
        }
    }

    /// E-9.3 checks 14 to 18, which need the order built.
    fn checks_after_build<S: ResultSink>(
        &mut self,
        slot: u32,
        limits: &RuleLimits,
        sink: &mut S,
    ) -> Result<Option<RejectReason>, FailStop> {
        use RejectReason as R;
        let o = self.orders[slot as usize];
        let tif = o.time_in_force;
        let may_rest = matches!(tif, TimeInForce::GTC | TimeInForce::GTD);
        // The dry run is evaluated only where its answer can change the outcome.
        let needs_dry_run =
            tif == TimeInForce::FOK || (may_rest && (limits.max_resting_orders > 0 || self.active_order_limit > 0));
        let filled = if needs_dry_run { self.dry_run(slot, sink)? } else { 0 };
        if tif == TimeInForce::FOK && filled < o.remaining {
            return Ok(Some(R::FOK_NOT_FILLABLE));
        }
        if self.position_limit_breached(o.account_slot, o.instrument_slot, o.side, o.remaining, NONE) {
            return Ok(Some(R::POSITION_LIMIT));
        }
        if may_rest && needs_dry_run && filled < o.remaining {
            let resting = self.accounts[o.account_slot as usize].resting_orders;
            if limits.max_resting_orders > 0 && resting >= limits.max_resting_orders {
                return Ok(Some(R::RULE_LIMIT));
            }
            if self.active_order_limit > 0 && self.resting_orders >= self.active_order_limit {
                return Ok(Some(R::ORDER_CAPACITY));
            }
        }
        let account = self.accounts[o.account_slot as usize];
        // E-3.3: an account below zero can spend nothing.
        if self.reservation_of(&o)? > (account.balance - account.reserved).max(0) {
            return Ok(Some(R::INSUFFICIENT_SLEEVE));
        }
        Ok(None)
    }

    /// E-10.9: the quantity the walk would fill for the order in `slot`, with its lock taken.
    fn dry_run<S: ResultSink>(&mut self, slot: u32, sink: &mut S) -> Result<u32, FailStop> {
        let o = self.orders[slot as usize];
        let instrument_id = self.instruments[o.instrument_slot as usize].id;
        self.begin_dry_run()?;
        let walked = match self.add_locked(o.account_id, instrument_id, o.side, i64::from(o.lock)) {
            Ok(()) => self.walk(slot, sink).map(|_| self.orders[slot as usize].remaining),
            Err(stop) => Err(stop),
        };
        self.rollback()?;
        Ok(o.remaining - walked?)
    }

    /// E-9.8.
    fn accept_and_match<S: ResultSink>(&mut self, slot: u32, sink: &mut S) -> Result<(), FailStop> {
        let group = self.config.sequencer_group;
        let id = next_id(group, &mut self.next_order)?;
        let o = self.orders[slot as usize];
        let instrument_id = self.instruments[o.instrument_slot as usize].id;
        self.order_w(slot)?.id = id;
        self.add_locked(o.account_id, instrument_id, o.side, i64::from(o.lock))?;
        self.refresh_reserved(slot)?;
        if let Some(entry) = self.duplicates.get_mut(&(o.account_id, o.client_order_id)) {
            entry.order_id = Some(id);
        }
        let accepted = Out::OrderAccepted {
            order_id: id,
            account_id: o.account_id,
            session_id: o.session_id,
            client_order_id: o.client_order_id,
            instrument_id,
            side: o.side,
            tick: o.tick,
            quantity: o.remaining,
            reserved_cents: self.orders[slot as usize].reserved,
        };
        self.out(sink, &accepted)?;
        self.match_and_place(slot, sink)
    }

    /// The walk, then E-9.9.
    fn match_and_place<S: ResultSink>(&mut self, slot: u32, sink: &mut S) -> Result<(), FailStop> {
        let self_match = self.walk(slot, sink)?;
        let o = self.orders[slot as usize];
        if o.remaining == 0 {
            self.release(slot)?;
            return self.free_slot(slot);
        }
        if self_match {
            return self.remove(slot, CancelReason::SELF_MATCH_PREVENTED, sink);
        }
        match o.time_in_force {
            // A passing dry run means a FOK order cannot have a remainder.
            TimeInForce::FOK => return Err(FailStop::Inconsistent),
            TimeInForce::IOC => return self.remove(slot, CancelReason::IOC_REMAINDER, sink),
            _ => {}
        }
        if let Some(cap) = o.spend_cap_remaining {
            let afford = self.affordable(o.instrument_slot, o.side, o.tick, cap)?.min(u64::from(o.remaining)) as u32;
            if afford == 0 {
                return self.remove(slot, CancelReason::SPEND_CAP_REACHED, sink);
            }
            if afford < o.remaining {
                self.set_remaining(slot, afford)?;
                if o.lock > afford {
                    self.set_lock(slot, afford)?;
                }
                let reduced = Out::OrderReduced {
                    order_id: o.id,
                    account_id: o.account_id,
                    instrument_id: self.instruments[o.instrument_slot as usize].id,
                    remaining_quantity: afford,
                };
                self.out(sink, &reduced)?;
            }
        }
        self.refresh_reserved(slot)?;
        self.rest(slot)
    }

    // ------------------------------------------------------------------ the walk (§10)

    /// E-10.3 to E-10.8 for the order in `slot`. Returns whether self-match prevention stopped it.
    fn walk<S: ResultSink>(&mut self, slot: u32, sink: &mut S) -> Result<bool, FailStop> {
        loop {
            let o = self.orders[slot as usize];
            if o.remaining == 0 {
                return Ok(false);
            }
            let Some(tick) = self.next_crossing(o.instrument_slot, o.side, o.tick) else { return Ok(false) };
            let inst = self.instruments[o.instrument_slot as usize];
            let lot = u64::from(inst.lot_step);
            let mut q = u64::from(o.remaining);
            if let Some(cap) = o.spend_cap_remaining {
                q = q.min(self.affordable(o.instrument_slot, o.side, tick, cap)?);
            }
            let q = floor_to(q, lot) as u32;
            if q == 0 {
                return Ok(false);
            }
            let stopped = if inst.policy == AllocationPolicy::PRO_RATA {
                self.pro_rata(slot, tick, q, sink)?
            } else {
                self.price_time(slot, tick, q, sink)?
            };
            if stopped {
                return Ok(true);
            }
        }
    }

    /// E-10.4.
    fn price_time<S: ResultSink>(&mut self, slot: u32, tick: u16, q: u32, sink: &mut S) -> Result<bool, FailStop> {
        let taker = self.orders[slot as usize];
        let mode = self.accounts[taker.account_slot as usize].self_match_prevention;
        let mut left = q;
        let mut cur = self.level(taker.instrument_slot, opposite(taker.side), tick).head;
        while cur != NONE && left > 0 {
            let r = self.orders[cur as usize];
            let next = r.next;
            if !r.in_use {
                // Removed by lock restoration earlier in the walk.
                cur = next;
                continue;
            }
            if r.account_id == taker.account_id {
                // E-13.1
                match mode {
                    SelfMatchPrevention::CANCEL_RESTING => {
                        self.remove(cur, CancelReason::SELF_MATCH_PREVENTED, sink)?;
                        cur = next;
                        continue;
                    }
                    SelfMatchPrevention::CANCEL_BOTH => {
                        self.remove(cur, CancelReason::SELF_MATCH_PREVENTED, sink)?;
                        return Ok(true);
                    }
                    _ => return Ok(true),
                }
            }
            let f = left.min(r.remaining);
            self.fill(slot, cur, tick, f, sink)?;
            left -= f;
            cur = next;
        }
        Ok(false)
    }

    /// E-10.5.
    fn pro_rata<S: ResultSink>(&mut self, slot: u32, tick: u16, q: u32, sink: &mut S) -> Result<bool, FailStop> {
        let taker = self.orders[slot as usize];
        let (inst_slot, maker_side) = (taker.instrument_slot, opposite(taker.side));
        let lot = self.instruments[inst_slot as usize].lot_step;
        let mode = self.accounts[taker.account_slot as usize].self_match_prevention;
        // E-13.1: the account's own orders at the tick, together, in ascending order ID.
        let mut own = 0;
        let mut cur = self.level(inst_slot, maker_side, tick).head;
        while cur != NONE {
            let r = self.orders[cur as usize];
            if r.account_id == taker.account_id {
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
                self.remove(s, CancelReason::SELF_MATCH_PREVENTED, sink)?;
            }
            if mode == SelfMatchPrevention::CANCEL_BOTH {
                return Ok(true);
            }
        }
        // The other orders, oldest first, as (lots, slot).
        let (mut n, mut total) = (0, 0u64);
        cur = self.level(inst_slot, maker_side, tick).head;
        while cur != NONE {
            let r = self.orders[cur as usize];
            let lots = u64::from(r.remaining / lot);
            self.scratch[n] = (lots, u64::from(cur));
            total += lots;
            n += 1;
            cur = r.next;
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
            let r = self.orders[maker as usize];
            // Lock restoration may have shrunk or removed an order during the allocation.
            if lots > 0 && r.in_use && r.remaining > 0 {
                let f = (lots * u64::from(lot)).min(u64::from(r.remaining)) as u32;
                self.fill(slot, maker, tick, f, sink)?;
            }
        }
        Ok(false)
    }

    /// E-10.6 and E-10.7.
    fn fill<S: ResultSink>(
        &mut self,
        taker_slot: u32,
        maker_slot: u32,
        tick: u16,
        quantity: u32,
        sink: &mut S,
    ) -> Result<(), FailStop> {
        let (taker, maker) = (self.orders[taker_slot as usize], self.orders[maker_slot as usize]);
        let inst_slot = taker.instrument_slot;
        let inst = self.instruments[inst_slot as usize];
        let (buyer, seller) = if taker.side == Side::BUY { (taker, maker) } else { (maker, taker) };
        let pb = self.holding(buyer.account_id, inst.id).position;
        let ps = self.holding(seller.account_id, inst.id).position;
        let q = i64::from(quantity);
        let yes = self.yes_cost(inst_slot, tick, u64::from(quantity))?;
        let b_burn = q.min(pb.checked_neg().ok_or(FailStop::Overflow)?.max(0));
        let s_reduce = q.min(ps.max(0));
        let burned = b_burn.min(s_reduce);
        let minted = q - b_burn.max(s_reduce);
        let buyer_cash = 100 * b_burn - yes;
        let seller_cash = yes - 100 * (q - s_reduce);
        self.set_position(buyer.account_id, inst.id, pb.checked_add(q).ok_or(FailStop::Overflow)?)?;
        self.set_position(seller.account_id, inst.id, ps.checked_sub(q).ok_or(FailStop::Overflow)?)?;
        self.add_event_cash(buyer.account_slot, inst.event_id, buyer_cash)?;
        self.add_event_cash(seller.account_slot, inst.event_id, seller_cash)?;
        self.add_collateral(inst.event_id, 100 * (minted - burned))?;
        // E-11.3
        for x in [taker_slot, maker_slot] {
            let o = self.orders[x as usize];
            let used = quantity.min(o.lock);
            if used > 0 {
                self.add_locked(o.account_id, inst.id, o.side, -i64::from(used))?;
            }
            let cap = match o.spend_cap_remaining {
                Some(cap) => Some(cap - self.unit_cost(inst_slot, o.side, tick, u64::from(quantity))?),
                None => None,
            };
            self.set_remaining(x, o.remaining - quantity)?;
            let w = self.order_w(x)?;
            w.lock = o.lock - used;
            w.spend_cap_remaining = cap;
        }
        let group = self.config.sequencer_group;
        let trade_id = next_id(group, &mut self.next_trade)?;
        let (t, m) = (self.orders[taker_slot as usize], self.orders[maker_slot as usize]);
        let fill = Out::Fill {
            trade_id,
            instrument_id: inst.id,
            tick,
            quantity,
            maker_order_id: m.id,
            maker_account_id: m.account_id,
            maker_remaining: m.remaining,
            taker_order_id: t.id,
            taker_account_id: t.account_id,
            taker_remaining: t.remaining,
            taker_side: t.side,
            minted_pairs: minted as u32,
            burned_pairs: burned as u32,
            buyer_cash_cents: buyer_cash,
            seller_cash_cents: seller_cash,
        };
        self.out(sink, &fill)?;
        // E-11.4, for both accounts in ascending order.
        let (first, second) = if buyer.account_id <= seller.account_id { (buyer, seller) } else { (seller, buyer) };
        self.restore_locks(first.account_slot, inst_slot, sink)?;
        if second.account_id != first.account_id {
            self.restore_locks(second.account_slot, inst_slot, sink)?;
        }
        // A resting order with nothing remaining leaves the book. Lock restoration may already have removed it.
        let maker_alive = self.orders[maker_slot as usize].id == maker.id;
        let m = self.orders[maker_slot as usize];
        let maker_done = maker_alive && m.remaining == 0 && m.in_use;
        if maker_done {
            self.unlink(maker_slot)?;
            self.release(maker_slot)?;
        }
        for account_slot in [buyer.account_slot, seller.account_slot] {
            self.roll_second(account_slot)?;
            let a = self.account_w(account_slot)?;
            a.fills_in_second = a.fills_in_second.checked_add(1).ok_or(FailStop::Overflow)?;
        }
        // E-13.2, E-13.3
        self.count_maker(maker.account_id, inst.event_id, quantity)?;
        self.count_group(taker.account_id, taker.group, quantity)?;
        self.count_group(maker.account_id, maker.group, quantity)?;
        for x in [taker_slot, maker_slot] {
            if x == maker_slot && !maker_alive {
                continue;
            }
            let o = self.orders[x as usize];
            if o.remaining > 0 || o.in_use {
                self.refresh_reserved(x)?;
            } else if o.reserved != 0 {
                self.release(x)?;
            }
        }
        if maker_done {
            self.free_slot(maker_slot)?;
        }
        self.mark((MARK_FILLED, inst.event_id, buyer.account_id))?;
        self.mark((MARK_FILLED, inst.event_id, seller.account_id))
    }

    /// E-11.4 for one account on one instrument.
    pub(crate) fn restore_locks<S: ResultSink>(
        &mut self,
        account_slot: u32,
        inst_slot: u32,
        sink: &mut S,
    ) -> Result<(), FailStop> {
        let account_id = self.accounts[account_slot as usize].id;
        let instrument_id = self.instruments[inst_slot as usize].id;
        for side in [Side::SELL, Side::BUY] {
            let h = self.holding(account_id, instrument_id);
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
                    let key = (o.accepted_at, o.id);
                    if o.instrument_slot == inst_slot
                        && o.side == side
                        && o.lock > 0
                        && below.is_none_or(|b| key < b)
                        && victim.is_none_or(|(v, _)| key > v)
                    {
                        victim = Some((key, cur));
                    }
                    cur = o.account_next;
                }
                let Some((key, v)) = victim else { break };
                below = Some(key);
                let o = self.orders[v as usize];
                let take = i64::from(o.lock).min(excess) as u32;
                self.add_locked(account_id, instrument_id, side, -i64::from(take))?;
                self.order_w(v)?.lock = o.lock - take;
                excess -= i64::from(take);
                if o.reduce_only {
                    let remaining = o.remaining - take;
                    self.set_remaining(v, remaining)?;
                    if remaining == 0 {
                        self.remove(v, CancelReason::POSITION_CLOSED, sink)?;
                        continue;
                    }
                    let reduced =
                        Out::OrderReduced { order_id: o.id, account_id, instrument_id, remaining_quantity: remaining };
                    self.out(sink, &reduced)?;
                }
                self.refresh_reserved(v)?;
            }
            if excess > 0 {
                return Err(FailStop::Inconsistent);
            }
        }
        Ok(())
    }

    fn count_maker(&mut self, account_id: u64, event_id: u64, quantity: u32) -> Result<(), FailStop> {
        let setting =
            self.maker_settings.get(&(account_id, event_id)).or_else(|| self.maker_settings.get(&(account_id, 0)));
        let Some(setting) = setting.copied() else { return Ok(()) };
        let key = (account_id, event_id);
        let mut counter = self.maker_counters.get(&key).copied().unwrap_or_default();
        let count = window_count(self.now, &mut counter, setting.window_millis, quantity)?;
        self.maker_counter_w(key, counter)?;
        if count >= u64::from(setting.threshold) {
            self.mark((MARK_PULL, account_id, event_id))?;
        }
        Ok(())
    }

    fn count_group(&mut self, account_id: u64, group: Option<u32>, quantity: u32) -> Result<(), FailStop> {
        let Some(group) = group else { return Ok(()) };
        let key = (account_id, group);
        let Some(mut g) = self.order_groups.get(&key).copied() else { return Ok(()) };
        if g.threshold == 0 {
            return Ok(());
        }
        let mut counter = WindowCounter { count: g.count, window_start: g.window_start };
        let count = window_count(self.now, &mut counter, g.window_millis, quantity)?;
        g.count = counter.count;
        g.window_start = counter.window_start;
        self.order_group_w(key, g)?;
        if count >= u64::from(g.threshold) {
            self.mark((MARK_TRIGGER, account_id, u64::from(group)))?;
        }
        Ok(())
    }

    /// E-13.4 and E-14.3, then the end of the unit.
    pub(crate) fn finish_order_unit<S: ResultSink>(&mut self, sink: &mut S) -> Result<(), FailStop> {
        let n = self.marks_len;
        self.marks[..n].sort_unstable();
        let mut previous = None;
        for i in 0..n {
            let mark = self.marks[i];
            if previous == Some(mark) {
                continue;
            }
            previous = Some(mark);
            match mark.0 {
                MARK_PULL => {
                    let (account, event) = (mark.1, mark.2);
                    let keep = |o: &Order, inst: &Instrument| o.account_id == account && inst.event_id == event;
                    self.remove_where(keep, CancelReason::MAKER_AUTO_PULL, sink)?;
                    self.maker_counter_w((account, event), WindowCounter::default())?;
                }
                MARK_TRIGGER => {
                    let (account, group) = (mark.1, mark.2 as u32);
                    let mut g = self.order_groups.get(&(account, group)).copied().ok_or(FailStop::Inconsistent)?;
                    g.triggered = true;
                    g.count = 0;
                    g.window_start = None;
                    self.order_group_w((account, group), g)?;
                    let updated = Out::OrderGroupUpdated {
                        account_id: account,
                        client_order_group: group,
                        max_contracts_filled_per_window: g.threshold,
                        window_millis: g.window_millis,
                        triggered: true,
                    };
                    self.out(sink, &updated)?;
                    let keep = |o: &Order, _: &Instrument| o.account_id == account && o.group == Some(group);
                    self.remove_where(keep, CancelReason::ORDER_GROUP_TRIGGERED, sink)?;
                }
                _ => self.set_burn(mark.1, mark.2, sink)?,
            }
        }
        self.marks_len = 0;
        self.end_unit(sink)
    }

    // ------------------------------------------------------------------ set burns (E-6.4, E-14.3)

    pub(crate) fn is_partition(&self, event: &Event) -> bool {
        if event.instrument_count < 2 {
            return false;
        }
        let mut seen = 0u64;
        let mut cur = event.first_instrument;
        while cur != NONE {
            let inst = &self.instruments[cur as usize];
            if seen & inst.mask != 0 {
                return false;
            }
            seen |= inst.mask;
            cur = inst.next_in_event;
        }
        let full = if event.state_count >= 64 { u64::MAX } else { (1u64 << event.state_count) - 1 };
        seen == full
    }

    /// The smallest unlocked holding across the event's instruments on one side, floored to the common step.
    fn burnable(&self, event: &Event, account_id: u64, side: Side) -> (i64, i64) {
        let (mut step, mut count, mut least) = (1u64, 0i64, i64::MAX);
        let mut cur = event.first_instrument;
        while cur != NONE {
            let inst = &self.instruments[cur as usize];
            let lot = u64::from(inst.lot_step);
            step = step / gcd(step, lot) * lot;
            count += 1;
            least = least.min(self.unlocked_holding(account_id, inst.id, side));
            cur = inst.next_in_event;
        }
        (floor_to(least.max(0) as u64, step) as i64, count)
    }

    fn set_burn<S: ResultSink>(&mut self, event_id: u64, account_id: u64, sink: &mut S) -> Result<(), FailStop> {
        let Some(event) = self.events.get(&event_id).copied() else { return Ok(()) };
        if event.resolved || !self.is_partition(&event) {
            return Ok(());
        }
        let account_slot = *self.account_index.get(&account_id).ok_or(FailStop::Inconsistent)?;
        for (side, kind) in [(Side::SELL, ReleaseKind::YES_SET_BURN), (Side::BUY, ReleaseKind::NO_SET_BURN)] {
            let (k, n) = self.burnable(&event, account_id, side);
            if k <= 0 {
                continue;
            }
            let mut cur = event.first_instrument;
            while cur != NONE {
                let inst = self.instruments[cur as usize];
                let p = self.holding(account_id, inst.id).position;
                let moved = if side == Side::SELL { p - k } else { p + k };
                self.set_position(account_id, inst.id, moved)?;
                cur = inst.next_in_event;
            }
            let cents = if side == Side::SELL { 100 * k } else { 100 * (n - 1) * k };
            self.add_event_cash(account_slot, event_id, cents)?;
            self.add_collateral(event_id, -cents)?;
            self.out(sink, &Out::CollateralReleased { account_id, event_id, cents, kind })?;
        }
        Ok(())
    }

    // ------------------------------------------------------------------ cancels, amendments, batches (§12)

    fn find_by_client_order_id(&self, account_id: u64, client_order_id: &ClientOrderId) -> Option<u32> {
        if *client_order_id == NO_CLIENT_ORDER_ID {
            return None;
        }
        let &account_slot = self.account_index.get(&account_id)?;
        let mut found: Option<(u64, u32)> = None;
        let mut cur = self.accounts[account_slot as usize].first_order;
        while cur != NONE {
            let o = &self.orders[cur as usize];
            if o.client_order_id == *client_order_id && found.is_none_or(|(id, _)| o.id < id) {
                found = Some((o.id, cur));
            }
            cur = o.account_next;
        }
        found.map(|(_, slot)| slot)
    }

    /// E-12.2.
    fn cancel<S: ResultSink>(
        &mut self,
        session_id: u64,
        account_id: u64,
        instrument_id: u64,
        order_id: Option<u64>,
        client_order_id: ClientOrderId,
        sink: &mut S,
    ) -> Result<(), FailStop> {
        let slot = match order_id {
            Some(id) => self.order_index.get(&id).copied(),
            None => self.find_by_client_order_id(account_id, &client_order_id),
        };
        let target = slot.filter(|&s| {
            let o = &self.orders[s as usize];
            o.account_id == account_id && self.instruments[o.instrument_slot as usize].id == instrument_id
        });
        let Some(slot) = target else {
            let reject = Out::OrderRejected {
                account_id,
                session_id,
                client_order_id,
                instrument_id,
                reason: RejectReason::UNKNOWN_ORDER,
                original_order_id: None,
            };
            return self.out(sink, &reject);
        };
        let o = self.orders[slot as usize];
        self.unlink(slot)?;
        self.release(slot)?;
        let cancelled = Out::OrderCancelled {
            order_id: o.id,
            account_id,
            instrument_id,
            cancelled_quantity: o.remaining,
            reason: CancelReason::CLIENT_REQUEST,
        };
        self.out(sink, &cancelled)?;
        self.free_slot(slot)
    }

    /// E-12.3 to E-12.5.
    fn amend<S: ResultSink>(&mut self, input: &[u8], sink: &mut S) -> Result<(), FailStop> {
        let d = amend_order_codec::AmendOrderDecoder::default().header(header(input), 0);
        let (session_id, account_id, instrument_id) = (d.session_id(), d.account_id(), d.instrument_id());
        let (new_tick, new_quantity, aes) = (d.new_tick(), d.new_quantity(), d.at_event_start());
        let (slot, tick, quantity, priority_kept) =
            match self.amend_checks(account_id, instrument_id, d.order_id(), new_tick, new_quantity, aes)? {
                Ok(verdict) => verdict,
                Err(reason) => {
                    let reject = Out::OrderRejected {
                        account_id,
                        session_id,
                        client_order_id: NO_CLIENT_ORDER_ID,
                        instrument_id,
                        reason,
                        original_order_id: None,
                    };
                    self.out(sink, &reject)?;
                    return self.end_unit(sink);
                }
            };
        let o = self.orders[slot as usize];
        if priority_kept {
            // E-12.4
            self.set_remaining(slot, quantity)?;
        } else {
            // E-12.5
            self.unlink(slot)?;
        }
        if o.lock > quantity {
            self.set_lock(slot, quantity)?;
        }
        let accepted_at = self.last_sequence;
        {
            let w = self.order_w(slot)?;
            if aes != AtEventStart::UNCHANGED {
                w.at_event_start = aes;
            }
            if !priority_kept {
                w.tick = tick;
                w.remaining = quantity;
                w.accepted_at = accepted_at;
            }
        }
        self.refresh_reserved(slot)?;
        let o = self.orders[slot as usize];
        let amended = Out::OrderAmended {
            order_id: o.id,
            account_id,
            instrument_id,
            tick: o.tick,
            quantity: o.remaining,
            at_event_start: o.at_event_start,
            priority_kept,
        };
        self.out(sink, &amended)?;
        if priority_kept {
            return self.end_unit(sink);
        }
        self.match_and_place(slot, sink)?;
        self.finish_order_unit(sink)
    }

    /// The plan for an amendment that passes its checks.
    fn amend_checks(
        &self,
        account_id: u64,
        instrument_id: u64,
        order_id: u64,
        new_tick: Option<u16>,
        new_quantity: Option<u32>,
        aes: AtEventStart,
    ) -> Result<Result<AmendPlan, RejectReason>, FailStop> {
        use RejectReason as R;
        let Some(&slot) = self.order_index.get(&order_id) else { return Ok(Err(R::UNKNOWN_ORDER)) };
        let o = self.orders[slot as usize];
        let inst = self.instruments[o.instrument_slot as usize];
        if o.account_id != account_id || inst.id != instrument_id {
            return Ok(Err(R::UNKNOWN_ORDER));
        }
        let nothing = new_tick.is_none() && new_quantity.is_none() && aes == AtEventStart::UNCHANGED;
        if aes == AtEventStart::NullVal || nothing || new_quantity == Some(0) {
            return Ok(Err(R::MALFORMED));
        }
        if new_tick.is_some_and(|t| t >= inst.table.len) {
            return Ok(Err(R::TICK_NOT_ON_LADDER));
        }
        if new_quantity.is_some_and(|q| !q.is_multiple_of(inst.lot_step)) {
            return Ok(Err(R::QUANTITY_NOT_ON_LOT_STEP));
        }
        let tick = new_tick.unwrap_or(o.tick);
        let quantity = new_quantity.unwrap_or(o.remaining);
        if tick == o.tick && quantity <= o.remaining {
            return Ok(Ok((slot, tick, quantity, true)));
        }
        if !matches!(self.venue_mode, VenueMode::NORMAL | VenueMode::NEW_ORDER_REJECT) {
            return Ok(Err(R::VENUE_MODE));
        }
        if inst.market_state != MarketState::OPEN || self.event_suspended(inst.event_id) {
            return Ok(Err(R::MARKET_NOT_OPEN));
        }
        if o.reduce_only && quantity > o.lock {
            return Ok(Err(R::REDUCE_ONLY_NO_POSITION));
        }
        if o.post_only && self.best(o.instrument_slot, opposite(o.side)).is_some_and(|b| crosses(o.side, b, tick)) {
            return Ok(Err(R::POST_ONLY_WOULD_TAKE));
        }
        if self.position_limit_breached(o.account_slot, o.instrument_slot, o.side, quantity, slot) {
            return Ok(Err(R::POSITION_LIMIT));
        }
        let new_lock = o.lock.min(quantity);
        let cost = self.unit_cost(o.instrument_slot, o.side, tick, u64::from(quantity - new_lock))?;
        let new_reserved = o.spend_cap_remaining.map_or(cost, |cap| cost.min(cap));
        let account = &self.accounts[o.account_slot as usize];
        if new_reserved - o.reserved > (account.balance - account.reserved).max(0) {
            return Ok(Err(R::INSUFFICIENT_SLEEVE));
        }
        Ok(Ok((slot, tick, quantity, false)))
    }

    /// E-12.6.
    fn mass_cancel<S: ResultSink>(&mut self, input: &[u8], sink: &mut S) -> Result<(), FailStop> {
        let d = mass_cancel_codec::MassCancelDecoder::default().header(header(input), 0);
        let (session_id, account_id, scope, scope_ref) = (d.session_id(), d.account_id(), d.scope(), d.scope_ref());
        let reason = match d.cause() {
            MassCancelCause::CLIENT_REQUEST => Some(CancelReason::MASS_CANCEL),
            MassCancelCause::SESSION_DISCONNECT => Some(CancelReason::SESSION_DISCONNECT),
            MassCancelCause::HEARTBEAT_MISSED => Some(CancelReason::HEARTBEAT_MISSED),
            MassCancelCause::OPERATOR_KILL_SWITCH => Some(CancelReason::KILL_SWITCH),
            MassCancelCause::NullVal => None,
        };
        let Some(reason) = reason.filter(|_| scope != MassCancelScope::NullVal) else {
            let reject = Out::OrderRejected {
                account_id,
                session_id,
                client_order_id: NO_CLIENT_ORDER_ID,
                instrument_id: 0,
                reason: RejectReason::MALFORMED,
                original_order_id: None,
            };
            self.out(sink, &reject)?;
            return self.end_unit(sink);
        };
        let keep = |o: &Order, inst: &Instrument| {
            o.account_id == account_id
                && match scope {
                    MassCancelScope::SESSION => o.session_id == session_id,
                    MassCancelScope::ACCOUNT => true,
                    MassCancelScope::INSTRUMENT => inst.id == scope_ref,
                    MassCancelScope::EVENT => inst.event_id == scope_ref,
                    MassCancelScope::CLIENT_ORDER_GROUP => o.group.is_some_and(|g| u64::from(g) == scope_ref),
                    MassCancelScope::NullVal => false,
                }
        };
        self.remove_where(keep, reason, sink)?;
        self.end_unit(sink)
    }

    /// E-12.8.
    fn batch<S: ResultSink>(&mut self, input: &[u8], sink: &mut S) -> Result<(), FailStop> {
        let d = batch_orders_codec::BatchOrdersDecoder::default().header(header(input), 0);
        let (session_id, account_id) = (d.session_id(), d.account_id());
        let mut entries = d.entries_decoder();
        while entries.advance().map_err(|_| FailStop::MalformedCommand)?.is_some() {
            match entries.entry_type() {
                BatchEntryType::NEW => {
                    let fields = new_order_fields!(entries, session_id, account_id);
                    self.new_order(&fields, sink)?;
                    self.finish_order_unit(sink)?;
                }
                BatchEntryType::CANCEL => {
                    let (instrument_id, order_id) = (entries.instrument_id(), entries.order_id());
                    self.cancel(session_id, account_id, instrument_id, order_id, entries.client_order_id(), sink)?;
                    self.end_unit(sink)?;
                }
                BatchEntryType::NullVal => {
                    let reject = Out::OrderRejected {
                        account_id,
                        session_id,
                        client_order_id: entries.client_order_id(),
                        instrument_id: entries.instrument_id(),
                        reason: RejectReason::MALFORMED,
                        original_order_id: None,
                    };
                    self.out(sink, &reject)?;
                    self.end_unit(sink)?;
                }
            }
        }
        Ok(())
    }

    /// E-12.7.
    fn order_group<S: ResultSink>(&mut self, template: u16, input: &[u8], sink: &mut S) -> Result<(), FailStop> {
        let (account_id, group, settings) = if template == set_order_group_codec::SBE_TEMPLATE_ID {
            let d = set_order_group_codec::SetOrderGroupDecoder::default().header(header(input), 0);
            (d.account_id(), d.client_order_group(), Some((d.max_contracts_filled_per_window(), d.window_millis())))
        } else {
            let d = reset_order_group_codec::ResetOrderGroupDecoder::default().header(header(input), 0);
            (d.account_id(), d.client_order_group(), None)
        };
        let key = (account_id, group);
        let mut g = self.order_groups.get(&key).copied().unwrap_or_default();
        match settings {
            Some((threshold, window_millis)) => {
                g.threshold = threshold;
                g.window_millis = window_millis;
            }
            None => {
                g.triggered = false;
                g.count = 0;
                g.window_start = None;
            }
        }
        self.order_group_w(key, g)?;
        let updated = Out::OrderGroupUpdated {
            account_id,
            client_order_group: group,
            max_contracts_filled_per_window: g.threshold,
            window_millis: g.window_millis,
            triggered: g.triggered,
        };
        self.out(sink, &updated)?;
        self.end_unit(sink)
    }
}
