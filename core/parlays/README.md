# venue-parlays: the parlay service

Parlays for a mini venue (blueprint §6), behind the gateway's requests for quote (interface 1,
`interfaces/gateway/README.md`, parlays). Test environments only.

| Module | What it is |
|---|---|
| `src/rules.rs` | What a parlay is, as pure functions of the feed's view: canonical legs, the parlay's ID, the inputs that list it, and what its legs call for (expire, void, resolve, halt, close). |
| `src/responders.rs` | Who may respond and who is listening: the approved responder accounts and their records in PostgreSQL (`migrations/`), the sessions subscribed and their filters, and where a request goes. |
| `src/service.rs` | The process: follows the feed, lists parlays on their first request, holds open requests and the quotes attached to them, fans requests out, and sequences what the legs call for. Its protocol with the gateways is in the module comment. |
| `src/operator.rs` | The operator API: approving and revoking responders. |

```sh
venue-parlays migrate --database-url postgres://USER:PASSWORD@HOST:5432/parlays
venue-parlays --ingress 127.0.0.1:7101 --feed 127.0.0.1:7102 --catalogue markets.json \
  --database-url postgres://USER:PASSWORD@HOST:5432/parlays \
  [--listen 127.0.0.1:7103] [--window-seconds 30] [--max-parlays 100] [--capacity small] \
  [--operator 127.0.0.1:7104] [--min-quantity 10] [--store postgres|memory]
```

Every flag can also be given as `VENUE_<FLAG>`.

## Requests

A request names two to six legs and a quantity: a multiple of the parlay lot step (10) and at least
`--min-quantity`, which is one lot unless an environment sets more. The first request for a leg set lists the parlay.
A request accepts quotes for `--window-seconds` of sequencer time and is remembered for an hour after its window and
its last quote have gone, so the requester can still read how it went. Requests are in this process's memory only:
the engine has no RFQ state machine (spec §19), and the quotes that matter are orders, which are journaled.

## Responders

**Approval.** Only approved accounts see requests and quote. The list is kept in PostgreSQL (schema `parlays`,
`core/store`): `responders` holds the accounts approved now and `responder_changes` every approval and revocation,
append-only, with who made it. The first time the service starts on an empty schema, the list starts from the
catalogue's `"responder": true` accounts; after that the database, not the catalogue, is the truth, so a revoked
catalogue responder stays revoked across restarts. An operator changes it through the operator API. The service
reads the list when it starts and routes from memory; a change is written to the database before it is made, and
refused with 503 if it cannot be. While the database is unreachable, approvals and revocations are refused, requests
keep being routed to the responders approved in memory, and `/healthz` says `"database": "unavailable: ..."` while
staying ready. At start the service waits for the database and its schema (`venue-parlays migrate`), saying why.
`venue-parlays import --responders FILE` starts the list from a file kept by earlier versions (`{"responders": [10,
12]}`); `--store memory` keeps approvals in memory only, for throwaway local venues. Approval is per account; whether a
credential may act for a responder (an API key's `respond` permission) is the gateway's to check, and both are needed.

The operator API listens on `--operator` (`none` turns it off), apart from the gateways' port:

| Call | Does |
|---|---|
| `GET /v1/responders` | the approved accounts, each with its name and how many sessions it has subscribed |
| `PUT /v1/responders/{accountId}` | approves a catalogue account |
| `DELETE /v1/responders/{accountId}` | revokes one: its subscriptions end at once, each session sent `rfqSubscription` with `subscribed: false`, and it can attach no more quotes; quotes it attached already are orders and stay on the book |
| `GET /healthz` | `{"ready": true, "database": "ok"}` once caught up with the feed, 503 until then; `database` is `unavailable: ...` while the approvals' database cannot be reached |

```sh
curl -X PUT http://127.0.0.1:7104/v1/responders/12
curl http://127.0.0.1:7104/v1/responders
```

**Subscriptions.** A gateway subscribes one of its sessions for an approved account (`subscribeRfqs` on the private
WebSocket), with optional filters: `minLegs`, `maxLegs`, `eventIds` and `instrumentIds` (a request passes if any leg is
on one of them) and `minQuantity`, every one given having to pass. The answer carries the requests still open that
the filters select, which the gateway sends the session ahead of the reply. Subscribing and reading what is open
happen under the same lock as opening a request, so a request is either in that catch-up or sent afterwards, never
both and never neither. Subscriptions belong to the gateway connection they came through and are forgotten when it
ends.

**Fan-out.** Each new request goes to every subscribed session of an approved account whose filters select it, except
the requester's own sessions, grouped into one message per gateway. There is no cap on how many responders receive a
request. The requester is never named to a responder, and the requester is told only how many responder accounts
were reached (`responders`). A responder cannot quote its own request.

## Restarting

The service is meant to be restarted freely. On start it reads the approved responders from its database, binds its ports at once, and
replays the feed from the beginning to rebuild parlays, their legs and how each stands, refusing new requests with
`UNAVAILABLE` until it has caught up and deciding nothing (no halts, closes or settlements) from a picture of the
past. Settlement commands carry fixed IDs per parlay, so one sent before a restart and again after it is sequenced
once.

What is lost: open requests, the link from quotes to their requests (the quotes stay on the book as orders, but their
requesters are no longer sent `quote` updates), and subscriptions. Subscriptions come back without the responder
doing anything: each gateway remembers what its live sessions subscribed with and sends it again as soon as it has
reconnected (it tries every second, so within about a second of the service listening again), before anything else.
A resubscription the service refuses (an approval revoked meanwhile) ends that session's subscription with an
`rfqSubscription` message. A client whose own session ended subscribes again on its new one, as the synthetic maker
does after every login. In a local run, a restarted service was ready in under 100 ms and the synthetic maker was
resubscribed by its gateway within a second, without logging in again.

## Tests

`cargo test -p venue-parlays` covers the rules, approval and its records (in PostgreSQL when `VENUE_TEST_DATABASE_URL` is set), filters, fan-out and anonymity, and catch-up.
The gateway's tests (`core/gateway`) run the service against a real node: subscribing, catching up, refusal of an
account that is not approved, a quote attached and taken end to end, and, against a stand-in service, subscriptions
sent again after a reconnect.
