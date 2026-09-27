//! What a cut archives and what a cut removes have to be the same bytes, and a cut asked to prove its history
//! was stored must refuse when it was not.
//!
//! `cut-journal` on its own deletes. That was acceptable while nothing could be cut at all, and stops being
//! acceptable the moment it runs against a venue anyone cares about: the bytes it drops are the only copy of
//! everything before the snapshot. `export-cut` writes that stretch out first, `journal_ship.py` puts it in
//! object storage and reads it back, and `--require-shipped` is what makes the last step refuse to proceed on
//! anything less. These tests cover the two halves of that claim — that the segment really is the removed
//! bytes, and that the refusal really refuses.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use venue_node::node::{cut_checked, export_cut, replay};
use venue_node::snapshots::embed;

fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let nth = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("venue-export-cut-{name}-{}-{nth}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    dir
}

/// A data directory from the fixture with a real result log and a snapshot embedded at `at`.
fn prepared(name: &str, at: u64) -> PathBuf {
    let dir = scratch(name);
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded");
    let journal = dir.join("journal.sbe");
    std::fs::copy(fixture.join("journal.sbe"), &journal).expect("the journal copies");
    std::fs::copy(fixture.join("capacity"), dir.join("capacity")).expect("the capacity copies");
    let results = dir.join("results.sbe");
    replay(&journal, &results).expect("the journal replays");
    embed(&journal, Some(at)).expect("a snapshot embeds");
    dir
}

/// Writes the receipt `journal_ship.py` would write, so the Rust side can be tested without object storage.
/// `tamper` gets to change it first — that is the whole point of several of these.
fn receipt(dir: &Path, at: u64, tamper: impl FnOnce(&mut serde_json::Value)) {
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("segment.json")).expect("a manifest")).expect("json");
    let mut receipt = serde_json::json!({
        "at": at,
        "environment": "test",
        "verified": true,
        "journal_key": "journals/test/x.journal.sbe",
        "results_key": "journals/test/x.results.sbe",
        "journal_sha256": manifest["journal_sha256"],
        "results_sha256": manifest["results_sha256"],
    });
    tamper(&mut receipt);
    std::fs::write(dir.join("shipped.json"), serde_json::to_string(&receipt).expect("json")).expect("written");
}

/// The load-bearing property. Everything the cut takes away is exactly what the segment kept — not merely the
/// same byte count, and not merely "some of it": the journal the cut leaves behind plus the journal half of the
/// segment must account for every frame the original journal had, and nothing twice.
#[test]
fn the_segment_holds_exactly_what_the_cut_removes() {
    let dir = prepared("exact", 62);
    let (journal, results) = (dir.join("journal.sbe"), dir.join("results.sbe"));
    let before = std::fs::read(&journal).expect("the journal reads");
    let results_before = std::fs::read(&results).expect("the results read");

    let out = dir.join("segment");
    let segment = export_cut(&journal, &results, &out).expect("a segment exports");
    let archived_journal = std::fs::read(&segment.journal).expect("the archived journal reads");
    let archived_results = std::fs::read(&segment.results).expect("the archived results read");

    cut_checked(&journal, &results, None).expect("cuts");
    let kept_journal = std::fs::read(&journal).expect("the cut journal reads");
    let kept_results = std::fs::read(&results).expect("the cut results read");

    assert_eq!(
        archived_journal.len() + kept_journal.len(),
        before.len(),
        "the segment and the cut journal do not add up to the journal that was there"
    );
    // The result log's removal is a prefix, so the archived half can be checked byte for byte against it.
    assert_eq!(archived_results, &results_before[..archived_results.len()], "the archived results are not the prefix");
    // The kept half continues exactly where the archived half stopped: no gap, no overlap.
    let resumes_at = archived_results.len();
    assert_eq!(
        kept_results,
        &results_before[resumes_at..resumes_at + kept_results.len()],
        "the kept results do not continue where the archived ones stopped"
    );
    // But the two need not add up to the whole file, and it would be wrong to assert that they do: `cut` calls
    // `repair_results` first, which drops an *unconfirmed tail* as well — the last group in a result log is
    // never trusted, since nothing distinguishes one a crash cut short from one nothing has followed yet. So a
    // cut removes a prefix (archived here) and possibly a tail (which a replay regenerates, and which is
    // therefore not history to keep). `export_cut` deliberately reads with `trusted_results` instead, because
    // it must not write to the file it is archiving.
    assert!(resumes_at + kept_results.len() <= results_before.len(), "the cut kept more result bytes than were there");
}

