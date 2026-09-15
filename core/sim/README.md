# venue-sim

The mini venue's actors (blueprint §16), for laptops and test clusters only.

| Command | What it does | Talks to |
|---|---|---|
| `seed` | Defines the cent tick table, funds the catalogue's accounts and lists round 1 of every market. Every command has a fixed ID, so running it again changes nothing (spec E-7.1). | the core's ingress, as an operator |
| `maker` | Quotes several levels on both sides of every open market (three by default, each a tick further out and larger) around a seeded random-walk fair value, rescaled to sum to 100¢ across an event's outcomes, with a doubled spread once the event starts. Pulls its quotes when an event ends or is suspended. | the gateway's REST API, with its dev token |
| `taker` | Sends immediate-or-cancel orders at the touch at random: buying YES, buying NO or selling YES, with prices as ticks or cents and buys often as stakes, and an occasional sweep three ticks through. | the gateway's REST API, with its dev token |
| `web` | The web page: an order book, a market maker tab and a retail customer tab that trade over the gateway's WebSocket, and a settlement tab that drives the dummy oracle. It passes `/v4` through to the gateway, so one address serves the page and the API. | the gateway for trading; the core's ingress and feed for settlement |

The bots and the trading tabs are ordinary clients of interface 1 (`interfaces/gateway/`): the core cannot tell them
from customers, which is the point of §16's rule that activity arrives through the real edge.

## The catalogue

`deploy/charts/venue-mini/files/markets.json` names the accounts, their dev tokens, which tab shows each, and the
markets. The engine never sees titles or outcome names, so IDs carry where they came from: event
`template × 1000 + round`, instrument `event × 10 + index`. "List a new round" on the settlement tab lists the same
market again under the next round.

## Running it without a cluster

```sh
cargo build --release -p venue-node -p venue-gateway -p venue-sim
CAT=../deploy/charts/venue-mini/files/markets.json
target/release/venue-node serve --data /tmp/venue --tick-ms 100 &
target/release/venue-sim seed --ingress 127.0.0.1:7101 --catalogue $CAT
target/release/venue-gateway --ingress 127.0.0.1:7101 --feed 127.0.0.1:7102 --catalogue $CAT &
target/release/venue-sim maker --gateway 127.0.0.1:8081 &
target/release/venue-sim taker --gateway 127.0.0.1:8081 &
target/release/venue-sim web --ingress 127.0.0.1:7101 --feed 127.0.0.1:7102 --gateway 127.0.0.1:8081 --catalogue $CAT
```

Then open http://127.0.0.1:8080.

## Known shortcuts

- **Dev tokens.** Tokens are fixed per account in the catalogue, and the page is handed them. Test environments only.
- **No resolution console.** The dummy oracle sequences `SetEventPhase`, `SetMarketState` and `ResolveEvent`
  directly; §16's proposal, sign-off and auto-approver path does not exist yet.
- **The bots use REST.** They exercise orders, stakes and price forms but not WebSocket sessions; the page's tabs do.
