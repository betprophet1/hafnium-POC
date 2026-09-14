//! Baskets (spec E-14.1, E-14.2) and resolution and settlement (§15).

use crate::commands::header;
use crate::emit::{BasketDirection, CancelReason, MarketState, Out, RejectReason, ResolutionKind, Side};
use crate::orders::NO_CLIENT_ORDER_ID;
use crate::state::*;
use crate::{Engine, FailStop, ResultSink};
use venue_journal::instrument_class::InstrumentClass;
use venue_journal::venue_mode::VenueMode;
use venue_journal::{basket_mint_redeem_codec, expire_parlay_codec, resolve_event_codec};

/// Settlement weights by state (E-15.4). `total` is never 0.
pub(crate) struct Weights {
    of: [u64; 64],
    total: u64,
}

impl Weights {
    fn only(state: u8) -> Self {
        let mut of = [0; 64];
        of[usize::from(state)] = 1;
        Self { of, total: 1 }
    }
}

/// E-15.1.
fn payout(position: i64, mask: u64, state: u8) -> i64 {
    let in_mask = mask >> state & 1 == 1;
    if (position > 0 && in_mask) || (position < 0 && !in_mask) {
        100 * position.abs()
    } else {
        0
    }
}

impl<'a> Engine<'a> {
    // ------------------------------------------------------------------ E-15.3, E-15.4

    /// The event, kind and weights of a valid `ResolveEvent`, or `None` if it is rejected.
    pub(crate) fn resolution(&self, input: &[u8]) -> Result<Option<(u64, ResolutionKind, Weights)>, FailStop> {
        let dec = resolve_event_codec::ResolveEventDecoder::default().header(header(input), 0);
        let (event_id, kind, winning) = (dec.event_id(), dec.kind(), dec.winning_state());
        if kind == ResolutionKind::NullVal {
            return Err(FailStop::MalformedCommand);
        }
        let Some(event) = self.events.get(&event_id).copied() else { return Ok(None) };
        if event.resolved {
            return Ok(None);
        }
        if kind == ResolutionKind::RESOLVED {
            return Ok((winning < event.state_count).then(|| (event_id, kind, Weights::only(winning))));
        }
        let mut weights = Weights { of: [0; 64], total: 0 };
        let (mut seen, mut given) = (0u64, 0usize);
        let mut entries = dec.void_weights_decoder();
        while entries.advance().map_err(|_| FailStop::MalformedCommand)?.is_some() {
            let state = entries.state();
            if state >= event.state_count || seen >> state & 1 == 1 {
                return Ok(None);
            }
            seen |= 1 << state;
            weights.of[usize::from(state)] = u64::from(entries.weight());
            weights.total += u64::from(entries.weight());
            given += 1;
        }
        if given == 0 {
            for w in &mut weights.of[..usize::from(event.state_count)] {
                *w = 1;
            }
            weights.total = u64::from(event.state_count);
        } else if weights.total == 0 {
            return Ok(None);
        }
        Ok(Some((event_id, kind, weights)))
    }

    /// E-15.7: the event and weights of a valid `ExpireParlay`, or `None` if it is rejected.
    pub(crate) fn parlay_expiry(&self, input: &[u8]) -> Result<Option<(u64, Weights)>, FailStop> {
        let dec = expire_parlay_codec::ExpireParlayDecoder::default().header(header(input), 0);
        let Some(&slot) = self.instrument_index.get(&dec.instrument_id()) else { return Ok(None) };
        let inst = self.instruments[slot as usize];
        let Some(event) = self.events.get(&inst.event_id).copied() else { return Ok(None) };
        if inst.class != InstrumentClass::PARLAY || event.resolved {
            return Ok(None);
        }
        // Listing (E-6.3) guarantees the mask leaves some state out.
        let losing = (0..event.state_count).find(|s| inst.mask >> s & 1 == 0).ok_or(FailStop::Inconsistent)?;
        Ok(Some((inst.event_id, Weights::only(losing))))
    }

    pub(crate) fn apply_resolution<S: ResultSink>(&mut self, input: &[u8], sink: &mut S) -> Result<(), FailStop> {
        let (event_id, kind, weights) = self.resolution(input)?.ok_or(FailStop::Inconsistent)?;
        self.settle(event_id, kind, &weights, sink)
    }

