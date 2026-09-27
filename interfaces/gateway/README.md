# Interface 1: the REST and WebSocket gateway

Draft v1. `openapi.yaml` is the REST half and `asyncapi.yaml` the WebSocket half; this file holds the semantics
both share. The gateway is the edge of blueprint §5 and §13: every app, bot and tool reaches a sequencer group
through it, and it is the only client-facing component that writes to one (interface 7).

## What the gateway does, and what it never does

It authenticates the session and checks that its credential holds the permission each request needs, validates
every message completely, converts prices and stakes into the journal's tick ordinals and contract counts, enforces
flood limits, turns a disconnect or missed heartbeat into a sequenced mass cancel, routes to the sequencer group, and
answers once the input is committed. It fans out each account's executions and conflated public market data.
Deposits and withdrawals it passes to the treasury once it has authenticated the account, and it fans out the
treasury's updates to the account's private sessions.

It never decides anything the engine decides. Client order ID duplicates, lot steps, market state, rule limits and
collateral are sequenced checks (spec §9), so their rejections replay. It never decides anything about money either:
the treasury owns every transfer and its ledger (blueprint §11). Nor does it keep API keys: the identity service does,
and the gateway only asks it whether a key is valid and remembers the answer for a few seconds. The gateway holds no
state a failover could lose except flood-limit counters, which by design never reach the journal (blueprint §12), and
that cache.

## Public and private

The gateway has two halves, told apart by path alone: `/v4/ws_private`, `/v4/account`, and `/v4/orders`,
`/v4/transfers`, `/v4/rfqs` and `/v4/keys` with everything under them are private; `/v4/ws_public` and
`/v4/markets` with everything under it are public; `/healthz` belongs to both.

| | Public | Private |
|---|---|---|
| WebSocket | `/v4/ws_public` | `/v4/ws_private` |
| REST | `GET /v4/markets`, `GET /v4/markets/{instrumentId}/book` | `GET /v4/account`; `/v4/orders`, `/v4/transfers`, `/v4/rfqs`, `/v4/keys` and everything under them |
| Login | never | required: a credential (an API key or a dev token) on every REST call, `login` first on a session |
| Carries | markets (parlays included), books and trades, the same for everyone | one account's orders, executions, account updates, payouts, releases and transfers; its requests for quote and their quotes |
| Reaches the sequencer group | never | every order, amendment and cancel; transfers through the treasury; a request for a parlay not yet listed, through the parlay service |

A private session carries nothing public, and a public session nothing private. A client that trades and watches the
market holds one of each and follows the market on the public one. An op sent to the wrong endpoint is refused with
an `error` (`BAD_REQUEST`) naming the endpoint it belongs on, and a REST path the half does not serve is 404.

The halves are separate so that watching never costs trading anything. Market data is fanned out to every viewer
(blueprint §9), which at tens of thousands of viewers is most of a gateway's work; orders and their answers are few
and latency-sensitive. Behind the paths they may be one gateway or two fleets scaled apart: that is a deployment
choice (blueprint §13), and a client addresses them only by path. `GET /healthz` is answered by both.

## Refused at the edge, or rejected by the engine

| | Refused by the gateway | Rejected by the engine |
|---|---|---|
| REST | HTTP 400, 401, 403, 404, 429 or 503 with an `Error` | HTTP 200, `OrderResult.status: rejected` |
| WebSocket | `error` message | `reply` with `status: rejected` |
| Journaled | Never | Always, with its reason |
| Examples | malformed body, bad credential, a key without the permission, price not on the ladder, stake off its step, flood limit | insufficient sleeve, market not open, duplicate client order ID |

A transfer the treasury refuses at its edge (a malformed request, a reused client transfer ID, a cancel too late) is an
HTTP 400 or 409 `Error` and creates nothing. A withdrawal the sequencer group refuses is HTTP 200 with the transfer
`failed`, because that refusal is journaled.

A 503 means the gateway could not get an answer from the sequencer group; the order may or may not exist. Resend
with the same `clientOrderId`: a committed original comes back as `status: duplicate` with its order ID.

A `clientOrderId` is remembered for **two hours** per account, across every market (spec E-9.2, D-12). Within that
window it cannot be reused, even by an order the engine rejected; an order the gateway refused never reached the
engine and does not use up its ID.

## The audit log

A refusal at the edge is never journaled, so the gateway keeps the record of it: every REST request and its
response, and every WebSocket's upgrade, frames and end, in the audit log format of `interfaces/audit/` (blueprint
§12, flood protection). On the private half every frame both ways is recorded; on the public half the client's frames
except heartbeats and the errors sent back, but not market data, and each REST request with its answer's status, length
and hash, the body only of a refusal or an error. Credentials are masked: the `Authorization` value
becomes `Bearer ***`, the `token` of a `login` becomes `***`, and so do the `secret` and `credential` of a key just
created. Every record of a request or session authenticated with an API key names the key (`key`), so an auditor can
tell which key did what; its secret never appears.