/// A segment only exists where a cut would be allowed, so the refusals have to match. A journal with no
/// snapshot has nothing a cut could remove, and exporting one would write a segment that means nothing.
#[test]
fn a_journal_with_no_snapshot_exports_nothing() {
    let dir = scratch("nosnapshot");
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../interfaces/audit/recorded");
    let journal = dir.join("journal.sbe");
    std::fs::copy(fixture.join("journal.sbe"), &journal).expect("the journal copies");
    let results = dir.join("results.sbe");
    replay(&journal, &results).expect("the journal replays");

    let refused = export_cut(&journal, &results, &dir.join("segment")).expect_err("exported without a snapshot");
    assert!(refused.to_string().contains("no embedded snapshot"), "unhelpful refusal: {refused}");
}

/// Exporting must not change the files it reads — it runs against a venue that may still be serving, and the
/// whole point is that nothing is discarded until the archive is proven.
#[test]
fn exporting_changes_neither_file() {
    let dir = prepared("readonly", 62);
    let (journal, results) = (dir.join("journal.sbe"), dir.join("results.sbe"));
    let (before, results_before) = (std::fs::read(&journal).unwrap(), std::fs::read(&results).unwrap());

    export_cut(&journal, &results, &dir.join("segment")).expect("a segment exports");

    assert_eq!(std::fs::read(&journal).unwrap(), before, "export-cut rewrote the journal");
    assert_eq!(std::fs::read(&results).unwrap(), results_before, "export-cut rewrote the result log");
}

/// A second export into the same directory would overwrite a segment that may not have been shipped yet.
#[test]
fn a_directory_that_already_holds_a_segment_is_refused() {
    let dir = prepared("twice", 62);
    let (journal, results) = (dir.join("journal.sbe"), dir.join("results.sbe"));
    let out = dir.join("segment");
    export_cut(&journal, &results, &out).expect("the first export");
    let refused = export_cut(&journal, &results, &out).expect_err("the second export overwrote the first");
    assert!(refused.to_string().contains("already exists"), "unhelpful refusal: {refused}");
}

/// `--require-shipped` with a real receipt lets the cut through, and the cut still does what it always did.
#[test]
fn a_cut_proceeds_when_the_segment_really_was_shipped() {
    let dir = prepared("shipped", 62);
    let (journal, results) = (dir.join("journal.sbe"), dir.join("results.sbe"));
    let out = dir.join("segment");
    export_cut(&journal, &results, &out).expect("a segment exports");
    receipt(&out, 62, |_| {});

    let (at, journal_removed, _) = cut_checked(&journal, &results, Some(&out)).expect("cuts with a receipt");
    assert_eq!(at, 62);
    assert!(journal_removed > 0, "the cut removed nothing");
}

/// No receipt at all is the ordinary case of someone reaching for `--require-shipped` before shipping.
#[test]
fn a_cut_is_refused_when_nothing_was_shipped() {
    let dir = prepared("unshipped", 62);
    let (journal, results) = (dir.join("journal.sbe"), dir.join("results.sbe"));
    let out = dir.join("segment");
    export_cut(&journal, &results, &out).expect("a segment exports");
    let before = std::fs::read(&journal).expect("the journal reads");

    let refused = cut_checked(&journal, &results, Some(&out)).expect_err("cut without a receipt");
    assert!(refused.to_string().contains("shipped.json"), "unhelpful refusal: {refused}");
    assert_eq!(std::fs::read(&journal).unwrap(), before, "a refused cut still changed the journal");
}

