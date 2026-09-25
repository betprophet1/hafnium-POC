#!/usr/bin/env python3
"""Publish a snapshot (`venue-node snapshot`/`embed-snapshot`) to object storage, named so a test or preview
environment can fetch it back by tag (§3.9: starting from whatever snapshot it likes, not just genesis or
whatever the live venue happens to be running).

  snapshot_publish.py FILE --to s3://BUCKET --environment NAME --tag TAG
  snapshot_publish.py --pair DIR --to s3://BUCKET --environment NAME [--tag TAG]   both halves of a pair
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
import time
from pathlib import Path


def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


class Destination:
    """Somewhere to put an object and read it back. A directory or a bucket; the difference is two commands.

    `prefix` is what kind of thing is being stored — `snapshots` here, `journals` for `journal_ship.py`. Keeping
    them apart in the key space is the same instinct as the audit shipper's per-environment prefixes: a lifecycle
    rule written for one must not be able to reach the other (§7.4 learned that the hard way).
    """

    def __init__(self, where, environment, prefix="snapshots"):
        self.where = where.rstrip("/")
        self.environment = environment
        self.prefix = prefix
        self.s3 = where.startswith("s3://")

    def key(self, tag, suffix="snapshot"):
        if "/" in self.environment or "/" in tag:
            raise ValueError("environment and tag each become part of a key: no slashes")
        return f"{self.prefix}/{self.environment}/{tag}.{suffix}"

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


def publish(path, destination, tag, suffix="snapshot"):
    """Uploads `path` and a `.sha256` sidecar naming its digest, then reads both back and compares before
    returning. Raises on any mismatch or transport failure — nothing is reported published unless it verified
    in place, the same principle `interfaces/audit/README.md` already rests on.

    `journal_ship.py` uses this too, with a different `suffix`: one verified upload, not two implementations of
    the same care."""
    path = Path(path)
    sha = digest(path)
    snapshot_key = destination.key(tag, suffix)
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


def publish_pair(directory, destination, tag=None):
    """Publishes both halves of a snapshot pair (`venue-view snapshot`) under one tag, each verified the same
    way a lone snapshot is. A pair is one artifact in two files: the engine's half founds a node, the reader's
    half founds everything downstream, and a half on its own founds nothing — so they are published together
    and the tag names the pair, not the file.

    The tag defaults to a UTC timestamp, so a scheduled job needs to invent nothing."""
    directory = Path(directory)
    tag = tag or time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    # The half is part of the *tag*, not just the suffix, because `publish` derives the `.sha256` sidecar's key
    # from the tag alone. Two halves sharing one tag would share one sidecar, and the second upload would
    # silently overwrite the first's digest — leaving a pair whose engine half is unverifiable and a checksum
    # that describes the wrong file. `journal_ship.py` names its two halves the same way, for the same reason.
    halves = (("engine", "engine.snapshot", f"{tag}.engine", "snapshot"), ("view", "view.json", f"{tag}.view", "json"))

    # Both halves are checked before either is uploaded. Publishing one and then discovering the other is
    # missing would leave exactly the thing this function exists to prevent: a tag that looks like a pair and
    # founds nothing.
    for _, filename, _, _ in halves:
        if not (directory / filename).exists():
            raise ValueError(f"{directory / filename} is missing: {directory} is not a pair venue-view wrote")

    published = {
        half: publish(directory / filename, destination, half_tag, suffix) for half, filename, half_tag, suffix in halves
    }
    return tag, published


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

        # A pair goes up as one artifact under one tag, and a half on its own founds nothing — so a directory
        # missing either half is refused rather than half-published.
        pair = Path(work) / "pair"
        pair.mkdir()
        (pair / "engine.snapshot").write_bytes(b"the engine's half, for the self-test")
        try:
            publish_pair(pair, destination, "half")
            failures.append("a pair missing its view half was published")
        except ValueError:
            pass
        if (Path(store) / "snapshots" / "selftest" / "half.snapshot").exists():
            failures.append("a pair missing its view half uploaded the engine half before being refused")

        (pair / "view.json").write_text('{"sequence": 62}')
        tag, published = publish_pair(pair, destination, "whole")
        if tag != "whole":
            failures.append(f"an explicit tag was not used: {tag}")
        if set(published) != {"engine", "view"}:
            failures.append(f"a pair did not publish both halves: {sorted(published)}")
        # Both halves AND both sidecars. Checking only that "a .sha256 exists" is what let the first version of
        # this ship a pair whose two halves shared one sidecar, so the engine half's digest was overwritten by
        # the view half's and the engine half could never be verified again.
        for name in ("whole.engine.snapshot", "whole.engine.sha256", "whole.view.json", "whole.view.sha256"):
            if not (Path(store) / "snapshots" / "selftest" / name).exists():
                failures.append(f"a published pair is missing {name}")
        digests = {
            name: (Path(store) / "snapshots" / "selftest" / name).read_text().strip()
            for name in ("whole.engine.sha256", "whole.view.sha256")
            if (Path(store) / "snapshots" / "selftest" / name).exists()
        }
        if len(set(digests.values())) != len(digests):
            failures.append("both halves of a pair were published under the same digest sidecar")

        # And the tag invents itself, so a scheduled job needs to know nothing.
        auto, _ = publish_pair(pair, destination)
        if not (len(auto) == 16 and auto[8] == "T" and auto.endswith("Z")):
            failures.append(f"an automatic pair tag is not a UTC stamp: {auto}")

    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


def main():
    parser = argparse.ArgumentParser(add_help=True, description=__doc__,
                                      formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("file", nargs="?")
    parser.add_argument("--pair", help="a directory venue-view wrote: publishes engine.snapshot and view.json")
    parser.add_argument("--to")
    parser.add_argument("--environment", help="which venue this is for: prod, pr-4. Names part of the key.")
    parser.add_argument("--tag", help="the name a test/preview environment will ask for later: main, pre-load, ...")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        return self_test()
    if args.pair:
        if not args.to or not args.environment:
            parser.error("--pair needs --to and --environment")
        try:
            tag, published = publish_pair(args.pair, Destination(args.to, args.environment))
        except (ValueError, subprocess.CalledProcessError, OSError) as e:
            print(f"not published: {e}", file=sys.stderr)
            return 1
        for half, (key, sha) in published.items():
            print(f"published {key} ({sha[:12]}\u2026)")
        print(f"the pair is tagged {tag}")
        return 0
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
