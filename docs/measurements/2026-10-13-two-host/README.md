# Gateway with the load generator on its own box, 2026-10-13

**Questions.** With the load generator's memory fixed (`Session::discard_unsolicited`, this branch) and the
generator on a machine of its own: does the generator stay small, what does the gateway do with nothing else on
its machine, and what throughput does it sustain?

**Answers.**

- **The generator fix works.** The generator peaked at **32–67 MB** per representative load, against 12.7 GB before
  the fix, and at **173 MB** over a five-minute soak, against 60 GB and being killed (`../2026-10-13-gateway-memory/`).
  It still grows by about 27 MB a minute in the soak. That matches the latency sample it keeps per request for the
  percentiles (8 bytes at about 58,000 requests a second), so it is bounded by the run's length.
- **The gateway matches the single-box run.** It peaked at **212–213 MB** at the representative load, stayed flat
  through the soak (**−0.07 MB/min**), and reached **816 MB** at 16,384 retail clients.
- **At 128 connections the gateway sustains about 59.7k messages/s**, the same as on one box (59.1k and 59.8k).
  That figure is the gateway's ceiling for this load.
- **At 32 connections it is 39.4k, not 51.7k**, and that is the network, not the gateway. With 32 connections
  each one waits for its reply, so the rate is about connections ÷ round-trip time. Across a real network the round
  trip is longer than over loopback (p50 634 µs here against 490 µs), so the rate falls. At 128 connections there is
  enough concurrency that the gateway sets the rate. The single-box figure at 32 connections was flattered by
  loopback, and is not the one to quote.

## Representative load (60 s each, small profile, 8 books, ~10 orders per trade)

| load | messages/s (3 replicates) | spread | p50 / p99 round trip | fills/s | client load (16 vCPU) | generator peak |
|---|---|---|---|---|---|---|
| 32 connections | 39,212 / 39,250 / 39,773, mean **39,412** | 1.4% | 622–640 / 4,792–4,833 µs | 1,084–1,133 | 1.1–1.5 | 32–33 MB |
| 128 connections | 60,000 / 59,531 / 59,496, mean **59,676** | 0.8% | 1,867–1,911 / 6,002–6,333 µs | 2,170–2,180 | 1.0–1.5 | 66–67 MB |

The gateway peaked at 212, 212 and 213 MB. The server never had less than 60.3 GB available.

## Retail-population ladder (launch profile, 33,000 accounts, 32 trading connections plus N retail clients, 60 s, a fresh server for each)

| retail clients | gateway idle | gateway peak | gateway end | per client | gateway threads | descriptors | trading msg/s | retail msg/s delivered | dropped | client load | generator peak |
|---|---|---|---|---|---|---|---|---|---|---|---|
| 0 | 110 MB | 180 MB | 180 MB | - | 25 | 84 | 39,903 | - | 0 | 1.2 | 34 MB |
| 1,024 | 110 MB | 262 MB | 262 MB | 82 KB | 25 | 2,132 | 38,407 | 15,741 | 0 | 0.7 | 300 MB |
| 4,096 | 110 MB | 382 MB | 359 MB | 50 KB | 102 | 8,276 | 37,835 | 61,042 | 0 | 3.3 | 1,096 MB |
| 16,384 | 110 MB | 816 MB | 600 MB | 40 KB | 166 | 32,852 | 33,459 | 212,941 | 0 | 3.2 | 4,280 MB |

With the clients on their own machine, these retail figures are the gateway's, unlike the single-box ladder. The
client box never passed a load of 3.3 on 16 vCPU, so it was not the limit. Trading throughput fell by 16% from 0 to
16,384 retail clients while the gateway delivered 213k market-data messages a second to them, with no client
dropped. The ADR-0002 sweep's equivalent row delivered 175k/s.

The gateway's thread count rose from 25 to 102 and then 166 as the population grew. I have not looked into why. It
was 25 throughout the representative load, apart from one replicate where it briefly reached 47.

The generator's own memory at 4,096 and 16,384 clients (1.1 and 4.3 GB) is the retail clients' sessions, which do
read their messages. That is the cost of simulating that many clients, not a leak.

## Soak (128 connections, 300 s)

| | |
|---|---|
| gateway | 184 MB at 60 s, 203 MB at the end, peak 205 MB, slope after the first minute −0.07 MB/min |
| generator | peak 173 MB, +27 MB/min (the latency samples); the client never had less than 29.9 GB available |
| throughput | 58,331 messages/s, p50 1,826 µs, p99 6,390 µs, 2,110 fills/s |

## How to read it

- The round-trip columns are the generator's view across the VPC at saturation. They are **not venue latency**
  (blueprint §16), and are here so that the runs compare like for like.
- Throughput is closed-loop: each connection waits for its reply before sending again (see the 32-connection
  point above).
- The node is the simulated single-member sequencer (`core/node`), not the Aeron cluster path (WP-10).

## Conditions

- **Code:** the `gwload-inbox` branch at `1aacdcd`, which is `main` (`57e3d13`) plus the generator fix.
- **Server:** `c6i.8xlarge` on-demand, eu-central-1b: node and gateway (`facts-server.txt`).
- **Client:** `c6i.4xlarge` on-demand in the same zone and subnet: the generator (`facts-client.txt`). It is smaller
  than the server because the account's on-demand limit of 64 vCPUs does not fit two 32-vCPU boxes beside the live
  venue. Its load never passed 3.3.
- **Load:** what `docs/adr/measurements/real.sh` runs (representative) and `client-sweep.sh` runs (ladder), driven from
  the client to the server's private address. The server sampled the gateway and node, and the client sampled the
  generator, once a second each.
- **The first attempt** used a spot `c6i.8xlarge` client, which AWS reclaimed during the 16,384-client rung.
  Everything on it was lost. The server's samples from that attempt are in `first-attempt-server/`. Its gateway
  peaks repeat this run's: 209–212 MB against 212–213 MB for the replicates, and 186 / 244 / 366 MB against
  180 / 262 / 382 MB for the first three rungs.
- **Housekeeping:** both boxes, the security group and the key pair were removed, and the account was checked
  afterwards. In total the boxes cost about $1.30.

## Files

| file | what |
|---|---|
| `client.txt` | The generator's output for every load, and the client's load average after each. |
| `server.txt` | After each load: feed-apply cost, `too far behind`, audit bytes, the gateway's peak and current memory and threads. |
| `srv-*.csv` | The server's samples once a second (gateway and node columns filled). |
| `cli-*.csv` | The client's samples once a second (generator column filled). |
| `facts-server.txt`, `facts-client.txt` | The two machines. |
| `first-attempt-server/` | The server's side of the interrupted first attempt. |
| `aggregate.py` | Produces the tables above: `python3 -I aggregate.py .` |

The CSVs have the same columns as `../2026-10-13-gateway-memory/`.
