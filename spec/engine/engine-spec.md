# Engine specification — draft v0

This document is normative. It states what one sequencer group's engine does with each journal input:
the state it holds, the checks it makes and in which order, the state it changes, and every result it
emits, in order. An implementation that follows it produces the same results, byte for byte, as every
other implementation that follows it — including the reference model in `reference/`, which exists to
make this document executable.

- **Encoding** is fixed by `interfaces/sbe/venue-journal.xml`. Message, field and enum names below are the
  schema's.
- **Rules** are numbered `E-<section>.<n>` so tests, code comments and review notes can cite them.
- **Choices** that were made to complete the specification, rather than taken from the blueprint, are listed
  with their reasons in `decisions.md`, and each is marked here as *(D-n)*.
- **"Must"** marks a requirement; anything not required is not permitted either. When this document and the
  reference model disagree, one of them has a bug; fix the one that is wrong and add a scenario.

## Contents

1. Scope
2. Arithmetic and units
3. State
4. Processing an input
5. Results and their order
6. Reference data commands
7. Service commands
8. Time
9. New orders
10. Matching and clearing
11. Holdings locks and reservations
12. Cancels, amendments, mass cancels and batches
13. Protections: self-match, auto-pull and order groups
14. Set burns
15. Resolution and settlement
16. Market state, event phases and venue modes
17. Sequencer obligations
18. Fail-stop
19. Not in v0
20. Canonical snapshots

---

## 1. Scope

**E-1.1** The engine is one deterministic state machine per sequencer group. Its only inputs are journal
inputs in sequence order; its only outputs are results. It reads no clock, file, network, configuration or
randomness of its own (blueprint §4).

**E-1.2** Everything a group holds is covered here: tick tables, events, instruments, books, orders,
accounts' sleeves and positions, protections, limits and duplicate detection. Cross-group work — sleeve top-ups,
parlay leg tracking, event migration scheduling — belongs to services and reaches the engine only as commands.

---

## 2. Arithmetic and units

**E-2.1** All arithmetic is on integers. Money is cents (`int64`); quantities are whole contracts; prices
are tick ordinals. No step rounds except where a rule names the rounding.

**E-2.2** Any arithmetic overflow is a fail-stop (§18), never a wrap.

**E-2.3** A tick table maps each ordinal `t` to an exact probability `P(t) = numerator / denominator`,
strictly between 0 and 1, strictly increasing in `t`.

**E-2.4** The YES cost of `q` contracts at tick `t` is `yesCost(t, q) = 100 · q · numerator(t) / denominator(t)`
cents. The NO cost is `noCost(t, q) = 100 · q − yesCost(t, q)`. Listing validation (E-6.3) guarantees
both are integers for every `q` that is a multiple of the instrument's lot step.

**E-2.5** Sides are canonical: `BUY` buys YES and `SELL` sells YES. A position is one signed integer per
account per instrument: positive is YES held, negative is NO held (blueprint §7).

**E-2.6** An order's **unit cost** is what one lot costs the account on its own side at tick `t`:
`yesCost(t, ·)` for `BUY` and `noCost(t, ·)` for `SELL`.

**E-2.7** An order is a **purchase in entered terms** when it is `BUY` entered as `YES` or `SELL` entered
as `NO`. Its **gross spend** at tick `t` for `q` contracts is its unit cost for `q` at `t`.

**E-2.8** Order IDs and trade IDs are `(sequencerGroup << 48) + n`, where `n` counts from 1 separately for
orders and for trades within the group *(D-13)*.

---

## 3. State

**E-3.1** A group holds exactly the following. Initial values are in brackets.

| State | Contents |
|---|---|
| Clock | `now` [0], `lastSequence` [0], `protocolVersion` [1] |
| Venue | `venueMode` [`NORMAL`], `activeOrderLimit` [0 = none], `clientOrderIdWindowSeconds` [7 200] *(D-12)* |
| Listing | `settledRetentionSeconds` [604 800] *(D-29)*; `maxEvents`, `maxInstruments`, `maxPriceLevels` [all 0 = none] *(D-30)* |
| Tick tables | by `tickTableId`: ordered rows of `numerator`, `denominator`, `complement` |
| Events | by `eventId`: `stateCount`, `scheduledStart`, `started`, `suspended`, `ended` [all false], `resolved` [false], `resolvedAt` [none], `collateralCents` [0] |
| Instruments | by `instrumentId`: `eventId`, `instrumentClass`, `tickTableId`, `lotStep`, `allocationPolicy`, `stateMask`, `positionLimit`, `legs`, `marketState` [`LISTED`] |
| Books | per instrument, per side, per tick: a FIFO of order IDs |
| Orders | by `orderId`: `accountId`, `sessionId`, `clientOrderId`, `instrumentId`, `side`, `enteredContract`, `tick` (effective limit), `remaining`, `timeInForce`, `expiresAt`, `atEventStart`, `postOnly`, `reduceOnly`, `spendCapRemaining` (or none), `clientOrderGroup` (or none), `lock`, `reserved`, `acceptedAt` (input sequence) |
| Accounts | by `accountId`: `balance` [0], `reserved` [0] (always the sum of its orders' `reserved`); per instrument `position` [0], `yesLocked` [0], `noLocked` [0]; per event `eventCash` [0], the net cash the event has paid the account, fees included *(D-1, D-28)*; `closedOnly` [false], `selfMatchPrevention` [`CANCEL_INCOMING`] *(D-3)* |
| Fees | the default schedule: `makerRateMicros` [0], `takerRateMicros` [0] *(D-2)*; overrides by `overrideId`: `priority`, `accountId`, `instrumentId`, `eventId`, `makerRateMicros`, `takerRateMicros`, `expiresAt` (or none) *(D-27)* |
| Rule limits | by `accountId` (0 is the default for accounts without their own): the four fields of `SetRuleLimits` [all 0 = none]; per account, per second of sequencer time, counts of new orders and of fills |
| Maker protection | by `(accountId, eventId)` with `eventId` 0 as the account's default: threshold and window; per `(accountId, eventId)` a counter and window start |
| Order groups | by `(accountId, clientOrderGroup)`: threshold, window, counter, window start, `triggered` |
| Duplicates | by `(accountId, clientOrderId)`: expiry, and the order ID it created or none |
| Commands | by `commandId`: expiry, seven days after the command was processed *(D-25)* |
| Releases | the resolved events not yet released, in the order they were resolved (E-8.4) |
| Counters | next order number and next trade number [1] |

**E-3.2** An account exists from its first mention; until then every field has its initial value. Account 0
is the exchange's own account, the settlement reserve and the fee account *(D-21)*, and may not submit orders.

**E-3.3** **Available cash** is `balance − reserved`. Two things make it negative: a void (E-15.5), by taking
back cash an account has already received *(D-1)*, and a fee, which is charged at fill time without being
reserved (E-10.11) *(D-26)*. For every input other than a void, each account other than 0 has available cash
afterwards at least the lesser of 0 and its value before, less the fees the input's fills charged it (the sum of
its positive `makerFeeCents` and `takerFeeCents`; rebates are not counted). So an account below zero can receive
cash but not spend it, and only fees take it further below. An implementation must treat anything else as a
fail-stop (§18). Account 0 is exempt, because it pays rebates. Wherever a rule compares an amount with available
cash, it compares with `max(0, available cash)`.

