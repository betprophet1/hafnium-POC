# Deploy

The mini venue of blueprint §16: one Helm chart, run on a laptop under k3d and, later, in ephemeral namespaces
through ArgoCD. There is no compose file; the laptop runs the same chart.

| Path | What it is |
|---|---|
| `images/venue-node.Dockerfile` | The image `venue`: `venue-node`, `venue-gateway`, `venue-parlays`, `venue-sim`, `venue-treasury` and `venue-identity`, built from the repository root |
| `images/venue-fix.Dockerfile` | The image `venue-fix`: the FIX 4.4 adapter (`adapters/fix`), built with Gradle from the repository root |
| `charts/venue-mini/` | The chart: the core, the gateway, the identity service, the dummy treasury, the parlay service, PostgreSQL for their records and a job that migrates them, a seed job for the demo markets, the dummy oracle, synthetic maker and taker, the web page, network policies and a `helm test` smoke test |
| `charts/venue-mini/templates/postgres.yaml`, `migrate.yaml` | PostgreSQL (in the cluster, or a URL per service outside it) and each service's schema migrations (Databases, below) |
| `charts/venue-mini/files/markets.json` | The demo catalogue: accounts with their dev tokens, tabs and whether they respond to requests for quote, markets and their outcome names |
| `charts/venue-mini/files/calendar.json` | The dummy oracle's calendar: how often each market lists a new round, its timings, and the resolution timers (`core/sim/README.md`) |
| `charts/venue-mini/templates/gateway.yaml` | Also the gateway's audit log volume (`/var/lib/gateway-audit`, `gateway.audit` in values): every request and answer, in the format of `interfaces/audit/`. Nothing ships it to retained storage yet. With `gateway.split.enabled`, the public and private halves as StatefulSets of their own (several gateways, below) |
| `charts/venue-mini/templates/ingress.yaml` | With `ingress.enabled`, the page and the gateway under one host name, routed by path to the halves (below) |
| `charts/venue-mini/templates/fix.yaml` | With `fix.enabled`, the FIX adapter in front of the gateway's private half, its data and audit volumes, its Service and network policy (below) |
| `laptop.sh` | `up`, `web`, `expose`, `scale`, `restart` and `down` for a local k3d cluster, its API on loopback |

## On a laptop

Needs Docker, [k3d](https://k3d.io) 5.9, Helm 4.3 and kubectl 1.37.

```sh
deploy/laptop.sh up     # build both images, install, seed, smoke test
deploy/laptop.sh web    # then open http://127.0.0.1:8080 (deploy/laptop.sh web 18080 if 8080 is taken)
```

`up --split` (or `VENUE_SPLIT=1`) serves the gateway's halves from StatefulSets of their own (several gateways,
below); `--no-fix` (or `VENUE_FIX=0`) leaves out the FIX adapter and its image; other flags, such as
`--set gateway.split.public.replicas=3`, pass through to `helm upgrade`, which otherwise resets values to the chart's.

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
The same port also serves the gateway itself under `/v4`, and the FIX adapter listens on the `venue-fix` Service
(`kubectl port-forward svc/venue-fix 9879`, then `adapters/fix/scripts/fix-shell.py --port 9879`):

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

A cluster `up` creates has its Kubernetes API on loopback only, `127.0.0.1:6550` (`VENUE_API_PORT` to change it, for
example `127.0.0.1:6551` when that port is taken); k3d's own default would listen on `0.0.0.0` on a random port, where
anyone who can reach the machine can reach the cluster's admin endpoint. `up` makes `k3d-venue` kubectl's current
context. A cluster created before this change keeps the binding it was created with: only recreating it moves it
(below).

### On an address of this machine

