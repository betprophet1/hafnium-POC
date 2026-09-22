#!/usr/bin/env python3
"""Take closed audit files to object storage and off the disk they were written on (interfaces/audit/README.md).

  audit_ship.py --dir DIR --to s3://BUCKET --environment NAME [--keep-records [--retain-years N]]
  audit_ship.py --dir DIR --to /some/directory --environment NAME    on disk, for a demo or a test
  audit_ship.py --self-test                                          check the tool against files it writes

A gateway that cannot write its audit log does not run, and its volume grows until it is full, so this is what
keeps the venue up rather than housekeeping. Nothing is removed until it is somewhere else and has been read back
and compared; `--keep` skips the removal entirely.
"""
import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tempfile
import tracemalloc
from datetime import datetime, timezone
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from audit_verify import FORMAT, ZEROS, verify  # noqa: E402  (a sibling tool, not a package)

MANIFEST = "audit-archive.jsonl"
# What S3 accepts in a single PutObject. Only the locked path is bound by it; everything else is uploaded in parts.
SINGLE_PUT = 5 * 1024 ** 3


class TooBig(Exception):
    """A file that cannot be written the way its retention requires."""


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


def last_line(path, window=1 << 16):
    """A file's last non-empty line, read from the end.

    Audit files reach gigabytes and the last line is all that is wanted, so the file is never read whole: seek to
    the end, take a block, and widen only if no newline is in it — which bounds the read by the length of one
    record rather than the size of the file.
    """
    with path.open("rb") as f:
        f.seek(0, os.SEEK_END)
        size = f.tell()
        while True:
            block = min(window, size)
            f.seek(size - block)
            data = f.read(block).rstrip(b"\n")
            if not data:
                return b""
            cut = data.rfind(b"\n")
            if cut != -1:
                return data[cut + 1:]
            if block >= size:
                return data
            window *= 2


