#!/usr/bin/env python3
"""Ship a cut segment — what `venue-node export-cut` wrote — to object storage, and leave a receipt saying so.

  journal_ship.py SEGMENT_DIR --to s3://BUCKET --environment NAME
  journal_ship.py SEGMENT_DIR --to /some/directory --environment NAME   on disk, for a demo or a test
  journal_ship.py --self-test                                          check the tool against files it writes

This is the step that makes cutting a journal *archiving* rather than deleting. `export-cut` writes out exactly
what a cut would remove without touching the live files; this puts those bytes somewhere durable and reads them
back to prove it; then `venue-node cut-journal --require-shipped SEGMENT_DIR` refuses to discard anything unless
the receipt here covers that same snapshot, digest for digest. Nothing in the chain trusts the previous step's
word for it: §7.5 is in the handoff because a delete was once reported that had not happened.

The readiness test is the one §8 item 7(c) asks for and is not the audit shipper's. An audit file is ready when
it is closed and its day is over; a journal has no such marker, and a *sequence* is the only thing that means
anything here — a segment is ready exactly when there is a durable snapshot it sits before, which is what
`export-cut` refuses without. That refusal is the readiness test, so this tool does not repeat it: it ships what
it is given and verifies what it shipped.
"""
import argparse
import json
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from snapshot_publish import Destination, digest, publish  # noqa: E402  (after sys.path)

MANIFEST = "segment.json"
RECEIPT = "shipped.json"


def ship(segment_dir, destination):
    """Uploads both halves of the segment in `segment_dir`, verifies each against the digest the manifest
    recorded *and* against what object storage gives back, then writes the receipt. Returns it.

    Two things are checked, and it is worth being precise about which does the work. `publish` reads the object
    back and compares, which catches a transport that lost or rewrote bytes; it also returns the digest it
    actually sent, and comparing *that* against the manifest is what catches a segment whose files changed on
    disk after `export-cut` measured them. Either way that second case is caught — the up-front `digest(path)`
    below is **not** a third guarantee, and removing it would not let a bad segment through. It is there only so
    that such a segment is refused *before* anything is uploaded, rather than after: no half-written object is
    left in the bucket for someone to find later and wonder about."""
    segment_dir = Path(segment_dir)
    manifest = json.loads((segment_dir / MANIFEST).read_text())
    at = manifest["at"]

    receipt = {"at": at, "environment": destination.environment, "verified": False}
    for half in ("journal", "results"):
        path = segment_dir / manifest[half]
        recorded = manifest[f"{half}_sha256"]
        if digest(path) != recorded:
            raise ValueError(f"{path} is not the file {MANIFEST} describes: it changed after export-cut measured it")
        key, sha = publish(path, destination, f"{at}.{half}", "sbe")
        if sha != recorded:
            raise ValueError(f"{path} was published as {sha}, which is not the recorded {recorded}")
        receipt[f"{half}_key"] = key
        receipt[f"{half}_sha256"] = sha

    receipt["verified"] = True
    (segment_dir / RECEIPT).write_text(json.dumps(receipt, indent=2) + "\n")
    return receipt


def self_test():
    failures = []
    with tempfile.TemporaryDirectory() as work, tempfile.TemporaryDirectory() as store:
        segment = Path(work) / "segment"
        segment.mkdir()

        def lay_out(at=4321, journal=b"journal frames, for the self-test", results=b"result frames, likewise"):
            (segment / f"journal-{at}.sbe").write_bytes(journal)
            (segment / f"results-{at}.sbe").write_bytes(results)
            (segment / MANIFEST).write_text(json.dumps({
                "at": at,
                "first_sequence": 1,
                "last_sequence": at,
                "journal": f"journal-{at}.sbe",
                "results": f"results-{at}.sbe",
                "journal_sha256": digest(segment / f"journal-{at}.sbe"),
                "results_sha256": digest(segment / f"results-{at}.sbe"),
            }))

        lay_out()
        destination = Destination(store, "selftest", prefix="journals")
        receipt = ship(segment, destination)

        if receipt["journal_key"] != "journals/selftest/4321.journal.sbe":
            failures.append(f"unexpected journal key: {receipt['journal_key']}")
        if not (Path(store) / "journals" / "selftest" / "4321.results.sbe").exists():
            failures.append("the results half is not where the receipt says")
        if not (Path(store) / "journals" / "selftest" / "4321.journal.sha256").exists():
            failures.append("no .sha256 sidecar for the journal half")
        if receipt["verified"] is not True:
            failures.append("a successful ship did not record verified")
        written = json.loads((segment / RECEIPT).read_text())
        if written != receipt:
            failures.append("the receipt on disk is not the receipt returned")
        if written["journal_sha256"] != json.loads((segment / MANIFEST).read_text())["journal_sha256"]:
            failures.append("the receipt's digest is not the manifest's digest")

        # A segment file edited after export-cut measured it must be refused, not archived under a digest that
        # no longer describes it. This is the check that keeps the receipt meaningful to `--require-shipped`.
        stale = Path(work) / "stale"
        stale.mkdir()
        segment, real_segment = stale, segment
        lay_out(at=99)
        (stale / "journal-99.sbe").write_bytes(b"changed underneath")
        try:
            ship(stale, Destination(store, "selftest", prefix="journals"))
            failures.append("a segment whose bytes changed after export was shipped anyway")
        except ValueError:
            pass
        if (stale / RECEIPT).exists():
            failures.append("a refused ship still left a receipt behind")
        # And it was refused before anything went to the destination, which is the only thing the up-front
        # digest check buys over the comparison `publish` already makes. Without that check this assertion
        # fails and the rest of the stale case still passes — which is exactly what it is here to pin down.
        if list((Path(store) / "journals" / "selftest").glob("99.*")):
            failures.append("a segment that changed after export was uploaded before being refused")
        segment = real_segment

        # And a destination that rewrites what it is given must fail the ship, not produce a receipt for bytes
        # object storage does not actually hold.
        corrupt = Path(work) / "corrupt"
        corrupt.mkdir()
        segment = corrupt
        lay_out(at=7)
        real_fetch = Destination.fetch

        def corrupting_fetch(self, key, into):
            real_fetch(self, key, into)
            if key.endswith(".sbe"):
                Path(into).write_bytes(b"not what was uploaded")

        Destination.fetch = corrupting_fetch
        try:
            ship(corrupt, Destination(store, "selftest", prefix="journals"))
            failures.append("a segment that came back different from what was sent was reported shipped")
        except ValueError:
            pass
        finally:
            Destination.fetch = real_fetch
        if (corrupt / RECEIPT).exists():
            failures.append("a ship that failed verification still left a receipt behind")

    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


def main():
    parser = argparse.ArgumentParser(add_help=True, description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("segment", nargs="?", help="the directory venue-node export-cut wrote")
    parser.add_argument("--to")
    parser.add_argument("--environment", help="which venue this is for: prod, pr-4. Names part of the key.")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        return self_test()
    if not args.segment or not args.to or not args.environment:
        parser.error("SEGMENT_DIR, --to and --environment are all needed")

    try:
        receipt = ship(args.segment, Destination(args.to, args.environment, prefix="journals"))
    except (ValueError, KeyError, subprocess.CalledProcessError, OSError) as e:
        print(f"not shipped: {e}", file=sys.stderr)
        return 1
    print(f"shipped {receipt['journal_key']} and {receipt['results_key']}")
    print(f"now: venue-node cut-journal JOURNAL RESULTS --require-shipped {args.segment}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