---

## 4. Processing an input

**E-4.1** An input's `record.sequence` must equal `lastSequence + 1` and its `record.sequencedAt` must be at
least `now`; otherwise fail-stop. The engine then sets `lastSequence` and `now` from the record before any
other processing.

**E-4.2** An input is processed completely before the next one. No rule observes a partly processed input.

**E-4.3** A message whose enum field holds an undefined value, or whose record header group differs from the
group's own, is rejected with `MALFORMED` if it is a client message and is a fail-stop if it is a command
or sequencer event.

**E-4.4** The processing of one client message, one command, one sequencer event, or one entry of
`BatchOrders`, is a **unit** (E-12.8). Results are organised by unit (§5).

---

## 5. Results and their order

**E-5.1** Every result carries a `ResultHeader` naming the input's sequence and time and its index among
that input's results, counting from 0 across all units of the input.

**E-5.2** A unit's results are emitted in five blocks, in this order:

1. **Outcome** — exactly one of `OrderAccepted`, `OrderRejected`, `OrderCancelled` (for `CancelOrder`),
   `OrderAmended`, `OrderGroupUpdated` (for `SetOrderGroup` and `ResetOrderGroup`) or
   `CommandApplied`. `MassCancel`, `TimeTick`, `EngineStarted` and `SeedSet` have no outcome result.
2. **Events** in the order they occur: `Fill`, `OrderCancelled`, `OrderReduced`, `OrderGroupUpdated`
   (triggers), `CollateralReleased`, `MarketStateChanged`, `SettlementPayout`, `EventReleased`.
3. **Positions** — one `PositionChanged` for each `(accountId, instrumentId)` whose position differs from its
   value at the start of the unit, ascending by `accountId` then `instrumentId`, carrying the final value.
4. **Sleeves** — one `SleeveChanged` for each account whose `balance` or `reserved` differs from its value
   at the start of the unit, ascending by `accountId`, carrying final `availableCents` and `reservedCents`.
5. **Book levels** — one `BookLevelChanged` for each `(instrumentId, side, tick)` whose aggregate remaining
   quantity or order count differs from its value at the start of the unit, ascending by `instrumentId`,
   then `BUY` before `SELL`, then `tick`, carrying the final quantity and count.

**E-5.3** A rejected unit emits only its outcome, and changes no state except the duplicate table (E-9.2),
the command set (E-7.1) and rule-limit counters (E-9.3 step 10).

**E-5.4** Every `Fill` carries the fees E-10.11 charged its maker and taker in `makerFeeCents` and
`takerFeeCents`: positive when the party pays, negative for a rebate. Account 0's sleeve is reported in block 4 like
any other account's.

---

## 6. Reference data commands

All commands first follow E-7.1. Every rejection below is `CommandApplied` with outcome `REJECTED` and
reason `INVALID_COMMAND` unless stated.

**E-6.1 `DefineTickTable`.** Rejected if the table ID exists (tables are immutable), if it has no rows, if
rows are not ordinals 0, 1, 2 … in order, if any row has `numerator == 0` or `numerator >= denominator`, if
probabilities are not strictly increasing, or if any row's `complement` `c` fails `P(t) + P(c) = 1` exactly
or `complement(c) = t`. Otherwise stored.

**E-6.2 `DefineEvent`.** Rejected if the event ID exists or `stateCount` is not between 2 and 64; then rejected
`LISTING_CAPACITY` if `maxEvents` is not 0 and the group carries `maxEvents` events (E-6.5). Otherwise stored with every
flag false and `collateralCents` 0.

**E-6.3 `ListInstrument`.** Rejected if the instrument ID exists; the event does not exist or is resolved;
the tick table does not exist; `lotStep` is 0; for any row of the tick table, `100 · lotStep · numerator`
is not divisible by `denominator`; `stateMask` is 0, has a bit at or above `stateCount`, or has every
state's bit set; or the class constraint fails (below). It is then rejected `LISTING_CAPACITY` if `maxInstruments`
is not 0 and the group carries `maxInstruments` instruments, or if `maxPriceLevels` is not 0 and the price levels the
group carries plus the listing's own exceed `maxPriceLevels` (E-6.5) *(D-30)*. The class constraints:

| Class | Constraint |
|---|---|
| `BINARY` | event `stateCount` is 2; no legs |
| `CATEGORICAL_OUTCOME` | exactly one bit set; no legs |
| `SCALAR_BAND` | no legs |
| `PARLAY` | event `stateCount` is 2; at least one leg |

Otherwise stored with `marketState` `LISTED` and empty books; outcome `CommandApplied` then
`MarketStateChanged`.

**E-6.4** An event is a **partition event** while it has at least two instruments, their state masks are
pairwise disjoint, and together they cover every state. The property is re-evaluated whenever an instrument
is listed.

**E-6.5 Carried reference data.** The group **carries** every event that has been defined and not released
(E-8.4), and every instrument that has been listed and not released. An instrument's **price levels** are twice the
number of rows in its tick table, and the price levels the group carries are the sum over the instruments it carries.
Tick tables are never released and are not counted.

---

## 7. Service commands

**E-7.1** Every command carries a `commandId`. If the command set holds it with an expiry later than `now`,
the outcome is `CommandApplied` with `DUPLICATE`, reason `NONE`, and nothing else happens. Otherwise the ID is
recorded whatever the outcome, with expiry `now + 604 800 · 10⁹` *(D-25)*. An applied command's outcome is `CommandApplied` `APPLIED` with reason `NONE`,
emitted before any result the command causes.

