# Gateway throughput under the representative load, 2026-10-13

**Question.** What throughput does the gateway sustain on current `main`, under the representative load that
ADR-0002 settled on?

**Answer.** About **51.7k messages/s at 32 connections and 59.1k at 128**, steady across three replicates (the rate
moved by less than 1% between them), with no connection dropped and the feed never too far behind. That is
consistent with ADR-0002's numbers for the same load: +2.0% and +3.7%. Treat that as the same result, not an
improvement: the ADR's runs were on a different instance on a different day, and nothing here separates a code
difference of a few percent from host-to-host variation.

| load | metric | min | mean | max | ADR-0002 `fix` mean | change |
|---|---|---|---|---|---|---|
| 32 connections | messages/s | 51,480 | **51,681** | 51,932 | 50,645 | +2.0% |
| 32 connections | fills/s | 1,377 | 1,392 | 1,421 | 1,373 | +1.4% |
| 32 connections | round trip p50 / p99 (µs) | 483 / 3,670 | 490 / 3,700 | 496 / 3,717 | 489 / 3,656 | +0.3% / +1.2% |
| 128 connections | messages/s | 58,830 | **59,052** | 59,340 | 56,947 | +3.7% |
| 128 connections | fills/s | 2,150 | 2,158 | 2,169 | 2,098 | +2.9% |
| 128 connections | round trip p50 / p99 (µs) | 1,838 / 3,912 | 1,856 / 4,010 | 1,873 / 4,066 | 1,999 / 4,351 | −7.2% / −7.8% |

Other readings, per replicate: 0 connections dropped; `too_far_behind` 0; the gateway's feed-apply cost averaged
5.98 µs per result on the public half and 11.42 µs on the private half; the audit log grew by 12.8–12.9 GB per
replicate (two 60-second runs).

## What was run

- **Code:** `main` at `3aa4b31` (the merge of #47).
- **Machine:** one `c6i.8xlarge` **on-demand** in eu-central-1b (Intel Xeon Platinum 8375C @ 2.90 GHz, 32 vCPU,
  61 GiB, Linux 7.0.0-1014-aws, Ubuntu 24.04, rustc 1.99.0): see `facts.txt`. Spot had no capacity for this type in
  any of the three zones that day.
- **Build:** `cargo build --profile perf -p venue-node -p venue-gateway -p venue-sim` (fat LTO), and only those three.
- **Load:** `docs/adr/measurements/real.sh`, unchanged: node, gateway and load generator on the one host; the gateway
  with 8 pinned reactor threads and its audit log on; the load generator pinned to cores 9-15 and 25-31; eight books
  with exponentially unequal popularity; about ten orders per trade; two 60-second runs (32 and 128 connections) per
  replicate; three replicates back to back. The node ran `--capacity small --tick-ms 50`.
- **Housekeeping:** about ten minutes of instance time. The instance, its security group and its key pair were
  removed afterwards, and the account was checked for anything left.

## How to read it

- **It is throughput at saturation.** Each connection sends as fast as its replies come back, so this is what the
  gateway sustains under a closed-loop generator. It is not a promise about open-loop retail traffic, where arrivals
  do not wait for replies.
- **The round-trip columns are not venue latency.** They are the load generator's view, on a host shared by the
  node, the gateway and the generator, at saturation. The blueprint's rule (§16) is that a shared or functional
  environment must not produce a latency figure that gets quoted. They are here only because ADR-0002 recorded the
  same columns, and the comparison is like for like.
- **The sequencer here is the single-member simulated one** (`core/node`), not the Aeron cluster path (WP-10). The
  engine inside it is the real engine. Numbers for the clustered path do not exist yet.
- **Only one arm was run**, so unlike ADR-0002 nothing was interleaved to cancel host drift.

## What this does not cover

- **The gateway's memory.** Not sampled in this run. It was measured the same day in
  [`../2026-10-13-gateway-memory/`](../2026-10-13-gateway-memory/README.md): the gateway stays under about 205 MB
  at this load, and it was the load generator that grew without bound.
- The ADR's other loads: the retail population sweeps from a separate client machine, amend-only, and the watcher
  ladders.
- Anything on production-grade, pinned hardware, which is what a latency figure would need.

## Reproduce

**To measure the gateway today, use `deploy/loadtest/` instead** (README there). It runs this same load with the
generator on a box of its own, which this run did not. On one box the 32-connection figure comes out about a third
higher than across a network (`../2026-10-13-two-host/`), so compare a new number with that record, not this one.

To repeat exactly this single-host run, use the scripts in `docs/adr/measurements/` (`launch2.sh`,
`provision2.sh`, `real.sh`, `allreal.sh`). What was changed for this run, and nothing else: one host instead of two;
on-demand in a chosen zone after spot capacity failed; only the three binaries built; one arm in `allreal.sh`. Then:

```sh
python3 -I aggregate.py . ../../adr/measurements   # this run against ADR-0002's real-fix-* runs
```

## Files

| file | what |
|---|---|
| `real-main-{1,2,3}.txt` | The raw output of each replicate. |
| `facts.txt` | The machine and toolchain. |
| `aggregate.py` | Parses these and ADR-0002's `real-fix-*` the same way and prints the table above. |
