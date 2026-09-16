# venue-sim

The mini venue's actors (blueprint §16), for laptops and test clusters only.

| Command | What it does | Talks to |
|---|---|---|
| `seed` | Sets the demo fee schedule (7% taker, 1.75% maker, of contracts × P × (1 − P), with a standing 0.5% maker rebate for the synthetic maker), defines the cent tick table, funds the catalogue's accounts and lists round 1 of every market. Every command has a fixed ID, so running it again changes nothing (spec E-7.1). | the core's ingress, as an operator |
| `maker` | Quotes several levels on both sides of every open market (three by default, each a tick further out and larger) around a seeded random-walk fair value, rescaled to sum to 100¢ across an event's outcomes, with a doubled spread once the event starts. Pulls its quotes when an event ends or is suspended. It follows its own executions on a private session with cancel-on-disconnect and a 10 s heartbeat, and books on a public one, so a maker that dies or hangs has its quotes pulled by the gateway; it logs in again, cancels anything an earlier session left (orders a dead gateway could not cancel), and reads `/v4/account` to start afresh. As a registered responder (`"responder": true` in the catalogue), answers each request for quote on a parlay: it prices the parlay as the product of its legs' mid prices, read from their books, adds `--margin` thousandths and 0.1¢, rests a GTD sell of the whole size until the request's window closes, and attaches it with `attachQuote`. It never quotes parlays otherwise. | the gateway's two WebSocket endpoints, `/v4/ws_private` with its dev token and `/v4/ws_public` for books (REST only for the market list, account snapshots and the books of a parlay's legs) |
| `taker` | Leaves parlays to their requesters. Sends immediate-or-cancel orders at the touch at random: buying YES, buying NO or selling YES, with prices as ticks or cents and buys often as stakes, and an occasional sweep three ticks through. | the gateway's REST API, with its dev token |
| `oracle` | The dummy oracle: lists a new round of each demo market on a calendar, starts it, sometimes suspends it, ends it, closes its markets and settles it through two dummy sources, a referee, the auto-approver and a dispute window, voiding one now and then. Serves a control API for the page and for people. See [The dummy oracle](#the-dummy-oracle). | the core's ingress and feed, as an operator |
| `web` | The web page: an order book, a market maker tab and a retail customer tab that trade over the gateway's private WebSocket (the book comes from its public one), an account tab, a parlays tab, and a settlement tab that shows the oracle's schedule and cases and drives it. The account tab shows either account's available and reserved cash and positions, deposits and withdraws through `/v4/transfers`, and lists its transfers as their states change. The parlays tab builds a parlay from open markets (one leg per event, yes or no, in lots of 10), requests quotes as the retail customer, shows each quote's price and remaining validity as it arrives, buys at a quote with an immediate-or-cancel order, and lists every parlay with its legs' states and its own: open, closed, settled as won, expired as lost, or voided. A parlay's book trades from the other tabs like any market's. It passes `/v4` through to the gateway and `/oracle` to the dummy oracle, so one address serves the page and both APIs. | the gateway for trading, transfers and requests for quote; the dummy oracle for settlement; the core's feed for the event list |

Parlays themselves are the parlay service's (`core/parlays`, `venue-parlays`), not an actor here: it lists them on
request, routes requests for quote, and settles them from their legs.

The bots and the trading tabs are ordinary clients of interface 1 (`interfaces/gateway/`): the core cannot tell them
from customers, which is the point of §16's rule that activity arrives through the real edge.

## The catalogue

`deploy/charts/venue-mini/files/markets.json` names the accounts, their dev tokens, which tab shows each, and the
markets. The engine never sees titles or outcome names, so IDs carry where they came from: event
`template × 1 000 000 + round`, instrument `event × 100 + index`, so a template has a million rounds and an event a
hundred markets. Parlays are numbered apart, from 2^52 up, by a hash of their legs; an account with `"responder":
true` receives requests for quote. Events below a million were listed under the first scheme (`template × 1000 + round`,
instrument `event × 10 + index`) and are still labelled. "List now" on the settlement tab lists the same market again
under the next round.

## The dummy oracle

`deploy/charts/venue-mini/files/calendar.json` gives each template a cadence and timings, in seconds, and chances,
in thousandths:

| Field | Meaning |
|---|---|
| `everySeconds` | a new round is listed this long after the previous one was |
| `leadSeconds` | the round's `DefineEvent.scheduledStart` is this long after listing; it starts then |
| `runSeconds` | in play this long after it actually started, then ended |
| `closeAfterSeconds` | its markets close this long after it ended |
| `suspendChance`, `suspendSeconds` | chance a round is suspended a third of the way through its run, and for how long |
| `voidChance` | chance the round is abandoned: every source reports it void |
| `errorChance` | chance each primary source, independently, misreads the result |
| `resolution.*` | `reportAfterSeconds` from closing to the sources' reports, `tiebreakSeconds` before the referee answers, `disputeSeconds` of dispute window, `escalationSeconds` before an unanswered escalation is voided |

Every choice is drawn from `seed` and the event ID alone, and every stage counts from what the journal recorded, so a
run is reproducible and a restarted oracle carries on where the venue is. Phases the calendar sets carry
`PhaseSource` `DUMMY_ORACLE`; those set by hand through the API carry `OPERATOR`.

**Two oracles and a referee.** Each round's result comes from two independent dummy sources, `alpha` and `beta`,
with a `referee` as the registered fallback (blueprint §11). When the markets have closed, `alpha` and `beta` report
with evidence. If they agree, that is the proposal. If not, the referee reports after the tiebreak and two of three
decide; if all three differ (possible with three outcomes), the case is escalated to an operator. The auto-approver
signs a proposal with two test-operator credentials once it has checked the outcome exists and two sources (or an
operator) back it. A signed proposal waits out the dispute window; a dispute escalates it, otherwise `ResolveEvent`
is sequenced with case ID `event × 100 + proposal`. An escalation nobody answers is voided, never settled on a guess.
A resolve or void from the page or the API is an operator's proposal and takes the same sign-off and window.

**Control API** (a subset of interface 23), served by `oracle` on `--listen` and by `web` under `/oracle`. JSON;
times are milliseconds since the epoch on the sequencer's clock.

| Request | Does |
|---|---|
| `GET /oracle/schedule` | each template's latest and next round; every open or upcoming event and the six most recently settled, with timings, markets, resolution and case (reports, proposal, signatures, dispute window, history) |
| `POST /oracle/templates/{template}/list` | lists the template's next round now |
| `POST /oracle/events/{event}/start` · `suspend` · `resume` · `end` · `close` | that change, now; a round suspended by hand stays suspended until resumed by hand |
| `POST /oracle/events/{event}/propose` `{"winningState": 1, "evidence": "..."}` | an operator's proposal |
| `POST /oracle/events/{event}/void` `{"evidence": "..."}` | a proposal to void |
| `POST /oracle/events/{event}/dispute` `{"reason": "..."}` | escalates the proposal in its dispute window |
| `POST /oracle/events/{event}/postpone` `{"seconds": 120}` | holds a round that has not started |

```sh
curl -s localhost:8080/oracle/schedule
curl -s -XPOST localhost:8080/oracle/events/2000007/propose -d '{"winningState": 1, "evidence": "replay"}'
```

**Followers.** Resolutions and voids are public: `GET /v4/markets` gives each event its `resolution` (kind, winning
state, outcome), and the gateway's public `markets` topic sends it when the event resolves, after the markets' own
state changes. A trading client asks `GET /v4/markets?resolved=false` to leave the settled rounds out.

**Capacity.** The engine never frees a listed event or market, and listing past its capacity profile fail-stops the
core. The oracle takes the node's `--capacity` and stops listing while another round would pass 95% of the profile's
events, markets or price levels, and the schedule says so. With `small` and the chart's calendar that is about ten
hours; a fresh journal starts again. The parlay service (`venue-parlays --capacity`) applies the same limit to
parlays, counting the oracle's rounds, so neither takes the core over its ceiling. The oracle leaves parlays' events
alone: they are not on its schedule, and its control API refuses to act on them.

## Running it without a cluster

```sh
cargo build --release -p venue-node -p venue-gateway -p venue-parlays -p venue-sim -p venue-treasury
CAT=../deploy/charts/venue-mini/files/markets.json
target/release/venue-node serve --data /tmp/venue --tick-ms 100 &
target/release/venue-sim seed --ingress 127.0.0.1:7101 --catalogue $CAT
target/release/venue-treasury --ingress 127.0.0.1:7101 --feed 127.0.0.1:7102 --catalogue $CAT --ledger /tmp/venue/treasury.jsonl &
target/release/venue-parlays --ingress 127.0.0.1:7101 --feed 127.0.0.1:7102 --catalogue $CAT &
target/release/venue-gateway --ingress 127.0.0.1:7101 --feed 127.0.0.1:7102 --catalogue $CAT --treasury 127.0.0.1:8083 \
  --parlays 127.0.0.1:7103 &
target/release/venue-sim oracle --ingress 127.0.0.1:7101 --feed 127.0.0.1:7102 --catalogue $CAT \
  --calendar ../deploy/charts/venue-mini/files/calendar.json &
target/release/venue-sim maker --gateway 127.0.0.1:8081 &
target/release/venue-sim taker --gateway 127.0.0.1:8081 &
target/release/venue-sim web --ingress 127.0.0.1:7101 --feed 127.0.0.1:7102 --gateway 127.0.0.1:8081 --catalogue $CAT
```

Then open http://127.0.0.1:8080.

## Money

Accounts start with the seed's grants. After that, money arrives only as deposits and leaves only as withdrawals,
through the gateway to the dummy treasury (`core/treasury`) and its fake bank, which grants and reclaims buying power
in the core as sequenced commands. The page has no button that grants money straight into the core: blueprint §16
has demo balances come from a fake provider, so that a demo exercises the path real money takes.

## Known shortcuts

- **Dev tokens.** Tokens are fixed per account in the catalogue, and the page is handed them. Test environments only.
- **No resolution console.** Proposals, sign-offs and disputes live inside the oracle process rather than behind
  interface 24, and are lost if it restarts: a case still open is opened again from the sources, which report the
  same outcomes. Postponements and suspensions by hand are lost the same way. The auto-approver's two credentials are
  names, not credentials from an issuer.
- **One oracle.** Run exactly one per venue: two would both act on every round.
- **One flow each.** The maker trades over a WebSocket session, as market makers do; the taker over REST, as simple
  programmatic clients do. Between them and the page's tabs, both of interface 1's order flows carry load.
