//! `on_reset` fires whenever the node's ack says a resume was not honoured — including when the reader asked for
//! position 0, which used to be exempt.
//!
//! `core/node/tests/feed_resume.rs` proves the node's ack byte is now correct. This proves the *client* actually
//! acts on it: before this fix, `follow_reporting`'s own guard was `resume_from > 0 && !resuming`, so a reader
//! that asked for 0 — which is every reader without a snapshot pair to restore from, which today is every reader
//! deployed (`STATE.md` §8) — never called `on_reset` no matter what the ack said. The two bugs sat on either
//! side of the same wire byte: fixing only the node would still not have reached a reader that had decided, on
//! its own side, never to look.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use std::time::Duration;

use venue_link::wire;

/// A fake feed that answers every connection's handshake with `ack`, then closes without sending a unit —
/// `read_units` errors on the EOF and `follow_reporting` reconnects, so one fake serves as many connections as a
/// test needs. `positions` reports what each connection asked for, in case a test wants to check it.
fn fake_feed(ack: u8, positions: Sender<u64>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a port");
    let addr = listener.local_addr().expect("its address");
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let mut position = [0u8; 8];
            if stream.read_exact(&mut position).is_err() {
                continue;
            }
            let _ = positions.send(u64::from_le_bytes(position));
            let _ = stream.write_all(&[ack]);
            // Nothing else is written: the connection just ends there, which is enough to drive
            // `follow_reporting` through one full connect/ack/on_reset cycle without a real node.
        }
    });
    addr
}

/// Runs `follow_reporting` against `feed` with a fixed `resume_from`, and returns whether `on_reset` was called
/// after the first connection.
fn reset_fired(feed: SocketAddr, resume_from: u64) -> bool {
    let (tx, rx): (Sender<bool>, Receiver<bool>) = channel();
    let addr = feed.to_string();
    thread::spawn(move || {
        wire::follow_reporting(
            &addr,
            move || resume_from,
            move || {
                let _ = tx.send(true);
            },
            |_unit| {},
            |_e| {}, // the connection ending with no units is expected here; nothing to report
        );
    });
    // A connection that is NOT reset sends nothing on `rx` at all, so this cannot simply await one message —
    // absence is the thing half of these cases are asserting. Give it a real window to have happened, then
    // decide by what arrived.
    rx.recv_timeout(Duration::from_millis(500)).unwrap_or(false)
}

#[test]
fn asking_for_zero_and_being_honoured_does_not_reset() {
    let (tx, _rx) = channel();
    let feed = fake_feed(1, tx); // ack != 0: honoured
    assert!(!reset_fired(feed, 0), "a genuinely full backlog should not have thrown the view away");
}

#[test]
fn asking_for_zero_and_being_refused_resets() {
    let (tx, _rx) = channel();
    let feed = fake_feed(0, tx); // ack == 0: not honoured — a cut journal answering a fresh reader
    assert!(reset_fired(feed, 0), "asking for everything and not getting it must still reset the view");
}

#[test]
fn asking_to_resume_and_being_honoured_does_not_reset() {
    let (tx, _rx) = channel();
    let feed = fake_feed(1, tx);
    assert!(!reset_fired(feed, 62), "a resume the node granted should not have thrown the view away");
}

#[test]
fn asking_to_resume_and_being_refused_resets() {
    let (tx, _rx) = channel();
    let feed = fake_feed(0, tx);
    assert!(reset_fired(feed, 62), "a resume the node refused must reset the view — this is the pre-existing case");
}