The log is written under `--audit-dir` (`VENUE_AUDIT_DIR`, default `./gateway-audit`), one file per UTC day named for
`--audit-instance` (`VENUE_AUDIT_INSTANCE`, default the host name), synced unless `--audit-fsync false`
(`VENUE_AUDIT_FSYNC`). The event loops only queue records; a writer thread writes them. A full queue makes a loop
wait rather than drop a record, and a queue full for 10 seconds, a failed write or an audit directory that cannot be
written stops the gateway (exit status 74) rather than let it serve unrecorded. `venue-gateway verify-audit FILE...`
checks a file's chain, as `interfaces/tools/audit_verify.py` does. Behind an ingress, `--trusted-proxies`
(`VENUE_TRUSTED_PROXIES`, a comma-separated list of addresses and networks, none by default) names the proxies whose
`X-Forwarded-For` or `Forwarded` header the gateway believes, and records then carry the client address they give as
`client` beside `peer`, the proxy (interfaces/audit/README.md, the client behind a proxy). Each gateway process records only its own traffic,
under an instance of its own: a gateway whose instance another live process is writing in the same directory does not
start.

## Deposits, withdrawals and internal transfers

`POST /v4/transfers` moves money into the account, out of it, or (`"kind": "internal"`, decisions D-34, G-22) into
another account of the same family; `GET /v4/transfers` lists the account's transfers and
`DELETE /v4/transfers/{transferId}` cancels one the bank has not decided yet. The gateway authenticates the account
and passes the request to the treasury, which validates it, decides it and answers in the same `Error` form; the
gateway never holds a transfer. A treasury that does not answer is a 503 whose outcome is unknown, as for an order.

