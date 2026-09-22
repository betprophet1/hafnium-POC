//! E-20.2, as far as it can be reached without a decoder: a snapshot does not depend on how the maps are laid out.
//!
//! Feeding the same inputs to engines with different hash seeds and different table sizes rearranges every
//! `FixedMap` — a different seed sends every key to a different home slot, and a different size changes the mask
//! it lands under. Neither is observable in results by construction, so any difference in the snapshot is the
//! encoder writing storage order where it should be writing key order. Checked by mutation: encoding the
//! duplicates as they are stored rather than sorted fails this test.
//!
//! **It does not yet reach the slab.** Both engines here receive the same inputs in the same order, and the order
//! free list starts ascending from zero, so they put every order in the same slot whatever their seed — a leaked
//! slot number passes this test unnoticed, which was confirmed by mutation before the claim was narrowed. Two
//! engines in the same state with genuinely different slots cannot be built by feeding inputs, because any extra
//! work moves the order counter and so changes the state. The way to get one is to restore a snapshot: a restored
//! engine rebuilds every slab from the encoding and its slots differ by construction. That is WP-7's own property
//! test and it arrives with the decoder.

use serde_json::{json, Value};
use venue_engine::snapshot::{Refused, STATE_VERSION};
use venue_engine::state::Capacities;
use venue_engine::{Engine, EngineConfig, FailStop};
use venue_harness::codec;
use venue_harness::memory::OwnedMemory;
use venue_harness::Collect;

const START: u64 = 1_800_000_000_000_000_000;
const SECOND: u64 = 1_000_000_000;

struct Venue<'a> {
    engine: Engine<'a>,
    sequence: u64,
    at: u64,
}

impl Venue<'_> {
    /// Applies an input and insists it was taken.
    ///
    /// `apply` returning `Ok` means only that the engine did not fail-stop: a refused command and a rejected order
    /// are ordinary results. Letting those through is how every test in this file once passed against a venue that
    /// had never come into existence — a tick table refused for an out-of-range complement, and with it the
    /// instrument, the market and every order. An empty engine encodes and restores perfectly.
    fn send(&mut self, after: u64, mut input: Value) -> Result<(), FailStop> {
        self.sequence += 1;
        self.at += after;
        input["record"] = json!({"sequence": self.sequence, "sequencedAt": self.at, "sequencerGroup": 1});
        let mut buf = vec![0u8; 1 << 16];
        let len = codec::encode(&input, &mut buf).expect("the input encodes");
        let mut sink = Collect::default();
        let outcome = self.engine.apply(&buf[..len], &mut sink);
        if outcome.is_ok() {
            for raw in &sink.0 {
                let result = codec::decode(raw).expect("a result decodes");
                let refused = match result["msg"].as_str() {
                    Some("OrderRejected") => result["reason"].as_str().map(str::to_string),
                    Some("CommandApplied") if result["outcome"].as_str() != Some("APPLIED") => {
                        Some(format!("{}", result["outcome"]))
                    }
                    _ => None,
                };
                if let Some(reason) = refused {
                    panic!("the venue refused {}: {reason}", input["msg"]);
                }
            }
        }
        outcome
    }

    fn snapshot(&mut self) -> Vec<u8> {
        let mut out = vec![0u8; 1 << 20];
        let len = self.engine.snapshot(&mut out).expect("the snapshot fits");
        out.truncate(len);
        out
    }
}

fn venue(owned: &mut OwnedMemory, seed: u64) -> Venue<'_> {
    let config = EngineConfig { sequencer_group: 1, binary_hash: [0; 32], table_seed: seed };
    Venue { engine: Engine::new(config, owned.memory()).expect("an engine"), sequence: 0, at: START }
}

