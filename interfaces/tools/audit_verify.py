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
from pathlib import Path

ZEROS = "0" * 64
FORMAT = "venue-audit/1"


def verify(path, require_closed=False):
    """(ok, message) for one file. The message names the first broken line, counted from 1, or summarises the file."""
    data = Path(path).read_bytes()
    if not data:
        return False, f"{path}: empty: a file always starts with its open record"
    lines = data.split(b"\n")
    if lines[-1] == b"":
        lines.pop()
    else:
        return False, f"{path}: line {len(lines)}: cut off part-way (no newline)"
    prev = ZEROS
    closed = False
    header = None
    for number, raw in enumerate(lines):
        where = f"{path}: line {number + 1}"
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
            if record["kind"] != "open" or record.get("format") != FORMAT:
                return False, f"{where}: the first record must be an open record of format {FORMAT}"
            if record.get("file") != Path(path).name:
                return False, f"{where}: the open record names {record.get('file')!r}, not this file"
            header = (record["component"], record["instance"])
        elif (record["component"], record["instance"]) != header:
            return False, f"{where}: component or instance differs from the open record"
        elif record["kind"] == "open":
            return False, f"{where}: a second open record"
        if record["kind"] == "close":
            if record.get("records") != number:
                return False, f"{where}: the close record counts {record.get('records')!r} lines, not {number}"
            closed = True
        prev = hashlib.sha256(raw).hexdigest()
    if require_closed and not closed:
        return False, f"{path}: line {len(lines)}: not closed"
    return True, f"{path}: ok, {len(lines)} records, {'closed' if closed else 'open'}"


def self_test():
    """Writes a chained file the way the writers do, then damages copies of it in each way the verifier must catch."""

    def line(record, prev):
        record["prev"] = prev
        return json.dumps(record, separators=(",", ":")).encode()

    with tempfile.TemporaryDirectory() as tmp:
        name = "gateway.test.2026-09-17.0000.jsonl"
        base = {"ts": "2026-09-17T00:00:00.000000Z", "component": "gateway", "instance": "test"}
        records = [dict(base, n=0, kind="open", format=FORMAT, file=name, pid=1),
                   dict(base, n=1, kind="ws-frame", dir="in", msg='{"op":"login","token":"***"}'),
                   dict(base, n=2, kind="close", reason="shutdown", records=2)]
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
        check([whole[0], whole[1].replace(b"login", b"logout"), whole[2]], False, False, "an altered record")
        check([whole[0], whole[2]], False, False, "a removed record")
        check([whole[0], whole[1], raw[2]], False, False, "a cut-off last line")
        check([whole[0], whole[1], whole[2], whole[1]], False, False, "a record after the close")
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
