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
| `--data` | `./fix-data` | Artio's session state and message archive, and the embedded Aeron directory |
| `--cancel-on-disconnect` | `true` | whether sessions' orders are cancelled when they end |
| `--keep-orders-for` | none | SenderCompIDs, comma-separated, whose orders outlive their sessions |

A firm then logs on with its SenderCompID, `TargetCompID=VENUE` and its dev token in `Password (554)`.

The integration test:

```sh
adapters/fix/test.sh                        # bootstrap, cargo build --release of the venue, gradle test
```

It builds `venue-node`, `venue-gateway` and `venue-sim` in release, starts a node seeded with the mini venue's
catalogue and a gateway (`scripts/venue.sh`, ports 17430-17432), runs the adapter in the test's JVM on 17440, and
logs on two Artio initiators as a market maker and a retail customer. It checks: a refused token and a missing one;
the security list; place, replace, cancel, a cancel too late and one of an unknown order; a price off the ladder, a
duplicate ClOrdID, a malformed OrdType and a GTD without ExpireTime; a mass cancel by market and of everything; a
fill between the two sessions reported to both, with maker and taker, partial and full fills and AvgPx, and no
report twice; cancel-on-disconnect after Logout; and an adapter restart after which the market maker logs on
without resetting sequence numbers and has its first session's messages resent from the archive.

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
  with the token from Password, `cancelOnDisconnect` (per the flags above) and `heartbeatSeconds` of three times the
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
- **Threads.** Artio's framer and the embedded media driver and archive run on their own threads; every piece of
  translation state is touched only by the library's polling thread. The WebSocket client's callbacks only queue.
  Idle loops sleep a millisecond rather than spin: the machine is shared, and the gateway round trip dominates.
- **Gateway JSON.** Read as trees, so fields the gateway adds (fees on executions, treasury fields) pass unnoticed.

## Across adapter restarts

- **Kept** in `--data`: each session's sequence numbers and every message sent and received (Artio's archive). A firm
  that logs on again without ResetSeqNumFlag carries on where it was and can ask for any of it to be resent, as the
  test does.
- **Lost**: the gateway sessions. When the adapter stops, their WebSockets close and the gateway cancels the orders
  of every session that logged in with cancel-on-disconnect; no ExecutionReport is sent for those cancels.
- **Lost**: the translation state (which OrderIDs a session placed, their ClOrdIDs, CumQty), which lives only as long
  as one logon. Orders that outlive their session (sessions in `--keep-orders-for`) are therefore not reported on,
  cancelled or replaced over FIX after a reconnect or a restart; a firm manages them over REST or mass cancels them. Executions that happened while no WebSocket was open are not replayed as FIX either.
  The gateway's `resumeFrom` and persisting the order map would close both gaps; neither is done.

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
| `src/main/java/venue/fix/GatewayAuthenticator.java` | Logon: the token checked by logging in to the gateway |
| `src/main/java/venue/fix/GatewayLink.java` | One gateway WebSocket session |
| `src/main/java/venue/fix/VenueSession.java` | The translation, both ways, for one FIX session |
| `src/main/java/venue/fix/OrderState.java` | What a session knows about one of its orders |
| `src/main/java/venue/fix/Translate.java` | Fixed mappings: prices, quantities, reject reasons |
| `src/test/java/venue/fix/` | The integration test and its Artio initiator |
| `scripts/venue.sh` | A private node and gateway for tests and manual runs |
| `bootstrap.sh`, `test.sh` | Toolchain, and the whole test from nothing |
