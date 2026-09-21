//! A subscriber that says where it has got to is sent only what follows.
//!
//! Every reader used to rebuild its world from the first input on every connection, so the cost of a reconnect
//! grew with the journal and had no upper bound. A reader that can carry its state across a reconnect now says so
//! and pays for what it missed. It is also the thing that has to exist before the head of a journal can ever be
//! taken away: as long as any subscriber might ask for sequence 1, sequence 1 has to be there.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use venue_node::node::{serve, Config};

fn free_port() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0").expect("a port").local_addr().expect("its address")
}

/// A node serving the committed journal fixture, recovered from disk rather than traded into existence.
fn node() -> (SocketAddr, PathBuf) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded");
    // `Instant::now` is disallowed in this crate, and a clock is not what makes a directory name unique anyway.
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nth = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("venue-feed-resume-{}-{nth}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a data directory");
    std::fs::copy(fixture.join("journal.sbe"), dir.join("journal.sbe")).expect("the journal copies");
    std::fs::copy(fixture.join("capacity"), dir.join("capacity")).expect("the capacity copies");

    let (listen, feed) = (free_port(), free_port());
    let config = Config {
        listen,
        feed,
        data_dir: dir.clone(),
        group: 1,
        node_id: 1,
        // Slow, so the backlog is what these tests see rather than a stream of ticks.
        tick: Duration::from_secs(3600),
        fsync: false,
        capacity: "small".to_string(),
        metrics: None,
    };
    std::thread::spawn(move || {
        let _ = serve(&config);
    });
    // The feed port is bound once the node is up: counted rather than clocked, for the same reason.
    for _ in 0..600 {
        if TcpStream::connect(feed).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    (feed, dir)
}

/// Reads whole frames until the node goes quiet, and returns the sequence each unit's input carries.
fn sequences(feed: SocketAddr, resume_from: u64) -> Vec<u64> {
    let mut stream = TcpStream::connect(feed).expect("the feed accepts");
    stream.write_all(&resume_from.to_le_bytes()).expect("the position is sent");
    stream.flush().expect("flushed");
    stream.set_read_timeout(Some(Duration::from_millis(1500))).expect("a read timeout");

    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
            Err(_) => break, // the timeout: the backlog is done and nothing is live
        }
    }

    // A unit is the input frame, its result frames, then an empty frame. Only the input's sequence is wanted, so
    // walk the frames and take the sequence of the one after each terminator.
    let mut out = Vec::new();
    let (mut at, mut next_is_input) = (0usize, true);
    while at + 4 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[at..at + 4].try_into().expect("a length")) as usize;
        at += 4;
        if len == 0 {
            next_is_input = true;
            continue;
        }
        if at + len > bytes.len() {
            break;
        }
        if next_is_input {
            out.push(u64::from_le_bytes(bytes[at + 8..at + 16].try_into().expect("a sequence")));
            next_is_input = false;
        }
        at += len;
    }
    out
}

#[test]
fn a_subscriber_is_sent_only_what_follows_the_sequence_it_holds() {
    let (feed, _dir) = node();

    let everything = sequences(feed, 0);
    assert!(everything.len() > 4, "the fixture journal is too short to cut: {} units", everything.len());

    // Somewhere in the middle, on a sequence the feed actually carries.
    let cut = everything[everything.len() / 2];
    let after = sequences(feed, cut);

    assert!(!after.is_empty(), "resuming sent nothing at all");
    assert!(
        after.iter().all(|&s| s > cut),
        "a subscriber that had reached {cut} was sent something it already had: {:?}",
        after.iter().filter(|&&s| s <= cut).collect::<Vec<_>>()
    );
    let expected: Vec<u64> = everything.iter().copied().filter(|&s| s > cut).collect();
    assert_eq!(after, expected, "resuming changed which units are sent, not just where they start");
}

/// Saying nothing is the same as asking for everything, which is what every reader does today.
#[test]
fn a_subscriber_at_zero_is_sent_the_whole_backlog() {
    let (feed, _dir) = node();
    let everything = sequences(feed, 0);
    assert!(everything.len() > 4);
    assert_eq!(everything.first().copied(), Some(1), "the backlog did not start at the first input");
}
