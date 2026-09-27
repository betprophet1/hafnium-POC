# venue-treasury

The mini venue's dummy treasury (blueprint §5, §11, §16), for laptops and test clusters only. It owns the money that
moves in and out of the venue: deposits, withdrawals and internal transfers, a double-entry ledger, and a fake bank
in place of real rails. Clients reach it through the private half of the gateway (`/v4/transfers`,
`interfaces/gateway/README.md`, deposits, withdrawals and internal transfers); the gateway authenticates the
account and passes the request on, holding nothing.

## What happens to a transfer

| | Deposit | Withdrawal | Internal (D-34, G-22) |
|---|---|---|---|
| Requested | handed to the fake bank: `pending` | `SleeveAdjust RECLAIM` to the sequencer group; if applied, handed to the fake bank: `pending`; if rejected, `failed` with `insufficient_available_cash` | `SleeveAdjust RECLAIM` from the source; if applied, `SleeveAdjust GRANT` to the destination at once — no bank leg; if the reclaim is rejected, `failed` with `insufficient_available_cash` |
| The bank settles | `SleeveAdjust GRANT`, then `settled` | `settled` | n/a — settles as soon as the grant applies |
| The bank fails | `failed`, `declined_by_bank` | `SleeveAdjust GRANT` back, then `failed`, `returned_by_bank` | n/a — a grant of a positive amount is never refused (E-7.2) |
| Cancelled before the bank decides | `cancelled` | `SleeveAdjust GRANT` back, then `cancelled` | never cancellable: there is no bank step to cancel while it is in flight, and it is usually already settled by the time a client could ask |

Once the bank has decided a transfer it cannot be cancelled (`409 TRANSFER_NOT_CANCELLABLE`), whether or not its notice
has reached the treasury yet. An internal transfer is never cancellable at all.

A reclaim or grant carries a command ID fixed by the transfer (`T` + the book's nonce + the transfer ID + `r` or
`g`), so sending it again after a restart is answered `DUPLICATE` (spec E-7.1) and moves nothing twice. A duplicate
says only that the command was decided, so the treasury follows the node's feed and takes the first outcome of each
of its commands from there.

## The ledger

Integer cents, debits positive, every posting balanced, so all balances sum to zero. Accounts:

| Account | Holds |
|---|---|
| `customer:{id}:cash` | what the account has brought in less what it has taken out (a credit balance: negative) |
| `bank:fbo_omnibus` | the fake bank's FBO omnibus: the seed's grants and deposits settled, less withdrawals paid out |
| `transit:deposits` | deposits the bank has settled and the sequencer group has not granted yet |
| `transit:withdrawals` | withdrawals reclaimed and not yet paid out or granted back |
| `transit:internal_transfers` | internal transfers reclaimed from their source and not yet granted to their destination (D-34, G-22); no bank or FBO leg, but still needed, for the same reason the other two are: the reclaim and the grant are two commands applied at two moments, so the ledger needs somewhere to hold the money balanced in between |

The seed's grants are posted once, when a book is created, as money the fake bank holds for each account, so the
omnibus always equals the customers' cash plus what is in transit (§11's invariant). Trading moves cash inside the
sequencer group, which this ledger does not follow yet: it is the treasury's account of
what crossed the venue's edge, not of what each account now holds.

## Records in PostgreSQL

The ledger, transfers, the fake bank's book and reconciliation are kept in PostgreSQL (`migrations/`, through
`core/store`), in the database and schema `treasury`; the decisions are S-6 to S-12 of `core/store/decisions.md`.

| Table | Holds | Changes |
|---|---|---|
| `book` | the book's nonce, which prefixes its command IDs | never |
| `ledger_accounts` | each ledger account, its kind, and the venue account a customer's belongs to | never |
| `postings`, `entries` | each posting (memo, transfer, idempotency key) and its lines, `bigint` cents | never |
| `transfers` | each transfer as it now stands; `(account_id, client_transfer_id)` unique | where it stands, never its terms |
| `transfer_events` | every state each transfer has been in, numbered | never |
| `bank_book`, `bank_receipts`, `bank_cancellations`, `bank_notices` | the fake bank's own book | never |
| `recon_runs`, `recon_breaks`, `recon_resolutions` | each run's report, each break as found, each resolution | never |

What the database itself enforces, whatever writes to it:

- **Double entry.** A deferred constraint trigger checks, when a transaction commits, that every posting written in
  it has at least two lines and that they sum to zero; a transaction that breaks this is rolled back whole.
