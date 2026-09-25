# venue-store

How the mini venue's services keep their records in PostgreSQL (blueprint §18: small records that change go in
PostgreSQL). A library, not a process: each service links it, owns its own tables, and runs its own migrations. The
reasons for each choice are in `decisions.md` beside this file (S-1 onwards).

## Configuration

Every service that keeps records reads the same settings, as a flag or as `VENUE_<FLAG>`:

| Flag | Environment | Default | |
|---|---|---|---|
| `--database-url` | `VENUE_DATABASE_URL` | none | `postgres://user:password@host:5432/database`; libpq's `sslmode` and `sslrootcert` parameters are understood |
| `--database-schema` | `VENUE_DATABASE_SCHEMA` | the service's name | the schema its tables live in |
| `--database-pool-size` | `VENUE_DATABASE_POOL_SIZE` | 4 | connections at most |
| `--database-tls` | `VENUE_DATABASE_TLS` | the URL's `sslmode`, else `prefer` | `disable`, `prefer`, `require`, `verify-ca` or `verify-full` |
| `--database-ca-file` | `VENUE_DATABASE_CA_FILE` | the URL's `sslrootcert`, else the system's CAs | a PEM file of CAs to trust |

Self-hosted and managed (RDS, Aurora) databases differ only in these values: nothing in the code knows which it has.
A managed database is normally reached with `verify-full` and its provider's CA bundle mounted as a file.

Connections have a 5 s connect timeout, a 10 s TCP user timeout and keepalives, so a database that vanishes is noticed
in seconds rather than when the kernel gives up; statements time out after 30 s; each session runs in UTC with the
service's schema first on its `search_path`.

## Migrations

Each service keeps its migrations as plain SQL files in `migrations/`, named as Atlas names them,
`<version>_<name>.sql`, with the `atlas.sum` Atlas keeps beside them, and embeds them in its binary
(`Migration { version, name, sql: include_str!(...) }`); a test checks that the files, the sum and the embedded list
agree (`check_files`). They are forward-only: a mistake is fixed by a new migration, never by editing one.

**Atlas applies them**, in a test, on a laptop and in a deployment alike (`decisions.md`, S-2). It keeps its history
in `atlas_schema_revisions` in the service's own schema, takes its own lock so two runs queue rather than collide, and
applies only what is missing:

```sh
atlas migrate apply --dir file://core/treasury/migrations --revisions-schema treasury \
  --url "$VENUE_DATABASE_URL?search_path=treasury&sslmode=verify-full"
atlas migrate status --dir file://core/treasury/migrations --revisions-schema treasury --url "..."
```

A deployment runs exactly that, from the files in the image, as the chart's migrate job (`deploy/README.md`).
`Database::migrate` runs the same Atlas for tests and for `--migrate-on-start true` on a laptop; it finds the binary
at `VENUE_ATLAS` or on the PATH, and `./check` downloads a pinned one when it is missing. `VENUE_MIGRATIONS` says
where the files are when they are not beside the crate, as they are not in an image.

**Rolling back** is `atlas migrate down`, which plans the revert against a throwaway dev database and checks before
it destroys anything — reverting a column it has data in stops rather than dropping it:

```sh
atlas migrate down 1 --dir file://core/treasury/migrations --revisions-schema treasury \
  --url "$VENUE_DATABASE_URL?search_path=treasury" --dev-url "docker://postgres/18/dev"
```

**A service at start** calls `Database::wait_for_schema`: it carries on once its schema is at the last migration it
knows, waits with backoff while the schema is behind (saying so), and refuses to start when the schema is ahead of
this build — an older build must not write rows a newer migration defined.

**What Atlas does not catch**, and this does: a migration edited *after* it was applied, once its directory has been
hashed again. `atlas migrate apply`, `status` and `validate` all pass in that case, because the sum file and the files
agree again. So the first build to see a version applied records the SHA-256 of the SQL it embeds
(`venue_migration_sql`), and any later build holding something else for that version refuses to start. A released
migration must never be edited; this is what makes that more than a rule in a document.

## Pool and transactions

