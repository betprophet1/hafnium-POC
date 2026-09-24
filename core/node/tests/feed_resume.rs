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
use venue_node::snapshots::embed;

fn free_port() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0").expect("a port").local_addr().expect("its address")
}

/// A node serving the committed journal fixture, recovered from disk rather than traded into existence.
fn node() -> (SocketAddr, PathBuf) {
    node_with(|_| {})
}

/// The same, but `prepare` runs against the copied journal before the node starts serving it — for a test that
/// wants a `Snapshot` frame already embedded (`embed`) in what the node recovers from and serves.
fn node_with(prepare: impl FnOnce(&Path)) -> (SocketAddr, PathBuf) {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded");
    // `Instant::now` is disallowed in this crate, and a clock is not what makes a directory name unique anyway.
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nth = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("venue-feed-resume-{}-{nth}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a data directory");
    let journal = dir.join("journal.sbe");
    std::fs::copy(fixture.join("journal.sbe"), &journal).expect("the journal copies");
    std::fs::copy(fixture.join("capacity"), dir.join("capacity")).expect("the capacity copies");
    prepare(&journal);

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
    // The node answers whether it is honouring the position before any unit.
    let mut answer = [0u8; 1];
    stream.read_exact(&mut answer).expect("the node answers");
    assert_eq!(answer[0] != 0, resume_from > 0, "the node did not do what it said with {resume_from}");
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

/// Asking for a sequence the journal has never reached is a reader whose node was replaced under it — a fresh
/// data directory, or another group's. It is sent everything and told so, because a reader that kept a view has
/// to be able to throw it away. Sending it the tail instead would leave it with a view of a history that never
/// happened, and nothing would say so.
#[test]
fn a_position_the_journal_never_reached_is_refused_and_the_backlog_starts_over() {
    let (feed, _dir) = node();
    let everything = sequences(feed, 0);
    let last = *everything.last().expect("the fixture has inputs");

    let mut stream = TcpStream::connect(feed).expect("the feed accepts");
    stream.write_all(&(last + 1_000).to_le_bytes()).expect("the position is sent");
    stream.flush().expect("flushed");
    let mut answer = [0u8; 1];
    stream.read_exact(&mut answer).expect("the node answers");
    assert_eq!(answer[0], 0, "the node claimed to honour a position it cannot have");

    // And what follows really is the whole backlog.
    stream.set_read_timeout(Some(Duration::from_millis(1500))).expect("a read timeout");
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    while let Ok(n) = stream.read(&mut chunk) {
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
    assert!(!bytes.is_empty(), "nothing was sent at all");
    let first = u64::from_le_bytes(bytes[12..20].try_into().expect("a sequence"));
    assert_eq!(first, 1, "the backlog did not start at the first input");
}

/// A `Snapshot` frame embedded in the journal is not an input: a subscriber with no use for it never sees it, in
/// the backlog a fresh recovery reads or in what a running node goes on to serve. What a subscriber is sent is
/// exactly what it would have been sent had nothing ever been embedded.
#[test]
fn an_embedded_snapshot_is_not_sent_to_a_subscriber() {
    let (plain_feed, _plain_dir) = node();
    let plain = sequences(plain_feed, 0);
    assert!(plain.len() > 4, "the fixture journal is too short to cut: {} units", plain.len());

    let (feed, _dir) = node_with(|journal| {
        embed(journal, None).expect("a snapshot embeds");
    });
    let everything = sequences(feed, 0);
    assert_eq!(everything, plain, "an embedded Snapshot frame changed what a subscriber is sent");
}

/// Opens a subscription and returns only whether the node said it was honouring the position, without reading
/// the backlog: the handshake is the whole of what these next tests are about.
fn honoured(feed: SocketAddr, resume_from: u64) -> bool {
    let mut stream = TcpStream::connect(feed).expect("the feed accepts");
    stream.write_all(&resume_from.to_le_bytes()).expect("the position is sent");
    stream.flush().expect("flushed");
    let mut answer = [0u8; 1];
    stream.read_exact(&mut answer).expect("the node answers");
    answer[0] != 0
}

/// A journal serving a fixture that has been cut back to a snapshot at 62: it no longer carries 1..62.
fn cut_node() -> (SocketAddr, PathBuf) {
    node_with(|journal| {
        let results = journal.with_file_name("results.sbe");
        venue_node::node::replay(journal, &results).expect("the fixture replays");
        embed(journal, Some(62)).expect("a snapshot embeds");
        venue_node::node::cut(journal, &results).expect("the journal cuts");
    })
}

/// The lower bound. A cut journal no longer reaches back past its snapshot, so a reader asking to carry on from
/// inside the stretch that was removed must be refused — honouring it would send only what follows where the
/// journal now starts, silently skipping everything between, while the ack said the resume had worked.
#[test]
fn a_position_below_what_a_cut_journal_still_carries_is_refused() {
    let (feed, _dir) = cut_node();
    assert!(!honoured(feed, 30), "a position inside the stretch the cut removed was honoured");
    assert!(!honoured(feed, 61), "the input before the cut point is gone, so carrying on from it is not possible");
}

/// The floor itself is honoured, not refused: a reader holding *through* 62 is asking for 63 onward, which is
/// exactly what a journal cut at 62 carries. This is what every reader founded on a snapshot pair asks for, so
/// getting the boundary wrong here refuses the entire founding path.
#[test]
fn a_reader_holding_through_the_cut_point_is_honoured() {
    let (feed, _dir) = cut_node();
    assert!(honoured(feed, 62), "a reader holding through the cut point was refused what follows it");
}

/// And the bound is a floor, not a ban: a position the cut journal does still carry resumes as it always did.
#[test]
fn a_position_a_cut_journal_still_carries_is_honoured() {
    let (feed, _dir) = cut_node();
    let after = sequences(feed, 0);
    let carried = after.iter().copied().find(|&s| s > 62).expect("the cut journal carries something after 62");
    assert!(honoured(feed, carried), "a position the journal still carries ({carried}) was refused");
}

// The other half of the bound — that a snapshot sitting physically last does not drag the *upper* bound below
// the journal's own inputs — cannot be reached through a live node, and so is not tested here: `serve` appends
// `EngineStarted` at every start, so by the time any subscriber can connect there is always a real input after
// any snapshot an offline `embed` left at the tail. It is covered directly against `range` in node.rs's own
// tests instead, and will start mattering here the day a live node embeds its own snapshot (§3.7).
