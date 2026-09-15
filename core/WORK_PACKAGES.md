# Work packages for the core

The engine is built in the order below. Each package is sized for one agent working in one change, names
the specification rules it implements, and is done only when its acceptance criteria hold and `./check`
passes. Read `AGENTS.md` first.

Scenario numbers refer to `spec/engine/scenarios/NN-*.json`. "Differential" means the vectors exported by
`fuzz.py --export`, which `./check` writes to `core/target/differential`.

| WP | Package | Depends on | Spec rules | Scenarios that must pass |
|---|---|---|---|---|
| 0 | Toolchain and CI baseline | — | — | none (codec round trip passes) |
| 1 | Processing loop and results | 0 | §4, §5, E-2.8, §18 plumbing | none yet; unit tests |
| 2 | Fixed-capacity state and reference data | 1 | §3, §6, §7 | 16 |
| 3 | Resting orders without matching | 2 | §8, E-9.1–E-9.3 (not checks 13, 14, 16, 17), E-9.5–E-9.8, §11, E-12.1–E-12.2, E-12.6, §16 | 08, 09, 10, 21 |
| 4 | Matching and clearing | 3 | §10, E-9.9, E-12.3–E-12.5, E-12.8, E-13.1 | 01 steps 1–4, 02, 03, 04, 05, 11, 13, 14, 18, 20 |
| 5 | Protections, limits, set burns, settlement | 4 | E-9.4, E-9.7, E-12.7, E-13.2–E-13.4, §14, §15 | all, and every differential vector |
| 6 | Invariants inside the engine | 5 | E-3.3, E-10.10, E-15.2, E-18.1 | all, plus fail-stop tests |
| 7 | Canonical snapshots and event transfer | 6 | spec first, then §19 items | snapshot equivalence |
| 8 | Simulated sequencer, journal files and replay | 6 | §17 | golden replay of differential vectors |
| 9 | Performance | 5 | blueprint §6, §9 | no change in any result |
| 10 | Aeron Cluster integration | 8 | interface 10, spec first | end-to-end journal equivalence |

---

## WP-0 Toolchain and CI baseline

**Goal.** `./check` passes with the engine stub.

- Pin the toolchain in `core/rust-toolchain.toml` to the version CI uses, with `rustfmt` and `clippy`.
- Run `cargo deny check bans licenses sources` in CI and make it pass.
- Confirm the codec round trip covers every message the scenarios and differential vectors use.