/// The dangerous one. A receipt from an *earlier* segment is a real archive of real bytes, verified against real
/// object storage — just not the bytes this cut is about to destroy. Nothing about it looks wrong; only the
/// sequence gives it away.
#[test]
fn a_receipt_for_a_different_snapshot_does_not_authorise_this_cut() {
    let dir = prepared("stale", 62);
    let (journal, results) = (dir.join("journal.sbe"), dir.join("results.sbe"));
    let out = dir.join("segment");
    export_cut(&journal, &results, &out).expect("a segment exports");
    receipt(&out, 62, |r| r["at"] = serde_json::json!(30));
    let before = std::fs::read(&journal).expect("the journal reads");

    let refused = cut_checked(&journal, &results, Some(&out)).expect_err("cut on a receipt for another snapshot");
    assert!(refused.to_string().contains("not for the snapshot at 62"), "unhelpful refusal: {refused}");
    assert_eq!(std::fs::read(&journal).unwrap(), before, "a refused cut still changed the journal");
}

/// The genuinely dangerous case, and the reason the check is made against the *segment's manifest* and not only
/// against the receipt. Nothing here is forged or stale in any way a person would notice: a segment was exported
/// at 30 and honestly shipped, and the receipt says so truthfully. Then the venue ran on and a newer snapshot
/// was embedded at 62. Pointing the cut at that older segment now would destroy everything between 30 and 62 —
/// which no archive anywhere contains — while every individual artifact involved checks out.
#[test]
fn a_segment_archived_before_a_newer_snapshot_does_not_authorise_cutting_to_that_newer_one() {
    let dir = prepared("superseded", 30);
    let (journal, results) = (dir.join("journal.sbe"), dir.join("results.sbe"));
    let out = dir.join("segment");
    export_cut(&journal, &results, &out).expect("a segment exports at 30");
    receipt(&out, 30, |_| {}); // an honest receipt, for the snapshot that was current when it was shipped

    embed(&journal, Some(62)).expect("the venue runs on and a newer snapshot embeds");
    let before = std::fs::read(&journal).expect("the journal reads");

    let refused = cut_checked(&journal, &results, Some(&out)).expect_err("cut to 62 on a segment archived at 30");
    assert!(
        refused.to_string().contains("does not contain what is about to be removed"),
        "unhelpful refusal: {refused}"
    );
    assert_eq!(std::fs::read(&journal).unwrap(), before, "a refused cut still changed the journal");
}

/// A receipt whose digests are not the segment's own: the archive is of something else, whatever it says.
#[test]
fn a_receipt_naming_other_bytes_does_not_authorise_this_cut() {
    let dir = prepared("digest", 62);
    let (journal, results) = (dir.join("journal.sbe"), dir.join("results.sbe"));
    let out = dir.join("segment");
    export_cut(&journal, &results, &out).expect("a segment exports");
    receipt(&out, 62, |r| r["journal_sha256"] = serde_json::json!("0".repeat(64)));

    let refused = cut_checked(&journal, &results, Some(&out)).expect_err("cut on a receipt for other bytes");
    assert!(refused.to_string().contains("not for these bytes"), "unhelpful refusal: {refused}");
}

/// And a ship that failed partway must not read as a ship that worked.
#[test]
fn a_receipt_that_does_not_claim_verification_is_refused() {
    let dir = prepared("unverified", 62);
    let (journal, results) = (dir.join("journal.sbe"), dir.join("results.sbe"));
    let out = dir.join("segment");
    export_cut(&journal, &results, &out).expect("a segment exports");
    receipt(&out, 62, |r| r["verified"] = serde_json::json!(false));

    let refused = cut_checked(&journal, &results, Some(&out)).expect_err("cut on an unverified receipt");
    assert!(refused.to_string().contains("verified"), "unhelpful refusal: {refused}");
}
