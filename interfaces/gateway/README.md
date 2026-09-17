# Interface 1: the REST and WebSocket gateway

Draft v1. `openapi.yaml` is the REST half and `asyncapi.yaml` the WebSocket half; this file holds the semantics
both share. The gateway is the edge of blueprint §5 and §13: every app, bot and tool reaches a sequencer group
through it, and it is the only client-facing component that writes to one (interface 7).

## What the gateway does, and what it never does

It authenticates the session, validates every message completely, converts prices and stakes into the journal's
tick ordinals and contract counts, enforces flood limits, turns a disconnect or missed heartbeat into a sequenced
mass cancel, routes to the sequencer group, and answers once the input is committed. It fans out each account's
executions and conflated public market data. Deposits and withdrawals it passes to the treasury once it has
authenticated the account, and it fans out the treasury's updates to the account's private sessions.

It never decides anything the engine decides. Client order ID duplicates, lot steps, market state, rule limits and
collateral are sequenced checks (spec §9), so their rejections replay. It never decides anything about money either:
the treasury owns every transfer and its ledger (blueprint §11). The gateway holds no state a failover could lose
except flood-limit counters, which by design never reach the journal (blueprint §12).

## Public and private

The gateway has two halves, told apart by path alone: `/v4/ws_private`, `/v4/account`, and `/v4/orders`,
`/v4/transfers` and `/v4/rfqs` with everything under them are private; `/v4/ws_public` and `/v4/markets` with
everything under it are public; `/healthz` belongs to both.

| | Public | Private |
|---|---|---|
| WebSocket | `/v4/ws_public` | `/v4/ws_private` |
| REST | `GET /v4/markets`, `GET /v4/markets/{instrumentId}/book` | `GET /v4/account`; `/v4/orders`, `/v4/transfers`, `/v4/rfqs` and everything under them |
| Login | never | required: a token on every REST call, `login` first on a session |
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
| REST | HTTP 400, 401, 404, 429 or 503 with an `Error` | HTTP 200, `OrderResult.status: rejected` |
| WebSocket | `error` message | `reply` with `status: rejected` |
| Journaled | Never | Always, with its reason |
| Examples | malformed body, bad token, price not on the ladder, stake off its step, flood limit | insufficient sleeve, market not open, duplicate client order ID |

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
except heartbeats and the errors sent back, but not market data. Tokens are masked: the `Authorization` value becomes
`Bearer ***`, and the `token` of a `login` becomes `***`.

The log is written under `--audit-dir` (`VENUE_AUDIT_DIR`, default `./gateway-audit`), one file per UTC day named for
`--audit-instance` (`VENUE_AUDIT_INSTANCE`, default the host name), synced unless `--audit-fsync false`
(`VENUE_AUDIT_FSYNC`). The event loops only queue records; a writer thread writes them. A full queue makes a loop
wait rather than drop a record, and a queue full for 10 seconds, a failed write or an audit directory that cannot be
written stops the gateway (exit status 74) rather than let it serve unrecorded. `venue-gateway verify-audit FILE...`
checks a file's chain, as `interfaces/tools/audit_verify.py` does.

## Deposits and withdrawals

`POST /v4/transfers` moves money into the account or out of it, `GET /v4/transfers` lists the account's transfers
and `DELETE /v4/transfers/{transferId}` cancels one the bank has not decided yet. The gateway authenticates the
account and passes the request to the treasury, which validates it, decides it and answers in the same `Error` form;
the gateway never holds a transfer. A treasury that does not answer is a 503 whose outcome is unknown, as for an
order.

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

## Permissions

Moving money is a permission of its own, apart from trading, so that a credential for a trading program can be
issued without it. The contract names it `transfers` in the security requirement of every `/v4/transfers` operation.
A dev token holds every permission. When API keys arrive, each carries a flag for it: a key without the flag is
refused with 403 `FORBIDDEN` on `/v4/transfers`, and its sessions are not sent `transfer` messages. Order entry and
market data never need it.

## Authentication in test environments

Each account in an environment's catalogue has a fixed dev token. REST sends `Authorization: Bearer <token>`;
a WebSocket session sends it in `login`. Tokens exist only in test environments and act only as their account.
Real identity is interface 6 and the environment manifest's issuer (interface 22).

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

## Sessions: cancel-on-disconnect and heartbeats

These are private sessions, on `/v4/ws_private`. `cancelOnDisconnect` in `login` makes the end of the session, a
clean close or a dropped connection, a `MassCancel` of the session's resting orders with cause `SESSION_DISCONNECT`.
`heartbeatSeconds` makes silence that long a `MassCancel` with cause `HEARTBEAT_MISSED`, then a close. Both are keyed to the client session, never to the
gateway's connection to the sequencer group, so a failover does not empty the books (blueprint §5). REST requests
have no session and never cancel on disconnect.

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
refused this way reaches the journal. The limits are set comfortably above legitimate use; what a customer should
actually meet is a rule limit, which is sequenced and explainable (spec E-9.4).

## Market data

| Topic | Contents | Delivery |
|---|---|---|
| `markets` | market state changes and event phases | every change |
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

**Requesting.** `requestQuote` on `/v4/ws_private`, or `POST /v4/rfqs`, with `legs` and `quantity` (a multiple of the
lot step). The answer names the request (`requestId`), the parlay's `instrumentId` and `eventId`, the canonical legs,
and `expiresAt`: the end of the request's window, during which responders may quote. Anything wrong with the legs is
refused at the edge with `BAD_REQUEST` and a reason, and nothing is listed.

**Responding.** Accounts registered as responders in the environment's catalogue receive each request as an `rfq`
message on their private sessions while they are logged in. The requester is never named. A quote is a real resting
order: the responder places a sell of YES on the parlay's book, usually GTD so that it lapses (its collateral is
reserved like any order's), and then attaches it to the request with `attachQuote` or
`POST /v4/rfqs/{requestId}/quotes`, naming the order. Only a responder's own resting sell on the request's parlay,
within the request's window, can be attached. The order is on the public book from the moment it rests, so anyone
may take it, not only the requester.

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
quotes, so a requester and a responder need not share one. A restarted parlay service forgets open requests; the
quotes stay on the book as orders.

## Not in this draft

The binary market-maker gateway (interface 2) and the FIX adapter; API keys and their permissions; real payment rails
and their holds, limits and clawbacks; minimum-sequence reads from the stores (interface 28); the environment manifest
(interface 22); real identity (interface 6); a sequenced RFQ state machine (blueprint §6: requests, windows and
acceptances in the journal) and responder caps and minimum sizes per class; order history beyond what a session
resumes.