**E-7.2 `SleeveAdjust`.** `deltaCents` must be positive, else rejected `INVALID_COMMAND`. `GRANT` adds it
to `balance`. `RECLAIM` subtracts it if it does not exceed available cash, else rejected
`INSUFFICIENT_SLEEVE`.

**E-7.3 `SetRuleLimits`.** Replaces the limits for `accountId` (0 for the default).

**E-7.4 `SetGroupParameters`.** Replaces `activeOrderLimit` and `clientOrderIdWindowSeconds`; the window
must be positive, else rejected. A new window applies to duplicate entries recorded from then on; entries already
recorded keep their expiry (E-9.2).

**E-7.5 `SetAccountControls`.** Replaces the account's `closedOnly` and `selfMatchPrevention`. Resting
orders are not changed *(D-18)*.

**E-7.6 `SetMakerProtection`.** Sets the threshold and window for `(accountId, eventId)`; a threshold of 0
removes the setting. Counters are not reset.

**E-7.7 `ActivateProtocol`.** If `protocolVersion` is not greater than the current version, rejected. If
the engine does not implement it, or `requiredBinaryHash` is not its own hash, fail-stop. Otherwise the new
version applies from the next input.

**E-7.8 `SetEventPhase`, `SetMarketState`, `SetVenueMode`** — §16. **`ResolveEvent`, `ExpireParlay`** —
§15. **`TransferOut`, `TransferIn`** — §19.

**E-7.9 `SetFeeSchedule`.** Rejected if `makerRateMicros` is below −1 000 000 or above 1 000 000, or
`takerRateMicros` is below 0 or above 1 000 000. Otherwise replaces the default rates *(D-2)*. Fills already
emitted are not changed.

**E-7.10 `SetFeeOverride`.** Rejected if its rates fail the ranges of E-7.9, or `expiresAt` is present and not
later than `now`. Otherwise stores the override under `overrideId`, replacing any override with that ID *(D-27)*.

**E-7.11 `RemoveFeeOverride`.** Rejected if no active override (E-10.12) has `overrideId`. Otherwise removes it.

**E-7.12 `SetListingParameters`.** Rejected if `settledRetentionSeconds` is 0. Otherwise replaces
`settledRetentionSeconds`, `maxEvents`, `maxInstruments` and `maxPriceLevels`. A retention applies from the next
`TimeTick` to every resolved event not yet released, whenever it was resolved (E-8.4). A limit applies to later
listings only: nothing the group already carries is released or changed by a limit below it *(D-29, D-30)*.

---

## 8. Time

**E-8.1 `TimeTick`.** After E-4.1, every resting order with `expiresAt <= now` is cancelled with reason
`EXPIRED`, in ascending `(expiresAt, orderId)` order (E-12.1 applies to each). Expiry happens only on a
tick, so its precision is the tick interval *(D-5)*.

**E-8.2** Duplicate-table entries and command-set entries whose expiry is at or before `now` may be removed on a
tick. Removal is not observable, because E-9.2 and E-7.1 compare `now` with the expiry recorded, which a later
change of window does not move. Fee overrides whose `expiresAt` is at or before `now` may be removed at any time:
an expired override never wins (E-10.12), E-7.11 treats it as absent, and E-7.10 replaces it as if it were.

**E-8.3 `EngineStarted`, `SeedSet`.** No state change visible in results. `SeedSet` seeds the keyed hash
or PRNG; no rule in v0 draws randomness.

**E-8.4 Release.** On a `TimeTick`, after E-8.1: if the resolved event resolved earliest among those the group still
carries has `resolvedAt + settledRetentionSeconds · 10⁹ <= now`, under the retention in force at this tick, that event
is released. At most one event is released per tick, so a tick's work is bounded by one event's instruments; a
backlog, after the retention is shortened, is released one event a tick in the order the events were resolved
*(D-29)*. Releasing an event, in order: each of its instruments, ascending ID, ceases to exist; every maker protection
counter for `(accountId, eventId)` is discarded (E-13.2); the event ceases to exist; `EventReleased` is emitted with
its `eventId`. The event's positions are 0 and its books empty (E-15.5), so a release changes no position, sleeve or
book level and emits nothing else.

**E-8.5 After a release.** A released event or instrument is as if it had never been defined or listed. Every rule
that looks one up finds nothing: an order on a released instrument is rejected `UNKNOWN_INSTRUMENT` (E-9.3 check 4), a
cancel or amendment of an order on it `UNKNOWN_ORDER` (E-12.2, E-12.3), a mass cancel naming it removes nothing, and a
command naming it, a resolution or parlay expiry included, `INVALID_COMMAND`. Its ID may be defined or listed again,
and is then a new event or instrument. Nothing that is not the listing's own is released: accounts, sleeves, the
duplicate table and command set, rule limits, order groups, maker protection settings and fee overrides are keyed by
account, client order ID, command, group or override, so they stay, and a setting or override naming a released ID
applies again to a listing that reuses it *(D-29)*.

---

## 9. New orders

**E-9.1** A `NewOrder`, or a `NEW` entry of `BatchOrders`, is processed by the checks of E-9.3 in order.
The first failing check rejects the order with the reason given, and nothing changes except as E-5.3 allows.

**E-9.2 Duplicates.** A client order ID is the 32 bytes of `clientOrderId`. An entry `(accountId,
clientOrderId)` whose expiry is later than `now` makes the order a duplicate: rejected
`DUPLICATE_CLIENT_ORDER_ID` with `originalOrderId` set to the order it created, or null if it created none.
Otherwise, from check 3 on, the entry is recorded whatever the outcome, with expiry
`now + clientOrderIdWindowSeconds · 10⁹` under the window in force at this input, and updated with the order ID
if the order is accepted *(D-12)*.

**E-9.3 Checks.**

