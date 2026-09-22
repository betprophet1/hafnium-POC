#!/usr/bin/env python3
"""Take closed audit files to object storage and off the disk they were written on (interfaces/audit/README.md).

  audit_ship.py --dir DIR --to s3://BUCKET/PREFIX [--retain-until DATE] [--keep] [--dry-run]
  audit_ship.py --dir DIR --to /some/directory              a destination on disk, for a demo or a test
  audit_ship.py --self-test                                 check the tool against files it writes

A gateway that cannot write its audit log does not run, and its volume grows until it is full, so this is what
keeps the venue up rather than housekeeping. Nothing is removed until it is somewhere else and has been read back
and compared; `--keep` skips the removal entirely.
"""
import argparse
import hashlib
import json
import shutil
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from audit_verify import FORMAT, ZEROS, verify  # noqa: E402  (a sibling tool, not a package)

MANIFEST = "audit-archive.jsonl"


def today():
    return datetime.now(timezone.utc).strftime("%Y-%m-%d")


def now():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def day_of(name):
    """The UTC day in `<component>.<instance>.<YYYY-MM-DD>.<segment>.jsonl`, or None if it is not one of ours."""
    parts = name.split(".")
    if len(parts) != 5 or parts[4] != "jsonl":
        return None
    try:
        datetime.strptime(parts[2], "%Y-%m-%d")
    except ValueError:
        return None
    return parts[2]


def closed(path):
    """Whether a file's last line is its close record. The format promises nothing is ever appended after one."""
    try:
        lines = path.read_bytes().rstrip(b"\n").split(b"\n")
        return json.loads(lines[-1]).get("kind") == "close"
    except (OSError, ValueError, IndexError):
        return False


def opening(path):
    """A file's `open` record, which carries the `after` that names the file before it."""
    try:
        with path.open("rb") as f:
            return json.loads(f.readline())
    except (OSError, ValueError):
        return {}


def ready(directory, day):
    """The files worth taking: ours, closed, and from a day that is over.

    A file from today is never taken even when closed. The next segment number for a day is worked out from the
    files still on the disk, so removing today's `0000` would have the next process of that instance open `0000`
    again — a different file under a name the archive already holds.
    """
    out = []
    for path in sorted(Path(directory).iterdir()):
        if not path.is_file():
            continue
        on = day_of(path.name)
        if on is None or on >= day or not closed(path):
            continue
        out.append(path)
    return out


def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


class Destination:
    """Somewhere to put an object and read it back. A directory or a bucket; the difference is two commands."""

    def __init__(self, where, retain_until=None):
        self.where = where.rstrip("/")
        self.retain_until = retain_until
        self.s3 = where.startswith("s3://")

    def key(self, day, name):
        return f"audit/{day}/{name}"

    def put(self, path, key):
        if not self.s3:
            target = Path(self.where) / key
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, target)
            return
        args = ["aws", "s3api", "put-object", "--bucket", self.bucket(), "--key", self.prefixed(key),
                "--body", str(path), "--checksum-algorithm", "SHA256"]
        if self.retain_until:
            # Compliance mode is the bucket's, not this tool's: it can set a retention but never shorten one.
            args += ["--object-lock-mode", "COMPLIANCE", "--object-lock-retain-until-date", self.retain_until]
        subprocess.run(args, check=True, capture_output=True)

    def fetch(self, key, into):
        if not self.s3:
            shutil.copy2(Path(self.where) / key, into)
            return
        subprocess.run(["aws", "s3api", "get-object", "--bucket", self.bucket(), "--key", self.prefixed(key),
                        str(into)], check=True, capture_output=True)

    def bucket(self):
        return self.where[len("s3://"):].split("/", 1)[0]

    def prefixed(self, key):
        rest = self.where[len("s3://"):].split("/", 1)
        return f"{rest[1]}/{key}" if len(rest) > 1 else key


def append(path, record):
    """Writes one record, returning its hash so the next line can chain to it."""
    raw = json.dumps(record, separators=(",", ":"), sort_keys=True).encode()
    with path.open("ab") as f:
        f.write(raw + b"\n")
    return hashlib.sha256(raw).hexdigest()


def manifest_line(directory, fields):
    """Appends one line to the local manifest, chained to the line before it as an audit log is.

    The manifest *is* an audit log — it opens with an `open` record naming itself and its format, so the tool that
    verifies a gateway's files verifies this too and nothing new has to be trusted to read the archive.
    """
    path = Path(directory) / MANIFEST
    previous, n = ZEROS, 0
    if path.exists():
        lines = path.read_bytes().rstrip(b"\n").split(b"\n")
        if lines and lines[0]:
            previous = hashlib.sha256(lines[-1]).hexdigest()
            n = json.loads(lines[-1])["n"] + 1
    common = {"component": "archive", "instance": "audit_ship"}
    if n == 0:
        previous = append(path, {"ts": now(), "n": 0, "kind": "open", "format": FORMAT, "file": MANIFEST,
                                 "after": None, "prev": ZEROS, **common})
        n = 1
    append(path, {"ts": now(), "n": n, "kind": "archived", "prev": previous, **common, **fields})
    return path


