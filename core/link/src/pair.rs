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
    std::fs::create_dir_all(dir)?;
    let engine_out = dir.join(ENGINE);
    let (engine_at, _) = venue_node::snapshots::take(journal, &engine_out, at)?;

    let (paired, borrowed) = match results {
        Some(path) => (path.to_path_buf(), true),
        None => {
            let path = dir.join("results.sbe");
            venue_node::node::replay(journal, &path)?;
            (path, false)
        }
    };
    let view = View::replayed(journal, &paired, at);
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
