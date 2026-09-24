# venue-node

A single-member node for laptops and test environments (blueprint §16, work package WP-8). One process runs
one sequencer group: a simulated sequencer, the engine, a journal and its result log, and a TCP ingress.
Production puts the engine behind Aeron Cluster instead (WP-10). This node never produces a latency number.

## What it does

- **Sequencer (spec §17).** Stamps each input's `RecordHeader` in place: sequence gap-free from 1, `sequencedAt`
  the later of the wall clock and the previous stamp plus 1 ns. Writes `EngineStarted` at every start and a
  `TimeTick` every `--tick-ms` (default 100; production uses 1 ms, and expiry precision is the tick, D-5).
- **Journal (interface 11).** `DATA/journal.sbe`. Each input is appended and flushed, `fsync`ed too with `--fsync`,
  before the engine sees it.
- **Result log (interface 20).** `DATA/results.sbe`. Regenerated from the journal at every start, so it always
  equals a replay. Written durably: flushed, and `fsync`ed too with `--fsync`, before the next input is committed
  — the same discipline the journal already has, and for the same reason (below, Snapshots).
- **Recovery.** At start, the latest embedded snapshot is used when the preserved result log already covers it —
  otherwise (no snapshot, or the result log doesn't yet reach one that exists) the journal is replayed through a
  fresh engine from sequence 1, exactly as before there were snapshots. A torn final frame left by a crash is
  cut off either way (below, Snapshots).
- **Fail-stop (spec §18).** The process exits. Restarting replays the journal and stops at the same input,
  which is what E-18.2 requires; recovery is an operator decision.

- **Feed.** `--feed` (default `0.0.0.0:7102`), a development stand-in for interface 12. A reader receives every
  input with its results, from the start of the journal, then live, as units: the input frame, its result frames,
  an empty frame. `TimeTick`s that changed nothing are left out. The web page and the bots in `core/sim` read it.

  A reader opens by saying where it has got to — eight little-endian bytes, the last sequence it holds, or zero
  for everything — and is answered one byte saying whether that position was honoured. It is honoured only when
  the journal can actually answer for it at **both** ends: not past what the journal has reached (a reader whose
  node was replaced under it), and not back before what the journal still carries, once `cut` has removed the
  start of it or `seed` founded above it. A position below that floor is refused rather than served from where
  the journal now happens to begin, which would skip everything in between while the answer said the resume had
  worked. Holding *through* the floor is not below it: a journal founded or cut at 62 carries 63 onward, and a
  reader saying "I have 62" is asking for exactly that.

## Framing

Journal files, result logs and the TCP ingress all use one framing: a little-endian `u32` length, then that many
bytes of SBE (`interfaces/sbe/venue-journal.xml`), message header first.

There are two length bounds, not one. An input is bounded by its schema — no arrangement of fields makes a
`PlaceOrder` large — so inputs are held to `MAX_FRAME`, 64 KiB, which is far above any of them and refuses a
garbage length cheaply. A `Snapshot` is not bounded that way: it is the engine's whole logical state, and it
grows with the venue, so it is held to `MAX_SNAPSHOT_FRAME` (16 MiB, the buffer `snapshots::compute` already
encodes into). Only template 53 gets the larger bound; anything else past 64 KiB is still refused, on the way
out and on the way in. On the way in the template is read from the header's first four bytes *before* the
length is honoured, so a garbage length still costs four bytes rather than an allocation.

This matters more than a limit usually does. A real venue's state passed 64 KiB somewhere between its 30,000th
and 60,000th input — a few hours of demo traffic — and from there `snapshot` and `embed-snapshot` both failed
with nothing to say but `frame longer than MAX_FRAME`. Since a journal with no snapshot in it can never be
`cut`, the disk this was all meant to stop filling filled anyway, and nothing said why.

On the ingress a connection sends client messages or commands with a zeroed record header. For each input it
receives the input's results, then an empty frame. Only the sender receives an input's results, and only after
the input is journaled (E-17.4 without a quorum). Sequencer events and anything that is not a journal input close
the connection and are not journaled. The ingress is for tools, bots and a development gateway; it has no
authentication and must never face a network anyone else can reach.

## Snapshots (spec §20)

A snapshot is the engine's logical state, taken by a replaying engine and never by the one serving traffic
(E-20.10): the hot path does not pause to serialise itself. `venue-node snapshot JOURNAL OUT [--at N]` writes one
to a file of its own; `venue-node embed-snapshot JOURNAL [--at N]` writes the same frame into the journal itself
instead (`Snapshot`, template 53), which is what D-32 asks for — a journal with no snapshot in it can never be
cut, because recovery has nowhere to begin but sequence 1. A `Snapshot` frame is not an input: every reader that
walks a journal frame by frame (a full replay, recovery, a subscriber's backlog) skips it by template id, exactly
as it skips one embedded earlier when computing a later one.

`venue-node recover JOURNAL RESULTS [--snapshot-out FILE]` restores from the latest embedded snapshot and replays
only what follows it — the single-journal-file version of `replay-from` (which does the same from a snapshot
file of its own). Both are checked against a full replay from genesis for the same journal, byte for byte
(E-20.9): `core/node/tests/snapshot_replay.rs`, `core/node/tests/embedded_snapshot.rs`.

**Result log durability.** The result log used to be flushed once a tick rather than per input, so a crash could
lose results a client had already been told about, and a preserved copy could never be trusted after one: nothing
records how many results one input produced (`ResultHeader` carries `inputSequence` and `resultIndex`, not a
count), so a group cut short by a crash still ends on a whole, well-formed frame. `commit` now flushes (and
`--fsync`s) the result log the same way it already does the journal, before the next input is committed —
`core/node/tests/result_log_durability.rs` checks a result is on disk before the client that caused it is told
so, not only once a tick later. `trusted_results`/`repair_results` read a result log back and say how far it can
be trusted: every group but the last is confirmed complete, because a different sequence following it is only
possible once E-4.1 has moved past it; the last group is never trusted, since nothing distinguishes one a crash
cut short from one nothing has followed yet.

**Wired into `serve`'s own recovery.** `choose_recovery` (`core/node/src/node.rs`) decides once at start: a
snapshot is only used when the preserved result log's trusted point (`repair_results`) reaches **at least** its
own sequence — a snapshot found but not yet covered by a trusted result log falls back to genesis, because using
it without also re-deriving the results it covers would leave a gap in `results.sbe` a subscriber's backlog
cannot fill for those sequences, and re-deriving them defeats what the snapshot was for. Taking the snapshot's
path restores the engine from it, replays anything already trusted but not yet reflected in the engine's memory
into a throwaway sink (advancing state without rewriting durable results), then replays only the genuinely new
tail for real, appended to the *preserved* result log rather than a fresh one.
`core/node/tests/wired_recovery.rs` checks a node recovering this way is indistinguishable from one that
recovered from genesis, both in what a subscriber's backlog receives and in the result log's own bytes.

**Cutting.** `venue-node cut-journal JOURNAL RESULTS` removes what the latest embedded snapshot makes
removable from *both* files — the journal has no way to drop bytes from its front the way `repair_results` drops
an unconfirmed tail, so this rewrites each one whole (a sibling temp file, `fsync`ed, renamed over the original —
nothing is replaced until its replacement is already durable). Refuses, touching neither file, unless the result
log is already trusted through at least the snapshot's own sequence — the same check `choose_recovery` makes,
because cutting past what it covers would permanently lose the ability to reconstruct those results.

A single "keep from this byte onward" cut point is **not enough** to do this correctly: `embed` appends wherever
the journal currently ends, which is not necessarily right after the sequence it describes (§3.7, position
carries no meaning) — a higher-sequence input can physically sit *before* a snapshot embedded after it was
already journaled. Cutting keeps the target snapshot and everything with a greater sequence, filtered frame by
frame in their original relative order, the same discipline `replay_tail` already uses for the same reason. This
also means a journal that has ever been cut can no longer be `replay`ed from genesis — its first *real* input is
no longer sequence 1 (`E-4.1` would fail-stop on it) — `replay` and `serve`'s own recovery both check for this
and refuse or fall back cleanly rather than attempting a doomed replay; `recover` is what reads a cut journal.
`core/node/tests/cut_journal.rs` checks a cut-and-recovered node reaches the same tail a full replay would have,
sequence for sequence and byte for byte in the result log, and that a refused cut changes nothing.

Shipping what is cut to object storage is not built yet.

**Seeding a fresh environment from a chosen snapshot.** `venue-node seed SNAPSHOT DATA_DIR [--capacity ...]`
founds a brand-new `DATA_DIR` on `SNAPSHOT` instead of genesis: `journal.sbe` holds only that one `Snapshot`
frame, `results.sbe` starts empty, and `capacity` records the profile given. A `serve` pointed at `DATA_DIR`
starts running from exactly that state, with no history of its own before it — for a test or preview
environment that wants to start from whatever snapshot it likes, not genesis or whatever the live venue happens
to be running. Refuses a `DATA_DIR` that already has a journal, so it cannot silently replace a real
environment's own history.

This needed a small extension to `choose_recovery`: normally a snapshot is only trusted when the result log
already covers it (above), because skipping replay must not leave a gap in `results.sbe` for real inputs the
journal still carries. A freshly seeded journal has no real input anywhere in it, before or after its snapshot
(`founded_only`, narrower than `starts_at_genesis` — an ordinary journal with a snapshot merely appended at its
*tail*, `embed` with no `--at`, still owes results for everything before it and must keep falling back to
genesis) — so there is nothing an empty or missing result log could have lost, and the snapshot is trusted
unconditionally. `core/node/tests/seed_from_snapshot.rs` checks a seeded node continues sequencing exactly where
the snapshot left off, and that it answers a new input identically to a node that reached the same point by
actually running from genesis.

`interfaces/tools/snapshot_publish.py`/`snapshot_fetch.py` put a snapshot in object storage under
`snapshots/{environment}/{tag}`, and fetch one back by tag, verified against a `.sha256` sidecar the same way
the audit shipper verifies before it trusts anything (`interfaces/audit/README.md`) — so a test or preview
environment's own setup can be as simple as
`snapshot_fetch.py --from s3://... --environment prod --tag pre-load --to seed.snapshot && venue-node seed
seed.snapshot ./venue-data`. Wiring this into any specific environment's own startup (ArgoCD, a preview
environment's init step) is not done — these are the building blocks, not an automatic default for every
preview environment.

## Commands

```sh
venue-node serve --data ./venue-data --listen 127.0.0.1:7101
venue-node send 127.0.0.1:7101 inputs.json      # JSON inputs in the harness codec's format; results as JSON lines
venue-node dump ./venue-data/journal.sbe        # a journal or result log as JSON lines
venue-node replay ./venue-data/journal.sbe out.sbe && cmp out.sbe ./venue-data/results.sbe
venue-node snapshot ./venue-data/journal.sbe snap.sbe
venue-node embed-snapshot ./venue-data/journal.sbe   # only while nothing has it open (no `serve` running)
venue-node recover ./venue-data/journal.sbe tail.results.sbe
venue-node cut-journal ./venue-data/journal.sbe ./venue-data/results.sbe   # same "no live serve" caveat
venue-node seed snap.sbe ./fresh-venue-data --capacity small
venue-node import core/target/differential/fuzz-90000.json golden.journal
```

`serve` also reads `VENUE_LISTEN`, `VENUE_FEED`, `VENUE_DATA`, `VENUE_GROUP`, `VENUE_NODE_ID` and `VENUE_TICK_MS`.

## Metrics and logs

`serve --metrics ADDR` (or `VENUE_METRICS`) serves Prometheus metrics at `http://ADDR/metrics`: inputs by type, the
journal's append and `fsync` times and bytes, the sequence, and the feed's subscribers and their backlog
(`deploy/observability/metrics.md`). None by default. They are counted around the engine, never in it, and the engine
is not timed. `VENUE_LOG_FORMAT=json` writes the log as JSON lines on standard output; otherwise it is readable text.

A replay on another architecture yields the same result log, but its `EngineStarted` names a different binary hash.
Build with the release profile to compare against a release node (overflow checks are on in both, blueprint §16).
