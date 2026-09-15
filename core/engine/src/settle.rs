//! Resolution and settlement (spec §15). There are no baskets (D-24).

use crate::commands::header;
use crate::emit::{CancelReason, MarketState, Out, ResolutionKind};
use crate::state::*;
use crate::{Engine, FailStop, ResultSink};
use venue_journal::instrument_class::InstrumentClass;
use venue_journal::{expire_parlay_codec, resolve_event_codec};

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
    // ------------------------------------------------------------------ E-15.3

    /// The event, kind and winning state of a valid `ResolveEvent`, or `None` if it is rejected. A void takes no
    /// weights (D-1); its winning state is unused.
    pub(crate) fn resolution(&self, input: &[u8]) -> Result<Option<(u64, ResolutionKind, u8)>, FailStop> {
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
            return Ok((winning < event.state_count).then_some((event_id, kind, winning)));
        }
        let mut weights = dec.void_weights_decoder();
        if weights.advance().map_err(|_| FailStop::MalformedCommand)?.is_some() {
            return Ok(None);
        }
        Ok(Some((event_id, kind, 0)))
    }

    /// E-15.7: the event and losing state of a valid `ExpireParlay`, or `None` if it is rejected.
    pub(crate) fn parlay_expiry(&self, input: &[u8]) -> Result<Option<(u64, u8)>, FailStop> {
        let dec = expire_parlay_codec::ExpireParlayDecoder::default().header(header(input), 0);
        let Some(&slot) = self.instrument_index.get(&dec.instrument_id()) else { return Ok(None) };
        let inst = self.instruments[slot as usize];
        let Some(event) = self.events.get(&inst.event_id).copied() else { return Ok(None) };
        if inst.class != InstrumentClass::PARLAY || event.resolved {
            return Ok(None);
        }
        // Listing (E-6.3) guarantees the mask leaves some state out.
        let losing = (0..event.state_count).find(|s| inst.mask >> s & 1 == 0).ok_or(FailStop::Inconsistent)?;
        Ok(Some((inst.event_id, losing)))
    }

    pub(crate) fn apply_resolution<S: ResultSink>(&mut self, input: &[u8], sink: &mut S) -> Result<(), FailStop> {
        let (event_id, kind, winning) = self.resolution(input)?.ok_or(FailStop::Inconsistent)?;
        self.settle(event_id, kind, winning, sink)
    }

    pub(crate) fn apply_parlay_expiry<S: ResultSink>(&mut self, input: &[u8], sink: &mut S) -> Result<(), FailStop> {
        let (event_id, losing) = self.parlay_expiry(input)?.ok_or(FailStop::Inconsistent)?;
        self.settle(event_id, ResolutionKind::RESOLVED, losing, sink)
    }

    /// E-15.5, E-15.6.
    fn settle<S: ResultSink>(
        &mut self,
        event_id: u64,
        kind: ResolutionKind,
        winning: u8,
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
        let paid = if kind == ResolutionKind::RESOLVED {
            self.pay_winners(event_id, winning, sink)?
        } else {
            self.reverse(&event, event_id, sink)?
        };
        // The event's cash is settled with it.
        self.event_cash.retain(|key, _| key.1 != event_id);
        if paid != event.collateral {
            return Err(FailStop::Conservation);
        }
        let e = self.events.get_mut(&event_id).ok_or(FailStop::Inconsistent)?;
        e.collateral = 0;
        e.resolved = true;
        Ok(())
    }

    /// E-15.5 for `RESOLVED`: each open position is paid in the winning state, ascending by account then instrument.
    fn pay_winners<S: ResultSink>(&mut self, event_id: u64, winning: u8, sink: &mut S) -> Result<i64, FailStop> {
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
            let cents = payout(position, mask, winning);
            let account_slot = self.account_slot(account_id)?;
            self.add_balance(account_slot, cents)?;
            self.set_position(account_id, instrument_id, 0)?;
            paid = paid.checked_add(cents).ok_or(FailStop::Overflow)?;
            let kind = ResolutionKind::RESOLVED;
            let payout = Out::SettlementPayout { account_id, instrument_id, kind, position_closed: position, cents };
            self.out(sink, &payout)?;
        }
        // Settled holdings hold nothing and no order can lock them again.
        for j in 0..n {
            let key = self.scratch[j];
            self.holdings.remove(&key);
        }
        Ok(paid)
    }

    /// E-15.5 for `VOID` (D-1): each account with a position or cash in the event, ascending by account, has its
    /// positions closed and is paid the negation of the cash the event paid it.
    fn reverse<S: ResultSink>(&mut self, event: &Event, event_id: u64, sink: &mut S) -> Result<i64, FailStop> {
        self.voided = true;
        // Every fill and set burn records cash, so every account holding a position has an entry.
        let n = {
            let Self { event_cash, scratch, .. } = self;
            let mut n = 0;
            event_cash.for_each(|&(account_id, e), _| {
                if e == event_id {
                    scratch[n] = (account_id, 0);
                    n += 1;
                }
            });
            scratch[..n].sort_unstable();
            n
        };
        let mut paid = 0i64;
        for j in 0..n {
            let account_id = self.scratch[j].0;
            let cash = self.event_cash.get(&(account_id, event_id)).copied().unwrap_or(0);
            let mut open = false;
            let mut cur = event.first_instrument;
            while cur != NONE {
                let inst = self.instruments[cur as usize];
                if self.holding(account_id, inst.id).position != 0 {
                    open = true;
                    self.set_position(account_id, inst.id, 0)?;
                    // A voided holding holds nothing and no order can lock it again.
                    self.holdings.remove(&(account_id, inst.id));
                }
                cur = inst.next_in_event;
            }
            if cash == 0 && !open {
                continue;
            }
            let cents = cash.checked_neg().ok_or(FailStop::Overflow)?;
            let account_slot = self.account_slot(account_id)?;
            self.add_balance(account_slot, cents)?;
            paid = paid.checked_add(cents).ok_or(FailStop::Overflow)?;
            let kind = ResolutionKind::VOID;
            self.out(sink, &Out::SettlementPayout { account_id, instrument_id: 0, kind, position_closed: 0, cents })?;
        }
        // A position with no cash entry would have escaped the reversal.
        let mut left = 0usize;
        let Self { holdings, instrument_index, instruments, .. } = self;
        holdings.for_each(|&(_, instrument_id), h| {
            let in_event =
                instrument_index.get(&instrument_id).is_some_and(|&s| instruments[s as usize].event_id == event_id);
            if h.position != 0 && in_event {
                left += 1;
            }
        });
        if left > 0 {
            return Err(FailStop::Inconsistent);
        }
        Ok(paid)
    }
}
