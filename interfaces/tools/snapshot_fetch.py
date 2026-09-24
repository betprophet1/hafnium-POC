#!/usr/bin/env python3
"""Fetch a snapshot `snapshot_publish.py` published, by the tag it was given, and verify it against the `.sha256`
sidecar published beside it (§3.9: a test or preview environment starting from whatever snapshot it likes).

  snapshot_fetch.py --from s3://BUCKET --environment NAME --tag TAG --to OUT
  snapshot_fetch.py --from /some/directory --environment NAME --tag TAG --to OUT
  snapshot_fetch.py --self-test                                             check the tool against files it writes

Typical use, once fetched: `venue-node seed OUT ./venue-data && venue-node serve --data ./venue-data`.
"""
import argparse
import hashlib
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


class Source:
    """Somewhere to read an object from. A directory or a bucket; the difference is one command."""

    def __init__(self, where, environment):
        self.where = where.rstrip("/")
        self.environment = environment
        self.s3 = where.startswith("s3://")

    def key(self, tag, suffix="snapshot"):
        if "/" in self.environment or "/" in tag:
            raise ValueError("environment and tag each become part of a key: no slashes")
        return f"snapshots/{self.environment}/{tag}.{suffix}"

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


class Mismatch(Exception):
    """What came down does not match the digest it was published with."""


def fetch(source, tag, out):
    """Downloads the snapshot tagged `tag` to `out`, and its `.sha256` sidecar, comparing the two before
    returning — a corrupted or partial transfer is reported, not handed to a caller as if it were whole."""
    out = Path(out)
    with tempfile.TemporaryDirectory() as tmp:
        sha_file = Path(tmp) / "sha256"
        source.fetch(source.key(tag, "sha256"), sha_file)
        expected = sha_file.read_text().strip()
        source.fetch(source.key(tag), out)
        got = digest(out)
        if got != expected:
            out.unlink(missing_ok=True)
            raise Mismatch(f"{tag} came down as {got}, published as {expected}")
    return out


def self_test():
    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from snapshot_publish import Destination, publish

    failures = []
    with tempfile.TemporaryDirectory() as work, tempfile.TemporaryDirectory() as store:
        snapshot = Path(work) / "some.snapshot"
        snapshot.write_bytes(b"a snapshot's bytes, for the self-test's purposes only")
        publish(snapshot, Destination(store, "selftest"), "smoke")

        out = Path(work) / "fetched.snapshot"
        fetch(Source(store, "selftest"), "smoke", out)
        if out.read_bytes() != snapshot.read_bytes():
            failures.append("what was fetched does not match what was published")

        # A sidecar that does not match what actually comes down must be caught, not handed over anyway.
        tampered = Path(store) / "snapshots" / "selftest" / "smoke.snapshot"
        tampered.write_bytes(b"corrupted after publishing")
        try:
            fetch(Source(store, "selftest"), "smoke", out)
            failures.append("a snapshot that does not match its sidecar's digest was returned anyway")
        except Mismatch:
            if out.exists():
                failures.append("a mismatched fetch left a partial file behind")

        try:
            Source(store, "selftest").key("bad/tag")
            failures.append("a slash in --tag was accepted")
        except ValueError:
            pass

    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


def main():
    parser = argparse.ArgumentParser(add_help=True, description=__doc__,
                                      formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--from", dest="source", help="s3://BUCKET or a directory, as published to")
    parser.add_argument("--environment", help="which venue this is for: prod, pr-4. Names part of the key.")
    parser.add_argument("--tag", help="the name it was published under: main, pre-load, ...")
    parser.add_argument("--to", help="where to write the snapshot locally")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        return self_test()
    if not args.source or not args.environment or not args.tag or not args.to:
        parser.error("--from, --environment, --tag and --to are all needed")

    try:
        out = fetch(Source(args.source, args.environment), args.tag, args.to)
    except (Mismatch, subprocess.CalledProcessError, OSError) as e:
        print(f"not fetched: {e}", file=sys.stderr)
        return 1
    print(f"fetched {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
