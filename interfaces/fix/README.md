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
| Credentials | The account's token in **Password (554)** on Logon. Test environments use the dev tokens of `deploy/charts/venue-mini/files/markets.json`, such as `dev-market-maker`. Username (553) is optional and ignored. |
| Logon refused | A missing or refused token is answered with a Logout (5) whose Text (58) says why (`token refused`), and the connection is closed. |
| Account | The token decides the account. Account (1) is optional; on a NewOrderSingle it must equal the logged-on account ID when given. |
| EncryptMethod | 0 only. |
| HeartBtInt | Chosen by the firm on Logon, and honoured by the venue both ways (TestRequest, then Logout on silence). |
| Sequence numbers | Persistent: they carry on across logons and across restarts of the adapter, until the firm logs on with ResetSeqNumFlag (141) = Y. |
| Resend and gap fill | A ResendRequest (2) is answered from the adapter's message archive: application messages again with PossDupFlag (43) = Y and OrigSendingTime (122), session messages replaced by SequenceReset-GapFill (4, 123=Y). This includes messages sent before an adapter restart. |
| Malformed messages | A required tag missing, a tag or value the dictionary does not have, a tag out of place: session-level Reject (3) with RefTagID (371) and SessionRejectReason (373). |
| Unsupported message types | BusinessMessageReject (j) with BusinessRejectReason (380) = 3. |

### Cancel-on-disconnect

When a session ends, however it ends (Logout, a dropped connection, a missed heartbeat), every order still resting
from that session is cancelled (venue reason `session_disconnect`). The session that owned the orders has gone, so no
ExecutionReport is sent for these cancels; after logging on again a firm finds its open orders over REST
(`GET /v4/account`, interface 1). This is on by default; the venue can switch it off per SenderCompID (the adapter's
`--keep-orders-for`). Only the session's own orders are cancelled, never the account's orders from other sessions.

The venue also watches the adapter: if the adapter hangs, the gateway cancels the session's orders after three
heartbeat intervals of silence (`heartbeat_missed`).

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
which may be the ClOrdID of the order or of any replace of it that was accepted. Symbol and Side must be the order's;
OrdType is 2. **OrderQty is the order's new total size, filled part included**, and must exceed CumQty; the venue
rests OrderQty − CumQty. A reduction at the same price keeps the order's priority; anything else loses it and may
trade at once (spec E-12.4, E-12.5). The new ClOrdID (11) answers for the order from then on.

### OrderCancelRequest (F)

Found as for G. The answer is an ExecutionReport (150=4) under the request's ClOrdID, with OrigClOrdID.

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

Every report carries OrderID (37), ClOrdID (11), ExecID (17), Symbol, Side, OrderQty, Price, OrdType, LeavesQty (151),
CumQty (14) and AvgPx (6). OrderID is the venue's order ID, a decimal string; a rejected order has `NONE`.
ExecID is made from the venue's sequence number, order ID and trade ID, so it is unique and says which venue event
the report is of.

Executions of orders a session did not place (the same account's other sessions, the web page) are not reported on
it. A drop copy (interface 5) is where every execution of an account is found.

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
- 380=4, with BusinessRejectRefID = the ClOrdID: **the venue did not answer in time and the outcome is unknown**. The
  order may or may not exist. If it was taken, its ExecutionReport (150=0) still follows when the venue reports it.
  Resend it with the same ClOrdID: if the original was taken, the resend is rejected with 103=6; if not, the resend
  is the order.

## Not in this draft

OrderStatusRequest (H), MarketDataRequest (V) and market data over FIX, quotes and RFQ, stake-based orders, NO-terms
entry, order groups (`clientOrderGroup`), `protectionTicks` and `spendCapCents`, drop copy over FIX (interface 5), and
positions or balances. After a disconnect a firm reconciles open orders over REST (`GET /v4/account`, interface 1).
