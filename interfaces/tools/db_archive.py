#!/usr/bin/env python3
"""Archive a PostgreSQL database to object storage, verified, and delete nothing.

  db_archive.py --database identity --to s3://BUCKET --environment prod
  db_archive.py --database identity --to /some/directory --environment prod   on disk, for a demo or a test
  db_archive.py --self-test                                                   check the tool against files it writes

The connection string comes from the environment (`VENUE_DATABASE_URL` by default, `--url-env` to name another),
never from a command line, and the password is never put in this process's or `pg_dump`'s argv — it goes to the
child in `PGPASSWORD`, so it cannot be read out of `ps`.

**This tool never deletes anything, from the database or from object storage.** That is not an oversight, and a
future version should not "fix" it:

  - The four venue databases are deliberately append-only. Every one installs a trigger that raises on DELETE and
    TRUNCATE and REVOKEs those privileges — `core/treasury/migrations/...ledger.sql` ("is append-only: refused, a
    correction is a compensating entry, never an edit"), and the same in identity, oracle and parlays, each with
    tests asserting the refusal. Deleting rows on an age rule would mean tearing that out.
  - They are also not a capacity problem. Measured 2026-09-25, the four together held about 4 MB of application
    data after half a day of continuous trading — roughly 0.34 MB/hour, against the engine journal's 35 MB/hour.
    The journal was the thing filling the disk, and it is dealt with separately (`venue-node export-cut`).

What was actually missing is a copy. The journal ships to S3 and the audit log ships to S3; the databases — the
ledger, the identity records, the resolution history — existed only on one node's local disk with nothing dumping
them anywhere. That is what this fixes.

Deciding what may eventually be *removed* from a store, as opposed to copied out of it, is interface 29
("Stores &rarr; retention archive", `interfaces/catalogue.toml`), which is planned and unbuilt, and which has to
account for the legal-hold register that outranks every retention period. This tool is deliberately only the
copying half, and takes no position on the other.
"""
import argparse
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from urllib.parse import unquote, urlparse

sys.path.insert(0, str(Path(__file__).resolve().parent))
from snapshot_publish import Destination, digest, publish  # noqa: E402  (after sys.path)


def dump(url, into):
    """`pg_dump` in its custom format, then `pg_restore --list` to prove the file is a readable archive and not
    merely bytes that arrived intact. A checksum tells you the object did not rot in transit; it tells you
    nothing about whether the dump was truncated at source, which is the failure a backup actually dies of.

    Returns how many entries the archive's table of contents holds."""
    parsed = urlparse(url)
    if parsed.scheme not in ("postgres", "postgresql"):
        raise ValueError(f"not a postgres URL: {parsed.scheme or 'no scheme'}")
    flags = ["--format=custom", "--file", str(into)]
    if parsed.hostname:
        flags += ["--host", parsed.hostname]
    if parsed.port:
        flags += ["--port", str(parsed.port)]
    if parsed.username:
        flags += ["--username", unquote(parsed.username)]
    database = parsed.path.lstrip("/")
    if not database:
        raise ValueError("the URL names no database")
    flags += ["--dbname", database]

    # The password goes to the child in the environment, never in argv, so it cannot be read out of `ps`.
    environment = dict(os.environ)
    if parsed.password:
        environment["PGPASSWORD"] = unquote(parsed.password)
    subprocess.run(["pg_dump", *flags], check=True, capture_output=True, env=environment)

    listing = subprocess.run(["pg_restore", "--list", str(into)], check=True, capture_output=True, text=True)
    entries = [line for line in listing.stdout.splitlines() if line and not line.startswith(";")]
    if not entries:
        raise ValueError(f"{into} has an empty table of contents: the dump is not a usable archive")
    return len(entries)


def stamp():
    return time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())


def archive(database, url, destination, at=None, dumper=dump):
    """Dumps `database`, proves the archive is readable, uploads it and reads it back. Returns the key and the
    digest. Raises rather than returning on any failure — a backup reported as taken and not actually there is
    worse than one that visibly failed."""
    at = at or stamp()
    with tempfile.TemporaryDirectory() as work:
        into = Path(work) / f"{database}-{at}.dump"
        entries = dumper(url, into)
        if not into.exists() or into.stat().st_size == 0:
            raise ValueError(f"{database}: pg_dump produced nothing")
        size = into.stat().st_size
        key, sha = publish(into, destination, f"{database}-{at}", "dump")
    return key, sha, entries, size


