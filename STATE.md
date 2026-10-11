# Where the venue's state lives, and how it restarts, archives and reproduces

A map of every kind of state the venue holds, what makes it durable, what a restart actually does, and how a test
environment is founded on a chosen point in history rather than on genesis.

`AGENTS.md` covers the repository. `interfaces/README.md` covers the interfaces. This covers **state**, which cuts
across both and is otherwise only described in the places that implement it.

---

## 1. Five kinds of state, two of which are derived

| | Where it lives | Survives a restart? | Archived to S3 |
|---|---|---|---|
| **Journal** `journal.sbe` | `venue-core-0`, PVC `journal-venue-core-0` | yes, it *is* the record | yes, `journals/` |
| **Result log** `results.sbe` | beside the journal | yes, and re-derivable | yes, with the journal |
| **Engine state** | `venue-core-0` memory | **no** — rebuilt | as a `Snapshot` frame |
| **Reader views** (gateway, sim, observer, parlays) | each process's memory | **no** — rebuilt | no; derived |
| **Databases** (identity, oracle, parlays, treasury) | `venue-postgres-0`, PVC | yes | yes, `databases/` |
| **Audit log** | gateway PVC | yes | yes, `audit/` |

The two "no"s are the important ones, and they are not a gap. The engine's state and every reader's view are
**functions of the journal**. Losing them costs time, never information. Everything else is a record that has to
be kept because nothing can recompute it.

That is also why the durability work splits the way it does: the journal needs *trimming* (it grows forever and
is the only copy of itself), the databases need *copying* (they are small, and were the only copy of themselves),
and the views need neither.

---

## 2. The journal: snapshot, archive, cut

An append-only file of SBE frames, one per input, gap-free from sequence 1 (E-4.1). It grows at roughly
**35 MB/hour** on the demo venue, and before any of this existed it reached **3.2 GB**.

A **`Snapshot` frame** (template 53, spec §20) is the engine's whole logical state at one sequence, taken by a
*replaying* engine and never by the one serving traffic (E-20.10). Embedded into the journal itself (D-32), it is
what makes everything before it removable — recovery has somewhere to begin other than sequence 1.

Cutting is three steps, each of which refuses on its own:

```sh
venue-node export-cut  journal.sbe results.sbe ./segment     # writes what a cut would remove; changes nothing
interfaces/tools/journal_ship.py ./segment --to s3://BUCKET --environment prod
venue-node cut-journal journal.sbe results.sbe --require-shipped ./segment
```

`export-cut` is the complement of the cut's own filter, frame for frame, so the two cannot drift. It refuses
without a durable snapshot the result log already covers — **that refusal is the readiness test**: a journal has
no "closed" marker the way an audit file does, and only a sequence means anything.

`--require-shipped` exists for an *honest stale receipt*, not a forged one: a segment shipped at sequence 30,
after which the venue embedded a newer snapshot at 62. Every artifact checks out individually, and cutting to 62
against that segment destroys 30..62, which no archive holds.

**Measured on the live venue, 2026-09-25:** 437 MB → 9.7 MB, −97.8%. Bytes removed equalled bytes archived
exactly. The node then restarted by replaying **4 inputs instead of 1,878,320**.

A `Snapshot` of this venue is ~650 KB, which is why `MAX_FRAME` had to stop applying to it: the input bound is
64 KiB, and a venue that has traded for a few hours cannot snapshot itself under it (`core/node/src/frames.rs`).

**Reading it back.** `venue-node restore-cut SEGMENT JOURNAL RESULTS OUT` puts an archived segment back in front
of the journal it was cut from. It exists as much to be *tested* as to be run: an archive nobody has ever read
back is bytes, not a record.

The restored journal is deliberately **not** byte-identical to the one that was cut — `embed` appends wherever
the file ends, so the original carried its `Snapshot` at the tail, after inputs with higher sequences, while a
restored journal carries it in the middle. Position has never meant anything in a journal (§3.7 of the handoff).
What holds is that a replay of the restored journal produces the same result log as a replay of the original,
byte for byte, and that is what the test asserts.

---

## 3. What a restart actually does

**The node** (`choose_recovery`, `core/node/src/node.rs`) decides once at start:

- an embedded snapshot the result log already covers → restore from it, replay only what follows;
- otherwise → replay the journal from sequence 1.

The fallback is deliberate. Using a snapshot the result log does not cover would leave a gap in `results.sbe` that
a subscriber's backlog could never fill.

**Readers** reconnect to the feed and open by saying where they got to — eight little-endian bytes, the last
sequence held, or zero for everything — and are answered one byte saying whether that position was honoured. It is
honoured only when the journal can answer at **both** ends: not past what it has reached (a reader whose node was
replaced under it), and not below what it still carries after a cut. A position below the floor is *refused*
rather than served from wherever the journal now begins, which would skip everything in between while the ack
claimed success.

Why this matters more than it sounds: before readers could resume, every deploy restarted every reader into a full
genesis replay. With ~36.7M inputs and eight subscribers replaying at once on two vCPUs, the gateway took
**~102 minutes** to come back. After the cut, it is seconds.

