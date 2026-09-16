# Interface 1: the REST and WebSocket gateway

Draft v1. `openapi.yaml` is the REST half and `asyncapi.yaml` the WebSocket half; this file holds the semantics
both share. The gateway is the edge of blueprint §5 and §13: every app, bot and tool reaches a sequencer group
through it, and it is the only client-facing component that writes to one (interface 7).

## What the gateway does, and what it never does

It authenticates the session, validates every message completely, converts prices and stakes into the journal's
tick ordinals and contract counts, enforces flood limits, turns a disconnect or missed heartbeat into a sequenced
mass cancel, routes to the sequencer group, and answers once the input is committed. It fans out each account's
executions and conflated public market data.

It never decides anything the engine decides. Client order ID duplicates, lot steps, market state, rule limits and
collateral are sequenced checks (spec §9), so their rejections replay. The gateway holds no state a failover could
lose except flood-limit counters, which by design never reach the journal (blueprint §12).

## Public and private

The gateway has two halves, told apart by path alone:

| | Public | Private |
|---|---|---|
| WebSocket | `/v4/ws_public` | `/v4/ws_private` |
| REST | `GET /v4/markets`, `GET /v4/markets/{instrumentId}/book` | `GET /v4/account`, `/v4/orders` and everything under it |
| Login | never | required: a token on every REST call, `login` first on a session |
| Carries | markets, books and trades, the same for everyone | one account's orders, executions, account updates, payouts and releases |
| Reaches the sequencer group | never | every order, amendment and cancel |

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

A 503 means the gateway could not get an answer from the sequencer group; the order may or may not exist. Resend
with the same `clientOrderId`: a committed original comes back as `status: duplicate` with its order ID.

A `clientOrderId` is remembered for **two hours** per account, across every market (spec E-9.2, D-12). Within that
window it cannot be reused, even by an order the engine rejected; an order the gateway refused never reached the
engine and does not use up its ID.

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

## Not in this draft

The binary market-maker gateway (interface 2) and the FIX adapter; minimum-sequence reads from the stores
(interface 28); the environment manifest (interface 22); real identity (interface 6); RFQ; order history beyond what
a session resumes.