| # | Check | Reason |
|---|---|---|
| 1 | Account is not 0; `clientOrderId` is not all zero; `quantity > 0`; `expiresAt` present exactly when `GTD`; `postOnly` not combined with `IOC` or `FOK`; `spendCapCents`, if present, is positive and the order is a purchase in entered terms (E-2.7) | `MALFORMED` |
| 2 | Not a duplicate (E-9.2) | `DUPLICATE_CLIENT_ORDER_ID` |
| 3 | `venueMode` is `NORMAL` | `VENUE_MODE` |
| 4 | Instrument exists | `UNKNOWN_INSTRUMENT` |
| 5 | `marketState` is `OPEN` and the event is not `suspended` | `MARKET_NOT_OPEN` |
| 6 | `tick` is a row of the tick table | `TICK_NOT_ON_LADDER` |
| 7 | `quantity` is a multiple of `lotStep` | `QUANTITY_NOT_ON_LOT_STEP` |
| 8 | For `GTD`, `expiresAt > now` | `EXPIRY_IN_PAST` |
| 9 | The order's group, if any, is not `triggered` | `ORDER_GROUP_TRIGGERED` |
| 10 | Rule limits (E-9.4); on passing, the new-order counter increments | `RULE_LIMIT` |
| 11 | Reduce-only (E-9.5): at least one lot is reducible | `REDUCE_ONLY_NO_POSITION` if the order is flagged `REDUCE_ONLY`, otherwise `CLOSED_ONLY_MODE` |
| 12 | Spend cap: `spendCapCents` covers one lot's gross spend at the effective limit (E-9.6) | `SPEND_CAP_BELOW_ONE_CONTRACT` |
| 13 | Post-only: the best opposite tick does not cross the effective limit | `POST_ONLY_WOULD_TAKE` |
| 14 | `FOK`: the dry run (E-10.9) fills the whole quantity | `FOK_NOT_FILLABLE` |
| 15 | Position limit (E-9.7) | `POSITION_LIMIT` |
| 16 | If `GTC` or `GTD` and the dry run fills less than the quantity: the account's resting orders in the group are below `maxRestingOrders` | `RULE_LIMIT` |
| 17 | Same condition: the group's resting orders are below `activeOrderLimit` | `ORDER_CAPACITY` |
| 18 | The order's reservation (E-11.2) does not exceed available cash | `INSUFFICIENT_SLEEVE` |

A limit of 0 in checks 10, 15, 16 and 17 means none.

**E-9.4 Rule limits.** With `s = ⌊now / 10⁹⌋`, `n` the account's new orders counted in second `s` and `f`
its fills in second `s` (each `Fill` it is party to counts once), using the account's own limits or else
account 0's: reject if `maxNewOrdersPerSecond > 0` and `n >= maxNewOrdersPerSecond`; reject if
`maxOrderToTradeRatioMilli > 0` and `n >= ratioFreeOrdersPerSecond` and
`(n + 1 − ratioFreeOrdersPerSecond) · 1000 > maxOrderToTradeRatioMilli · f` *(D-11)*.

**E-9.5 Reduce-only.** An order is reduce-only if `REDUCE_ONLY` is set or the account is `closedOnly`. Its
**reducible** quantity is the account's unlocked holding on the side it reduces: for `SELL`,
`max(0, position) − yesLocked`; for `BUY`, `max(0, −position) − noLocked`. The quantity is cut to the largest
multiple of `lotStep` not above `min(quantity, reducible)`; if that is 0 the order is rejected. A reduce-only
order is always fully locked (E-11.1).

**E-9.6 Effective limit.** With `n = protectionTicks`: if `n > 0` and the opposite side of the book has a
best tick `b`, the effective limit is `min(tick, b + n)` for `BUY` and `max(tick, b − n)` for `SELL`,
clamped to the table. Otherwise it is `tick` *(D-15)*. Check 12 compares `spendCapCents` with the gross
spend of one lot at the effective limit *(D-16)*.

**E-9.7 Position limit.** For `BUY`: `position + (remaining of the account's resting BUY orders on the
instrument) + quantity > positionLimit` rejects. For `SELL`: `−position + (remaining of resting SELL orders) +
quantity > positionLimit` rejects *(D-17)*.

**E-9.8 Acceptance.** The order receives the next order ID and `acceptedAt = input sequence`. Its lock is
taken (E-11.1), its reservation computed and added to the account (E-11.2), and `OrderAccepted` is emitted
with the effective limit, the quantity after any reduce-only cut and the reservation. The duplicate entry
records the order ID. Then it matches (§10).

**E-9.9 After matching.** If quantity remains:

- `IOC` (and `FOK`, which cannot have any): cancelled with reason `IOC_REMAINDER`.
- `GTC` or `GTD` with a spend cap: the remaining quantity is cut to the largest multiple of `lotStep` whose
  gross spend at the effective limit does not exceed `spendCapRemaining`. If that is 0 the order is
  cancelled with `SPEND_CAP_REACHED`; if it is smaller than before, its lock is cut to at most the new quantity
  and `OrderReduced` is emitted. Then it rests.
- `GTC` or `GTD` otherwise: it rests at the back of its tick's queue.

A resting order's reservation is recomputed (E-11.2) whenever its remaining quantity, lock or spend cap
changes.

---

## 10. Matching and clearing

**E-10.1 Crossing.** A `BUY` with effective limit `L` crosses a resting `SELL` at tick `t` if `t <= L`; a
`SELL` crosses a resting `BUY` if `t >= L`. The book is never crossed after a unit (E-10.10).

**E-10.2 Price.** Every fill is at the resting order's tick.

**E-10.3 Walk.** The incoming order visits opposite ticks from the best (lowest ask, highest bid) toward
its limit. At each tick it takes `Q`, the largest multiple of `lotStep` not above the smaller of its remaining
quantity and, for a spend-capped order, what `spendCapRemaining` affords at that tick. If `Q` is 0 the walk
stops.

**E-10.4 Price-time allocation.** Resting orders at the tick are visited oldest first. Each fills
`min(Q_left, its remaining)`; self-match prevention (E-13.1) applies to each before it fills. An
order that lock restoration (E-11.4) removed earlier in the walk is skipped.

**E-10.5 Pro-rata allocation.** Self-match prevention is applied to the account's own orders at the tick
first (E-13.1). If the other orders' total `M` is at most `Q`, each fills completely. Otherwise, measuring
in lots, order `i` with `m_i` lots receives `⌊Q · m_i / M⌋`, and the lots left over go one each to the orders
with the largest `(Q · m_i) mod M`, ties to the lower order ID. Fills are emitted oldest first; orders
receiving nothing are not filled. An order that lock restoration shrank during the allocation fills at most its remaining
quantity, and one it removed is skipped.

**E-10.6 One fill.** For `q` contracts at tick `t` between buyer `B` and seller `S`, using positions before
the fill:

