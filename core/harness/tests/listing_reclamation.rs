//! E-8.4 and E-18.3 where they cannot be seen in results: storage a release frees must be reusable by any later
//! listing, which with a bump-allocated level pool means compacting it. These run the engine with room for three
//! books and list, trade, resolve and release far more than that, on tick tables of different lengths so that the
//! holes releases leave do not fit what is listed next.

use serde_json::{json, Value};
use venue_engine::state::Capacities;
use venue_engine::{Engine, EngineConfig, FailStop};
use venue_harness::codec;
use venue_harness::memory::OwnedMemory;
use venue_harness::Collect;

const START: u64 = 1_800_000_000_000_000_000;
const SECOND: u64 = 1_000_000_000;
/// Tick table 1 has 99 rows (198 levels a book), table 2 has 9 (18 levels).
const CENTS: u64 = 1;
const TENTHS: u64 = 2;

struct Venue<'a> {
    engine: Engine<'a>,
    sequence: u64,
    at: u64,
    commands: u64,
}

impl Venue<'_> {
    /// Applies `input` `after` nanoseconds after the previous one and returns its results without their headers.
    fn send(&mut self, after: u64, mut input: Value) -> Result<Vec<Value>, FailStop> {
        self.sequence += 1;
        self.at += after;
        input["record"] = json!({"sequence": self.sequence, "sequencedAt": self.at, "sequencerGroup": 1});
        if input.get("commandId").is_some_and(Value::is_null) {
            self.commands += 1;
            input["commandId"] = json!(format!("c{}", self.commands));
        }
        let mut buf = vec![0u8; 1 << 16];
        let len = codec::encode(&input, &mut buf).expect("the input encodes");
        let mut sink = Collect::default();
        self.engine.apply(&buf[..len], &mut sink)?;
        Ok(sink
            .0
            .iter()
            .map(|r| {
                let mut v = codec::decode(r).expect("a result decodes");
                v.as_object_mut().expect("an object").remove("result");
                v
            })
            .collect())
    }

    fn command(&mut self, mut input: Value) -> Vec<Value> {
        input["commandId"] = Value::Null;
        let msg = input["msg"].clone();
        self.send(1_000, input).unwrap_or_else(|e| panic!("{msg}: fail-stop {e:?}"))
    }

    fn applied(&mut self, input: Value) -> Vec<Value> {
        let msg = input["msg"].clone();
        let results = self.command(input);
        assert_eq!(results[0]["outcome"], "APPLIED", "{msg}: {results:?}");
        results
    }

    fn table(&mut self, id: u64, numerators: &[u64], denominator: u64) {
        let last = numerators.len() as u64 - 1;
        let ticks: Vec<Value> = numerators
            .iter()
            .enumerate()
            .map(|(t, n)| json!({"tick": t, "numerator": n, "denominator": denominator, "complement": last - t as u64}))
            .collect();
        self.applied(json!({"msg": "DefineTickTable", "tickTableId": id, "ticks": ticks}));
    }

    /// Defines event `event` with one binary book `event × 10` on `table`, open.
    fn list(&mut self, event: u64, table: u64) -> Vec<Value> {
        self.applied(json!({"msg": "DefineEvent", "eventId": event, "stateCount": 2, "scheduledStart": 0}));
        let results = self.command(json!({"msg": "ListInstrument", "instrumentId": event * 10, "eventId": event,
            "instrumentClass": "BINARY", "tickTableId": table, "lotStep": 1, "allocationPolicy": "PRICE_TIME",
            "stateMask": 2, "positionLimit": 0, "legs": []}));
        if results[0]["outcome"] == "APPLIED" {
            self.applied(json!({"msg": "SetMarketState", "instrumentId": event * 10, "state": "OPEN"}));
        }
        results
    }

    fn resolve(&mut self, event: u64) {
        self.applied(json!({"msg": "ResolveEvent", "eventId": event, "kind": "RESOLVED", "caseId": 0,
                            "winningState": 1, "voidWeights": []}));
    }

    fn tick(&mut self, after: u64) -> Vec<Value> {
        self.send(after, json!({"msg": "TimeTick"})).unwrap_or_else(|e| panic!("tick: fail-stop {e:?}"))
    }

    fn order(&mut self, account: u64, instrument: u64, side: &str, tick: u64, quantity: u64, tif: &str) -> Vec<Value> {
        self.commands += 1;
        let order = json!({"msg": "NewOrder", "accountId": account, "sessionId": account * 10,
                           "clientOrderId": format!("o{}", self.commands), "instrumentId": instrument, "side": side,
                           "tick": tick, "quantity": quantity, "timeInForce": tif, "expiresAt": null,
                           "atEventStart": "KEEP", "enteredContract": "YES", "flags": [], "protectionTicks": 0,
                           "spendCapCents": null, "enteredStakeCents": null, "clientOrderGroup": null});
        self.send(1_000, order).unwrap_or_else(|e| panic!("order: fail-stop {e:?}"))
    }
}