/// Everything an order needs to exist: the ladder, the events, the markets and the money.
fn setup(v: &mut Venue<'_>) {
    let table = json!({"msg": "DefineTickTable", "commandId": "t1", "tickTableId": 1,
                       "ticks": [{"tick": 0, "numerator": 25, "denominator": 100, "complement": 2},
                                 {"tick": 1, "numerator": 50, "denominator": 100, "complement": 1},
                                 {"tick": 2, "numerator": 75, "denominator": 100, "complement": 0}]});
    v.send(1, table).expect("a tick table");

    for (event, instruments) in [(10u64, [100u64, 101]), (20, [200, 201])] {
        v.send(
            1,
            json!({"msg": "DefineEvent", "commandId": format!("e{event}"), "eventId": event,
                         "stateCount": 2, "scheduledStart": START + 3600 * SECOND}),
        )
        .expect("an event");
        for id in instruments {
            v.send(
                1,
                json!({"msg": "ListInstrument", "commandId": format!("i{id}"), "instrumentId": id,
                             "eventId": event, "instrumentClass": "BINARY", "stateMask": 2, "tickTableId": 1,
                             "lotStep": 1, "allocationPolicy": "PRICE_TIME", "positionLimit": 0, "legs": []}),
            )
            .expect("an instrument");
            v.send(
                1,
                json!({"msg": "SetMarketState", "commandId": format!("o{id}"), "instrumentId": id,
                             "state": "OPEN"}),
            )
            .expect("a market opens");
        }
    }

    // Money, so orders can rest.
    for account in [1u64, 2, 3] {
        v.send(
            1,
            json!({"msg": "SleeveAdjust", "commandId": format!("s{account}"), "accountId": account,
                         "deltaCents": 1_000_000, "reason": "GRANT"}),
        )
        .expect("a sleeve");
    }

    v.send(1, json!({"msg": "SetFeeSchedule", "commandId": "f1", "makerRateMicros": -50, "takerRateMicros": 200}))
        .expect("a fee schedule");
    v.send(
        1,
        json!({"msg": "SetFeeOverride", "commandId": "f2", "overrideId": 7, "priority": 3, "accountId": 2,
                     "instrumentId": 0, "eventId": 0, "makerRateMicros": 0, "takerRateMicros": 100,
                     "expiresAt": null}),
    )
    .expect("a fee override");
    v.send(
        1,
        json!({"msg": "SetRuleLimits", "commandId": "r1", "accountId": 1, "maxRestingOrders": 50,
                     "maxNewOrdersPerSecond": 10, "maxOrderToTradeRatioMilli": 0, "ratioFreeOrdersPerSecond": 0}),
    )
    .expect("rule limits");
    v.send(
        1,
        json!({"msg": "SetMakerProtection", "commandId": "m1", "accountId": 2, "eventId": 10,
                     "maxContractsFilledPerWindow": 5, "windowMillis": 1000}),
    )
    .expect("maker protection");
}

/// The orders that stay, and the state that hides in corners around them.
fn orders(v: &mut Venue<'_>) {
    // Orders at several ticks, on both sides, in an order that gives the queues something to preserve.
    let mut n = 0;
    for (account, instrument, side, tick) in [
        (1u64, 100u64, "BUY", 0u16),
        (2, 100, "BUY", 0),
        (3, 100, "BUY", 1),
        (1, 100, "SELL", 2),
        (2, 101, "BUY", 1),
        (3, 201, "SELL", 2),
        (1, 200, "BUY", 0),
    ] {
        n += 1;
        v.send(
            1,
            json!({"msg": "NewOrder", "accountId": account, "clientOrderId": format!("c{n}"),
                         "instrumentId": instrument, "side": side, "tick": tick, "quantity": 10}),
        )
        .expect("an order rests");
    }

    // One that expires, so the GTD records have something live in them.
    v.send(
        1,
        json!({"msg": "NewOrder", "accountId": 2, "clientOrderId": "gtd1", "instrumentId": 101,
                     "side": "BUY", "tick": 0, "quantity": 5, "timeInForce": "GTD",
                     "expiresAt": START + 7200 * SECOND}),
    )
    .expect("a GTD order rests");

    v.send(
        1,
        json!({"msg": "SetAccountControls", "commandId": "a1", "accountId": 3, "closedOnly": "TRUE",
                     "selfMatchPrevention": "CANCEL_RESTING"}),
    )
    .expect("an account mode");
    v.send(SECOND, json!({"msg": "TimeTick"})).expect("a tick");
}

/// A venue, with no churn: what most of these tests want.
fn build(v: &mut Venue<'_>) {
    setup(v);
    orders(v);
}

