// Reading a feed unit's results through `serde_json::Value` against reading them typed, for ADR-0002.
//
// The gateway's feed thread is what now caps the venue: one thread per half applies every unit, and profiling
// put roughly nine tenths of it in representing the data rather than in the work -- allocating, comparing JSON
// object keys, building and dropping `Value` trees. The representation is not there for the gateway's benefit.
// `venue_harness::codec` exists so the hand-computed scenarios in spec/engine/scenarios can be diffed against
// what the engine emits, and `venue-link` reuses it on the feed path because it was there.
//
// Underneath that codec are the typed SBE decoders in core/journal, generated from the same schema. They are
// already how `codec::decode` reads the bytes -- it reads a field and then puts it in a `Map` under a `String`
// key. So this measures the two ways of getting the same fields out of the same bytes:
//
//   value: codec::decode(frame) -> Value, then read the fields by string key, as View::apply/Hub::apply do
//   typed: the generated decoder for the template, reading the same fields straight out
//
// Nothing here is a view: both paths just read the fields and sum them, so what is timed is getting at the
// data, not what is done with it. That is deliberate -- the work either way is the same, and the question is
// only what the representation costs on top.

use serde_json::{json, Value};

/// What the gateway uses, so the `Value` path is not measured against an allocator it no longer runs.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::hint::black_box;
use std::time::Instant;
use venue_harness::codec;
use venue_journal::*;

/// The three results a trading feed is mostly made of: an order resting, a fill against it, and the book level
/// that moved. Built once through the JSON codec's encoder, so both paths read bytes the engine would emit.
fn frames() -> Vec<Vec<u8>> {
    let messages = [
        json!({
            "msg": "OrderAccepted",
            "result": {"inputSequence": 8123, "resultIndex": 0, "sequencedAt": 1_727_000_000_000_000_000u64,
                       "sequencerGroup": 1},
            "orderId": 900_123, "accountId": 9_600_007, "instrumentId": 9_500_000, "side": "BUY",
            "tick": 50, "quantity": 1_000, "reservedCents": 50_000
        }),
        json!({
            "msg": "Fill",
            "result": {"inputSequence": 8124, "resultIndex": 1, "sequencedAt": 1_727_000_000_000_000_001u64,
                       "sequencerGroup": 1},
            "tradeId": 55_001, "instrumentId": 9_500_000, "tick": 50, "quantity": 7,
            "makerOrderId": 900_123, "makerAccountId": 9_600_007, "makerRemaining": 993,
            "takerOrderId": 900_124, "takerAccountId": 9_600_008
        }),
        json!({
            "msg": "BookLevelChanged",
            "result": {"inputSequence": 8124, "resultIndex": 2, "sequencedAt": 1_727_000_000_000_000_001u64,
                       "sequencerGroup": 1},
            "instrumentId": 9_500_000, "side": "BUY", "tick": 50, "quantity": 993, "orderCount": 1
        }),
    ];
    messages
        .iter()
        .map(|m| {
            let mut buf = vec![0_u8; 4096];
            let n = codec::encode(m, &mut buf).expect("the codec encodes what it decodes");
            buf.truncate(n);
            buf
        })
        .collect()
}

/// What the feed path does today: inflate the frame into a `Value`, then pull the fields out by string key.
/// The keys are the ones `View::apply` and `Hub::apply` actually read for these three messages.
fn through_value(frame: &[u8]) -> u64 {
    let v = codec::decode(frame).expect("a frame the codec wrote");
    let u = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
    let mut sum = v["result"]["inputSequence"].as_u64().unwrap_or(0);
    sum = sum.wrapping_add(u("instrumentId")).wrapping_add(u("tick")).wrapping_add(u("quantity"));
    match v["msg"].as_str().unwrap_or("") {
        "OrderAccepted" => sum.wrapping_add(u("orderId")).wrapping_add(u("accountId")),
        "Fill" => sum
            .wrapping_add(u("makerOrderId"))
            .wrapping_add(u("makerAccountId"))
            .wrapping_add(u("makerRemaining"))
            .wrapping_add(u("takerOrderId"))
            .wrapping_add(u("takerAccountId")),
        "BookLevelChanged" => sum.wrapping_add(u("orderCount")),
        _ => sum,
    }
}

