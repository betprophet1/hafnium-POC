# ADR-0002: Replacing the gateway's global `Mutex<Hub>`

**Status:** Rejected — the premise did not survive measurement. See Prototype Findings.
**Superseded in part:** several measurements below are retracted; see Retraction 2 and [What a representative
load says](#what-a-representative-load-says-2026-09-27). The rejection itself is narrower than it reads: the
lock *does* matter, on the write side, once the venue has market-data subscribers — see [The lock does
matter](#the-lock-does-matter-on-the-write-side-with-customers-on-the-venue-2026-09-27). Neither of this
ADR's options was the fix.
**Date:** 2026-09-26
**Deciders:** core/gateway owners (Engineering)

> **Retraction 1 (2026-09-26, after this ADR was drafted).** The Context below argues that `Mutex<Hub>` is the
> gateway's throughput ceiling. Measured on dedicated hardware, it is not: swapping it for an `RwLock` changes
> throughput by 1–2%, which is noise, and the reactor threads that would contend for it sit ~97% idle. The ceiling
> is the audit log's `fsync`. The Context is kept as written because the state catalogue in it is still accurate
> and useful, but its conclusion — and the decision that followed from it — are wrong. Read Prototype Findings
> first.
>
> **Retraction 2 (2026-09-27). Three of the measurements below are wrong, and the load they were taken on was
> not representative.** In short:
>
> 1. **Every `fills/s` figure in this ADR is about 2.4× too high.** The harness counted the length of
>    `result.executions`, which carries an `accepted` for an order that merely rested, a `cancelled`, an
>    `amended` — not only fills. Fixed in `2adfa71`.
> 2. **"Taking `Value` off the feed path" claims −29% / −25% on apply cost. It is not reproducible.** That
>    measurement was taken between `f16227f` and `b1d2d1e`, exactly the window in which an allow-list bug was
>    silently dropping `OrderAmended` — so the gateway was skipping work, not doing it faster.
> 3. **Every trading number was taken on a load where every order traded, at one price level, on one book**,
>    in 8–10 second runs. A venue carries the opposite shape. Re-measured on a representative load, conclusions
>    drawn from the old one reverse.
>
> The corrected numbers, and what survived, are in **[What a representative load
> says](#what-a-representative-load-says-2026-09-27)**. Read that section before quoting any figure from this
> document.

## Context

`gateway/src/api.rs` guards essentially all of the gateway's in-memory venue state — the reconstructed view it builds by following the feed, every session, every account's subscriptions and replay history — behind one `Mutex<Hub>` per half (`struct Gateway { hub: Mutex<Hub>, ... }`, `api.rs:315-317`). Public and private each run their own `Gateway`/`Hub` pair, but within a half it is one lock for everything.

This was measured directly, not assumed. Real-hardware testing (c6i.4xlarge and c6i.8xlarge EC2 spot instances, `venue-sim gwload` driving repeated order-amend round trips through `/v4/ws_private`, reactor threads CPU-pinned via `core_affinity` to distinct physical cores) found:

- Raising `--reactor-threads` gave clean, near-linear throughput scaling only up to the pinned thread count, then plateaued rather than continuing to climb — more threads bought nothing once something else was fully serializing the work.
- Spreading load across 8 synthetic instruments (exponential popularity weighting, `venue-sim gwload --instruments N`) made throughput measurably *worse*, not better, at every matched connection count — e.g. at 96 connections, 6,803 msg/s single-instrument vs. 5,062 msg/s spread across 8 (p50 latency 8.4ms vs. 16.9ms). This rules out "contention on one hot instrument" as the mechanism: a lock that were actually scoped per instrument would have benefited from spreading, not regressed.
- Both configurations show the same shape: throughput peaks around 2 connections, degrades from roughly 48 connections onward, with tail latency exploding at high connection counts (p99.9 in the hundreds of ms, max 400–500ms at 128 connections) — the signature of a single serialized queue saturating, independent of which instrument is involved.

Together this points at one thing: `Mutex<Hub>` itself, not per-instrument hot-spotting, is the ceiling.

**What's actually inside `Hub` (`hub.rs:217-245`), and who touches it.** A full call-site catalog (every `.hub()` acquisition across the gateway crate, both halves) shows the state splits cleanly into three domains:

- **Account-keyed**: `tokens` (dev-token → account), `by_account` (sessions per account), `history` (per-account private-message replay buffer), plus `view`'s sleeves/positions.
- **Instrument-keyed**: `view`'s instruments/ladders, `dirty` (book levels changed since last conflate flush), `by_topic`'s book/trades entries.
- **Global feed-progress**: `sessions`, `connected`, `last_unit`, `last_sequence`, `highest_order`, `accepted`/`accepted_order`, `awaiting` — none of this is instrument- or account-keyed, and several hot paths touch it on every order lifecycle event.

Most reactor-thread call sites are narrow: one instrument (`place_input`'s market lookup, `GET /v4/markets/{id}/book`), one order (`amend_input`, `cancel_input`), one account (`authenticate`, `login`). But a handful are genuinely cross-cutting, and these are the ones that make sharding non-trivial rather than a mechanical find-and-replace:

- **`Hub::apply` (`hub.rs:392`)**, called from a single dedicated `gw-feed` thread on every feed unit — not from the reactor pool. One unit can update view state for an arbitrary number of instruments at once (a `TimeTick` releasing a whole event releases every one of its markets in the same call, `hub.rs:407-411`), compute executions for several accounts (a `Fill` always touches two — maker and taker), and fan out private messages to each of those accounts plus public messages to every affected topic.
- **`flush_books` (`hub.rs:731`)**, called from a single dedicated `gw-conflate` thread every 100ms, iterates every instrument in `dirty` in one lock hold — under real load, "most of them."
- **`mass_cancel_result`/`result`** (`api.rs:1046`, `api.rs:1058`) can span however many instruments one account's mass-cancel or multi-fill result touched, for one account, from whichever reactor thread handled that request.
- **`GET /v4/markets`** (`rest.rs:113`) scans the entire catalogue every call, by design (it's a full-venue snapshot endpoint).
- **`GET /v4/account`** (`rest.rs:129`) has no per-account index over `view.positions`/`view.orders` — every call linearly scans *every* position and order in the venue, filtering for the caller's account. This is an existing latent inefficiency, independent of which locking strategy wins; sharding would make it worse (cross-shard scatter) rather than better unless a per-account index is added first. Flagged, not addressed here (see Non-Goals).

**The read/write split that actually shapes this decision.** Writes into `view` state come from exactly three places: the `gw-feed` thread's `apply()`, the `gw-conflate` thread's `flush_books()`, and one narrow per-request write — `result()` (`api.rs:1059`) calls `hub.accept(...)` on the reactor thread that handled a request, but only when the results contain an `OrderAccepted` (i.e., once per new order, not on every amend/cancel). Everything else the reactor pool does under load — order lookups, market data reads, `amend_input`'s two reads, REST reads — is a pure read. Concretely, for the exact workload this ADR's own measurements were driven with (`gwload`: one order placed, then repeated amends), only the very first request per connection takes the write path; every subsequent request under load is read-only. This is not a coincidence particular to the benchmark — placing new orders is inherently rarer than amending/cancelling/querying existing ones in any real trading pattern.

This matters because it means the dominant contention today is very likely **reader-reader** blocking (reactor threads serialized against each other for reads that don't conflict), not writer-writer or reader-writer conflict — which is a strictly easier problem to solve than a general sharding scheme has to solve.

**Existing precedent in this crate**: `Gateway` already keeps several separate, purpose-scoped locks — `rest_buckets`, `anonymous_buckets`, `logon_buckets` (flood-limit state), a per-in-flight-request `Mutex<Unseen>` (`api.rs:65`), `identity.rs`'s API-key cache. `Hub`'s single coarse lock is the outlier in an otherwise fine-grained-locking codebase, not the house style.

## Decision

Prototype and benchmark three concrete alternatives, locally, before adopting any of them for real:

**A. `RwLock<Hub>`** in place of `Mutex<Hub>` — the same single lock, same critical-section boundaries as today, but concurrent readers no longer block each other. Given the read/write split above, this targets the dominant contention directly with the smallest possible change: no new dependency (the crate already uses `std::sync::Mutex` directly, no `parking_lot`), no change to `Hub`'s internal structure, no new lock-ordering surface.

**B. Per-instrument sharded locking** — partition instrument-keyed state (`view.instruments`/`view.ladders`, `dirty`, `by_topic`'s book/trades entries) into N shards, each its own `Mutex` or `RwLock`, keyed by `instrument_id % N` (or a hash).

**C. Per-account sharded locking** — the same idea, partitioning account-keyed state (`tokens`, `by_account`, `history`, sleeves/positions) by `account_id % N`. This is the interpretation used for the user's "per-lock" — flagged explicitly here in case a different meaning was intended (e.g. splitting `Hub` into several purpose-scoped locks by *subsystem* rather than sharding either key space; that reading isn't prototyped below, but the account-keyed/instrument-keyed/global-feed-progress split found in the catalog above would be exactly the natural seam for it if that's what was meant).

Both B and C run into the same real structural problem: `Hub::apply`'s cross-cutting writes (one `TimeTick` can touch every market of a released event; a `Fill` always touches two accounts) mean the write path can't cleanly stay within one shard. A sharded design either needs a consistent global lock-acquisition order across however many shards one feed unit touches (reintroducing exactly the deadlock-avoidance discipline a single mutex sidesteps by construction), or `apply()` itself needs restructuring into per-shard batches applied under separately-ordered locks. This is real, correctness-sensitive engineering work on a live exchange's gateway — not something to wire into the real `Hub` unsupervised. Options B and C are therefore prototyped in isolation (a synthetic benchmark modeling the same read/write ratio, shard-crossing frequency, and hold-time shape, following the same precedent `core/prototypes/tieredbook` set for ADR-0001 — measure the *mechanism* before touching live code), while Option A — being a same-shape, correctness-preserving change — is implemented and benchmarked for real, against the actual gateway, using the same local stack and `gwload` methodology as the cloud testing this ADR is motivated by.

## Non-Goals

- **`GET /v4/account`'s unindexed full-venue scan.** Found along the way, real, and independent of which locking strategy is chosen — worth its own ticket, not folded in here. Note for whoever picks it up: fixing it before sharding account-keyed state is probably the right order, since sharding without a per-account index would turn one bad linear scan into N worse ones.
- **Rewriting `Hub`'s internal maps onto a different data structure** (e.g. a concurrent map crate) as an alternative to hand-rolled sharding. Not ruled out forever, but out of scope until the simpler options are actually measured and found insufficient — no new dependency without evidence it's needed.
- **The crate's other, already-fine-grained locks** (`rest_buckets`, `anonymous_buckets`, `logon_buckets`, `Unseen`, identity's cache) — not part of the measured bottleneck, not touched here.
- **Restructuring `Hub::apply` itself** to make cross-shard writes clean. Necessary before B or C could ever be adopted for real, explicitly out of scope for this round — this ADR only goes as far as measuring whether the shape is worth that follow-on investment.

## Options Considered

### Option A (prototyped for real): `RwLock<Hub>`
| Dimension | Assessment |
|---|---|
| Complexity | Low — same critical sections, swap the primitive and the accessor's `.lock()`/`.write()` split |
| Cost | Low engineering risk: no new shard-boundary semantics, no new deadlock surface |
| Scalability | Good for read-dominated load (this venue's actual shape, see Context) — bounded by however much write time genuinely needs exclusivity |
| Team familiarity | High — `std::sync::RwLock` is a direct, well-understood substitute for `std::sync::Mutex` |

**Pros:** targets the read/write split found in the catalog directly; smallest possible correctness-risk change; no new dependency.
**Cons:** a write still briefly excludes every reader — if write volume or write hold-time ever grows relative to read volume, this stops helping; doesn't address the cache-locality cost noted when the cloud test spread load across instruments (still one shared structure, not partitioned memory), though that effect looked secondary to lock serialization itself.

### Option B (prototyped in isolation): per-instrument sharded locking
| Dimension | Assessment |
|---|---|
| Complexity | High — `Hub::apply`'s cross-instrument writes need a lock-ordering discipline or a restructured apply path; `GET /v4/markets` degrades from "one lock" to "every shard" |
| Cost | Real implementation risk on live-exchange state; `view`/`dirty`/`by_topic` would need restructuring around a shard key |
| Scalability | Best theoretical ceiling of the three — parallelizes both the read hot path and the common (single-instrument) case of `apply`/`flush_books` writes |
| Team familiarity | Low — no sharded-lock pattern exists anywhere else in this codebase yet |

**Pros:** could parallelize write throughput too, not just reads, for the common single-instrument case.
**Cons:** the uncommon cross-instrument case (event-wide release, `GET /v4/markets`) is exactly the case that determines correctness and worst-case latency; real risk of a subtle deadlock or torn-read bug in production venue state.

### Option C (prototyped in isolation): per-account sharded locking
| Dimension | Assessment |
|---|---|
| Complexity | High — same shape of complexity as B |
| Cost | Same implementation risk as B |
| Scalability | Poor fit for this venue's actual hot path — order placement/amend/cancel and market data reads are instrument-centric, not account-centric; the highest-value parallelization target (`view.instruments`/`ladders`) is instrument-keyed, not account-keyed |
| Team familiarity | Low, same as B |

**Pros:** would help a workload dominated by account/session lookups (logins, account-status polling), and `mass_cancel_result`/`result` are naturally account-scoped.
**Cons:** doesn't touch the biggest measured win (parallelizing instrument-keyed reads); `mass_cancel_result` can still span several instruments for one account, so it doesn't even fully avoid cross-shard reads within its own natural key.

## Trade-off Analysis

The core trade is the same one every lock-granularity decision makes, sharpened by what the catalog actually found: **how much of the measured contention is reader-reader (cheap to fix, Option A) versus how much would need genuine write-side parallelism to fix (expensive, Options B/C).** Given writes into `view` state come from two single dedicated background threads plus one narrow per-accepted-order write, and the benchmarked workload (and most realistic trading activity) is read-dominated, Option A is the best-justified first move — it's a same-shape, low-risk change that targets the actual measured mechanism, not a guess.

Options B and C's added ceiling only matters if Option A's numbers show write-exclusivity is still the binding constraint after removing reader-reader blocking — which is exactly what the local benchmarking below is for. Both would also reopen the deadlock-avoidance question `Hub::apply`'s cross-cutting writes currently avoid by having only one lock; that cost is worth paying only if the numbers show Option A isn't enough.

## Consequences

- **Easier:** if Option A's numbers confirm the read-dominated hypothesis, the actual fix is a small, low-risk, reviewable change — not a rewrite of `Hub`'s internals.
- **Easier:** the catalog itself is now written down (this ADR) — any future gateway capacity work starts from a real map of what's account-keyed, instrument-keyed, and cross-cutting, instead of re-deriving it.
- **Harder:** if the numbers instead show write-exclusivity dominates (e.g. `flush_books`'s every-100ms full-`dirty` sweep turns out to hold the write lock longer than expected under real load), the next step is Option B or C for real — and that means solving `Hub::apply`'s cross-shard write problem first, which is genuine new design work, not a mechanical follow-up.
- **New operational surface, if B or C is ever adopted for real:** shard count becomes a new tuning parameter (like `--reactor-threads` already is), and `GET /v4/markets`'s full-catalogue scan would need to acquire every shard — a new worst case that doesn't exist under a single lock.
- **Open, unresolved by this ADR:** `GET /v4/account`'s unindexed full-venue scan (see Non-Goals) — flagged for its own follow-up regardless of which locking strategy wins.

## Prototype Findings

Option A was implemented for real and measured against the unmodified `Mutex<Hub>` on a dedicated
**c6i.8xlarge** (32 vCPU, Xeon 8375C, otherwise idle), 8 reactor threads pinned to their own cores, the load
client confined to cores the gateway never touches (`taskset -c 9-15,25-31`). Both builds carried the audit fix
below, so the only difference between them is the lock. Options B and C were never prototyped: the Option A
result removed the reason to.

**1. The lock is not the ceiling.** Single instrument, amendments/s:

| connections | 1 | 2 | 4 | 8 | 16 | 32 | 48 | 64 | 96 | 128 |
|---|---|---|---|---|---|---|---|---|---|---|
| `Mutex<Hub>` | 7938 | 14205 | 11855 | 6761 | 3737 | 2135 | 1545 | 1184 | 835 | 611 |
| `RwLock<Hub>` + split `result()` | 7560 | 15604 | 11778 | 6785 | 3813 | 2161 | 1550 | 1192 | 824 | 615 |

From four connections upward the two are within 1–2% — indistinguishable. (An earlier ~12% reading favouring
the `RwLock` came from a shared, heavily oversubscribed dev VM whose load average swung between 2 and 500 during
testing; it did not reproduce on quiet hardware.)

**2. What the ceiling actually is: the audit log's `fsync`.** Per-thread CPU under 64 connections:

```
gateway-audit   41.0%  state D   (uninterruptible — blocked on disk)
gw-priv x8       3.0%  state S   each
```

The eight reactor threads — the only threads that ever contend for `Hub` — are ~97% idle, waiting on the audit
writer. `--audit-fsync` defaults to `true`, so the writer syncs to disk once per batch. With it off:

| connections | 2 | 8 | 32 | 64 | 128 |
|---|---|---|---|---|---|
| fsync on (amendments/s) | 15604 | 6785 | 2161 | 1192 | 615 |
| fsync off | 16736 | 18358 | 5260 | 2819 | 1438 |

2.3–2.7x at every count from 8 up, and p99 at 8 connections falls from 6.75ms to 2.04ms. This is not a proposal
to turn `fsync` off — a venue that must not trade ahead of its audit trail needs that durability — it is where
the time goes.

**3. Spreading across instruments makes no difference.** Single-instrument and 8-instrument runs are within 2–3%
at every connection count. The Context's claim that spreading measurably *hurt* (and the cache-locality
explanation offered for it) does not reproduce on a quiet host; that reading came from contaminated runs.

**4. Two confounds that invalidated the original measurements.**

- **An unbounded audit backlog** (`audit.rs`): `mpsc::sync_channel(CAPACITY)` carries `Vec<Record>` — a *batch* —
  while `CAPACITY = 65_536` was documented as "records the queue holds". The writer could therefore fall tens of
  gigabytes behind. At 64 connections the gateway went from 16MB to 2.98GB resident in seconds and stopped
  answering, rather than pushing back. Because every sweep ran 1→128 connections against one process and never
  released that memory, every point after the first blow-up — including the whole 8-instrument section, which
  always ran last — was measured against a crippled gateway. Fixed here: the per-thread batch is bounded by a new
  `BATCH_BYTES` (256KiB) and the queue holds 256 such batches, ~64MiB, after which a loop waits.
- **The load generator gives every connection the same token** (`sim/src/gwload.rs`), so all N sessions belong to
  one account and each private message fans out to all N of them — O(N²) in connections. Most of the apparent
  "collapse past 16–32 connections" is this, not a gateway limit: counted as deliveries rather than amendments,
  throughput *rises* and plateaus (~79k/s with `fsync`, ~184k/s without). A benchmark that means to measure the
  gateway should give each connection its own account.

**Conclusion.** Options A, B and C all target something that is not the bottleneck. Option A is harmless and is
kept (it is a fair description of which call sites mutate `Hub`), but it buys no measurable throughput, and B and
C would buy none either while costing real complexity. The work worth doing is on the audit write path.

## What the gateway actually does, measured cleanly

Everything above was measured with every connection on one account, which costs the gateway a copy of each
message per session of that account — so those figures describe the harness as much as the gateway. Re-run on a
dedicated c6i.8xlarge (32 vCPU, otherwise idle, 8 reactor threads pinned, client confined to cores the gateway
never touches) with one account per connection, and with the audit fixes below in place, amendments/s:

| connections | 1 | 2 | 4 | 8 | 16 | 32 | 48 | 64 | 96 | 128 | 192 | 256 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| single instrument | 8072 | 15334 | 26092 | 37599 | 51271 | 50294 | 51748 | 41770 | 50522 | 53014 | 58002 | 46187 |
| spread over 8 | 7493 | 15507 | 26027 | 36731 | 50040 | 54933 | 51193 | 42923 | 50436 | 54361 | 49549 | 52183 |

Round trip, single instrument: p50 118µs at 1 connection, 204µs at 16, 847µs at 64, 1.8ms at 128, 3.4ms at 256;
p99 154µs at 1, 5.6ms at 16, 7.1ms at 128. Resident memory 278MB at 128 connections, and no client was ever
dropped for falling behind.

The shape is a normal saturation curve, not the collapse every earlier sweep showed: near-linear to 16
connections, then a plateau of roughly **50–58k amendments/s** that holds to 256, with p50 growing linearly past
saturation as requests queue. At 128 connections that is 53,014/s against the 615/s this ADR originally
recorded — the difference being almost entirely the harness's own fan-out. Spreading across instruments still
makes no difference at any point, now for a clean reason rather than a contaminated one.

**Where the ceiling sits now.** Per-thread CPU at 128 connections:

```
gw-feed        89.4%   <- the busiest thread
gw-feed        66.8%      (the other half's)
gateway-audit  46.2%
gw-priv x8   ~40-41% each (3-5% before any of this work)
```

The reactor pool is finally working, and the audit writer is no longer dominant. The busiest thread is `gw-feed`,
which runs `Hub::apply`: one thread per half applying every feed unit to the view, in order. So this ADR's
instinct that `Hub` was implicated was directionally right and its diagnosis was wrong in a way that matters —
the cost is not contention between readers (they never contended; they were idle), it is that application of the
feed is **serial by construction**. No lock granularity reaches that. Parallelising it means parallelising the
ordered application of a sequenced feed, which is a much harder and riskier problem than anything considered
here, and nothing yet says it is worth attempting.

### What the 50k is not

It is one workload on one machine, and it should not be quoted as the gateway's capacity without these:

- **Nothing trades.** `gwload` places one resting order per connection at a tick chosen so it never fills, then
  amends its quantity back and forth. So matching, executions, the position and sleeve updates a fill causes and
  the messages they send are all absent. Fills are the obvious thing to measure next, and they cost more per
  request than an amendment that touches one order.
- **Nobody is watching the book.** `gwload` never subscribes to a market-data topic, so the conflated book
  fan-out — the thing that scales with subscribers rather than with requests — does no work at all here. A venue
  with market-data clients is a different load.
- **No cancels, no REST, one order per connection.** The mix is one request type.
- **One request in flight per connection.** Each connection waits for its reply before sending again. Clients
  that pipeline would change the shape.
- **The node is in the loop but was never profiled.** Every amendment goes to the node and back through its
  feed. Some of this ceiling may be the node's rather than the gateway's; the two were not measured apart.
- **One 8-second sample per point, no repeats.** The plateau wobbles between 41.8k and 58k, and the 64-connection
  point sits below both its neighbours. Read it as "about 50k", not as a figure to two significant digits.
- **It depends on the durability window.** The group commit below is what buys roughly half of this. If the
  answer to the open question on `COMMIT_WINDOW` is that every batch must be synced, a good part of the
  throughput goes with it.

**And the audit log is the operational constraint before the CPU is.** Measured: about **1.97 KB of audit per
amendment** (four records — what the client sent, what it was sent, and the frames derived from the feed), at
356MB written for 180,882 amendments. At 50k amendments/s that is ~98 MB/s, ~354 GB/hour, ~8.5 TB/day. A default
gp3 volume does 125 MB/s, so sustaining this rate needs provisioned storage throughput, and retention and
shipping (`interfaces/tools/audit_ship.py`) become the binding cost long before the gateway runs out of CPU.

## A venue-shaped load: trading, and being watched

> **Superseded (2026-09-27).** This section calls its load "venue-shaped". It is not. Every taker order crossed,
> so the mix was **one order to one trade**, at **one price level**, on **one book**, in **8-second** runs. A
> venue is the other way round: most of what arrives rests, is amended or cancelled, and a small fraction
> matches. The `fills/s` column is additionally inflated ~2.4× by the counter bug. Kept for the record; the
> replacement is [What a representative load says](#what-a-representative-load-says-2026-09-27).

The two things the amend measurement leaves out, added to `gwload` (`--mode trade`, `--watchers`) and run on the
same class of machine. Makers rest a large sell and amend it back to size; takers cross it immediate-or-cancel,
each against a different account, so these are real fills. Watchers subscribe to the book on the public half and
only read.

| connections | 4 | 8 | 16 | 32 | 64 | 128 | 256 |
|---|---|---|---|---|---|---|---|
| trading, requests/s | 15073 | 21444 | 24951 | 17863 | 27382 | 20729 | 18943 |
| trading, fills/s | 26520 | 38212 | 44547 | 31353 | 48351 | 38844 | 35434 |
| trading, p50 | 237µs | 296µs | 535µs | 937µs | 1.95ms | 4.16ms | 9.25ms |
| amend-only, requests/s | 21809 | 35804 | 49339 | 52098 | 44536 | 53121 | 58043 |
| amend-only, p50 | 126µs | 149µs | 207µs | 426µs | 836µs | 1.78ms | 3.56ms |

**Trading costs roughly half the request rate of amending, and two to three times the latency.** Where the
amend-only load plateaus near 50–58k requests/s, a trading load plateaus around **20–27k requests/s** — while
doing **35–48k fills/s**, which is the work the amend measurement never had to do. At 128 connections p50 is
4.2ms trading against 1.8ms amending.

**Market data is close to free at this scale.** With 32 trading connections held constant:

| watchers | 0 | 32 | 128 | 256 |
|---|---|---|---|---|
| trading requests/s | 23650 | 15042 | 25212 | 21049 |
| messages to watchers/s | — | 282 | 1263 | 2217 |

Watcher traffic rises linearly and trading throughput shows no trend against it (the spread is run-to-run noise;
see the caveats above). That is conflation doing its job: a book topic carries at most ten snapshots a second,
so a subscriber costs ~10 messages/s no matter how hard the book is being traded. Making market data expensive
takes tens of thousands of subscribers, not hundreds — a different question from order-entry capacity, and one
this shape of test can now ask.

**Two things this run turned up on its own.**

- **The slow-consumer disconnect fired for the first time** (`too_far_behind=1`). A maker taking thousands of
  executions a second is a genuine slow consumer of its own fills. The outbox bound added earlier was written as
  unproven hardening against a path nothing had exercised; this exercised it, and it did what it was for.
- **The engine's `small` profile fail-stops under real order flow.** `duplicates` (client order IDs, which never
  evict) is 262,144 entries and its own comment sizes it for "about thirty-six new orders a second". The amend
  load places one order per connection and then only amends, so it never approached that; a trading load mints a
  fresh order per taker request and exhausted the table in about 25 seconds — `engine fail-stop Capacity at
  input 348086 (spec §18)`, exactly as specified.

  **This is a rate limit, not a memory figure, and no profile fixes it.** A group takes
  `duplicates / window` new orders a second: at the two-hour default that is ~36/s on `small`, ~583/s on
  `launch` and ~2,330/s on `scale`. Twenty thousand orders a second at a two-hour window would want 144 million
  entries, an order of magnitude past `scale`. The window is the half a deployment chooses, and E-7.4 accepts a
  shorter one. So `Catalogue::seed_with` now takes the window to found a group on and `venue-sim seed` derives
  it from `--orders-per-second` (or `--id-window-seconds` outright), leaving the default untouched. Verified: a
  30-second trading load on `--capacity small` seeded for 20k orders/s ran 516,022 requests and 878,234 fills
  with no fail-stop. **Anyone sizing a real group has to pick the window against the order rate it will see**,
  and that is a compliance question as much as an engineering one, since the window is how long a duplicate
  client order ID is detected (D-12).

The whole sweep wrote **12.9 GB** of audit log in about 200 seconds of load.

### Run twice: how repeatable any of this is

The same sweep again on a second machine, this time on `--capacity small` founded on a 6-second client order ID
window (`--orders-per-second 40000`) rather than `scale`. It completed with **no fail-stop and the node up at
every one of the eighteen points**, which is the sizing fix holding over ~3.4M orders rather than one 30-second
run. Requests/s, first run against second:

| connections | 4 | 8 | 16 | 32 | 64 | 128 | 256 |
|---|---|---|---|---|---|---|---|
| trading, `scale` | 15073 | 21444 | 24951 | 17863 | 27382 | 20729 | 18943 |
| trading, `small` | 15990 | 20876 | 25422 | 20581 | 27706 | 21319 | 19569 |
| amend-only, `scale` | 21809 | 35804 | 49339 | 52098 | 44536 | 53121 | 58043 |
| amend-only, `small` | 21320 | 36715 | 50214 | 57212 | 41522 | 52155 | 62098 |

**Most points repeat within about 3%**, so the earlier caveat that these wobble by a third was too pessimistic:
the spread is real but concentrated. Two other things reproduce exactly. The watcher rates are identical to
within a message a second (282/1263/2217 against 293/1263/2229) — unsurprising once conflation is doing the
pacing, but it confirms the market-data result is not luck. And `too_far_behind` is 1 in both runs: the same
maker-as-slow-consumer disconnect, in the same place.

**The dip at 32 connections and the peak at 64 are in both runs**, on different machines. That makes them a
property of the system rather than noise. Nothing here explains them, and nothing yet depends on the answer.

**The capacity profile makes no difference to throughput.** `small` matches or slightly beats `scale` at every
point, so the larger tables buy nothing for a workload that fits in either — worth knowing before anyone
reaches for a bigger profile expecting speed rather than headroom.

## What applying a unit actually costs, and where it goes

`venue_gateway_feed_apply_duration_seconds` now measures this rather than leaving it to be inferred from `top`.
On a trading load, both halves applying the same 222,325 units: **private 36.0µs a unit** (p50 24µs, p90 ~78µs,
p99 ~250µs), **public 19.9µs**. The public half has no subscribers there, so the ~16µs between them is building
and sending an account's own messages, and the ~20µs both pay is applying the unit to the view. The mean sitting
half again above the median is a tail — a fill against many resting orders, a tick releasing a whole event — and
that tail is what a lag spike is made of. A further **~6µs a unit** goes on the feed thread *outside* `apply`,
decoding each SBE frame into a `serde_json::Value` in `read_units` before `apply` is ever called.

**Profiled** (`perf record` on the busier `gw-feed` thread, 39,426 samples, 64 trading connections,
`--profile perf` with fat LTO and `target-cpu=native`), self time:

| | share | |
|---|---|---|
| `_int_malloc` / `malloc` / `_int_free` / `cfree` / `realloc` | **42%** | allocating and freeing |
| `__memcmp_evex_movbe` | 12% | comparing JSON object keys |
| `__memmove_evex_unaligned_erms` | 4% | copying |
| `BTreeMap::insert`, `String::write_str`, `serde_json` serialize, `drop_in_place<Value>` | ~18% | building and tearing down `Value` trees |
| `View::apply`, `Hub::apply`, `executions`, `send_to_account` | **~8%** | the venue state change itself |

By object: **60.6% libc, 35.7% venue-gateway**.

**So roughly nine tenths of the feed thread goes on the JSON representation and one tenth on the work.** The
feed arrives as compact binary SBE, is inflated into `serde_json::Value` trees, walked by string key — that 12%
in `memcmp` is comparing `"msg"`, `"orderId"`, `"accountId"` inside `BTreeMap<String, Value>` — and then
serialised back into JSON strings for clients. The allocator churn that produces is the single largest line in
the profile.

Two things follow, in increasing order of cost:

- **A different allocator — done, and it is worth about a third.** 42% of the thread was in glibc
  `malloc`/`free`, whose arena design suits many small short-lived allocations poorly. `mimalloc` as the global
  allocator is one line and changes no semantics. Measured on an idle c6i.8xlarge, the same tree with and
  without it, over ~2.5M units each: **private apply 28.2µs → 20.3µs (−28%), public 15.0µs → 10.5µs (−30%)**.
  Throughput follows on the load it was aimed at — trading at 32 connections 30,986 → 36,387 requests/s, at 128
  connections 21,360 → 35,769 — while the amend-only load, cheaper per unit and bounded elsewhere, is a wash.
  97/97 tests pass. One trade point (64 connections) was lost to a client reset mid-run, so that row is missing
  rather than bad.
- **The structural answer is not to materialise `Value` on the feed path at all.** The journal is SBE and the
  view wants typed fields; going binary → `Value` → string-keyed walk → typed state is overhead the format does
  not require. JSON is needed at the client boundary, not for the gateway's own bookkeeping. That is a real
  piece of work and nothing yet says the ceiling is close enough to justify it — but it is where the 42µs is,
  and no amount of lock or profile tuning reaches it. The allocator above buys back a third of it without
  touching the shape; the remaining two thirds need the shape changed.

### Taking `Value` off the feed path: measured

> **Retracted (2026-09-27). The −29% / −25% below does not reproduce, and it was measured on a bugged tree.**
> `f16227f` (19:37) introduced an allow-list that silently dropped `OrderAmended`; this measurement ran
> 19:41–19:54 and was recorded at 19:55; the bug was fixed at 20:11 in `b1d2d1e`. For the whole measurement
> window the gateway was not building or sending amend executions at all, so on an amend-heavy sweep it was
> skipping work rather than doing it faster.
>
> Re-measured against `ea4724c` (the commit before this work) with both trees free of that bug, n=3 interleaved,
> the same apply cost comes out **−1% public / −9% private** on the old sweep, and **−0.1% public / +3.3%
> private** on a representative load — that is, no effect. See [What a representative load
> says](#what-a-representative-load-says-2026-09-27).

Done in two steps and measured against the commit before them, same machine, both trees built and run back to
back. `Unit` now carries the SBE bytes (`wire::Frame`) with the JSON shape decoded only if something asks and
kept once it has been; `View` reads `OrderAccepted`, `Fill` and `BookLevelChanged` straight out of the generated
decoders; and every reader of a `BookLevelChanged` — the hub's loop, `executions`' input filter, three template
scans and the view's applied-command check — now takes it typed, so that result is applied without being
decoded at all.

Both runs applied within 0.1% of the same number of units (2,363,862 against 2,366,023), which makes
`feed_apply` a controlled comparison rather than an impression:

| | before | after | |
|---|---|---|---|
| public apply | 9.94µs | **7.04µs** | −29% |
| private apply | 18.74µs | **14.09µs** | −25% |
| busier `gw-feed` thread | 95.4% | **77.0%** | no longer near saturation |

The thread profile is the corroboration: the feed thread was at 95.4% and is now at 77.0%, which is the same
result seen a second way and the headroom the ceiling was short of.

Throughput is a weaker signal here and is reported as such. Amend-only improved consistently (+18% at 8
connections, +5% at 32, +9% at 128); the trading numbers moved the other way at 32 connections and one base
point did not complete at all. Those are single ten-second samples of the metric that has varied by a third
run to run all through this ADR, against a `feed_apply` figure drawn from 2.36M samples. Read the apply cost,
not the trade rate.

**What is left.** `Fill` and `OrderAccepted` still decode, because both are read by `executions`, which carries
state across its arms, serves the node's replies as well as the feed, and maps the cash and fee fields. That is
worth doing on its own terms, with a differential harness in place first, rather than folded into this.

**A faster JSON parser would not help here, and it is worth writing down why.** The obvious reach is for
something like simdjson, but there is no JSON *text* to parse anywhere on this path: `codec::decode`
(`harness/src/codec.rs`) reads SBE binary and *constructs* a `serde_json::Value` field by field, and neither
`wire.rs` nor the codec calls `from_str` or `from_slice` at all. The cost is Value **construction**, **string-keyed
access** (that 12% in `memcmp` is comparing `"msg"` and `"orderId"` inside a `BTreeMap<String, Value>`) and
**serialisation** — none of which a parser addresses. Where the gateway does parse client JSON is `ws.rs:221`
and `rest.rs:85`, on the reactor threads, which sat at ~41% while the feed thread saturated; a faster parser
could shave those, and there is a redundant re-parse of each outbound frame in `reactor.rs:808` and `:822` worth
removing first, but neither is the constraint today.

## What a representative load says (2026-09-27)

Everything above measures a load that does not look like a venue, and two of its numbers came from broken
counting. This section is what replaces them.

### What was wrong with the load

`gwload --mode trade` had makers resting at one tick and takers sending immediate-or-cancel orders at that same
tick, so **every taker order crossed**: one order to one trade, at a single price level, on a single book,
measured in 8–10 second runs. Real order flow is the other way round — the great majority of what reaches an
exchange rests, is amended or is cancelled, and only a fraction matches.

That is not a cosmetic difference for this ADR, because the results a message produces differ by kind. A trade
is two `Fill`s, two accounts, their positions and their sleeves; a resting order is an `OrderAccepted` and a
`BookLevelChanged`. The whole argument here is about which templates the feed path decodes, so a load that
over-weights fills by an order of magnitude is measuring the wrong mixture of exactly the thing in question.

`--orders-per-trade N` (default 10) now sends N−1 post-only buys a tick or three behind the ask, each cancelled
before the next, then one crossing order. Books come from `--instruments 8`, whose exponential weighting
(`2^32 >> rank`, halving per rank) already existed and simply had never been used for a trading run. The
achieved ratio is counted and printed rather than assumed — eight connections over eight books give 1:20, not
1:10, because the unpopular books get takers and no maker.

### Two counting bugs, both fixed

- **`fills` counted every execution kind.** `result.executions` carries `accepted`, `cancelled` and `amended`
  as well as `fill`, and the harness took the array's length: 189,264 "fills" reported for 80,233 taker orders
  that can each match at most once. The true figure was 78,557. **Every `fills/s` number above this section is
  about 2.4× too high.** Fixed in `2adfa71`.
- **`MAKER_SIZE` was 1,000,000 contracts**, reserving about half a load account's 100,000,000 cents, so a maker
  re-posting hit `insufficient_sleeve` partway through a minute. Now 10,000. Fixed in `a1eaf16`.

Neither was reachable in an 8-second run. Nor were the gateway's slow-consumer disconnects, which used to fail
a whole run through one `?` and are now counted and reported.

### Capacity

c6i.8xlarge (32 vCPU), 8 pinned reactor threads, one account per connection, 8 books, ~10 orders per trade,
**60-second** runs, three replicates:

| connections | messages/s | p50 | fills/s | achieved ratio |
|---|---|---|---|---|
| 32 | **50,645 ±894** | 489µs | 1,373 | 1:13 |
| 128 | **56,947 ±665** | 1,999µs | 2,098 | 1:10 |

No dropped connections, no fail-stops. Audit writes ~105 MB/s, under a default gp3 volume's 125 MB/s but not by
much — it remains the operational limit that arrives before CPU does.

**Run length matters as much as shape.** The identical load gives 38.5k over ten seconds and 27.8k over seventy.
A third of the short-run figure does not survive going the distance. Quote the 60-second numbers.

### The stages 2+3 question, settled

With a representative load, the tree carrying `Frame` and the typed readers and the tree before them are
indistinguishable. Same box, same load generator and node, only the gateway binary swapped, n=3 interleaved:

| | pre-stage `ea4724c` | `74dbc71` | delta |
|---|---|---|---|
| 32 conns | 49,209 ±1124 | 50,645 ±894 | +2.9% |
| 128 conns | 57,904 ±492 | 56,947 ±665 | −1.7% |
| feed_apply public | 6.34µs | 6.33µs | −0.1% |
| feed_apply private | 13.33µs | 13.77µs | +3.3% |

Within ±3% everywhere.

This matters because the unrepresentative load said otherwise, loudly, and nearly caused a revert. On the 1:1
single-book load the same comparison showed **−25.1% at 32 trading connections**, a bisect attributed it to
`0bf4a7b` (the lazy `Frame` itself rather than the typed readers), and a whole-process profile confirmed the
cost was real: `feed_apply` private +11.4% and public +26.7%, appearing as +12% p50 latency rather than CPU
saturation, with `Hub::apply` self time going 0.78% → 2.18%. All of that was reproducible and all of it was an
artifact of the load. The lazy `Frame` only pays for itself where a reader never asks for the `Value`, and on a
1:1 fill-dominated load almost every frame is asked for; on a realistic mix enough of them are not.

The lesson worth keeping is procedural: **before drawing a conclusion from a load test, ask what mix of results
it puts on the feed.** Three separate wrong conclusions here — that stages 2+3 were worth 25%, that they cost
25%, and that the answer was to revert — all came from not asking.

### What this leaves

`74dbc71` removes a genuine double read (`executions_of` built a `Value` for `Fill` and `OrderAccepted`, which
`View::result_typed` then re-read from the bytes). It is correct and slightly reduces variance, but it does not
move throughput measurably, and it was committed with a rationale these numbers do not support.

Nothing in the `Value`-removal line has been shown to pay for itself on a representative load, and nothing has
been shown to cost anything either. It is neutral. Finishing it — typed `executions` and typed `View::result`,
so no reader builds a `Value` at all — remains the only version of it with a case, and that case is now
unproven rather than established.

**Raw data.** `docs/adr/measurements/`: six 60-second replicates (`real-*.txt`), the earlier 10-second rounds
(`rep-*.txt`), both whole-process profiles (`prof-*.txt`), the full connection ladders (`results-*.txt`), and
the scripts that produced and aggregated them.

## The lock does matter — on the write side, with customers on the venue (2026-09-27)

This ADR is Rejected because swapping `Mutex<Hub>` for `RwLock<Hub>` changed throughput by 1–2%. That holds.
What it does not establish, and what the wording above implies, is that *the lock* does not matter. It does.
Every run behind the rejection had no market-data subscribers, and two of the three things that hold the hub's
write lock are proportional to work those runs never asked for.

### How the question came up

A venue's population is a handful of market makers carrying the order flow and a large number of customers who
watch and trade rarely. Nothing here could express that: every load had every connection trading, and the most
ever run was 256 connections. Building it (`--retail`, `2adfa71` onward) turned up a 28% trading cost at 16,384
customers and p99 past 800ms **while the machine sat at 16% CPU**. Idle cores and rising latency is the shape of
waiting, not of work.

Two attempts to name the cause from reading the code were wrong before one was right, so the answer came from
two histograms instead: `venue_gateway_conflate_hold_duration_seconds` (how long a flush holds the write lock)
and `venue_gateway_hub_read_wait_duration_seconds` (how long a request waited for the read lock).

### What they said

Before/after deltas over single 60-second runs, 8 books, 32 trading connections:

| | no subscribers | 16,384 subscribers |
|---|---|---|
| conflate flush, write-lock hold | 6.7µs | **122,473µs** |
| feed-apply, private (write lock) | 13.5µs × 2.8M = **63% of wall** | 16.7µs × 2.3M = **65% of wall** |
| request wait for the read lock | 36.5µs | 49.0µs |

Two separate findings, and only the second was the one being looked for.

**`Hub::apply` holds the write lock about two thirds of every second, with no subscribers at all.** It is called
per feed unit on the one thread per half that applies them, and at these rates that is most of the time. This is
what the reactor threads' 36–50µs read waits queue behind, and it is the honest answer to why order latency is
what it is. Nothing in this ADR's options would have touched it: `RwLock` does not help when the contention is a
writer, and sharding a lock does not help when one holder wants most of it.

**The conflate flush grows from 6.7µs to 122ms as subscribers arrive.** `flush_books` copied each book message
once per subscriber — `message.to_string()` per session in `send_to_topic` — inside the write lock. At 16,384
subscribers that is ~131,000 allocate-and-copy operations per flush. The cycle is 100ms, so flushes could not
keep up: 276 in 60 seconds, **4.6/s where the interface promises 10**. Customers were not merely waiting; they
were getting market data at under half the advertised rate, which is why per-client delivery fell as the
population grew. That was read as the gateway saturating. It was the publisher falling behind.

### The fix, and what it was worth

Neither sharding nor a different lock type. Two changes (`da43b0a`):

- **Build each message once.** `Queued.text` is an `Arc<str>`, built once per topic and handed out by reference.
  The copy into a socket's buffer still happens, but on whichever reactor thread serves that session rather than
  on the one thread holding the lock. Every fan-out does this now — executions to an account's sessions, parlay
  deliveries, transfers.
- **Split the flush.** `take_dirty` mutates and is microseconds; `publish_books` only reads, so it runs under a
  read lock while the feed thread and every request carry on. `take_dirty` returns the view's sequence with the
  levels so a message still carries the sequence its levels are as of — the fan-out now happens after the write
  lock is dropped, and a later sequence would tell a client it holds changes it was never sent.

At 16,384 customers:

| | before | after |
|---|---|---|
| conflate write-lock hold | 122,473µs | **9.7µs** |
| fan-out, now under a read lock | — | 41,017µs |
| flushes per 60s | 276 (4.6/s) | **435 (7.3/s)** |
| retail order p50 | 13.9ms | **6.3ms** |

Four orders of magnitude off the exclusive hold. Conflation recovers from 4.6 Hz toward the promised 10, and a
customer's order comes back in less than half the time.

### The load this is measured on

Two corrections were needed before any of it meant anything.

**The generator had to stop being the bottleneck.** A thread per customer costs a wake-up per customer per poll
interval: at 24,576 customers the client reached a load average of 6,465 on 32 vCPU while the venue it was
measuring sat at 16%, and per-customer delivery fell — the harness starving, read as the gateway saturating.
Putting the client on its own machine did not help, because threads were the wrong axis. `retail::run`
(`21b9beb`) gives a few workers thousands of sockets each through epoll; at 16,384 customers client load went
from 1,947 to 8.0 and the measured trading rate rose from 20,763 to 34,922 for the same load. Everything the
earlier run attributed to the gateway at that size was the client.

**And retail had to stop subscribing to everything.** Each customer was subscribed to every book on the venue,
which is an API client's shape, not an app user's. Since fan-out costs one message per subscriber per topic,
that overstated retail's cost by roughly the number of books. `--retail-books` (default 2, `891998b`) draws each
customer's books with the same exponential weighting the connections use; `--watchers` still takes all of them
and is the API-customer shape. Running both gives the mix.

### Capacity with customers on the venue

c6i.8xlarge server, separate c6i.8xlarge client, 32 trading connections, 8 API clients on all books, retail on
two weighted books each, 60-second runs:

| customers | trading msg/s | market data out | per customer | retail order p50 |
|---|---|---|---|---|
| 0 | 47,325 | — | — | — |
| 1,024 | 44,455 (−6%) | 14,989/s | 881 | 3.1ms |
| 4,096 | 41,502 (−12%) | 53,750/s | 792 | 4.4ms |
| 16,384 | **36,190 (−24%)** | 174,757/s | 650 | 6.3ms |

No disconnects at any size; 1.1GB resident at 16,384; the client had headroom throughout. **16,384 concurrent
customers cost about a quarter of the trading rate**, and the gateway was not the limit — one client machine runs
out of ephemeral ports near 30,000 connections before the venue runs out of anything.

### What is still open

- **`Hub::apply`'s write lock, two thirds of every second.** Untouched, and now the largest known source of
  order latency. Whatever is done about it, the answer is not this ADR's options.
- **The fan-out is still 41ms a flush.** Off the critical section, but the reason conflation runs at 7.3 Hz
  rather than 10. Reaching 10 means a cheaper per-recipient send or spreading it across threads.
- **Whether market makers can opt out of conflation.** Ten snapshots a second is fine for an app and not for a
  maker. Trades are already unconflated (`hub.rs:712`, sent from `apply`); only the book is batched. The options
  are a faster tier for some credentials, an unconflated book subscription, or letting professional flow follow
  the SBE feed directly. That is a client-contract change and belongs in its own ADR — but it now has a cost
  basis, and the `Arc<str>` change makes a faster tier considerably cheaper than it would have been.

## Action Items

1. [x] Implement `RwLock<Hub>` (Option A) for real, verify it compiles clean and the full test suite passes — done, 97/97.
2. [x] Benchmark Option A against the unmodified `Mutex<Hub>` baseline on the same hardware — done, on a dedicated c6i.8xlarge. No measurable difference.
3. [x] ~~Build an isolated synthetic prototype for Options B and C~~ — dropped. Option A's result shows the lock is not the bottleneck, so modelling a finer-grained version of it answers nothing.
4. [x] Record real numbers in Prototype Findings — done.
5. [ ] **Fix the audit backlog bound** (`audit.rs`): `sync_channel` holds batches, not records. Done in this branch; needs review.
6. [x] **Group-commit the audit log** — prototyped and measured on a c6i.8xlarge. The writer used to end a drain
   as soon as the queue ran dry, so the sync rate followed producer burstiness rather than `BATCH`. It now ends a
   drain on `BATCH` (raised 1024 → 8192) or `COMMIT_WINDOW` (10ms), flushing to the kernel meanwhile so a killed
   process still loses nothing. Amendments/s: 6826 → 13795 at 8 connections, 1215 → 2162 at 64, 631 → 1095 at
   128 — 1.7–2.0x, about 80% of what turning `fsync` off would give, while keeping durability. The writer thread
   moves from 41.6% CPU blocked on disk to 75.8% running.
7. [ ] **Confirm the durability window** — group commit widens what a *machine* losing power can lose from one
   drain to `COMMIT_WINDOW` (10ms). A killed process still loses nothing. This needs whoever owns the regulatory
   requirement to accept 10ms, or name the number they want; it is not Engineering's call alone.
8. [x] **Move record formatting off the writer thread** — done. A line is `{head}{fields},"prev":"…"}`, and only
   head and tail depend on where the record lands, so the fields (masking, escaping, per-item digests — the part
   that reads the whole message) are now rendered in `Audit::record` on the thread that took the record. The
   writer keeps what only it can know: the number, the chain, the SHA-256 of the line, and the write. Output is
   byte-identical; `interfaces/tools/audit_verify.py` verifies a 1,089,957-record file written by it. A further
   +5–27% (8 conns: 14153 → 18007 amendments/s; 128: 1124 → 1268), the writer thread 75.6% → 47.1% CPU and the
   reactor threads 5.0% → 11.4% each, and resident memory over the sweep 318MB → 106MB.

   Cumulatively, from the committed baseline before any of this: **2.0–2.6x** (8 conns 6826 → 18007, 32 conns
   2119 → 4791, 64 conns 1215 → 2342, 128 conns 631 → 1268).
9. [x] **Give `venue-sim gwload` one account per connection** — done. A gateway only knows the dev tokens its
   catalogue gave it at startup, so `venue-sim catalogue --accounts N` writes a catalogue with those accounts on
   the end (seeding funds them like any other), and `gwload --token-prefix` gives connection `i` the token
   `{prefix}{i}`. Sharing one account now warns, naming the flag. The difference is most of what the sweeps past
   32 connections were measuring — at 64 connections, same gateway process: **2,900 → 38,412 amendments/s
   (13.2x)**, p50 16.3ms → 1.4ms, p99 86ms → 3.8ms. Every earlier number in this ADR above ~8 connections is
   therefore a measurement of the harness's fan-out as much as of the gateway, and should not be read as a
   gateway capacity figure.
10. [ ] File a separate follow-up for `GET /v4/account`'s unindexed scan (Non-Goals) — not resolved here.
11. [ ] Consider whether `Hub::deliver`'s fan-out to every session of one account needs attention on its own merits — only if many concurrent sessions per account is a real client pattern, which nobody has established.
12. [x] **Make the trading load representative** — done (`2adfa71`, `a1eaf16`). Every taker order used to cross,
    at one price level on one book, in 8-second runs. `--orders-per-trade` (default 10) now rests and cancels
    most orders near the touch, `--instruments 8` spreads them over books with the exponential popularity that
    already existed, and runs are 60 seconds. Two harness bugs only a minute-long run reached: `fills` counted
    every execution kind (so every `fills/s` figure above is ~2.4× too high) and `MAKER_SIZE` exhausted an
    account's sleeve mid-run. A dropped connection no longer fails the whole run.
13. [x] **Re-measure stages 2+3 on that load** — done. Pre-stage and HEAD are within ±3% on throughput and on
    apply cost: the change is neutral, not the −29%/−25% win this ADR claimed and not the −25% loss the
    unrepresentative load showed. The proposal to revert `0bf4a7b..HEAD` is withdrawn.
14. [ ] **Decide whether to finish or abandon the `Value` removal.** Half-done is measurably neutral, so there
    is no urgency either way. Finishing means typed `executions` (its differential harness is
    `hub::tests::executions_agrees_however_the_results_are_read`) and typed `View::result`, so that no reader
    builds a `Value`; abandoning means removing `Frame`, the typed readers and the harness. What is not
    defensible is leaving it half-done *and* believing it bought something.
15. [x] **Find out what a venue-sized customer population costs** — done. `--retail` models customers who watch
    and trade rarely, multiplexed over epoll workers so the generator is not the bottleneck, on their own
    machine. 16,384 concurrent customers cost about a quarter of the trading rate, with no disconnects; the
    ceiling found was the client's ephemeral ports near 30,000 connections, not the gateway.
16. [x] **Measure what holds the hub's locks** — done, with two histograms rather than a third guess from
    reading the code. `Hub::apply` holds the write lock about two thirds of every second with no subscribers
    at all; the conflate flush grew from 6.7us to 122ms at 16,384 subscribers.
17. [x] **Take the fan-out off the write lock and stop copying per subscriber** — done (`da43b0a`). Exclusive
    hold 122ms -> 9.7us, conflation 4.6Hz -> 7.3Hz, retail order p50 13.9ms -> 6.3ms.
18. [ ] **`Hub::apply`'s write lock.** Two thirds of every second, untouched, and now the largest known source
    of order latency. Needs its own investigation; neither Option A nor B/C addresses a single writer that
    wants the lock most of the time.
19. [ ] **The remaining 41ms fan-out.** Under a read lock now, so it excludes nobody, but it is why conflation
    runs at 7.3Hz rather than the 10 the interface promises.
20. [ ] **Decide whether market makers can opt out of conflation.** Ten snapshots a second suits an app and not
    a maker. Trades are already unconflated; only the book is batched. A faster tier per credential, an
    unconflated subscription, or letting professional flow follow the SBE feed directly — a client-contract
    change, so its own ADR.
21. [ ] **Re-measure the amend-only and market-data sections on 60-second runs.** Only the trading load was
    redone. The amend ladder, the watcher table and the audit-per-amendment figure are all still 8-second
    single samples, and the trading redo showed short runs overstate by about a third.
