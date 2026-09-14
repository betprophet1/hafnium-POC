//! Round trips through the code generated from interfaces/sbe/venue-journal.xml, and the
//! schema rules that SBE itself cannot express.

use venue_journal::batch_orders_codec::{encoder::EntriesEncoder, BatchOrdersDecoder, BatchOrdersEncoder};
use venue_journal::fill_codec::{FillDecoder, FillEncoder};
use venue_journal::message_header_codec::{self, MessageHeaderDecoder};
use venue_journal::new_order_codec::{NewOrderDecoder, NewOrderEncoder};
use venue_journal::resolve_event_codec::{encoder::VoidWeightsEncoder, ResolveEventDecoder, ResolveEventEncoder};
use venue_journal::transfer_in_codec::{TransferInDecoder, TransferInEncoder};
use venue_journal::*;

const HDR: usize = message_header_codec::ENCODED_LENGTH;
const RECORD_HEADER_LEN: usize = 8 + 8 + 2;

fn header_of(buf: &[u8]) -> MessageHeaderDecoder<ReadBuf<'_>> {
    MessageHeaderDecoder::default().wrap(ReadBuf::new(buf), 0)
}

fn encode_new_order(buf: &mut [u8]) -> usize {
    let mut enc = NewOrderEncoder::default().wrap(WriteBuf::new(buf), HDR);
    enc = enc.header(0).parent().unwrap();
    // RecordHeader is left zero: the gateway sends it that way and the sequencer stamps it.
    enc.session_id(77).account_id(1001);
    enc.client_order_id_zero_padded(b"mm-7-000123");
    enc.instrument_id(42)
        .side(side::Side::SELL)
        .entered_contract(contract::Contract::NO)
        .tick(38)
        .quantity(500)
        .time_in_force(time_in_force::TimeInForce::FOK)
        .expires_at_opt(None)
        .at_event_start(at_event_start::AtEventStart::CANCEL);
    let mut flags = order_flags::OrderFlags::default();
    flags.set_reduce_only(true);
    enc.flags(flags);
    enc.protection_ticks(3)
        .spend_cap_cents_opt(Some(19_000))
        .entered_stake_cents_opt(None)
        .client_order_group_opt(Some(9));
    enc.get_limit()
}

fn stamp(buf: &mut [u8], sequence: u64, sequenced_at: u64, group: u16) {
    let enc = NewOrderEncoder::default().wrap(WriteBuf::new(buf), HDR);
    let mut record = enc.record_encoder();
    record.sequence(sequence).sequenced_at(sequenced_at).sequencer_group(group);
    let _ = record.parent().unwrap();
}

#[test]
fn new_order_round_trips_with_optional_fields_and_flags() {
    let mut buf = vec![0u8; 256];
    let len = encode_new_order(&mut buf);

    let header = header_of(&buf[..len]);
    assert_eq!(header.template_id(), 1);
    assert_eq!(header.schema_id(), 7101);
    assert_eq!(header.version(), 0);

    let dec = NewOrderDecoder::default().header(header, 0);
    assert_eq!(dec.session_id(), 77);
    assert_eq!(dec.account_id(), 1001);
    assert_eq!(&dec.client_order_id()[..11], b"mm-7-000123");
    assert!(dec.client_order_id()[11..].iter().all(|b| *b == 0));
    assert_eq!(dec.instrument_id(), 42);
    assert_eq!(dec.side(), side::Side::SELL);
    assert_eq!(dec.entered_contract(), contract::Contract::NO);
    assert_eq!(dec.tick(), 38);
    assert_eq!(dec.quantity(), 500);
    assert_eq!(dec.time_in_force(), time_in_force::TimeInForce::FOK);
    assert_eq!(dec.expires_at(), None);
    assert_eq!(dec.at_event_start(), at_event_start::AtEventStart::CANCEL);
    assert!(dec.flags().get_reduce_only());
    assert!(!dec.flags().get_post_only());
    assert_eq!(dec.protection_ticks(), 3);
    assert_eq!(dec.spend_cap_cents(), Some(19_000));
    assert_eq!(dec.entered_stake_cents(), None);
    assert_eq!(dec.client_order_group(), Some(9));
}

#[test]
fn sequencer_stamps_the_record_header_in_place_and_changes_nothing_else() {
    let mut buf = vec![0u8; 256];
    let len = encode_new_order(&mut buf);
    let ingress = buf[..len].to_vec();

    // On ingress the record header, directly after the message header, is all zero.
    assert!(ingress[HDR..HDR + RECORD_HEADER_LEN].iter().all(|b| *b == 0));

    stamp(&mut buf, 123_456, 1_789_000_000_123_456_789, 2);
    let journal = &buf[..len];

    for (i, (before, after)) in ingress.iter().zip(journal.iter()).enumerate() {
        if !(HDR..HDR + RECORD_HEADER_LEN).contains(&i) {
            assert_eq!(before, after, "byte {i} outside the record header changed");
        }
    }
    let dec = NewOrderDecoder::default().header(header_of(journal), 0);
    let mut record = dec.record_decoder();
    assert_eq!(record.sequence(), 123_456);
    assert_eq!(record.sequenced_at(), 1_789_000_000_123_456_789);
    assert_eq!(record.sequencer_group(), 2);
    let dec = record.parent().unwrap();
    assert_eq!(dec.quantity(), 500);
}