**An internal transfer** (`toAccountId`, no `method`) moves cash between an account and one of its own family:
itself and its master, itself and a sibling subaccount, or a master and one of its subaccounts — never anywhere
else. The gateway checks this before the request ever reaches the treasury, asking identity fresh each time (never
cached the way a credential's verdict is): a request between accounts that do not share a master is 403 `FORBIDDEN`.
Both accounts must already be known to the treasury (a fresh subaccount opens the same way a fresh member's account
does, by depositing through its own key); the money moves as a reclaim from the source and a grant to the
destination (D-34), with no bank leg and nothing sent to the fake bank, so it settles in one step rather than
waiting on a bank decision.

The treasury owns the authoritative balance (blueprint §5, §11). Money reaches the sequencer group only as sequenced
sleeve changes the treasury sends (`SleeveAdjust`, spec E-7.2), never as a write by the gateway or a client:

| | Deposit | Withdrawal |
|---|---|---|
| On request | sent to the bank: `pending` | reclaimed from available cash, then sent to the bank: `pending`; a refused reclaim is `failed`, `insufficient_available_cash` |
| The bank settles | granted to available cash: `settled` | paid out: `settled` |
| The bank fails | `failed`, `declined_by_bank`; nothing was granted | granted back: `failed`, `returned_by_bank` |
| Cancelled while at the bank | `cancelled`; nothing was granted | granted back: `cancelled` |

A state other than `pending` is final. `clientTransferId` makes a request idempotent: resending returns the transfer
it created, and an ID is never reusable, even by a transfer that failed. Each change reaches every private session of
the account as a `transfer` message. These come from the treasury, not the journal: they carry no sequence and are
not replayed by `resumeFrom`, so a client that reconnects reads `GET /v4/transfers`. A transfer's own `sequence`
names the input whose `account` message carries the cash it moved.

Test environments have only the fake provider's methods (blueprint §16): `dummy_bank` is decided after a delay and
occasionally fails, `dummy_instant` settles at once, and `dummy_failing` always fails.

## API keys and permissions

A client authenticates with an **API key**: a credential for one account, of which the account may hold several, each
with its own permissions, label and optional expiry, so that a trading program, a reporting script and a person's
console each get only what they need and can be revoked alone. Keys are kept by the identity service
(`core/identity`), which the gateway asks; `decisions.md` beside this file says why each choice below was made.

**A key** has a public ID and a secret. The ID is `ak_` and 16 lower-case hex digits, and names the key everywhere,
in lists and in audit records. The secret is 64 lower-case hex digits (256 random bits), shown once, in the answer
that creates the key, and never again: the venue keeps only a hash of it and cannot tell anyone what it was. A client
presents both joined by a dot, `ak_0123456789abcdef.<secret>`, called the key's **credential**.

| Where | How the credential is sent |
|---|---|
| REST | `Authorization: Bearer <keyId>.<secret>` |
| WebSocket | `login` with `token: "<keyId>.<secret>"` |
| FIX | Username (553) the key ID and Password (554) the secret, or the whole credential in Password (interfaces/fix) |

**Permissions.** Each key holds some of these; every request needs one.

| Permission | Allows |
|---|---|
| `read` | `GET /v4/account`, `GET /v4/transfers` and one transfer, `GET /v4/rfqs/{requestId}`, and logging in to a private session, which streams the account's executions, account updates, payouts, releases, transfers, requests and quotes. **Every key holds it**: a key is refused at creation without it. |
| `trade` | orders, amendments, cancels and mass cancels (`/v4/orders`, and `order`, `amend`, `cancel`, `cancelAll`); requests for quote and quotes (`POST /v4/rfqs`, `POST /v4/rfqs/{requestId}/quotes`, `requestQuote`, `attachQuote`) |
| `transfers` | moving money: `POST /v4/transfers` and `DELETE /v4/transfers/{transferId}` |
| `keys` | managing the account's keys: `/v4/keys` |
| `respond` | answering requests for quote as a responder: `GET /v4/rfqs`, `subscribeRfqs` and `unsubscribeRfqs`, and, with `trade` as well, attaching a quote (`POST /v4/rfqs/{requestId}/quotes`, `attachQuote`). The account must also be an approved responder (parlays, responders): the permission and the approval are both needed. |

A request with a valid credential that lacks the permission is refused with HTTP 403 `FORBIDDEN`, or an `error` with
`FORBIDDEN` on the WebSocket, naming the permission; nothing is sent on, and the refusal is in the audit log with the
key's ID. A missing, unknown, wrong, revoked or expired credential is 401 `UNAUTHORIZED`. Cancel-on-disconnect and a
missed heartbeat cancel a session's orders whatever its key holds: they protect the account, and are not trading.

**Managing keys** (`/v4/keys`, REST only, needs `keys`). `POST /v4/keys` creates a key with a `label`, `permissions`,
an optional `expiresAt` and an optional `allowedIps`, and answers with the key, its `secret` and its `credential`.
`GET /v4/keys` lists every key of the account, revoked and expired ones included, newest first, without secrets.
`DELETE /v4/keys/{keyId}` revokes a key at once and for good; revoking one already revoked answers it as it is.
Nothing about a key can be changed after it is made: a key that should hold something else is replaced. A key that
manages keys cannot create a key with a permission it does not hold itself, nor one that outlives it: a key made by
an expiring key must expire no later. Revoking a key does not revoke the keys it created. An account holds at most 20
keys that are neither revoked nor expired.

**`allowedIps`** (decision G-23) restricts a key to the addresses or CIDR networks it names (`192.0.2.7`,
`10.42.0.0/16`), at most 20 entries; omitted or empty, a key works from anywhere, as every key made before this
existed does. A request from an address not on the list is refused with the same 403 `FORBIDDEN` a missing permission
gets, worded so a caller cannot tell an off-list address from a key that does not exist. The gateway checks the
request's real client address the same way it already resolves one for flood protection: the address a trusted proxy
named, or the connection's own. Checked on REST today; a WebSocket or FIX session's own logon-time check, and closing
a live session within the same 10-second bound a revocation gets, are not yet built.

**Revocation and expiry take effect within 10 seconds.** A gateway remembers what the identity service said about a
credential for up to 5 seconds, so a REST call with a key revoked or expired that moment is refused at most 5 seconds
later; a private session logged in with such a key is sent an `error` (`UNAUTHORIZED`) and closed within 10 seconds,
which cancels its orders if it asked for cancel-on-disconnect. The identity service not answering is 503
`UNAVAILABLE` for a key not already remembered; dev tokens do not depend on it.

A key's **last use** is kept to within a minute: the identity service records it when a gateway asks about the key,
which is at most every few seconds while the key is in use.

## Subaccounts

A market maker can organise trading into accounts that share an owner, its **master**, but cannot trade against one
another (spec/engine/decisions.md, D-33) — a liquidity-provision book and a scalping book, say. A family is two
levels deep at most: a master may not itself be a subaccount, and an account with subaccounts of its own may not
become one.

`POST /v4/subaccounts` (REST only, needs `keys`) makes one under the caller's own account, with a `label`; the
caller's account must not itself be a subaccount. `GET /v4/subaccounts` lists an account's own, oldest first.

Making a subaccount does two things: identity commits it, and the gateway tells the matching engine
(`SetMasterAccount`, spec E-7.13) so self-match prevention actually protects the family from that moment (D-33). If
the gateway dies between the two, `POST /v4/subaccounts` answers 503 rather than success, but the subaccount already
exists in identity, unprotected, until the next minute's reconciliation sweep resends the same command and closes
the gap on its own (decision G-25) — no restart and no operator needed.

A fresh subaccount has no dev token and no session of its own to make a key with, so the master's own credential
does it instead: `POST /v4/subaccounts/{accountId}/keys` (and its `GET`/`DELETE`), needing `keys` and that
`accountId` be one of the caller's own subaccounts — checked fresh against identity on every call, never cached.
The key it makes belongs to the subaccount, not the master: it does not appear in `GET /v4/keys`, and everything it
does — trading, transfers, more keys of its own if it holds `keys` — is the subaccount's, not the master's.

Moving money between a master and its subaccounts is an internal transfer (above, decisions D-34, G-22), not a
special subaccount API of its own.

## Dev tokens in test environments

Each account in an environment's catalogue also has a fixed dev token (`deploy/charts/venue-mini/files/markets.json`),
sent exactly as a key's credential is, which the demo, the bots, the tests and the web page use. A dev token holds
every permission, `keys` included, so it is how the first key of an account is made in a test environment. It is not
a key: it is not listed, cannot be revoked, and its records carry no `key`. A gateway started with
`--dev-tokens false` refuses them.

Keys and dev tokens are both test-environment identity: the identity service here keeps its store on a local disk
with no encryption at rest, no hardware-backed secrets, no second factor for managing keys and no issuer of its own.
Real identity is interface 6, with the environment manifest's issuer (interface 22); keys and their permissions are
the shape it should keep.

## Identifiers

Account, event, instrument, order and trade IDs and sequence numbers are decimal strings. A session ID is assigned
by the gateway at login and is unique across gateway restarts, because a mass cancel by session (spec E-12.6) must
never reach an older session's orders.

## Prices

Every ladder is a table of exact rationals, closed under complement (blueprint §6). A price on input is the price of
the outcome the order names, given as exactly one of:

- `tick`: the row's ordinal;
- `probability`: an exact rational, `"53/100"`;
- `cents`: price per $1 contract, `"53"`, `"0.5"`;
- `american`: American odds, `"-110"`, `"+150"`.

Each form is turned into an exact rational and must equal a row. Nothing is rounded: a price between rows is refused
with `PRICE_NOT_ON_LADDER`. Every price on output is a `PriceQuote` carrying the tick, the exact probability, cents,
and American odds when they are a whole number.

Books, resting orders and executions are in canonical YES terms (spec E-2.5), because the journal does not record
which outcome an order was entered in. A NO bid at 47¢ appears as a YES ask at 53¢, and the best NO price to buy is
100¢ minus the best YES bid.

## Orders

| `action` | `outcome` | Journal side | Entered contract | Tick sent |
|---|---|---|---|---|
| buy | yes | BUY | YES | the row of `price` |
| buy | no | SELL | NO | the complement of the row of `price` |
| sell | yes | SELL | YES | the row of `price` |
| sell | no | BUY | NO | the complement of the row of `price` |

`quantity` is contracts. `stakeCents` (buys only) is money to spend: at a price of `n/d` a contract costs `100·n/d`
cents, so a stake buys `stake·d / (100·n)` contracts. The gateway accepts the stake only when that is a whole
multiple of the lot step, and refuses it otherwise with `STAKE_NOT_ON_STEP` and the step in `details`. The stake is
journaled as `enteredStakeCents` beside the contract count (blueprint §6: stake on the wire, contracts in the
journal).

`timeInForce`, `postOnly`, `reduceOnly`, `protectionTicks`, `spendCapCents`, `clientOrderGroup` and `atEventStart`
map one to one onto `NewOrder` (spec §9). `expiresAt` is converted to nanoseconds of sequencer time.

Amending needs no instrument: the gateway knows each resting order's instrument. A new price is in YES terms unless
`outcome` says otherwise.

A venue may run several gateways, and each learns of orders from the sequencer group's feed, which can reach it a
moment after another gateway has answered the client that placed the order. An amendment or cancel naming an order ID
the gateway's feed has not delivered yet waits up to two seconds for it, and is then sent as usual; one still not
delivered is refused with 503 `UNAVAILABLE`, and nothing was sent (decision G-14). An order ID the feed has passed and
that no longer rests is 404 `NOT_FOUND` at once. Reads are as of their `sequence`: `GET /v4/account` on one gateway
shows an order placed through another once that gateway's feed has reached it.

## Fees

Each party to a fill, the maker (the resting order's account) and the taker (the incoming order's), pays a fee on
that fill of

```
rate × contracts × P × (1 − P)   dollars
```

where `P` is the fill's YES price as a fraction (`29/100` at 29¢), rounded up to the next whole cent, per fill (spec
E-10.11, decisions D-2). An order that fills in several pieces pays on each piece, so it can pay up to a cent a fill
more than the formula on its whole quantity. The fee is largest at 50¢ and shrinks toward 0¢ and 100¢. Exactly, in
the spec's integers, with the rate `r` in millionths and `yesCost` and `noCost` the fill's YES and NO cost in cents
(E-2.4):

