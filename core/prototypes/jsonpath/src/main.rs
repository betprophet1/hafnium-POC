// What an order costs an event loop between the socket and the ingress, step by step.
//
// The gateway reads a frame, parses it into a `serde_json::Value`, copies the map without `op` and `id`, validates it
// while building a second `Value` for the node, encodes that to SBE and copies the bytes into a request for the
// forwarder thread. This times each of those, and against them a path that parses the frame into a typed struct
// borrowed from the frame's own bytes and encodes from that, to say what the tree costs and what it buys.
//
// Not a statement of the venue's latency (core/node: `bench` and `load` are for that). Build with the release
// profile; the numbers below the printout are what one order costs one loop.

use serde::Deserialize;
use serde_json::{json, Value};
use std::hint::black_box;
use std::time::Instant;
use venue_harness::codec;

/// What a maker's amendment looks like on the wire, as `connection-scale.js` sends it.
const AMEND: &str = r#"{"op":"amend","id":"a","orderId":"281474976710658","quantity":20}"#;

/// And an order, which carries more fields and a nested price.
const ORDER: &str = r#"{"op":"order","id":"o1","clientOrderId":"o1","instrumentId":"100000100","action":"buy","outcome":"yes","price":{"tick":4},"quantity":2}"#;

/// The reply the client is sent for an amendment, of the size the gateway really sends.
const REPLY: &str = r#"{"type":"reply","id":"a","result":{"orderId":"281474976710658","status":"amended","remaining":20,"price":{"tick":4,"cents":"2","probability":"0.0400","american":"+2400"},"executions":[]}}"#;

/// The same amendment, read into borrowed fields rather than a tree. Nothing is copied out of the frame: the strings
/// point into it, and the numbers are parsed where they are read.
#[derive(Deserialize)]
struct Amend<'a> {
    #[allow(dead_code)]
    op: &'a str,
    #[allow(dead_code)]
    id: &'a str,
    #[serde(rename = "orderId")]
    #[allow(dead_code)]
    order_id: &'a str,
    #[allow(dead_code)]
    quantity: Option<u64>,
}

fn time(name: &str, runs: u32, mut work: impl FnMut()) -> f64 {
    // A few passes first, so the measurement is of warm code and warm allocator.
    for _ in 0..runs / 10 {
        work();
    }
    let started = Instant::now();
    for _ in 0..runs {
        work();
    }
    let each = started.elapsed().as_nanos() as f64 / f64::from(runs);
    println!("{name:<44} {each:8.0} ns");
    each
}

/// The map the gateway hands to the API after taking `op` and `id` out of it (`body_of`).
fn body_of(message: &Value) -> Value {
    let mut body = message.as_object().cloned().unwrap_or_default();
    body.remove("op");
    body.remove("id");
    Value::Object(body)
}

/// The input the node is sent for an amendment, as `amend_input` builds it.
fn node_input(
    session: u64,
    account: u64,
    instrument: u64,
    order: u64,
    tick: Option<u64>,
    quantity: Option<u64>,
) -> Value {
    json!({
        "msg": "AmendOrder", "sessionId": session, "accountId": account, "instrumentId": instrument,
        "orderId": order, "newTick": tick, "newQuantity": quantity, "atEventStart": "UNCHANGED",
        "record": {"sequence": 0, "sequencedAt": 0, "sequencerGroup": 0}
    })
}

fn main() {
    let runs = 200_000;
    let mut buf = vec![0u8; 1 << 16];

    println!("one amendment, {} bytes on the wire", AMEND.len());
    let parse = time("parse the frame into a Value", runs, || {
        black_box(serde_json::from_str::<Value>(black_box(AMEND)).expect("JSON"));
    });
    let typed = time("parse the frame into borrowed fields", runs, || {
        black_box(serde_json::from_str::<Amend<'_>>(black_box(AMEND)).expect("JSON"));
    });
    let message: Value = serde_json::from_str(AMEND).expect("JSON");
    let body = time("copy the map without op and id", runs, || {
        black_box(body_of(black_box(&message)));
    });
    let build = time("build the node's input as a Value", runs, || {
        black_box(node_input(7, 1, 100_000_100, 281_474_976_710_658, None, Some(20)));
    });
    let input = node_input(7, 1, 100_000_100, 281_474_976_710_658, None, Some(20));
    let encode = time("encode that Value to SBE", runs, || {
        black_box(codec::encode(black_box(&input), black_box(&mut buf)).expect("encoded"));
    });
    let n = codec::encode(&input, &mut buf).expect("encoded");
    let copy = time("copy the SBE bytes for the forwarder", runs, || {
        black_box(buf[..n].to_vec());
    });
    let reply_value: Value = serde_json::from_str(REPLY).expect("JSON");
    let reply = time("serialise the reply to text", runs, || {
        black_box(black_box(&reply_value).to_string());
    });

    println!("\none order, {} bytes on the wire", ORDER.len());
    time("parse the frame into a Value", runs, || {
        black_box(serde_json::from_str::<Value>(black_box(ORDER)).expect("JSON"));
    });

    // What the loop does today for one amendment, and what is left if the tree goes.
    let now = parse + body + build + encode + copy + reply;
    let typed_path = typed + encode + reply;
    println!(
        "\nan amendment through the loop today: {now:.0} ns (parse {parse:.0} + copy {body:.0} + build {build:.0} + \
         encode {encode:.0} + copy {copy:.0} + reply {reply:.0})"
    );
    println!(
        "the same without the tree, if SBE were written straight from the borrowed fields: about {typed_path:.0} ns"
    );
    println!("(the reply is still built and serialised as a Value in both)");
}
