#!/usr/bin/env python3
"""Verify edge audit log files (interfaces/audit/README.md): each file's hash chain, record numbers and close record.

  audit_verify.py FILE...            exit 1 if any file is broken; prints the first broken line of each
  audit_verify.py --closed FILE...   and every file must end with its close record
  audit_verify.py --self-test        check the verifier itself against files it writes and then damages
"""
import hashlib
import json
import sys
import tempfile
import tracemalloc
from pathlib import Path

ZEROS = "0" * 64
FORMAT = "venue-audit/2"
# Written before frames derived from the journal were recorded by reference. Those files verify as they always did.
FORMATS = ("venue-audit/1", FORMAT)


def verify(path, require_closed=False):
    """(ok, message) for one file. The message names the first broken line, counted from 1, or summarises the file.

    A line at a time, holding only that line. These files reach gigabytes — the live gateway's largest was 5.8 GB
    before anything shipped them — and reading one whole and splitting it holds the file twice over. That OOM-killed
    the shipper the first time it met a real backlog, which is the one moment it has to work: the files are only
    that size because nothing had been taking them away.
    """
    prev = ZEROS
    closed = False
    header = None
    count = 0
    with open(path, "rb") as handle:
        for number, line in enumerate(handle):
            count = number + 1
            where = f"{path}: line {count}"
            # Only a file's last line can lack its newline, so this is the cut-off-part-way check.
            if not line.endswith(b"\n"):
                return False, f"{where}: cut off part-way (no newline)"
            raw = line[:-1]
            if closed:
                return False, f"{where}: a record after the close record"
            try:
                record = json.loads(raw.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError) as e:
                return False, f"{where}: not a JSON object: {e}"
            if not isinstance(record, dict):
                return False, f"{where}: not a JSON object"
            if record.get("prev") != prev:
                return False, f"{where}: prev {record.get('prev')!r} is not the hash of the line before ({prev})"
            if record.get("n") != number:
                return False, f"{where}: n is {record.get('n')!r}, expected {number}"
            for field in ("ts", "component", "instance", "kind"):
                if not isinstance(record.get(field), str):
                    return False, f"{where}: {field} missing"
            if number == 0:
                if record["kind"] != "open" or record.get("format") not in FORMATS:
                    return False, f"{where}: the first record must be an open record of a known format"
                if record.get("file") != Path(path).name:
                    return False, f"{where}: the open record names {record.get('file')!r}, not this file"
                header = (record["component"], record["instance"])
            elif (record["component"], record["instance"]) != header:
                return False, f"{where}: component or instance differs from the open record"
            elif record["kind"] == "open":
                return False, f"{where}: a second open record"
            broken = reference_broken(record)
            if broken:
                return False, f"{where}: {broken}"
            if record["kind"] == "close":
                if record.get("records") != number:
                    return False, f"{where}: the close record counts {record.get('records')!r} lines, not {number}"
                closed = True
            prev = hashlib.sha256(raw).hexdigest()
    if count == 0:
        return False, f"{path}: empty: a file always starts with its open record"
    if require_closed and not closed:
        return False, f"{path}: line {count}: not closed"
    return True, f"{path}: ok, {count} records, {'closed' if closed else 'open'}"


def reference_broken(record):
    """Why a record that references the journal is malformed, or None. A frame the gateway rendered from a sequenced
    input is recorded as what it was, which input it came from, and the hash of the bytes sent, never as the bytes
    themselves (interfaces/audit/README.md, frames derived from the journal)."""
    if record["kind"] != "ws-frame" or "type" not in record:
        return None
    if "msg" in record:
        return "a frame recorded by reference also carries msg"
    if not isinstance(record.get("sequence"), str) or not record["sequence"].isdigit():
        return "a frame recorded by reference needs the sequence it came from"
    if not isinstance(record.get("length"), int) or record["length"] < 0:
        return "a frame recorded by reference needs the length of what was sent"
    digest = record.get("sha256")
    if not isinstance(digest, str) or len(digest) != 64 or not all(c in "0123456789abcdef" for c in digest):
        return "a frame recorded by reference needs the SHA-256 of what was sent"
    return None