```
yes      = yesCost(t, q)
bBurn    = min(q, max(0, −position(B)))      units that close B's NO
sReduce  = min(q, max(0,  position(S)))      units that close S's YES
burned   = min(bBurn, sReduce)
minted   = q − max(bBurn, sReduce)
buyerCash  = −yes + 100 · bBurn
sellerCash =  yes − 100 · (q − sReduce)
```

Then `position(B) += q`, `position(S) −= q`, `balance(B) += buyerCash`, `balance(S) += sellerCash`,
the same amounts are added to each account's `eventCash` for the event, and the event's `collateralCents += 100 · (minted − burned)`, which always equals
`−(buyerCash + sellerCash)`. Fees are then charged (E-10.11). A `Fill` is emitted with the trade ID, both orders'
remaining quantities after the fill, `mintedPairs`, `burnedPairs`, both cash changes, which exclude fees, and both
fees.

**E-10.7** After each fill, in this order: both orders' locks are consumed (E-11.3); lock restoration runs
for both accounts (E-11.4); a resting order with nothing remaining leaves the book; rule-limit fill counters
increment for both accounts; protection counters update (E-13.2, E-13.3). Reservations of both orders are
recomputed.

**E-10.8 End of walk.** The walk ends when the incoming order has nothing remaining, the next tick does not
cross, `Q` is 0, or self-match prevention stops it.

**E-10.9 Dry run.** The dry run is the walk of E-10.3 to E-10.8 for the order as it would be accepted — its
lock taken, and self-match prevention, the spend cap and lock restoration (E-11.4) all applied — evaluated on a
copy of the state that is then discarded. Its result is the quantity the walk filled. A walk over the book
alone is not enough, because a fill can shrink a reduce-only order further along the walk (scenario 18).
Protection triggers cannot affect it, because they act at the end of the unit (E-13.4). An implementation may
use an undo log rather than a copy; only the answer is specified.

**E-10.10** After every unit the best bid is strictly below the best ask in every book. A violation is a
fail-stop.

**E-10.11 Fees.** The maker of a fill is the resting order's account and the taker the incoming order's. For `q`
contracts at tick `t`, a party whose rate is `r` (E-10.12) is charged

```
fee(r, t, q) = ⌈ r · yesCost(t, q) · noCost(t, q) / (100 000 000 · q) ⌉      cents
```

which is `r / 10⁶ × q × P(t) × (1 − P(t))` dollars. The quotient is exact and then rounded toward positive
infinity to a whole cent, for both signs: a charge is never less than the formula and a rebate, from a negative
`r`, never more (`⌈−1.3⌉ = −1`) *(D-2)*. With `makerFee` and `takerFee` so computed: `balance(maker) −= makerFee`,
`balance(taker) −= takerFee` and `balance(0) += makerFee + takerFee`, and the same changes are made to each of the
three accounts' `eventCash` for the event, so the fees change no event's `collateralCents` *(D-28)*. Fees are
neither reserved (E-11.2) nor compared with available cash: no order is rejected, reduced or stopped for a fee it
would pay, and a fee may take available cash below zero (E-3.3) *(D-26)*. A spend cap counts gross spend without
fees (E-11.3). Fees play no part in the dry run's answer (E-10.9).

**E-10.12 Rates.** The maker pays at the `makerRateMicros` and the taker at the `takerRateMicros` of the override
that wins for that party, or of the default schedule if none does; each party is resolved on its own. An override
**matches** a party when its `accountId` is 0 or the party's account, its `instrumentId` is 0 or the fill's
instrument, and its `eventId` is 0 or the instrument's event. It is **active** while it has no `expiresAt` or
`now < expiresAt`. Of the active overrides that match, the lowest `priority` wins, and of equal priorities the
lowest `overrideId` *(D-27)*. Which fields an override names does not count: one naming the account beats one
naming only the instrument only by a lower priority.

---

## 11. Holdings locks and reservations

**E-11.1 Lock.** At acceptance an order locks holdings on the side it would reduce: a `SELL` locks the
largest multiple of `lotStep` not above `min(quantity, max(0, position) − yesLocked)` and adds it to
`yesLocked`; a `BUY` locks the largest multiple of `lotStep` not above
`min(quantity, max(0, −position) − noLocked)` and adds it to `noLocked`. A reduce-only order's quantity was
already cut to that amount. Locks are never increased later *(D-7)*.

**E-11.2 Reservation.** An order's reservation is its unit cost at its effective limit for
`remaining − lock` contracts, capped at `spendCapRemaining` if it has one. The account's `reserved` is the
sum of its orders' reservations at all times. A reservation includes no fee (E-10.11) *(D-26)*.

**E-11.3 Consuming a lock.** When `f` contracts of an order fill, `min(f, lock)` is removed from the
order's lock and from the account's `yesLocked` or `noLocked`; the order's `remaining` falls by `f`; and a
spend-capped order's `spendCapRemaining` falls by the fill's gross spend (E-2.7 at the fill tick).

**E-11.4 Lock restoration.** After a fill, if an account's `yesLocked` exceeds `max(0, position)`, or
`noLocked` exceeds `max(0, −position)`, locks are removed from that account's resting orders on the
affected side and instrument, most recently accepted first — an amendment that lost priority counts as
accepted when it was amended — until it does not. A normal order losing lock
keeps its quantity and its reservation grows. A reduce-only order losing lock loses the same quantity: it
emits `OrderReduced`, or `OrderCancelled` with `POSITION_CLOSED` if nothing remains. Each emission follows
the `Fill` that caused it.

**E-11.5** A resting order's lock is released when it leaves the book for any reason.

---

## 12. Cancels, amendments, mass cancels and batches

**E-12.1 Removing an order.** Removing a resting order removes it from its queue, releases its lock and
reservation, and emits `OrderCancelled` with its remaining quantity and the reason.

**E-12.2 `CancelOrder`.** The target is `orderId` if present, otherwise the account's resting order with
that `clientOrderId`. It must exist, rest, belong to `accountId` and be on `instrumentId`; else rejected
`UNKNOWN_ORDER`. Allowed in every venue mode and market state. Outcome: `OrderCancelled` with
`CLIENT_REQUEST`.

**E-12.3 `AmendOrder` checks.** In order: the order exists, rests, belongs to the account and is on the
instrument (`UNKNOWN_ORDER`); at least one of `newTick`, `newQuantity` or an `atEventStart` other than
`UNCHANGED` is given, and `newQuantity` is not 0 (`MALFORMED`); `newTick` is on the table
(`TICK_NOT_ON_LADDER`); `newQuantity` is a multiple of `lotStep` (`QUANTITY_NOT_ON_LOT_STEP`).