`Database::get` hands out one of at most `pool_size` connections, waiting up to 5 s. A connection that broke is closed
rather than returned, and the next checkout makes a new one: a database that comes back is used again without a
restart. `Database::transaction` runs a closure in a transaction and commits it if the closure returns `Ok`.
`Database::ping` is what health checks call. `Database::connect_retrying` is how services start: it tries again with
backoff (1 s doubling to 30 s), saying on stderr each time what it could not reach and why, never the password.

Errors are `Unavailable` (nothing is known to have been written; try again) or `Refused` (the database answered no: a
constraint, a trigger, or a check of the service's own).

## Tests

`testing::database(name, dir)` gives a test a schema of its own, with the migrations in `dir` applied by Atlas, on the server `VENUE_TEST_DATABASE_URL`
names, or prints `skipping <name>: VENUE_TEST_DATABASE_URL is not set` and returns `None`. Without Atlas the database
tests are skipped too, since it is what builds their schemas. `./check` starts a throwaway
server in a container for the run when Docker is usable (directly or through `sg docker`) and removes it afterwards; by
hand:

```sh
docker run -d --rm --name venue-pg -e POSTGRES_PASSWORD=dev -p 127.0.0.1:18400:5432 postgres:18.6-alpine3.24
VENUE_TEST_DATABASE_URL='postgres://postgres:dev@127.0.0.1:18400/postgres?sslmode=disable' \
  VENUE_ATLAS=core/target/tools/atlas-v1.3.3 cargo test --workspace
```

## Archiving a database

`interfaces/tools/db_archive.py` takes a verified `pg_dump` of one database to object storage. A CronJob per
database runs it (`deploy/charts/venue-mini/templates/dbarchive.yaml`, off unless `database.archive.enabled`).

```sh
VENUE_DATABASE_URL=... db_archive.py --database identity --to s3://BUCKET --environment prod
# -> databases/prod/identity-20260925T093224Z.dump, with a .sha256 beside it
```

**It copies and never deletes** — not from the database, not from the bucket — and that is deliberate twice over.

These databases are *append-only by construction*: each installs a trigger that raises on `DELETE` and
`TRUNCATE` and `REVOKE`s those privileges, and each has a test asserting the refusal. Treasury's message says
what the rule is for — "a correction is a compensating entry, never an edit". Age-based pruning would mean
removing that.

They are also not a capacity problem. Measured 2026-09-25, all four together held about **4 MB** of application
data after half a day of continuous trading — roughly **0.34 MB/hour**, against the engine journal's
**35 MB/hour**. The journal was what filled the disk, and it is cut and archived separately (`core/node`). What
the databases lacked was not pruning but *any copy at all*: the journal ships to S3, the audit log ships to S3,
and the ledger and identity records existed only on one node's local disk.

Deciding what may eventually be *removed* from a store is interface 29 ("Stores → retention archive",
`interfaces/catalogue.toml`), which is planned, unbuilt, and has to answer to the legal-hold register that
outranks every retention period. This tool is only the copying half and takes no position on the other.

Three details worth knowing before changing any of it:

- **`pg_dump` must not be older than the server.** `deploy/images/venue-dbarchive.Dockerfile` is built `FROM` the
  same digest-pinned PostgreSQL image the server runs, rather than `apk add postgresql-client` — Alpine 3.21's
  client is 17 against a server on 18.6, which builds fine and then fails only when it meets a real database.
  Move this image's base when `values.yaml` moves the server's.
- **The password never reaches a command line.** The URL comes from the environment; the tool parses it and hands
  the password to `pg_dump` in `PGPASSWORD`, so it is not visible in `ps`.
- **A dump of `identity` is Restricted data.** `members` is the venue's only personal data and `outbox_mail`
  carries addresses too. The destination bucket must block public access and encrypt at rest; treat a dump the
  way you would treat the database.

The dump is verified twice: `pg_restore --list` proves it is a readable archive rather than bytes that merely
arrived intact — a checksum cannot tell you a dump was truncated at source — and then the object is read back
from storage and compared before anything is reported archived.
