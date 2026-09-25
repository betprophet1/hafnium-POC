# ADR-0001: Tiered, space-time-tradeoff representation for order book levels

**Status:** Proposed
**Date:** 2026-09-25
**Deciders:** core/engine owners (Engineering)

## Context

`core/engine`'s `Capacities` (`core/engine/src/state.rs`) presizes every table a sequencer group needs — `instruments`, `orders`, `levels`, `accounts`, etc. — once at process start, and never grows them while running: "the engine never allocates" (`state.rs:1-4`). This is deliberate: no allocation on the hot path, deterministic replay (no allocator-timing dependence), and a clean fail-stop story (a full table is a sizing failure, decided identically on every replica, never a silent resize).

`ListInstrument` (`core/engine/src/commands.rs:387-417`) reserves `2 * per_side` slots from the shared `levels` pool for every listed book, where `per_side` is that book's tick-table width — and holds that reservation for the book's entire lifetime, regardless of whether any order ever rests there. `levels` is one flat array shared across every book in the group (`state.rs:380`); a book's `levels_start` is just an offset into it.

This was measured directly. Building `venue-node` with the `perf` profile and driving it with `venue-sim workload` (a direct-to-core order-flow generator, `core/sim/src/workload.rs`) against the `small` capacity profile (`levels: 131_072`) and the workload's default 99-tick cent tick table:

- 600–660 concurrent books: sustained fine, ~100,000–117,000 inputs/sec.
- 665+ books: `engine fail-stop Capacity` (spec §18), always at the same journal position (the 661st `ListInstrument`), regardless of `--accounts`/`--makers` or how far past 665 the target book count was.

The arithmetic confirms the mechanism: `131,072 ÷ (2 × 99 ticks) ≈ 661.98` — the `levels` pool, not the documented `instruments: 1_024` ceiling, is what actually limits book count for this tick width. `instruments` still had ~360 slots free when the run died.

This is corroborated at the spec level, not just measured: `spec/engine/decisions.md` D-30 already names the identical mechanism with a different example — *"a book reserves two levels per tick-table row whether it uses them or not, so on a node with 131,072 levels a parlay ladder of 279 rows uses up levels long before instruments run out"* — and gives `maxPriceLevels` as the only existing mitigation (a hard per-group cap, not a representation change). D-30's own rationale points at exactly this ADR's mechanism without proposing it: *"an implementation with a bump-allocated level pool satisfies [freeing storage on release] by compacting it when a listing does not fit at its end"* — i.e., the spec already anticipates that the level pool's storage strategy is an implementation detail with room to change, so long as capacity accounting and fail-stop behavior stay a deterministic function of the journal.

This directly contradicts the venue's own stated requirement (`blueprint/prediction-exchange.html`, §1): *"Power-law volume over a huge instrument count... the marginal cost of a listed instrument must approach zero, forcing a tiered activity model (§10)."* Blueprint §10 sketches a HOT/WARM/COLD/DORMANT tiering scheme with resident-memory budgets (~2 MB / ~64 KB / ~8 KB / 0, for ≤50 / ≤2,000 / ~100,000 / unbounded markets respectively) and a promotion/demotion behavior ("demotion is lazy and cheap; promotion must be instant... within one sequencer event"), but **no data structure or trigger rule is specified, and none of it is implemented** — `grep -r "tier\|dormant\|DORMANT" core/engine/src core/node/src` returns nothing. Every book in the current engine runs permanently as if it were HOT.

Separately, the per-book `Book`-shape metadata itself (aggregate qty + FIFO head/tail per tick + occupancy bitmap; blueprint's illustrative struct, `[u32;100]×3 + u128×2 + u8 + u64`) costs **~1.25 KiB fixed per book** regardless of activity — this is the "shape of the book" cost that scales with tick-width × book-count and that this ADR targets. It is distinct from order storage: individual orders already live in one shared, elastic slab (`core/engine/src/state.rs`'s `orders` pool) and cost nothing per book beyond what's actually resting — a level with 10,000 resting orders costs the same fixed level-slot as one with zero.

