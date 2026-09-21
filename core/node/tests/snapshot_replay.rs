//! E-20.9 against a real journal: a snapshot at N, replayed to the end, gives the results a replay from genesis
//! gives for those same inputs — byte for byte.
//!
//! This is the property the whole scheme rests on, checked where it matters. The engine tests build a venue by
//! hand; this one takes the journal committed as a fixture, cuts it at an arbitrary point, and shows that starting
//! from the middle is the same as starting from the beginning. Which is what makes the beginning removable.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use venue_node::frames::read_frame;
use venue_node::node::replay;
use venue_node::snapshots::{replay_from, take};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded/journal.sbe")
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("venue-snapshot-replay-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir.join(name)
}

/// Every frame of a file, as bytes. Results are framed exactly as inputs are.
fn frames(path: &Path) -> Vec<Vec<u8>> {
    let mut reader = BufReader::new(File::open(path).expect("the file opens"));
    let mut buf = Vec::new();
    let mut out = Vec::new();
    while let Some(n) = read_frame(&mut reader, &mut buf).expect("a frame reads") {
        out.push(buf[..n].to_vec());
    }
    out
}

/// The input sequence a frame names: `record.sequence` on an input, `resultHeader.inputSequence` on a result, and
/// both sit at the same offset — the message header, then the sequence.
fn sequence(frame: &[u8]) -> u64 {
    u64::from_le_bytes(frame[8..16].try_into().expect("a sequence"))
}

#[test]
fn a_replay_from_a_snapshot_gives_the_same_results_as_a_replay_from_genesis() {
    let journal = fixture();
    let all = scratch("all.results.sbe");
    let inputs = replay(&journal, &all).expect("the journal replays");
    assert!(inputs > 4, "the fixture journal is too short to cut: {inputs} inputs");

    // Somewhere in the middle, and not on a boundary anybody chose.
    let cut = sequence(frames(&journal)[inputs as usize / 2].as_slice());

    let snapshot = scratch("middle.snapshot.sbe");
    let (at, bytes) = take(&journal, &snapshot, Some(cut)).expect("a snapshot is taken");
    assert_eq!(at, cut, "the snapshot was taken at the wrong sequence");
    assert!(bytes > 0, "the snapshot has no state in it");

    let tail = scratch("tail.results.sbe");
    let (restored_at, applied) = replay_from(&snapshot, &journal, &tail, None).expect("the snapshot replays");
    assert_eq!(restored_at, cut);
    assert!(applied > 0, "nothing followed the snapshot, so this proves nothing");

    let expected: Vec<Vec<u8>> = frames(&all).into_iter().filter(|f| sequence(f) > cut).collect();
    let got = frames(&tail);
    assert_eq!(
        got.len(),
        expected.len(),
        "a replay from the snapshot produced a different number of results after sequence {cut}"
    );
    assert_eq!(got, expected, "a replay from the snapshot diverged from a replay from genesis");
}

/// A snapshot taken at the end of a journal and one taken from a replay that *started* at a snapshot describe the
/// same venue: the second half of E-20.9, where the state itself is compared rather than the results.
///
/// The two engines reach the same input by different routes and with different slab layouts — one filled its
/// slots by replaying everything, the other rebuilt them from an encoding partway through — so equal bytes here
/// mean the encoding carries the venue and not the engine.
#[test]
fn a_snapshot_of_the_end_is_the_same_whichever_way_the_end_was_reached() {
    let journal = fixture();
    let whole = scratch("whole.snapshot.sbe");
    let (end, _) = take(&journal, &whole, None).expect("a snapshot of the end");

    // Cut early, so the restored engine reaches the end by a different route than a replay from genesis.
    let early = sequence(frames(&journal)[2].as_slice());
    assert!(early < end, "the cut was not before the end");
    let cut = scratch("early.snapshot.sbe");
    take(&journal, &cut, Some(early)).expect("a snapshot early on");

    let tail = scratch("from-early.results.sbe");
    let reached = scratch("from-early.end.snapshot.sbe");
    let (_, applied) = replay_from(&cut, &journal, &tail, Some(&reached)).expect("the early snapshot replays");
    assert!(applied > 0, "nothing followed the early snapshot, so this proves nothing");

    let from_genesis = std::fs::read(&whole).expect("the genesis snapshot reads");
    let from_snapshot = std::fs::read(&reached).expect("the restored snapshot reads");
    assert_eq!(from_snapshot, from_genesis, "the end reached from a snapshot is not the end reached from genesis");
}