```
fee = ⌈ r · yesCost · noCost / (100 000 000 · contracts) ⌉   cents
```

Rounding is always toward positive infinity, for both signs. A negative rate, which only a maker rate may be, is a
rebate, and rounding up takes it toward zero: a rebate is never larger than the formula (`⌈−0.8⌉ = 0`, `⌈−8.3⌉ = −8`).

Maker and taker rates are separate. They come from a default schedule (E-7.9), which overrides replace for the
parties they match (E-7.10, E-10.12, D-27). An override names an account, a market and an event, each or all of them
"any", with both rates, a priority and optionally an expiry. For each party separately, of the unexpired overrides
that match its account, the fill's market and that market's event, the lowest priority number wins, and a tie goes
to the lower override ID; with none, the default schedule applies. Which fields an override names does not matter,
only its priority. Rates are set by the operator; this draft has no endpoint that lists them.

Fees are charged when the fill happens. They are not reserved when an order is accepted or amended, no order is
refused for a fee it would pay, and a spend cap counts the price without fees, so a fee can leave available cash
below zero; the account can then receive cash but spend none until it is funded (E-3.3, D-26). A void refunds the
event's fees, and takes back its rebates, with the rest of the event's cash (D-1, D-28).

Every `fill` execution, on REST and WebSocket alike, carries `feeCents`: what this account paid at its own maker or
taker rate, negative for a rebate, taken from cash on top of `cashCents`, which excludes it.

