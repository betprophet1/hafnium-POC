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
edge. Where the two meet — the venue telling an account what the engine did — the log keeps a reference into the
journal rather than a second copy of it (below), and the reference carries the hash that proves the copy it stands
for.

## Files

- **One line, one record.** UTF-8 text, each line a JSON object terminated by `\n`. JSON escapes every control
  character, so no record spans lines. Lines are only ever appended.
- **One file per UTC day per component instance**, named
  `<component>.<instance>.<YYYY-MM-DD>.<segment>.jsonl`, for example `gateway.gateway-0.2026-09-17.0000.jsonl`.
  `component` is `fix-adapter` or `gateway`; `instance` is the process's instance name (`--audit-instance`, by default
  the host name) with anything but letters, digits, `-` and `_` replaced by `-`. A process that starts again on the same
  day opens the next segment (`0001`, ...) rather than appending to a file another process wrote.
- **One live process per instance.** Two processes writing one instance's files at once would each seal and append
  to the other's open file and break both chains. The gateway holds an exclusive lock on `.<component>.<instance>.lock`
  in the directory for as long as it runs, which the operating system releases however the process ends, and refuses
  to start while another process holds it: gateways sharing a directory each need an instance of their own.
- **Rotation.** At the first record of a new UTC day, or within a second of midnight when nothing is being recorded,
  the file is closed with a `close` record and the next day's file is opened. A clean shutdown closes the file too
  (the FIX adapter's). The gateway has no clean shutdown: stopped by a signal, it leaves its file open, and the next
  gateway of the same instance seals it.
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
| `open` | Always line 0. `format` (`venue-audit/2`), `file` (its own name), `pid`, `renderer` (the version of the gateway's rules for the frames it derives from the journal, below; absent from a FIX adapter's files, which derive nothing), `build` (the version of the process that wrote the file), and `after`: the previous file of this component and instance, as `{"file": name, "last": hash of its close line}`, or `{"file": name, "sealed": false}` when that file could not be sealed, or absent when there is none. `after` chains files together for a later manifest; each file still verifies alone. |
| `close` | Always the last line of a closed file. `reason` (`rotate`, `shutdown` or `recovered`) and `records`, the number of lines before it. |

### FIX adapter (`component: fix-adapter`)

Every FIX message of every session, in both directions, session-level messages included: Logon, Heartbeat,
TestRequest, ResendRequest, SequenceReset, Reject and Logout as well as orders and reports, and the messages of
connections whose Logon was refused.

| `kind` | Fields |
|---|---|
| `fix` | `dir`; `conn` (Artio's connection ID), `session` (Artio's session ID), `sender` and `target` (SenderCompID 49 and TargetCompID 56 as the message carries them), `msgType` (35), `seq` (34), `msg` (the whole message, SOH shown as `\|`, secrets masked), and `status` when Artio did not mark the message OK (`AUTH_REJECT` on the Logon of a refused connection). `session` is -1 on a connection that never logged on. |
| `gateway-login` | The adapter's login to the gateway for a FIX Logon: `conn`, `sender`, `peer` (the firm's `address:port`), and either `accountId`, `gatewaySession` (the `sessionId` of the gateway's `welcome`) and, for an API key, `key` (its ID), or `refused` with the reason. `gatewaySession` is what ties a FIX session to the gateway's own audit records of the WebSocket the adapter holds for it. |
| `disconnect` | A connection ended, whether or not it logged on: `conn`, `session`, `reason` (Artio's, such as `REMOTE_DISCONNECT` or `FAILED_AUTHENTICATION`). |

The adapter does not log the JSON it exchanges with the gateway: the gateway's audit log has every frame of that
WebSocket, under the `gatewaySession` above.

### Gateway (`component: gateway`)

Every record has `conn` (a connection number, unique within the process), `peer` (`address:port` as the gateway saw
it; behind a load balancer or an ingress, the proxy) and `half` (`public` or `private`); `client`, when a trusted
proxy named the client it forwarded for (below); once a session or request is
authenticated, `account`, and `key` — the API key's ID, or the member session's, whose prefixes tell them apart —
when it was authenticated with one rather than a dev token; on a
WebSocket once it has one, `session` (the engine session ID the gateway gave it).

| `kind` | Fields |
|---|---|
| `http-request` | `dir: in`. `line` (the request line), `headers` (every header, names in lower case, secrets masked), `body` (as text). Or, for bytes that can never be a request, `line` (what could be read of it) and `unreadable` (why). |
| `http-response` | `dir: out`. `status`, and `body` (as text, secrets masked); on the public half, for a status below 400, `length` (the body's length in bytes as sent) and `sha256` (the SHA-256 of those bytes, lower-case hex) instead of `body`. |
| `ws-open` | A WebSocket upgrade completed: `path`. |
| `ws-refused` | A WebSocket upgrade that failed its handshake: `path`. |
| `ws-frame` | `dir`, and `msg`, the text frame (secrets masked); for a binary frame from a client, `binary` (its length) instead. A frame derived from the journal carries `type`, `sequence`, `length` and `sha256` in place of `msg` (below). |
| `ws-close` | The connection ended: `reason` (`heartbeat missed`, `too far behind` on what it was sent, `key no longer valid` for a session whose API key was revoked or expired, or `closed`: by the client, the network or an error). |

What is recorded on each half:

| | Private half | Public half |
|---|---|---|
| REST request | line, headers (masked), body | line, headers (masked), body |
| REST response | status and body | a refusal or error (4xx, 5xx): status and body; any other: status, `length` and `sha256` of the body, not the body |
| WebSocket | open, close; every frame from the client; every frame to it — replies, errors, quotes and transfers in full, and what the engine did (executions, account updates, payouts, releases) by reference to the journal (below) | open, close; every frame from the client except `heartbeat`; every `error` sent to it |

The private half is recorded in full: what an account asked for and was told is what a dispute turns on, and much of
it (a refused order, a transfer's answer, a quote) is in no journal. The public half serves market data, which is the
same for every viewer, high in volume, and reconstructable from the journal, so it is not stored: book, trade and market
messages on its WebSocket are not recorded, nor the bodies of its successful REST answers (`GET /v4/markets`,
`GET /v4/markets/{id}/book` and the rest of its routes), which clients poll all day. What a client asked for is still
recorded, and the length and hash of its answer show what it was sent, which a client holding the answer can check. A
public answer's hash cannot be recomputed from the journal alone, since the bytes depend on the moment of the answer.
Heartbeats on the public half, which viewers send all day, are not recorded either. A connection
that never completes a request line, a whole request or a WebSocket handshake within the gateway's arrival time has
said nothing to record, and is not recorded. Refusals are always recorded: every 4xx and 5xx response, and every `error` frame,
including flood limits and failed logins.

### Frames derived from the journal

A private session is sent two kinds of frame. Some answer what the client asked — replies, errors, refusals — and
exist nowhere else. The rest are the venue telling an account what the engine did: an execution, an account snapshot,
a payout, a release, a trade, a market's state. Those are a rendering of a sequenced input's results, and the journal
already holds the input, the results and everything they were rendered from.

A frame the gateway did not compose in answer to a request, and which carries a `sequence`, is recorded by reference:

```json
{"ts":"...","n":812,"component":"gateway","instance":"gateway-0","kind":"ws-frame","dir":"out",
 "type":"execution","sequence":"222981","length":412,
 "sha256":"9f2b...","conn":2,"peer":"10.42.0.7:51344","half":"private","account":"1",
 "session":"992440534292070","prev":"..."}
```

- `type` is the frame's `type` member, `sequence` the input it came from, `length` the bytes as sent, and `sha256`
  their SHA-256 in lower-case hex.
- Reference records of one connection are in the order the frames were sent, so several frames of one type at one
  sequence are told apart by their order, which the chain makes tamper-evident.
- Everything else is recorded in full, whatever its volume: every frame from a client, every reply, error and refusal,
  every authentication, and any queued frame that carries no `sequence`. A message type added later is recorded in
  full until it is given a sequence, so the failure is towards keeping too much.

**Following a reference.** The journal holds the inputs; replaying it produces their results, and the two together are
what the feed carried:

```
venue-node replay data/journal.sbe results.log
venue-gateway reconstruct --journal data/journal.sbe --results results.log --catalogue markets.json audit/*.jsonl
```

That renders every referenced frame again and checks it against the record's `length` and `sha256`, reporting any that
differ: either the journal is not the one that gateway was serving, or the renderers no longer render as they did. A
session's frames are matched in the order that session was sent them, so several frames of one type at one sequence
are told apart by their order, and each session of an account is followed through its own copies.

`interfaces/audit/recorded/` holds a session recorded this way — a journal and the audit file written beside it — and
`core/ci.sh` reconstructs it on every build. A change to a renderer fails there, which is the point: it is found while
it can still be versioned, rather than in a dispute years later. When a renderer must change, raise `renderer`, record
a new session beside the old one, and reconstruct each under the rules its file names.

**What the venue owes in return.** A reference is only as good as what it points at:

- **The journal is kept as long as the audit log** — at least five years (blueprint §12), archived with the same
  protection from alteration and loss. It stops being operational data the venue happens to keep.
- **Renderers are versioned.** The `renderer` version in each file's `open` record says which rules produced its
  frames. A change to what a renderer emits raises that version; reconstruction uses the rules of the version the
  file names, so a file written years ago still resolves.
- **A reconstruction is proof, not paperwork.** The hash in the record is what makes it one: a rendering that does not
  hash the same is wrong, and says so.

Public frames are not recorded at all (above), so nothing there is referenced: book data is conflated on a timer, and
what was sent depends on the moment rather than on the journal alone.

### The client behind a proxy

Behind an ingress, `peer` is the ingress. The gateway is given the addresses and networks of the proxies in front of
it (`--trusted-proxies`, `VENUE_TRUSTED_PROXIES`, for example `10.42.0.0/16`; none by default). On a connection whose
peer is one of them, it reads whom the request was forwarded for, from a REST request's headers or a WebSocket's
upgrade request, and records it as `client`, an IP address (or, for an entry that is not one, such as `unknown`, the
entry as sent, at most 64 characters) on every record of that request or session. `peer` stays the proxy.

- `X-Forwarded-For` if the request has one (every line of it, in order), otherwise `Forwarded` (RFC 7239, each
  element's `for=`, an element without one standing for `unknown`). A proxy that writes only `Forwarded` must remove
  any `X-Forwarded-For` a client sent.
- The list is read from the right: each proxy appends the address it was reached from, so entries that are trusted
  proxies are skipped, and the first that is not is the client. Anything left of it may have been written by the
  client and is ignored. If every entry is a trusted proxy, the left-most is recorded.
- From a peer that is not a trusted proxy, or when the list is empty, there is no `client`: forwarding headers are
  never believed from anyone else, and are recorded only among `headers`.

The client address is recorded only. Flood and logon limits are kept per account (REST) and per session and account
(WebSocket), never per address, so nothing a forwarding header says changes what a client is allowed.

### Masking

Secrets are replaced by `***` before a record is written; nothing unmasked ever reaches a file. The gateway queues a
frame as it arrived and masks it as it writes it, because masking reads the whole message and its event loops are not
where that work belongs; an unmasked secret therefore lives in the gateway's memory, as it already does in the socket
it was read from, and nowhere else.

| Where | What |
|---|---|
| FIX | the values of Password (554), NewPassword (925), EncryptedPassword (1402) and EncryptedNewPassword (1404). Username (553) is an identity, not a secret, and is kept. A masked message's BodyLength (9) and CheckSum (10) are left as they were sent, so they no longer match. |
| HTTP headers | `authorization`, `proxy-authorization` and `cookie`: the value becomes its scheme and `***` (`Bearer ***`), or `***` when it has no scheme. |
| JSON, in HTTP bodies and WebSocket frames | the string value of every member named `token`, `secret`, `credential`, `password`, `passphrase`, `apiKey` or `api_key`, whatever its case, wherever it appears, found textually so that a frame that is not valid JSON is masked too. `secret` and `credential` are how a new API key's secret is answered; the rest are names a client may reach for, and a frame the venue refuses for using one is still recorded in full. Frames sent to a client are masked like frames from one. |

## Writing

- **Off the hot path.** The code that serves clients (the gateway's event loops, the adapter's Aeron subscriber) builds
  a record and keeps it until it has served what was ready, then puts that turn's records on a bounded in-memory queue
  together: one order is six records, and queueing each on its own woke the writer six times. One writer thread per
  process takes them off, chains them, and hands everything it took to the kernel with one `write`, then syncs the file
  for all of it. It hands over as soon as the queue is empty, so what a killed process can lose is the records it was
  in the middle of writing.
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
venue-gateway verify-audit FILE...                         # the same checks, where there is a gateway and no Python
```

A file verifies when every line is a JSON object; line 0 is an `open` record naming the file's component and
instance; `n` counts from 0; every `prev` is the hash of the line before it (zeros on line 0); every line has the
same `component` and `instance`; and a `close` record, if there is one, is the last line and counts the lines before
it. The verifier prints the first line that breaks any of these, and whether the file is closed.

The chain shows that a file has not been altered or cut short in the middle, and a `close` record that it was not cut
short at the end. It does not show who wrote it: anyone able to rewrite a whole file can rewrite its chain. That is
what the archive's signed, chained manifests and compliance-mode object lock are for, once a shipper takes closed
files there; none of that is built yet.