**Done when** `./check` passes, the engine stub reports every scenario as failing with `NotImplemented`,
and the pinned toolchain builds identically on two machines (compare the engine library's hash).

## WP-1 Processing loop and results

**Goal.** The engine decodes any journal input, checks sequence and time, and emits correctly framed results.

- Decode by template ID with the generated `venue_journal` decoders. Commands and sequencer events that do not
  decode are a fail-stop; client messages that do not decode are rejected `MALFORMED` (E-4.3).
- E-4.1 sequence and time checks; `now` and `lastSequence` update before anything else.
- A result encoder that writes `ResultHeader` with the input's sequence, time, group and a running index
  across all units of the input (E-5.1), into a fixed buffer owned by the engine.
- The unit framework of E-5.2: an outcome slot, an event list, and change tracking for positions, sleeves
  and book levels that compares against start-of-unit values and emits blocks 3 to 5 in their sort orders,
  without allocation. Design the tracking so its cost is proportional to what a unit touched.
- `FailStop` returned, never a panic, for every §18 condition reachable so far.

**Done when** unit tests show: out-of-sequence and backwards-time inputs fail-stop; a malformed client message
yields exactly one `OrderRejected`; result indices run across units; and blocks 3 to 5 are emitted once per
changed key in the specified order.

## WP-2 Fixed-capacity state and reference data

**Goal.** All state of §3 exists in tables sized at construction, and every §6 and §7 command works.

- `EngineConfig` gains capacities: tick tables and rows, events, instruments, accounts, positions, resting
  orders (the slab ceiling of blueprint §6), duplicate-table entries, command IDs, order groups.
- Start from `core/prototypes/l3book`, which already implements this layout and is differential-tested against an
  ordered-map model; it is a prototype to learn from, not code to depend on.
- Books as blueprint §6 describes: per-tick aggregate quantity, head and tail slot, occupancy bitmap per side
  (two-level above 128 ticks), orders in one slab with intrusive FIFO links and a second link list per
  session, an ID map with open addressing sized from the ceiling and kept at most half full.
- Exhausting any table is a defined outcome, never an allocation: `ORDER_CAPACITY` for resting orders
  (E-9.3 check 17), fail-stop `Capacity` for tables that should never fill in operation.
- Anything iterated is iterated by explicit key order; slab slot numbers never appear in results.
- §6 and §7: `DefineTickTable`, `DefineEvent`, `ListInstrument` with every validation, partition events,
  command-ID duplicates, `SleeveAdjust`, `SetRuleLimits`, `SetGroupParameters`, `SetAccountControls`,
  `SetMakerProtection`, `ActivateProtocol`; `TransferOut` and `TransferIn` rejected until WP-7.

**Done when** scenario 16 passes and unit tests cover every `ListInstrument` rejection and a full slab.

## WP-3 Resting orders without matching

**Goal.** Orders are validated, rest, reserve, lock, cancel, expire and respond to state changes.

- E-9.3 checks 1–12, 15 and 18 in order (13, 14, 16 and 17 need the walk and belong to WP-4), E-9.2
  duplicates with the keyed hash seeded by `SeedSet`, E-9.5 reduce-only and closed-only, E-9.6 effective limit.
- E-11.1 locks and E-11.2 reservations; the account's `reserved` is always the sum of its orders'.
- E-12.1, E-12.2 cancels by order ID and client order ID; E-12.6 mass cancel over every scope.
- §8 `TimeTick` expiry in `(expiresAt, orderId)` order; §16 market states, event phases and venue modes.

**Done when** scenarios 08, 09, 10 and 21 pass.

## WP-4 Matching and clearing

**Goal.** Incoming orders trade.

- E-10.3 walk; E-10.4 price-time and E-10.5 pro-rata with largest remainder in lots.
- E-10.6 fill arithmetic exactly as written, including mixed fills; collateral per event.
- E-11.3 lock consumption and E-11.4 restoration, newest first, shrinking reduce-only orders.
- E-10.9 dry run for `FOK` and capacity: the full walk, lock restoration included, on state that is then
  restored. Use an undo log over the few tables a walk touches rather than copying state.
- E-9.3 checks 13, 14, 16, 17; E-9.9 remainders; spend caps; protection bands; post-only; E-13.1 self-match.
- E-12.3 to E-12.5 amendments, including repricing that trades; E-12.8 batches as separate units.

**Done when** scenarios 01 (steps 1–4), 02, 03, 04, 05, 11, 13, 14, 18 and 20 pass.

## WP-5 Protections, limits, set burns and settlement

**Goal.** The rest of the specification.

- E-9.4 rule limits per second of sequencer time; E-9.7 position limits.
- E-13.2 maker auto-pull and E-13.3 order groups, acting at the end of the unit (E-13.4); E-12.7.
- §14 set burns with lot-step rounding (E-14.3); basket mint and redeem are withdrawn (D-24).
- §15 resolution, voids reversing each account's event cash (D-1), parlay expiry.

**Done when** every scenario and every differential vector passes, and `ENGINE_REQUIRED=1` is set in
`./check`. From here on, run at least 300 differential seeds before proposing a change.

## WP-6 Invariants inside the engine

**Goal.** The engine detects its own §18 conditions cheaply enough to leave on in production.

- Conservation per event per state (E-15.2) maintained incrementally, not by rescanning accounts.
- Available cash, crossed books and lock-within-holding checks at the points they can change.
- Add fail-stop scenarios through a specification change (see `AGENTS.md`), one per §18 condition that input
  can reach.

**Done when** each reachable §18 condition has a test that triggers it and the differential vectors still pass.

## WP-7 Canonical snapshots and event transfer — specification first

**Goal.** A snapshot at sequence N plus replay to M equals replay from zero to M, byte for byte (blueprint §4).

- First, a specification change: the canonical snapshot encoding (explicit field order, books walked in price
  and time order, no slot numbers), the event-state encoding `TransferIn` carries, and the rules for
  `TransferOut` and `TransferIn` now listed in spec §19.
- Then the implementation, and a property test over differential vectors: snapshot at a random N, restore
  into a fresh engine with a different slab layout, replay, compare results and final snapshot bytes.

## WP-8 Simulated sequencer, journal files and replay

**Goal.** Everything blueprint §4 promises a bad afternoon: reproduce, bisect, prove a refactor safe.

- An in-process sequencer obeying §17 (gap-free sequence, monotonic stamping, `TimeTick` cadence,
  `EngineStarted`), writing journal files as length-prefixed SBE frames.
- A replay tool that runs a journal through the engine and writes the result log (interface 20).
- Golden replay: two runs of the same journal produce identical result logs; an invariant-checked bisect
  finds the first input that breaks a §18 condition.

## WP-9 Performance

**Goal.** The latency and memory shape of blueprint §6 and §9, with no change to any result.

- Benchmarks for the walk, cancels and the unit framework; a test that fails if a result changes.
- No allocation after construction (the crate is `no_std` without `alloc`, so this is already structural);
  cache layout of books and slab as §6 describes.
- Numbers are measured later on the metal of blueprint §9; laptop numbers are for regression only.

## WP-10 Aeron Cluster integration — specification first

**Goal.** The engine behind Aeron Cluster as blueprint §5 recommends.

- First, interface 10 (catalogue step 3): the IPC messages between the Java clustered service and the Rust
  engine, the snapshot fence, role changes and the fail-stop coupling of the two processes.
- Then the Java `ClusteredService` shim and the Rust IPC endpoint, and an end-to-end test that the cluster's
  committed log, replayed through WP-8's tool, yields the same result log as the running cluster.

---

## Handing a package to an agent

Give it `AGENTS.md`, this file, the package's section, and the instruction to finish with `./check`
passing. A useful opening prompt:

> Implement work package WP-N from `core/WORK_PACKAGES.md` in `core/engine`. The normative rules are in
> `spec/engine/engine-spec.md`; cite rule numbers in comments. Do not modify anything under `spec/`,
> `interfaces/` or the scenarios. If you believe a scenario or rule is wrong, stop and write it up in
> `spec/engine/questions/`. Finish when `./check` passes and the package's scenarios pass.