def closed(path):
    """Whether a file's last line is its close record. The format promises nothing is ever appended after one."""
    try:
        return json.loads(last_line(path)).get("kind") == "close"
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

    def __init__(self, where, environment, keep_records=False, retain_years=5):
        self.where = where.rstrip("/")
        self.environment = environment
        self.keep_records = keep_records
        self.retain_years = retain_years if keep_records else None
        self.s3 = where.startswith("s3://")

    def retain_until(self):
        """When an object written now stops being immutable, or None to write it deletable.

        Counted from the upload rather than fixed once, so a job that runs every day does not hand every object
        the same expiry and slowly shorten the retention of the newest. A retention only ever lengthens (§12), so
        a date that drifts forward is correctable and one that drifts back could not be applied at all.
        """
        if not self.retain_years:
            return None
        at = datetime.now(timezone.utc)
        return at.replace(year=at.year + self.retain_years).strftime("%Y-%m-%dT%H:%M:%SZ")

    def key(self, day, name):
        """Where an object goes: whether its records are kept, then which environment, then the day.

        Kept and transient are separate prefixes rather than a label because a lifecycle rule can only select by
        prefix, and there is no "everything except" — so an expiry that must not touch production has to be
        written against a prefix production is not under. Putting the class first is what lets one rule expire a
        preview's audit log after ninety days and another leave the venue's alone for ever.

        The environment next, because it is how somebody looking for a particular venue's day finds it, and
        because it keeps one environment's records answerable separately from another's.
        """
        return f"audit/{'kept' if self.keep_records else 'transient'}/{self.environment}/{day}/{name}"

    def put(self, path, key):
        if not self.s3:
            target = Path(self.where) / key
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, target)
            return
        until = self.retain_until()
        if until:
            # Per object, because one bucket holds environments whose records matter and environments whose
            # records do not. A bucket-wide default would lock a preview's audit log for as long as a production
            # one's, and no one — the account's root user included — could undo it.
            #
            # Whole, in one request, because that is the only way the object is born locked: a retention applied
            # after an upload leaves a window in which the object can still be deleted, which is the one thing a
            # locked object exists to prevent. The cost is S3's limit on a single PUT, and a file past it cannot be
            # shipped this way at all — say so rather than let the CLI fail with its own wording.
            if path.stat().st_size > SINGLE_PUT:
                raise TooBig(f"{path.name} is {path.stat().st_size} bytes; a locked object is written in one "
                             f"request and S3 refuses one over {SINGLE_PUT}. It needs segmenting before it can "
                             f"be kept.")
            subprocess.run(["aws", "s3api", "put-object", "--bucket", self.bucket(), "--key", self.prefixed(key),
                            "--body", str(path), "--checksum-algorithm", "SHA256",
                            "--object-lock-mode", "COMPLIANCE", "--object-lock-retain-until-date", until],
                           check=True, capture_output=True)
            return
        # Nothing to lock, so the upload may be split into parts — which it has to be, because an audit file that
        # nothing has been shipping grows past what one request may carry. `s3 cp` does that by itself; `put-object`
        # cannot. What was written is checked by reading it back and comparing, as it is for every other object.
        subprocess.run(["aws", "s3", "cp", str(path), f"s3://{self.bucket()}/{self.prefixed(key)}",
                        "--only-show-errors"], check=True, capture_output=True)

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

        # One file that cannot be shipped is that file's problem. It used to be every file's: an upload that failed
        # raised through the loop and the run ended there, so a single unshippable file left the whole backlog on
        # the disk — with the volume filling, which is the condition this exists to relieve.
        try:
            destination.put(path, key)
            # Read back before anything is removed: an upload that was refused, truncated or silently rewritten
            # must not be the reason a file stops existing here.
            with tempfile.TemporaryDirectory() as tmp:
                back = Path(tmp) / path.name
                destination.fetch(key, back)
                if digest(back) != sha:
                    problems.append(f"{path.name}: what came back from {key} is not what was sent")
                    continue
        except TooBig as e:
            problems.append(str(e))
            continue
        except subprocess.CalledProcessError as e:
            detail = (e.stderr or b"").decode("utf-8", "replace").strip().splitlines()
            problems.append(f"{path.name}: {' '.join(detail[-2:]) if detail else e}")
            continue
        except OSError as e:
            problems.append(f"{path.name}: {e}")
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
        destination.put(local, destination.key("", MANIFEST).replace("//", "/"))
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
        destination = Destination(store, "selftest")
        taken, _, problems = ship(work, destination, day="2026-09-21")

        if problems:
            failures.append(f"problems shipping a good file: {problems}")
        if len(taken) != 1 or old not in taken[0]:
            failures.append(f"expected only {old} to be taken, got {taken}")
        if (Path(work) / old).exists():
            failures.append("a shipped file was left on disk")
        if not (Path(work) / current).exists():
            failures.append("today's file was taken; the next segment could collide with it")
        if not (Path(store) / "audit" / "transient" / "selftest" / "2026-09-20" / old).exists():
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

        # `closed()` asks one question of a file that may be gigabytes, so it reads from the end. Reading the whole
        # file to see its last line is what OOM-killed this on the live gateway's 5.8 GB backlog.
        probe = Path(work) / "tail-probe"
        for content, want, what in ((b'{"kind":"close"}\n', b'{"kind":"close"}', "a trailing newline"),
                                    (b'{"kind":"close"}', b'{"kind":"close"}', "no trailing newline"),
                                    (b'a\n{"kind":"close"}\n\n\n', b'{"kind":"close"}', "several trailing newlines"),
                                    (b"", b"", "an empty file")):
            probe.write_bytes(content)
            if last_line(probe) != want:
                failures.append(f"last_line with {what}: got {last_line(probe)!r}, wanted {want!r}")

        # A record longer than the first window: the read has to widen, and widening is the part with the bug in it.
        long_record = b'{"kind":"close","pad":"' + b"y" * (1 << 18) + b'"}'
        probe.write_bytes(b"first\n" + long_record + b"\n")
        if last_line(probe, window=1 << 12) != long_record:
            failures.append("last_line did not widen its window for a record longer than it")
        if not closed(probe):
            failures.append("closed() missed a close record that was longer than one read")

        # And it must cost the tail, not the file.
        big = Path(work) / "big"
        big.write_bytes(b'{"kind":"pad"}\n' * 400_000 + b'{"kind":"close"}\n')
        size = big.stat().st_size
        tracemalloc.start()
        got = closed(big)
        peak = tracemalloc.get_traced_memory()[1]
        tracemalloc.stop()
        if not got:
            failures.append("closed() did not find the close record of a large file")
        if peak > size // 8:
            failures.append(f"closed() on {size} bytes peaked at {peak}: it is reading the file whole")
        probe.unlink()
        big.unlink()

        # Which request each kind of object goes up with. The commands are recorded rather than run, because the
        # branch is the whole point and a self-test may not write 5 GiB to find out.
        sample = write(work, "gateway.gw-0.2026-09-19.0000.jsonl",
                       [dict(opened, file="gateway.gw-0.2026-09-19.0000.jsonl"),
                        {"kind": "close", "reason": "rotate", "records": 1}])
        calls, real_run, real_limit = [], subprocess.run, globals()["SINGLE_PUT"]

        def record(args, **kwargs):
            calls.append(list(args))
            return subprocess.CompletedProcess(args, 0, b"", b"")

        try:
            subprocess.run = record
            Destination("s3://b", "e").put(sample, "k")
            if not calls or calls[-1][:3] != ["aws", "s3", "cp"]:
                failures.append(f"a transient object is not uploaded in parts: {calls[-1][:4] if calls else None}")

            Destination("s3://b", "e", keep_records=True).put(sample, "k")
            if calls[-1][:3] != ["aws", "s3api", "put-object"] or "--object-lock-mode" not in calls[-1]:
                failures.append(f"a kept object is not written whole and locked: {calls[-1][:4]}")

            # Past what one request may carry, a locked object must be refused rather than half-written.
            globals()["SINGLE_PUT"] = 1
            try:
                Destination("s3://b", "e", keep_records=True).put(sample, "k")
                failures.append("a locked object past the single-request limit was not refused")
            except TooBig:
                pass
            # The same file unlocked is not affected by that limit at all.
            Destination("s3://b", "e").put(sample, "k")
            if calls[-1][:3] != ["aws", "s3", "cp"]:
                failures.append("the single-request limit was applied to an object that is uploaded in parts")
        finally:
            subprocess.run = real_run
            globals()["SINGLE_PUT"] = real_limit
        sample.unlink()

    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


def main():
    parser = argparse.ArgumentParser(add_help=True, description=__doc__)
    parser.add_argument("--dir")
    parser.add_argument("--to")
    parser.add_argument("--environment", help="which venue these are from: prod, pr-4. Names the prefix.")
    parser.add_argument("--keep-records", action="store_true",
                        help="these are records somebody may have to produce: written under audit/kept/, where no "
                             "lifecycle rule expires them, and locked immutable. Without it they go under "
                             "audit/transient/ and are expired, which is what a demo or a preview wants.")
    parser.add_argument("--retain-years", type=int, default=5,
                        help="how long --keep-records locks for (default 5). A retention only ever lengthens.")
    parser.add_argument("--keep", action="store_true", help="upload but leave the files where they are")
    parser.add_argument("--dry-run", action="store_true", help="say what would be taken and take nothing")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        return self_test()
    if not args.dir or not args.to or not args.environment:
        parser.error("--dir, --to and --environment are all needed")
    if "/" in args.environment:
        parser.error("--environment names one venue and becomes part of a key: no slashes")

    destination = Destination(args.to, args.environment, args.keep_records, args.retain_years)
    taken, skipped, problems = ship(args.dir, destination, args.keep, args.dry_run)
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
