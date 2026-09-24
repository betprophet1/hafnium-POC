#!/usr/bin/env python3
"""Publish a snapshot (`venue-node snapshot`/`embed-snapshot`) to object storage, named so a test or preview
environment can fetch it back by tag (§3.9: starting from whatever snapshot it likes, not just genesis or
whatever the live venue happens to be running).

  snapshot_publish.py FILE --to s3://BUCKET --environment NAME --tag TAG
  snapshot_publish.py FILE --to /some/directory --environment NAME --tag TAG    on disk, for a demo or a test
  snapshot_publish.py --self-test                                              check the tool against files it writes

Nothing is left unverified: the object (and a small `.sha256` beside it) is read back and compared before this
reports success, the same discipline `audit_ship.py` uses before it ever unlinks a shipped file — the only
difference here is that a snapshot is never deleted locally afterward; it is one artifact meant to be fetched
by name, not a rotating log to clear off a filling disk.
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


class Destination:
    """Somewhere to put an object and read it back. A directory or a bucket; the difference is two commands."""

    def __init__(self, where, environment):
        self.where = where.rstrip("/")
        self.environment = environment
        self.s3 = where.startswith("s3://")

    def key(self, tag, suffix="snapshot"):
        if "/" in self.environment or "/" in tag:
            raise ValueError("environment and tag each become part of a key: no slashes")
        return f"snapshots/{self.environment}/{tag}.{suffix}"

    def put(self, path, key):
        if not self.s3:
            target = Path(self.where) / key
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, target)
            return
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


def publish(path, destination, tag):
    """Uploads `path` and a `.sha256` sidecar naming its digest, then reads both back and compares before
    returning. Raises on any mismatch or transport failure — nothing is reported published unless it verified
    in place, the same principle `interfaces/audit/README.md` already rests on."""
    path = Path(path)
    sha = digest(path)
    snapshot_key = destination.key(tag)
    sha_key = destination.key(tag, "sha256")

    destination.put(path, snapshot_key)
    with tempfile.NamedTemporaryFile() as sidecar:
        sidecar.write(sha.encode())
        sidecar.flush()
        destination.put(Path(sidecar.name), sha_key)

    with tempfile.TemporaryDirectory() as tmp:
        back = Path(tmp) / path.name
        destination.fetch(snapshot_key, back)
        if digest(back) != sha:
            raise ValueError(f"what came back from {snapshot_key} is not what was sent")
        sha_back = Path(tmp) / "sha256"
        destination.fetch(sha_key, sha_back)
        if sha_back.read_text().strip() != sha:
            raise ValueError(f"what came back from {sha_key} does not match the snapshot's own digest")
    return snapshot_key, sha


def self_test():
    failures = []
    with tempfile.TemporaryDirectory() as work, tempfile.TemporaryDirectory() as store:
        snapshot = Path(work) / "some.snapshot"
        snapshot.write_bytes(b"a snapshot's bytes, for the self-test's purposes only")
        destination = Destination(store, "selftest")

        key, sha = publish(snapshot, destination, "smoke")
        if key != "snapshots/selftest/smoke.snapshot":
            failures.append(f"unexpected key: {key}")
        if not (Path(store) / key).exists():
            failures.append("the object is not where the key says")
        if not (Path(store) / "snapshots" / "selftest" / "smoke.sha256").exists():
            failures.append("no .sha256 sidecar was written")
        if (Path(store) / "snapshots" / "selftest" / "smoke.sha256").read_text().strip() != sha:
            failures.append("the sidecar does not name the digest that was actually published")

        try:
            Destination(store, "sel/ftest").key("smoke")
            failures.append("a slash in --environment was accepted")
        except ValueError:
            pass
        try:
            destination.key("bad/tag")
            failures.append("a slash in --tag was accepted")
        except ValueError:
            pass

        # A mismatch after upload (corruption, or a destination that rewrites what it is given) must be caught,
        # not reported as a success.
        real_fetch = Destination.fetch

        def corrupting_fetch(self, key, into):
            real_fetch(self, key, into)
            if key.endswith(".snapshot"):
                Path(into).write_bytes(b"not what was uploaded")

        Destination.fetch = corrupting_fetch
        try:
            publish(snapshot, destination, "corrupt")
            failures.append("a snapshot that came back different from what was sent was reported published")
        except ValueError:
            pass
        finally:
            Destination.fetch = real_fetch

    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


def main():
    parser = argparse.ArgumentParser(add_help=True, description=__doc__,
                                      formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("file", nargs="?")
    parser.add_argument("--to")
    parser.add_argument("--environment", help="which venue this is for: prod, pr-4. Names part of the key.")
    parser.add_argument("--tag", help="the name a test/preview environment will ask for later: main, pre-load, ...")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        return self_test()
    if not args.file or not args.to or not args.environment or not args.tag:
        parser.error("FILE, --to, --environment and --tag are all needed")

    try:
        key, sha = publish(args.file, Destination(args.to, args.environment), args.tag)
    except (ValueError, subprocess.CalledProcessError, OSError) as e:
        print(f"not published: {e}", file=sys.stderr)
        return 1
    print(f"published {key} ({sha[:12]}…)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
