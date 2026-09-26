# ADR-0002: Replacing the gateway's global `Mutex<Hub>`

**Status:** Rejected — the premise did not survive measurement. See Prototype Findings.
**Date:** 2026-09-26
**Deciders:** core/gateway owners (Engineering)

> **Retraction (2026-09-26, after this ADR was drafted).** The Context below argues that `Mutex<Hub>` is the
> gateway's throughput ceiling. Measured on dedicated hardware, it is not: swapping it for an `RwLock` changes
> throughput by 1–2%, which is noise, and the reactor threads that would contend for it sit ~97% idle. The ceiling
> is the audit log's `fsync`. The Context is kept as written because the state catalogue in it is still accurate
> and useful, but its conclusion — and the decision that followed from it — are wrong. Read Prototype Findings
> first.

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
9. [ ] **Give `venue-sim gwload` one account per connection**, so a gateway benchmark measures the gateway and not an O(N²) fan-out the harness created.
10. [ ] File a separate follow-up for `GET /v4/account`'s unindexed scan (Non-Goals) — not resolved here.
11. [ ] Consider whether `Hub::deliver`'s fan-out to every session of one account needs attention on its own merits — only if many concurrent sessions per account is a real client pattern, which nobody has established.