    pub(crate) fn apply_parlay_expiry<S: ResultSink>(&mut self, input: &[u8], sink: &mut S) -> Result<(), FailStop> {
        let (event_id, weights) = self.parlay_expiry(input)?.ok_or(FailStop::Inconsistent)?;
        self.settle(event_id, ResolutionKind::RESOLVED, &weights, sink)
    }

    /// E-15.5, E-15.6.
    fn settle<S: ResultSink>(
        &mut self,
        event_id: u64,
        kind: ResolutionKind,
        weights: &Weights,
        sink: &mut S,
    ) -> Result<(), FailStop> {
        let event = *self.events.get(&event_id).ok_or(FailStop::Inconsistent)?;
        self.remove_where(|_, inst| inst.event_id == event_id, CancelReason::MARKET_RESOLVED, sink)?;
        let state = if kind == ResolutionKind::RESOLVED { MarketState::SETTLED } else { MarketState::VOIDED };
        let mut cur = event.first_instrument;
        while cur != NONE {
            let inst = &mut self.instruments[cur as usize];
            inst.market_state = state;
            let (instrument_id, next) = (inst.id, inst.next_in_event);
            self.out(sink, &Out::MarketStateChanged { instrument_id, state })?;
            cur = next;
        }
        // Open positions in the event, ascending by account then instrument.
        let n = {
            let Self { holdings, instrument_index, instruments, scratch, .. } = self;
            let mut n = 0;
            holdings.for_each(|&key, h| {
                let in_event =
                    instrument_index.get(&key.1).is_some_and(|&s| instruments[s as usize].event_id == event_id);
                if h.position != 0 && in_event {
                    scratch[n] = key;
                    n += 1;
                }
            });
            scratch[..n].sort_unstable();
            n
        };
        let mut paid = 0i64;
        for j in 0..n {
            let (account_id, instrument_id) = self.scratch[j];
            let position = self.holding(account_id, instrument_id).position;
            let slot = *self.instrument_index.get(&instrument_id).ok_or(FailStop::Inconsistent)?;
            let mask = self.instruments[slot as usize].mask;
            let mut weighted = 0i128;
            for s in 0..event.state_count {
                let w = weights.of[usize::from(s)];
                if w != 0 {
                    weighted += i128::from(w) * i128::from(payout(position, mask, s));
                }
            }
            let cents = i64::try_from(weighted / i128::from(weights.total)).map_err(|_| FailStop::Overflow)?;
            let account_slot = self.account_slot(account_id)?;
            self.add_balance(account_slot, cents)?;
            self.set_position(account_id, instrument_id, 0)?;
            paid = paid.checked_add(cents).ok_or(FailStop::Overflow)?;
            let payout = Out::SettlementPayout { account_id, instrument_id, kind, position_closed: position, cents };
            self.out(sink, &payout)?;
        }
        // Settled holdings hold nothing and no order can lock them again.
        for j in 0..n {
            let key = self.scratch[j];
            self.holdings.remove(&key);
        }
        let remainder = event.collateral.checked_sub(paid).ok_or(FailStop::Overflow)?;
        if remainder < 0 {
            return Err(FailStop::Conservation);
        }
        if remainder > 0 {
            let reserve = self.account_slot(0)?;
            self.add_balance(reserve, remainder)?;
            let payout =
                Out::SettlementPayout { account_id: 0, instrument_id: 0, kind, position_closed: 0, cents: remainder };
            self.out(sink, &payout)?;
        }
        let e = self.events.get_mut(&event_id).ok_or(FailStop::Inconsistent)?;
        e.collateral = 0;
        e.resolved = true;
        Ok(())
    }

    // ------------------------------------------------------------------ E-14.1, E-14.2

