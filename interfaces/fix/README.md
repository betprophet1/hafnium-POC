# Interface 4: the FIX 4.4 dialect

Draft v0, for review by market makers before it is frozen (catalogue status: drafted). `venue-fix44.xml` beside this
file is the dictionary, in the QuickFIX XML format: every message, field and enumerated value the venue accepts or
sends. This file holds the rules of engagement the dictionary cannot express. The adapter in `adapters/fix` generates
its codecs from the dictionary, so the two cannot drift apart.

FIX is an on-ramp for firms that already speak it (blueprint §13). It is not the fast lane: that is the native binary
gateway (interface 2). Every FIX message is translated to the same canonical order message the other protocols reach,
and nothing past the adapter knows it came from FIX.

## Sessions

| | |
|---|---|
| Version | FIX 4.4 (`8=FIX.4.4`), over TCP. TLS is terminated in front of the adapter where it is needed. |
| CompIDs | The firm chooses its SenderCompID (49). TargetCompID (56) is the venue's CompID, `VENUE` in test environments. A session is its pair of CompIDs. |
| Credentials | An API key on Logon (interface 1 README, API keys and permissions): the key ID in **Username (553)** and its secret in **Password (554)**, or the whole credential, `<keyId>.<secret>`, in Password with Username free. Test environments also take an account's dev token in Password, such as `dev-market-maker` from `deploy/charts/venue-mini/files/markets.json`. Username is read as a key ID only when it is one (`ak_` and 16 hex digits); otherwise it is the firm's own and ignored. |
| Logon refused | A missing or refused credential is answered with a Logout (5) whose Text (58) says why (`token refused`, also for a revoked or expired key), and the connection is closed. |
| Account | The credential decides the account. Account (1) is optional; on a NewOrderSingle it must equal the logged-on account ID when given. |
| Permissions | Logging on needs `read`, which every key holds, so a key without `trade` can log on and list securities. Its NewOrderSingle is rejected (ExecutionReport 150=8, OrdRejReason 103 = 99, Text `FORBIDDEN: ...` naming the permission), its OrderCancelReplaceRequest and OrderCancelRequest are refused with OrderCancelReject (102 = 99), and its OrderMassCancelRequest with 531=0 and 532=99. OrderStatusRequest and OrderMassStatusRequest need only `read`, so every key can ask for status; the adapter asks the gateway with the session's own credential and never shows or writes its secret. A session whose key is revoked or expires is logged out within 10 seconds, which cancels its orders as any disconnect does; a status request that meets the revoked key first is refused with BusinessMessageReject 380=6 and the session is logged out at once. |
| EncryptMethod | 0 only. |
| HeartBtInt | Chosen by the firm on Logon, and honoured by the venue both ways (TestRequest, then Logout on silence). |
| Sequence numbers | Persistent: they carry on across logons and across restarts of the adapter, until the firm logs on with ResetSeqNumFlag (141) = Y. |
| Resend and gap fill | A ResendRequest (2) is answered from the adapter's message archive: application messages again with PossDupFlag (43) = Y and OrigSendingTime (122), session messages replaced by SequenceReset-GapFill (4, 123=Y). This includes messages sent before an adapter restart. |
| Malformed messages | A required tag missing, a tag or value the dictionary does not have, a tag out of place: session-level Reject (3) with RefTagID (371) and SessionRejectReason (373). |
| Unsupported message types | BusinessMessageReject (j) with BusinessRejectReason (380) = 3. |

### Cancel-on-disconnect