/// The same venue, built into engines with different seeds and different tables, snapshots identically.
#[test]
fn a_snapshot_does_not_depend_on_how_the_maps_are_laid_out() {
    let mut small = OwnedMemory::new(&Capacities::small());
    let mut one = venue(&mut small, 0x5eed);
    build(&mut one);
    let first = one.snapshot();

    // A different seed rearranges every map and a different size changes the mask each hashes under. Sizes stay
    // within the rules `Engine::new` checks. Note what this does *not* vary: both engines saw the same inputs, so
    // their order slabs are filled identically and this says nothing about slot numbers (see the module comment).
    let mut other = OwnedMemory::new(&Capacities {
        orders: 4_096,
        duplicates: 1_024,
        commands: 512,
        accounts: 512,
        holdings: 2_048,
        ..Capacities::small()
    });
    let mut two = venue(&mut other, 0x_d1ff_e5ee);
    build(&mut two);
    let second = two.snapshot();

    assert!(!first.is_empty(), "the snapshot is not empty");
    assert_eq!(
        first, second,
        "two engines holding the same venue wrote different snapshots: the encoder is leaking its layout"
    );
}

/// Taking a snapshot does not change what the engine does next, and taking it twice gives the same bytes.
#[test]
fn taking_a_snapshot_changes_nothing() {
    let mut owned = OwnedMemory::new(&Capacities::small());
    let mut v = venue(&mut owned, 0x5eed);
    build(&mut v);

    let before = v.snapshot();
    let again = v.snapshot();
    assert_eq!(before, again, "snapshotting twice gave different bytes");

    // Carry on, and the engine is none the wiser for having been read.
    v.send(SECOND, json!({"msg": "TimeTick"})).expect("a tick after the snapshot");
    let after = v.snapshot();
    assert_ne!(before, after, "the clock moved, so the snapshot should have");
}

/// An engine that has fail-stopped must not be snapshotted: its state is the state at which processing stopped
/// (E-20.1), and a snapshot of it would be an invitation to continue from there.
#[test]
fn a_stopped_engine_refuses_to_be_snapshotted() {
    let mut owned = OwnedMemory::new(&Capacities::small());
    let mut v = venue(&mut owned, 0x5eed);
    build(&mut v);

    // Out of sequence: E-4.1 fail-stop.
    v.sequence += 10;
    let stopped = v.send(1, json!({"msg": "TimeTick"}));
    assert_eq!(stopped, Err(FailStop::SequenceOrTime), "the engine should have fail-stopped");

    let mut out = vec![0u8; 1 << 20];
    assert_eq!(v.engine.snapshot(&mut out), Err(FailStop::Stopped), "a stopped engine was snapshotted");
}

/// A buffer too small is answered, not overrun.
#[test]
fn a_snapshot_that_does_not_fit_is_refused() {
    let mut owned = OwnedMemory::new(&Capacities::small());
    let mut v = venue(&mut owned, 0x5eed);
    build(&mut v);
    let mut tiny = [0u8; 8];
    assert_eq!(v.engine.snapshot(&mut tiny), Err(FailStop::Capacity), "a short buffer was written past");
}

/// Churn: orders placed and cancelled, so the order free list is permuted and later orders land in reused slots.
/// Without it, an engine fed inputs fills its slab ascending and a restored engine's slabs come out identical by
/// accident rather than by the encoding — which is exactly how the test above passed a deliberately leaked slot
/// number.
fn churn(v: &mut Venue<'_>) {
    // Order IDs are assigned by the exchange: the group in the high bits, then a counter from 1 (E-2.8).
    let id = |n: u64| (1u64 << 48) + n;
    let mut placed = 0u64;
    for round in 0..6u64 {
        v.send(
            1,
            json!({"msg": "NewOrder", "accountId": 1, "clientOrderId": format!("churn{round}"),
                   "instrumentId": 100, "side": "BUY", "tick": 2, "quantity": 1}),
        )
        .expect("a churn order rests");
        placed += 1;
        v.send(1, json!({"msg": "CancelOrder", "accountId": 1, "instrumentId": 100, "orderId": id(placed)}))
            .expect("the churn order is cancelled");
    }
}