def ship(directory, destination, keep=False, dry_run=False, day=None):
    """Takes every ready file. Returns (taken, skipped, problems)."""
    day = day or today()
    taken, skipped, problems = [], [], []
    for path in ready(directory, day):
        ok, message = verify(path, require_closed=True)
        if not ok:
            # Shipping a file that does not verify archives a problem rather than a record.
            problems.append(message)
            continue
        on = day_of(path.name)
        key = destination.key(on, path.name)
        sha = digest(path)
        if dry_run:
            skipped.append(f"{path.name} -> {key} ({sha[:12]}…, would not be removed)")
            continue

        destination.put(path, key)
        # Read back before anything is removed: an upload that was refused, truncated or silently rewritten must
        # not be the reason a file stops existing here.
        with tempfile.TemporaryDirectory() as tmp:
            back = Path(tmp) / path.name
            destination.fetch(key, back)
            if digest(back) != sha:
                problems.append(f"{path.name}: what came back from {key} is not what was sent")
                continue

        head = opening(path)
        manifest_line(directory, {
            "file": path.name,
            "key": key,
            "sha256": sha,
            "bytes": path.stat().st_size,
            "records": sum(1 for _ in path.open("rb")),
            "after": head.get("after"),
        })
        if not keep:
            path.unlink()
        taken.append(f"{path.name} -> {key}")

    # The manifest goes up with them, whole, so the archive holds every version and the chain never depends on
    # appending to an object.
    local = Path(directory) / MANIFEST
    if taken and not dry_run and local.exists():
        destination.put(local, f"audit/{MANIFEST}")
    return taken, skipped, problems


def self_test():
    """Writes a small audit log, ships it to a directory, and checks what was taken, kept and recorded."""
    from audit_verify import verify as verify_file

    def write(directory, name, records):
        path = Path(directory) / name
        previous = ZEROS
        with path.open("wb") as f:
            for n, record in enumerate(records):
                line = dict(record, n=n, prev=previous, ts=now(), component="gateway", instance="gw-0")
                raw = json.dumps(line, separators=(",", ":"), sort_keys=True).encode()
                f.write(raw + b"\n")
                previous = hashlib.sha256(raw).hexdigest()
        return path

    failures = []
    with tempfile.TemporaryDirectory() as work, tempfile.TemporaryDirectory() as store:
        old = "gateway.gw-0.2026-09-20.0000.jsonl"
        current = "gateway.gw-0.2026-09-21.0000.jsonl"
        opened = {"kind": "open", "format": "venue-audit/2", "file": old, "after": None}
        write(work, old, [opened, {"kind": "close", "reason": "rotate", "records": 1}])
        write(work, current, [dict(opened, file=current), {"kind": "close", "reason": "rotate", "records": 1}])
        # Closed, ours, but from today: must be left alone.
        destination = Destination(store)
        taken, _, problems = ship(work, destination, day="2026-09-21")

        if problems:
            failures.append(f"problems shipping a good file: {problems}")
        if len(taken) != 1 or old not in taken[0]:
            failures.append(f"expected only {old} to be taken, got {taken}")
        if (Path(work) / old).exists():
            failures.append("a shipped file was left on disk")
        if not (Path(work) / current).exists():
            failures.append("today's file was taken; the next segment could collide with it")
        if not (Path(store) / "audit" / "2026-09-20" / old).exists():
            failures.append("the object is not where the key says")

        manifest = Path(work) / MANIFEST
        if not manifest.exists():
            failures.append("no manifest was written")
        else:
            ok, message = verify_file(manifest)
            if not ok:
                failures.append(f"the manifest is not a valid audit log: {message}")
            line = json.loads(manifest.read_bytes().splitlines()[1])
            if line.get("file") != old or len(line.get("sha256", "")) != 64:
                failures.append(f"the manifest line does not name the file and its digest: {line}")

        # A closed file whose chain is broken is reported and left where it is. (A file with no close record is
        # simply not ready, which is a different thing and not a problem.)
        name = "gateway.gw-0.2026-09-19.0000.jsonl"
        broken = write(work, name, [dict(opened, file=name), {"kind": "close", "reason": "rotate", "records": 1}])
        lines = broken.read_bytes().split(b"\n")
        lines[0] = lines[0].replace(b'"kind":"open"', b'"kind":"OPEN"')
        broken.write_bytes(b"\n".join(lines))
        _, _, problems = ship(work, destination, day="2026-09-21")
        if not problems:
            failures.append("a closed file with a broken chain was shipped")
        if not broken.exists():
            failures.append("a closed file with a broken chain was removed")

        unclosed = write(work, "gateway.gw-0.2026-09-18.0000.jsonl", [dict(opened, file="x")])
        taken, _, problems = ship(work, destination, day="2026-09-21")
        if any("2026-09-18" in line for line in taken) or not unclosed.exists():
            failures.append("a file with no close record was taken; it may still be being written")

    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


def main():
    parser = argparse.ArgumentParser(add_help=True, description=__doc__)
    parser.add_argument("--dir")
    parser.add_argument("--to")
    parser.add_argument("--retain-until", help="ISO 8601; the bucket's Object Lock is what enforces it")
    parser.add_argument("--keep", action="store_true", help="upload but leave the files where they are")
    parser.add_argument("--dry-run", action="store_true", help="say what would be taken and take nothing")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        return self_test()
    if not args.dir or not args.to:
        parser.error("--dir and --to are both needed")

    taken, skipped, problems = ship(args.dir, Destination(args.to, args.retain_until), args.keep, args.dry_run)
    for line in taken:
        print(f"took {line}")
    for line in skipped:
        print(f"would take {line}")
    for line in problems:
        print(f"left alone: {line}", file=sys.stderr)
    if not taken and not skipped and not problems:
        print("nothing ready: no closed file from a day that is over")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