A worked example. A taker buys 10 contracts at 21¢ at the 7% taker rate: `0.07 × 10 × 0.21 × 0.79 = 0.11613`
dollars, 11.613¢, so it pays 12¢. In integers, `yesCost = 210`, `noCost = 790` and
`⌈70 000 · 210 · 790 / (100 000 000 · 10)⌉ = ⌈11.613⌉ = 12` cents. A maker on the other side at 1.75% pays
`⌈2.903⌉ = 3` cents.

The demo venue's seed (`core/link`, `Catalogue::fees`) sets a default schedule of 7% taker and 1.75% maker, and one
override for the synthetic maker's account (account 10: override 1, priority 10, any market and event, no expiry):
a maker rebate of 0.5% and no taker fee, as a market maker on a programme would have. At 0.5% a small fill can round
to no rebate at all: the synthetic maker selling 10 contracts at 21¢ gets `⌈−0.8295⌉ = 0`, and 100 contracts at 21¢
earn it `⌈−8.295⌉ = −8`, an 8¢ rebate.

## Sessions: cancel-on-disconnect and heartbeats

These are private sessions, on `/v4/ws_private`. `cancelOnDisconnect` in `login` makes the end of the session, a
clean close or a dropped connection, a `MassCancel` of the session's resting orders with cause `SESSION_DISCONNECT`.
`heartbeatSeconds` makes silence that long a `MassCancel` with cause `HEARTBEAT_MISSED`, then a close. Both are keyed to the client session, never to the
gateway's connection to the sequencer group, so a failover does not empty the books (blueprint §5). REST requests
have no session and never cancel on disconnect.

The mass cancel is sent by the gateway the session was on, so a gateway that stops without seeing its sessions end (a
crash, a killed pod) cancels nothing for them: their orders stay resting under their old session IDs, which no later
session has. A client that asked for cancel-on-disconnect and reconnects through another gateway cancels them itself
(`cancelAll`, which reaches every order of the account) or takes them up from `GET /v4/account`.

A `heartbeat` that carries an `id` is answered with an empty `reply`. A client that sends one on a schedule always
hears back from a gateway that is alive, so it can close and reconnect a session that has gone silent instead of
mistaking a lost gateway for a quiet market. The mini venue's web page does this every 15 seconds.

## Sessions: being streamed to, or not

`stream` in `login` is accepted and has no effect: every session is streamed to as things happen. It existed so that
screens could ask to be woken rarely, when a gateway held a thread per connection and woke each one on a timer. A
gateway now waits on all its connections at once and wakes only for a connection with something to send, so an
idle session costs nothing to be prompt for, and screens watch the market on the public endpoint anyway.

## Flood limits

Per session, in the gateway's memory, as generic cell rate buckets (blueprint §12): messages per second, and logons
per minute per account. Exceeding one is HTTP 429 with `Retry-After`, or an `error` with `RATE_LIMITED`. Nothing
refused this way reaches the journal. REST requests have no session, so their bucket is per account. Buckets are kept
by each gateway process and shared with no other (decision G-13): an account refused by one gateway is still served by
another, and a venue of N gateways lets an account through up to N times the REST and logon limits. The limits are
set comfortably above legitimate use; what a customer should actually meet is a rule limit, which is sequenced and
explainable (spec E-9.4).

## Market data

| Topic | Contents | Delivery |
|---|---|---|
| `markets` | market state changes, event phases, resolutions and releases of settled events | every change |
| `book.{instrumentId}` | a full snapshot to the requested depth, both sides | a snapshot on subscribe, then at most ten a second, only when the book changed |
| `trades.{instrumentId}` | each trade: price, quantity, taker side | every trade |

Topics are subscribed to on `/v4/ws_public`, which needs no login. A retail client watching a market never causes
work in a sequencer group (blueprint §9).

## Sequence numbers and resuming

Every message derived from the journal carries `sequence`, the input that produced it. A client ignores anything at
or below the last sequence it applied. `login` with `resumeFrom` replays the account's executions, account updates
and payouts after that sequence if the gateway still holds them, and answers `resumed: true`; otherwise it sends
`resync`, and the client fetches `GET /v4/account` before continuing.

## Parlays and requests for quote

A parlay pays $1 if every one of its legs resolves as specified, and nothing otherwise (blueprint §6): economically a
binary, so it clears like any other contract. Parlays are created on demand by a request for quote (RFQ), and once
one exists it is an ordinary market with a public book that anyone can trade.

**The instrument.** A request names two to six legs, each a market and an outcome (`yes` or `no`). The legs are put in
canonical order, ascending by event, and must be on different events, on markets that are `open`, and not on
parlays. The parlay's ID is a hash of that canonical leg set, so the same parlay requested twice, in any order, is
the same instrument and accumulates one book. The first request lists it under an event of its own with two outcomes,
`Loses` and `Wins` (spec D-22), and opens it. `GET /v4/markets` shows a parlay as an event whose one market has
`class: parlay` and `legs`. Its ladder is finer than a listed market's, 0.1¢ from 0.1¢ to 10¢ and from 90¢ to 99.9¢
and 1¢ between, because parlays live deep out of the money; its lot step is 10 contracts, which that ladder needs to
keep every trade's cash in whole cents, and which is also the least a request may ask for.

