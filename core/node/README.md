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
- **Recovery.** At start the journal is replayed through a fresh engine. A torn final frame left by a crash is
  cut off.
- **Fail-stop (spec §18).** The process exits. Restarting replays the journal and stops at the same input,
  which is what E-18.2 requires; recovery is an operator decision.

- **Feed.** `--feed` (default `0.0.0.0:7102`), a development stand-in for interface 12. A reader receives every
  input with its results, from the start of the journal, then live, as units: the input frame, its result frames,
  an empty frame. `TimeTick`s that changed nothing are left out. The web page and the bots in `core/sim` read it.

## Framing

Journal files, result logs and the TCP ingress all use one framing: a little-endian `u32` length, then that many
bytes of SBE (`interfaces/sbe/venue-journal.xml`), message header first.

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

**Not yet wired into `serve`'s own recovery.** `serve` still always truncates `results.sbe` and rebuilds it whole
by replaying from sequence 1 at every start, embedded snapshot or not (harmlessly skipping one either way). What
durability buys is a result log a restart could trust instead of throwing away — `serve` doesn't do that yet:
choosing between "trust the preserved file up to `repair_results`'s answer, then replay only the tail" and
today's full rebuild, and picking where to resume the engine from (an embedded snapshot at or before that point,
or genesis) are still to be connected. That is the next piece of the capacity brief.

## Commands

```sh
venue-node serve --data ./venue-data --listen 127.0.0.1:7101
venue-node send 127.0.0.1:7101 inputs.json      # JSON inputs in the harness codec's format; results as JSON lines
venue-node dump ./venue-data/journal.sbe        # a journal or result log as JSON lines
venue-node replay ./venue-data/journal.sbe out.sbe && cmp out.sbe ./venue-data/results.sbe
venue-node snapshot ./venue-data/journal.sbe snap.sbe
venue-node embed-snapshot ./venue-data/journal.sbe   # only while nothing has it open (no `serve` running)
venue-node recover ./venue-data/journal.sbe tail.results.sbe
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
