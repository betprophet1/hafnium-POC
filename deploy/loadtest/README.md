# Load-testing the gateway

`loadtest.sh` measures what the gateway sustains under load, and how much memory it uses, on two throwaway EC2
boxes. The node and gateway run on one and the load generator on the other. It is the procedure behind the records
in `docs/measurements/2026-10-13-*`, made repeatable.

**Do not run the load on a laptop or a shared machine.** The generator and the venue on one box measure each other.
Loopback also makes the round trip unrealistically short, which inflates the 32-connection figure by about a third
(51.7k against 39.4k messages/s across a real network). A long run was also, until the fix of 2026-10-13, enough to
exhaust the memory of any machine: see `docs/measurements/2026-10-13-gateway-memory/`.

## What you need

- The AWS CLI (v2), signed in to an account where you may create EC2 instances, key pairs and security groups:
  `aws sso login --profile <yours>`, then `export AWS_PROFILE=<yours>`.
- **48 free on-demand vCPUs** in the standard instance families (A, C, D, H, I, M, R, T, Z): 32 for the server, 16 for
  the client. The account this was developed in had a limit of 64, which the live venue's box also counts against;
  check yours under Service Quotas, "Running On-Demand Standard (A, C, D, H, I, M, R, T, Z) instances".
- `git`, `ssh`, `scp` and `curl` here, and this checkout. The boxes build the code themselves.

## Cost

About **$2 an hour** while both boxes exist, with the default types on-demand in eu-central-1. A full run takes
about 25 minutes from `up` to `down` (the load itself is about 20), so about **80 cents**. A `--quick` check took
3.5 minutes from `up` to `down` when this was written, about **15 cents**. A pair forgotten overnight is about $50 a
day, which is why `all` always removes them and `status` exists.

## Quick start

```sh
export AWS_PROFILE=<your profile>
deploy/loadtest/loadtest.sh all results/$(date +%F)       # up, provision, run, fetch, down
python3 -I docs/measurements/2026-10-13-two-host/aggregate.py results/$(date +%F)
```

`all` removes everything at the end even if a step fails, so a failed run costs only the time it took. Check
afterwards anyway:

```sh
deploy/loadtest/loadtest.sh status      # should list nothing
```

## Step by step

For watching it, or for running only part of it:

```sh
deploy/loadtest/loadtest.sh up              # two instances, a key pair, a security group (ssh from your address only)
deploy/loadtest/loadtest.sh provision       # copy the code at $REF to both and build it (about two minutes)
deploy/loadtest/loadtest.sh run --quick     # one 15-second load: is everything connected?
deploy/loadtest/loadtest.sh run             # the full load, about 20 minutes
deploy/loadtest/loadtest.sh fetch results/x # results and machine facts into results/x
deploy/loadtest/loadtest.sh down            # remove it all, then list what is left (nothing)
```

To measure a branch or a commit other than what is checked out, set `REF`:
`REF=origin/main deploy/loadtest/loadtest.sh provision`.

## What `run` does

| part | load | venue | what it shows |
|---|---|---|---|
| Representative, ×3 | 60 s at 32 connections, then 60 s at 128; 8 books with exponentially unequal popularity; about 10 orders per trade | fresh, `small` profile, 512 accounts | throughput, and the gateway's memory at the load ADR-0002 settled on |
| Retail ladder | 32 trading connections plus 0, 1,024, 4,096 and 16,384 retail clients on 2 books each, 8 watchers, 60 s | fresh per rung, `launch` profile, 33,000 accounts | memory and market-data delivery as the population grows |
| Soak | 300 s at 128 connections | fresh, `small` | whether anything grows with time |

The server samples the gateway's and the node's memory once a second, and the client samples the generator's. After
each load the server also records the gateway's feed-apply cost, any "too far behind", the audit log's size, and the
gateway's peak memory and threads.

## What the traffic looks like

The generator (`venue-sim gwload`, `core/sim/src/gwload.rs`) has four kinds of client. The figures are from the
two-host run of 2026-10-13 (`docs/measurements/2026-10-13-two-host/client.txt`).

| client | what it does | sockets |
|---|---|---|
| **Market maker** (a quarter of the trading connections) | rests a sell on its book and keeps amending it back to size, so the takers never run out of liquidity | 1, private |
| **Taker** (the other three quarters) | nine passive post-only buys a few ticks behind the makers, each cancelled before the next, then one order that crosses: about 10 orders per trade. An algorithmic account's shape, not an app user's | 1, private |
| **Retail client** (`--retail`, ladder only) | logged in and subscribed to the book of 2 of the 8 markets, weighted towards the popular one; reads market data continuously and sends one order every 10 seconds | 2, private and public |
| **Watcher** (`--watchers`, ladder only) | subscribes to every book and only reads: an API customer following the whole venue | 1, public |

**The representative load is all professional traffic.** At 128 connections (32 makers, 96 takers) the 60k
requests a second split into about 17.7k maker amends (29%), 22.3k taker orders and 20.1k cancels (71%), for about
2,170 trades a second.

