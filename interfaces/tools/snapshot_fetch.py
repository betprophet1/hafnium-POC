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


def latest_pair_tag(source):
    """The newest pair tag under this environment, or None when there are none yet.

    Tags are UTC stamps (`snapshot_publish.py --pair`), so newest is simply greatest. `None` is a real answer
    and not an error: the first run of a scheduled job has no predecessor, and an uncut journal does not need
    one. A *cut* journal does, and `venue-view` is what refuses in that case rather than this."""
    prefix = f"snapshots/{source.environment}/"
    if not source.s3:
        directory = Path(source.where) / prefix
        names = [p.name for p in directory.glob("*.engine.snapshot")] if directory.is_dir() else []
    else:
        listing = subprocess.run(
            ["aws", "s3api", "list-objects-v2", "--bucket", source.bucket(),
             "--prefix", source.prefixed(prefix), "--query", "Contents[].Key", "--output", "text"],
            check=True, capture_output=True, text=True).stdout
        names = [key.rsplit("/", 1)[-1] for key in listing.split() if key.endswith(".engine.snapshot")]
    tags = sorted(name[: -len(".engine.snapshot")] for name in names)
    return tags[-1] if tags else None


def fetch_pair(source, into, tag=None):
    """Downloads both halves of a pair into `into`, named as `venue-view` writes them, each verified against its
    own sidecar. Returns the tag fetched, or None when the environment has no pair yet."""
    tag = tag or latest_pair_tag(source)
    if tag is None:
        return None
    into = Path(into)
    into.mkdir(parents=True, exist_ok=True)
    fetch(source, f"{tag}.engine", into / "engine.snapshot")
    fetch(source, f"{tag}.view", into / "view.json", suffix="json")
    return tag


class Mismatch(Exception):
    """What came down does not match the digest it was published with."""


def fetch(source, tag, out, suffix="snapshot"):
    """Downloads the snapshot tagged `tag` to `out`, and its `.sha256` sidecar, comparing the two before
    returning — a corrupted or partial transfer is reported, not handed to a caller as if it were whole."""
    out = Path(out)
    with tempfile.TemporaryDirectory() as tmp:
        sha_file = Path(tmp) / "sha256"
        source.fetch(source.key(tag, "sha256"), sha_file)
        expected = sha_file.read_text().strip()
        source.fetch(source.key(tag, suffix), out)
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

        # A pair comes back whole, and the newest one is found without being named — which is what a scheduled
        # job needs, since it knows when it is running but not what the last run was called.
        from snapshot_publish import publish_pair

        made = Path(work) / "pair"
        made.mkdir()
        (made / "engine.snapshot").write_bytes(b"the engine's half")
        (made / "view.json").write_text('{"sequence": 62}')
        publish_pair(made, Destination(store, "pairs"), "20260101T000000Z")
        (made / "view.json").write_text('{"sequence": 99}')
        publish_pair(made, Destination(store, "pairs"), "20260202T000000Z")

        got = Path(work) / "got"
        tag = fetch_pair(Source(store, "pairs"), got)
        if tag != "20260202T000000Z":
            failures.append(f"the newest pair was not the one fetched: {tag}")
        if (got / "view.json").read_text() != '{"sequence": 99}':
            failures.append("the fetched pair's view half is not the newest one's")
        if (got / "engine.snapshot").read_bytes() != b"the engine's half":
            failures.append("the fetched pair's engine half did not come back whole")

        # An environment with no pair yet is a real answer, not an error: the first scheduled run has none.
        if fetch_pair(Source(store, "empty"), Path(work) / "nothing") is not None:
            failures.append("an environment with no pairs claimed to have one")

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
    parser.add_argument("--pair", help="a directory to write both halves of a pair into")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        return self_test()
    if args.pair:
        if not args.source or not args.environment:
            parser.error("--pair needs --from and --environment")
        try:
            tag = fetch_pair(Source(args.source, args.environment), args.pair, args.tag)
        except (Mismatch, subprocess.CalledProcessError, OSError) as e:
            print(f"not fetched: {e}", file=sys.stderr)
            return 1
        # No pair yet is a real answer, not a failure: the first scheduled run has no predecessor, and an uncut
        # journal does not need one. Exit 0 so the job carries on and `venue-view` decides.
        print(f"fetched the pair tagged {tag} into {args.pair}" if tag else "no pair published yet")
        return 0
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