#[test]
fn batch_entries_decode_in_the_order_given() {
    let mut buf = vec![0u8; 512];
    let mut enc = BatchOrdersEncoder::default().wrap(WriteBuf::new(&mut buf), HDR);
    enc = enc.header(0).parent().unwrap();
    enc.session_id(5).account_id(6);
    let mut entries = enc.entries_encoder(2, EntriesEncoder::default());

    entries.advance().unwrap();
    entries
        .entry_type(batch_entry_type::BatchEntryType::NEW)
        .instrument_id(42)
        .order_id_opt(None)
        .side(side::Side::BUY)
        .entered_contract(contract::Contract::YES)
        .tick(61)
        .quantity(200)
        .time_in_force(time_in_force::TimeInForce::GTC)
        .expires_at_opt(None)
        .at_event_start(at_event_start::AtEventStart::KEEP)
        .protection_ticks(0)
        .spend_cap_cents_opt(None)
        .entered_stake_cents_opt(None)
        .client_order_group_opt(None);
    entries.client_order_id_zero_padded(b"n-1");

    entries.advance().unwrap();
    entries
        .entry_type(batch_entry_type::BatchEntryType::CANCEL)
        .instrument_id(42)
        .order_id_opt(Some(555))
        .expires_at_opt(None)
        .spend_cap_cents_opt(None)
        .entered_stake_cents_opt(None)
        .client_order_group_opt(None);

    let enc = entries.parent().unwrap();
    let len = enc.get_limit();

    let header = header_of(&buf[..len]);
    assert_eq!(header.template_id(), 5);
    let dec = BatchOrdersDecoder::default().header(header, 0);
    let mut entries = dec.entries_decoder();
    assert_eq!(entries.count(), 2);
    let mut seen = Vec::new();
    while entries.advance().unwrap().is_some() {
        seen.push((entries.entry_type(), entries.order_id(), entries.at_event_start()));
    }
    assert_eq!(
        seen,
        vec![
            (batch_entry_type::BatchEntryType::NEW, None, at_event_start::AtEventStart::KEEP),
            (batch_entry_type::BatchEntryType::CANCEL, Some(555), at_event_start::AtEventStart::CANCEL),
        ]
    );
}

#[test]
fn resolve_event_names_a_state_or_carries_void_weights() {
    let mut buf = vec![0u8; 256];
    let mut enc = ResolveEventEncoder::default().wrap(WriteBuf::new(&mut buf), HDR);
    enc = enc.header(0).parent().unwrap();
    enc.command_id_zero_padded(b"cmd-void-900");
    enc.event_id(900).kind(resolution_kind::ResolutionKind::VOID).case_id(31).winning_state(0);
    let mut weights = enc.void_weights_encoder(3, VoidWeightsEncoder::default());
    for (state, weight) in [(0u8, 1u32), (1, 1), (2, 2)] {
        weights.advance().unwrap();
        weights.state(state).weight(weight);
    }
    let enc = weights.parent().unwrap();
    let len = enc.get_limit();

    let header = header_of(&buf[..len]);
    assert_eq!(header.template_id(), 88);
    let dec = ResolveEventDecoder::default().header(header, 0);
    assert_eq!(dec.kind(), resolution_kind::ResolutionKind::VOID);
    let mut weights = dec.void_weights_decoder();
    let mut total = 0u32;
    while weights.advance().unwrap().is_some() {
        total += weights.weight();
    }
    assert_eq!(total, 4);
}

#[test]
fn transfer_in_carries_opaque_state() {
    let state = b"canonical snapshot of event 900".to_vec();
    let mut buf = vec![0u8; 256];
    let mut enc = TransferInEncoder::default().wrap(WriteBuf::new(&mut buf), HDR);
    enc = enc.header(0).parent().unwrap();
    enc.command_id_zero_padded(b"cmd-move-900");
    enc.event_id(900).source_group(1);
    enc.state(&state);
    let len = enc.get_limit();

    let header = header_of(&buf[..len]);
    assert_eq!(header.template_id(), 95);
    let mut dec = TransferInDecoder::default().header(header, 0);
    assert_eq!(dec.source_group(), 1);
    let coordinates = dec.state_decoder();
    assert_eq!(dec.state_slice(coordinates), state.as_slice());
}

#[test]
fn a_mint_fill_states_cash_that_backs_each_new_pair_with_one_dollar() {
    // 10 contracts at 62 cents between two flat accounts: every unit mints a pair. The buyer pays
    // 62c and the seller posts 38c per contract, so the pool receives $1.00 per pair (blueprint §7).
    let (quantity, price_cents) = (10i64, 62i64);
    let mut buf = vec![0u8; 256];
    let mut enc = FillEncoder::default().wrap(WriteBuf::new(&mut buf), HDR);
    enc = enc.header(0).parent().unwrap();
    enc.trade_id(1).instrument_id(42).tick(62).quantity(quantity as u32)
        .maker_order_id(10).maker_account_id(1).maker_remaining(0)
        .taker_order_id(11).taker_account_id(2).taker_remaining(0)
        .taker_side(side::Side::BUY)
        .minted_pairs(quantity as u32)
        .burned_pairs(0)
        .buyer_cash_cents(-quantity * price_cents)
        .seller_cash_cents(-quantity * (100 - price_cents))
        .maker_fee_cents(0)
        .taker_fee_cents(0);
    let len = enc.get_limit();

    let header = header_of(&buf[..len]);
    assert_eq!(header.template_id(), 102);
    let dec = FillDecoder::default().header(header, 0);
    let pool_change = -(dec.buyer_cash_cents() + dec.seller_cash_cents());
    assert_eq!(pool_change, 100 * (dec.minted_pairs() as i64 - dec.burned_pairs() as i64));
}