**Asking for "everything" is not exempt from the floor, and used to be treated as if it were.** Position 0 means
"send me the whole history" — but on a journal that has been cut, the whole history is not what the node has any
more. Until this was fixed, a reader asking for 0 against a floor above 0 was answered with the same ack byte a
genuine from-genesis serve gets, and the client's own guard only ever inspected that byte when it had asked for a
position *greater than* zero — so the one reader that most needed to be told ("you asked for everything and
this is not everything") was built not to look. Every reader deployed today asks for 0 on a fresh start, because
none is founded on a pair (§8), so this was live risk under a healthy-looking venue: any pod restart after a cut
would silently show a view missing everything before it. Both halves are fixed now — the ack reflects whether
`asked_for` is actually covered, `asked_for == 0` included, and the client's guard no longer excuses zero. A
restart in this state now throws the view away and says so (a log line, not a metric or an alert), the same path
that already existed for a node being replaced under a reader. **It does not make the view complete** — nothing
does that except founding on a pair — it makes the incompleteness observable instead of silent.

---

## 4. Founding an environment on a chosen snapshot

The unit is a **snapshot pair**, taken at one sequence:

| half | file | who reads it |
|---|---|---|
| engine | `engine.snapshot` | the node — it is its starting state |
| reader | `view.json` | everything downstream — ladders, orders, positions, sleeves |

Two halves because of a dependency direction: `venue-link` depends on `venue-node`, so **the node cannot see a
reader's projection**. The engine's snapshot is not a view and cannot be turned into one without replaying.

```sh
venue-view snapshot journal.sbe ./pair --at trusted --results results.sbe [--from-pair ./previous]
venue-node seed ./pair/engine.snapshot ./venue-data --capacity small  # a journal holding one Snapshot frame
venue-gateway --from-view ./pair/view.json ...                        # and the readers
```

**A journal that has been `cut` cannot give a pair on its own — it needs the previous one.** This is the single
most important operational fact here, because the failure it prevents is silent. The engine half copes alone: it
restores from the snapshot the journal was cut to. The view half cannot. A view is built by applying units in
order from an empty state, and the units before the cut are gone, so replaying one from nothing yields a view
holding **only what followed the cut** — with no error, and a `sequence` that still matches the engine's. The
two halves would agree with each other and both be wrong. `--from-pair` names the previously published pair to
carry on from, and `pair::take` refuses a cut journal without one, and a previous view older than what the
journal still carries.

That also makes each pair depend on its predecessor, which is why there is a *chain* rather than independent
snapshots, and why a venue whose journal has been cut needs one pair published by hand before a scheduled job
can take over.

**`--at trusted` rather than a sequence.** A pair can only be taken as far as the *result log* reaches, which is
not as far as the journal: inputs that produce nothing leave no group behind, and `cut` drops the last
unconfirmed one. Asking for more does not fail — `View::apply` advances the view's sequence for an input whether
or not results came with it — so a pair taken at the journal's end can claim a sequence whose effects it never
saw. Both halves are pinned to what the results cover, and an explicit `--at` beyond it is refused.

`seed` refuses a data directory that already has a journal, so it cannot silently replace a real environment's
history. The node then starts at exactly that sequence with no history of its own before it — `founded_only`
recognises that case and trusts the snapshot unconditionally, because a freshly seeded journal has no real input
anywhere that an empty result log could have lost.

`--from-view` is taken by the gateway, sim, observer and parlays. **The treasury takes it too but uses only the
sequence**, not the view: its state is its own database records, which outlive a restart. That sequence is where
its feed resumes and the floor below which reconciliation draws no conclusions — without it, a founded treasury
reports every transfer older than the founding point as a command the engine never applied. It deliberately does
*not* fill those in from its own records, because reconciliation compares what the treasury believes against what
the journal independently says, and sourcing both from one place would make it agree with itself.

Pairs move between environments by tag:

```sh
interfaces/tools/snapshot_publish.py pair/engine.snapshot --to s3://BUCKET --environment prod --tag pre-load
interfaces/tools/snapshot_fetch.py --from s3://BUCKET --environment prod --tag pre-load --to seed.snapshot
```

**Proven end to end on production data:** a venue founded on the live journal's pair at sequence 109,018 served
markets *identical* to one that replayed the same 109,019 inputs from genesis — 26 instruments, same event, state
and class, zero differences, queried at the same instant.

---

## 5. The databases: copied, never pruned

The four databases are **append-only by construction**. Each installs a trigger that raises on `DELETE` and
`TRUNCATE` and `REVOKE`s those privileges, with tests asserting the refusal. Treasury's message says why: *"a
correction is a compensating entry, never an edit."*

They are also small: **~4 MB between them after half a day of trading, ~0.34 MB/hour**, against the journal's
35 MB/hour. They were never what filled the disk.

What they lacked was a copy. `interfaces/tools/db_archive.py` takes a verified `pg_dump` to
`databases/{environment}/`, on a CronJob per database, and **deletes nothing**. The dump is checked twice:
`pg_restore --list` proves it is a readable archive — a checksum cannot tell you it was truncated at source — and
then the object is read back and compared.

Deciding what may eventually be *removed* from a store is **interface 29** ("Stores → retention archive"), which
is planned, unbuilt, and answerable to the legal-hold register that outranks every retention period.

A dump of `identity` is **Restricted**: `members` is the venue's only personal data, and `outbox_mail` carries
addresses too. The destination bucket must block public access and encrypt at rest.

---

## 6. Everything in one bucket, under four prefixes

```
audit/transient/     gateway audit files      Glacier at 30 days, expires at 90
audit/kept/          gateway audit files      Glacier at 30 days, never expires
journals/{env}/      cut journal segments     no lifecycle rule
databases/{env}/     pg_dump archives         no lifecycle rule
snapshots/{env}/     snapshot pairs, by tag   no lifecycle rule
```

The prefixes are deliberately separate so a lifecycle rule written for one cannot reach another — a whole-bucket
`filter {}` once nearly expired production records. The flip side is that the three newer prefixes have **no rule
at all**, so their objects sit in Standard indefinitely. That is correct for history nothing should ever delete,
and it is a cost decision being made by omission.

Every object has a `.sha256` beside it, written after the upload has been read back and compared — nothing is
reported archived that has not been verified in place. A pair is four objects, because its halves carry
**separate** sidecars:

```
snapshots/prod/20260925T184939Z.engine.snapshot   .engine.sha256
snapshots/prod/20260925T184939Z.view.json         .view.sha256
```

The half is part of the tag rather than only the suffix, and that is not cosmetic: the sidecar's key is derived
from the tag, so two halves sharing one tag shared one sidecar, and the second upload silently overwrote the
first's digest. `journal_ship.py` names its two halves the same way, for the same reason. The newest pair is
found by sorting these tags, which is why a tag must be a UTC stamp — a descriptive one like `bootstrap` sorts
above every timestamp and would pin the chain to itself for ever.

---

## 7. What runs on a schedule, and how to tell it still is

Three CronJobs, all in the `venue` namespace, all off by default in the chart and on in prod:

| | when | what it does | chart key |
|---|---|---|---|
| `venue-archive` | hourly, `:17` | ships closed audit files, then unlinks them | `gateway.audit.archive` |
| `venue-dbarchive-*` | daily, 02:43 | one verified `pg_dump` per database | `database.archive` |
| `venue-snapshot` | daily, 03:13 | takes a pair and publishes it | `core.snapshots` |

**Only one of these complains when it stops.** The audit shipper's volume fills up and eventually the gateway
cannot write, so silence there becomes loud on its own. Nothing downstream of the other two fills up: a database
that is not being dumped and a venue that is not producing pairs look exactly like ones that are. Both failed
silently at least once while being built, and both times it was found by running the job by hand rather than
waiting for the schedule. **Do that after any change to either.**

```sh
kubectl -n venue create job --from=cronjob/venue-snapshot check-it
kubectl -n venue logs job/check-it -c take        # and -c previous, -c publish
```

Note the Job controller deletes a failed pod within seconds and takes its logs with it, so read the logs while
it runs, or the events afterwards (`kubectl -n venue get events`). What is actually in object storage is the
only answer that settles it:

```sh
aws s3 ls s3://BUCKET/snapshots/prod/     # newest pair, and its two sidecars
aws s3 ls s3://BUCKET/databases/prod/     # one dump per database per day
```

A job that refuses is doing its job. `venue-snapshot` refusing with *"was cut at N ... give the previously
published view half"* means the chain has no start — publish one pair by hand (§4) and it recovers by itself.

## 8. What is not done

- **A journal cut is a job you start, not one that runs unattended.** `core.cut` (`templates/journal-cut.yaml`,
  `interfaces/tools/journal_cut.py`) does the three commands for you: it stops the core, embeds a snapshot at the
  latest published pair, archives what the cut removes, cuts, and starts the core again, always, from a `finally`.
  The core is **down for the duration**, and there is still no lock on a journal, which is why the CronJob is
  suspended by default and each run is a deliberate `kubectl create job --from=cronjob/...`. It refuses, before
  stopping anything, without a published pair to cut at or without room on the disk to stage the segment.
- **No chart passes `--from-view`.** Founding works and is proven twice over, but no deployed service uses it, so
  every deploy still restarts readers into a replay. This is the largest remaining gap: it is what would make a
  deploy cost seconds rather than a full backlog.
- **Restoring a database is manual.** The dumps are verified restorable — `pg_restore --list` at archive time,
  and a real restore into a fresh database once — but nothing automates putting one back.
- **Interface 29 is unbuilt** — no store has a retention policy, only a copy.
- **A live node never embeds its own snapshot.** Taking a pair needs no downtime and is scheduled; *embedding*
  one and cutting still need the writer stopped.
