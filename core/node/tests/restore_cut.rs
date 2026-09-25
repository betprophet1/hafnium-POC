//! An archived segment put back in front of the journal it was cut from has to yield the same history.
//!
//! Everything else about cutting has been checked from the *cutting* end: that the segment holds exactly what
//! the cut removed, that a cut is refused until the segment is shipped, that the cut journal still recovers.
//! None of that checks the archive from the other end. Until something reassembles a journal from a segment and
//! shows the result is the same history, "we archive before we cut" is a claim nobody has tested — and an
//! archive nobody has read back is bytes, not a record.
//!
//! The property cannot be byte-equality of the files. `embed` appends wherever the journal ends, so the
//! original carries its `Snapshot` at the tail, after inputs with higher sequences, while a restored journal
//! carries it between the segment and what followed. Position has never meant anything in a journal (§3.7).
//! What must hold is that **a replay of the restored journal produces the same result log as a replay of the
//! original** — the same shape of equivalence E-20.9 already uses for snapshots.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use venue_node::node::{cut_checked, export_cut, replay, restore_cut};
use venue_node::snapshots::embed;

fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nth = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("venue-restore-cut-{name}-{}-{nth}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// A data directory from the fixture with a real result log, and the result log a replay of the *uncut* journal
/// produces — which is the thing a restored journal has to reproduce.
fn prepared(name: &str) -> (PathBuf, PathBuf) {
    let dir = scratch(name);
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded");
    let journal = dir.join("journal.sbe");
    std::fs::copy(fixture.join("journal.sbe"), &journal).expect("the journal copies");
    std::fs::copy(fixture.join("capacity"), dir.join("capacity")).expect("the capacity copies");
    replay(&journal, &dir.join("results.sbe")).expect("the journal replays");

    // What a replay of the original produces, kept before anything is cut away.
    let original = dir.join("original.results");
    replay(&journal, &original).expect("the original replays");
    (dir, original)
}

/// The load-bearing test. Cut a journal, restore it from its own segment, and replay: the result log must equal
/// the one the journal produced before any of this happened, byte for byte.
#[test]
fn a_restored_journal_replays_to_the_history_that_was_cut_away() {
    let (dir, original) = prepared("roundtrip");
    let (journal, results) = (dir.join("journal.sbe"), dir.join("results.sbe"));
    let segment = dir.join("segment");

    embed(&journal, Some(62)).expect("a snapshot embeds");
    export_cut(&journal, &results, &segment).expect("a segment exports");
    cut_checked(&journal, &results, None).expect("cuts");

    let restored = dir.join("restored");
    let (first, last) = restore_cut(&segment, &journal, &results, &restored).expect("the segment restores");
    assert_eq!(first, 1, "a restored journal should start at the first input again");

    let replayed = dir.join("restored.results");
    replay(&restored.join("journal.sbe"), &replayed).expect("the restored journal replays");

    assert_eq!(
        std::fs::read(&replayed).expect("the replayed results read"),
        std::fs::read(&original).expect("the original results read"),
        "replaying the restored journal did not reproduce the history that was cut away"
    );
    assert!(last >= 62, "the restored journal stops at {last}, before the cut point");
}

/// The guard that matters, because the failure it prevents leaves a well-formed file. A segment from a
/// *different* cut splices a gap or an overlap into the history, and nothing downstream would notice: the
/// frames are all valid, the lengths all agree, and a replay would simply fail-stop somewhere confusing.
#[test]
fn a_segment_from_a_different_cut_is_refused() {
    let (dir, _) = prepared("mismatch");
    let (journal, results) = (dir.join("journal.sbe"), dir.join("results.sbe"));

    // A segment taken at 30 ...
    embed(&journal, Some(30)).expect("a snapshot embeds at 30");
    let early = dir.join("segment-30");
    export_cut(&journal, &results, &early).expect("a segment exports at 30");

    // ... against a journal that was actually cut at 62.
    embed(&journal, Some(62)).expect("a snapshot embeds at 62");
    let late = dir.join("segment-62");
    export_cut(&journal, &results, &late).expect("a segment exports at 62");
    cut_checked(&journal, &results, None).expect("cuts at 62");

    let refused = restore_cut(&early, &journal, &results, &dir.join("wrong")).expect_err("restored from 30");
    assert!(refused.to_string().contains("does not belong in front of this journal"), "unhelpful refusal: {refused}");

    // And the right one is accepted, so the check is a floor and not a ban.
    restore_cut(&late, &journal, &results, &dir.join("right")).expect("the matching segment restores");
}

/// Restoring into a directory that already holds a restored journal would overwrite it.
#[test]
fn a_directory_that_already_holds_a_restore_is_refused() {
    let (dir, _) = prepared("twice");
    let (journal, results) = (dir.join("journal.sbe"), dir.join("results.sbe"));
    let segment = dir.join("segment");
    embed(&journal, Some(62)).expect("a snapshot embeds");
    export_cut(&journal, &results, &segment).expect("a segment exports");
    cut_checked(&journal, &results, None).expect("cuts");

    let out = dir.join("restored");
    restore_cut(&segment, &journal, &results, &out).expect("the first restore");
    let refused = restore_cut(&segment, &journal, &results, &out).expect_err("the second restore overwrote");
    assert!(refused.to_string().contains("already exists"), "unhelpful refusal: {refused}");
}

/// A journal that was never cut has nothing in front of it, and restoring into one would duplicate history.
#[test]
fn a_journal_that_was_never_cut_is_refused() {
    let (dir, _) = prepared("uncut");
    let (journal, results) = (dir.join("journal.sbe"), dir.join("results.sbe"));
    let segment = dir.join("segment");
    embed(&journal, Some(62)).expect("a snapshot embeds");
    export_cut(&journal, &results, &segment).expect("a segment exports");
    // Deliberately no cut: the journal still carries everything the segment holds.

    let refused =
        restore_cut(&segment, &journal, &results, &dir.join("out")).expect_err("restored into an uncut journal");
    assert!(refused.to_string().contains("carries inputs from"), "unhelpful refusal: {refused}");
}
