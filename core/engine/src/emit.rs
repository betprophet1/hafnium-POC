//! Encoding results (spec §5) straight into the sink, without allocation.
//!
//! Every result carries a `ResultHeader` naming the input, and an index that runs across all units of the
//! input (E-5.1). The emitter owns one scratch buffer large enough for the largest result.

use crate::{FailStop, ResultSink};
use venue_journal::*;

pub use venue_journal::at_event_start::AtEventStart;
pub use venue_journal::basket_direction::BasketDirection;
pub use venue_journal::cancel_reason::CancelReason;
pub use venue_journal::command_outcome::CommandOutcome;
pub use venue_journal::market_state::MarketState;
pub use venue_journal::reject_reason::RejectReason;
pub use venue_journal::release_kind::ReleaseKind;
pub use venue_journal::resolution_kind::ResolutionKind;
pub use venue_journal::side::Side;

pub type ClientOrderId = [u8; 32];
pub type CommandId = [u8; 16];

/// One result, with the schema's fields. Fees are not carried: they are zero in v0 (E-5.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Out {
    OrderAccepted {
        order_id: u64,
        account_id: u64,
        session_id: u64,
        client_order_id: ClientOrderId,
        instrument_id: u64,
        side: Side,
        tick: u16,
        quantity: u32,
        reserved_cents: i64,
    },
    OrderRejected {
        account_id: u64,
        session_id: u64,
        client_order_id: ClientOrderId,
        instrument_id: u64,
        reason: RejectReason,
        original_order_id: Option<u64>,
    },
    Fill {
        trade_id: u64,
        instrument_id: u64,
        tick: u16,
        quantity: u32,
        maker_order_id: u64,
        maker_account_id: u64,
        maker_remaining: u32,
        taker_order_id: u64,
        taker_account_id: u64,
        taker_remaining: u32,
        taker_side: Side,
        minted_pairs: u32,
        burned_pairs: u32,
        buyer_cash_cents: i64,
        seller_cash_cents: i64,
    },
    OrderCancelled {
        order_id: u64,
        account_id: u64,
        instrument_id: u64,
        cancelled_quantity: u32,
        reason: CancelReason,
    },
    OrderAmended {
        order_id: u64,
        account_id: u64,
        instrument_id: u64,
        tick: u16,
        quantity: u32,
        at_event_start: AtEventStart,
        priority_kept: bool,
    },
    OrderReduced {
        order_id: u64,
        account_id: u64,
        instrument_id: u64,
        remaining_quantity: u32,
    },
    PositionChanged {
        account_id: u64,
        instrument_id: u64,
        position: i64,
    },
    SleeveChanged {
        account_id: u64,
        available_cents: i64,
        reserved_cents: i64,
    },
    CollateralReleased {
        account_id: u64,
        event_id: u64,
        cents: i64,
        kind: ReleaseKind,
    },
    BookLevelChanged {
        instrument_id: u64,
        side: Side,
        tick: u16,
        quantity: u32,
        order_count: u32,
    },
    MarketStateChanged {
        instrument_id: u64,
        state: MarketState,
    },
    SettlementPayout {
        account_id: u64,
        instrument_id: u64,
        kind: ResolutionKind,
        position_closed: i64,
        cents: i64,
    },
    BasketApplied {
        account_id: u64,
        session_id: u64,
        client_order_id: ClientOrderId,
        event_id: u64,
        quantity: u32,
        direction: BasketDirection,
        cash_cents: i64,
    },
    OrderGroupUpdated {
        account_id: u64,
        client_order_group: u32,
        max_contracts_filled_per_window: u32,
        window_millis: u32,
        triggered: bool,
    },
    CommandApplied {
        command_id: CommandId,
        outcome: CommandOutcome,
        reject_reason: RejectReason,
    },
}

const HDR: usize = message_header_codec::ENCODED_LENGTH;

#[inline]
fn boolean(b: bool) -> boolean_type::BooleanType {
    if b {
        boolean_type::BooleanType::TRUE
    } else {
        boolean_type::BooleanType::FALSE
    }
}

#[inline]
fn sbe<T>(r: SbeResult<T>) -> Result<T, FailStop> {
    r.map_err(|_| FailStop::Capacity)
}

/// Stamps and encodes results for the input being processed.
pub struct Emitter {
    buf: [u8; 256],
    input_sequence: u64,
    sequenced_at: u64,
    group: u16,
    index: u32,
}

impl Emitter {
    pub const fn new(group: u16) -> Self {
        Self {
            buf: [0; 256],
            input_sequence: 0,
            sequenced_at: 0,
            group,
            index: 0,
        }
    }

    /// Starts a new input: indices restart at 0 (E-5.1).
    pub fn begin(&mut self, input_sequence: u64, sequenced_at: u64) {
        self.input_sequence = input_sequence;
        self.sequenced_at = sequenced_at;
        self.index = 0;
    }