**E-12.4 Reductions.** If `newTick` is absent or equal to the order's tick and `newQuantity` is absent or
at most `remaining`, the amendment is a reduction: allowed in every venue mode and market state. The order
keeps its place; `remaining` and `atEventStart` are updated; its lock is cut to at most `remaining`
(releasing the difference) and its reservation recomputed. Outcome: `OrderAmended` with `priorityKept`
`TRUE`.

**E-12.5 Repricing and increases.** Otherwise the order loses priority. Additional checks, in order:
`venueMode` is `NORMAL` or `NEW_ORDER_REJECT` (`VENUE_MODE`); `marketState` is `OPEN` and the event not
suspended (`MARKET_NOT_OPEN`); a reduce-only order's new quantity does not exceed its lock
(`REDUCE_ONLY_NO_POSITION`); a post-only order at the new tick does not cross (`POST_ONLY_WOULD_TAKE`); the
position limit holds with the order's old quantity excluded (`POSITION_LIMIT`); the reservation at the new
tick and quantity, less the old reservation, does not exceed available cash (`INSUFFICIENT_SLEEVE`). On
failure the order is unchanged. On success the order leaves its queue, takes the new tick and quantity with
its existing lock cut to at most the new quantity, counts as accepted at this input for E-11.4, emits `OrderAmended` with `priorityKept` `FALSE`, and is
processed as an incoming order from E-10.3 — it may fill — and then rests at the back of its tick's queue
with the same order ID *(D-4)*.

**E-12.6 `MassCancel`.** Removes the account's resting orders in scope — `SESSION` (orders with this
`sessionId`), `ACCOUNT`, `INSTRUMENT`, `EVENT` or `CLIENT_ORDER_GROUP` (with `scopeRef`) — in ascending order
ID, with reason `MASS_CANCEL`, `SESSION_DISCONNECT`, `HEARTBEAT_MISSED` or `KILL_SWITCH` for causes
`CLIENT_REQUEST`, `SESSION_DISCONNECT`, `HEARTBEAT_MISSED` and `OPERATOR_KILL_SWITCH`. Allowed in every mode
and state, never rate-limited. With nothing in scope it emits nothing *(D-19)*.

**E-12.7 `SetOrderGroup`, `ResetOrderGroup`.** `SetOrderGroup` stores the threshold and window for the
group (0 removes the limit) and emits `OrderGroupUpdated`. `ResetOrderGroup` clears `triggered`, zeroes the
counter and emits `OrderGroupUpdated`; a group never set is reported with threshold 0.

**E-12.8 `BatchOrders`.** Entries are processed in order, each as its own unit with the batch's
`sessionId` and `accountId`: a `NEW` entry as a `NewOrder`, a `CANCEL` entry as a `CancelOrder` on
`orderId` if present or `clientOrderId` otherwise. An entry's rejection does not affect the others. A batch
with no entries emits nothing.

---

## 13. Protections: self-match, auto-pull and order groups

**E-13.1 Self-match prevention.** When the incoming order reaches a resting order of its own account, the
account's mode decides: `CANCEL_RESTING` removes the resting order (`SELF_MATCH_PREVENTED`) and the walk
continues; `CANCEL_INCOMING` stops the walk and the incoming order's remainder is cancelled
(`SELF_MATCH_PREVENTED`) instead of resting; `CANCEL_BOTH` does both. Under pro-rata allocation the account's
own orders at the tick are handled together, in ascending order ID, before allocation.

**E-13.2 Maker auto-pull.** After a fill, for the maker's account and the instrument's event, using the
setting for `(account, event)` or else `(account, 0)`: if the window has elapsed
(`now − windowStart >= windowMillis · 10⁶`) or never started, the counter resets to 0 and the window starts at
`now`; the fill quantity is added. A counter at or above the threshold marks the pair for pulling.

**E-13.3 Order groups.** After a fill, for each of the two orders that has a client order group with a
threshold: the same window rule, counting the fill quantity; a counter at or above the threshold marks the
group for triggering.

**E-13.4 End of unit.** After matching and resting, in this order: every pair marked by E-13.2, ascending
by account then event, removes the account's resting orders in the event (`MAKER_AUTO_PULL`) and resets its
counter; every group marked by E-13.3, ascending by account then group, becomes `triggered`, emits
`OrderGroupUpdated`, removes its resting orders (`ORDER_GROUP_TRIGGERED`) and resets its counter *(D-10)*.
Then set burns run (E-14.3).

---

## 14. Set burns

**E-14.1, E-14.2** Withdrawn. There is no basket mint or redeem: contracts are created and destroyed only by
fills (E-10.6) and set burns *(D-24)*.

**E-14.3 Set burns.** At the end of a unit containing fills, for each partition event with fills and each
account that was party to one, ascending by event then account, with `N` the event's instruments:

- `k` is the largest multiple of every instrument's `lotStep` not above the minimum over its instruments of
  `max(0, position) − yesLocked`. If `k > 0`: every position falls by `k`,
  `balance += 100 · k`, `collateralCents −= 100 · k`, and `CollateralReleased` is emitted with `YES_SET_BURN`.
- Then `k` is the largest multiple of every instrument's `lotStep` not above the minimum over its instruments
  of `max(0, −position) − noLocked`. If `k > 0`: every position rises by
  `k`, `balance += 100 · (N − 1) · k`, `collateralCents −= 100 · (N − 1) · k`, and `CollateralReleased` is
  emitted with `NO_SET_BURN` *(D-8, D-9)*.

Each release is also added to the account's `eventCash` for the event.

---

## 15. Resolution and settlement

**E-15.1 Payout.** A position `p` in an instrument with mask `m` pays, in state `s`: `100 · p` if `p > 0`
and `s ∈ m`; `100 · (−p)` if `p < 0` and `s ∉ m`; otherwise 0.

**E-15.2 Conservation.** For every unresolved event and every state `s`, the sum over all accounts and
instruments of the payout in `s` equals the event's `collateralCents`. Every rule above preserves this; a
violation is a fail-stop.

**E-15.3 `ResolveEvent` checks.** Rejected `INVALID_COMMAND` if the event does not exist or is resolved; for
`RESOLVED`, if `winningState >= stateCount`; for `VOID`, if `voidWeights` is not empty *(D-1)*.

