//! WP-8: a journal written by the simulated sequencer replays to the same result log every time.

use serde_json::{json, Value};
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::Path;
use venue_harness::codec;
use venue_node::frames::{read_frame, write_frame};
use venue_node::node::replay;
use venue_node::sequencer::{self, Sequencer};

fn zeroed() -> Value {
    json!({"sequence": 0, "sequencedAt": 0, "sequencerGroup": 0})
}

fn command(msg: &str, id: &str, mut fields: Value) -> Value {
    fields["msg"] = json!(msg);
    fields["commandId"] = json!(id);
    fields["record"] = zeroed();
    fields
}

fn order(account: u64, client_order_id: &str, side: &str, tick: u16, quantity: u32) -> Value {
    json!({
        "msg": "NewOrder", "record": zeroed(), "sessionId": account * 10, "accountId": account,
        "clientOrderId": client_order_id, "instrumentId": 1000, "side": side, "enteredContract": "YES",
        "tick": tick, "quantity": quantity, "timeInForce": "GTC", "expiresAt": null, "atEventStart": "CANCEL",
        "flags": [], "protectionTicks": 0, "spendCapCents": null, "enteredStakeCents": null, "clientOrderGroup": null
    })
}

fn inputs() -> Vec<Value> {
    let ticks: Vec<Value> =
        (0..99).map(|t| json!({"tick": t, "numerator": t + 1, "denominator": 100, "complement": 98 - t})).collect();
    vec![
        command("DefineTickTable", "c1", json!({"tickTableId": 1, "ticks": ticks})),
        command("DefineEvent", "c2", json!({"eventId": 100, "stateCount": 2, "scheduledStart": 0})),
        command(
            "ListInstrument",
            "c3",
            json!({"instrumentId": 1000, "eventId": 100, "instrumentClass": "BINARY", "stateMask": 2,
                   "tickTableId": 1, "lotStep": 1, "allocationPolicy": "PRICE_TIME", "positionLimit": 0, "legs": []}),
        ),
        command("SetMarketState", "c4", json!({"instrumentId": 1000, "state": "OPEN"})),
        command("SleeveAdjust", "c5", json!({"accountId": 1, "deltaCents": 10000, "reason": "GRANT"})),
        command("SleeveAdjust", "c6", json!({"accountId": 2, "deltaCents": 10000, "reason": "GRANT"})),
        order(1, "b1", "BUY", 59, 5),
        order(2, "s1", "SELL", 59, 3),
    ]
}

/// Journals the inputs as the node would: `EngineStarted`, then each input followed by a `TimeTick`.
fn write_journal(path: &Path) -> u64 {
    let mut sequencer = Sequencer::resume(1, 0, 0);
    let mut out = BufWriter::new(File::create(path).unwrap());
    let mut clock = 1_800_000_000_000_000_000u64;
    let mut started = sequencer::engine_started(1, &[7; 32]);
    sequencer.stamp(&mut started, clock);
    write_frame(&mut out, &started).unwrap();
    let mut buf = vec![0u8; 1 << 16];
    let mut count = 1;
    for input in inputs() {
        clock += 1_000_000;
        let n = codec::encode(&input, &mut buf).unwrap();
        sequencer.stamp(&mut buf[..n], clock);
        write_frame(&mut out, &buf[..n]).unwrap();
        let mut tick = sequencer::time_tick();
        sequencer.stamp(&mut tick, clock);
        write_frame(&mut out, &tick).unwrap();
        count += 2;
    }
    out.flush().unwrap();
    count
}

fn results(bytes: &[u8]) -> Vec<Value> {
    let (mut r, mut buf, mut out) = (bytes, Vec::new(), Vec::new());
    while let Some(n) = read_frame(&mut r, &mut buf).unwrap() {
        out.push(codec::decode(&buf[..n]).unwrap());
    }
    out
}

#[test]
fn a_journal_replays_to_the_same_result_log_every_time() {
    let dir = std::env::temp_dir().join(format!("venue-node-golden-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    let journal = dir.join("journal.sbe");
    let written = write_journal(&journal);

    let (a, b) = (dir.join("a.sbe"), dir.join("b.sbe"));
    assert_eq!(replay(&journal, &a).unwrap(), written);
    replay(&journal, &b).unwrap();
    let first = fs::read(&a).unwrap();
    assert_eq!(first, fs::read(&b).unwrap(), "two replays of one journal differ");

    let decoded = results(&first);
    let fill = decoded.iter().find(|r| r["msg"] == "Fill").expect("the crossing order filled");
    assert_eq!((fill["tick"].as_u64(), fill["quantity"].as_u64()), (Some(59), Some(3)));
    assert!(decoded.iter().all(|r| r["result"]["sequencerGroup"] == 1));

    // A torn final frame, as a crash mid-append leaves, is ignored by replay.
    OpenOptions::new().append(true).open(&journal).unwrap().write_all(&[9, 0, 0]).unwrap();
    replay(&journal, &b).unwrap();
    assert_eq!(first, fs::read(&b).unwrap());
    fs::remove_dir_all(&dir).ok();
}
