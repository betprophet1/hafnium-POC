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

Each service keeps its migrations as plain SQL files, `migrations/V001__<name>.sql`, `V002__...`, forward-only, and
embeds them in its binary (`Migration { version, name, sql: include_str!(...) }`); a test checks that the files and
the embedded list agree (`check_files`), so DevOps can also run the files with another tool. `Database::migrate`:

1. takes a transaction-scoped advisory lock for the schema, so two runs at once queue rather than collide;
2. creates the schema if it does not exist (when the role may) and the `schema_migrations` table;
3. refuses to go on if a migration already applied has a different name or checksum (a released migration was
   edited) or the schema is at a version this build does not know (an older build against a newer database);
4. applies the rest in order and records each, all in that one transaction: a failure leaves the schema as it was.

Migrations run explicitly, through each service's `migrate` subcommand (the chart runs it as a Helm hook job). A
service at start calls `Database::wait_for_schema`: it carries on once its schema is at the last migration it knows,
waits with backoff while the schema is behind (saying so), and refuses to start if the schema is ahead or edited. For
laptops and tests, `--migrate-on-start true` (`VENUE_MIGRATE_ON_START`) applies the migrations at start under the same
lock. A mistake is fixed by a new migration, never by editing or undoing one.

## Pool and transactions

`Database::get` hands out one of at most `pool_size` connections, waiting up to 5 s. A connection that broke is closed
rather than returned, and the next checkout makes a new one: a database that comes back is used again without a
restart. `Database::transaction` runs a closure in a transaction and commits it if the closure returns `Ok`.
`Database::ping` is what health checks call. `Database::connect_retrying` is how services start: it tries again with
backoff (1 s doubling to 30 s), saying on stderr each time what it could not reach and why, never the password.

Errors are `Unavailable` (nothing is known to have been written; try again) or `Refused` (the database answered no: a
constraint, a trigger, or a check of the service's own).

## Tests

`testing::database(name, migrations)` gives a test a schema of its own on the server `VENUE_TEST_DATABASE_URL`
names, or prints `skipping <name>: VENUE_TEST_DATABASE_URL is not set` and returns `None`. `./check` starts a throwaway
server in a container for the run when Docker is usable (directly or through `sg docker`) and removes it afterwards; by
hand:

```sh
docker run -d --rm --name venue-pg -e POSTGRES_PASSWORD=dev -p 127.0.0.1:18400:5432 postgres:18.6-alpine3.24
VENUE_TEST_DATABASE_URL='postgres://postgres:dev@127.0.0.1:18400/postgres?sslmode=disable' cargo test --workspace
```