**Requesting.** `requestQuote` on `/v4/ws_private`, or `POST /v4/rfqs`, with `legs` and `quantity`: a multiple of the
lot step, and at least the venue's minimum request, which the parlay service is configured with (one lot, 10
contracts, unless the environment says otherwise). The answer names the request (`requestId`), the parlay's
`instrumentId` and `eventId`, the canonical legs, `expiresAt`, the end of the request's window during which responders
may quote, and `responders`, how many responder accounts it was sent to. Anything wrong with the legs is refused at
the edge with `BAD_REQUEST` and a reason, and nothing is listed.

**Responders.** Only approved responders see requests and quote. Approval is per account and belongs to the parlay
service's operator, who approves and revokes accounts through the service's operator API; an environment starts from
the accounts its catalogue marks `"responder": true`, and the list survives restarts. A credential also needs the
`respond` permission (API keys and permissions), which every dev token holds and an API key holds only if it was
created with it; a key without it is refused even for an approved account, and an approved account's key with it
still needs the approval. Anything a
responder-only op or path is refused for, an account that is not approved included, is 403 `FORBIDDEN`. Revoking an
account ends its subscriptions at once (each session is sent `rfqSubscription` with `subscribed: false`) and refuses
its later quotes; quotes it attached already are orders and stay on the book.

**Subscribing.** A responder's private session asks for requests with `subscribeRfqs`, optionally with `filters`:
`minLegs` and `maxLegs` (2 to 6), `eventIds` and `instrumentIds` (a request passes if any leg is on one of them), and
`minQuantity`. Every filter given must pass. The session is sent an `rfq` message for each request still open that
its filters select, oldest first, then the reply (`subscribed: true`, the filters in force, and `openRequests`, how
many it was just sent), then an `rfq` for each new request its filters select as it opens, until the session ends or
sends `unsubscribeRfqs`. Nothing is missed or sent twice between the catch-up and what follows it, though a request
that opens while the subscription is being made may arrive just before the reply. Subscribing again
replaces the filters and catches up again, so a client may simply resubscribe whenever in doubt; a refused
`subscribeRfqs` leaves the session's subscription as it was. Subscriptions are per session: a responder with two
sessions subscribed receives each request on both, and counts once in `responders`. A client without a session
reads the same open requests with `GET /v4/rfqs`, unfiltered.

**Fan-out.** A request goes to every subscribed session of an approved responder whose filters select it, except the
requester's own sessions: an approved responder that requests is never sent its own request, and cannot quote it.
There is no cap on how many responders receive a request. The requester is never named, in an `rfq` message or
anywhere a responder can read.