- **Append-only.** Statement triggers refuse `UPDATE`, `DELETE` and `TRUNCATE` on every table of record; a row trigger
  refuses a change to a transfer's terms, or a version that does not follow the last. A correction is a compensating
  posting, never an edit. `PUBLIC` holds no such privileges either; the owning role keeps them only because
  PostgreSQL's foreign-key checks need them, and the triggers bind it.
- **Idempotency.** A posting's key is its transfer and the accounts it moves money between (`transfer:7:bank:fbo_omnibus>transit:deposits`), a step that happens once per transfer; an opening balance's is `opening:` and its accounts. A write
  repeated after a lost acknowledgement finds the posting already there with the same lines and posts nothing; the same
  key with other lines is refused. A transfer's state already recorded is not recorded again.
- **Integer cents** in `bigint` throughout, and views `trial_balance` (per account) and `ledger_check` (total, number
  of postings, whether it balances).

Every change is written in one transaction before anything that depends on it is done. The treasury keeps a working
copy in memory for reads and reconciliation, rebuilt from the records at start, and read again whenever a write failed
(its outcome unknown) before anything else is written. The invariant `ledger_sums_to_zero` sums the ledger of record
in the database, after the run's own fixes, and also checks that the working copy agrees with it.

Earlier versions kept the book in JSON-lines files; `venue-treasury import --ledger FILE` moves one (and the bank's
`FILE.bank.jsonl` beside it) into empty records, each line becoming the rows it would have been, and checks that the
result balances. The treasury no longer reads or writes the files.

### When the database cannot be reached