    pub(crate) fn basket<S: ResultSink>(&mut self, input: &[u8], sink: &mut S) -> Result<(), FailStop> {
        let d = basket_mint_redeem_codec::BasketMintRedeemDecoder::default().header(header(input), 0);
        let (session_id, account_id, client_order_id) = (d.session_id(), d.account_id(), d.client_order_id());
        let (event_id, quantity, direction) = (d.event_id(), d.quantity(), d.direction());
        let event = match self.basket_checks(account_id, &client_order_id, event_id, quantity, direction)? {
            Ok(event) => event,
            Err((reason, original_order_id)) => {
                let reject = Out::OrderRejected {
                    account_id,
                    session_id,
                    client_order_id,
                    instrument_id: 0,
                    reason,
                    original_order_id,
                };
                self.out(sink, &reject)?;
                return self.end_unit(sink);
            }
        };
        let (q, mint) = (i64::from(quantity), direction == BasketDirection::MINT);
        let account_slot = self.account_slot(account_id)?;
        // New YES closes NO the account holds, so only the rest is newly funded.
        let mut closed = 0i64;
        let mut cur = event.first_instrument;
        while cur != NONE {
            let inst = self.instruments[cur as usize];
            let position = self.holding(account_id, inst.id).position;
            if mint {
                closed += q.min((-position).max(0));
            }
            self.set_position(account_id, inst.id, if mint { position + q } else { position - q })?;
            cur = inst.next_in_event;
        }
        let cash = if mint { -100 * (q - closed) } else { 100 * q };
        self.add_balance(account_slot, cash)?;
        self.add_collateral(event_id, -cash)?;
        let applied = Out::BasketApplied {
            account_id,
            session_id,
            client_order_id,
            event_id,
            quantity,
            direction,
            cash_cents: cash,
        };
        self.out(sink, &applied)?;
        if mint {
            let mut cur = event.first_instrument;
            while cur != NONE {
                self.restore_locks(account_slot, cur, sink)?;
                cur = self.instruments[cur as usize].next_in_event;
            }
        }
        self.end_unit(sink)
    }

    fn basket_checks(
        &mut self,
        account_id: u64,
        client_order_id: &[u8; 32],
        event_id: u64,
        quantity: u32,
        direction: BasketDirection,
    ) -> Result<Result<Event, (RejectReason, Option<u64>)>, FailStop> {
        use RejectReason as R;
        if account_id == 0
            || *client_order_id == NO_CLIENT_ORDER_ID
            || quantity == 0
            || direction == BasketDirection::NullVal
        {
            return Ok(Err((R::MALFORMED, None)));
        }
        let key = (account_id, *client_order_id);
        if let Some(entry) = self.duplicates.get(&key).copied() {
            if self.now - entry.recorded_at < self.duplicate_window() {
                return Ok(Err((R::DUPLICATE_CLIENT_ORDER_ID, entry.order_id)));
            }
        }
        let entry = Duplicate { recorded_at: self.now, order_id: None };
        self.duplicates.insert(key, entry).map_err(|_| FailStop::Capacity)?;
        let event = match self.events.get(&event_id).copied() {
            Some(e) if !e.resolved && self.is_partition(&e) => e,
            _ => return Ok(Err((R::UNKNOWN_INSTRUMENT, None))),
        };
        let (mut open, mut on_step) = (!event.suspended, true);
        let mut cur = event.first_instrument;
        while cur != NONE {
            let inst = &self.instruments[cur as usize];
            open &= inst.market_state == MarketState::OPEN;
            on_step &= quantity.is_multiple_of(inst.lot_step);
            cur = inst.next_in_event;
        }
        if !open {
            return Ok(Err((R::MARKET_NOT_OPEN, None)));
        }
        if !on_step {
            return Ok(Err((R::QUANTITY_NOT_ON_LOT_STEP, None)));
        }
        let account_slot = self.account_slot(account_id)?;
        if direction == BasketDirection::MINT {
            if self.venue_mode != VenueMode::NORMAL {
                return Ok(Err((R::VENUE_MODE, None)));
            }
            let mut cur = event.first_instrument;
            while cur != NONE {
                if self.position_limit_breached(account_slot, cur, Side::BUY, quantity, NONE) {
                    return Ok(Err((R::POSITION_LIMIT, None)));
                }
                cur = self.instruments[cur as usize].next_in_event;
            }
            let a = &self.accounts[account_slot as usize];
            if 100 * i64::from(quantity) > a.balance - a.reserved {
                return Ok(Err((R::INSUFFICIENT_SLEEVE, None)));
            }
        } else {
            let mut cur = event.first_instrument;
            while cur != NONE {
                let inst = &self.instruments[cur as usize];
                if self.unlocked_holding(account_id, inst.id, Side::SELL) < i64::from(quantity) {
                    return Ok(Err((R::INSUFFICIENT_POSITION, None)));
                }
                cur = inst.next_in_event;
            }
        }
        Ok(Ok(event))
    }
}
