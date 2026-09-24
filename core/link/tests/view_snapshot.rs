//! A view snapshot is taken by replaying the unit stream, and restoring one leaves a reader indistinguishable
//! from one that applied every unit itself — the subscriber-side reading of E-20.9.
//!
//! This is what lets a reader start somewhere other than the beginning: a cold subscriber, or one in an
//! environment founded on a snapshot rather than on genesis, can be handed this instead of the whole history.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use venue_link::view::View;
use venue_link::wire;

fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nth = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("venue-view-snapshot-{name}-{}-{nth}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// The recorded fixture's journal, with the result log a replay of it produces — the two files a reader's unit
/// stream is rebuilt from.
fn fixture(dir: &Path) -> (PathBuf, PathBuf) {
    let recorded = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded");
    let journal = dir.join("journal.sbe");
    std::fs::copy(recorded.join("journal.sbe"), &journal).expect("the journal copies");
    std::fs::copy(recorded.join("capacity"), dir.join("capacity")).expect("the capacity copies");
    let results = dir.join("results.sbe");
    venue_node::node::replay(&journal, &results).expect("the fixture journal replays");
    (journal, results)
}

/// Views compare by what they serialise to: `View` has no `PartialEq`, and the serialised form is exactly the
/// part that has to survive a round trip anyway.
fn same(left: &View, right: &View) -> bool {
    serde_json::to_string(left).expect("serialises") == serde_json::to_string(right).expect("serialises")
}

#[test]
fn a_saved_view_reads_back_identical() {
    let dir = scratch("round-trip");
    let (journal, results) = fixture(&dir);
    let taken = View::replayed(&journal, &results, Some(62)).expect("replays");
    assert!(taken.sequence > 0, "the fixture produced an empty view; the test proves nothing");

    let path = dir.join("view.json");
    taken.save(&path).expect("saves");
    let restored = View::load(&path).expect("loads");
    assert!(same(&taken, &restored), "a view did not survive being written and read back");
}

/// The property this exists for: a reader handed a view snapshot taken at N, and then sent only what follows N,
/// ends up where a reader that was sent everything from the beginning ends up.
#[test]
fn a_restored_view_plus_the_tail_equals_having_applied_everything() {
    let dir = scratch("equivalence");
    let (journal, results) = fixture(&dir);

    let whole = View::replayed(&journal, &results, None).expect("replays");
    assert!(whole.sequence > 62, "the fixture is too short to cut in the middle: {}", whole.sequence);

    let path = dir.join("view.json");
    View::replayed(&journal, &results, Some(62)).expect("replays").save(&path).expect("saves");
    let mut resumed = View::load(&path).expect("loads");
    assert_eq!(resumed.sequence, 62, "the snapshot was not taken where it was asked for");

    for unit in wire::replayed_units(&journal, &results).expect("units") {
        if unit.input["record"]["sequence"].as_u64().unwrap_or(0) <= 62 {
            continue;
        }
        resumed.apply(&unit);
    }

    // `version` counts how many units this particular reader applied, so it legitimately differs: one reader
    // applied the whole history and the other only the tail. Everything a reader actually shows must match.
    let (mut a, mut b) = (serde_json::to_value(&whole).expect("value"), serde_json::to_value(&resumed).expect("value"));
    for v in [&mut a, &mut b] {
        v.as_object_mut().expect("an object").remove("version");
    }
    assert_eq!(a, b, "resuming from a view snapshot did not land where applying everything lands");
}

/// A `Snapshot` frame embedded in the journal is not an input, and the feed never sends one to a subscriber —
/// so rebuilding "what the feed carried" from the files must not produce one either.
///
/// This bites hardest in the case §3.7 describes: `embed` appends wherever the journal currently ends, so a
/// snapshot taken at an *earlier* sequence sits physically *after* later inputs. Applied as though it were a
/// unit, its record header would drag the view's own sequence backwards — and a reader that then resumed from
/// that sequence would be re-sent inputs it had already applied, accumulating fills and trades twice.
#[test]
fn an_embedded_snapshot_is_not_applied_as_though_it_were_an_input() {
    let dir = scratch("embedded");
    let (journal, results) = fixture(&dir);
    let whole = View::replayed(&journal, &results, None).expect("replays");
    let last = whole.sequence;

    // A snapshot for an early sequence, embedded once the journal already runs well past it.
    venue_node::snapshots::embed(&journal, Some(62)).expect("a snapshot embeds");
    let after = View::replayed(&journal, &results, None).expect("replays");

    assert_eq!(
        after.sequence, last,
        "embedding a snapshot for sequence 62 moved the view's own sequence; it was applied as an input"
    );
    let (mut a, mut b) = (serde_json::to_value(&whole).expect("value"), serde_json::to_value(&after).expect("value"));
    for v in [&mut a, &mut b] {
        v.as_object_mut().expect("an object").remove("version");
    }
    assert_eq!(a, b, "embedding a snapshot changed what a reader would hold");
}

/// Why this is replayed rather than projected from the engine's own snapshot (spec §20): the engine keeps only
/// what it needs to match on, so some of what a reader shows was never engine state at all. `defined_at` is the
/// one this fixture can demonstrate — the engine has no such field, so a reader rebuilt from engine state would
/// have nothing to put there and a calendar counting an event's stages would read zero.
///
/// The fixture cannot demonstrate the rest of that list: it produces no parlay legs, no resolutions and no
/// part-filled orders, so nothing here would notice if those stopped being carried. `View`'s own
/// `every_field_survives_being_saved_and_loaded` covers them against a view built by hand instead.
#[test]
fn a_view_snapshot_carries_an_events_timestamps_which_the_engine_never_had() {
    let dir = scratch("carries");
    let (journal, results) = fixture(&dir);
    let view = View::replayed(&journal, &results, None).expect("replays");

    assert!(
        view.events.values().any(|e| e.defined_at != 0),
        "no event carried a defined_at; this test cannot guard the loss of a field nothing sets"
    );
    assert!(!view.ladders.is_empty(), "no tick ladders; without these no reader can turn a tick into a price");
}