**E-15.4 Payouts.** `RESOLVED` pays each position its E-15.1 payout in `winningState`. `VOID` reverses the
event: each account is paid `−eventCash` for the event, undoing every fill, fee and set burn in it whether or
not the account still holds a position. The payment is negative for an account that took more cash out of the
event than it put in, and for account 0 when the event's fees exceeded its rebates *(D-1, D-28)*.

**E-15.5 Effect.** In order: `CommandApplied`; every resting order in the event's instruments is removed
with `MARKET_RESOLVED`, ascending order ID; each instrument, ascending ID, takes `marketState` `SETTLED`
(`RESOLVED`) or `VOIDED` (`VOID`) and emits `MarketStateChanged`; then:

- `RESOLVED`: for each account with a non-zero position in the event, ascending by account then instrument,
  the payout is added to `balance`, the position set to 0, and `SettlementPayout` emitted with the position
  closed and the cents.
- `VOID`: for each account with a non-zero position or non-zero `eventCash` in the event, ascending by
  account, every position in the event is set to 0, `−eventCash` is added to `balance`, and one
  `SettlementPayout` is emitted with `instrumentId` 0, `positionClosed` 0 and those cents.

Every account's `eventCash` for the event is then discarded.

**E-15.6 Conservation at settlement.** The payouts sum to the event's `collateralCents` exactly: for `RESOLVED`
by E-15.2, and for `VOID` because every change to `eventCash` either moved `collateralCents` by its negation or,
for a fee, was matched by the opposite change to account 0's `eventCash` (E-10.11). Anything
else is a fail-stop. The event is marked resolved with `resolvedAt` = `now`, its `collateralCents` set to 0, and it
joins the end of the releases (E-8.4) *(D-1, D-21, D-29)*. A resolution is final: E-15.3 rejects any later
resolution of the event, so nothing moves `resolvedAt`, and once the event is released no input can name it.

**E-15.7 `ExpireParlay`.** Rejected `INVALID_COMMAND` unless the instrument exists, is a `PARLAY` and its
event is unresolved. Then processed as `ResolveEvent` `RESOLVED` on the instrument's event with the
winning state the lowest state not in the parlay's mask *(D-22)*.

---

## 16. Market state, event phases and venue modes

**E-16.1 `SetMarketState`.** Allowed transitions: `LISTED → OPEN`, `OPEN → HALTED`, `HALTED → OPEN`,
`OPEN → CLOSED`, `HALTED → CLOSED`, `CLOSED → RESOLVING`, `RESOLVING → CLOSED`. Any other, including any to
`SETTLED` or `VOIDED`, or on a resolved event, is rejected. `CLOSED` removes the instrument's resting orders
with `MARKET_CLOSED` *(D-23)*. Emits `MarketStateChanged`.

**E-16.2** Matching and new orders require `OPEN` (E-9.3 check 5). A `HALTED` book keeps its resting
orders and accepts cancels and reductions only.

**E-16.3 `SetEventPhase`.** `STARTED` requires the event not started (else rejected); it sets `started`
and removes every resting order in the event with `atEventStart` `CANCEL` (`EVENT_START`), ascending order
ID. `SUSPENDED` sets `suspended`; `RESUMED` requires `suspended` and clears it; `ENDED` sets `ended`, with
no other effect. `SCHEDULED`, or any phase on a resolved event, is rejected.

**E-16.4 `SetVenueMode`.** Sets `venueMode`. `KILL_SWITCH` first removes every resting order in the group
with `KILL_SWITCH`, ascending order ID. `NEW_ORDER_REJECT` rejects new orders;
`CANCEL_ONLY` and `KILL_SWITCH` additionally reject amendments that reprice or increase (E-12.5). Cancels,
mass cancels and reductions are allowed in every mode.

---

## 17. Sequencer obligations

The sequencer is not the engine, but the engine's determinism depends on it.

**E-17.1** The sequencer assigns `sequence` gap-free from 1 per group and stamps `sequencedAt` as the later
of its clock and the previous input's `sequencedAt` plus 1 nanosecond, so time never runs backwards across an
election (blueprint §5).

**E-17.2** It stamps `RecordHeader` in place in the ingress bytes; it changes no other byte.

**E-17.3** It injects `TimeTick` on a fixed cadence (1 ms in production), and writes `EngineStarted` when a
node's engine starts.

**E-17.4** A client message is acknowledged to its sender only after the input is committed to a quorum
and its outcome result exists.

---

## 18. Fail-stop

**E-18.1** The engine stops, emitting nothing further, on: a sequence or time violation (E-4.1); a malformed
command or sequencer event (E-4.3); an arithmetic overflow (E-2.2); available cash falling below the bound of
E-3.3; a crossed book (E-10.10); a conservation violation (E-15.2, E-15.6); an unsupported protocol activation (E-7.7).

**E-18.2** Stopping is deterministic: every replica stops at the same input. Recovery is an operator
decision — a fixed engine replaying the journal, or the quarantine path of blueprint §5.

**E-18.3** An implementation holds its tables in storage sized when it starts. Filling one is a fail-stop, not a
rejection: storage is not in the journal, so a rejection that depended on it could differ between replicas, where a
fail-stop only stops them. The sequenced limits are what reject: `activeOrderLimit` (E-9.3 check 17), and
`maxEvents`, `maxInstruments` and `maxPriceLevels` (E-6.2, E-6.3). An operator sets them no higher than the storage
every replica has; an implementation must then never fail-stop for want of room for the events, instruments and price
levels they admit, so storage a release frees (E-8.4) must be reusable by any later listing *(D-30)*.

---

## 19. Not in v0

These are required eventually and deliberately absent from v0, so that nothing is half-specified:

- `TransferOut` and `TransferIn`: the event-state encoding they carry (§20 specifies the snapshot encoding
  they are a slice of, but not which events may move or what the destination group does with them). Until
  then the engine rejects both with `INVALID_COMMAND`.
- RFQ requests, quotes and acceptances (blueprint §2).
- Risk-group margining and floor advances across separate mints (blueprint §8).
- Designated-maker allocation privileges, split FIFO and top-order allocation.
- Quote lifespans, per-event notional caps, move-based halts, maximum GTC lifetime.
- Dormant book paging (blueprint §10), which must not change any result when added.
- Order-level (L3) market data.

---

## 20. Canonical snapshots

