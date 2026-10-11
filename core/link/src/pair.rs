//! Taking both halves of a snapshot at one sequence: the engine's, and a reader's.
//!
//! The engine's own snapshot (spec §20) restores a *node*. It cannot restore a *reader*. §20 has it carry the
//! engine's logical state with whatever the engine does not need left out (E-20.5, E-20.6), so it holds no
//! parlay legs — only the mask the engine matches on — no resolution kind or winning state, none of an event's
//! timestamps, and no per-order filled total. A reader rebuilt from it would look right and be quietly wrong
//! for parlays, for a calendar counting an event's stages, and for any client asking what an order has filled.
//!
//! A reader's state is a projection of the unit stream, so the reader's half is taken the way a reader builds
//! one: by replaying the same units through the same `apply` (`View::replayed`).
//!
//! The two are taken together because an environment founded on a snapshot needs both. The node is restored
//! from the engine half; every subscriber that connects to it would otherwise ask for sequence 0 and be sent an
//! empty backlog, because a founded journal holds no inputs at all — so each of them needs the view half
//! instead. One without the other leaves the venue up, correct inside, and empty to everyone looking at it.

use std::path::Path;

use crate::view::View;
use crate::wire::Error;

/// The engine's half of a pair, inside the directory that holds it.
pub const ENGINE: &str = "engine.snapshot";
/// The reader's half.
pub const VIEW: &str = "view.json";

/// Writes both halves into `dir`, taken at sequence `at` — or after the last input, when `at` is `None`.
///
/// `results` names an existing result log to pair the journal with; without one the journal is replayed to
/// produce it, which a journal `cut` has already shortened cannot be (it no longer starts at sequence 1), so
/// such a journal must be given its own.
///
/// Refuses if the two halves do not land on the same sequence, rather than writing a pair that only looks like
/// one: they are only usable together, and a mismatch means the reader would be restored to a different point
/// from the node it is about to follow.
pub fn take(journal: &Path, dir: &Path, at: Option<u64>, results: Option<&Path>) -> Result<u64, Error> {
    take_after(None, journal, dir, at, results)
}

/// The same, carrying on from the previously published pair.
///
/// **A journal that has been `cut` must be given one**, and this is where that is enforced, because it is the
/// only place both halves are visible at once. The engine half copes on its own — it restores from the snapshot
/// the journal was cut to. The view half cannot: it is built by applying units in order, and the units before
/// the cut are gone, so replaying from nothing yields a view missing everything that happened before the cut,
/// with no error and a `sequence` that still matches the engine's. The halves would agree and both be wrong.
///
/// So: a cut journal without a previous view is refused, and a previous view older than what the journal still
/// carries is refused too — the units between would be missing from both the view and the journal, which is the
/// same silent gap arriving by a different route.
pub fn take_after(
    previous: Option<&Path>,
    journal: &Path,
    dir: &Path,
    at: Option<u64>,
    results: Option<&Path>,
) -> Result<u64, Error> {
    let base = match previous {
        Some(path) => Some(View::load(path).map_err(|e| format!("the previous view {}: {e}", path.display()))?),
        None => None,
    };
    let floor = venue_node::node::journal_floor(journal)?;
    match (&base, floor) {
        (None, floor) if floor > 0 => {
            return Err(format!(
                "{} was cut at {floor}, so a pair taken from it alone would hold a view of only what followed: \
                 give the previously published view half to carry on from",
                journal.display()
            )
            .into())
        }
        (Some(view), floor) if view.sequence < floor => {
            return Err(format!(
                "the previous view is at {} but {} now starts after {floor}: the units in between are in the \
                 archive, not here, and a view cannot skip them (venue-node restore-cut)",
                view.sequence,
                journal.display()
            )
            .into())
        }
        _ => {}
    }

    std::fs::create_dir_all(dir)?;
    let (paired, borrowed) = match results {
        Some(path) => (path.to_path_buf(), true),
        None => {
            let path = dir.join("results.sbe");
            venue_node::node::replay(journal, &path)?;
            (path, false)
        }
    };

    // Where the pair can actually be taken, which is a property of the *result log*, not the journal. The
    // engine half could be taken at the journal's last input; the view half could not, because it is built from
    // units and a unit needs its results. A journal routinely runs ahead of its result log — inputs that
    // produced nothing leave no group behind, and `cut` drops the last unconfirmed one — and after a cut of the
    // committed fixture the journal reached input 122 while the results stopped at 94.
    //
    // Left alone that does not fail: `View::apply` advances the view's `sequence` for an input whether or not
    // results came with it, so the view would claim 122 having reflected nothing after 94, and the check that
    // the two halves agree would see 122 on both and be satisfied. Pin both halves to what the results cover
    // instead, and refuse an explicit `at` beyond it rather than quietly taking less than was asked for.
    let covered = venue_node::node::trusted_results(&paired)?.map(|(sequence, _)| sequence);
    let at = match (at, covered) {
        (Some(asked), Some(covered)) if asked > covered => {
            return Err(format!(
                "a pair was asked for at {asked}, but {} only carries results through {covered}: the view half \
                 would be built from units that have none, and would claim a sequence it never reached",
                paired.display()
            )
            .into())
        }
        (Some(asked), _) => Some(asked),
        (None, Some(covered)) => Some(covered),
        (None, None) => {
            return Err(format!("{} carries no results, so no view can be built from it", paired.display()).into())
        }
    };

    let engine_out = dir.join(ENGINE);
    // The engine half carries on from the previous pair's, as the view half does from its view, so the replay
    // covers what happened since that pair and not everything since the journal's own embedded snapshot — which on
    // a venue that is not cut grows every day. The two halves of a pair are taken at the same sequence (checked
    // below), so the previous engine half is exactly as far on as the previous view. Absent, or beyond `at`, it
    // is simply not used.
    let previous_engine = previous.and_then(Path::parent).map(|d| d.join(ENGINE)).filter(|p| p.exists());
    let (engine_at, _) = venue_node::snapshots::take_from(journal, &engine_out, at, previous_engine.as_deref())?;
    let view = View::replayed_from(base, journal, &paired, at);
    if !borrowed {
        // A byproduct of taking the view half, not part of the pair.
        let _ = std::fs::remove_file(&paired);
    }
    let view = view?;

    if view.sequence != engine_at {
        return Err(
            format!("the halves disagree: the engine's is at {engine_at}, the reader's at {}", view.sequence).into()
        );
    }
    view.save(&dir.join(VIEW))?;
    Ok(engine_at)
}

