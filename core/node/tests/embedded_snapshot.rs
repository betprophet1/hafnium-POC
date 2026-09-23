//! D-32: a `Snapshot` written into the journal itself, not beside it.
//!
//! `snapshot_replay.rs` proves E-20.9 for a snapshot kept in a file of its own. This proves the same equivalence
//! for one `embed` appends onto the journal, and that every existing reader of a journal — a full replay, and a
//! subscriber's backlog — still gets exactly what it got before, `Snapshot` frame or not: the format's own
//! promise that a reader with no use for one skips it by template id.

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use venue_node::frames::read_frame;
use venue_node::node::replay;
use venue_node::snapshots::{embed, latest, recover};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded/journal.sbe")
}

fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nth = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("venue-embedded-snapshot-{}-{nth}", std::process::id()));
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

/// The input sequence a frame names: `record.sequence` on an input or a `Snapshot`, `resultHeader.inputSequence`
/// on a result, and all three sit at the same offset — the message header, then the sequence.
fn sequence(frame: &[u8]) -> u64 {
    u64::from_le_bytes(frame[8..16].try_into().expect("a sequence"))
}

/// A journal that starts as a copy of the fixture, so `embed` (which appends in place) never touches it.
fn copy_of_fixture(name: &str) -> PathBuf {
    let journal = scratch(name);
    std::fs::copy(fixture(), &journal).expect("the fixture copies");
    journal
}

#[test]
fn embedding_a_snapshot_appends_it_without_disturbing_the_inputs_already_there() {
    let journal = copy_of_fixture("embed.journal.sbe");
    let before = frames(&journal);

    let cut = sequence(before[before.len() / 2].as_slice());
    let (at, bytes) = embed(&journal, Some(cut)).expect("a snapshot embeds");
    assert_eq!(at, cut, "the snapshot was embedded at the wrong sequence");
    assert!(bytes > 0, "the snapshot has no state in it");

    let after = frames(&journal);
    assert_eq!(&after[..before.len()], &before[..], "embedding changed a frame that was already there");
    assert_eq!(after.len(), before.len() + 1, "embedding did not append exactly one frame");
    assert_eq!(sequence(&after[before.len()]), cut, "the appended frame does not name the sequence it followed");
}

#[test]
fn latest_finds_the_furthest_snapshot_wherever_it_was_embedded() {
    let journal = copy_of_fixture("latest.journal.sbe");
    let all = frames(&journal);
    let early = sequence(all[2].as_slice());
    let later = sequence(all[all.len() / 2].as_slice());
    assert!(early < later, "the fixture is too short to pick two distinct points");

    assert!(latest(&journal).expect("scanned").is_none(), "a journal with no snapshot yet reported one");

    embed(&journal, Some(early)).expect("the early snapshot embeds");
    let (found, _) = latest(&journal).expect("scanned").expect("a snapshot");
    assert_eq!(found, early);

    embed(&journal, Some(later)).expect("the later snapshot embeds");
    let (found, _) = latest(&journal).expect("scanned").expect("a snapshot");
    assert_eq!(found, later, "latest did not prefer the furthest snapshot");
}

#[test]
fn recovering_from_an_embedded_snapshot_gives_the_same_results_as_a_replay_from_genesis() {
    let journal = copy_of_fixture("recover.journal.sbe");
    let all_results = scratch("recover.all.results.sbe");
    let inputs = replay(&journal, &all_results).expect("the journal replays");
    assert!(inputs > 4, "the fixture journal is too short to cut: {inputs} inputs");

    let cut = sequence(frames(&journal)[inputs as usize / 2].as_slice());
    embed(&journal, Some(cut)).expect("a snapshot embeds");

    let tail = scratch("recover.tail.results.sbe");
    let (restored_at, applied) = recover(&journal, &tail, None).expect("recovers").expect("a snapshot was there");
    assert_eq!(restored_at, cut);
    assert!(applied > 0, "nothing followed the snapshot, so this proves nothing");

    let expected: Vec<Vec<u8>> = frames(&all_results).into_iter().filter(|f| sequence(f) > cut).collect();
    let got = frames(&tail);
    assert_eq!(got, expected, "a recovery from the embedded snapshot diverged from a replay from genesis");
}

#[test]
fn recover_reports_none_for_a_journal_with_no_snapshot_yet() {
    let journal = copy_of_fixture("none.journal.sbe");
    let results = scratch("none.results.sbe");
    assert_eq!(recover(&journal, &results, None).expect("recovers"), None);
}

#[test]
fn a_full_replay_of_a_journal_with_an_embedded_snapshot_is_unchanged_by_it() {
    let plain = copy_of_fixture("plain.journal.sbe");
    let embedded = copy_of_fixture("plain-with-snapshot.journal.sbe");
    let all = frames(&plain);
    let cut = sequence(all[all.len() / 3].as_slice());
    embed(&embedded, Some(cut)).expect("a snapshot embeds");

    let plain_results = scratch("plain.results.sbe");
    let embedded_results = scratch("plain-with-snapshot.results.sbe");
    let plain_inputs = replay(&plain, &plain_results).expect("the plain journal replays");
    let embedded_inputs = replay(&embedded, &embedded_results).expect("the journal with a snapshot replays");

    assert_eq!(embedded_inputs, plain_inputs, "a Snapshot frame was counted, or applied, as an input");
    assert_eq!(
        frames(&plain_results),
        frames(&embedded_results),
        "an embedded Snapshot frame changed the result log of a full replay"
    );
}
