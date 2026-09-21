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
    fn send(&mut self, after: u64, mut input: Value) -> Result<(), FailStop> {
        self.sequence += 1;
        self.at += after;
        input["record"] = json!({"sequence": self.sequence, "sequencedAt": self.at, "sequencerGroup": 1});
        let mut buf = vec![0u8; 1 << 16];
        let len = codec::encode(&input, &mut buf).expect("the input encodes");
        self.engine.apply(&buf[..len], &mut Collect::default())
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

/// A venue with two events, four markets and a book on each, plus the state that hides in corners: fee overrides,
/// rule limits, an order group, a maker setting, closed-only accounts, a GTD order and spent client order IDs.
fn build(v: &mut Venue<'_>) {
    let table = json!({"msg": "DefineTickTable", "commandId": "t1", "tickTableId": 1,
                       "ticks": [{"tick": 0, "numerator": 1, "denominator": 100, "complement": 98},
                                 {"tick": 1, "numerator": 2, "denominator": 100, "complement": 97},
                                 {"tick": 2, "numerator": 50, "denominator": 100, "complement": 49}]});
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
    let mut two = venue(&mut other, 0xd1ffe5eed);
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
