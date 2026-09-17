# Working in this repository

This repository holds the blueprint for a regulated prediction-markets exchange, the specifications a coding
agent needs to build its core, and the core itself. Read this before changing anything.

## What is where

| Path | What it is |
|---|---|
| `blueprint/prediction-exchange.html` | The architecture blueprint. Explains why; not an implementation contract. |
| `interfaces/` | One specification per interface between components, and the catalogue of all of them. The journal schema is `interfaces/sbe/venue-journal.xml`. |
| `spec/engine/engine-spec.md` | The normative behaviour of one sequencer group's engine, in numbered rules (`E-10.6`). |
| `spec/engine/decisions.md` | Defaults chosen to complete the specification, some awaiting review. |
| `spec/engine/scenarios/` | Acceptance tests: inputs and exact expected results, computed by hand from the specification. |
| `spec/engine/reference/` | The reference model (an executable second statement of the specification), its scenario runner and its random-sequence tester. |
| `core/engine/` | The engine. The only code an implementation work package changes. |
| `core/harness/` | Runs scenarios and differential vectors against the engine; the JSON codec in `src/codec.rs` is generated. |
| `core/journal/` | The Rust codec generated from the schema. Never edited by hand. |
| `core/node/` | The single-member node for laptops and test environments: simulated sequencer, journal, result log, TCP ingress and feed. Never on the production path. |
| `core/link/` | Talking to a node from outside the engine: ingress client, feed follower, a projection of the feed, the demo catalogue. |
| `core/gateway/` | Interface 1, the REST and WebSocket gateway (`interfaces/gateway/`): the only client-facing component that writes to a sequencer group. Its public half (market data) and private half (orders, accounts, transfers, requests for quote) can run in one process or apart (`--serve`). |
| `core/treasury/` | The dummy treasury: deposits and withdrawals behind the gateway's `/v4/transfers`, a double-entry ledger and a fake bank. Test environments only. |
| `core/parlays/` | The parlay service: lists parlays, routes requests for quote to responders, and settles each parlay from its legs. Test environments only. |
| `core/sim/` | The mini venue's actors: synthetic makers (which also answer requests for quote) and takers, the dummy oracle and its calendar, demo markets and the web page. Test environments only. |
| `adapters/fix/` | Interface 4, the FIX 4.4 adapter (Java, Artio) in front of the gateway's private half; its dialect is `interfaces/fix/`. Not part of the Rust workspace: `bootstrap.sh` fetches its JDK and Gradle, `test.sh` runs its integration test, `scripts/fix-shell.py` is a console for trying it by hand. |
| `core/prototypes/` | Experiments behind blueprint claims. Learn from them; nothing depends on them. |
| `core/WORK_PACKAGES.md` | The order in which the core is built, and what "done" means for each piece. |
| `check` | Every gate in the repository. |

## Order of authority

When two of these disagree, the higher one wins until a reviewed change says otherwise:

1. `spec/engine/engine-spec.md` and `interfaces/sbe/venue-journal.xml`
2. `spec/engine/scenarios/*.json`
3. `spec/engine/reference/model.py`
4. `core/`

A disagreement is a bug in one of them. Decide which from the specification, and say which rule decided it.

## Rules for implementing the core

- **Never change the specification, the schema, a scenario or the reference model in the same change as
  engine code.** `./check` fails any commit that touches `core/engine/` together with `spec/` or `interfaces/sbe/`.
  If the engine and a scenario disagree and you believe the scenario is wrong, stop and write the case up in
  `spec/engine/questions/` in a separate change — the input, the result you believe is right, the rule, and why —
  for a person to review. Tests that are edited to match an implementation stop being tests.
- **Never regenerate expected results from an implementation.** The compiled vectors in `core/target` are
  derived and disposable; the scenarios are hand-computed and normative.
- **Never edit generated code:** `core/journal/` comes from `interfaces/tools/sbe.sh`, and
  `core/harness/src/codec.rs` from `interfaces/tools/gen_rust_json_codec.py`.
- **The engine is a pure function of its journal** (blueprint §4, spec E-1.1): `no_std`, no allocation after
  construction, no clock, no I/O, no randomness, no floating point, no hash-ordered iteration, checked
  arithmetic with overflow as a fail-stop. `./check` enforces what it can; review enforces the rest.
- **Cite rules.** Code that implements a rule names it in a comment (`// E-11.4`), so a reviewer can check the
  code against the sentence.
- **Emit results in exactly the order of spec §5.** Order is part of the contract; readers depend on it.
- **Stay inside the work package.** Each package in `core/WORK_PACKAGES.md` lists its rules and its passing
  scenarios. Do not implement later packages early, and do not implement anything listed in spec §19.

## Running the gates

```sh
./check                                                                # everything, in order
python3 spec/engine/reference/run_scenarios.py                         # reference model against scenarios
python3 spec/engine/reference/fuzz.py --seeds 50 --steps 300           # invariants on random sequences
(cd core && cargo run -p venue-harness --bin run-scenarios -- target/compiled target/differential)
```

`./check` must pass before a change is proposed. Set `ENGINE_REQUIRED=1` once work package WP-5 has landed.
Its last step, the FIX adapter's integration test, runs only once `adapters/fix/bootstrap.sh` has fetched the Java
toolchain and is skipped otherwise; a change that touches the gateway's private half or `adapters/fix/` should run
it. Each gateway test starts a node of its own, so on a machine short of memory limit them with
`RUST_TEST_THREADS=4`.

## Changing the specification

Specification changes are made by people, or by an agent explicitly asked to make one, never as a side effect of
implementation. A specification change updates, in one change: the rule in `engine-spec.md`, the decision in
`decisions.md` if a default moved, the reference model, at least one scenario that fails before the change and
passes after, and the schema, `core/journal` (via `interfaces/tools/sbe.sh`), the JSON codec and the catalogue if
the encoding moved. If the engine must change in the same commit, that commit is reviewed as such and checked with
`ALLOW_SPEC_AND_ENGINE=1`.
