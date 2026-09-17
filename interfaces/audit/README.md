# Edge audit logs

Draft v0. One file format for what a client sent to the venue's edges and what it was sent back: the FIX adapter
(`adapters/fix`) and the REST and WebSocket gateway (`core/gateway`). `interfaces/tools/audit_verify.py` checks a
file against it.

## Why the edges keep one

The regulatory audit trail must reconstruct trading from every order, modification and cancellation, protected from
alteration and accidental loss, and records are kept at least five years (blueprint §12). The node's journal already
records every sequenced input and its results. What it cannot record is what never reached it: a refused token, a
malformed message, a flood limit, a route that does not exist. Those rejections are logged at the gateway for
disputes (§12, flood protection), and these files are where. They also hold what a client actually sent, byte for
byte where it can be, before the edge translated it into the canonical message, and what it was actually told.

The journal stays the authority on what happened in the market. An audit log is the authority on what was said at the
edge.

## Files

- **One line, one record.** UTF-8 text, each line a JSON object terminated by `\n`. JSON escapes every control
  character, so no record spans lines. Lines are only ever appended.
- **One file per UTC day per component instance**, named
  `<component>.<instance>.<YYYY-MM-DD>.<segment>.jsonl`, for example `gateway.gateway-0.2026-09-17.0000.jsonl`.
  `component` is `fix-adapter` or `gateway`; `instance` is the process's instance name (`--audit-instance`, by default
  the host name) with anything but letters, digits, `-` and `_` replaced by `-`. A process that starts again on the same
  day opens the next segment (`0001`, ...) rather than appending to a file another process wrote.
- **Rotation.** At the first record of a new UTC day, or within a second of midnight when nothing is being recorded,
  the file is closed with a `close` record and the next day's file is opened. A clean shutdown closes the file too.
  A record's day is the day of its `ts`; a record taken just before midnight and written just after it goes in the
  file that is open, so a file can end with a moment of the next day and never begins with one of the previous.
- **Sealing after a crash.** A process that stops without closing its file leaves it without a `close` record. The
  next process of the same component and instance, at start, appends one with `reason: recovered` to every such file
  whose last line is complete, chained like any other record. A file whose last line was cut off part-way is left
  exactly as it is and reported on standard error; the verifier reports the cut line.
- **Closed files are final.** Nothing is ever written to a file after its `close` record. A shipper takes a file once
  its last line is a `close` record, and needs nothing else to verify it.

## Records

Every record has these fields; key order carries no meaning.

| Field | |
|---|---|
| `ts` | UTC wall-clock time, `YYYY-MM-DDTHH:MM:SS.ffffffZ` (microseconds). Read from the host's clock when the record is taken (for a FIX message, Artio's timestamp of it). It is not the sequencer's time and is used for no logic. Lines are in the order they were written, which for records taken on different threads can differ by milliseconds from the order of their `ts`. |
| `n` | The record's number in its file, from 0. |
| `component`, `instance` | Which process wrote it. The same on every line of a file. |
| `kind` | What the record is (below). |
| `prev` | Tamper evidence: the SHA-256, in lower-case hex, of the previous line's bytes without its `\n`. 64 zeros on the first line. |

Records about a message also have `dir`: `in` for what a client sent, `out` for what was sent to it. Records about a
connection rather than a message (opened, refused, closed) have no `dir`.

### Every file

| `kind` | Fields |
|---|---|
| `open` | Always line 0. `format` (`venue-audit/1`), `file` (its own name), `pid`, and `after`: the previous file of this component and instance, as `{"file": name, "last": hash of its close line}`, or `{"file": name, "sealed": false}` when that file could not be sealed, or absent when there is none. `after` chains files together for a later manifest; each file still verifies alone. |
| `close` | Always the last line of a closed file. `reason` (`rotate`, `shutdown` or `recovered`) and `records`, the number of lines before it. |

### FIX adapter (`component: fix-adapter`)

Every FIX message of every session, in both directions, session-level messages included: Logon, Heartbeat,
TestRequest, ResendRequest, SequenceReset, Reject and Logout as well as orders and reports, and the messages of
connections whose Logon was refused.