/// The same fields, read straight out of the generated decoder for the template. No map, no keys, no strings.
fn through_typed(frame: &[u8]) -> u64 {
    let header = message_header_codec::MessageHeaderDecoder::default().wrap(ReadBuf::new(frame), 0);
    match header.template_id() {
        order_accepted_codec::SBE_TEMPLATE_ID => {
            let mut dec = order_accepted_codec::OrderAcceptedDecoder::default().header(header, 0);
            let mut c = dec.result_decoder();
            let sum = c.input_sequence();
            dec = c.parent().expect("the result group");
            sum.wrapping_add(dec.instrument_id())
                .wrapping_add(u64::from(dec.tick()))
                .wrapping_add(u64::from(dec.quantity()))
                .wrapping_add(dec.order_id())
                .wrapping_add(dec.account_id())
        }
        fill_codec::SBE_TEMPLATE_ID => {
            let mut dec = fill_codec::FillDecoder::default().header(header, 0);
            let mut c = dec.result_decoder();
            let sum = c.input_sequence();
            dec = c.parent().expect("the result group");
            sum.wrapping_add(dec.instrument_id())
                .wrapping_add(u64::from(dec.tick()))
                .wrapping_add(u64::from(dec.quantity()))
                .wrapping_add(dec.maker_order_id())
                .wrapping_add(dec.maker_account_id())
                .wrapping_add(u64::from(dec.maker_remaining()))
                .wrapping_add(dec.taker_order_id())
                .wrapping_add(dec.taker_account_id())
        }
        book_level_changed_codec::SBE_TEMPLATE_ID => {
            let mut dec = book_level_changed_codec::BookLevelChangedDecoder::default().header(header, 0);
            let mut c = dec.result_decoder();
            let sum = c.input_sequence();
            dec = c.parent().expect("the result group");
            sum.wrapping_add(dec.instrument_id())
                .wrapping_add(u64::from(dec.tick()))
                .wrapping_add(u64::from(dec.quantity()))
                .wrapping_add(u64::from(dec.order_count()))
        }
        other => panic!("the prototype does not build template {other}"),
    }
}

fn time(name: &str, frames: &[Vec<u8>], rounds: u32, f: impl Fn(&[u8]) -> u64) -> f64 {
    // Once through first, so neither path is measured cold.
    // Both sides go through `black_box`: the typed path allocates nothing and the frames never change, so
    // without it the optimiser folds most of that path away and the comparison flatters it by orders.
    let mut sink = 0_u64;
    for frame in frames {
        sink = sink.wrapping_add(f(black_box(frame)));
    }
    let started = Instant::now();
    for _ in 0..rounds {
        for frame in frames {
            sink = sink.wrapping_add(black_box(f(black_box(frame))));
        }
    }
    let each = started.elapsed().as_nanos() as f64 / f64::from(rounds) / frames.len() as f64;
    println!("  {name:<6} {each:8.1} ns a result   (checksum {sink})");
    each
}

fn main() {
    let frames = frames();
    let rounds = 200_000;
    println!("reading {} results {rounds} times each, three shapes a trading feed is made of:", frames.len());
    let value = time("value", &frames, rounds, through_value);
    let typed = time("typed", &frames, rounds, through_typed);
    println!();
    println!("typed is {:.1}x faster, saving {:.1} ns a result", value / typed, value - typed);
    // A unit carries several results, so the per-unit saving is this times however many it has: the gateway
    // measured about 45us an amend unit and 36us at the mean under trading load (ADR-0002).
    println!("at 10 results a unit that is {:.1} us a unit", (value - typed) * 10.0 / 1000.0);
}