## Decision

Introduce a **per-book tiered representation for the `levels`/`level_bits` structures** (not for `orders`, see Non-Goals):

- **Dense** (today's mechanism, unchanged): a contiguous slice of the shared `levels`/`level_bits` pools, one slot per tick per side. O(1) best-price via bitmap count-leading-zeros, no scan. Reserved for genuinely active books.
- **Sparse/inline**: a small, fixed-capacity array of `(tick, qty, head, tail)` tuples held directly with the book's own record — sized for however many ticks a quiet book actually needs (a handful, not its full tick-table width). No interaction with any shared pool as long as it fits.
- **Overflow** (only if sparse capacity is exceeded without promoting to dense): extra occupied-tick entries borrow a slot from a new, separate, much-smaller shared pool — elastic like `orders` today, not a per-book reservation.

**Tier assignment**, evaluated only from state already available to the engine (no wall-clock reads, consistent with the "deterministic function of the journal" invariant, blueprint line 657):

- **Promotion** (sparse → dense): triggered synchronously by the touching command itself (an order arriving is already a sequenced event, so the check is free), the instant `occupied_ticks` would exceed **`PROMOTE_ABOVE = 32`** — never by a scan over other books.
- **Demotion** (dense → sparse): eligible once `occupied_ticks ≤ DEMOTE_BELOW = 16` **and** `current_tick − last_touched_tick > D_idle` — both conditions, not either, so a book doesn't flap between representations from ordinary quoting activity around the boundary.
- **No top-K/ranking step.** An earlier version of this proposal considered a comparative "HOT is the busiest ≤50 books" rule requiring periodic re-ranking across all instruments. Rejected: re-ranking cost scales with instrument count, which is exactly the cost this ADR is trying to bound, and it would need to run on a busy-spinning matching thread that must stay under ~40% peak load (blueprint §10). Threshold-based, single-book-local classification avoids ever comparing one book to another.

### Pinned thresholds

**`PROMOTE_ABOVE` / `DEMOTE_BELOW` (occupied-tick count) — benchmark-grounded, not a placeholder.** The tiered-book prototype (`core/prototypes/tieredbook`) measured `best_below` cost as a function of occupied-tick count directly (not as a ratio to tick-table width — cost tracks the count of occupied ticks, independent of how wide the book's own ladder is, so the original framing above of a *density ratio* was wrong and is corrected here): at `INLINE=8`, 8 occupied costs ~7.5 ns, 16 costs ~14.6 ns, 32 costs ~31.7 ns, 64 costs ~68 ns. Setting `PROMOTE_ABOVE = 32` keeps sparse-path lookup cost under ~32 ns — **under 0.5% of the 7–15µs matching budget** — with real margin before it would start to matter, rather than waiting until cost is already material. `DEMOTE_BELOW = 16` gives a 2x hysteresis gap below that, so a book oscillating between 20–30 occupied ticks (ordinary quoting churn) never flaps between representations. `INLINE = 8` is chosen to match: a book at or under the promotion threshold's lower band never touches the shared overflow pool at all, which is the common case this whole design is for.

**Staleness sweep mechanism — fully specified, reuses an existing spec pattern rather than inventing one.** A naive "check every instrument's idle time every tick" is exactly the O(instrument-count)-per-tick cost this ADR already rejected for HOT ranking. Spec `D-29` already solves a structurally identical problem — bounding per-tick cost while eventually visiting every instrument — by releasing *"at most one event, the earliest resolved"* per tick, in resolution order, off a queue. The demotion sweep reuses that pattern instead of inventing a new one: maintain instruments in a fixed-size ring, and on every `TimeTick` inspect a bounded chunk (`SWEEP_CHUNK`, e.g. 64) of it — `current_tick % (instruments / SWEEP_CHUNK)` selects which chunk — checking each candidate's `last_touched_tick` against `D_idle` and demoting eligible ones. This is a pure function of `current_tick` and existing per-instrument state, so it's deterministic and replays identically; it bounds added per-tick work to `SWEEP_CHUNK` cheap reads regardless of how many instruments exist; and a full sweep cycle completes in `instruments / SWEEP_CHUNK` ticks — at production's 1ms tick (`core/node/src/main.rs:102`, `core/node/README.md`: "production uses 1 ms"), `SWEEP_CHUNK = 64` sweeps `scale`'s 8,192 instruments in 128 ticks (~128ms), negligible next to `D_idle`'s scale.

**`D_idle` — cannot be honestly pinned to a specific duration from anything in this repo, and I'm not going to fabricate one.** Unlike the occupied-tick thresholds, there's no measured trading-activity distribution anywhere in this codebase (no production telemetry, no historical order-arrival data) to calibrate "how long is a market actually idle before it's safe to assume it'll stay idle." The blueprint's own tier budgets (≤50 / ≤2,000 / ~100,000 / unbounded) are stated as design targets, not derived from a decay curve either — there's nothing to anchor against. What I can responsibly do: recommend `D_idle` be a **group parameter, not a hardcoded constant** — the same treatment `settledRetentionSeconds` already gets under D-29 (an operator-set, sequenced value, applied as it stands at each tick) — so it can be revised without a redeploy once real idle-duration data exists from a running venue (`venue-observer`'s existing metrics pipeline, `deploy/observability/metrics.md`, is the natural source). As a starting placeholder to unblock implementation, not a calibrated figure: 24 hours of ticks (86,400,000 ticks at production's 1ms cadence) — explicitly flagged here as provisional and the first thing to revisit once the venue has real traffic to measure against.

## Non-Goals

- **Segmenting the shared `orders` slab is out of scope for this ADR.** It solves a different problem: not fixed-reservation waste (orders already cost nothing when absent), but shared-capacity pressure from many legitimate, low-activity resting orders accumulating over time ("what actually fills a slab is abuse and accumulation, not load," blueprint line 1336). The existing GTC max-lifetime and cancel-at-resolution rules already bound the worst case. Revisit only if long-tail slab pressure is measured as a real (not theoretical) problem — likely as a follow-up ADR.

  This is a stronger claim than "already elastic," worth stating precisely: the `orders` slab (`core/engine/src/book.rs:295-311`) is already a textbook intrusive free-list pool allocator — one scalar head (`free_order`), LIFO push/pop, threaded through the `account_next` field of freed slots themselves (no separate free-list storage), O(1) alloc and free, zero runtime allocation. A freed slot anywhere in the flat array is immediately reusable by any book, any account, any tick — there is no fragmentation mode where a hole becomes unusable, so there is nothing here for a tiering scheme to fix.

  The one real cost of this shape is that a level's FIFO (threaded through each `Order`'s own `prev`/`next`) can be scattered across the slab rather than contiguous — the blueprint says as much outright ("one tick's queue can be scattered across memory," line 1297) without proposing a mitigation. This is a materially smaller concern than a classic scattered-heap linked list, though: every hop lands inside one bounded, pre-sized array (not an arbitrary heap address), and the LIFO free list happens to favor reuse of recently-touched (cache-warm) slots. Where it costs something is a long FIFO walk during a large sweep fill — plausibly part of why `bench`'s measured `NewOrder` p99 (7.4µs) sits well above its p50 (0.27µs) — but that is a cost of the access pattern (walking time priority at a price), not of the allocator, and a tiered/segmented pool would not remove it. Reworking this pool would mean trading a solved problem (a correct, standard, already-adequate O(1) allocator) for an unsolved one (a new segmented pool with its own promotion/demotion correctness burden) without addressing the actual cost driver. That asymmetry, not just scope, is why this ADR leaves it alone.
- No change to matching semantics, order priority, or the `orders` pool's structure.
- No change to sequencer-group boundaries (moving a book between sequencer groups, blueprint §5, is a separate, coarser-grained mechanism and not addressed here).

## Options Considered

### Option A: Raise the `levels` capacity number
| Dimension | Assessment |
|---|---|
| Complexity | Low — change one constant |
| Cost | Low engineering, but pure resident-memory cost, paid even by books that never trade |
| Scalability | Poor — cost still scales linearly with book-count × tick-width; only moves the ceiling, doesn't remove the power-law problem blueprint §1 explicitly calls out |
| Team familiarity | High |

**Pros:** trivial, no new replay/snapshot surface.
**Cons:** does not address the actual requirement ("marginal cost of a listed instrument must approach zero"); a fixed table still "never grows while it runs" (`state.rs`), so it just relocates the same failure mode to a higher book count.

### Option B: Coarser tick tables for low-value books
| Dimension | Assessment |
|---|---|
| Complexity | Low-Medium |
| Cost | Low |
| Scalability | Partial — shrinks footprint only for books deliberately listed on a wider ladder |
| Team familiarity | High |

**Pros:** no new representation or promotion/demotion machinery at all.
**Cons:** changes the traded product (price granularity) rather than being a pure infrastructure optimization; a book that starts wide and becomes popular can't be widened to finer ticks without relisting, which is a much bigger compatibility break than a representation swap.

### Option C: Cross-sequencer-group migration (route cold instruments to a dedicated, cheaply-provisioned group)
| Dimension | Assessment |
|---|---|
| Complexity | High — needs the sequenced-transfer protocol blueprint §5 describes |
| Cost | Avoids in-process representation complexity entirely |
| Scalability | Good, but coarse-grained (whole-group, not per-book) |
| Team familiarity | Low — no such transfer exists yet |

**Pros:** keeps every engine's internal book model uniform and simple; cold instruments genuinely cost ~0 on the busy group.
**Cons:** much larger surface (cross-group consistency, a real migration protocol, "never by moving live memory" per blueprint §5); a promoted book pays a migration round-trip instead of an in-process representation swap; harder to reason about determinism across two sequencer groups than within one.

### Option D (chosen): In-process tiered book representation (Decision, above)
| Dimension | Assessment |
|---|---|
| Complexity | Medium-High — three code paths in the matching hot path instead of one |
| Cost | Scales with actual occupied-tick count, not book-count × tick-width |
| Scalability | Good — directly targets the mechanism blueprint §1/§10 call for |
| Team familiarity | Medium — extends existing patterns (`compact_levels()`'s relocation, the `orders` pool's elastic-slot model) rather than introducing new ones |

**Pros:** addresses the measured failure directly; reuses existing patterns (slot-index addressing, no pointers, elastic shared pools) rather than inventing new ones; demotion/promotion rules are single-book-local, so no ranking cost.
**Cons:** see Consequences.

## Trade-off Analysis

The core trade being made is **matching-latency guarantee vs. resident memory**, book by book instead of uniformly:

- Dense keeps the O(1), branch-free, bitmap-indexed best-price lookup the whole engine currently relies on (blueprint §6) — necessary for books that actually need the 7–15µs matching budget (blueprint §9).
- Sparse/inline trades that guarantee for memory: best-price becomes a scan over however many entries are occupied (cheap when that count is small, which is exactly the condition gating a book into this tier).
- The promotion rule is deliberately conservative (promote the instant capacity would be exceeded, not once thoroughly proven "hot") to avoid ever paying scan cost on a book with real depth.
- Rejecting the top-K/HOT-ranking idea (see Decision) keeps every classification decision local to one book, at the cost of not having a hard "≤50 busiest markets" cap the way blueprint §10's figure implies — a threshold-based scheme can't guarantee an exact count, only a resident-cost ceiling.

## Consequences

- **Easier:** the effective "how many books can we run" number stops being dominated by tick-width × book-count and starts tracking actual occupied-tick count across the venue — directly targets the blueprint's own stated requirement.
- **Easier:** capacity planning for the long tail (blueprint §1's "power-law volume... instrument count") no longer requires either wasting memory on dead books or capping the venue at a few hundred/thousand instruments.
- **Harder:** every hot-path book operation (insert, cancel, fill, best-price) now branches on tier — this must be measured against the existing 7–15µs matching-and-clearing budget (`core/node/src/bench.rs` is the existing tool for this) to confirm the branch doesn't erode it for dense/HOT books, which are the ones the budget matters most for.
- **Harder:** promotion/demotion is a new state transition that must be proven byte-identical on replay and survive snapshot/restore, the same way `core/node/tests/snapshot_replay.rs` / `embedded_snapshot.rs` already prove for existing recovery paths. A book that changes tier mid-journal needs its own test in this family.
- **Harder:** relocating a book's data between dense and sparse representations is a new bidirectional transform, distinct from (though related to) the existing `compact_levels()` relocation (`core/engine/src/commands.rs`), which only ever moves dense-to-dense on release/reclaim.
- **New operational surface:** the sparse-overflow pool is a second, smaller shared capacity ceiling alongside the existing dense `levels` pool — one hard ceiling becomes two softer ones. Both need their own `Capacities` fields and default sizing for `small`/`launch`/`scale`.
- **Resolved:** promotion is "grow within sparse until inline+overflow capacity is actually exceeded" (`PROMOTE_ABOVE = 32`), not "any touch instantly goes fully dense" — see Pinned Thresholds. The remaining open item is `D_idle`'s actual duration, which needs real telemetry this repo doesn't have, not a design choice this ADR can resolve on its own.

## Prototype Findings (2026-09-25)

Built `core/prototypes/tieredbook` (mirrors `core/prototypes/maskbench`'s pattern: a standalone benchmark, not wired into `core/engine`). Dense reproduces today's mechanism (100-tick array + bitmap); sparse holds a few ticks inline plus a shared `OverflowPool` implemented as the same intrusive singly-linked free list `core/engine/src/book.rs`'s order slab already uses, not a `Vec` per book (a `Vec`'s own header costs ~96 bytes regardless of occupancy, which would understate a cold book's real footprint).

**Memory** (fixed, independent of occupancy):

| Representation | Size |
|---|---|
| `DenseBook` (100 ticks, today's mechanism) | 1,216 bytes |
| `SparseBook<4>` inline shell | 64 bytes (19x smaller) |
| `SparseBook<8>` inline shell | 120 bytes (10x smaller) |
| `SparseBook<16>` inline shell | 232 bytes (5.2x smaller) |
| One overflow entry (shared, amortised) | 18 bytes |

**Best-price lookup** (`best_below`, scanning all 100 tick positions): dense stays flat at ~1.8 ns regardless of occupancy (as expected — O(1) bitmap). Sparse scales roughly with occupied-tick count: ~3.5 ns at 4 occupied (INLINE=4, no overflow), ~7.5 ns at 8 occupied (INLINE=8, no overflow), up to ~110 ns at 99 occupied (fully dense, worst realistic misclassification). The practically important number: **even the worst case (a 99-tick book wrongly still classified as sparse) costs ~110 ns — roughly 0.7–1.5% of the 7–15µs matching-and-clearing budget.** Misclassification is a real slowdown but not a latency-budget threat at any occupancy this format would plausibly be used at.

**Insert+remove one tick**: dense costs a flat ~17.4 ns regardless of existing occupancy (always touches the same fixed set of arrays + one bitmap bit). Sparse costs ~6–7 ns across every scenario tested — cheaper, not just competitive, since it only ever touches a handful of small array entries or one pool slot. Caveat: this measured the best case for removal — the freshly-inserted entry sits at the head of the overflow chain, so removing it is O(1); removing an arbitrary older entry buried in a long overflow chain would cost proportionally more, same shape as `best_below`.

**What this settles, and what it doesn't:**
- Inline capacity `K` is mostly a memory-floor decision, not a speed one — `best_below` cost at a given occupied-tick count is nearly identical whether those ticks are split inline/overflow (INLINE=4, 8 occupied: 6.98 ns) or packed differently (INLINE=8, 8 occupied: 7.52 ns). The lever `K` actually controls is what fraction of real cold books never touch the shared pool at all.
- The promotion policy doesn't need to be justified primarily by a latency cliff — absolute sparse cost stays small even at high occupancy. The real reason to promote before a book gets busy is code-path simplicity and avoiding pathological overflow-pool churn, not a hard matching-budget threat. This should soften Action Item 4 below: the exact threshold matters less for correctness than for keeping the common case cheap.
- Not measured here: removal cost for a deep/old overflow entry, and any interaction with real order-slab churn under concurrent inserts/cancels — both need the real `core/engine` integration to observe, not this isolated prototype.

**Conversion cost** (`promote`/`demote`, added after the initial prototype pass): both walk only occupied ticks — `promote` via the sparse book's inline entries plus its overflow chain, `demote` via the dense bitmap directly (`trailing_zeros`, never a 0..100 scan) — so cost tracks *occupied-tick count*, not the book's tick-table width. Neither touches an `Order` record; both only relocate the small per-tick `(qty, head, tail)` triple, the same scope `compact_levels()` already operates in.

| Occupied ticks | Promote (sparse→dense) | Demote (dense→sparse) |
|---|---|---|
| 1 | 12.3 ns | 4.8 ns |
| 4 (INLINE=4, at capacity) | 14.7 ns | 7.6 ns |
| 8 (INLINE=4, overflow) | 24.9 ns | 15.7 ns |
| 8 (INLINE=8, at capacity) | 18.1 ns | 13.3 ns |
| 32 (INLINE=8, overflow) | 50.7 ns | 73.5 ns |
| 64 (INLINE=8, overflow) | 98.6 ns | 168.0 ns |

Even the most expensive case measured (64 occupied ticks, demote) costs ~168 ns — **~1–2% of the 7–15µs matching-and-clearing budget**, comfortably inside "promotion must be instant, within one sequencer event." Demote is cheaper than promote at low occupancy (walking a bitmap directly beats iterating a small array) but grows faster at high occupancy (each overflow allocation touches five separate pool arrays vs. promote's flat writes into a fixed inline destination) — worth knowing, but not a practical concern at any occupancy where demotion should actually be triggered. Both round-tripped correctly against an independently-built reference book (dense-built-from-scratch vs. promoted-from-sparse, and vice versa) across every scenario tested.

## Action Items

1. [x] Prototype the sparse/inline book representation and its hot-path cost in isolation (following `core/prototypes/maskbench`'s existing pattern of micro-benchmarking a representation choice before adopting it) — confirm scan cost at the chosen inline capacity `K` is acceptable. **Done** — see Prototype Findings above; `core/prototypes/tieredbook`.
2. [x] Pin down the exact demotion thresholds (`D_idle` in `TimeTick` units, occupied-tick counts) as a pure function of existing/new per-instrument counters; confirm the function is replay-deterministic. **Done** — see Pinned Thresholds above: `PROMOTE_ABOVE = 32`, `DEMOTE_BELOW = 16`, `INLINE = 8` are benchmark-grounded; the sweep mechanism reuses spec D-29's "bounded work per tick" pattern and is a pure function of `current_tick`. `D_idle` itself is explicitly flagged as an unset placeholder (24h of ticks) pending real telemetry — see that section for why it can't honestly be more than that yet.
3. [ ] Add an `overflow` field to `Capacities` (`core/engine/src/state.rs`) and choose default sizes for `small`/`launch`/`scale`; re-run today's `venue-sim workload` bisection against a prototype to measure the new book-count ceiling for the same 99-tick workload.
4. [x] Decide the promotion policy from the open question in Consequences ("instant-to-dense" vs. "grow-within-sparse"). **Done, resolved as "grow-within-sparse"** — promotion triggers only once `occupied_ticks` would exceed `PROMOTE_ABOVE`, not on the first touch of a dormant book; see Pinned Thresholds.
5. [ ] Write replay/snapshot tests for a book that changes tier mid-journal, alongside the existing `snapshot_replay.rs` / `embedded_snapshot.rs` suite.
6. [ ] File a separate, explicitly lower-priority follow-up for order-slab segmentation (Non-Goals) rather than bundling it into this work.
