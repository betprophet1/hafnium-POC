# Deploy

The mini venue of blueprint §16: one Helm chart, run on a laptop under k3d and, later, in ephemeral namespaces
through ArgoCD. There is no compose file; the laptop runs the same chart.

| Path | What it is |
|---|---|
| `images/venue-node.Dockerfile` | The image `venue`: `venue-node`, `venue-gateway`, `venue-parlays`, `venue-sim`, `venue-treasury` and `venue-identity`, built from the repository root |
| `charts/venue-mini/` | The chart: the core, the gateway, the identity service, the dummy treasury, the parlay service, a seed job for the demo markets, the dummy oracle, synthetic maker and taker, the web page, network policies and a `helm test` smoke test |
| `charts/venue-mini/files/markets.json` | The demo catalogue: accounts with their dev tokens, tabs and whether they respond to requests for quote, markets and their outcome names |
| `charts/venue-mini/files/calendar.json` | The dummy oracle's calendar: how often each market lists a new round, its timings, and the resolution timers (`core/sim/README.md`) |
| `charts/venue-mini/templates/gateway.yaml` | Also the gateway's audit log volume (`/var/lib/gateway-audit`, `gateway.audit` in values): every request and answer, in the format of `interfaces/audit/`. Nothing ships it to retained storage yet. With `gateway.split.enabled`, the public and private halves as StatefulSets of their own (several gateways, below) |
| `laptop.sh` | `up`, `web`, `restart` and `down` for a local k3d cluster |

## On a laptop

Needs Docker, [k3d](https://k3d.io) 5.9, Helm 4.3 and kubectl 1.37.

```sh
deploy/laptop.sh up     # build, install, seed, smoke test
deploy/laptop.sh web    # then open http://127.0.0.1:8080
```

The page has an order book and five tabs. The market maker and retail customer tabs are private WebSocket sessions on
the gateway (`/v4/ws_private`), each logged in with its account's dev token, and the book comes from a public one
(`/v4/ws_public`); the account tab deposits and withdraws for either account through the dummy treasury and shows its
cash, positions and transfers, and makes, lists and revokes the account's API keys; the parlays tab builds a parlay from open markets and requests quotes for it as the
retail customer, shows the quotes as they arrive and lapse, takes one, and lists every parlay with its legs and how it
settled. Markets run on their own: the dummy oracle (`oracle.enabled`, on by default) lists a new round of each market
every few minutes (the S&P 500 market once a day, trading for 22 hours, so something stays put while you test) and starts, ends, closes and settles it through two dummy sources, an auto-approver and a 30 s
dispute window, voiding one now and then. The settlement tab shows its schedule, countdowns and resolution cases, and
acts through it: start, suspend, end, close, postpone, propose an outcome or a void, dispute, and list a round now.
The synthetic maker quotes over a WebSocket session with cancel-on-disconnect (delete its pod and its quotes leave the
book until the new pod logs in) and, as an approved responder, subscribes to requests for quote and answers them, and the
synthetic taker trades over REST, leaving parlays alone. The parlay service (`venue-parlays`) lists a parlay on its
first request, routes requests to approved responders that subscribe and quotes back for the gateway, and expires,
resolves or voids each parlay as its legs resolve (interfaces/gateway/README.md, parlays). Responders are approved
and revoked through its operator API, port 7104 of the `parlays` service (`core/parlays/README.md`), which a
network policy keeps from every pod: reach it with `kubectl port-forward svc/venue-parlays 7104` (release `venue`).
Only the gateway reaches the parlay service's port 7103, and the identity service's 8084.
The same port also serves the gateway itself under `/v4`:

```sh
curl -s localhost:8080/v4/markets?resolved=false
curl -s localhost:8080/oracle/schedule
curl -s -H 'Authorization: Bearer dev-retail-customer' localhost:8080/v4/account
curl -s -H 'Authorization: Bearer dev-retail-customer' localhost:8080/v4/transfers \
  -d '{"clientTransferId": "d1", "kind": "deposit", "amountCents": 10000, "method": "dummy_bank"}'
curl -s -H 'Authorization: Bearer dev-retail-customer' localhost:8080/v4/keys \
  -d '{"label": "my bot", "permissions": ["read", "trade"]}'   # then use its credential as the bearer token
```

`up` is safe to repeat. After rebuilding, it replaces every pod, the core included, which replays its journal before
it is ready. `down` deletes the cluster and the journal with it.

## Several gateways

Interface 1's halves can be served by separate gateway processes, several of each (interfaces/gateway/README.md,
public and private). Nothing a client relies on is held by one gateway alone: every gateway follows the core's feed,
and transfers, API keys and requests for quote live in the services they all forward to, so a client may use any
gateway for anything its half serves, and resume on another with `resumeFrom`. What is per gateway process: flood and
logon limits (decision G-13), and its audit instance, which no two live gateways may share.

**On one machine**, from built release binaries (`cargo build --release` in `core/`), with `CAT` the catalogue and
`BIN` `core/target/release`; clients pick a gateway, nothing balances them:

```sh
mkdir -p run
$BIN/venue-node serve --listen 127.0.0.1:18200 --feed 127.0.0.1:18201 --data run/node &
$BIN/venue-sim seed --ingress 127.0.0.1:18200 --catalogue $CAT
$BIN/venue-identity --catalogue $CAT --store run/identity.jsonl --listen 127.0.0.1:18202 &
$BIN/venue-treasury --ingress 127.0.0.1:18200 --feed 127.0.0.1:18201 --catalogue $CAT --ledger run/ledger.jsonl \
  --listen 127.0.0.1:18203 &
$BIN/venue-parlays --ingress 127.0.0.1:18200 --feed 127.0.0.1:18201 --catalogue $CAT --listen 127.0.0.1:18204 \
  --operator 127.0.0.1:18205 --responders run/responders.json &
# Two private and two public gateways, each with its own port and audit instance.
for g in private-a:private:18210 private-b:private:18211 public-c:public:18212 public-d:public:18213; do
  IFS=: read -r instance serve port <<<"$g"
  $BIN/venue-gateway --serve $serve --listen 127.0.0.1:$port --feed 127.0.0.1:18201 --ingress 127.0.0.1:18200 \
    --catalogue $CAT --identity 127.0.0.1:18202 --treasury 127.0.0.1:18203 --parlays 127.0.0.1:18204 \
    --audit-dir run/gateway-audit --audit-instance $instance &
done
# The maker trades on one private gateway and watches books on a public one; the taker uses the other pair.
$BIN/venue-sim maker --gateway 127.0.0.1:18210 --public-gateway 127.0.0.1:18212 &
$BIN/venue-sim taker --gateway 127.0.0.1:18211 --public-gateway 127.0.0.1:18213 &
# The page passes /v4/ws_public and /v4/markets to the public gateway, the rest of /v4 to the private one.
$BIN/venue-sim web --ingress 127.0.0.1:18200 --feed 127.0.0.1:18201 --catalogue $CAT --listen 127.0.0.1:18230 \
  --gateway 127.0.0.1:18211 --public-gateway 127.0.0.1:18213 --treasury 127.0.0.1:18203 &
```

Stop them by the PIDs they were started with. `interfaces/tools/audit_verify.py run/gateway-audit/*.jsonl` checks
every gateway's log; each file holds only its own gateway's traffic.

**In the chart**, `gateway.split.enabled: true` replaces the one gateway with two StatefulSets,
`<release>-gateway-public` and `<release>-gateway-private`, of `gateway.split.public.replicas` and
`gateway.split.private.replicas` pods, each half behind a Service of the same name, and each pod with an audit volume
of its own and the audit instance of its pod name. The web page and the bots are given both Services. A Service
balances connections, not requests: a WebSocket stays on the pod it reached, a REST call may land on any, and neither
needs session affinity, since nothing a client depends on lives in one pod; a client whose pod goes reconnects to
another and resumes. Two things are not in the chart. An Ingress that exposes the venue under one host name routes by
path: `/v4/ws_public` and `/v4/markets` with everything under it to the public Service, and `/v4/ws_private`,
`/v4/account`, `/v4/orders`, `/v4/transfers`, `/v4/rfqs` and `/v4/keys` with everything under them to the private
one, with WebSocket idle timeouts above the clients' heartbeat interval. And `deploy/laptop.sh up` restarts
Deployments after a rebuild, not these StatefulSets: `kubectl rollout restart statefulset <release>-gateway-public
<release>-gateway-private` does.

## What the chart does and does not do

- **One way in.** Customers, bots and the page's trading tabs reach the core only through the gateway; a network
  policy lets only the gateway, the operator tools and the smoke test reach the core's ingress and feed.
- **One writer.** The core is `replicas: 1` with `updateStrategy: OnDelete`: a sequencer group never rolls on its own.
- **No liveness probe on the core.** A fail-stop (spec §18) exits the process; the restart replays to the same stop
  and shows as `CrashLoopBackOff`, which is the signal to look at the journal (`venue-node dump`).
- **Money through the treasury.** With `treasury.enabled` (the default) the gateway passes `/v4/transfers` to the dummy
  treasury, which keeps its ledger on a volume of its own and moves buying power in the core only as sequenced grants
  and reclaims; only the gateway may reach it, and the web page for its reconciliation summary. Its fake bank's
  delays and failure rate are `treasury.bank` values. It reconciles its ledger against the fake bank's statement and
  the journal every `treasury.reconciliation.everyMs`; `treasury.faults: demo` loses a few percent of the bank's
  notices and of sleeve-adjustment answers on purpose, so the account tab's reconciliation section has breaks to show.
  The seed's grants are still how accounts start.
- **API keys through the identity service.** With `identity.enabled` (the default) the gateway verifies API keys with
  the identity service and passes `/v4/keys` to it; the key store is on a volume of its own and only the gateway may
  reach it. Dev tokens keep working beside keys. Test-environment identity only (`core/identity/README.md`).
- **Seeding is idempotent.** The seed job runs after every install and upgrade; its commands have fixed IDs.
- **Not production.** Production runs the core on dedicated hosts behind Aeron Cluster (blueprint §5, §9). Tokens are
  fixed dev tokens, and the page and operator actions are unauthenticated; never expose them outside the cluster.
- **One oracle, bounded by the core's capacity.** The oracle is `replicas: 1` with `Recreate`, so two never act on a
  round. The engine releases a settled round half an hour after it resolves (the seed's retention), so the calendar
  runs indefinitely on the `small` profile.
- **Requests for quote are not journaled.** Open requests and responders' subscriptions live in the parlay service's
  memory; replacing its pod forgets them (the gateway subscribes its sessions again), while parlays and quotes
  (listings and orders) stay in the journal, and the approved responders on the service's own volume. It lists at most
  `parlays.maxParlays` parlays at once, because every book reserves its levels until its settled event is released,
  and none that would take the core past 95% of its capacity profile, which the oracle's rounds share.
- **Not yet in the chart:** the resolution console behind the dummy oracle (its cases live in the oracle process), the
  scenario player and the environment manifest of §16.
