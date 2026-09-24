//! `venue-node seed` founds a fresh data directory on a chosen snapshot instead of genesis (§3.9: a test/preview
//! environment starting from whatever snapshot it likes). What matters is that this is not a shortcut that skips
//! restoring real state: a node seeded this way must continue sequencing exactly where the snapshot left off, the
//! same as a node that reached that state by actually running from genesis would.

use std::io::{BufReader, BufWriter, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use serde_json::json;
use venue_harness::codec;
use venue_node::frames::{read_frame, write_frame};
use venue_node::node::{self, serve, Config};
use venue_node::snapshots;

fn free_port() -> SocketAddr {
    TcpListener::bind("127.0.0.1:0").expect("a port").local_addr().expect("its address")
}

fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nth = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("venue-seed-from-snapshot-{name}-{}-{nth}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// A journal holding only the recorded fixture's inputs with sequence at most `at`, and nothing else — a
/// from-genesis history that stops exactly where a snapshot taken `at` would, so a node serving it and one seeded
/// from that snapshot can be sent the same new input and compared.
fn truncated_fixture_journal(at: u64, out_dir: &Path) -> PathBuf {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded");
    let mut reader = BufReader::new(std::fs::File::open(fixture.join("journal.sbe")).expect("the fixture opens"));
    let journal = out_dir.join("journal.sbe");
    let mut out = BufWriter::new(std::fs::File::create(&journal).expect("created"));
    let mut buf = Vec::new();
    while let Some(n) = read_frame(&mut reader, &mut buf).expect("a frame") {
        let sequence = u64::from_le_bytes(buf[8..16].try_into().expect("a sequence"));
        if sequence > at {
            break;
        }
        write_frame(&mut out, &buf[..n]).expect("written");
    }
    out.flush().expect("flushed");
    std::fs::copy(fixture.join("capacity"), out_dir.join("capacity")).expect("the capacity copies");
    journal
}

fn node(data_dir: PathBuf) -> SocketAddr {
    let (listen, feed) = (free_port(), free_port());
    let config = Config {
        listen,
        feed,
        data_dir,
        group: 1,
        node_id: 1,
        tick: Duration::from_secs(3600), // no ticks: only the input this test sends should move the sequence
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
    listen
}

/// Sends one `DefineEvent` (a fresh, very-out-of-range ID, so it cannot collide with anything the fixture's own
/// history already defined) and returns the sequence its own `CommandApplied`/journal record was stamped with.
fn define_event(listen: SocketAddr, event_id: u64) -> u64 {
    let mut input = json!({
        "msg": "DefineEvent", "eventId": event_id, "stateCount": 2, "scheduledStart": 0,
        "record": {"sequence": 0, "sequencedAt": 0, "sequencerGroup": 0},
    });
    let mut buf = vec![0u8; 1 << 16];
    let n = codec::encode(&input, &mut buf).expect("DefineEvent encodes");
    let stream = TcpStream::connect(listen).expect("the ingress accepts");
    let mut writer = BufWriter::new(stream.try_clone().expect("cloned"));
    write_frame(&mut writer, &buf[..n]).expect("written");
    writer.flush().expect("flushed");
    let mut reader = BufReader::new(stream);
    let mut frame = Vec::new();
    let mut sequence = None;
    while let Some(m) = read_frame(&mut reader, &mut frame).expect("a result frame") {
        if m == 0 {
            break;
        }
        let result = codec::decode(&frame[..m]).expect("a result decodes");
        if result["msg"] == "CommandApplied" {
            assert_eq!(result["outcome"], "APPLIED", "the event was refused: {result}");
            sequence = result["result"]["inputSequence"].as_u64();
        }
    }
    let _ = input.as_object_mut().map(|o| o.remove("record"));
    sequence.expect("no CommandApplied result named the sequence it was journaled at")
}

#[test]
fn a_node_seeded_from_a_snapshot_continues_sequencing_exactly_where_it_left_off() {
    let dir = scratch("continues");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded/journal.sbe");
    let snapshot = dir.join("snapshot.out");
    let (at, _) = snapshots::take(&fixture, &snapshot, Some(62)).expect("a snapshot is taken");
    assert_eq!(at, 62);

    let seeded_dir = dir.join("seeded");
    node::seed(&snapshot, &seeded_dir, "small").expect("seeds");
    let seeded = node(seeded_dir);

    // 64, not 63: every `serve` start journals `EngineStarted` first (sequence 63), before anything a client
    // sends, so this input is the second one after the snapshot.
    let sequence = define_event(seeded, 999_000_001);
    assert_eq!(sequence, 64, "a node seeded at 62 did not continue sequencing from where the snapshot left off");
}

#[test]
fn a_seeded_node_and_one_that_reached_the_same_point_from_genesis_answer_the_same_new_input_identically() {
    let dir = scratch("equivalent");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded/journal.sbe");

    let genesis_dir = dir.join("genesis");
    std::fs::create_dir_all(&genesis_dir).expect("a data directory");
    truncated_fixture_journal(62, &genesis_dir);
    let genesis = node(genesis_dir);

    let snapshot = dir.join("snapshot.out");
    snapshots::take(&fixture, &snapshot, Some(62)).expect("a snapshot is taken");
    let seeded_dir = dir.join("seeded");
    node::seed(&snapshot, &seeded_dir, "small").expect("seeds");
    let seeded = node(seeded_dir);

    let event_id = 999_000_002;
    let genesis_sequence = define_event(genesis, event_id);
    let seeded_sequence = define_event(seeded, event_id);
    assert_eq!(
        genesis_sequence, seeded_sequence,
        "a seeded node and a genesis-derived node reached at the same point sequenced the same new input \
         differently"
    );
    assert_eq!(genesis_sequence, 64); // 62 (the snapshot) + EngineStarted (63) + this event (64)
}
