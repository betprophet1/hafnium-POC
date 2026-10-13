# Gateway memory under load, 2026-10-13

**Question.** Running load tests on a development machine has been taking the machine down, and the gateway was the
suspect. How large does the gateway get under the representative load, as client counts grow, and over time?

**Answer.** **The gateway is small and steady. The load generator, `venue-sim gwload`, is what grows without bound.**

| what | result |
|---|---|
| Gateway, representative load (32 then 128 connections) | peaked at **193–205 MB** in all three replicates |
| Gateway, five minutes at 128 connections | **188 MB** after the first minute, **179 MB** at the end, a slope of **−0.5 MB/min**: flat |
| Gateway, 16,384 retail clients (launch profile) | **776 MB** peak, 581 MB at the end, against 150 MB idle |
| Load generator, one representative replicate | up to **12.7 GB** |
| Load generator, five-minute soak | grew about **12.4 GB/min** to **60.1 GB** on a 61 GB machine; **0.1 GB** was left available |

On the soak, the generator vanished at 292 s and printed no summary line. With the machine at 0.1 GB available, the
most likely reading is that the kernel killed it. The kernel log was not captured, so that is inferred, not seen.

## Why the generator grows

`Session::request` in `core/sim/src/session.rs` waits for the reply with the matching id, and pushes **every other
message** onto an `inbox` queue, "kept for `poll`". The generator's trading loops (`drive`, and the makers and takers
in `core/sim/src/gwload.rs`) only ever call `request`. Nothing calls `poll`, so nothing drains the queue, and every
unsolicited message the gateway sends on that connection (order updates, fills) stays in memory as a parsed JSON
value for the life of the run. The growth rate fits that: roughly 150 to 210 MB/s at 50,000 to 60,000 requests/s,
which is a few kilobytes held per request.

That is read from the code, and the numbers agree with it. I did not instrument the generator, so I have not
counted which messages pile up.

## Gateway, in detail

**Representative load** (small profile, 60 s each at 32 and 128 connections, the load of ADR-0002): the gateway
held 32 MB when started, 80–85 MB with 32 connections, about 185 MB with 128, and peaked at 193–205 MB. Threads
stayed at 25 and descriptors at 172.

**Retail ladder** (launch profile, 33,000 accounts, 32 trading connections plus N retail clients, a fresh server for
each rung):

| retail clients | idle MB | peak MB | end MB | peak above the 0-client rung, per client | threads | descriptors |
|---|---|---|---|---|---|---|
| 0 | 138 | 179 | 179 | - | 25 | 84 |
| 1,024 | 150 | 291 | 261 | 112 KB | 25 | 2,132 |
| 4,096 | 150 | 491 | 491 | 78 KB | 25 | 8,276 |
| 16,384 | 150 | 776 | 581 | 37 KB | 134 | 32,852 |

The cost per client falls as the population grows, and at the top rung about a quarter of the peak is gone by the end
of the run, so part of the peak is a burst at login. The thread count jumped from 25 to 134 at 16,384 clients; I did
not look into why.

**Soak.** 300 seconds at 128 connections: no growth.

## What it means for running load tests

- The thing to watch and bound is the generator, not the gateway. On this machine one two-minute replicate needed
  12.7 GB, and a soak used 60 GB in five minutes. Neither fits a small shared machine, which fits what has been
  happening locally.
- Running on AWS, as already decided, is right for latency for other reasons. For memory the real fix is in the
  generator: bound or drain the `inbox` (for example, drop the oldest messages beyond a cap, and count them).
  I have not made that change.
- Page cache from the gateway's audit log (up to 16.5 GB cached in the replicates and 21.7 GB in the soak, with at
  most about 520 MB of dirty pages) is reclaimable, and in the soak it was given back when the generator needed the
  memory, so it did not cause trouble here.

## What this does not cover

- The soak's throughput: its summary line was never printed, so the soak shows memory only.
- Retail-client **throughput**. The ladder drove the clients from the same host, which ADR-0002 found is
  client-bound above about a thousand clients. Memory is from the gateway's own process and is fine to read; the
  messages-per-second in `ladder.txt` above 1,024 are not the gateway's limit.
- A heap or allocation profile, and any rung beyond 16,384 retail clients.
- Which messages fill the generator's inbox (see above).

## The throughput of these same runs

The three replicates also give throughput, with the sampler running at once (one sample a second): **50,343
messages/s at 32 connections** (48,411–51,370) and **59,817 at 128** (59,450–60,028). The first run today, without
a sampler, gave 51,681 and 59,052 (`../2026-10-13-gateway-throughput/`). The two agree to within about three percent,
so the sampler did not visibly cost anything. The spread between replicates at 32 connections was 5.9% here against 0.9%
there. The raw output is `real-main-{1,2,3}.txt`.

## Conditions

One `c6i.8xlarge` on-demand in eu-central-1b, `main` at `57e3d13` (the code is the same as `3aa4b31`; the commit
adds only docs), the same build and load as the throughput record. `facts.txt` has the machine. The instance, its
security group and its key pair were removed afterwards, and the account was checked. The load itself ran for about
17 minutes.

## Files

| file | what |
|---|---|
| `mem-rep-{1,2,3}.csv` | One sample a second across each representative replicate. |
| `mem-ladder-{0,1024,4096,16384}.csv` | The same for each ladder rung. |
| `mem-soak.csv` | The same across the five-minute soak. |
| `ladder.txt`, `soak.txt` | The load generator's own output for those, and the gateway's peak at the end. |
| `real-main-{1,2,3}.txt`, `memrun.log` | The replicates' raw output, and the run's progress log. |
| `aggregate_memory.py` | Produces the tables above from the CSVs: `python3 -I aggregate_memory.py .` |
| `series.py` | Prints one CSV as a time series: `python3 -I series.py mem-soak.csv 30` |

Columns in the CSVs: `ts` (epoch seconds), then the gateway's resident memory, peak resident memory, threads and
descriptors; the node's and the generator's resident memory; and the system's available memory, page cache and
dirty pages. All memory columns are in kB.
