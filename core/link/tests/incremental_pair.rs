//! A pair taken from a cut journal, carrying on from the previous pair, has to equal one taken from the whole
//! history.
//!
//! Cutting and snapshotting used to undo each other. A cut journal no longer starts at sequence 1, so the
//! engine half fail-stopped on E-4.1 — loudly, which was survivable. The view half did something worse: it is
//! built by applying units in order from an empty state, and the units before the cut are gone, so it returned
//! a view holding only what followed, with no error and a `sequence` that still matched the engine's. The two
//! halves agreed with each other and were both wrong.
//!
//! So the view half now carries on from the previously published one, and these check the two things that
//! makes true: that the result is the *same view* the whole history would have produced, and that the ways of
//! getting it wrong are refused rather than quietly accepted.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use venue_link::pair;
use venue_link::view::View;
use venue_node::node::{cut_checked, export_cut, replay};
use venue_node::snapshots::embed;

fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nth = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("venue-incremental-pair-{name}-{}-{nth}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// A data directory from the fixture with a real result log.
fn prepared(name: &str) -> PathBuf {
    let dir = scratch(name);
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded");
    std::fs::copy(fixture.join("journal.sbe"), dir.join("journal.sbe")).expect("the journal copies");
    std::fs::copy(fixture.join("capacity"), dir.join("capacity")).expect("the capacity copies");
    replay(&dir.join("journal.sbe"), &dir.join("results.sbe")).expect("the journal replays");
    dir
}

/// The load-bearing test, and the whole reason for the incremental path. Take a pair at 62 from the full
/// journal; cut to 62; then take a pair at the end carrying on from the first. It must equal the pair the
/// *uncut* journal would have produced at the same sequence — not merely have the same sequence on it.
#[test]
fn a_pair_taken_across_a_cut_equals_one_taken_from_the_whole_history() {
    let dir = prepared("equivalence");
    let (journal, results) = (dir.join("journal.sbe"), dir.join("results.sbe"));

    // Both pairs are taken at one explicit sequence, and that matters. Left to itself each would land on its
    // own result log's trusted point, and those differ across a cut: `cut` drops the last unconfirmed group, so
    // the same data covers one group less afterwards. Comparing views taken at different sequences would
    // compare two different questions. 90 is inside what the fixture's results cover on both sides (they reach
    // 94 after the cut) and comfortably past the cut point.
    const AT: u64 = 90;

    // What the answer is, computed before anything is cut away.
    let whole = dir.join("whole");
    let at = pair::take(&journal, &whole, Some(AT), Some(&results)).expect("a pair from the whole history");

    // The pair that will be carried on from, and the cut.
    let first = dir.join("first");
    pair::take(&journal, &first, Some(62), Some(&results)).expect("the first pair");
    embed(&journal, Some(62)).expect("a snapshot embeds");
    export_cut(&journal, &results, &dir.join("segment")).expect("a segment exports");
    cut_checked(&journal, &results, None).expect("cuts");

    // And the same point, reached incrementally from a journal that no longer holds the beginning.
    let carried = dir.join("carried");
    let taken = pair::take_after(Some(&first.join(pair::VIEW)), &journal, &carried, Some(AT), Some(&results))
        .expect("a pair carrying on from the first");
    assert_eq!(taken, at, "the incremental pair landed on a different sequence");

    let expected = View::load(&whole.join(pair::VIEW)).expect("the whole-history view loads");
    let actual = View::load(&carried.join(pair::VIEW)).expect("the carried view loads");
    assert_eq!(
        serde_json::to_value(&actual).expect("json"),
        serde_json::to_value(&expected).expect("json"),
        "carrying on from the previous view did not reproduce the view the whole history gives"
    );
}

/// The silent case, now refused. Taking a pair from a cut journal with nothing to carry on from would produce a
/// view of only what followed the cut — and it would pass every check, because its sequence is right.
#[test]
fn a_cut_journal_alone_will_not_give_a_pair() {
    let dir = prepared("alone");
    let (journal, results) = (dir.join("journal.sbe"), dir.join("results.sbe"));
    embed(&journal, Some(62)).expect("a snapshot embeds");
    export_cut(&journal, &results, &dir.join("segment")).expect("a segment exports");
    cut_checked(&journal, &results, None).expect("cuts");

    let refused = pair::take(&journal, &dir.join("out"), None, Some(&results)).expect_err("took a pair alone");
    assert!(refused.to_string().contains("give the previously published view"), "unhelpful refusal: {refused}");
}

/// And a previous view from before what the journal still carries leaves the same gap, arriving by a different
/// route: the units between it and the journal's floor are in the archive, not here.
#[test]
fn a_previous_view_older_than_the_cut_is_refused() {
    let dir = prepared("stale");
    let (journal, results) = (dir.join("journal.sbe"), dir.join("results.sbe"));

    let early = dir.join("early");
    pair::take(&journal, &early, Some(30), Some(&results)).expect("an early pair");

    embed(&journal, Some(62)).expect("a snapshot embeds");
    export_cut(&journal, &results, &dir.join("segment")).expect("a segment exports");
    cut_checked(&journal, &results, None).expect("cuts at 62");

    let refused = pair::take_after(Some(&early.join(pair::VIEW)), &journal, &dir.join("out"), None, Some(&results))
        .expect_err("took a pair from a view older than the cut");
    assert!(refused.to_string().contains("units in between"), "unhelpful refusal: {refused}");
}

/// Carrying on from a base view is a fold: applying units 1..n must equal applying 1..m and then m+1..n. This
/// holds it to that independently of any cutting, which is what made it possible to tell, when the two sides
/// first disagreed, that the fold was sound and the cut had changed the units underneath it.
#[test]
fn carrying_on_from_a_view_is_the_same_as_never_having_stopped() {
    let dir = prepared("diag");
    let (journal, results) = (dir.join("journal.sbe"), dir.join("results.sbe"));
    let whole = View::replayed(&journal, &results, None).expect("whole");
    let base = View::replayed(&journal, &results, Some(62)).expect("base at 62");
    let carried = View::replayed_from(Some(base), &journal, &results, None).expect("carried");
    assert_eq!(
        serde_json::to_value(&carried).expect("json"),
        serde_json::to_value(&whole).expect("json"),
        "carrying on over an UNCUT journal already differs: the fold itself is at fault, not the cut"
    );
}

/// An uncut journal still needs nothing, so the requirement is a consequence of cutting rather than a new tax
/// on everybody.
#[test]
fn an_uncut_journal_still_gives_a_pair_on_its_own() {
    let dir = prepared("uncut");
    let (journal, results) = (dir.join("journal.sbe"), dir.join("results.sbe"));
    pair::take(&journal, &dir.join("out"), Some(62), Some(&results)).expect("a pair from an uncut journal");
}

/// A result log that still reaches back further than the journal does. `cut` trims both together, so this does
/// not arise from cutting alone — but it does the moment the two are restored, replaced or copied
/// independently, and the pairing walks them in lockstep rather than looking each input up. Results naming
/// inputs the journal no longer carries have to be stepped over: attaching input 1's results to input 63 would
/// corrupt the view silently, and stopping at the first mismatch would leave every later input with none.
#[test]
fn results_reaching_back_further_than_the_journal_are_stepped_over() {
    let dir = prepared("stale-results");
    let (journal, results) = (dir.join("journal.sbe"), dir.join("results.sbe"));

    let base = View::replayed(&journal, &results, Some(62)).expect("a base at 62");
    embed(&journal, Some(62)).expect("a snapshot embeds");
    let segment = dir.join("segment");
    export_cut(&journal, &results, &segment).expect("a segment exports");
    cut_checked(&journal, &results, None).expect("cuts");

    // Restoring gives back a result log reaching to input 1, while the journal beside it still starts at 63.
    let restored = dir.join("restored");
    venue_node::node::restore_cut(&segment, &journal, &results, &restored).expect("the segment restores");
    let full_results = restored.join("results.sbe");

    let from_cut =
        View::replayed_from(Some(base.clone()), &journal, &results, Some(90)).expect("from the cut result log");
    let from_full = View::replayed_from(Some(base), &journal, &full_results, Some(90))
        .expect("from the result log that reaches back further");
    assert_eq!(
        serde_json::to_value(&from_full).expect("json"),
        serde_json::to_value(&from_cut).expect("json"),
        "results reaching back before the journal changed what the view became"
    );
    assert_eq!(from_full.sequence, 90, "the view did not reach the sequence asked for");
}