/// E-20.9, the half a decoder makes reachable: a snapshot restored into an engine built differently, and laid out
/// differently, describes the same venue — so snapshotting it again gives the same bytes.
///
/// This is the test the encoder alone could not have. A restored engine rebuilds every slab from the encoding, so
/// its slots, its free list and its pool offsets are its own.
#[test]
fn a_snapshot_restored_into_a_different_engine_describes_the_same_venue() {
    let mut source = OwnedMemory::new(&Capacities::small());
    let mut one = venue(&mut source, 0x5eed);
    setup(&mut one);
    churn(&mut one);
    orders(&mut one);
    let first = one.snapshot();

    let mut other = OwnedMemory::new(&Capacities {
        orders: 4_096,
        duplicates: 1_024,
        commands: 512,
        accounts: 512,
        holdings: 2_048,
        ..Capacities::small()
    });
    let config = EngineConfig { sequencer_group: 1, binary_hash: [0; 32], table_seed: 0x_d1ff_e5ee };
    let mut restored = Engine::new(config, other.memory()).expect("an engine");
    restored.restore(STATE_VERSION, &first).expect("the snapshot restores");

    let mut out = vec![0u8; 1 << 20];
    let len = restored.snapshot(&mut out).expect("the restored engine snapshots");
    out.truncate(len);
    assert_eq!(first, out, "a restored engine described a different venue from the one it was given");
}

/// A restored engine carries on where the snapshot left off: E-4.1 wants the next sequence, not sequence 1.
#[test]
fn a_restored_engine_carries_on_from_the_sequence_it_was_given() {
    let mut source = OwnedMemory::new(&Capacities::small());
    let mut one = venue(&mut source, 0x5eed);
    build(&mut one);
    let taken = one.snapshot();
    let at = one.sequence;
    let when = one.at;

    let mut other = OwnedMemory::new(&Capacities::small());
    let config = EngineConfig { sequencer_group: 1, binary_hash: [0; 32], table_seed: 0x5eed };
    let mut engine = Engine::new(config, other.memory()).expect("an engine");
    engine.restore(STATE_VERSION, &taken).expect("the snapshot restores");
    assert_eq!(engine.position().0, at, "the restored engine is not at the snapshot's sequence");

    // The same input applied to both must leave them describing the same venue.
    let mut two = Venue { engine, sequence: at, at: when };
    one.send(SECOND, json!({"msg": "TimeTick"})).expect("a tick on the original");
    two.send(SECOND, json!({"msg": "TimeTick"})).expect("a tick on the restored");
    assert_eq!(one.snapshot(), two.snapshot(), "the two diverged on the first input after the snapshot");
}

/// A snapshot is restored into a fresh engine, never over the top of state already there (E-20.8).
#[test]
fn a_used_engine_refuses_a_snapshot() {
    let mut source = OwnedMemory::new(&Capacities::small());
    let mut one = venue(&mut source, 0x5eed);
    build(&mut one);
    let taken = one.snapshot();
    assert_eq!(one.engine.restore(STATE_VERSION, &taken), Err(Refused::NotFresh));
}

/// An encoding this build does not implement is refused rather than guessed at (E-20.8).
#[test]
fn an_unknown_state_version_is_refused() {
    let mut owned = OwnedMemory::new(&Capacities::small());
    let config = EngineConfig { sequencer_group: 1, binary_hash: [0; 32], table_seed: 0x5eed };
    let mut engine = Engine::new(config, owned.memory()).expect("an engine");
    let next = STATE_VERSION + 1;
    assert_eq!(engine.restore(next, &[]), Err(Refused::Version(next)));
}

/// Bytes that stop part-way are refused, not half-applied.
#[test]
fn a_truncated_snapshot_is_refused() {
    let mut source = OwnedMemory::new(&Capacities::small());
    let mut one = venue(&mut source, 0x5eed);
    build(&mut one);
    let taken = one.snapshot();

    let mut owned = OwnedMemory::new(&Capacities::small());
    let config = EngineConfig { sequencer_group: 1, binary_hash: [0; 32], table_seed: 0x5eed };
    let mut engine = Engine::new(config, owned.memory()).expect("an engine");
    let short = &taken[..taken.len() / 2];
    assert!(matches!(engine.restore(STATE_VERSION, short), Err(Refused::Truncated | Refused::Malformed)));
}