`up` installs an Ingress (Traefik's, which k3d ships) that serves the page and the gateway on one port, and the FIX
adapter's Service is a LoadBalancer that k3s publishes on the cluster's node. Neither reaches the machine until k3d's
load balancer maps a host address to them; `expose` adds both mappings, bound to the one address given:

```sh
deploy/laptop.sh expose 127.0.0.1                    # page and /v4 on 127.0.0.1:8088, FIX on 127.0.0.1:9878
deploy/laptop.sh expose <private-address> 8091 9879  # a private address of this machine (LAN, VPN or tailnet)
# which is:
k3d cluster edit venue --port-add <private-address>:8091:80@loadbalancer --port-add <private-address>:9879:9879@loadbalancer
ss -tln | grep -E ':(8091|9879) '                   # listening on that address only, not 0.0.0.0
```

`<private-address>` stands for an address this machine holds on a private network that only the people meant to
use the demo can reach, such as its address on a LAN, a VPN or a tailnet (`ip -brief address` lists them). It is
never `0.0.0.0` or any other wildcard, and never an address the public internet can reach.

k3d recreates its load balancer container with the ports; the cluster keeps running, and the mappings persist.
A cluster can also be created with them, so that deleting and recreating it is one command (`VENUE_EXPOSE` is read
only when `up` creates the cluster; on an existing one, use `expose`):

```sh
deploy/laptop.sh down                                # the journal goes with it
VENUE_SPLIT=1 VENUE_EXPOSE=<private-address> VENUE_EXPOSE_HTTP_PORT=8091 VENUE_EXPOSE_FIX_PORT=9879 deploy/laptop.sh up
ss -tln | grep -E ':(6550|8091|9879) '               # 127.0.0.1:6550, and the two ports on <private-address> only
```

`VENUE_FIX_SERVICE_PORT` (default 9879) must match `fix.service.port` if that is changed.
Never give an address the public can reach: the page and its operator APIs are unauthenticated and its tokens are
fixed dev tokens (`ingress.operatorApis`, below). k3d's API port is set when the cluster is created, which `up` does on
`127.0.0.1` (above).

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
# Records in a local PostgreSQL, one schema per service (core/store/README.md); migrated at start for the laptop.
export VENUE_DATABASE_URL='postgres://postgres:dev@127.0.0.1:5432/postgres?sslmode=disable' VENUE_MIGRATE_ON_START=true
$BIN/venue-identity --catalogue $CAT --listen 127.0.0.1:18202 &
$BIN/venue-treasury --ingress 127.0.0.1:18200 --feed 127.0.0.1:18201 --catalogue $CAT --listen 127.0.0.1:18203 &
$BIN/venue-parlays --ingress 127.0.0.1:18200 --feed 127.0.0.1:18201 --catalogue $CAT --listen 127.0.0.1:18204 \
  --operator 127.0.0.1:18205 &
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
another and resumes. `deploy/laptop.sh up` replaces these StatefulSets' pods after a rebuild, as it does the
Deployments'.

**One host name.** With `ingress.enabled` (`ingress.className`, default `traefik`; `ingress.host`, default any host),
an Ingress routes by path as the gateway tells its halves apart (`Half::of` in `core/gateway/src/api.rs`):
`/v4/ws_public` and `/v4/markets` with everything under it to the public Service, the rest of `/v4` (`/v4/ws_private`,
`/v4/account`, `/v4/orders`, `/v4/transfers`, `/v4/rfqs`, `/v4/keys`) to the private one, and everything else to the
page. A path cannot name a method, so a POST to `/v4/markets` reaches the public half too, which refuses it as the
gateway would. Without the split, `/v4` goes to the one gateway. WebSockets pass through Traefik as they are; the
page's and the bots' sessions heartbeat every 15 s, well inside its timeouts. `/oracle` and `/treasury`, the dummy
oracle's control API and the treasury's reconciliation summary, reach the page's server and through it those services,
as under `laptop.sh web`: for a demo on a private network only. `ingress.operatorApis: false` routes only `/` and
`/api` to the page, so neither is served (and the settlement tab and the reconciliation section stop working); that
is the setting for anything beyond a demo, and even then the page hands out dev tokens.
Behind the ingress every gateway connection comes from Traefik's pod, so the gateways are told to believe the
`X-Forwarded-For` of connections from the pod network (`ingress.trustedProxies`, `10.42.0.0/16`, k3s's default
cluster CIDR), and their audit records carry the address Traefik received the request from as `client` beside `peer`,
the Traefik pod (interfaces/audit/README.md, the client behind a proxy). On k3d that address is not the machine that
dialled an `expose`d address: k3d's load balancer proxies TCP to the node and k3s's ServiceLB forwards it on to Traefik,
so Traefik sees the node's pod network gateway (`10.42.0.1`), and that is what `client` says; only a load balancer that
passes the client's address on (PROXY protocol, which k3d's does not speak) would record the address itself. Every
pod in the network may send a forwarding header the gateways believe, which changes only what their audit records say.