**The ladder adds retail on top of 32 professional connections** (8 makers, 24 takers). At 16,384 retail clients:

| | professional (32 connections) | retail (16,384 clients) |
|---|---|---|
| orders in | about 33.5k requests a second | about 1.6k orders a second (one per client every ~10 s) |
| trades | about 940 fills a second | about 1,560 fills a second (nearly every retail order fills) |
| market data out | 8 watchers, about 280 messages a second | about 213k messages a second (about 13 a second per client) |

So retail is 4–5% of the order messages but about 60% of the trades, and almost all the outbound market data. The
gateway's cost of retail is fanning book updates out to tens of thousands of sockets. That is where the memory per
client goes, and why professional throughput fell 16% from 0 to 16,384 clients.

**These proportions are assumptions, not production data.** They were chosen when ADR-0002 was written:
- makers as a quarter of connections
- ten orders per trade
- one retail order every ten seconds
- two markets per retail client
- retail orders that cross

Each is a generator flag (`--makers`, `--orders-per-trade`, `--retail-order-seconds`, `--retail-books`,
`--retail`, `--watchers`). `loadtest.sh` does not take them as settings: they are written into its `run` step, so
change them there. To make the test describe ProphetX traffic, take the real figures from the current production
venue (orders per second by kind of client, order-to-trade ratio, sessions per user, markets followed) and set them.

## How many connections, so far

| kind | most tested | result |
|---|---|---|
| Retail clients | 16,384 (about 32,850 sockets on the gateway) | none dropped; 213k market-data messages a second delivered; gateway peak 816 MB |
| Active trading connections | 128 here; 256 in ADR-0002's older runs | throughput levels off at about 60k messages a second; more connections queue rather than add capacity |

**No connection limit has been found yet.** Nothing gave out at the top rung. The gateway's memory per retail
client fell to about 40 KB there, but extending that to larger populations would be a guess. What is likely to
bind first, none of it measured:
- professional throughput under heavy retail load (−16% at 16,384 clients);
- the gateway's thread count, which rose from 25 to 166 at the top rung for reasons nobody has looked into;
- the test itself. One client box can hold only about 30,000 retail clients: two sockets each, against about
  64,000 local ports. Going higher needs several client boxes, which `loadtest.sh` does not do yet, and more vCPUs
  than a 64-vCPU on-demand limit allows.

## Reading the results

`fetch` writes, for the full run: `client.txt` (the generator's report for each load, and the client's load
average), `server.txt` (the server's readings after each load), `srv-*.csv` and `cli-*.csv` (the samples),
`facts-server.txt`, `facts-client.txt` and `REF` (the commit measured).
`docs/measurements/2026-10-13-two-host/aggregate.py DIR` turns a full run into the tables in that record's
README, which are the numbers to compare against. It needs a full run, not `--quick`.

- **Quote the 128-connection throughput** as what the gateway sustains: about 59.7k messages/s on 2026-10-13. The
  32-connection figure is limited by the round trip, because each connection waits for its reply, so it depends on
  the network more than on the gateway.
- **The round-trip columns are not venue latency.** They are the generator's view of a saturated gateway on cloud
  hardware that is not pinned for latency (blueprint §16).
- **Check the client's load** in `client.txt`. If it nears the client's vCPU count, the generator is the limit and the
  run measures the client, not the gateway.
- **Check `too_far_behind`** in `server.txt`. It should be 0.

To keep a run as a record, put it in `docs/measurements/<date>-<what>/` with a README saying the conditions, the
results and what the run does not cover, as the existing records do.

## Settings

| variable | default | what |
|---|---|---|
| `AWS_PROFILE`, `AWS_REGION` | `venue`, `eu-central-1` | where it runs |
| `LOADTEST_AZ` | `<region>b` | the zone for both boxes; change it when a zone has no capacity for the type |
| `SERVER_TYPE`, `CLIENT_TYPE` | `c6i.8xlarge`, `c6i.4xlarge` | the server matches every record so far; the client never passed a load of 3.3 on 16 vCPU |
| `SERVER_MARKET`, `CLIENT_MARKET` | `ondemand` | `spot` is about half the price, but often has no capacity for these types, and one spot client was reclaimed mid-run, taking its results with it |
| `REF` | `HEAD` | the commit to build and measure |
| `LOADTEST_NAME` | `venue-loadtest` | what everything is named and tagged; two people running at once need different names |
| `LOADTEST_STATE` | `~/.cache/venue-loadtest` | the key, the addresses and the provisioning logs |

## When it goes wrong

- **`InsufficientInstanceCapacity`**: the zone has none of that type right now. Run `down`, then `up` again with
  another `LOADTEST_AZ`.
- **`VcpuLimitExceeded`**: the quota above. Use a smaller `CLIENT_TYPE` or ask for a higher limit.
- **A box stops answering mid-run**: if it was spot, it was probably reclaimed (`status` shows it gone). Its results
  are lost; `fetch` what the other box has, then `down`.
- **Provisioning failed**: the logs are `$LOADTEST_STATE/provision-{server,client}.log`.
- **`up` refuses because the security group exists**: a previous run was not taken down. Run `down`.
