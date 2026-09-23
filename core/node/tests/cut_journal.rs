//! Cutting a journal removes what a snapshot makes unneeded; recovering from the cut journal has to reach the
//! same state, and send a subscriber the same tail, as recovering from the uncut one would have.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use venue_node::node::{cut, replay, serve, Config};
use venue_node::snapshots::embed;

fn free_port() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0").expect("a port").local_addr().expect("its address")
}

fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nth = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("venue-cut-journal-{name}-{}-{nth}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// A data directory seeded from the fixture, with a real result log built by replaying it, and a snapshot
/// embedded at `at`. `cut` is then applied only when `also_cut` is set, so the same setup produces either side of
/// the comparison.
fn prepared(name: &str, at: u64, also_cut: bool) -> PathBuf {
    let dir = scratch(name);
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded");
    let journal = dir.join("journal.sbe");
    std::fs::copy(fixture.join("journal.sbe"), &journal).expect("the journal copies");
    std::fs::copy(fixture.join("capacity"), dir.join("capacity")).expect("the capacity copies");
    let results = dir.join("results.sbe");
    replay(&journal, &results).expect("the journal replays");
    embed(&journal, Some(at)).expect("a snapshot embeds");
    if also_cut {
        cut(&journal, &results).expect("cuts");
    }
    dir
}

fn node_over(data_dir: &Path) -> SocketAddr {
    let (listen, feed) = (free_port(), free_port());
    let config = Config {
        listen,
        feed,
        data_dir: data_dir.to_path_buf(),
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
        if TcpStream::connect(feed).is_ok() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    feed
}

/// The sequence each backlog unit's input carries, in the order sent.
fn backlog_sequences(feed: SocketAddr) -> Vec<u64> {
    let mut stream = TcpStream::connect(feed).expect("the feed accepts");
    stream.write_all(&0u64.to_le_bytes()).expect("the position is sent");
    stream.flush().expect("flushed");
    let mut answer = [0u8; 1];
    stream.read_exact(&mut answer).expect("the node answers");
    stream.set_read_timeout(Some(Duration::from_millis(1500))).expect("a read timeout");
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
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

fn results_bytes(dir: &Path) -> Vec<u8> {
    std::fs::read(dir.join("results.sbe")).expect("results.sbe reads")
}

#[test]
fn a_cut_journal_recovers_to_the_same_tail_a_full_replay_would_have_reached() {
    let uncut_dir = prepared("uncut", 62, false);
    let uncut_feed = node_over(&uncut_dir);
    let uncut_all = backlog_sequences(uncut_feed);
    assert!(uncut_all.len() > 4, "the fixture journal is too short to cut: {} units", uncut_all.len());

    let cut_dir = prepared("cut", 62, true);
    let cut_feed = node_over(&cut_dir);
    let cut_all = backlog_sequences(cut_feed);

    // The cut journal no longer has anything at or before 62 to send at all: what it does send must match the
    // uncut node's tail exactly, sequence for sequence.
    let expected: Vec<u64> = uncut_all.into_iter().filter(|&s| s > 62).collect();
    assert!(!expected.is_empty(), "nothing follows the cut point, so this test proves nothing");
    assert_eq!(cut_all, expected, "a cut-and-recovered node sent a different tail than an uncut one");

    // And the durable record itself, not just what a subscriber happened to receive: results.sbe holds only the
    // tail after a cut, but that tail must be byte-identical to the uncut node's.
    let uncut_results = results_bytes(&uncut_dir);
    let cut_results = results_bytes(&cut_dir);
    assert!(uncut_results.len() > cut_results.len(), "cutting did not shrink the result log at all");
    assert!(
        uncut_results.ends_with(&cut_results),
        "the cut result log's surviving tail does not match the uncut node's"
    );
}

#[test]
fn cutting_refuses_and_changes_nothing_when_the_result_log_does_not_cover_the_snapshot() {
    let dir = scratch("refuses");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded");
    let journal = dir.join("journal.sbe");
    std::fs::copy(fixture.join("journal.sbe"), &journal).expect("the journal copies");
    embed(&journal, Some(62)).expect("a snapshot embeds");
    let before = std::fs::read(&journal).expect("read");

    // No results.sbe at all: nothing is trusted, so cutting must refuse rather than guess.
    let result = cut(&journal, &dir.join("results.sbe"));
    assert!(result.is_err(), "cut succeeded with nothing trusted at all");
    assert_eq!(std::fs::read(&journal).expect("read"), before, "a refused cut still touched the journal");
}