- **At start** the treasury tries again with backoff (1 s doubling to 30 s), saying on stderr what it cannot reach and
  why, and does not listen until it can; then it waits the same way for its schema to be at this build's migration,
  which `venue-treasury migrate` applies (the chart's migrate job). A schema newer than the build, or an edited
  migration, stops it.
- **While running**, a new transfer, a cancellation or an operator's resolution is refused with `503 UNAVAILABLE` and
  nothing is done. Transfers already under way wait where they are: a bank decision or a sleeve answer is taken up once
  it can be written. Reads are answered from the working copy. Reconciliation runs are postponed and tried again every
  five seconds. `/healthz` answers 503, so the pod is taken out of its Service and the gateway answers transfers with
  503 until the database is back; nothing needs restarting.

## The fake bank

`dummy_bank` is decided after a delay drawn between `--min-delay-ms` and `--max-delay-ms`, and fails
`--failure-per-mille` of transfers; `dummy_instant` settles at once; `dummy_failing` always fails. Each decision is a
function of `--seed` and the transfer alone, so a run can be repeated.

The bank stands for another company's system, so it keeps a book of its own beside the treasury's (tables of its own,
`bank_*`, sharing no keys with the treasury's): each transfer it received, when and which way it decides, whether it was cancelled,
and which of its notices it has sent. The treasury learns a decision only from the bank's notice, and the bank's
statement (`GET /v1/bank/statement`) gives the omnibus's opening and closing balance for a period and a line for each
transfer received, decided or cancelled in it. A bank book opened beside an older treasury book opens with the
ledger's omnibus balance and takes over the transfers the ledger has at the bank; a bank book that belongs to another
treasury book is refused.

## Losing messages on purpose

Reconciliation needs something to find. `--faults demo` loses a few percent of each kind of message below; `none`, the
default, loses nothing. Each share can be set on its own, in thousandths, over either profile. Which messages are lost
is drawn from `--seed` and the transfer or command ID, so a run can be repeated.

| Flag | What is lost | What the treasury sees |
|---|---|---|
| `--lost-notice-per-mille` | the bank's notice of its decision, never sent | the transfer pending at the bank for ever |
| `--late-notice-per-mille`, `--late-notice-ms` | the notice, sent that long after the decision | pending well past the decision, then a notice for a transfer already booked |
| `--duplicate-notice-per-mille` | nothing: the notice comes twice, two seconds apart | a second notice for a transfer already past the bank |
| `--lost-answer-per-mille` | the answer to a reclaim or grant the sequencer group applied | a command sent and never answered |
| `--lost-command-per-mille` | the reclaim or grant itself, before the sequencer group | the same |

A notice's fate is drawn when the bank takes the transfer and written into the bank's book, so restarting with other
faults neither brings a lost notice back nor loses one already on its way. Only a command's first send can be lost.
A command the node client could not send, or whose answer it could not read, is an error the treasury sees and retries
within a second, as before; a lost answer is the kind of loss nothing notices, so the transfer waits *in doubt*, and
only reconciliation moves it on. A notice for a transfer already past the bank is ignored when it comes: acting on it
would post it twice.

## Reconciliation

A run (`recon.rs`) starts every `--reconcile-every-ms` (default 60 s; 0 runs only when asked), and when an operator
asks. Everything it reads is in memory: the bank's record, and the journal as the node's feed replays it (every
`SleeveAdjust` of this book's command prefix, with its first outcome). Each problem it finds is a *break*: a kind, the
transfer and account, and what was found. What it can fix safely it fixes at once and marks resolved, with a note of
what it did; the rest stays open for an operator.

| Break | Found when | Resolution |
|---|---|---|
| `bank_decision_unbooked` | the statement shows a decision older than `--reconcile-grace-ms` (default 10 s) for a transfer the treasury has at the bank | booked as if the notice had come, the posting's memo naming the run and break |
| `bank_no_record` | the treasury has a transfer at the bank and the bank has no record of it | handed to the bank again, which takes a transfer once |
| `bank_late_notice`, `bank_duplicate_notice` | a notice came for a transfer already past the bank | none needed: it was ignored when it came; reported resolved |
| `bank_outcome_mismatch` | the treasury and the bank disagree on how a transfer ended | open |
| `bank_amount_mismatch` | the ledger's omnibus postings for a transfer differ from the bank's movement for it | open |
| `bank_unknown_transfer` | the bank has a transfer the treasury never made | open |
| `engine_answer_lost` | a command is in doubt and the journal has its outcome | the outcome applied, as if the answer had come |
| `engine_command_lost` | a command is in doubt and the journal does not have it | sent again under the same command ID; if it did arrive after all, it is answered `DUPLICATE` and moves nothing (spec E-7.1) |
| `engine_unexpected_command` | the journal applied a command of this book's the transfer should never have sent | open |
| `engine_command_missing` | the treasury has a command applied at a sequence the feed has passed, and the journal does not have it | open |
| `engine_outcome_mismatch`, `engine_amount_mismatch` | the journal's outcome, amount, account or reason differ from the treasury's | open |
| `engine_unknown_command` | the journal has a command with this book's prefix and no transfer of it | open |
| `invariant_…` | an invariant below does not hold | open |

A finding that matches an open break (same kind, transfer and account) is that break, still open, not a new one; an
open break a run no longer finds is closed with the note `not found again by run N`. Every fix checks first that the
transfer is still where the break found it, so a run repeated, or a late notice after a booking, never moves money
twice. Breaks and runs are in the records, so they survive a restart, and run numbers never repeat.

### Invariants

Checked at the end of every run, after its fixes:

- **`ledger_sums_to_zero`**: the ledger of record, summed in the database, is zero, and the working copy agrees.
- **`omnibus_covers_customer_cash`**: the bank's own omnibus balance, less the decisions it has made that are still
  within the grace period and not booked, equals the customers' cash plus money in transit in the ledger. This is
  §11's "sum of customer cash equals the FBO balance minus in-flight transfers", with the balance taken from the bank
  rather than the ledger's copy of it, which would make it the same check as the one above.
- **`journal_matches_treasury`**: for every account, what the treasury has granted less what it reclaimed, over
  transfers not in flight, equals what the journal applied under those transfers' command IDs.

**A breach is reported and flagged, and halts nothing.** The run's status is `breach`, the breach is an open break
like any other, and the treasury logs `INVARIANT BREACHED` on stderr. §11 halts new listings and pages an operator;
here there is no one to page, listings belong to the dummy oracle rather than the treasury, and the demo's losses are
injected on purpose, so a halt would stop a demo to report what it was asked to show. Refusing withdrawals on a breach
was considered and left out for the same reason. A production treasury would do both, and would not resolve a break
without a person when the fix moves money it cannot find in a statement.

### Where this sits relative to blueprint §11

§11 reconciles against the bank daily and against a full journal replay nightly, and checks its invariants
continuously. The demo keeps the three checks and changes only their cadence and scope: both reconciliations run in
one pass on one schedule, a minute by default, because a demo that shows a break a day later shows nothing; the
statement is the bank's whole history rather than the day's file, because there are a few hundred transfers rather
than millions; the "full journal replay" is the node's feed read from the start, which is every input the sequencer
group applied, and is matched only for the treasury's own `SleeveAdjust` commands, because trading moves cash inside
the sequencer group and this ledger does not follow it (see Known shortcuts). Automatic fixes are limited to the ones
that repeat a step the treasury would have taken had the lost message arrived; everything that needs a judgement
about which side is wrong is left to an operator.

## Its own API

Not interface 1, and never exposed beyond the gateway: it trusts the account in the path.

| Call | What it does |
|---|---|
| `POST /v1/accounts/{accountId}/transfers` | a transfer request, body and answer as `POST /v4/transfers`; answers once the transfer is at the bank or finished, or after five seconds |
| `GET /v1/accounts/{accountId}/transfers?limit=` | as `GET /v4/transfers` |
| `GET`, `DELETE /v1/accounts/{accountId}/transfers/{transferId}` | as the same `/v4` calls |
| `GET /v1/updates` | every change from the moment of connecting, one JSON line each (`{"accountId", "transfer"}`), and an empty line every 15 s |
| `GET /v1/ledger` | the trial balance |
| `GET /v1/bank/statement?from=&to=` | the fake bank's statement of the omnibus, times in wall-clock milliseconds (default: from its opening to now) |
| `GET /v1/reconciliation` | the schedule, the faults being injected, the last run, open breaks and the latest resolved ones |
| `GET /v1/reconciliation/runs?limit=` | runs, newest first: status (`clean`, `resolved`, `breaks`, `breach`), what each found and resolved, its invariants |
| `POST /v1/reconciliation/runs` | runs now; answers with the run, or `{"requested": true}` if the treasury was busy for ten seconds |
| `GET /v1/reconciliation/breaks?state=open\|resolved\|all&limit=` | breaks, newest first |
| `POST /v1/reconciliation/breaks/{breakId}/resolve` | `{"note": "..."}`: an operator marks a break resolved; `409 BREAK_ALREADY_RESOLVED` if it is. A break whose cause is still there is found again by the next run, as a new break |
| `GET /healthz` | |

The web page (`venue-sim web --treasury`) passes `GET /v1/reconciliation` and `POST /v1/reconciliation/runs` through
under `/treasury` for the account tab's reconciliation section, and nothing else of this API.

## Running it

```sh
export VENUE_DATABASE_URL='postgres://postgres:dev@127.0.0.1:5432/postgres?sslmode=disable'
target/release/venue-treasury migrate
target/release/venue-treasury --ingress 127.0.0.1:7101 --feed 127.0.0.1:7102 \
  --catalogue ../deploy/charts/venue-mini/files/markets.json
target/release/venue-gateway --ingress 127.0.0.1:7101 --feed 127.0.0.1:7102 \
  --catalogue ../deploy/charts/venue-mini/files/markets.json --treasury 127.0.0.1:8083
```

With messages lost and a run every five seconds, to watch reconciliation at work:

```sh
target/release/venue-treasury --ingress 127.0.0.1:7101 --feed 127.0.0.1:7102 \
  --catalogue ../deploy/charts/venue-mini/files/markets.json \
  --faults demo --reconcile-every-ms 5000 --reconcile-grace-ms 3000
curl -s 127.0.0.1:8083/v1/reconciliation
curl -s -X POST 127.0.0.1:8083/v1/reconciliation/breaks/7/resolve -d '{"note": "checked with the bank"}'
```

## Known shortcuts

- **Reconciliation covers the treasury's edge only.** The journal check matches the treasury's own sleeve grants and
  reclaims; fills, fees and payouts move cash inside the sequencer group and are not in this ledger, so an account's
  cash in the ledger is not its available cash in the engine, and nothing compares the two.
- **Breaks and runs live beside the ledger.** No separate case store: a busy schedule adds a row a run.
- **One writer.** The working copy assumes this treasury is the only one writing its records (the chart runs one pod,
  replaced rather than rolled); a second would find its writes refused on transfer IDs and idempotency keys rather than
  doubling money, but would not serve correctly.
- **The records are the outbox.** A reclaim or grant to send is a transfer's stage in `transfers`, written before it
  is sent and done when its answer or the journal says so (blueprint §18); there is no separate outbox table.
- **The journal comes from the feed**, read from the start each time the treasury starts. Until the feed has passed a
  transfer's sequence a missing command is not reported, and a command in doubt the feed has not reached yet is sent
  again, which is safe but reads as `engine_command_lost` rather than `engine_answer_lost`.
- **No top-up policy.** One sequencer group, so a deposit is granted whole to its sleeve; §5's sleeves per group
  topped up as they drain need more than one group to mean anything.
- **No limits, holds or KYC.** Only a largest single transfer (`--max-cents`).