| `kind` | Fields |
|---|---|
| `fix` | `dir`; `conn` (Artio's connection ID), `session` (Artio's session ID), `sender` and `target` (SenderCompID 49 and TargetCompID 56 as the message carries them), `msgType` (35), `seq` (34), `msg` (the whole message, SOH shown as `\|`, secrets masked), and `status` when Artio did not mark the message OK (`AUTH_REJECT` on the Logon of a refused connection). `session` is -1 on a connection that never logged on. |
| `gateway-login` | The adapter's login to the gateway for a FIX Logon: `conn`, `sender`, `peer` (the firm's `address:port`), and either `accountId` and `gatewaySession` (the `sessionId` of the gateway's `welcome`), or `refused` with the reason. `gatewaySession` is what ties a FIX session to the gateway's own audit records of the WebSocket the adapter holds for it. |
| `disconnect` | A connection ended, whether or not it logged on: `conn`, `session`, `reason` (Artio's, such as `REMOTE_DISCONNECT` or `FAILED_AUTHENTICATION`). |

The adapter does not log the JSON it exchanges with the gateway: the gateway's audit log has every frame of that
WebSocket, under the `gatewaySession` above.

### Gateway (`component: gateway`)

Every record has `conn` (a connection number, unique within the process), `peer` (`address:port` as the gateway saw
it; behind a load balancer, the balancer) and `half` (`public` or `private`); once a session or request is
authenticated, `account`; on a WebSocket once it has one, `session` (the engine session ID the gateway gave it).

| `kind` | Fields |
|---|---|
| `http-request` | `dir: in`. `line` (the request line), `headers` (every header, names in lower case, secrets masked), `body` (as text). Or, for bytes that can never be a request, `line` (what could be read of it) and `unreadable` (why). |
| `http-response` | `dir: out`. `status` and `body`. |
| `ws-open` | A WebSocket upgrade completed: `path`. |
| `ws-refused` | A WebSocket upgrade that failed its handshake: `path`. |
| `ws-frame` | `dir`, and `msg`, the text frame (secrets masked); for a binary frame from a client, `binary` (its length) instead. |
| `ws-close` | The connection ended: `reason` (`closed` by the client or the network, `heartbeat missed`, `too far behind`, or `ended` for anything else). |

What is recorded on each half:

| | Private half | Public half |
|---|---|---|
| REST | every request and its response | every request and its response |
| WebSocket | open, close; every frame from the client; every frame to it (replies, errors, executions, account updates, quotes, transfers) | open, close; every frame from the client except `heartbeat`; every `error` sent to it |

Market data (book, trade and market messages) is not recorded: it is the same for every viewer, high in volume, and
reconstructable from the journal. Refusals are always recorded: every 4xx and 5xx response, and every `error` frame,
including flood limits and failed logins.

### Masking

Secrets are replaced by `***` before a record is taken; nothing unmasked is ever queued or written.

| Where | What |
|---|---|
| FIX | the values of Password (554), NewPassword (925), EncryptedPassword (1402) and EncryptedNewPassword (1404). Username (553) is an identity, not a secret, and is kept. A masked message's BodyLength (9) and CheckSum (10) are left as they were sent, so they no longer match. |
| HTTP headers | `authorization`, `proxy-authorization` and `cookie`: the value becomes its scheme and `***` (`Bearer ***`), or `***` when it has no scheme. |
| JSON, in HTTP bodies and WebSocket frames | the string value of every member named `token`, wherever it appears, found textually so that a frame that is not valid JSON is masked too. |

## Writing

- **Off the hot path.** The code that serves clients (the gateway's event loops, the adapter's Aeron subscriber) builds
  a record and puts it on a bounded in-memory queue. One writer thread per process takes records off it, chains them,
  writes each line with one `write`, and syncs the file once for everything it took together.
- **fsync** is on by default (`--audit-fsync true`) and can be turned off for tests. With it on, a record is durable
  within one batch of being written; with it off, when the operating system writes it.
- **A client is not made to wait for the disk.** Its answer can leave before the record of it is durable. A record is
  taken before the answer is sent, so a process that stops in between can lose the tail of a batch but never records
  an answer out of order with its request.
- **A full queue is back-pressure, never a drop.** The thread that would queue a record waits for room. If there is no
  room for 10 seconds (the disk has hung), that is treated as a write failure.
- **A write failure stops the process.** Failing to create, write, sync or rotate a file is printed to standard error
  and the process exits with status 74 (`EX_IOERR`). An audit log that silently stops is worse than none, and refusing
  traffic while staying up would still need its refusals recorded somewhere. Exiting closes every connection, which
  cancels the orders of every session that asked for cancel-on-disconnect, and leaves the restart to the supervisor;
  if the disk is still full the process fails to start, loudly. A process that cannot open its audit directory at
  start does not start.

## Verifying

```sh
python3 interfaces/tools/audit_verify.py FILE...            # every chain; exit 1 at the first broken line
python3 interfaces/tools/audit_verify.py --closed FILE...   # and each file must end with its close record
```

A file verifies when every line is a JSON object; line 0 is an `open` record naming the file's component and
instance; `n` counts from 0; every `prev` is the hash of the line before it (zeros on line 0); every line has the
same `component` and `instance`; and a `close` record, if there is one, is the last line and counts the lines before
it. The verifier prints the first line that breaks any of these, and whether the file is closed.

The chain shows that a file has not been altered or cut short in the middle, and a `close` record that it was not cut
short at the end. It does not show who wrote it: anyone able to rewrite a whole file can rewrite its chain. That is
what the archive's signed, chained manifests and compliance-mode object lock are for, once a shipper takes closed
files there; none of that is built yet.