def self_test():
    """Writes a chained file the way the writers do, then damages copies of it in each way the verifier must catch."""

    def line(record, prev):
        record["prev"] = prev
        return json.dumps(record, separators=(",", ":")).encode()

    with tempfile.TemporaryDirectory() as tmp:
        name = "gateway.test.2026-09-17.0000.jsonl"
        base = {"ts": "2026-09-17T00:00:00.000000Z", "component": "gateway", "instance": "test"}
        records = [dict(base, n=0, kind="open", format=FORMAT, file=name, pid=1, renderer=1, build="0.1.0"),
                   dict(base, n=1, kind="ws-frame", dir="in", msg='{"op":"login","token":"***"}'),
                   dict(base, n=2, kind="ws-frame", dir="out", type="execution", sequence="29", length=255,
                        sha256="e" * 64),
                   dict(base, n=3, kind="close", reason="shutdown", records=3)]
        raw, prev = [], ZEROS
        for r in records:
            raw.append(line(r, prev))
            prev = hashlib.sha256(raw[-1]).hexdigest()
        path = Path(tmp) / name

        def check(lines, closed, expect_ok, what):
            path.write_bytes(b"".join(lines))
            ok, message = verify(path, closed)
            if ok != expect_ok:
                sys.exit(f"self-test failed: {what}: {message}")

        whole = [r + b"\n" for r in raw]
        check(whole, True, True, "an intact closed file")
        check(whole[:2], False, True, "an intact open file")
        check(whole[:2], True, False, "an open file where a closed one is required")
        check([whole[0], whole[1].replace(b"login", b"logout"), whole[2], whole[3]], False, False, "an altered record")
        check([whole[0], whole[2], whole[3]], False, False, "a removed record")
        check([whole[0], whole[1], whole[2], raw[3]], False, False, "a cut-off last line")
        check(whole + [whole[1]], False, False, "a record after the close")

        # A reference into the journal says what it points at, and never carries the frame as well.
        for broken, what in (({"sha256": "nonsense"}, "a reference without a usable hash"),
                             ({"sequence": None}, "a reference without a sequence"),
                             ({"msg": "{}"}, "a reference that also holds the frame")):
            lines, prev = [], ZEROS
            for r in (records[0], records[1], dict(records[2], **broken), records[3]):
                lines.append(line(dict(r), prev) + b"\n")
                prev = hashlib.sha256(lines[-1][:-1]).hexdigest()
            check(lines, False, False, what)

        # Verifying a big file must cost a line, not a file. Reading it whole and splitting it held the file twice
        # over, and the live gateway's largest was 5.8 GB — the shipper was OOM-killed by the first real backlog it
        # met. Measured rather than left to inspection, so a rewrite that slurps fails here and not in production.
        name = "gateway.big.2026-09-17.0000.jsonl"
        path = Path(tmp) / name
        prev, filler = ZEROS, "x" * 256
        with path.open("wb") as f:
            for n in range(40_000):
                record = dict(base, n=n, prev=prev, pad=filler,
                              **({"kind": "open", "format": FORMAT, "file": name} if n == 0
                                 else {"kind": "ws-frame", "dir": "in", "msg": "{}"}))
                line = json.dumps(record, separators=(",", ":")).encode()
                f.write(line + b"\n")
                prev = hashlib.sha256(line).hexdigest()
        size = path.stat().st_size
        tracemalloc.start()
        ok, message = verify(path)
        peak = tracemalloc.get_traced_memory()[1]
        tracemalloc.stop()
        if not ok:
            sys.exit(f"self-test failed: a large intact file: {message}")
        if peak > size // 8:
            sys.exit(f"self-test failed: verifying {size} bytes peaked at {peak}: it is reading the file whole")
    print("audit_verify self-test: ok")


def main(argv):
    if argv == ["--self-test"]:
        self_test()
        return 0
    require_closed = "--closed" in argv
    files = [a for a in argv if a != "--closed"]
    if not files:
        print(__doc__, file=sys.stderr)
        return 64
    failed = False
    for f in files:
        ok, message = verify(f, require_closed)
        print(message if ok else f"FAIL {message}")
        failed |= not ok
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
