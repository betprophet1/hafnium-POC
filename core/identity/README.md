# venue-identity

The identity service of test environments: it keeps every account's API keys and tells the gateways which account and
permissions a key's credential stands for (interfaces/gateway/README.md, API keys and permissions; the reasons for each
choice are in interfaces/gateway/decisions.md, G-1 to G-12). Clients never reach it: they create, list and revoke keys
through the private half of the gateway (`/v4/keys`), which authenticates them and passes the call on.

It is test-environment identity. Production needs interface 6 and the environment manifest's issuer (interface 22):
a store with encryption at rest and replication, secrets kept or at least peppered
with a key in a hardware security module, a second factor and approvals for making keys that move money, alerts on a
key used from somewhere new, and the people who own accounts, which this service does not know about at all.

## What a key is

| | |
|---|---|
| ID | `ak_` and 16 lower-case hex digits, from `/dev/urandom`. Public. |
| Secret | 64 lower-case hex digits (256 bits), from `/dev/urandom`, answered once when the key is made. |
| Credential | `<keyId>.<secret>`, which clients send wherever a token goes. |
| Kept as | `SHA-256("venue-api-key/1:" + keyId + ":" + secret)`, compared in constant time. |
| Permissions | `read` (every key), `trade`, `transfers`, `keys`. |
| Also | a label, when it was made, an optional expiry, when it was revoked, the key that made it, its last use. |

At most 20 keys of an account are neither revoked nor expired at once. A key cannot be changed, only revoked.

## The records

Accounts and keys are kept in PostgreSQL (`migrations/`, through `core/store`), in the database and schema `identity`;
the decisions are S-13 to S-16 of `core/store/decisions.md`.

| Table | Holds |
|---|---|
| `accounts` | each account of the environment: its ID, display name and, in test environments, `SHA-256("venue-dev-token/1:" + token)` of its dev token; seeded from the catalogue at every start (new accounts added, names and tokens brought up to date, none removed) |
| `api_keys` | each key: account, label, permissions, `secret_sha256` (never the secret), when it was made and by which key, expiry, revocation, last use |

`accounts` holds no personal data. It is where the mapping from pseudonymous account IDs to people will live (blueprint
§12: the journal, ClickHouse and Valkey carry account IDs only; the mapping lives only in the accounts database), in
tables of their own that refer to it, with retention of their own; an account is never deleted.

The database refuses a key's deletion, any change to its terms, and a revocation undone or moved; only its last use
moves, written at most once a minute per key. Every key is read into memory at start, and verification is answered
from memory. A change is written before it is made in memory or answered; a write that fails is refused with 503, and
the keys are read again from the database before the next call, so a revocation whose acknowledgement was lost is not
missed.

When the database cannot be reached: at start the service tries again with backoff, saying why, and does not listen
until it can, then waits for its schema to be at this build's migration (`venue-identity migrate`, the chart's
migrate job). While running, making and revoking keys answer 503; verification carries on from memory; `/healthz`
answers 503, so the pod is taken out of its Service and gateways answer keys they have not verified in the last 5 s
with 503 (G-7), while dev tokens keep working. `--store memory` keeps keys in memory only, for tests and throwaway
local venues (the FIX adapter's test uses it when there is no test database). `venue-identity import --file FILE`
moves a key store kept in a JSON-lines file by earlier versions into empty records.

## Its own API

Not interface 1, and never exposed beyond the gateways: it trusts the account in the path. The key calls answer in
interface 1's terms (`Key`, `KeyCreated`, `Keys`, `Error`), so the gateway passes them on untouched.

| Call | What it does |
|---|---|
| `POST /v1/accounts/{accountId}/keys` | makes a key: a `KeyRequest`, plus `createdBy` (a key ID) when a key asked; answers `KeyCreated` |
| `GET /v1/accounts/{accountId}/keys` | every key of the account, newest first |
| `DELETE /v1/accounts/{accountId}/keys/{keyId}` | revokes a key; one already revoked is answered as it is |
| `POST /v1/verify` | `{"credential": "<keyId>.<secret>"}`: `{"keyId", "accountId", "permissions", "expiresAt"}`, or 401 `UNAUTHORIZED` for an unknown, wrong, revoked or expired key; notes the key's use |
| `GET /healthz` | |

The gateway enforces what a caller may make (a key's permissions and expiry no more than the key asking); the service
checks only that a key is well formed and its account is in the catalogue. An operator with network access to the
service can make the first key of an account where dev tokens are switched off.

## How the gateway uses it

A gateway started with `--identity ADDR` (`VENUE_IDENTITY`) asks `POST /v1/verify` about a credential it has not seen
in the last 5 seconds and remembers the answer, valid or not, for 5 seconds. Every second it asks again about the
credentials of logged-in private sessions whose answers are older than that, and closes sessions whose keys are no
longer valid, so a revocation or expiry takes effect within 10 seconds everywhere. A gateway without `--identity`
refuses keys and `/v4/keys` with 503.

## Running it

```sh
export VENUE_DATABASE_URL='postgres://postgres:dev@127.0.0.1:5432/postgres?sslmode=disable'
target/release/venue-identity migrate
target/release/venue-identity --catalogue ../deploy/charts/venue-mini/files/markets.json --listen 127.0.0.1:8084
target/release/venue-gateway --ingress 127.0.0.1:7101 --feed 127.0.0.1:7102 \
  --catalogue ../deploy/charts/venue-mini/files/markets.json --identity 127.0.0.1:8084

# The first key of the retail customer, with its dev token:
curl -s -H 'Authorization: Bearer dev-retail-customer' -d '{"label": "my bot", "permissions": ["read", "trade"]}' \
  http://127.0.0.1:8081/v4/keys
```