When a session ends, however it ends (Logout, a dropped connection, a missed heartbeat), every order still resting
from that session is cancelled (venue reason `session_disconnect`). The session that owned the orders has gone; their
ExecutionReports (150=4, Text `session_disconnect`) are sent when the same SenderCompID next logs on, as any other
execution missed while away is (below, reconnecting). This is on by default; the venue can switch it off per
SenderCompID (the adapter's `--keep-orders-for`). Only the session's own orders are cancelled, never the account's
orders from other sessions.

The venue also watches the adapter: if the adapter hangs, the gateway cancels the session's orders after three
heartbeat intervals of silence (`heartbeat_missed`).

### Reconnecting: what happened while a session was away

The venue remembers, per SenderCompID, the orders a session knows and how far through the account's executions it
has been reported. When the SenderCompID logs on again (after a Logout, a dropped connection or a restart of the
adapter, with or without ResetSeqNumFlag), every execution of those orders since then is sent first, in venue order:
fills of orders kept past the disconnect, the cancels that ended the last session, expiries. ExecIDs are the ones the
reports would have had, so a report received twice (after a crash of the adapter, for example) can be recognised.
Reports that were made but could not be handed to the session before it ended are sent too.

This works while the venue still holds those executions (the gateway keeps a window per account). When it no longer
does, or the SenderCompID logs on to a different account, nothing is replayed: **the firm restores its state with an
OrderMassStatusRequest (AF)**, whose reports are the venue's current state of every open order, and which also brings
the orders the venue now holds for it back into step. A firm that wants certainty after any reconnect sends AF anyway;
it costs one report per open order.

## Instruments

A market is named by its **instrument ID**, a decimal string, in **Symbol (55)**. SecurityID (48) with
SecurityIDSource (22) = 8 may repeat it; if given it must equal Symbol. An **event** (the question a group of markets
answers: a match, an election) is the underlying of its markets, named by event ID in UnderlyingSymbol (311).

SecurityListRequest (x) with SecurityListRequestType (559) = 4 lists every market not yet settled; = 0 lists the one
named in Symbol, if it is not settled. A venue can have more markets than one message holds, so the answer may come in
several SecurityList (y) messages under one SecurityResponseID (322): each carries TotNoRelatedSym (393), the whole
count, and LastFragment (893) = Y marks the last. Each carries, per market in NoRelatedSym (146):

| Tag | Content |
|---|---|
| 55, 48, 22 | instrument ID, instrument ID, 8 |
| 107 SecurityDesc | the event's title and the market's label, `Will it rain in London tomorrow?: Rain` |
| 311 UnderlyingSymbol | the event ID |
| 965 SecurityStatus | 1 when the market is open, 2 otherwise |
| 561 RoundLot | the lot step: order quantities must be multiples of it |

SecurityRequestResult (560) is 0, 2 when nothing matched, or 5 when the venue could not be asked.

## Sides, outcomes and prices

**The dialect is in YES terms throughout**, like the venue's books and executions (interface 1 README, prices).

- **Side (54)**: `1` buys YES contracts, `2` sells YES contracts. There is no separate NO order: buying NO at 47¢ is
  selling YES at 53¢, and resting orders, fills and books show it that way. A position is signed YES contracts.
- **Price (44)**: the YES price in **cents per $1 contract**, a decimal: `53`, `0.5`. It must equal a row of the
  market's price ladder exactly; nothing is rounded. A price between rows is rejected (`PRICE_NOT_ON_LADDER`).
- **OrderQty (38)**: whole contracts, a multiple of RoundLot. A stake in money is not expressible in FIX.
- Prices out (Price, LastPx, AvgPx) are in the same cents. AvgPx is rounded to four decimal places.

## Orders in

### NewOrderSingle (D)

| Tag | Rule |
|---|---|
| 11 ClOrdID | 1 to 32 printable ASCII characters, unique per account for two hours across every market and every session, including orders the venue rejected (spec E-9.2). |
| 55 Symbol | The instrument ID. |
| 54 Side | 1 or 2, as above. |
| 38 OrderQty | Contracts. |
| 40 OrdType | 2 (limit) only. |
| 44 Price | Cents, required. |
| 59 TimeInForce | 1 GTC (default), 3 IOC, 4 FOK, 6 GTD. |
| 126 ExpireTime | Required with 59=6, and only then. |
| 18 ExecInst | Space-separated: `6` post only (Participant Don't Initiate); `E` reduce only (Do Not Increase). |
| 60 TransactTime | Required by FIX; not used. |

### OrderCancelReplaceRequest (G)

Changes a resting order's price or size. The order is found by OrderID (37) when given, else by OrigClOrdID (41),
which may be the ClOrdID of the order or of any replace of it that was accepted. The order must be one the session
knows: one it placed, or one an order status request answered about (below), whichever session, protocol or earlier
logon placed it; any other is rejected as unknown (102=1). Symbol and Side must be the order's;
OrdType is 2. **OrderQty is the order's new total size, filled part included**, and must exceed CumQty; the venue
rests OrderQty − CumQty. A reduction at the same price keeps the order's priority; anything else loses it and may
trade at once (spec E-12.4, E-12.5). The new ClOrdID (11) answers for the order from then on.

### OrderCancelRequest (F)

Found as for G. The answer is an ExecutionReport (150=4) under the request's ClOrdID, with OrigClOrdID.

### OrderStatusRequest (H)

| Tag | Rule |
|---|---|
| 790 OrdStatusReqID | Optional; echoed on the report. |
| 37 OrderID | The venue's order ID. When given, the order is found by it. |
| 11 ClOrdID | Required by FIX. Without OrderID, the order is found by it: a ClOrdID the session knows the order by (a replace's included), or else the client order ID the order was placed with, over any protocol. |
| 55 Symbol, 54 Side | Required by FIX; not used to find the order. |
| 1 Account | Optional; if given, the logged-on account. |

Any resting order of the account can be asked about: placed by this session, an earlier logon, another FIX session,
REST or the web page. The answer is one ExecutionReport with **ExecType (150) = I**, OrdStatus (39), OrderQty (38, the
whole size, filled part included), Price, CumQty (14), LeavesQty (151), AvgPx (6), ClOrdID and OrdStatusReqID (790).

An order the venue does not hold as resting and the session does not know (filled or cancelled before the session
knew it, rejected, never placed) is answered with 150=I, **OrdStatus 8 and OrdRejReason (103) 5 (unknown order)**,
OrderID as asked or `NONE`. An order the session knows that has finished is answered with its final state (39=2 or
4, LeavesQty 0).

### OrderMassStatusRequest (AF)

| MassStatusReqType (585) | Reports |
|---|---|
| 7 | every open order of the account |
| 1 | the account's open orders in the market named by Symbol (55) |
| 8 | the account's open orders in the event named by UnderlyingSymbol (311) |

MassStatusReqID (584) is required; Side (54) narrows to one side. One ExecutionReport (150=I) per open order, in
OrderID order, each with MassStatusReqID (584) and TotNumReports (911), and LastRptRequested (912) = Y on the last.
When no order matches, the answer is a single report with **TotNumReports 0**, 912=Y, OrderID `NONE`, 39=8 and Text
`no open orders`; its Symbol is the one asked about or `NONE`, and its Side the one asked about or 1. Symbol missing
for type 1, or UnderlyingSymbol for type 8, is a BusinessMessageReject with 380=5.

### Orders a session takes up

Every order a status request (H or AF) answers about becomes **known to the session**, as if it had placed it: it can
be cancelled (F) and replaced (G) by OrderID or by ClOrdID, it is included when the session comes back after a
disconnect, and every later execution of it is reported on the session (150=F, 4, 5, D) under the ClOrdID it answers
to. Nothing is taken up unasked: a session is sent executions only of orders it placed or asked about. The same order
may be known to several sessions, and each is then told of its executions. Cancel-on-disconnect still cancels only the
orders a session placed.

A status request the venue cannot answer in time (its account view is unavailable) is answered with
BusinessMessageReject 380=4, BusinessRejectRefID the OrdStatusReqID (or ClOrdID) or MassStatusReqID; send it again.

### OrderMassCancelRequest (q)

| MassCancelRequestType (530) | Cancels |
|---|---|
| 7 | every resting order of the account |
| 1 | the account's orders in the market named by Symbol (55) |
| 2 | the account's orders in the event named by UnderlyingSymbol (311) |

Answered by OrderMassCancelReport (r): MassCancelResponse (531) repeats the type, TotalAffectedOrders (533) counts
the orders cancelled; or 531=0 with MassCancelRejectReason (532) when the request was refused. Each cancelled order of
this session then has its own ExecutionReport (150=4). A mass cancel reaches the account's orders from every session.

## Reports out

### ExecutionReport (8)

| ExecType (150) | When | OrdStatus (39) |
|---|---|---|
| 0 New | the order is accepted | 0 |
| F Trade | a fill; LastQty (32), LastPx (31), LastLiquidityInd (851: 1 maker, 2 taker) | 1 or 2 |
| 5 Replaced | a replace is accepted; ClOrdID is the replace's, OrigClOrdID the one before | 0 or 1 |
| 4 Canceled | a cancel request, a mass cancel, IOC remainder, expiry, event start; Text (58) gives the venue's reason | 4 |
| D Restated | the venue reduced the order (ExecRestatementReason 378 = 99), for example a reduce-only order as the position shrinks | 0 or 1 |
| 8 Rejected | the order never rested; OrdRejReason (103) and Text (58) | 8 |
| I Order Status | the answer to an OrderStatusRequest (H) or OrderMassStatusRequest (AF); not an execution | 0, 1, 2, 4, or 8 for an unknown order |

Every report carries OrderID (37), ClOrdID (11), ExecID (17), Symbol, Side, OrderQty, Price, OrdType, LeavesQty (151),
CumQty (14) and AvgPx (6). TimeInForce (59) and ExecInst (18) are sent for orders the session placed; for an order
taken up by a status request the venue's account view does not have them, and they are left out. OrderID is the
venue's order ID, a decimal string; a rejected order has `NONE`.
ExecID is made from the venue's sequence number, order ID and trade ID, so it is unique and says which venue event
the report is of. A status report (150=I) is of no venue event: its ExecID starts `I-`.

Reports carry no fee: there is no Commission (12) or CommType (13) on a fill. Each fill does charge one, at the
account's maker or taker rate (interface 1 README, fees), and interface 1 reports it as `feeCents` on the fill's
execution: in the answer to the order and on the account's private WebSocket session.

Executions of orders a session neither placed nor asked the status of (the same account's other sessions, the web
page) are not reported on it. A drop copy (interface 5) is where every execution of an account is found.

Rejections and their OrdRejReason:

| 103 | Venue reasons (in Text) |
|---|---|
| 1 Unknown symbol | `unknown_instrument`, no such market |
| 2 Exchange closed | `market_not_open`, `market_closed`, `market_resolved`, `venue_mode`, `closed_only_mode`, `event_started` |
| 3 Order exceeds limit | `insufficient_sleeve`, `position_limit`, `rule_limit`, `spend_cap_*`, `order_capacity`, `reduce_only_no_position` |
| 6 Duplicate order | `duplicate_client_order_id` |
| 13 Incorrect quantity | `quantity_not_on_lot_step` |
| 99 Other | everything else, including `PRICE_NOT_ON_LADDER`, `post_only_would_take`, `fok_not_fillable`, and orders FIX can express but the venue cannot (a market order is malformed instead: 40=1 is not in the dictionary) |

### OrderCancelReject (9)

For a G or F that was not carried out. CxlRejResponseTo (434) is 1 for F, 2 for G. CxlRejReason (102): 0 too late
(the order is filled or cancelled), 1 unknown order, 3 a request for the order is already pending, 99 other with Text.
OrdStatus is the order's status, or 8 when there is no such order.

### BusinessMessageReject (j)

- 380=3: a message type the dialect does not have.
- 380=4, with BusinessRejectRefID = the OrdStatusReqID, ClOrdID or MassStatusReqID: a status request could not be
  answered because the venue's account view did not answer; send it again.
- 380=5: an OrderMassStatusRequest without the Symbol or UnderlyingSymbol its type needs.
- 380=6, Text `UNAUTHORIZED: ...`, with BusinessRejectRefID = the OrdStatusReqID, ClOrdID or MassStatusReqID: a status
  request whose session's API key has been revoked or has expired; the session is logged out.
- 380=0: a status request whose Account (1) is not the logged-on account.
- 380=4, with BusinessRejectRefID = the ClOrdID: **the venue did not answer in time and the outcome is unknown**. The
  order may or may not exist. If it was taken, its ExecutionReport (150=0) still follows when the venue reports it.
  Resend it with the same ClOrdID: if the original was taken, the resend is rejected with 103=6; if not, the resend
  is the order.

## Not in this draft

MarketDataRequest (V) and market data over FIX, quotes and RFQ, stake-based orders, NO-terms entry, order groups
(`clientOrderGroup`), `protectionTicks` and `spendCapCents`, drop copy over FIX (interface 5), positions or balances,
and the status of orders that are no longer resting and that the session never knew (the venue's account view holds
resting orders only).