A snapshot is the engine's state written out so that another engine can be put into that state and carry on.
It exists so that recovery, and a reader joining the feed, need not replay from the first input: replay from
genesis stops being practical within weeks (blueprint §4), and a journal that can never be cut can never be
archived (§12).

**E-20.1** A snapshot is taken **between inputs**, never during one. The state it describes is the state after
the input named by its `lastSequence` and before any later input. An engine that has fail-stopped (§18) must not
be snapshotted: its state is the state at which processing stopped, and nothing may continue from it.

**E-20.2** A snapshot is **logical, not physical**. It carries what the rules of this specification observe, and
nothing about how an implementation arranged it. In particular it carries no slab slot, no pool offset, no free
list, no hash-table position, no ring-buffer head, no heap permutation and no table capacity. Two engines that
this specification cannot tell apart must produce byte-identical snapshots, whatever their histories.

**E-20.3 Contents.** A snapshot carries exactly the state of E-3.1, and the following, which E-3.1 leaves to an
implementation but which is observable through E-3.3:

| State | Contents |
|---|---|
| Fee accounting | `unitSerial`; per account `feesUnit` and `feesCharged` |

`unitSerial` numbers the units of work an engine has completed, so that fees charged in one unit are not counted
against the available-cash floor of the next (E-3.3). An account's `feesCharged` is read only when its `feesUnit`
equals the current `unitSerial`. Carrying the accounts without the serial would leave a stale `feesUnit` able to
collide with the restored numbering, and the floor would then be computed from fees charged in a unit that this
engine never ran. An encoder may instead write `feesUnit` and `feesCharged` as their initial values for every
account and `unitSerial` as zero, since a snapshot is taken between units and no account's fees are then in
force; it must do one or the other, and the same one every time.

**E-20.4 Order.** Every keyed table is written ascending by key: tick tables by `tickTableId`, events by
`eventId`, instruments by `instrumentId`, orders by `orderId`, accounts by `accountId`, holdings by
`(accountId, instrumentId)`, event cash by `(accountId, eventId)`, order groups by `(accountId, clientOrderGroup)`,
maker settings and counters by `(accountId, eventId)`, duplicates by `(accountId, clientOrderId)`, commands by
`commandId`. Fee overrides are written by `(accountId, priority, overrideId)`. Within a tick table, rows are
written in their given order, which is the order that defines them.

Books are written per instrument in `instrumentId` order, within an instrument `BUY` before `SELL`, within a side
by tick ascending, and within a tick **in queue order, head first** — the order in which the orders would match.
This is the only place where the order of a snapshot carries information that no key could: time priority is
state, and losing it would change every later fill.

The release queue (E-8.4) is written in the order events will be released, which is the order they resolved.

**E-20.5 Entries at their initial value are not written.** E-3.2 says an account exists from its first mention and
that until then every field has its initial value; an account whose every field holds its initial value is
therefore indistinguishable from one never mentioned, and writing it would make two engines this specification
cannot tell apart produce different bytes. The same holds for every other keyed table. A decoder creates an entry
when it reads one, and answers for any other key exactly as it answers for a key never mentioned.

**E-20.6 What a decoder rebuilds.** A snapshot does not carry state that is determined by what it does carry. A
decoder recomputes, and must arrive at: each account's `reserved` (the sum of its orders' `reserved`), each
level's quantity and order count, each book's occupancy, the group's and each account's resting-order counts, and
the carried counts that E-6.5 keeps against `maxEvents`, `maxInstruments` and `maxPriceLevels`. An implementation
that cannot recompute one of these from the snapshot's contents has found a gap in E-20.3 rather than a reason to
add a field.

**E-20.7 Capacity is not carried, and the omission is safe in one direction only.** Table sizes are an
implementation's, not this specification's (D-30), and a snapshot is restored into an engine whose tables may be
larger, smaller or differently shaped. Nor does a snapshot carry the residue that a running engine accumulates:
records in the duplicate and command windows whose entries have already gone, GTD records for orders no longer
resting, and fee overrides that have expired. All of these are invisible to every rule in this specification
except one — `FailStop::Capacity` (E-18.3) — because they occupy room until something reclaims them.

A restored engine therefore holds **no residue at all**, and so has at least as much room as the engine the
snapshot was taken from. Restoring can postpone a capacity fail-stop; it cannot bring one forward. This is what
makes E-20.2 sound rather than merely convenient, and it is why the equivalence property below is stated for runs
that do not fail-stop on capacity: within that regime the two runs agree exactly, and outside it the restored run
is the one that survives longer.

**E-20.8 Restoring.** An engine restored from a snapshot has `lastSequence` and `now` from the snapshot, and is
in every other respect as if it had replayed every input up to that sequence. E-4.1 then applies unchanged: the
next input's `record.sequence` must equal `lastSequence + 1`. A restored engine is not a fresh engine, and the
initial values of E-3.1 describe only an engine that has been given no snapshot.

An engine must refuse a snapshot it cannot honour: one whose `protocolVersion` it does not implement, or whose
contents do not fit its tables. Refusing is not a fail-stop — nothing has been processed — and an operator may
retry with a larger engine.

**E-20.9 The equivalence property.** For a journal, a sequence `N` and a later sequence `M`, let *direct* be a
fresh engine that applies every input from 1 to `M`, and *restored* be a fresh engine given the snapshot that
*direct* would take after `N`, which then applies every input from `N+1` to `M`. Provided neither run fail-stops,
and neither would fail-stop on capacity (E-20.7):

- the results the two engines emit for inputs `N+1` to `M` are byte-identical, in the same order; and
- the snapshots the two engines take after `M` are byte-identical.

This holds for any `N`, and whatever table sizes, hash seeds or prior churn *restored* was built with. The second
half is what makes the property worth testing: equal results could be produced by two engines whose state had
already diverged in a way no input had yet reached.

**E-20.10 Who takes a snapshot.** A snapshot is taken by an engine replaying the journal, never by the engine
serving traffic (blueprint §4): the hot path does not pause to serialise itself. Because replay is deterministic,
a snapshot taken this way is the same snapshot the serving engine would have taken, which E-20.9 is the statement
of.

**E-20.11 Snapshots carry no personal data**, because engine state carries none: an account is an identifier, and
the mapping from it to a person lives only in the accounts database (blueprint §12). A snapshot may therefore be
moved, copied and restored wherever the journal may go, and a test environment restored from one holds no more
personal data than the journal it came from. What may not be copied into a test environment is the accounts
database, which is where the mapping is and which no snapshot touches.