**Responding.** A quote is a real resting order: the responder places a sell of YES on the parlay's book, usually GTD
so that it lapses (its collateral is reserved like any order's), and then attaches it to the request with
`attachQuote` or `POST /v4/rfqs/{requestId}/quotes`, naming the order. Only an approved responder's own resting sell
on the request's parlay, within the request's window, can be attached; being subscribed is not required. The order is
on the public book from the moment it rests, so anyone may take it, not only the requester.

**Receiving and accepting quotes.** The requester's private sessions receive a `quote` message when a quote is
attached (`state: open`), again whenever a fill reduces it, and a last time when it leaves the book (`filled`,
`expired` or `cancelled`). A REST client reads the same with `GET /v4/rfqs/{requestId}`. The responder is not named.
To accept, the requester places an ordinary order: buy YES on the parlay at the quote's price, usually `IOC` or
`FOK`. Nothing about acceptance is special; the fill is an execution like any other.

**Settlement.** The parlay service follows every leg. The first leg to resolve against the parlay expires it at once
(`ExpireParlay`, spec E-15.7: the parlay resolves `Loses`, and sellers' collateral comes back without waiting for
the other legs). A voided leg voids the whole parlay (`resolution.kind: void`, every trade in it reversed). When
every leg has won, the parlay resolves `Wins`. Whichever of a loss and a void happened first decides. A parlay trades
only while all its legs do: it halts while a leg is halted or its event suspended, and closes for good, cancelling its
resting orders, once any leg stops trading, whether or not it ever traded.

**What holds what.** Parlays, their legs, quotes as orders, fills and settlements are all in the journal. Requests,
and which order answers which request, are held by the parlay service, not by the gateway and not in the journal: a
gateway that fails over loses nothing, and every gateway connected to the service delivers the same requests and
quotes, so a requester and a responder need not share one.

**Restarts.** A restarted parlay service forgets open requests and subscriptions, rereads its approved responders,
and relearns parlays, their legs and how they stand from the journal; until it has caught up with the feed it refuses
new requests with `UNAVAILABLE`. Quotes stay on the book as orders, but their requesters are no longer sent `quote`
updates for them. Re-establishing subscriptions is the gateway's job, not the client's: a gateway remembers what each
of its live sessions subscribed with and subscribes them again as soon as it reaches the service again, so a
responder's session stays subscribed across a service restart without doing anything. A client whose session ends
(its gateway restarted, or it reconnected) subscribes on its new session. While the service is unreachable,
`subscribeRfqs`, `requestQuote`, `attachQuote` and `GET /v4/rfqs` are refused with `UNAVAILABLE`.

## Members and sessions

A person who uses an app is a **member**: an email, a password and an account of their own. This is the first cut of
interface 6, enough for the venue's own apps to sign people in; what a real identity service adds is listed at the end
of this section.

### Before a session

These are the routes a person reaches with no session, which is the point of them. Each is limited by the address it
came from rather than by an account, and none of them ever says whether an address is a member's.

| | |
|---|---|
| `POST /v4/members` | Signs up: `email`, `password` (10 to 256 characters) and an optional `displayName`. Answers `{"checkYourEmail": true}` and nothing else — no session, and no sign of whether the address was already somebody's. A message goes to it either way: the code that proves it, or a note that somebody tried to sign up with an address that already has an account. |
| `POST /v4/members/verification` | Sends the proving code again, for a member who has lost the message. Same answer. |
| `POST /v4/members/verify` | Proves the address: `token`, the code from that message. Answers `{"verified": true}`. The code lasts a day and is good once. |
| `POST /v4/members/reset` | Asks to set a new password: `email`. Same answer, and a message either way — a code, or a note that the address has no account. |
| `POST /v4/members/password` | Sets it: `token` (good for an hour, once) and `password`. Answers `passwordSet` and `sessionsEnded`. |
| `POST /v4/sessions` | Logs in: `email`, `password`, an optional `code` for a second factor, and an optional `device` naming what the app runs on. Answers `memberId`, `accountId`, `email`, `displayName`, `emailVerified` and a `session`. |
| `DELETE /v4/sessions/current` | Logs out the session the request is made with. |

### With a session

| | |
|---|---|
| `GET /v4/members/me` | The member this session belongs to, with `emailVerified`, `secondFactor` and `recoveryCodesLeft`. |
| `GET /v4/sessions` | Every session this member has held, each with the `device` its app named, when it was made and last used, and whether it is `live`. |
| `DELETE /v4/sessions/{sessionId}` | Ends one of their own. A session that is not theirs is `404`, whoever holds its ID. |
| `POST /v4/members/me/totp` | Starts a second factor: answers a `secret`, an `otpauth://` `uri` for the app's camera, and the `digits` and `periodSeconds` it expects. Nothing changes until a code from it arrives. |
| `PUT /v4/members/me/totp` | Confirms it with `code`, and answers the `recoveryCodes` — ten, shown this once and kept only as hashes. |
| `DELETE /v4/members/me/totp` | Takes it off. Needs the `password` in the body, so a session somebody else is holding cannot. |

Every route here is addressed by the member the session belongs to, never by one named in the request, so a session
can only ever reach its own. An API key is an account's rather than a person's, so it has no member routes at all
and is refused with `400`.

### What a session may do

A session's `credential` is a bearer token like an API key's, and works everywhere one does: the `Authorization`
header on REST, and `login` on `/v4/ws_private`. It lasts 30 days, and logging out ends it at once: the gateway
forgets what it remembered about a credential whose session it has just ended, rather than letting it stand for the
few seconds a cached answer would.

**Until the address is proved** it holds `read`, `trade` and `keys` over its own account: a member can look around
and trade what they have, and cannot move money in or out. Proving the address adds `transfers`, and a session
already open gains it without logging in again — within the five seconds the gateway remembers an answer for, the
same window a revoked key keeps working in. No session ever holds `respond`, which is a market maker's and needs the
parlay service's approval.

A new member's account has no money, **and cannot yet be given any**: the treasury knows only the accounts the
catalogue names, and a member's is allocated by the identity service from 100,000 upwards, so `POST /v4/transfers`
answers `404 NOT_FOUND`. Proving the address grants the `transfers` permission, and the call then fails one step
later for want of a ledger rather than for want of a right. Until an account made at sign-up reaches the treasury, a
member can look and trade but cannot fund the account they trade with.

### What the refusals mean

| | |
|---|---|
| `401 UNAUTHORIZED` | The email or the password is wrong, or a second-factor code is. An email nobody has and a wrong password are refused in the same words and take the same time, so a refusal says nothing about which addresses the venue knows. |
| `401 SECOND_FACTOR_REQUIRED` | The password was right and this account asks for a code. Its own code, because an app must tell it from a wrong password: one asks for a code, the other asks for the password again. |
| `429 LOCKED_OUT` | Five failed attempts in a row lock the member for fifteen minutes, and the member is told by mail that it happened. The lock is on the member rather than on the address the attempts came from, which is what stops guessing spread over many addresses. Setting a new password lifts it. |

A second factor is TOTP (RFC 6238): six digits over thirty seconds, one step of drift either way for a clock that is
a little off, and a code is good once — the step it was accepted at is written down, so the same code inside its own
window does not work twice. A recovery code gets in when the authenticator is gone, is also good once, and is matched
however it was copied down: case, spaces and the dash between its groups are all ignored.

The venue writes to a member when something happens to their account that they would want to know about: a new
sign-in, a password changed, a second factor added or removed, a recovery code used, an account locked. The password
is kept only as an Argon2id hash, and every other secret here — the session token, the emailed codes, the recovery
codes — only as a SHA-256 of itself, as an API key's secret is (G-2).

### Reading the mail in a demo

A test environment has no mail provider, so the messages sit in the identity service's outbox and `GET /v4/mail`
answers with them (`?to=` for one address), newest first. It exists so a demo can show the codes and a test can
follow one the way a person follows it out of an inbox. **The identity service answers `404` unless it was started
with `--show-mail`**, because the messages hold the very codes they were sent to keep secret: a venue that can really
post to an address must never turn it on.

**Not in this cut**, and needed before anyone but a test environment relies on it: an account made at sign-up
reaching the treasury, without which a member cannot deposit at all (above); a real mail provider in place of the
outbox; checking a password against the breached-password corpora; and rate limits on the emailed codes themselves,
which today are limited only as anonymous requests are.

## Choosing an environment

An app is built knowing one address and asks the venue where it may go. `GET /v4/bootstrap` answers the venues it may
point itself at, and nothing else:

```json
{"environments": [{"id": "prod", "label": "Live", "url": "https://venue.example", "live": true}]}
```

Most callers are told about the live venue alone. A caller whose session belongs to one of the venue's own people is
also told which test environments exist, so a developer can point the app in their hand at the environment their
branch is running in. `live` says which venue's money is real; an app shows that where a person can see it, and a
branch is never dressed up as the live venue — an entry that forgets to say is taken as not live.

The list is a file the gateway is given (`--environments`), read afresh on every request, so environments coming and
going does not mean restarting anything. Without the flag the route is not offered and an app stays where it was
built pointing.

**This is discovery, not authorization.** What is answered decides what a client is *told about*; it cannot protect a
test environment, because a URL is not a secret and anything reachable will be reached. Each environment
authenticates its own callers out of its own records. Withholding the list buys two smaller things, both worth
having: internal addresses are not advertised, and nobody wanders onto a venue whose money is imaginary while
believing it is real.

**A credential never crosses.** A session belongs to the venue that issued it and to no other: the environments keep
separate identity services and separate databases, so a member of the live venue is nobody in a test one and signs in
there separately. That is the point the whole arrangement rests on, and the reason is in decision G-20.

Being one of the venue's own people is a member attribute, not a permission — permissions are what a credential may
do with an account, and this is a fact about a person that no account has anything to do with. **There is no route
that sets it.** An operator runs `venue-identity staff --email ADDRESS` against the records, so that nothing a member
sends can reach it: not a session, not a key, not a request that got as far as the gateway. It is asked of the
records on each bootstrap rather than kept with the credential, so granting or withdrawing it takes effect at once
rather than whenever a cached answer expires.

## Browsers

A page the venue serves itself calls the API from the same origin and needs nothing here. A page served from anywhere
else — a front end in development, or a web app on its own host — is subject to the browser's same-origin rule, and
the gateway answers it only from origins it has been given (`--allowed-origins`, `VENUE_ALLOWED_ORIGINS`,
comma-separated, none by default).

- A request that names an allowed `Origin` is answered with `Access-Control-Allow-Origin` echoing that origin,
  `Access-Control-Expose-Headers: Retry-After` so a client can read how long a flood limit lasts, and `Vary: Origin`
  so a cache never answers one origin with another's response. The origin is echoed rather than `*` because a client
  sends its token in a header and an allow-list of exact origins is easier to reason about than a wildcard.
- A request that names any other origin, or none, is answered exactly as it always was: the answer carries no such
  headers, and a browser refuses to let the page read it.
- `OPTIONS` from an allowed origin is the browser asking what it may send, and is answered 204 with
  `Access-Control-Allow-Methods: GET, POST, PATCH, DELETE, OPTIONS`,
  `Access-Control-Allow-Headers: authorization, content-type` and `Access-Control-Max-Age: 600`. From anywhere else it
  is a route that does not exist, 404.
- **The WebSockets are not origin-checked**, and do not need to be: a session carries no ambient credential. Nothing
  is authenticated by a cookie the browser would attach on its own; a session is only an account's once it has sent
  `login` with a token, which a page from another site does not have. The audit log records the origin a request
  came from either way (`headers`).

Credentials in the browser's sense (cookies) are not used, so `Access-Control-Allow-Credentials` is never sent.

## Not in this draft

The binary market-maker gateway (interface 2); keys scoped to markets, IP allow-lists and signed requests; real
payment rails and their holds, limits and clawbacks; minimum-sequence reads from the stores (interface 28); the
environment manifest (interface 22); real identity (interface 6); a sequenced RFQ state machine (blueprint §6:
requests, windows and acceptances in the journal); caps on how many responders receive a request, and minimum sizes
per class of parlay; order history beyond what a session resumes.