**Scaling each half.** The halves' replica counts are independent values:

```sh
helm upgrade venue deploy/charts/venue-mini --reuse-values --set gateway.split.public.replicas=3
helm upgrade venue deploy/charts/venue-mini --reuse-values --set gateway.split.private.replicas=1
deploy/laptop.sh scale public 3                      # the same, and waits for the rollout
```

A new pod takes traffic once it has caught up with the core's feed (its readiness probe). A pod scaled away is
first taken out of its Service's endpoints and keeps answering for `gateway.drainSeconds` (a `preStop` sleep), so
the ingress and kube-proxy stop sending it new connections; then it stops at once, closing its sessions, whose clients
(the page, the bots, the FIX adapter's gateway sessions and so FIX sessions) reconnect through the Service to a pod
that remains. The page's server passes `/v4` to the Services, not to pods, so it keeps working as either half scales.
The gateway does not close its audit file on SIGTERM: the file of a pod scaled away stays unsealed on its volume, which
scaling down keeps (`persistentVolumeClaimRetentionPolicy: Retain`), until a pod of the same name returns and seals
it. Its chain verifies as it is (`interfaces/tools/audit_verify.py` without `--closed`); copy the files out with
`kubectl cp <pod>:/var/lib/gateway-audit <dir>` while the pod runs.

**FIX.** With `fix.enabled`, the FIX adapter runs as a StatefulSet of one (`<release>-fix`: Artio's session state has
one writer), its sessions' gateway WebSockets going to the private Service and its security list read from the public
one (`--public-gateway`), or both to the one gateway without the split. A FIX session whose private gateway pod goes
away is logged out, and the client logs on again to another. Its data (Artio's sequence numbers and message archive)
and its audit log are volumes of their own, kept across replacements; the pod's name is its audit instance. Artio's
archive keeps every Logon's Password (554), API key secrets included, in clear, so the adapter runs with umask 077 and
keeps its files in subdirectories private to its user, no other pod mounts the volumes, the pod has no service account
token, and wherever real keys are used the volumes belong on encrypted storage with access to the node's disk
restricted. Its network policy lets anything reach its FIX port (a Logon's credential admits a client) and lets the
adapter reach only the gateway pods and cluster DNS. `fix.service.type` is `LoadBalancer` for k3d (published on the
node, then mapped by `expose`), `ClusterIP` to keep FIX inside the cluster; `fix.service.port` is its port.

## From a phone, a simulator or a front end in development

The demo is served over plain HTTP, which a laptop browser is happy with and a phone is not: iOS refuses cleartext by
default and Android has since 9. Two things make a device work.

**A certificate.** `ingress.tls.secretName` names a Secret of type `kubernetes.io/tls` already in the namespace, and
the venue is then served over `https` and `wss`. For a test, a self-signed certificate for the host name the device
will use, trusted on the device (iOS: install the profile, then Settings → General → About → Certificate Trust
Settings; Android: install as a CA and add a `network_security_config` that trusts user CAs in the debug build):

```sh
openssl req -x509 -newkey rsa:2048 -nodes -days 365 -subj "/CN=venue.local" \
  -addext "subjectAltName=DNS:venue.local,IP:<private-address>" -keyout tls.key -out tls.crt
kubectl create secret tls venue-tls --cert=tls.crt --key=tls.key
helm upgrade --install venue deploy/charts/venue-mini --set ingress.enabled=true \
  --set ingress.host=venue.local --set ingress.tls.secretName=venue-tls
```

**An origin the gateway answers.** A front end served from its own development server is another origin, and the
browser refuses to let it read the answer unless the gateway says so (interfaces/gateway/README.md, browsers):

```sh
helm upgrade --install venue deploy/charts/venue-mini --set gateway.allowedOrigins="http://127.0.0.1:8081"
```

Where the device looks for the venue: a real phone and an iOS simulator use the machine's own address on the network
they share; an Android emulator reaches its host as `10.0.2.2`. Neither can use `127.0.0.1`, which is the device
itself.

## An environment per pull request

A pull request labelled `deploy-env` gets a venue of its own — its own namespace, its own databases, its own journal
— and loses it again when the label comes off, the request closes, or the time limit passes. `deploy/argocd/` holds
the four pieces:

| | |
|---|---|
| `project.yaml` | What a preview is allowed to be: its own namespaces and nothing else, no cluster-scoped objects. |
| `applicationset.yaml` | One Application per labelled pull request, from that request's head. |
| `preview.yaml` | The values a preview runs with, chosen for footprint. |
| `reaper.yaml` | Takes the label off environments older than the limit, so ArgoCD takes them down. |

```sh
kubectl create namespace argocd
# --server-side, not a plain apply: the ApplicationSet CRD is larger than the 256 KiB annotation a client-side apply
# writes, and a plain apply fails with "metadata.annotations: Too long".
kubectl apply --server-side -n argocd -f https://raw.githubusercontent.com/argoproj/argo-cd/stable/manifests/install.yaml
kubectl create secret generic venue-github-token -n argocd --from-literal=token=<a token that may read PRs and write labels>
kubectl apply -f deploy/argocd/
```

ArgoCD also needs to be able to read the repository — a deploy key on it, added as a `repo` secret in `argocd` — or
every Application sits unsynced saying it cannot fetch the source.

Then label a pull request `deploy-env` and it appears at `pr-<number>.preview.venue.example` within a couple of
minutes.

**Environments are opt-in on purpose.** A full venue asks for about 1150m of CPU and 3.1 GiB of memory, so a node
with six cores and 15 GiB holds three or four beside everything else. Giving one to every open branch fills the
cluster the first busy week; giving one to every branch somebody asked about does not. `preview.yaml` trims what it
can — no FIX adapter, no observer, the smallest engine — and keeps what a reviewer actually needs, which is the
synthetic traders (an empty book shows nothing) and the oracle (without it nothing ever settles).

**The time limit works by removing the label, not by deleting anything.** This is the part that catches people. The
ApplicationSet owns its Applications: delete one, or its namespace, and the controller makes it again on its next
pass — which looks like a reaper that does not work, and is really two controllers disagreeing. The CronJob removes
`deploy-env` from the pull request instead; it then stops matching the generator, and ArgoCD removes the Application,
prunes the namespace and leaves it gone. Re-label to bring it back. The job also comments on the request saying what
happened, because an environment vanishing mid-review with no explanation is worse than one that never existed.

The reaper compares timestamps **as strings**. RFC3339 in UTC sorts lexicographically in time order, so nothing has
to parse a date — which matters, because the image's `date` is busybox's and cannot read RFC3339 at all.

**The chart cannot generate its own database passwords under ArgoCD.** It makes them with `randAlphaNum` and reuses
them by looking the Secret up in the cluster — correct under `helm install`, and impossible here, because ArgoCD
renders with `helm template`, which has no cluster to look in. `lookup` returns nothing, a new password is written on
every reconcile, and PostgreSQL goes on wanting the one it was initialised with; the environment comes up with
several containers failing to authenticate. The ApplicationSet works around it by writing that Secret once and then
ignoring it, which is sound for a database that dies with its pull request. **Anything meant to last should make its
passwords outside the chart and name them through `postgres.existingSecret`**, which the chart already supports.

### An image per pull request

A preview deploys the branch's chart, and it has to run the branch's *code* too, or it is a convincing test of the
wrong binary. `.github/workflows/preview-image.yml` builds the image when a pull request is labelled or pushed to,
and pushes it to this repository's own registry tagged with the head commit:

```
ghcr.io/<owner>/venue:<head sha>
```

The ApplicationSet names that exact tag, so the chart and the code it runs move together or not at all. The workflow
builds **only for labelled pull requests** — a release build of this workspace is long, and building every push on
every branch would spend an afternoon of runner time a day producing images nobody pulls. It is the same label that
asks for the environment, so there is one thing to remember rather than two.

It checks out `pull_request.head.sha` rather than the default. A `pull_request` event checks out a merge commit that
exists on no branch, and an image tagged with a SHA the ApplicationSet can never ask for is an image that never gets
deployed.

**The cluster needs a credential to pull it**, because the package is as private as the repository. One secret, in
each preview namespace — and since the namespaces come and go, the practical answer is a secret the cluster copies
into new namespaces, or one created alongside. A token with `read:packages` is all it needs:

```sh
kubectl create secret docker-registry ghcr-pull \
  --docker-server=ghcr.io --docker-username=<user> --docker-password=<a token with read:packages> \
  -n venue-pr-<number>
```

`image.pullSecrets` is empty by default, so a venue whose image was built and imported by hand — which is what
`laptop.sh` does — needs none of this and is unchanged.

**A preview never sees live data.** Each environment has its own identity service and its own database, so a session
from the live venue is nobody's in a preview and a developer signs in there separately (decision G-20). That is not
politeness: a preview runs a branch nobody has reviewed, and a branch that could verify live credentials could be
written to collect them. The `AppProject` is the other half of the same thought — a branch can change the chart, so
without a project restricting destinations to `venue-pr-*` it could deploy itself into the live namespace and ArgoCD
would oblige.

**That refusal happens at sync, not at admission**, which is worth knowing before you go looking for it. ArgoCD has
no admission webhook for destinations: an Application aimed somewhere it may not go is *created* quite happily, and
then never syncs, carrying a condition that says why —

```
InvalidSpecError: application destination server 'https://kubernetes.default.svc' and namespace 'default'
do not match any of the allowed destinations in project 'venue-previews'
```

— with nothing deployed. So the boundary holds, but what an operator sees is an Application sitting in error rather
than a `kubectl apply` that failed. Measured against this cluster, not assumed.

**Two things that will waste an afternoon on a single-node cluster**, neither of them the chart's fault, both met
the first time these environments were run:

- **Nothing external resolves from inside a pod**, on a k3s node whose host runs systemd-resolved. CoreDNS is
  configured `forward . /etc/resolv.conf`, the host's resolv.conf says `nameserver 127.0.0.53`, and from a pod that
  address is the pod's own loopback. Internal names work, so it looks like a firewall rather than DNS. k3s imports
  `coredns-custom`, so an `external.override` naming the resolver the host really uses fixes it:

  ```sh
  kubectl create configmap coredns-custom -n kube-system \
    --from-literal=external.override='forward . <the real resolver> 1.1.1.1 { policy sequential }'
  kubectl rollout restart deploy/coredns -n kube-system
  ```

- **ArgoCD's repo-server looks like it is crash-looping and is being killed.** Its liveness probe asks
  `/healthz?full=true` with a five-second timeout, and on a loaded node that check does not return in time, so the
  kubelet restarts it — over and over, mid-manifest-generation, leaving every Application unable to load its target
  state. The container exits 0 each time, which is the tell: it is being asked to stop, not falling over. Give the
  probe a longer timeout and a larger failure threshold.

**Finding an environment from the app** is `GET /v4/bootstrap` (interface 1 README, choosing an environment). The
live venue names the previews only to members its records mark as staff, and `--environments` points the gateway at
the file listing them — a ConfigMap, read afresh on every request, so environments coming and going needs no
restart.

## Databases

The treasury, identity, parlay and oracle services keep their records in PostgreSQL (`core/store/README.md`; the
reasons are `core/store/decisions.md`). Each owns a database, a role and a schema named after it, and no service reads
another's. Where those databases are is configuration only:

- **In the cluster** (`postgres.enabled: true`, the default): one PostgreSQL StatefulSet (`postgres.image`, pinned by
  tag and digest) on a volume of its own (`postgres.storage`), with the superuser's and each service's password
  generated into the Secret `<release>-postgres`, kept across upgrades and uninstalls (or `postgres.existingSecret`,
  URL-safe passwords). A network policy lets only the four services and the migrate job reach port 5432. The migrate
  job first makes each service's role, database and schema if they are missing (and keeps each role's password what
  the Secret says), then runs each service's `migrate`.
- **Outside it** (`postgres.enabled: false`), self-hosted or managed (RDS, Aurora): give each service a URL, inline
  (`database.treasury.url`, kept in a Secret the chart makes) or from a Secret of your own
  (`database.treasury.existingSecret`, `existingSecretKey`), and likewise for `identity`, `parlays` and `oracle`; the
  URLs may name one server or four. `database.tls` (default `verify-full`) and `database.caSecret` (a Secret with
  `ca.pem`, for example a provider's CA bundle) apply to all of them. The roles must own their databases (or at least
  their schema); the chart creates nothing there.

```sh
helm upgrade --install venue deploy/charts/venue-mini --set postgres.enabled=false \
  --set database.caSecret=rds-ca \
  --set database.treasury.existingSecret=venue-treasury-db --set database.identity.existingSecret=venue-identity-db \
  --set database.parlays.existingSecret=venue-parlays-db --set database.oracle.existingSecret=venue-oracle-db
```

**Migrations** are applied by **Atlas** (`core/store/README.md`), which the image carries beside the SQL files, in one
Job, `<release>-migrate`, a pre-install and pre-upgrade hook, so a schema is current before pods of a new build start;
while an in-cluster PostgreSQL does not exist yet (the install or upgrade that creates it), a Job
`<release>-migrate-first` runs beside it instead, since a hook could only wait for a server not yet created. Services
never migrate in the chart: at start each waits, saying so in its log, until its schema is at the migration it was
built with, and refuses to start against a newer one or one whose SQL has changed since it was applied. Every run takes
Atlas's lock, so overlapping runs are safe, and a run repeated applies nothing.

**A database made before Atlas** has tables Atlas kept no history for, and it refuses the whole run rather than
guess: `connected database is not clean: found table "accounts" in schema "identity"`. The migrate job then fails and,
being a pre-upgrade hook, the upgrade fails with it and nothing is rolled. Tell Atlas which migration that schema
already has, and it records that one as applied and carries on from the next — nothing is re-run, and nothing is
dropped:

```sh
helm upgrade --install venue deploy/charts/venue-mini --set migrations.baseline.identity=20260917000001
```

The version to name is the last migration that database really has. Read it off the schema rather than guessing: if
`identity` holds `accounts` and `api_keys` but no `members`, it is at `20260917000001`. **Take the value out again
after the upgrade.** On a schema Atlas already knows it does nothing, but left in place it would quietly bridge the
same error on a database that is genuinely in the wrong state, which is the one case you want to hear about. A
database this chart created is never in this position.

**Rolling back a migration** is Atlas's `migrate down`, which plans the revert against a throwaway dev database and
checks before it destroys anything (a column with data in it stops the revert rather than being dropped). It needs a
database Atlas may create for the planning, so it is run from a machine with Docker rather than from the job:

```sh
kubectl port-forward --address 127.0.0.1 svc/venue-postgres 15432:5432 &
atlas migrate down 1 --dir file://core/treasury/migrations --revisions-schema treasury \
  --url "postgres://treasury:$(kubectl get secret venue-postgres -o jsonpath='{.data.treasury-password}' | base64 -d)@127.0.0.1:15432/treasury?sslmode=disable&search_path=treasury" \
  --dev-url "docker://postgres/18/dev"
```

Roll the services back to the build that matches, or they will refuse to start against a schema they no longer know.
DevOps may run the same files with another tool: they are ordinary SQL in Atlas's layout (`core/*/migrations/`).

**Looking inside**, in the cluster:

```sh
kubectl exec -it statefulset/venue-postgres -- psql -U postgres -d treasury -c 'SELECT * FROM treasury.ledger_check'
kubectl exec -it statefulset/venue-postgres -- psql -U postgres -d identity -c 'SELECT key_id, account_id, revoked_at FROM identity.api_keys'
```

**When a database is unreachable**: the treasury refuses new transfers with 503 and goes unready (the gateway answers
transfers 503), keeps what is under way waiting and retries reconciliation; the identity service goes unready (the
gateway answers keys it has not verified in the last 5 s with 503; dev tokens keep working); the parlay service keeps
routing requests to the responders approved in memory and refuses approvals and revocations; the oracle keeps running
the calendar and pauses cases. Each reconnects on its own when the database returns; at start each waits for its
database with backoff, saying what it cannot reach. Deleting the PostgreSQL pod is a way to watch this.

Earlier versions kept these records in files on volumes; `venue-treasury import --ledger`, `venue-identity import
--file` and `venue-parlays import --responders` move them into empty databases (the oracle's cases were in memory).

## Observability

Prometheus, Alertmanager, Grafana, Loki and Alloy, with the venue's dashboards and alerts, install into namespace
`monitoring` of the same cluster with one command, from community charts pinned in `observability/versions.env`:

```sh
deploy/observability/install.sh
kubectl -n monitoring get secret grafana -o jsonpath='{.data.admin-password}' | base64 -d; echo
kubectl -n monitoring port-forward --address 127.0.0.1 svc/grafana 13000:80        # then http://127.0.0.1:13000
```

Nothing is exposed by default; exposing Grafana, sending metrics and logs to managed services, and alert receivers are
all configuration (`observability/README.md`). What each process exports, with units and labels, is
`observability/metrics.md`; the dashboards and alert rules are `charts/venue-observability`. Grafana, Loki and Alloy
are AGPL-3.0, used unmodified (the licence note in `observability/README.md`).

In the venue-mini chart, `metrics.enabled` (on by default) gives the core, the gateways and the FIX adapter a metrics
port, runs the observer (`<release>-observer`, `core/observer`: metrics derived from the journal and read-only
operator endpoints), writes every log line of those as JSON (`metrics.logFormat`), and, where the Prometheus
Operator's API exists, a PodMonitor labelling every series with `metrics.env` and `metrics.profile` (a laptop is
`functional`, blueprint §16). `metrics.monitoringNamespace` is the namespace let in to the metrics ports. The observer's
operator endpoints are internal too:

```sh
kubectl port-forward --address 127.0.0.1 svc/venue-observer 19103:9103
curl -s 127.0.0.1:19103/v1/markets; curl -s '127.0.0.1:19103/v1/markets/<market-id>?depth=5'; curl -s 127.0.0.1:19103/v1/accounts/1
```

## What the chart does and does not do

- **One way in.** Customers, bots and the page's trading tabs reach the core only through the gateway; a network
  policy lets only the gateway, the operator tools and the smoke test reach the core's ingress and feed.
- **One writer.** The core is `replicas: 1` with `updateStrategy: OnDelete`: a sequencer group never rolls on its own.
- **No liveness probe on the core.** A fail-stop (spec §18) exits the process; the restart replays to the same stop
  and shows as `CrashLoopBackOff`, which is the signal to look at the journal (`venue-node dump`).
- **Money through the treasury.** With `treasury.enabled` (the default) the gateway passes `/v4/transfers` to the dummy
  treasury, which keeps its ledger in its PostgreSQL database and moves buying power in the core only as sequenced grants
  and reclaims; only the gateway may reach it, and the web page for its reconciliation summary. Its fake bank's
  delays and failure rate are `treasury.bank` values. It reconciles its ledger against the fake bank's statement and
  the journal every `treasury.reconciliation.everyMs`; `treasury.faults: demo` loses a few percent of the bank's
  notices and of sleeve-adjustment answers on purpose, so the account tab's reconciliation section has breaks to show.
  The seed's grants are still how accounts start.
- **API keys through the identity service.** With `identity.enabled` (the default) the gateway verifies API keys with
  the identity service and passes `/v4/keys` to it; accounts and keys are in its PostgreSQL database and only the
  gateway may reach the service. Dev tokens keep working beside keys. Test-environment identity only (`core/identity/README.md`).
- **Seeding is idempotent.** The seed job runs after every install and upgrade; its commands have fixed IDs.
- **Not production.** Production runs the core on dedicated hosts behind Aeron Cluster (blueprint §5, §9). Tokens are
  fixed dev tokens, and the page and operator actions are unauthenticated; never expose them beyond a private network
  (a loopback or private address, above).
- **One oracle, bounded by the core's capacity.** The oracle is `replicas: 1` with `Recreate`, so two never act on a
  round. The engine releases a settled round half an hour after it resolves (the seed's retention), so the calendar
  runs indefinitely on the `small` profile.
- **Requests for quote are not journaled.** Open requests and responders' subscriptions live in the parlay service's
  memory; replacing its pod forgets them (the gateway subscribes its sessions again), while parlays and quotes
  (listings and orders) stay in the journal, and the approved responders in its PostgreSQL database. It lists at most
  `parlays.maxParlays` parlays at once, because every book reserves its levels until its settled event is released,
  and none that would take the core past 95% of its capacity profile, which the oracle's rounds share.
- **Not yet in the chart:** the resolution console behind the dummy oracle (its cases are in its PostgreSQL database,
  shown on the settlement tab), object storage for the archive, ClickHouse, Valkey, the scenario player and the
  environment manifest of §16.
