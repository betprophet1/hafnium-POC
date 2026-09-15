# Deploy

The mini venue of blueprint §16: one Helm chart, run on a laptop under k3d and, later, in ephemeral namespaces
through ArgoCD. There is no compose file; the laptop runs the same chart.

| Path | What it is |
|---|---|
| `images/venue-node.Dockerfile` | The image `venue`: `venue-node`, `venue-gateway` and `venue-sim`, built from the repository root |
| `charts/venue-mini/` | The chart: the core, the gateway, a seed job for the demo markets, synthetic maker and taker, the web page, a network policy and a `helm test` smoke test |
| `charts/venue-mini/files/markets.json` | The demo catalogue: accounts with their dev tokens and tabs, markets and their outcome names |
| `laptop.sh` | `up`, `web`, `restart` and `down` for a local k3d cluster |

## On a laptop

Needs Docker, [k3d](https://k3d.io) 5.9, Helm 4.3 and kubectl 1.37.

```sh
deploy/laptop.sh up     # build, install, seed, smoke test
deploy/laptop.sh web    # then open http://127.0.0.1:8080
```

The page has an order book and three tabs. The market maker and retail customer tabs are WebSocket sessions on the
gateway, each logged in with its account's dev token; settlement drives the dummy oracle (start, suspend, end, close,
resolve, void, and list a new round). The synthetic maker and taker trade through the gateway's REST API. The same
port also serves the gateway itself under `/v4`:

```sh
curl -s localhost:8080/v4/markets
curl -s -H 'Authorization: Bearer dev-retail-customer' localhost:8080/v4/account
```

`up` is safe to repeat. After rebuilding, it replaces every pod, the core included, which replays its journal before
it is ready. `down` deletes the cluster and the journal with it.

## What the chart does and does not do

- **One way in.** Customers, bots and the page's trading tabs reach the core only through the gateway; a network
  policy lets only the gateway, the operator tools and the smoke test reach the core's ingress and feed.
- **One writer.** The core is `replicas: 1` with `updateStrategy: OnDelete`: a sequencer group never rolls on its own.
- **No liveness probe on the core.** A fail-stop (spec §18) exits the process; the restart replays to the same stop
  and shows as `CrashLoopBackOff`, which is the signal to look at the journal (`venue-node dump`).
- **Seeding is idempotent.** The seed job runs after every install and upgrade; its commands have fixed IDs.
- **Not production.** Production runs the core on dedicated hosts behind Aeron Cluster (blueprint §5, §9). Tokens are
  fixed dev tokens, and the page and operator actions are unauthenticated; never expose them outside the cluster.
- **Not yet in the chart:** the resolution console behind the dummy oracle, RFQ responders, the scenario player and
  the environment manifest of §16.
