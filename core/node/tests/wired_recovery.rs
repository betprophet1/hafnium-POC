//! `serve`'s startup now uses an embedded snapshot when the preserved result log already covers it, instead of
//! always replaying from genesis. This is the end-to-end proof: a node recovering the fast way is indistinguishable
//! from one that recovered the slow way, from a subscriber's seat and in what it durably wrote — the two vantage
//! points that have to agree.
//!
//! Not compared: raw backlog bytes between two independently-started nodes. Both commit an `EngineStarted` right
//! after recovery, stamped with the real wall clock (`Pipeline::commit`, E-17.1's sequencer, not the engine) — two
//! processes started moments apart never agree on that one field, correctly, and comparing raw bytes across nodes
//! would fail on that alone. Sequences sent (order and completeness) and the result log's own bytes (which
//! `EngineStarted` never touches — it produces no result) are what actually has to match.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use venue_node::node::{replay, serve, Config};
use venue_node::snapshots::embed;

fn free_port() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0").expect("a port").local_addr().expect("its address")
}

fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nth = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("venue-wired-recovery-{name}-{}-{nth}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// Whether, and where, `prepared` embeds a `Snapshot` into the journal it builds.
enum Snapshot {
    None,
    At(u64),
}

/// A data directory seeded from the committed journal fixture, with a real (not synthetic) result log built by
/// replaying it — standing in for "the node ran for a while and this is durably on disk" — and, per `snapshot`, a
/// `Snapshot` embedded into the journal. `replay`'s result log is what a fresh genesis recovery would also
/// produce, so building it this way rather than copying the fixture's own `results.sbe` (if it even matches this
/// schema version) keeps the two nodes below comparable on the same footing.
fn prepared(name: &str, snapshot: Snapshot) -> PathBuf {
    let dir = scratch(name);
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded");
    let journal = dir.join("journal.sbe");
    std::fs::copy(fixture.join("journal.sbe"), &journal).expect("the journal copies");
    std::fs::copy(fixture.join("capacity"), dir.join("capacity")).expect("the capacity copies");
    replay(&journal, &dir.join("results.sbe")).expect("the journal replays");
    match snapshot {
        Snapshot::None => {}
        Snapshot::At(at) => {
            embed(&journal, Some(at)).expect("a snapshot embeds");
        }
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

/// The sequence each backlog unit's input carries, in the order sent — immune to `EngineStarted`'s wall clock,
/// which never enters a sequence number.
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

/// The result log's bytes, up to (not including) `EngineStarted`'s own results — there are none, since it
/// produces no result, but reading the whole file is simplest and this is only ever compared to another whole
/// file built the same way.
fn results_bytes(dir: &Path) -> Vec<u8> {
    std::fs::read(dir.join("results.sbe")).expect("results.sbe reads")
}

#[test]
fn recovering_from_a_snapshot_the_result_log_already_covers_is_indistinguishable_from_genesis() {
    // No snapshot: recovers exactly as before PR #14/#15, replaying every input from sequence 1.
    let genesis_dir = prepared("genesis", Snapshot::None);
    let genesis_feed = node_over(&genesis_dir);

    // A snapshot at 62: `results.sbe` was built by `replay`, which (like every full replay) covers every
    // sequence — `repair_results` discards only its last group, well past 62, so this recovers from the
    // snapshot and replays only the tail.
    let from_snapshot_dir = prepared("from-snapshot", Snapshot::At(62));
    let from_snapshot_feed = node_over(&from_snapshot_dir);

    let expected_sequences = backlog_sequences(genesis_feed);
    assert!(!expected_sequences.is_empty(), "the fixture journal produced no backlog at all, proves nothing");
    assert_eq!(
        backlog_sequences(from_snapshot_feed),
        expected_sequences,
        "a snapshot-based recovery sent a different set of sequences"
    );

    // The durable record itself, not just what a subscriber happened to receive.
    assert_eq!(
        results_bytes(&from_snapshot_dir),
        results_bytes(&genesis_dir),
        "a snapshot-based recovery produced a different result log"
    );
}

#[test]
fn a_snapshot_with_no_trusted_result_log_still_recovers_correctly_by_falling_back() {
    // A snapshot exists, but nothing durable backs it (the result log is missing outright) — `choose_recovery`
    // must fall back to genesis (unit-tested directly in `node.rs`), and that fallback must still be correct end
    // to end, not merely chosen.
    let genesis_dir = prepared("genesis-fallback", Snapshot::None);
    let genesis_feed = node_over(&genesis_dir);

    let stale_dir = prepared("stale-snapshot", Snapshot::At(62));
    std::fs::remove_file(stale_dir.join("results.sbe")).expect("results.sbe removed");
    let stale_feed = node_over(&stale_dir);

    let expected_sequences = backlog_sequences(genesis_feed);
    assert_eq!(
        backlog_sequences(stale_feed),
        expected_sequences,
        "a snapshot with no trusted results did not fall back to sending the same backlog"
    );
    assert_eq!(
        results_bytes(&stale_dir),
        results_bytes(&genesis_dir),
        "a snapshot with no trusted results did not fall back to the same result log"
    );
}
