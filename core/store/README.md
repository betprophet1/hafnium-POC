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

Each service lists its migrations in code (`Migration { version, name, sql }`), versions from 1 with no gaps. At
start, and in the service's `migrate` subcommand, `Database::migrate`:

1. takes a transaction-scoped advisory lock for the schema, so services starting together queue rather than race;
2. creates the schema if it does not exist (when the role may) and the `schema_migrations` table;
3. refuses to go on if a migration already applied has a different name or checksum (a released migration was
   edited) or the schema is at a version this build does not know (an older build started against a newer database);
4. applies the rest in order and records each, all in that one transaction: a failure leaves the schema as it was.

Running it again does nothing. DDL rights can be kept from the running service by running `migrate` as a separate job
with a role that has them, and starting the service with a role that does not; the startup run then finds nothing to do.

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
