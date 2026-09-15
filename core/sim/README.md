# venue-sim

The mini venue's actors (blueprint §16), for laptops and test clusters only. Every actor is an ordinary client of
the node: inputs go through its ingress, and everything it knows comes from its feed.

| Command | What it does |
|---|---|
| `seed` | Defines the cent tick table, funds the catalogue's accounts and lists round 1 of every market. Every command has a fixed ID, so running it again changes nothing (spec E-7.1). |
| `maker` | Quotes both sides of every open market around a seeded random-walk fair value, rescaled to sum to 100¢ across an event's outcomes, with a doubled spread once the event starts. Pulls its quotes when an event ends or is suspended. |
| `taker` | Sends immediate-or-cancel orders at the touch at random, retail-sized, with the occasional sweep three ticks through. |
| `web` | The web page: an order book, a market maker tab, a retail customer tab and a settlement tab driving the dummy oracle. |

## The catalogue

`deploy/charts/venue-mini/files/markets.json` names the accounts, which tab shows each, and the markets. The engine
never sees titles or outcome names, so IDs carry where they came from: event `template × 1000 + round`, instrument
`event × 10 + index`. "List a new round" on the settlement tab lists the same market again under the next round.

## Running it without a cluster

```sh
cargo build --release -p venue-node -p venue-sim
target/release/venue-node serve --data /tmp/venue --tick-ms 100 &
CAT=../deploy/charts/venue-mini/files/markets.json
target/release/venue-sim seed --ingress 127.0.0.1:7101 --catalogue $CAT
target/release/venue-sim maker --ingress 127.0.0.1:7101 --feed 127.0.0.1:7102 &
target/release/venue-sim taker --ingress 127.0.0.1:7101 --feed 127.0.0.1:7102 &
target/release/venue-sim web --ingress 127.0.0.1:7101 --feed 127.0.0.1:7102 --catalogue $CAT
```

Then open http://127.0.0.1:8080.

## Known shortcuts

- **No gateway.** The actors talk to the core's ingress, so they skip the odds-to-tick and stake handling of §6
  that the gateway will do.
- **No resolution console.** The dummy oracle sequences `SetEventPhase`, `SetMarketState` and `ResolveEvent`
  directly; §16's proposal, sign-off and auto-approver path does not exist yet.
- **No authentication.** The web page acts as any account it is asked to.
