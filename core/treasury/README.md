# venue-treasury

The mini venue's dummy treasury (blueprint §5, §11, §16), for laptops and test clusters only. It owns the money that
moves in and out of the venue: deposits and withdrawals, a double-entry ledger, and a fake bank in place of real rails.
Clients reach it through the private half of the gateway (`/v4/transfers`, `interfaces/gateway/README.md`, deposits
and withdrawals); the gateway authenticates the account and passes the request on, holding nothing.

## What happens to a transfer

| | Deposit | Withdrawal |
|---|---|---|
| Requested | handed to the fake bank: `pending` | `SleeveAdjust RECLAIM` to the sequencer group; if applied, handed to the fake bank: `pending`; if rejected, `failed` with `insufficient_available_cash` |
| The bank settles | `SleeveAdjust GRANT`, then `settled` | `settled` |
| The bank fails | `failed`, `declined_by_bank` | `SleeveAdjust GRANT` back, then `failed`, `returned_by_bank` |
| Cancelled at the bank | `cancelled` | `SleeveAdjust GRANT` back, then `cancelled` |

A reclaim or grant carries a command ID fixed by the transfer (`T` + the book's nonce + the transfer ID + `r` or
`g`), so sending it again after a restart is answered `DUPLICATE` (spec E-7.1) and moves nothing twice. A duplicate
says only that the command was decided, so the treasury follows the node's feed and takes the first outcome of each
of its commands from there.

## The ledger

Integer cents, debits positive, every posting balanced, so all balances sum to zero. Accounts:

| Account | Holds |
|---|---|
| `customer:{id}:cash` | what the account has brought in less what it has taken out (a credit balance: negative) |
| `bank:fbo_omnibus` | the fake bank's FBO omnibus: deposits settled less withdrawals paid out |
| `transit:deposits` | deposits the bank has settled and the sequencer group has not granted yet |
| `transit:withdrawals` | withdrawals reclaimed and not yet paid out or granted back |
| `house:demo_opening_balances` | the seed's grants, posted once when a book is created, which no bank ever held |

Trading moves cash inside the sequencer group, which this ledger does not follow yet: it is the treasury's account of
what crossed the venue's edge, not of what each account now holds.

The book is an append-only file of JSON lines. Its first line names the book's nonce; each later line is a transfer
as it now stands with the postings that took it there, written before anything that depends on it is done. On start the
treasury replays it, drops a last line cut short by a stop mid-write, and carries on from where each transfer was.

## The fake bank

`dummy_bank` is decided after a delay drawn between `--min-delay-ms` and `--max-delay-ms`, and fails
`--failure-per-mille` of transfers; `dummy_instant` settles at once; `dummy_failing` always fails. Each decision is a
function of `--seed` and the transfer alone, so a run can be repeated.

## Its own API

Not interface 1, and never exposed beyond the gateway: it trusts the account in the path.

| Call | What it does |
|---|---|
| `POST /v1/accounts/{accountId}/transfers` | a transfer request, body and answer as `POST /v4/transfers`; answers once the transfer is at the bank or finished, or after five seconds |
| `GET /v1/accounts/{accountId}/transfers?limit=` | as `GET /v4/transfers` |
| `GET`, `DELETE /v1/accounts/{accountId}/transfers/{transferId}` | as the same `/v4` calls |
| `GET /v1/updates` | every change from the moment of connecting, one JSON line each (`{"accountId", "transfer"}`), and an empty line every 15 s |
| `GET /v1/ledger` | the trial balance |
| `GET /healthz` | |

## Running it

```sh
target/release/venue-treasury --ingress 127.0.0.1:7101 --feed 127.0.0.1:7102 \
  --catalogue ../deploy/charts/venue-mini/files/markets.json --ledger /tmp/venue/treasury.jsonl
target/release/venue-gateway --ingress 127.0.0.1:7101 --feed 127.0.0.1:7102 \
  --catalogue ../deploy/charts/venue-mini/files/markets.json --treasury 127.0.0.1:8083
```

## Known shortcuts

- **No reconciliation.** Blueprint §11 reconciles the ledger against the bank daily and against a journal replay
  nightly; there are no bank statements and no replay check yet.
- **No top-up policy.** One sequencer group, so a deposit is granted whole to its sleeve; §5's sleeves per group
  topped up as they drain need more than one group to mean anything.
- **No limits, holds or KYC.** Only a largest single transfer (`--max-cents`).