fn fills(results: &[Value]) -> Vec<(u64, u64)> {
    results
        .iter()
        .filter(|r| r["msg"] == "Fill")
        .map(|r| (r["tick"].as_u64().expect("tick"), r["quantity"].as_u64().expect("quantity")))
        .collect()
}

/// Room for three books, two of them on the 99-row table and one on the 9-row table.
fn tiny() -> OwnedMemory {
    OwnedMemory::new(&Capacities { events: 4, instruments: 3, levels: 2 * 99 * 2 + 2 * 9, ..Capacities::small() })
}

fn venue(owned: &mut OwnedMemory) -> Venue<'_> {
    let config = EngineConfig { sequencer_group: 1, binary_hash: [0; 32], table_seed: 7 };
    let engine = Engine::new(config, owned.memory()).expect("the engine builds");
    let mut v = Venue { engine, sequence: 0, at: START, commands: 0 };
    let cents: Vec<u64> = (1..=99).collect();
    v.table(CENTS, &cents, 100);
    v.table(TENTHS, &[10, 20, 30, 40, 50, 60, 70, 80, 90], 100);
    for account in [1, 2] {
        v.applied(json!({"msg": "SleeveAdjust", "accountId": account, "deltaCents": 1_000_000, "reason": "GRANT"}));
    }
    v.applied(json!({"msg": "SetListingParameters", "settledRetentionSeconds": 1, "maxEvents": 0,
                     "maxInstruments": 0, "maxPriceLevels": 0}));
    v
}

#[test]
fn books_keep_their_orders_when_a_listing_compacts_the_pool() {
    let mut owned = tiny();
    let mut v = venue(&mut owned);
    v.list(1, CENTS);
    v.list(2, TENTHS);
    v.list(3, CENTS);
    // Resting orders on both sides of the book that will move, and on the small one.
    v.order(1, 30, "BUY", 10, 5, "GTC");
    v.order(2, 30, "SELL", 89, 7, "GTC");
    v.order(1, 20, "BUY", 3, 2, "GTC");
    v.resolve(1);
    let released = v.tick(SECOND);
    assert!(released.iter().any(|r| r["msg"] == "EventReleased" && r["eventId"] == 1), "{released:?}");
    // 198 levels at the end no longer fit, but 198 are free at the front: the pool is compacted, not exhausted.
    let listed = v.list(4, CENTS);
    assert_eq!(listed[0]["outcome"], "APPLIED", "{listed:?}");
    // Every book still has what rested on it, at the ticks it rested at.
    assert_eq!(fills(&v.order(2, 30, "SELL", 10, 5, "IOC")), vec![(10, 5)]);
    assert_eq!(fills(&v.order(1, 30, "BUY", 98, 7, "IOC")), vec![(89, 7)]);
    assert_eq!(fills(&v.order(2, 20, "SELL", 0, 2, "IOC")), vec![(3, 2)]);
    // The new book trades too.
    v.order(1, 40, "BUY", 50, 3, "GTC");
    assert_eq!(fills(&v.order(2, 40, "SELL", 50, 3, "IOC")), vec![(50, 3)]);
}

#[test]
fn a_calendar_of_rounds_runs_indefinitely_on_three_books() {
    let mut owned = tiny();
    let mut v = venue(&mut owned);
    v.list(1, CENTS);
    v.list(2, TENTHS);
    for (oldest, event) in (1..).zip(3..200) {
        // Each round lists on the other table from the last, so holes rarely fit what comes next.
        let table = if event % 3 == 0 { TENTHS } else { CENTS };
        let listed = v.list(event, table);
        assert_eq!(listed[0]["outcome"], "APPLIED", "round {event}: {listed:?}");
        v.order(1, event * 10, "BUY", 2, 1, "GTC");
        v.resolve(oldest);
        let released = v.tick(SECOND);
        assert!(released.iter().any(|r| r["msg"] == "EventReleased" && r["eventId"] == oldest), "{released:?}");
    }
}

#[test]
fn limits_refuse_what_storage_could_not_hold() {
    let mut owned = tiny();
    let mut v = venue(&mut owned);
    v.applied(json!({"msg": "SetListingParameters", "settledRetentionSeconds": 1, "maxEvents": 4,
                     "maxInstruments": 3, "maxPriceLevels": 2 * 99 * 2 + 2 * 9}));
    assert_eq!(v.list(1, CENTS)[0]["outcome"], "APPLIED");
    assert_eq!(v.list(2, CENTS)[0]["outcome"], "APPLIED");
    let refused = v.list(3, CENTS);
    assert_eq!(refused[0]["rejectReason"], "LISTING_CAPACITY", "{refused:?}");
    assert_eq!(v.list(4, TENTHS)[0]["outcome"], "APPLIED");
    let refused = v.command(json!({"msg": "DefineEvent", "eventId": 5, "stateCount": 2, "scheduledStart": 0}));
    assert_eq!(refused[0]["rejectReason"], "LISTING_CAPACITY", "{refused:?}");
}
