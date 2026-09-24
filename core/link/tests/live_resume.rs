//! A `Live` view that already holds part of the journal is not thrown away and rebuilt on every reconnect: it
//! tells the node where it has got to and asks to resume from there.
//!
//! `core/node/tests/feed_resume.rs` proves the node honours a resume position when a subscriber asks for one.
//! This proves a real subscriber actually asks: before this, `view::Live::follow` (used by the sim `web` and
//! `oracle` bots) hardcoded position 0 on every single reconnect, discarding its view even when nothing about
//! the connection needed that — see HANDOFF.md §3.8/§6.9.

use serde_json::json;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::mpsc::{channel, Sender};
use std::thread;
use std::time::Duration;

use venue_harness::codec;
use venue_link::view::Live;
use venue_node::frames::write_frame;

/// A minimal fake feed: answers the handshake, sends one quiet `TimeTick` at `sequence` (a real `View` still
/// updates its held sequence from a tick that changes nothing else shown, view.rs's own `apply`), then closes —
/// so the real client's next connection is the thing under test. Reports each connection's requested position.
fn fake_feed(sequence: u64, positions: Sender<u64>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let addr = listener.local_addr().expect("its address");
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut position = [0u8; 8];
            if stream.read_exact(&mut position).is_err() {
                continue;
            }
            let asked = u64::from_le_bytes(position);
            if positions.send(asked).is_err() {
                return; // the test has read what it needed
            }
            let _ = stream.write_all(&[u8::from(asked > 0)]);
            let mut input = json!({"msg": "TimeTick"});
            input["record"] = json!({"sequence": sequence, "sequencedAt": sequence, "sequencerGroup": 1});
            let mut buf = vec![0u8; 1 << 16];
            let n = codec::encode(&input, &mut buf).expect("a TimeTick encodes");
            let _ = write_frame(&mut stream, &buf[..n]);
            let _ = write_frame(&mut stream, b""); // zero results: the unit is complete
            drop(stream); // ends the connection, forcing the real client to reconnect — the thing under test
        }
    });
    addr
}

#[test]
fn a_reconnecting_view_asks_to_resume_from_what_it_already_holds() {
    let (tx, rx) = channel();
    let addr = fake_feed(5, tx);
    let _live = Live::follow(&addr.to_string());

    let first = rx.recv_timeout(Duration::from_secs(2)).expect("the first connection's requested position");
    assert_eq!(first, 0, "a fresh reader should ask for everything");

    let second = rx.recv_timeout(Duration::from_secs(3)).expect("a reconnect's requested position");
    assert_eq!(second, 5, "a reconnect should resume from the sequence the view already holds, not start over");
}
