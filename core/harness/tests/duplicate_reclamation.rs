//! E-8.2 reclamation of client order IDs, where it cannot be seen in results: an entry that is never removed only
//! shows up as the table filling and the engine fail-stopping. So these run the engine with a table of eight entries
//! and count on it to keep making room.

use serde_json::{json, Value};
use venue_engine::state::Capacities;
use venue_engine::{Engine, EngineConfig, FailStop};
use venue_harness::codec;
use venue_harness::memory::OwnedMemory;
use venue_harness::Collect;

const START: u64 = 1_800_000_000_000_000_000;
const SECOND: u64 = 1_000_000_000;
const ROUNDS: u64 = 40;

struct Venue<'a> {
    engine: Engine<'a>,
    sequence: u64,
    at: u64,
}

impl Venue<'_> {
    /// Applies `input` `after` nanoseconds after the previous one.
    fn send(&mut self, after: u64, mut input: Value) -> Result<(), FailStop> {
        self.sequence += 1;
        self.at += after;
        input["record"] = json!({"sequence": self.sequence, "sequencedAt": self.at, "sequencerGroup": 1});
        let mut buf = vec![0u8; 1 << 16];
        let len = codec::encode(&input, &mut buf).expect("the input encodes");
        self.engine.apply(&buf[..len], &mut Collect::default())
    }

    fn window(&mut self, id: &str, seconds: u64) -> Result<(), FailStop> {
        let set = json!({"msg": "SetGroupParameters", "commandId": id, "activeOrderLimit": 0,
                         "clientOrderIdWindowSeconds": seconds});
        self.send(1, set)
    }

    /// Records a client order ID. The instrument does not exist, so the order is rejected after its ID is recorded
    /// (E-9.2 records from check 3 on; check 4 rejects it), which needs no market to be set up.
    fn record(&mut self, after: u64, client_order_id: &str) -> Result<(), FailStop> {
        let order = json!({"msg": "NewOrder", "accountId": 1, "sessionId": 1, "clientOrderId": client_order_id,
                           "instrumentId": 999, "side": "BUY", "tick": 1, "quantity": 1, "timeInForce": "GTC",
                           "expiresAt": null, "atEventStart": "KEEP", "enteredContract": "YES", "flags": [],
                           "protectionTicks": 0, "spendCapCents": null, "enteredStakeCents": null,
                           "clientOrderGroup": null});
        self.send(after, order)
    }

    fn tick(&mut self, after: u64) -> Result<(), FailStop> {
        self.send(after, json!({"msg": "TimeTick"}))
    }
}

fn tiny() -> OwnedMemory {
    OwnedMemory::new(&Capacities { duplicates: 8, ..Capacities::small() })
}

fn venue(owned: &mut OwnedMemory) -> Venue<'_> {
    let config = EngineConfig { sequencer_group: 1, binary_hash: [0; 32], table_seed: 7 };
    let engine = Engine::new(config, owned.memory()).expect("the engine builds");
    Venue { engine, sequence: 0, at: START }
}

#[test]
fn a_steady_window_keeps_making_room() {
    let mut owned = tiny();
    let mut v = venue(&mut owned);
    v.window("p1", 1).expect("window set");
    for i in 0..ROUNDS {
        v.record(2 * SECOND, &format!("short-{i}")).unwrap_or_else(|e| panic!("round {i}: {e:?}"));
        v.tick(2 * SECOND).unwrap_or_else(|e| panic!("round {i}: {e:?}"));
    }
}

#[test]
fn once_the_longer_lived_entries_have_gone_it_keeps_making_room() {
    let mut owned = tiny();
    let mut v = venue(&mut owned);
    v.window("p1", 100_000).expect("window set");
    v.record(1, "long").expect("recorded");
    v.window("p2", 1).expect("window shortened");
    for i in 0..ROUNDS / 2 {
        v.record(2 * SECOND, &format!("before-{i}")).unwrap_or_else(|e| panic!("round {i}: {e:?}"));
        v.tick(2 * SECOND).unwrap_or_else(|e| panic!("round {i}: {e:?}"));
    }
    // Past the long entry's expiry: the engine goes back to keeping the records in recording order.
    v.tick(100_000 * SECOND).expect("ticked past the long window");
    for i in 0..ROUNDS {
        v.record(2 * SECOND, &format!("after-{i}")).unwrap_or_else(|e| panic!("round {i}: {e:?}"));
        v.tick(2 * SECOND).unwrap_or_else(|e| panic!("round {i}: {e:?}"));
    }
}

#[test]
fn shortening_the_window_keeps_making_room() {
    let mut owned = tiny();
    let mut v = venue(&mut owned);
    v.window("p1", 100_000).expect("window set");
    // Recorded under the long window, so it expires long after everything recorded below.
    v.record(1, "long").expect("recorded");
    v.window("p2", 1).expect("window shortened");
    for i in 0..ROUNDS {
        v.record(2 * SECOND, &format!("short-{i}")).unwrap_or_else(|e| panic!("round {i}: {e:?}"));
        v.tick(2 * SECOND).unwrap_or_else(|e| panic!("round {i}: {e:?}"));
    }
}
