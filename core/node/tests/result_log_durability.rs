//! `results.sbe` is durable the same way `journal.sbe` already is: flushed before the next input is committed, not
//! only once a tick. Before this, a result could sit in a `BufWriter`'s memory for up to a whole tick after the
//! client holding it had already been told it was done — invisible to anything reading the file directly, and lost
//! outright if the process went down in that window.

use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use venue_node::frames::{read_frame, write_frame, MAX_FRAME};
use venue_node::node::{serve, Config};

fn free_port() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0").expect("a port").local_addr().expect("its address")
}

/// A node over a fresh, empty data directory — nothing recovered, so the only results on disk are what this test
/// sends it — with a tick slow enough that a tick's own periodic flush (long since redundant, but there in
/// spirit) never fires during the test.
fn node() -> (SocketAddr, PathBuf) {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nth = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("venue-result-durability-{}-{nth}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a data directory");

    let (listen, feed) = (free_port(), free_port());
    let config = Config {
        listen,
        feed,
        data_dir: dir.clone(),
        group: 1,
        node_id: 1,
        tick: Duration::from_secs(3600),
        fsync: false,
        capacity: "small".to_string(),
        metrics: None,
    };
    std::thread::spawn(move || {
        let _ = serve(&config);
    });
    for _ in 0..600 {
        if TcpStream::connect(listen).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    (listen, dir)
}

/// Sends one client message over the ingress (a zeroed record header, stamped by the sequencer on arrival) and
/// reads its reply frames until the terminator. Returns whether any result frame came back.
fn send(listen: SocketAddr, message: serde_json::Value) -> bool {
    let mut input = message;
    input
        .as_object_mut()
        .expect("a JSON object")
        .insert("record".to_string(), serde_json::json!({"sequence": 0, "sequencedAt": 0, "sequencerGroup": 0}));
    let mut buf = vec![0u8; MAX_FRAME];
    let n = venue_harness::codec::encode(&input, &mut buf).expect("the message encodes");

    let mut stream = TcpStream::connect(listen).expect("the ingress accepts");
    stream.set_nodelay(true).ok();
    write_frame(&mut stream, &buf[..n]).expect("sent");
    stream.flush().expect("flushed");

    let mut reader = std::io::BufReader::new(stream);
    let mut frame = Vec::new();
    let mut got_a_result = false;
    loop {
        match read_frame(&mut reader, &mut frame).expect("the reply reads") {
            Some(0) => break,
            Some(_) => got_a_result = true,
            None => panic!("the node closed the connection before the terminator"),
        }
    }
    got_a_result
}

#[test]
fn a_result_is_on_disk_before_the_client_that_caused_it_is_told_so() {
    let (listen, dir) = node();

    // Against a fresh engine with nothing listed, an order is rejected — which is still a result, and needs no
    // setup to provoke.
    let order = serde_json::json!({
        "msg": "NewOrder",
        "accountId": 1,
        "clientOrderId": "a1",
        "instrumentId": 1000,
        "side": "SELL",
        "tick": 59,
        "quantity": 5,
    });
    assert!(send(listen, order), "the order was not rejected, so this test proves nothing");

    // No tick has fired (tick is an hour), and nothing here flushes anything explicitly: reading the file through
    // an independent handle is the only way this passes, and only because `commit` already flushed it.
    let results = std::fs::read(dir.join("results.sbe")).expect("results.sbe reads");
    assert!(!results.is_empty(), "the result was not durable by the time the reply came back");

    let mut reader = &results[..];
    let mut frame = Vec::new();
    let mut sequences = Vec::new();
    while (read_frame(&mut reader, &mut frame).expect("a frame reads")).is_some() {
        sequences.push(u64::from_le_bytes(frame[8..16].try_into().expect("a sequence")));
    }
    assert!(!sequences.is_empty(), "the file had bytes but no whole result frame");
    // The order was the second input (EngineStarted is the first, at serve's own startup) and produced at least
    // one result of its own.
    assert!(sequences.contains(&2), "no result named the order's own sequence: {sequences:?}");
}
