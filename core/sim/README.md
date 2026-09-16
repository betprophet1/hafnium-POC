# venue-sim

The mini venue's actors (blueprint §16), for laptops and test clusters only.

| Command | What it does | Talks to |
|---|---|---|
| `seed` | Sets the demo fee schedule (7% taker, 1.75% maker, of contracts × P × (1 − P), with a standing 0.5% maker rebate for the synthetic maker), defines the cent tick table, funds the catalogue's accounts and lists round 1 of every market. Every command has a fixed ID, so running it again changes nothing (spec E-7.1). | the core's ingress, as an operator |
| `maker` | Quotes several levels on both sides of every open market (three by default, each a tick further out and larger) around a seeded random-walk fair value, rescaled to sum to 100¢ across an event's outcomes, with a doubled spread once the event starts. Pulls its quotes when an event ends or is suspended. It follows its own executions on a private session with cancel-on-disconnect and a 10 s heartbeat, and books on a public one, so a maker that dies or hangs has its quotes pulled by the gateway; it logs in again, cancels anything an earlier session left (orders a dead gateway could not cancel), and reads `/v4/account` to start afresh. | the gateway's two WebSocket endpoints, `/v4/ws_private` with its dev token and `/v4/ws_public` for books (REST only for the market list and account snapshots) |
| `taker` | Sends immediate-or-cancel orders at the touch at random: buying YES, buying NO or selling YES, with prices as ticks or cents and buys often as stakes, and an occasional sweep three ticks through. | the gateway's REST API, with its dev token |
| `web` | The web page: an order book, a market maker tab and a retail customer tab that trade over the gateway's private WebSocket (the book comes from its public one), an account tab, and a settlement tab that drives the dummy oracle. The account tab shows either account's available and reserved cash and positions, deposits and withdraws through `/v4/transfers`, and lists its transfers as their states change. It passes `/v4` through to the gateway, so one address serves the page and the API. | the gateway for trading and transfers; the core's ingress and feed for settlement |

The bots and the trading tabs are ordinary clients of interface 1 (`interfaces/gateway/`): the core cannot tell them
from customers, which is the point of §16's rule that activity arrives through the real edge.

## The catalogue

`deploy/charts/venue-mini/files/markets.json` names the accounts, their dev tokens, which tab shows each, and the
markets. The engine never sees titles or outcome names, so IDs carry where they came from: event
`template × 1000 + round`, instrument `event × 10 + index`. "List a new round" on the settlement tab lists the same
market again under the next round.

## Running it without a cluster

```sh
cargo build --release -p venue-node -p venue-gateway -p venue-sim -p venue-treasury
CAT=../deploy/charts/venue-mini/files/markets.json
target/release/venue-node serve --data /tmp/venue --tick-ms 100 &
target/release/venue-sim seed --ingress 127.0.0.1:7101 --catalogue $CAT
target/release/venue-treasury --ingress 127.0.0.1:7101 --feed 127.0.0.1:7102 --catalogue $CAT --ledger /tmp/venue/treasury.jsonl &
target/release/venue-gateway --ingress 127.0.0.1:7101 --feed 127.0.0.1:7102 --catalogue $CAT --treasury 127.0.0.1:8083 &
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
- **No resolution console.** The dummy oracle sequences `SetEventPhase`, `SetMarketState` and `ResolveEvent`
  directly; §16's proposal, sign-off and auto-approver path does not exist yet.
- **One flow each.** The maker trades over a WebSocket session, as market makers do; the taker over REST, as simple
  programmatic clients do. Between them and the page's tabs, both of interface 1's order flows carry load.