def self_test():
    failures = []

    def fake_dump(contents=b"PGDMP fake archive for the self-test", toc=7):
        def dumper(url, into):
            Path(into).write_bytes(contents)
            return toc
        return dumper

    with tempfile.TemporaryDirectory() as store:
        destination = Destination(store, "selftest", prefix="databases")

        key, sha, entries, _ = archive("identity", "postgres://u:p@h:5432/identity", destination,
                                       at="20260925T000000Z", dumper=fake_dump())
        if key != "databases/selftest/identity-20260925T000000Z.dump":
            failures.append(f"unexpected key: {key}")
        if not (Path(store) / key).exists():
            failures.append("the dump is not where the key says")
        if not (Path(store) / "databases" / "selftest" / "identity-20260925T000000Z.sha256").exists():
            failures.append("no .sha256 sidecar beside the dump")
        if entries != 7:
            failures.append("the table-of-contents count was not reported back")

        # A dump that produced no bytes must not be published as a backup.
        def empty(url, into):
            Path(into).write_bytes(b"")
            return 0

        try:
            archive("identity", "postgres://u:p@h/identity", destination, at="empty", dumper=empty)
            failures.append("an empty dump was published as a backup")
        except ValueError:
            pass
        if (Path(store) / "databases" / "selftest" / "identity-empty.dump").exists():
            failures.append("an empty dump reached the destination before being refused")

        # A dump that fails at source must not be published either.
        def broken(url, into):
            raise subprocess.CalledProcessError(1, "pg_dump")

        try:
            archive("identity", "postgres://u:p@h/identity", destination, at="broken", dumper=broken)
            failures.append("a failed pg_dump was reported as a backup")
        except subprocess.CalledProcessError:
            pass

        # And a destination that rewrites what it is given must fail the archive, not report success.
        real_fetch = Destination.fetch

        def corrupting_fetch(self, key, into):
            real_fetch(self, key, into)
            if key.endswith(".dump"):
                Path(into).write_bytes(b"not what was uploaded")

        Destination.fetch = corrupting_fetch
        try:
            archive("identity", "postgres://u:p@h/identity", destination, at="corrupt", dumper=fake_dump())
            failures.append("a dump that came back different from what was sent was reported archived")
        except ValueError:
            pass
        finally:
            Destination.fetch = real_fetch

    # The URL parsing is what keeps the password out of argv, so it is worth its own check: a URL with no
    # scheme, or naming no database, must be refused before `pg_dump` is ever reached.
    for bad in ("h:5432/identity", "postgres://u:p@h:5432/", "mysql://u:p@h/identity"):
        try:
            dump(bad, "/dev/null")
            failures.append(f"a bad URL was accepted: {bad}")
        except ValueError:
            pass
        except FileNotFoundError:
            failures.append(f"a bad URL reached pg_dump before being refused: {bad}")

    for failure in failures:
        print(f"self-test: {failure}", file=sys.stderr)
    return 1 if failures else 0


def main():
    parser = argparse.ArgumentParser(add_help=True, description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--database", help="which database this is, and what the key is named after")
    parser.add_argument("--to")
    parser.add_argument("--environment", help="which venue this is for: prod, pr-4. Names part of the key.")
    parser.add_argument("--url-env", default="VENUE_DATABASE_URL",
                        help="the environment variable holding the connection URL (default VENUE_DATABASE_URL)")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args()

    if args.self_test:
        return self_test()
    if not args.database or not args.to or not args.environment:
        parser.error("--database, --to and --environment are all needed")
    url = os.environ.get(args.url_env)
    if not url:
        print(f"not archived: {args.url_env} is not set", file=sys.stderr)
        return 1

    try:
        key, sha, entries, _ = archive(args.database, url,
                                       Destination(args.to, args.environment, prefix="databases"))
    except (ValueError, subprocess.CalledProcessError, OSError) as e:
        # A CalledProcessError's stderr can carry the connection string; print only what the tool knows is safe.
        detail = e.__class__.__name__ if isinstance(e, subprocess.CalledProcessError) else e
        print(f"not archived: {args.database}: {detail}", file=sys.stderr)
        return 1
    print(f"archived {key} ({sha[:12]}…, {entries} entries)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