    /// Results emitted so far for the current input.
    pub const fn count(&self) -> u32 {
        self.index
    }

    /// Encodes `out` and hands it to `sink`.
    pub fn emit<S: ResultSink>(&mut self, sink: &mut S, out: &Out) -> Result<(), FailStop> {
        let (seq, at, group, index) = (
            self.input_sequence,
            self.sequenced_at,
            self.group,
            self.index,
        );
        macro_rules! begin {
            ($codec:ident, $encoder:ident) => {{
                let mut enc = $codec::$encoder::default().wrap(WriteBuf::new(&mut self.buf), HDR);
                enc = sbe(enc.header(0).parent())?;
                let mut h = enc.result_encoder();
                h.input_sequence(seq)
                    .result_index(index)
                    .sequenced_at(at)
                    .sequencer_group(group);
                sbe(h.parent())?
            }};
        }
        let len = match *out {
            Out::OrderAccepted {
                order_id,
                account_id,
                session_id,
                client_order_id,
                instrument_id,
                side,
                tick,
                quantity,
                reserved_cents,
            } => {
                let mut e = begin!(order_accepted_codec, OrderAcceptedEncoder);
                e.order_id(order_id)
                    .account_id(account_id)
                    .session_id(session_id);
                e.client_order_id(&client_order_id);
                e.instrument_id(instrument_id)
                    .side(side)
                    .tick(tick)
                    .quantity(quantity)
                    .reserved_cents(reserved_cents);
                e.get_limit()
            }
            Out::OrderRejected {
                account_id,
                session_id,
                client_order_id,
                instrument_id,
                reason,
                original_order_id,
            } => {
                let mut e = begin!(order_rejected_codec, OrderRejectedEncoder);
                e.account_id(account_id).session_id(session_id);
                e.client_order_id(&client_order_id);
                e.instrument_id(instrument_id)
                    .reason(reason)
                    .original_order_id_opt(original_order_id);
                e.get_limit()
            }
            Out::Fill {
                trade_id,
                instrument_id,
                tick,
                quantity,
                maker_order_id,
                maker_account_id,
                maker_remaining,
                taker_order_id,
                taker_account_id,
                taker_remaining,
                taker_side,
                minted_pairs,
                burned_pairs,
                buyer_cash_cents,
                seller_cash_cents,
            } => {
                let mut e = begin!(fill_codec, FillEncoder);
                e.trade_id(trade_id)
                    .instrument_id(instrument_id)
                    .tick(tick)
                    .quantity(quantity);
                e.maker_order_id(maker_order_id)
                    .maker_account_id(maker_account_id)
                    .maker_remaining(maker_remaining);
                e.taker_order_id(taker_order_id)
                    .taker_account_id(taker_account_id)
                    .taker_remaining(taker_remaining);
                e.taker_side(taker_side)
                    .minted_pairs(minted_pairs)
                    .burned_pairs(burned_pairs);
                e.buyer_cash_cents(buyer_cash_cents)
                    .seller_cash_cents(seller_cash_cents);
                e.maker_fee_cents(0).taker_fee_cents(0);
                e.get_limit()
            }
            Out::OrderCancelled {
                order_id,
                account_id,
                instrument_id,
                cancelled_quantity,
                reason,
            } => {
                let mut e = begin!(order_cancelled_codec, OrderCancelledEncoder);
                e.order_id(order_id)
                    .account_id(account_id)
                    .instrument_id(instrument_id);
                e.cancelled_quantity(cancelled_quantity).reason(reason);
                e.get_limit()
            }
            Out::OrderAmended {
                order_id,
                account_id,
                instrument_id,
                tick,
                quantity,
                at_event_start,
                priority_kept,
            } => {
                let mut e = begin!(order_amended_codec, OrderAmendedEncoder);
                e.order_id(order_id)
                    .account_id(account_id)
                    .instrument_id(instrument_id)
                    .tick(tick)
                    .quantity(quantity);
                e.at_event_start(at_event_start)
                    .priority_kept(boolean(priority_kept));
                e.get_limit()
            }
            Out::OrderReduced {
                order_id,
                account_id,
                instrument_id,
                remaining_quantity,
            } => {
                let mut e = begin!(order_reduced_codec, OrderReducedEncoder);
                e.order_id(order_id)
                    .account_id(account_id)
                    .instrument_id(instrument_id);
                e.remaining_quantity(remaining_quantity);
                e.get_limit()
            }
            Out::PositionChanged {
                account_id,
                instrument_id,
                position,
            } => {
                let mut e = begin!(position_changed_codec, PositionChangedEncoder);
                e.account_id(account_id)
                    .instrument_id(instrument_id)
                    .position(position);
                e.get_limit()
            }
            Out::SleeveChanged {
                account_id,
                available_cents,
                reserved_cents,
            } => {
                let mut e = begin!(sleeve_changed_codec, SleeveChangedEncoder);
                e.account_id(account_id)
                    .available_cents(available_cents)
                    .reserved_cents(reserved_cents);
                e.get_limit()
            }
            Out::CollateralReleased {
                account_id,
                event_id,
                cents,
                kind,
            } => {
                let mut e = begin!(collateral_released_codec, CollateralReleasedEncoder);
                e.account_id(account_id)
                    .event_id(event_id)
                    .cents(cents)
                    .kind(kind);
                e.get_limit()
            }
            Out::BookLevelChanged {
                instrument_id,
                side,
                tick,
                quantity,
                order_count,
            } => {
                let mut e = begin!(book_level_changed_codec, BookLevelChangedEncoder);
                e.instrument_id(instrument_id)
                    .side(side)
                    .tick(tick)
                    .quantity(quantity)
                    .order_count(order_count);
                e.get_limit()
            }
            Out::MarketStateChanged {
                instrument_id,
                state,
            } => {
                let mut e = begin!(market_state_changed_codec, MarketStateChangedEncoder);
                e.instrument_id(instrument_id).state(state);
                e.get_limit()
            }
            Out::SettlementPayout {
                account_id,
                instrument_id,
                kind,
                position_closed,
                cents,
            } => {
                let mut e = begin!(settlement_payout_codec, SettlementPayoutEncoder);
                e.account_id(account_id)
                    .instrument_id(instrument_id)
                    .kind(kind);
                e.position_closed(position_closed).cents(cents);
                e.get_limit()
            }
            Out::BasketApplied {
                account_id,
                session_id,
                client_order_id,
                event_id,
                quantity,
                direction,
                cash_cents,
            } => {
                let mut e = begin!(basket_applied_codec, BasketAppliedEncoder);
                e.account_id(account_id).session_id(session_id);
                e.client_order_id(&client_order_id);
                e.event_id(event_id)
                    .quantity(quantity)
                    .direction(direction)
                    .cash_cents(cash_cents);
                e.get_limit()
            }
            Out::OrderGroupUpdated {
                account_id,
                client_order_group,
                max_contracts_filled_per_window,
                window_millis,
                triggered,
            } => {
                let mut e = begin!(order_group_updated_codec, OrderGroupUpdatedEncoder);
                e.account_id(account_id)
                    .client_order_group(client_order_group);
                e.max_contracts_filled_per_window(max_contracts_filled_per_window)
                    .window_millis(window_millis);
                e.triggered(boolean(triggered));
                e.get_limit()
            }
            Out::CommandApplied {
                command_id,
                outcome,
                reject_reason,
            } => {
                let mut e = begin!(command_applied_codec, CommandAppliedEncoder);
                e.command_id(&command_id);
                e.outcome(outcome).reject_reason(reject_reason);
                e.get_limit()
            }
        };
        sink.emit(&self.buf[..len]);
        self.index = self.index.checked_add(1).ok_or(FailStop::Overflow)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use venue_journal::fill_codec::FillDecoder;
    use venue_journal::message_header_codec::MessageHeaderDecoder;

    struct Last(Option<([u8; 256], usize)>, usize);
    impl ResultSink for Last {
        fn emit(&mut self, encoded: &[u8]) {
            let mut b = [0u8; 256];
            b[..encoded.len()].copy_from_slice(encoded);
            self.0 = Some((b, encoded.len()));
            self.1 += 1;
        }
    }

    #[test]
    fn stamps_headers_and_counts_across_the_input() {
        let mut e = Emitter::new(3);
        let mut sink = Last(None, 0);
        e.begin(41, 1_800);
        let fill = Out::Fill {
            trade_id: 9,
            instrument_id: 1000,
            tick: 59,
            quantity: 5,
            maker_order_id: 1,
            maker_account_id: 2,
            maker_remaining: 0,
            taker_order_id: 3,
            taker_account_id: 4,
            taker_remaining: 1,
            taker_side: Side::BUY,
            minted_pairs: 5,
            burned_pairs: 0,
            buyer_cash_cents: -300,
            seller_cash_cents: -200,
        };
        e.emit(&mut sink, &fill).unwrap();
        e.emit(&mut sink, &fill).unwrap();
        assert_eq!(e.count(), 2);
        let (bytes, len) = sink.0.unwrap();
        let header = MessageHeaderDecoder::default().wrap(ReadBuf::new(&bytes[..len]), 0);
        assert_eq!(header.template_id(), fill_codec::SBE_TEMPLATE_ID);
        let dec = FillDecoder::default().header(header, 0);
        let mut h = dec.result_decoder();
        assert_eq!(
            (
                h.input_sequence(),
                h.result_index(),
                h.sequenced_at(),
                h.sequencer_group()
            ),
            (41, 1, 1_800, 3)
        );
        let dec = h.parent().unwrap();
        assert_eq!(
            (
                dec.buyer_cash_cents(),
                dec.seller_cash_cents(),
                dec.minted_pairs()
            ),
            (-300, -200, 5)
        );
        e.begin(42, 1_801);
        assert_eq!(e.count(), 0);
    }
}
