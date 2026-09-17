# FIX 4.4 adapter

Interface 4 for the mini venue: FIX 4.4 sessions for firms that want them (blueprint §13), on
[Artio](https://github.com/artiofix/artio), Real Logic's FIX engine on Aeron. Artio does the session layer (logon,
heartbeats, sequence numbers, resend and gap fill, the message archive); this project is the translation, and nothing
else (blueprint §18). The dialect it speaks is specified in [`interfaces/fix/`](../../interfaces/fix/README.md); its
codecs are generated from the dictionary there at build time.

| Pinned | Version | Where |
|---|---|---|
| JDK | Temurin 21.0.12.1+1 | `bootstrap.sh` (checksum) |
| Gradle | 9.7.1 | `gradle/wrapper/gradle-wrapper.properties` (checksum) |
| Artio | 0.183 (Aeron 1.53.1, Agrona 2.6.0) | `build.gradle` |
| Jackson | 2.20.0 | `build.gradle` |

## Why here, and why Java

`adapters/` sits beside `core/` rather than in it. `core/` is the Rust workspace and its gates (formatting, clippy,
cargo deny, the engine's guardrails); a JVM project has no place in them. The blueprint puts protocol adapters
outside the canonical core and independently deployable (§3), N+1 per protocol on the gateway nodes (§18), and says
the FIX engine is adopted rather than written, in another language if that is where a mature engine is (§13). Artio
is that engine, in Java.

## Build, run, test

Nothing needs to be installed: `bootstrap.sh` fetches the JDK and Gradle into `.toolchain/` (ignored by git), checks
their SHA-256 sums, and writes the Gradle wrapper jar, which is not committed either.

```sh
adapters/fix/bootstrap.sh                   # once: JDK 21 and the Gradle wrapper into adapters/fix/.toolchain
eval "$(adapters/fix/bootstrap.sh --env)"   # JAVA_HOME and GRADLE_USER_HOME for this shell
cd adapters/fix
./gradlew build -x test                     # generate codecs from interfaces/fix/venue-fix44.xml, compile
./gradlew installDist                       # build/install/venue-fix-adapter/bin/venue-fix-adapter
```

Run it against a gateway (the laptop venue's is `127.0.0.1:8081`; `scripts/venue.sh start DIR` starts a private one
on 17410-17412):

```sh
build/install/venue-fix-adapter/bin/venue-fix-adapter \
  --listen 127.0.0.1:17420 --comp-id VENUE --gateway 127.0.0.1:17412 --data build/fix-data
```

| Flag (or `VENUE_FIX_<FLAG>`) | Default | |
|---|---|---|
| `--listen` | `127.0.0.1:17420` | where FIX clients connect |
| `--comp-id` | `VENUE` | the venue's CompID; clients send it as TargetCompID |
| `--gateway` | `127.0.0.1:8081` | the gateway's host and port (`/v4/ws_private`, `GET /v4/markets`) |
| `--data` | `./fix-data` | Artio's session state and message archive, the embedded Aeron directory, and what each SenderCompID's sessions know (`sessions/`) |
| `--cancel-on-disconnect` | `true` | whether sessions' orders are cancelled when they end |
| `--keep-orders-for` | none | SenderCompIDs, comma-separated, whose orders outlive their sessions |
| `--audit-dir` | `<data>/audit` | the audit log of every FIX message (below) |
| `--audit-instance` | `$HOSTNAME`, else `fix-adapter` | this adapter's name in the audit log's file names |
| `--audit-fsync` | `true` | whether the audit log syncs what it writes; `false` only for tests |

A firm then logs on with its SenderCompID, `TargetCompID=VENUE` and an API key: the key ID in `Username (553)` and
the secret in `Password (554)`, or the whole `keyId.secret` in Password. In test environments a dev token in Password
works too. Keys are made on the gateway (`POST /v4/keys`, interfaces/gateway/README.md).

To try it by hand, `scripts/fix-shell.py` is a FIX console that needs only Python 3: it logs on, keeps the session
alive, prints reports as they arrive, and turns `list`, `buy`, `sell`, `replace`, `cancel` and `cancelall` into
messages (`help` lists them). It is a test tool, not a FIX engine: every logon resets sequence numbers.

```sh
python3 scripts/fix-shell.py --host 127.0.0.1 --port 17420 --token dev-retail-customer --sender ME
```

The integration test:

```sh
adapters/fix/test.sh                        # bootstrap, cargo build --release of the venue, gradle test
```

It builds `venue-node`, `venue-gateway`, `venue-sim` and `venue-identity` in release, starts a node seeded with the
mini venue's catalogue, an identity service, a gateway and a second gateway serving the private half beside it
(`scripts/venue.sh`, ports 17430-17434), runs the adapter in the test's JVM on 17440, and
logs on two Artio initiators as a market maker and a retail customer. It checks: a refused token and a missing one;
the security list; place, replace, cancel, a cancel too late and one of an unknown order; a price off the ladder, a
duplicate ClOrdID, a malformed OrdType and a GTD without ExpireTime; a mass cancel by market and of everything; a
fill between the two sessions reported to both, with maker and taker, partial and full fills and AvgPx, and no
report twice; cancel-on-disconnect after Logout; a session whose orders are kept (`--keep-orders-for`) that is sent, when it logs on
again, the fill that happened while it was away; a second session that takes those orders up with an
OrderMassStatusRequest (all, by market, by event, and one with no open orders), cancels one by ClOrdID, replaces one by
OrderID and is then told of its fill; the status of an order placed over REST and of an unknown order; an adapter
restart after which the market maker logs on without resetting sequence numbers, is sent the cancel that ended its
last session, and has its first session's messages resent from the archive; and the audit log:
one closed file per run of the adapter whose chain verifies and which holds no token in clear, with a Logon both ways
(Password masked), an order and the report that answered it, a TestRequest and the Heartbeat Artio answered it with, a
ResendRequest and its gap fill, Logouts, the status requests and their reports, and the refused logons with their Logouts and the gateway's reasons. Last,
API keys: a read-only key logs on with Username and Password and its order is rejected `FORBIDDEN`, a trading key
logs on with the whole credential in Password and trades; both ask for order status and mass status with their keys,
the read-only one seeing the trading key's order on the same account, and the gateway's audit log names the key the
adapter's `GET /v4/account` came with; once revoked, the trading key's session is logged out while it keeps asking for
status (each answered, or refused with 380=6 once the gateway no longer takes the key), the key reads no status over
REST, and it is refused at logon; the logon's audit record names the key and no secret is written in either audit log
or in the sessions' memory. And a venue of several gateways: an order placed over FIX is cancelled at once through the
second gateway, which the FIX session hears; an order placed through the second gateway is answered for by the
adapter's order status, from its own gateway, once that gateway has it; and cancelled over FIX, it leaves the second
gateway's account too.
`test.sh` then checks the same files with `interfaces/tools/audit_verify.py`.

`./check` runs `test.sh` as its last step when the toolchain has been bootstrapped (`bootstrap.sh --check`), and
skips it with a note otherwise, so a machine without Java still passes every other gate.

## How it works

```
firm ── FIX 4.4 / TCP ──► Artio engine (session layer, archive) ──► library, one polling thread
                                                                     │  VenueSession per FIX session
                                                                     ▼
                                              gateway /v4/ws_private (JSON), one WebSocket per FIX session
                                                                     │
                                                                     ▼
                                                            node ingress → engine
```

- **Logon.** `GatewayAuthenticator` answers Artio asynchronously: it opens the gateway WebSocket and sends `login`
  with the credential from Username and Password (`Credentials`), `cancelOnDisconnect` (per the flags above) and `heartbeatSeconds` of three times the
  firm's HeartBtInt. A `welcome` accepts the logon; an `error` rejects it with a Logout saying why. The open WebSocket
  is handed to the session's `VenueSession` when Artio gives the library the session.
- **Heartbeats.** Artio keeps the FIX heartbeat and ends a silent session. The adapter relays the firm's liveness to
  the gateway (a `heartbeat` whenever nothing else has been sent for half an interval), so a hung adapter is a missed
  heartbeat at the gateway, and its orders are cancelled.
- **Session end.** Logout, disconnect or a missed heartbeat ends the FIX session, which closes the WebSocket, which
  the gateway turns into a mass cancel of that session's orders. If the gateway closes the WebSocket, the adapter
  logs the FIX session out.
- **Orders.** D, G, F and q become `order`, `amend`, `cancel` and `cancelAll`, each with a request `id`. Replies and
  the account's `execution` stream become ExecutionReports, OrderCancelRejects and OrderMassCancelReports. The
  gateway reports an execution both in the reply and on the stream, in either order, so `VenueSession` reports each
  once and holds streamed ones back until no earlier reply can still arrive. The mapping is in the dialect document.
- **Order status.** H and AF are answered from `GET /v4/account` (with the session's token), which lists every resting
  order of the account with the client order ID it was placed with, its whole size, what filled and at what YES cost.
  Each order a request answers about is taken up: the session knows it from then on, as if it had placed it. The
  snapshot is taken at a sequence the session's stream may be behind or ahead of, so a taken-up order ignores
  executions at or below that sequence, and executions of unknown orders that arrive while a snapshot is awaited are
  applied on top of it once it has come.
- **Memory across logons.** What a session knows (its orders, the executions it has reported, reports it could not
  deliver) belongs to its SenderCompID, not to one logon (`SessionMemory`). Reports and markers leave in one queue;
  a marker after each streamed execution moves the SenderCompID's resume point once the reports before it have been
  handed to Artio. The next logon sends `login` with `resumeFrom` that point, and the gateway replays the account's
  executions since, which are reported (deduplicated against what was already reported) before anything new. A
  `resync` instead marks the known orders stale, and the next status request takes the snapshot's word for them.
- **Threads.** Artio's framer and the embedded media driver and archive run on their own threads; every piece of
  translation state is touched only by the library's polling thread. The WebSocket client's callbacks only queue.
  Idle loops sleep a millisecond rather than spin: the machine is shared, and the gateway round trip dominates.
- **Gateway JSON.** Read as trees, so fields the gateway adds (fees on executions, treasury fields) pass unnoticed.

## The audit log

Every FIX message of every session, in and out, is written to `--audit-dir` in the format of
[`interfaces/audit/`](../../interfaces/audit/README.md): one file per UTC day per adapter instance, hash-chained lines,
Password (554) masked. That includes what the adapter's own code never sees, because Artio's engine does it: Logon
and its answer, Heartbeat, TestRequest, ResendRequest, SequenceReset, Reject, Logout, replays from the archive, and
the Logon and Logout of a connection whose token was refused.

- **How.** `FixAuditor` runs Artio's `FixMessageLogger`, which subscribes to the Aeron streams between the engine and
  its libraries (inbound, outbound, and the engine's replay stream) and hands every FIX message over in timestamp
  order. Artio's archive holds the same messages, but its archive printer could not be made to read them back, and a
  record of what was said should not depend on a store kept for resends. `GatewayAuthenticator` adds a
  `gateway-login` record for each Logon (the account and the gateway session it opened, or why it was refused, and
  the firm's address) and a `disconnect` record for each connection.
- **Tying FIX to the gateway.** The adapter does not log its JSON. The gateway's own audit log holds every frame of
  the WebSocket behind each FIX session, and `gatewaySession` in the `gateway-login` record is the `session` the
  gateway's records carry.
- **Off the polling thread.** The subscriber has its own thread and the writer another; the polling thread never waits
  on either. A disk slow enough to fill the writer's queue (65 536 records) makes the subscriber wait, which Aeron
  turns into back-pressure on Artio's engine: sessions slow down rather than a message going unrecorded. A queue full
  for 10 seconds, or any failure to write, stops the adapter with status 74; so does an audit directory it cannot
  write at start.
- **Where.** `<data>/audit` unless `--audit-dir` says otherwise; in the chart, a volume of its own.

## Across adapter restarts

- **Kept**: the audit log. A restart closes the file it was writing and opens the next segment of the day; after a
  crash, the next start seals the file left open (interfaces/audit/README.md, files).
- **Kept** in `--data`: each session's sequence numbers and every message sent and received (Artio's archive). A firm
  that logs on again without ResetSeqNumFlag carries on where it was and can ask for any of it to be resent, as the
  test does. Artio needs inbound messages archived to keep sequence numbers (`logInboundMessages(false)` refuses
  persistent sessions), so the archive holds each Logon as received, **Password (554) in clear**: a dev token, or an
  API key's secret or whole credential. `--data` must be protected like the identity service's key store, and
  revoking a key is the remedy if it leaks. Nothing else the adapter writes holds a secret: the audit log masks
  Password, `sessions/` holds no credential, and the gateway credential kept in memory for status requests
  (`GatewayLink.token`) is sent only in the `Authorization` header of `GET /v4/account` and never printed.
- **Kept** in `--data/sessions`: each SenderCompID's memory (the live orders its sessions know, their ClOrdIDs and
  totals, the gateway sequence it has been reported up to, the reports it could not deliver), written when a session
  ends and when the adapter stops. On the next logon the gateway replays what happened since, so orders kept past a
  disconnect (`--keep-orders-for`) are reported on, cancelled and replaced as before, and the fills and cancels in
  between are reported.
- **Lost**: the gateway sessions. When the adapter stops, their WebSockets close and the gateway cancels the orders
  of every session that logged in with cancel-on-disconnect; the ExecutionReports for those cancels are sent when each
  SenderCompID logs on again.
- **Lost after a crash**: what changed in a memory since it was last written. The next logon resumes from the older
  sequence and reports those executions again, under the same ExecIDs.
- **Known gaps.** The gateway holds a window of each account's messages (`HISTORY` in `core/gateway/src/hub.rs`); a
  SenderCompID away for longer, or one whose memory is lost, gets a `resync` and nothing replayed, and restores its
  state with OrderMassStatusRequest (AF). A new order whose answer never reached the session before it ended is not
  known on the next logon (the venue's replay carries its acceptance, but not the request that would tie it to the
  session); a status request by its ClOrdID takes it up. ClOrdIDs of replaces are known only where they were sent: the
  venue keeps the client order ID an order was placed with, so another session or a lost memory finds the order by
  that or by OrderID. Status is only of resting orders or ones the session knows: the venue's account view has no
  order history. TimeInForce and ExecInst of a taken-up order are not reported, for the same reason.

## The end state, and what would change

This adapter is a client of the gateway's private WebSocket, so that every check and conversion (price ladders,
lot steps, clientOrderId rules, flood limits) stays in one place, the Rust gateway, while the mini venue has one
ingress path. It is not the blueprint's end state. There, every protocol adapter decodes to the **canonical order
message** at the edge and writes it to the sequencer group itself (blueprint §3 and §13, interface 7): FIX does not pass
through JSON and a second process on the way.

To get there:

1. **Canonical messages, not JSON.** `VenueSession` would build the SBE `NewOrder`, `AmendOrder`, `CancelOrder` and
   `MassCancel` of `interfaces/sbe/venue-journal.xml` (generated Java codecs, `interfaces/tools/sbe.sh`) and send them
   to the cluster ingress, and decode `OrderAccepted`, `Fill`, `OrderCancelled` and the rest from egress directly
   into ExecutionReports.
2. **Edge validation moves or is shared.** Price to tick (the ladder), stake to contracts, ClOrdID shape, flood limits
   and session auth are the gateway's today. The adapter would need the same ladders and lot steps from the
   reference data (interface 6's cache), and the same flood limiter, or a shared normaliser library both use.
3. **Sessions are the adapter's.** It would allocate engine session IDs (unique across restarts, spec E-12.6) and
   send the `MassCancel` with cause `SESSION_DISCONNECT` or `HEARTBEAT_MISSED` itself.
4. **Executions by order ID from egress** replace the reply-and-stream deduplication, and a restart recovers its
   state by reading the result log from a sequence (interface 12), which also closes the restart gaps above.
5. **Deployment.** One adapter per gateway node, N+1, sticky per SenderCompID, with the archive on local disk.

## Files

| Path | What it is |
|---|---|
| `src/main/java/venue/fix/FixAdapter.java` | Startup: media driver and archive, Artio engine, library, polling thread |
| `src/main/java/venue/fix/GatewayAuthenticator.java` | Logon: the credential checked by logging in to the gateway |
| `src/main/java/venue/fix/Credentials.java` | Which credential a Logon's Username and Password make |
| `src/main/java/venue/fix/GatewayLink.java` | One gateway WebSocket session |
| `src/main/java/venue/fix/VenueSession.java` | The translation, both ways, for one FIX session |
| `src/main/java/venue/fix/FixAuditor.java` | Every FIX message, both ways, off Artio's internal streams into the audit log |
| `src/main/java/venue/fix/AuditLog.java` | The audit log's writer: queue, chain, rotation, sealing, masking, verification |
| `src/main/java/venue/fix/OrderState.java` | What a session knows about one of its orders |
| `src/main/java/venue/fix/SessionMemory.java` | What a SenderCompID's sessions know beyond one logon, and where it is kept |
| `src/main/java/venue/fix/Report.java` | One execution report as data, so an undelivered one can be kept |
| `src/main/java/venue/fix/Translate.java` | Fixed mappings: prices, quantities, reject reasons |
| `src/test/java/venue/fix/` | The integration test and its Artio initiator |
| `scripts/venue.sh` | A private node, identity service and gateway for tests and manual runs |
| `bootstrap.sh`, `test.sh` | Toolchain, and the whole test from nothing |