/// The sequence the engine's half of a pair was taken at.
fn engine_sequence(path: &Path) -> Result<u64, Error> {
    let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut buf = Vec::new();
    let n = venue_node::frames::read_frame(&mut reader, &mut buf)?.ok_or("the engine half is empty")?;
    let frame = venue_harness::codec::decode(&buf[..n])?;
    if frame["msg"] != "Snapshot" {
        return Err(format!("{} does not begin with a Snapshot frame", path.display()).into());
    }
    Ok(frame["record"]["sequence"].as_u64().unwrap_or(0))
}

/// The sequence a pair in `dir` was taken at, refusing one whose halves disagree or whose reader's half is
/// missing — what a consumer of a pair checks before trusting it.
pub fn sequence_of(dir: &Path) -> Result<u64, Error> {
    let view = View::load(&dir.join(VIEW))?;
    let engine = engine_sequence(&dir.join(ENGINE))?;
    if view.sequence != engine {
        return Err(
            format!("the halves disagree: the engine's is at {engine}, the reader's at {}", view.sequence).into()
        );
    }
    Ok(engine)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("venue-pair-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    fn fixture(dir: &Path) -> PathBuf {
        let recorded = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded");
        let journal = dir.join("journal.sbe");
        std::fs::copy(recorded.join("journal.sbe"), &journal).expect("the journal copies");
        std::fs::copy(recorded.join("capacity"), dir.join("capacity")).expect("the capacity copies");
        journal
    }

    #[test]
    fn both_halves_are_written_at_the_same_sequence() {
        let dir = scratch("both");
        let journal = fixture(&dir);
        let out = dir.join("pair");
        let at = take(&journal, &out, Some(62), None).expect("takes");
        assert_eq!(at, 62);
        assert_eq!(sequence_of(&out).expect("reads back"), 62, "the pair did not read back at the sequence taken");
        assert!(out.join(ENGINE).exists(), "no engine half");
        assert!(out.join(VIEW).exists(), "no reader half");
        assert!(!out.join("results.sbe").exists(), "the result log it replayed was left behind in the pair");
        assert_eq!(View::load(&out.join(VIEW)).expect("loads").sequence, 62);
    }

    #[test]
    fn a_pair_whose_halves_disagree_is_refused() {
        let dir = scratch("disagree");
        let journal = fixture(&dir);
        let out = dir.join("pair");
        take(&journal, &out, Some(62), None).expect("takes");
        // The reader's half replaced by one taken somewhere else entirely.
        let other = dir.join("other");
        take(&journal, &other, Some(41), None).expect("takes");
        std::fs::copy(other.join(VIEW), out.join(VIEW)).expect("swapped");

        let err = sequence_of(&out).expect_err("a mismatched pair was accepted");
        assert!(err.to_string().contains("disagree"), "{err}");
    }
}
