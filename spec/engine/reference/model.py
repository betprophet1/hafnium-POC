"""Reference model of spec/engine/engine-spec.md, draft v0.

A second statement of the specification, written to be read beside it: integer arithmetic only, no
performance concerns, rule numbers in comments. Inputs and results are dicts whose keys are the schema's
field names; enum values are the schema's names; client order IDs and command IDs are strings.

    engine = Engine(group=1)
    results = engine.apply({"msg": "DefineEvent", "record": {...}, "commandId": "c1", ...})

The model raises FailStop exactly where the specification requires the engine to stop (§18), and also on
any internal inconsistency, which in a correct model never happens.
"""

import copy
from dataclasses import dataclass, field

NS_PER_SECOND = 10**9
NS_PER_MILLI = 10**6

CLIENT_MESSAGES = {
    "NewOrder", "CancelOrder", "AmendOrder", "MassCancel", "BatchOrders", "BasketMintRedeem",
    "SetOrderGroup", "ResetOrderGroup",
}
COMMANDS = {
    "ActivateProtocol", "SleeveAdjust", "DefineTickTable", "DefineEvent", "ListInstrument", "SetEventPhase",
    "SetMarketState", "SetVenueMode", "ResolveEvent", "ExpireParlay", "SetRuleLimits", "SetGroupParameters",
    "SetAccountControls", "SetMakerProtection", "TransferOut", "TransferIn",
}
SEQUENCER_EVENTS = {"TimeTick", "EngineStarted", "SeedSet"}

CANCEL_CAUSE_REASON = {
    "CLIENT_REQUEST": "MASS_CANCEL",
    "SESSION_DISCONNECT": "SESSION_DISCONNECT",
    "HEARTBEAT_MISSED": "HEARTBEAT_MISSED",
    "OPERATOR_KILL_SWITCH": "KILL_SWITCH",
}
MARKET_TRANSITIONS = {
    ("LISTED", "OPEN"), ("OPEN", "HALTED"), ("HALTED", "OPEN"), ("OPEN", "CLOSED"), ("HALTED", "CLOSED"),
    ("CLOSED", "RESOLVING"), ("RESOLVING", "CLOSED"),
}


class FailStop(Exception):
    """§18: the engine stops."""


class Reject(Exception):
    def __init__(self, reason):
        super().__init__(reason)
        self.reason = reason


# ----------------------------------------------------------------------------------------------- state (§3)

@dataclass
class TickRow:
    numerator: int
    denominator: int
    complement: int


@dataclass
class Event:
    event_id: int
    state_count: int
    scheduled_start: int
    started: bool = False
    suspended: bool = False
    ended: bool = False
    resolved: bool = False
    collateral: int = 0
    instruments: list = field(default_factory=list)


@dataclass
class Instrument:
    instrument_id: int
    event_id: int
    instrument_class: str
    tick_table_id: int
    lot_step: int
    allocation_policy: str
    state_mask: int
    position_limit: int
    legs: list
    market_state: str = "LISTED"
    book: dict = field(default_factory=lambda: {"BUY": {}, "SELL": {}})


@dataclass
class Order:
    order_id: int
    account_id: int
    session_id: int
    client_order_id: str
    instrument_id: int
    side: str
    entered_contract: str
    tick: int
    remaining: int
    time_in_force: str
    expires_at: object
    at_event_start: str
    post_only: bool
    reduce_only: bool
    spend_cap_remaining: object
    client_order_group: object
    accepted_at: int
    lock: int = 0
    reserved: int = 0
    resting: bool = False


@dataclass
class Account:
    balance: int = 0
    reserved: int = 0
    positions: dict = field(default_factory=dict)
    yes_locked: dict = field(default_factory=dict)
    no_locked: dict = field(default_factory=dict)
    closed_only: bool = False
    self_match_prevention: str = "CANCEL_INCOMING"          # D-3
    second: int = -1
    new_orders_in_second: int = 0
    fills_in_second: int = 0


@dataclass
class Unit:
    """§5: one unit's results and the start values it compares against."""
    outcome: list = field(default_factory=list)
    events: list = field(default_factory=list)
    positions_at_start: dict = field(default_factory=dict)
    sleeves_at_start: dict = field(default_factory=dict)
    levels_at_start: dict = field(default_factory=dict)
    filled: set = field(default_factory=set)                # (eventId, accountId) with a fill
    pulls: set = field(default_factory=set)                 # (accountId, eventId) marked by E-13.2
    triggers: set = field(default_factory=set)              # (accountId, group) marked by E-13.3


def floor_to(value, step):
    return (value // step) * step


def lcm(a, b):
    x, y = a, b
    while y:
        x, y = y, x % y
    return a * b // x


# ------------------------------------------------------------------------------------------------ engine

class Engine:
    def __init__(self, group, binary_hash="", protocol_versions=(1,)):
        self.group = group
        self.binary_hash = binary_hash
        self.protocol_versions = set(protocol_versions)
        self.now = 0
        self.last_sequence = 0
        self.protocol_version = 1
        self.venue_mode = "NORMAL"
        self.active_order_limit = 0
        self.client_order_id_window_seconds = 86_400          # D-12
        self.tick_tables = {}
        self.events = {}
        self.instruments = {}
        self.orders = {}                                       # resting orders only
        self.accounts = {}
        self.rule_limits = {}
        self.maker_protection = {}
        self.maker_counters = {}
        self.order_groups = {}
        self.duplicates = {}
        self.commands = set()
        self.next_order = 1
        self.next_trade = 1
        self.results = []
        self.unit = None

    # ---- accessors that record start values for §5

    def account(self, account_id):
        return self.accounts.setdefault(account_id, Account())

    def position(self, account_id, instrument_id):
        return self.account(account_id).positions.get(instrument_id, 0)

    def set_position(self, account_id, instrument_id, value):
        key = (account_id, instrument_id)
        if key not in self.unit.positions_at_start:
            self.unit.positions_at_start[key] = self.position(account_id, instrument_id)
        self.account(account_id).positions[instrument_id] = value

    def touch_sleeve(self, account_id):
        if account_id not in self.unit.sleeves_at_start:
            acc = self.account(account_id)
            self.unit.sleeves_at_start[account_id] = (acc.balance, acc.reserved)

    def add_balance(self, account_id, cents):
        self.touch_sleeve(account_id)
        self.account(account_id).balance += cents

    def level(self, instrument_id, side, tick):
        queue = self.instruments[instrument_id].book[side].get(tick, [])
        return (sum(self.orders[o].remaining for o in queue), len(queue))

    def touch_level(self, instrument_id, side, tick):
        key = (instrument_id, side, tick)
        if key not in self.unit.levels_at_start:
            self.unit.levels_at_start[key] = self.level(instrument_id, side, tick)

    def locked(self, account_id, instrument_id, side):
        acc = self.account(account_id)
        table = acc.yes_locked if side == "SELL" else acc.no_locked
        return table.get(instrument_id, 0)

    def add_locked(self, account_id, instrument_id, side, delta):
        acc = self.account(account_id)
        table = acc.yes_locked if side == "SELL" else acc.no_locked
        table[instrument_id] = table.get(instrument_id, 0) + delta

    # ---- arithmetic (§2)

    def row(self, instrument, tick):
        return self.tick_tables[instrument.tick_table_id][tick]

    def yes_cost(self, instrument, tick, quantity):                      # E-2.4
        r = self.row(instrument, tick)
        n = 100 * quantity * r.numerator
        if n % r.denominator:
            raise FailStop("yes cost is not a whole number of cents")
        return n // r.denominator

    def unit_cost(self, instrument, side, tick, quantity):               # E-2.6
        yes = self.yes_cost(instrument, tick, quantity)
        return yes if side == "BUY" else 100 * quantity - yes

    @staticmethod
    def is_purchase(side, entered):                                      # E-2.7
        return (side == "BUY" and entered == "YES") or (side == "SELL" and entered == "NO")

    def affordable(self, instrument, side, tick, cap):
        """Largest multiple of lotStep whose gross spend at tick does not exceed cap."""
        lot = instrument.lot_step
        per_lot = self.unit_cost(instrument, side, tick, lot)
        return (cap // per_lot) * lot if per_lot else 0

    def new_order_id(self):
        n = self.next_order
        self.next_order += 1
        return (self.group << 48) + n                                    # E-2.8

    def new_trade_id(self):
        n = self.next_trade
        self.next_trade += 1
        return (self.group << 48) + n

    # ---- results (§5)

    def emit_outcome(self, msg, **fields):
        self.unit.outcome.append(dict(msg=msg, **fields))

    def emit_event(self, msg, **fields):
        self.unit.events.append(dict(msg=msg, **fields))

    def begin_unit(self):
        self.unit = Unit()

    def end_unit(self):                                                  # E-5.2
        u = self.unit
        out = list(u.outcome) + list(u.events)
        for (a, i) in sorted(u.positions_at_start):
            if self.position(a, i) != u.positions_at_start[(a, i)]:
                out.append(dict(msg="PositionChanged", accountId=a, instrumentId=i, position=self.position(a, i)))
        for a in sorted(u.sleeves_at_start):
            acc = self.account(a)
            if (acc.balance, acc.reserved) != u.sleeves_at_start[a]:
                out.append(dict(msg="SleeveChanged", accountId=a, availableCents=acc.balance - acc.reserved,
                                reservedCents=acc.reserved))
        for (i, side, t) in sorted(u.levels_at_start, key=lambda k: (k[0], 0 if k[1] == "BUY" else 1, k[2])):
            now = self.level(i, side, t)
            if now != u.levels_at_start[(i, side, t)]:
                out.append(dict(msg="BookLevelChanged", instrumentId=i, side=side, tick=t, quantity=now[0],
                                orderCount=now[1]))
        self.results.extend(out)
        self.unit = None

    def reject_unit(self, msg, **fields):
        """E-5.3: a rejected unit keeps only its outcome."""
        self.unit.outcome = [dict(msg=msg, **fields)]
        self.unit.events = []
        self.unit.positions_at_start = {}
        self.unit.sleeves_at_start = {}
        self.unit.levels_at_start = {}

    # ---- processing an input (§4)

    def apply(self, inp):
        record = inp["record"]
        if record["sequence"] != self.last_sequence + 1 or record["sequencedAt"] < self.now:
            raise FailStop("sequence or time violation (E-4.1)")
        msg = inp["msg"]
        self.last_sequence = record["sequence"]
        self.now = record["sequencedAt"]
        self.results = []
        if record["sequencerGroup"] != self.group:
            if msg in CLIENT_MESSAGES:
                self.begin_unit()
                self.emit_outcome("OrderRejected", accountId=inp.get("accountId", 0),
                                  sessionId=inp.get("sessionId", 0), clientOrderId=inp.get("clientOrderId", ""),
                                  instrumentId=inp.get("instrumentId", 0), reason="MALFORMED", originalOrderId=None)
                self.end_unit()
                return self.finish(inp)
            raise FailStop("command or event for another group (E-4.3)")
        handler = getattr(self, "on_" + msg, None)
        if handler is None:
            raise FailStop(f"unknown message {msg}")
        handler(inp)
        return self.finish(inp)

    def finish(self, inp):
        record = inp["record"]
        for index, result in enumerate(self.results):
            result["result"] = dict(inputSequence=record["sequence"], resultIndex=index,
                                    sequencedAt=record["sequencedAt"], sequencerGroup=self.group)
        self.check_invariants()
        return self.results

    # ---- invariants checked after every input (E-3.3, E-10.10, E-15.2, E-11.2)

    def check_invariants(self):
        for inst in self.instruments.values():
            bids = [t for t, q in inst.book["BUY"].items() if q]
            asks = [t for t, q in inst.book["SELL"].items() if q]
            if bids and asks and max(bids) >= min(asks):
                raise FailStop(f"crossed book on {inst.instrument_id} (E-10.10)")
        for ev in self.events.values():
            if ev.resolved:
                continue
            for s in range(ev.state_count):
                total = sum(self.payout(self.position(a, i), self.instruments[i].state_mask, s)
                            for a in self.accounts for i in ev.instruments)
                if total != ev.collateral:
                    raise FailStop(f"conservation broken on event {ev.event_id} state {s} (E-15.2)")
        for a, acc in self.accounts.items():
            orders = [o for o in self.orders.values() if o.account_id == a]
            if acc.reserved != sum(o.reserved for o in orders):
                raise FailStop(f"reserved mismatch for {a}")
            if acc.balance - acc.reserved < 0:
                raise FailStop(f"negative available cash for {a} (E-3.3)")
            for i in set(acc.positions) | set(acc.yes_locked) | set(acc.no_locked):
                yl = sum(o.lock for o in orders if o.instrument_id == i and o.side == "SELL")
                nl = sum(o.lock for o in orders if o.instrument_id == i and o.side == "BUY")
                if yl != acc.yes_locked.get(i, 0) or nl != acc.no_locked.get(i, 0):
                    raise FailStop(f"lock mismatch for {a} on {i}")
                p = acc.positions.get(i, 0)
                if yl > max(0, p) or nl > max(0, -p):
                    raise FailStop(f"lock exceeds holding for {a} on {i} (E-11.4)")
        for o in self.orders.values():
            if o.reserved != self.compute_reserved(o):
                raise FailStop(f"stale reservation on order {o.order_id}")
            if o.remaining % self.instruments[o.instrument_id].lot_step or o.lock % self.instruments[o.instrument_id].lot_step:
                raise FailStop(f"order {o.order_id} off its lot step")

    @staticmethod
    def payout(position, mask, state):                                   # E-15.1
        in_mask = bool(mask >> state & 1)
        if position > 0 and in_mask:
            return 100 * position
        if position < 0 and not in_mask:
            return 100 * -position
        return 0

    # ============================================================ sequencer events (§8)

    def on_TimeTick(self, inp):                                          # E-8.1
        self.begin_unit()
        due = sorted((o for o in self.orders.values() if o.expires_at is not None and o.expires_at <= self.now),
                     key=lambda o: (o.expires_at, o.order_id))
        for o in due:
            self.remove(o, "EXPIRED")
        self.end_unit()

    def on_EngineStarted(self, inp):
        pass

    def on_SeedSet(self, inp):                                           # E-8.3
        pass

    # ============================================================ commands (§6, §7)

    def command(self, inp, body):
        """E-7.1"""
        self.begin_unit()
        cid = inp["commandId"]
        if cid in self.commands:
            self.emit_outcome("CommandApplied", commandId=cid, outcome="DUPLICATE", rejectReason="NONE")
            self.end_unit()
            return
        self.commands.add(cid)
        try:
            body(inp, cid)
        except Reject as r:
            self.reject_unit("CommandApplied", commandId=cid, outcome="REJECTED", rejectReason=r.reason)
        self.end_unit()

    def applied(self, cid):
        self.emit_outcome("CommandApplied", commandId=cid, outcome="APPLIED", rejectReason="NONE")

    def on_DefineTickTable(self, inp):                                   # E-6.1
        def body(inp, cid):
            rows = inp["ticks"]
            if inp["tickTableId"] in self.tick_tables or not rows:
                raise Reject("INVALID_COMMAND")
            table = []
            for n, r in enumerate(rows):
                if r["tick"] != n or not (0 < r["numerator"] < r["denominator"]):
                    raise Reject("INVALID_COMMAND")
                table.append(TickRow(r["numerator"], r["denominator"], r["complement"]))
            for a, b in zip(table, table[1:]):
                if a.numerator * b.denominator >= b.numerator * a.denominator:
                    raise Reject("INVALID_COMMAND")
            for t, r in enumerate(table):
                if not (0 <= r.complement < len(table)):
                    raise Reject("INVALID_COMMAND")
                c = table[r.complement]
                if r.numerator * c.denominator + c.numerator * r.denominator != r.denominator * c.denominator:
                    raise Reject("INVALID_COMMAND")
                if c.complement != t:
                    raise Reject("INVALID_COMMAND")
            self.tick_tables[inp["tickTableId"]] = table
            self.applied(cid)
        self.command(inp, body)

    def on_DefineEvent(self, inp):                                       # E-6.2
        def body(inp, cid):
            if inp["eventId"] in self.events or not (2 <= inp["stateCount"] <= 64):
                raise Reject("INVALID_COMMAND")
            self.events[inp["eventId"]] = Event(inp["eventId"], inp["stateCount"], inp["scheduledStart"])
            self.applied(cid)
        self.command(inp, body)

    def on_ListInstrument(self, inp):                                    # E-6.3
        def body(inp, cid):
            ev = self.events.get(inp["eventId"])
            table = self.tick_tables.get(inp["tickTableId"])
            mask, cls, legs = inp["stateMask"], inp["instrumentClass"], inp.get("legs", [])
            full = (1 << ev.state_count) - 1 if ev else 0
            if (inp["instrumentId"] in self.instruments or ev is None or ev.resolved or table is None
                    or inp["lotStep"] == 0 or mask == 0 or mask > full or mask == full):
                raise Reject("INVALID_COMMAND")
            if any((100 * inp["lotStep"] * r.numerator) % r.denominator for r in table):
                raise Reject("INVALID_COMMAND")
            if cls == "BINARY" and (ev.state_count != 2 or legs):
                raise Reject("INVALID_COMMAND")
            if cls == "CATEGORICAL_OUTCOME" and (bin(mask).count("1") != 1 or legs):
                raise Reject("INVALID_COMMAND")
            if cls == "SCALAR_BAND" and legs:
                raise Reject("INVALID_COMMAND")
            if cls == "PARLAY" and (ev.state_count != 2 or not legs):
                raise Reject("INVALID_COMMAND")
            inst = Instrument(inp["instrumentId"], inp["eventId"], cls, inp["tickTableId"], inp["lotStep"],
                              inp["allocationPolicy"], mask, inp.get("positionLimit", 0), legs)
            self.instruments[inst.instrument_id] = inst
            ev.instruments = sorted(ev.instruments + [inst.instrument_id])
            self.applied(cid)
            self.emit_event("MarketStateChanged", instrumentId=inst.instrument_id, state="LISTED")
        self.command(inp, body)

    def is_partition(self, ev):                                          # E-6.4
        if len(ev.instruments) < 2:
            return False
        seen = 0
        for i in ev.instruments:
            m = self.instruments[i].state_mask
            if seen & m:
                return False
            seen |= m
        return seen == (1 << ev.state_count) - 1

    def on_SleeveAdjust(self, inp):                                      # E-7.2
        def body(inp, cid):
            a, delta = inp["accountId"], inp["deltaCents"]
            if delta <= 0:
                raise Reject("INVALID_COMMAND")
            if inp["reason"] == "RECLAIM":
                acc = self.account(a)
                if delta > acc.balance - acc.reserved:
                    raise Reject("INSUFFICIENT_SLEEVE")
                delta = -delta
            self.applied(cid)
            self.add_balance(a, delta)
        self.command(inp, body)

    def on_SetRuleLimits(self, inp):                                     # E-7.3
        def body(inp, cid):
            self.rule_limits[inp["accountId"]] = dict(
                maxRestingOrders=inp["maxRestingOrders"], maxNewOrdersPerSecond=inp["maxNewOrdersPerSecond"],
                maxOrderToTradeRatioMilli=inp["maxOrderToTradeRatioMilli"],
                ratioFreeOrdersPerSecond=inp["ratioFreeOrdersPerSecond"])
            self.applied(cid)
        self.command(inp, body)

    def on_SetGroupParameters(self, inp):                                # E-7.4
        def body(inp, cid):
            if inp["clientOrderIdWindowSeconds"] <= 0:
                raise Reject("INVALID_COMMAND")
            self.active_order_limit = inp["activeOrderLimit"]
            self.client_order_id_window_seconds = inp["clientOrderIdWindowSeconds"]
            self.applied(cid)
        self.command(inp, body)

    def on_SetAccountControls(self, inp):                                # E-7.5
        def body(inp, cid):
            acc = self.account(inp["accountId"])
            acc.closed_only = inp["closedOnly"] == "TRUE"
            acc.self_match_prevention = inp["selfMatchPrevention"]
            self.applied(cid)
        self.command(inp, body)

    def on_SetMakerProtection(self, inp):                                # E-7.6
        def body(inp, cid):
            key = (inp["accountId"], inp["eventId"])
            if inp["maxContractsFilledPerWindow"] == 0:
                self.maker_protection.pop(key, None)
            else:
                self.maker_protection[key] = (inp["maxContractsFilledPerWindow"], inp["windowMillis"])
            self.applied(cid)
        self.command(inp, body)

    def on_ActivateProtocol(self, inp):                                  # E-7.7
        def body(inp, cid):
            v = inp["protocolVersion"]
            if v <= self.protocol_version:
                raise Reject("INVALID_COMMAND")
            if v not in self.protocol_versions or inp["requiredBinaryHash"] != self.binary_hash:
                raise FailStop("unsupported protocol activation (E-7.7)")
            self.protocol_version = v
            self.applied(cid)
        self.command(inp, body)

    def on_TransferOut(self, inp):                                       # §19
        self.command(inp, lambda inp, cid: (_ for _ in ()).throw(Reject("INVALID_COMMAND")))

    def on_TransferIn(self, inp):
        self.command(inp, lambda inp, cid: (_ for _ in ()).throw(Reject("INVALID_COMMAND")))

    # ============================================================ market state, phases, modes (§16)

    def on_SetMarketState(self, inp):                                    # E-16.1
        def body(inp, cid):
            inst = self.instruments.get(inp["instrumentId"])
            if inst is None or self.events[inst.event_id].resolved:
                raise Reject("INVALID_COMMAND")
            if (inst.market_state, inp["state"]) not in MARKET_TRANSITIONS:
                raise Reject("INVALID_COMMAND")
            self.applied(cid)
            if inp["state"] == "CLOSED":
                for o in self.resting_where(lambda o: o.instrument_id == inst.instrument_id):
                    self.remove(o, "MARKET_CLOSED")
            inst.market_state = inp["state"]
            self.emit_event("MarketStateChanged", instrumentId=inst.instrument_id, state=inp["state"])
        self.command(inp, body)

    def on_SetEventPhase(self, inp):                                     # E-16.3
        def body(inp, cid):
            ev = self.events.get(inp["eventId"])
            phase = inp["phase"]
            if ev is None or ev.resolved or phase == "SCHEDULED":
                raise Reject("INVALID_COMMAND")
            if phase == "STARTED" and ev.started:
                raise Reject("INVALID_COMMAND")
            if phase == "RESUMED" and not ev.suspended:
                raise Reject("INVALID_COMMAND")
            self.applied(cid)
            if phase == "STARTED":
                ev.started = True
                for o in self.resting_where(lambda o: self.instruments[o.instrument_id].event_id == ev.event_id
                                            and o.at_event_start == "CANCEL"):
                    self.remove(o, "EVENT_START")
            elif phase == "SUSPENDED":
                ev.suspended = True
            elif phase == "RESUMED":
                ev.suspended = False
            elif phase == "ENDED":
                ev.ended = True
        self.command(inp, body)

    def on_SetVenueMode(self, inp):                                      # E-16.4
        def body(inp, cid):
            self.applied(cid)
            if inp["mode"] == "KILL_SWITCH":
                for o in self.resting_where(lambda o: True):
                    self.remove(o, "KILL_SWITCH")
            self.venue_mode = inp["mode"]
        self.command(inp, body)

    # ============================================================ resolution (§15)

    def on_ResolveEvent(self, inp):                                      # E-15.3 .. E-15.6
        def body(inp, cid):
            ev = self.events.get(inp["eventId"])
            if ev is None or ev.resolved:
                raise Reject("INVALID_COMMAND")
            if inp["kind"] == "RESOLVED":
                if inp["winningState"] >= ev.state_count:
                    raise Reject("INVALID_COMMAND")
                weights = {inp["winningState"]: 1}
            else:
                given = inp.get("voidWeights", [])
                states = [w["state"] for w in given]
                if any(s >= ev.state_count for s in states) or len(set(states)) != len(states):
                    raise Reject("INVALID_COMMAND")
                if given and sum(w["weight"] for w in given) == 0:
                    raise Reject("INVALID_COMMAND")
                weights = {w["state"]: w["weight"] for w in given} if given else \
                    {s: 1 for s in range(ev.state_count)}
            self.applied(cid)
            self.settle(ev, inp["kind"], weights)
        self.command(inp, body)

    def on_ExpireParlay(self, inp):                                      # E-15.7
        def body(inp, cid):
            inst = self.instruments.get(inp["instrumentId"])
            if inst is None or inst.instrument_class != "PARLAY" or self.events[inst.event_id].resolved:
                raise Reject("INVALID_COMMAND")
            ev = self.events[inst.event_id]
            losing = next(s for s in range(ev.state_count) if not inst.state_mask >> s & 1)
            self.applied(cid)
            self.settle(ev, "RESOLVED", {losing: 1})
        self.command(inp, body)

    def settle(self, ev, kind, weights):
        for o in self.resting_where(lambda o: self.instruments[o.instrument_id].event_id == ev.event_id):
            self.remove(o, "MARKET_RESOLVED")
        new_state = "SETTLED" if kind == "RESOLVED" else "VOIDED"
        for i in ev.instruments:
            self.instruments[i].market_state = new_state
            self.emit_event("MarketStateChanged", instrumentId=i, state=new_state)
        total_weight = sum(weights.values())
        paid = 0
        for a in sorted(self.accounts):
            for i in ev.instruments:
                p = self.position(a, i)
                if p == 0:
                    continue
                mask = self.instruments[i].state_mask
                cents = sum(w * self.payout(p, mask, s) for s, w in weights.items()) // total_weight
                self.add_balance(a, cents)
                self.set_position(a, i, 0)
                paid += cents
                self.emit_event("SettlementPayout", accountId=a, instrumentId=i, kind=kind, positionClosed=p,
                                cents=cents)
        remainder = ev.collateral - paid
        if remainder < 0:
            raise FailStop("payouts exceed collateral")
        if remainder:
            self.add_balance(0, remainder)
            self.emit_event("SettlementPayout", accountId=0, instrumentId=0, kind=kind, positionClosed=0,
                            cents=remainder)
        ev.collateral = 0
        ev.resolved = True

    # ============================================================ order book mechanics

    def resting_where(self, predicate):
        return sorted((o for o in self.orders.values() if predicate(o)), key=lambda o: o.order_id)

    def best(self, inst, side):
        ticks = [t for t, q in inst.book[side].items() if q]
        if not ticks:
            return None
        return max(ticks) if side == "BUY" else min(ticks)

    def crossing_ticks(self, inst, side, limit):
        """Opposite ticks an order of `side` at `limit` crosses, best first (E-10.1)."""
        opposite = "SELL" if side == "BUY" else "BUY"
        ticks = [t for t, q in inst.book[opposite].items() if q]
        if side == "BUY":
            return sorted(t for t in ticks if t <= limit)
        return sorted((t for t in ticks if t >= limit), reverse=True)

    def compute_reserved(self, o):                                       # E-11.2
        inst = self.instruments[o.instrument_id]
        r = self.unit_cost(inst, o.side, o.tick, o.remaining - o.lock)
        if o.spend_cap_remaining is not None:
            r = min(r, o.spend_cap_remaining)
        return r

    def refresh_reserved(self, o):
        new = self.compute_reserved(o)
        if new != o.reserved:
            self.touch_sleeve(o.account_id)
            self.account(o.account_id).reserved += new - o.reserved
            o.reserved = new

    def rest(self, o):
        inst = self.instruments[o.instrument_id]
        self.touch_level(inst.instrument_id, o.side, o.tick)
        inst.book[o.side].setdefault(o.tick, []).append(o.order_id)
        o.resting = True
        self.orders[o.order_id] = o

    def unlink(self, o):
        inst = self.instruments[o.instrument_id]
        self.touch_level(inst.instrument_id, o.side, o.tick)
        inst.book[o.side][o.tick].remove(o.order_id)
        o.resting = False
        del self.orders[o.order_id]

    def release(self, o):
        """E-11.5 and E-12.1: release lock and reservation."""
        if o.lock:
            self.add_locked(o.account_id, o.instrument_id, o.side, -o.lock)
            o.lock = 0
        if o.reserved:
            self.touch_sleeve(o.account_id)
            self.account(o.account_id).reserved -= o.reserved
            o.reserved = 0

    def remove(self, o, reason):                                         # E-12.1
        if o.resting:
            self.unlink(o)
        self.release(o)
        self.emit_event("OrderCancelled", orderId=o.order_id, accountId=o.account_id,
                        instrumentId=o.instrument_id, cancelledQuantity=o.remaining, reason=reason)

    # ============================================================ new orders (§9)

    def rule_limits_for(self, account_id):
        return self.rule_limits.get(account_id) or self.rule_limits.get(0) or dict(
            maxRestingOrders=0, maxNewOrdersPerSecond=0, maxOrderToTradeRatioMilli=0, ratioFreeOrdersPerSecond=0)

    def roll_second(self, acc):
        s = self.now // NS_PER_SECOND
        if acc.second != s:
            acc.second, acc.new_orders_in_second, acc.fills_in_second = s, 0, 0

    def duplicate_of(self, account_id, coid):                            # E-9.2
        entry = self.duplicates.get((account_id, coid))
        if entry and self.now < entry[0]:
            return entry
        return None

    def duplicate_expiry(self):
        """E-9.2: an entry recorded now expires under the window in force now (E-7.4)."""
        return self.now + self.client_order_id_window_seconds * NS_PER_SECOND

    def unlocked_holding(self, account_id, inst, side):                  # E-9.5
        p = self.position(account_id, inst.instrument_id)
        held = max(0, p) if side == "SELL" else max(0, -p)
        return held - self.locked(account_id, inst.instrument_id, side)

    def resting_same_side(self, account_id, instrument_id, side):
        return sum(o.remaining for o in self.orders.values()
                   if o.account_id == account_id and o.instrument_id == instrument_id and o.side == side)

    def position_limit_breached(self, account_id, inst, side, quantity, exclude=None):   # E-9.7
        if not inst.position_limit:
            return False
        resting = sum(o.remaining for o in self.orders.values()
                      if o.account_id == account_id and o.instrument_id == inst.instrument_id
                      and o.side == side and o is not exclude)
        p = self.position(account_id, inst.instrument_id)
        signed = p if side == "BUY" else -p
        return signed + resting + quantity > inst.position_limit

    def on_NewOrder(self, inp):
        self.begin_unit()
        self.new_order(inp, inp["sessionId"], inp["accountId"])
        self.finish_order_unit()

    def new_order(self, e, session_id, account_id):
        coid = e.get("clientOrderId", "")
        instrument_id = e.get("instrumentId", 0)

        def rejected(reason, original=None):
            self.reject_unit("OrderRejected", accountId=account_id, sessionId=session_id, clientOrderId=coid,
                             instrumentId=instrument_id, reason=reason, originalOrderId=original)

        tif, cap = e["timeInForce"], e.get("spendCapCents")
        post_only = "POST_ONLY" in e.get("flags", [])
        # check 1
        if (account_id == 0 or not coid or e["quantity"] <= 0
                or (tif == "GTD") != (e.get("expiresAt") is not None)
                or (post_only and tif in ("IOC", "FOK"))
                or (cap is not None and (cap <= 0 or not self.is_purchase(e["side"], e["enteredContract"])))):
            return rejected("MALFORMED")
        # check 2
        dup = self.duplicate_of(account_id, coid)
        if dup:
            return rejected("DUPLICATE_CLIENT_ORDER_ID", dup[1])
        self.duplicates[(account_id, coid)] = (self.duplicate_expiry(), None)
        try:
            order = self.check_and_build(e, session_id, account_id)
        except Reject as r:
            return rejected(r.reason)
        self.accept_and_match(order)

    def check_and_build(self, e, session_id, account_id):                # E-9.3 checks 3..18
        acc = self.account(account_id)
        if self.venue_mode != "NORMAL":
            raise Reject("VENUE_MODE")
        inst = self.instruments.get(e["instrumentId"])
        if inst is None:
            raise Reject("UNKNOWN_INSTRUMENT")
        ev = self.events[inst.event_id]
        if inst.market_state != "OPEN" or ev.suspended:
            raise Reject("MARKET_NOT_OPEN")
        table = self.tick_tables[inst.tick_table_id]
        if not (0 <= e["tick"] < len(table)):
            raise Reject("TICK_NOT_ON_LADDER")
        quantity = e["quantity"]
        if quantity % inst.lot_step:
            raise Reject("QUANTITY_NOT_ON_LOT_STEP")
        if e["timeInForce"] == "GTD" and e["expiresAt"] <= self.now:
            raise Reject("EXPIRY_IN_PAST")
        group = e.get("clientOrderGroup")
        if group is not None and self.order_groups.get((account_id, group), {}).get("triggered"):
            raise Reject("ORDER_GROUP_TRIGGERED")
        # check 10 (E-9.4)
        limits = self.rule_limits_for(account_id)
        self.roll_second(acc)
        n, f = acc.new_orders_in_second, acc.fills_in_second
        if limits["maxNewOrdersPerSecond"] and n >= limits["maxNewOrdersPerSecond"]:
            raise Reject("RULE_LIMIT")
        ratio, free = limits["maxOrderToTradeRatioMilli"], limits["ratioFreeOrdersPerSecond"]
        if ratio and n >= free and (n + 1 - free) * 1000 > ratio * f:
            raise Reject("RULE_LIMIT")
        acc.new_orders_in_second += 1
        side = e["side"]
        # check 11 (E-9.5)
        flagged = "REDUCE_ONLY" in e.get("flags", [])
        reduce_only = flagged or acc.closed_only
        if reduce_only:
            quantity = floor_to(min(quantity, self.unlocked_holding(account_id, inst, side)), inst.lot_step)
            if quantity <= 0:
                raise Reject("REDUCE_ONLY_NO_POSITION" if flagged else "CLOSED_ONLY_MODE")
        # check 12 (E-9.6)
        limit = e["tick"]
        n_ticks = e.get("protectionTicks", 0)
        opposite_best = self.best(inst, "SELL" if side == "BUY" else "BUY")
        if n_ticks and opposite_best is not None:
            limit = min(limit, opposite_best + n_ticks) if side == "BUY" else max(limit, opposite_best - n_ticks)
        cap = e.get("spendCapCents")
        if cap is not None and cap < self.unit_cost(inst, side, limit, inst.lot_step):
            raise Reject("SPEND_CAP_BELOW_ONE_CONTRACT")
        # check 13
        if "POST_ONLY" in e.get("flags", []) and opposite_best is not None and \
                (opposite_best <= limit if side == "BUY" else opposite_best >= limit):
            raise Reject("POST_ONLY_WOULD_TAKE")
        order = Order(0, account_id, session_id, e["clientOrderId"], inst.instrument_id, side,
                      e["enteredContract"], limit, quantity, e["timeInForce"], e.get("expiresAt"),
                      e["atEventStart"], "POST_ONLY" in e.get("flags", []), reduce_only, cap, group,
                      self.last_sequence)
        order.lock = floor_to(min(quantity, self.unlocked_holding(account_id, inst, side)), inst.lot_step)
        may_rest = e["timeInForce"] in ("GTC", "GTD")
        # The dry run is evaluated only where its answer can change the outcome; results are identical.
        needs_dry_run = e["timeInForce"] == "FOK" or (
            may_rest and (limits["maxRestingOrders"] or self.active_order_limit))
        fillable = self.dry_run(order) if needs_dry_run else None
        # check 14
        if e["timeInForce"] == "FOK" and fillable < quantity:
            raise Reject("FOK_NOT_FILLABLE")
        # check 15
        if self.position_limit_breached(account_id, inst, side, quantity):
            raise Reject("POSITION_LIMIT")
        # checks 16, 17
        if may_rest and needs_dry_run and fillable < quantity:
            resting_account = sum(1 for o in self.orders.values() if o.account_id == account_id)
            if limits["maxRestingOrders"] and resting_account >= limits["maxRestingOrders"]:
                raise Reject("RULE_LIMIT")
            if self.active_order_limit and len(self.orders) >= self.active_order_limit:
                raise Reject("ORDER_CAPACITY")
        # check 18
        if self.compute_reserved(order) > acc.balance - acc.reserved:
            raise Reject("INSUFFICIENT_SLEEVE")
        return order

    def accept_and_match(self, o):                                       # E-9.8, E-9.9
        inst = self.instruments[o.instrument_id]
        o.order_id = self.new_order_id()
        self.add_locked(o.account_id, o.instrument_id, o.side, o.lock)
        self.refresh_reserved(o)
        self.duplicates[(o.account_id, o.client_order_id)] = (self.duplicates[(o.account_id, o.client_order_id)][0],
                                                                o.order_id)
        self.emit_outcome("OrderAccepted", orderId=o.order_id, accountId=o.account_id, sessionId=o.session_id,
                          clientOrderId=o.client_order_id, instrumentId=o.instrument_id, side=o.side, tick=o.tick,
                          quantity=o.remaining, reservedCents=o.reserved)
        self.match_and_place(o, inst)

    def match_and_place(self, o, inst):
        stopped = self.walk(o, inst)
        if o.remaining == 0:
            self.release(o)
            return
        if stopped == "SELF_MATCH":
            return self.remove(o, "SELF_MATCH_PREVENTED")
        if o.time_in_force in ("IOC", "FOK"):
            if o.time_in_force == "FOK":
                raise FailStop("FOK left a remainder after a passing dry run")
            return self.remove(o, "IOC_REMAINDER")
        if o.spend_cap_remaining is not None:
            afford = min(o.remaining, self.affordable(inst, o.side, o.tick, o.spend_cap_remaining))
            if afford == 0:
                return self.remove(o, "SPEND_CAP_REACHED")
            if afford < o.remaining:
                o.remaining = afford
                if o.lock > afford:
                    self.add_locked(o.account_id, o.instrument_id, o.side, afford - o.lock)
                    o.lock = afford
                self.emit_event("OrderReduced", orderId=o.order_id, accountId=o.account_id,
                                instrumentId=o.instrument_id, remainingQuantity=o.remaining)
        self.refresh_reserved(o)
        self.rest(o)

    def finish_order_unit(self):
        """E-13.4 and E-14.3, then the end of the unit."""
        u = self.unit
        for (a, event_id) in sorted(u.pulls):
            for o in self.resting_where(lambda o: o.account_id == a
                                        and self.instruments[o.instrument_id].event_id == event_id):
                self.remove(o, "MAKER_AUTO_PULL")
            self.maker_counters[(a, event_id)] = [0, None]
        for (a, g) in sorted(u.triggers):
            grp = self.order_groups[(a, g)]
            grp["triggered"] = True
            grp["count"], grp["start"] = 0, None
            self.emit_event("OrderGroupUpdated", accountId=a, clientOrderGroup=g,
                            maxContractsFilledPerWindow=grp["threshold"], windowMillis=grp["window"],
                            triggered="TRUE")
            for o in self.resting_where(lambda o: o.account_id == a and o.client_order_group == g):
                self.remove(o, "ORDER_GROUP_TRIGGERED")
        self.set_burns()
        self.end_unit()

    # ============================================================ matching (§10)

    def dry_run(self, order):                                            # E-10.9
        """The walk of E-10.3 to E-10.8, lock restoration included, on a copy of the state that is then
        discarded. Protection triggers cannot affect it because they act at the end of the unit."""
        trial = copy.deepcopy(self)
        trial.unit = Unit()
        o = copy.deepcopy(order)
        o.order_id = -1
        trial.add_locked(o.account_id, o.instrument_id, o.side, o.lock)
        trial.walk(o, trial.instruments[o.instrument_id])
        return order.remaining - o.remaining

    def walk(self, o, inst):                                             # E-10.3 .. E-10.8
        opposite = "SELL" if o.side == "BUY" else "BUY"
        mode = self.account(o.account_id).self_match_prevention
        while o.remaining > 0:
            ticks = self.crossing_ticks(inst, o.side, o.tick)
            if not ticks:
                return None
            tick = ticks[0]
            q = o.remaining
            if o.spend_cap_remaining is not None:
                q = min(q, self.affordable(inst, o.side, tick, o.spend_cap_remaining))
            q = floor_to(q, inst.lot_step)
            if q == 0:
                return "CAP"
            queue = inst.book[opposite][tick]
            if inst.allocation_policy == "PRICE_TIME":
                left = q
                for rid in list(queue):
                    if left == 0:
                        break
                    r = self.orders.get(rid)
                    if r is None:                                        # removed by lock restoration
                        continue
                    if r.account_id == o.account_id:                     # E-13.1
                        if mode == "CANCEL_RESTING":
                            self.remove(r, "SELF_MATCH_PREVENTED")
                            continue
                        if mode == "CANCEL_BOTH":
                            self.remove(r, "SELF_MATCH_PREVENTED")
                        return "SELF_MATCH"
                    f = min(left, r.remaining)
                    self.fill(o, r, inst, tick, f)
                    left -= f
            else:
                own = sorted((self.orders[x] for x in queue if self.orders[x].account_id == o.account_id),
                             key=lambda r: r.order_id)
                if own:
                    if mode == "CANCEL_INCOMING":
                        return "SELF_MATCH"
                    for r in own:
                        self.remove(r, "SELF_MATCH_PREVENTED")
                    if mode == "CANCEL_BOTH":
                        return "SELF_MATCH"
                makers = [self.orders[x] for x in queue]
                lot = inst.lot_step
                lots = [r.remaining // lot for r in makers]
                total, want = sum(lots), q // lot
                if total <= want:
                    alloc = lots
                else:
                    alloc = [want * m // total for m in lots]
                    spare = want - sum(alloc)
                    order = sorted(range(len(makers)), key=lambda k: (-(want * lots[k] % total), makers[k].order_id))
                    for k in order[:spare]:
                        alloc[k] += 1
                for r, a in zip(makers, alloc):
                    if a and r.resting and r.remaining:                  # lock restoration may have shrunk it
                        self.fill(o, r, inst, tick, min(a * lot, r.remaining))
        return None

    def fill(self, taker, maker, inst, tick, f):                         # E-10.6, E-10.7
        ev = self.events[inst.event_id]
        buyer, seller = (taker, maker) if taker.side == "BUY" else (maker, taker)
        b, s = buyer.account_id, seller.account_id
        pb, ps = self.position(b, inst.instrument_id), self.position(s, inst.instrument_id)
        yes = self.yes_cost(inst, tick, f)
        b_burn = min(f, max(0, -pb))
        s_reduce = min(f, max(0, ps))
        burned = min(b_burn, s_reduce)
        minted = f - max(b_burn, s_reduce)
        buyer_cash = -yes + 100 * b_burn
        seller_cash = yes - 100 * (f - s_reduce)
        self.touch_level(inst.instrument_id, maker.side, maker.tick)
        self.set_position(b, inst.instrument_id, pb + f)
        self.set_position(s, inst.instrument_id, ps - f)
        self.add_balance(b, buyer_cash)
        self.add_balance(s, seller_cash)
        ev.collateral += 100 * (minted - burned)
        for x in (taker, maker):                                         # E-11.3
            used = min(f, x.lock)
            if used:
                x.lock -= used
                self.add_locked(x.account_id, x.instrument_id, x.side, -used)
            x.remaining -= f
            if x.spend_cap_remaining is not None:
                x.spend_cap_remaining -= self.unit_cost(inst, x.side, tick, f)
        self.emit_event("Fill", tradeId=self.new_trade_id(), instrumentId=inst.instrument_id, tick=tick, quantity=f,
                        makerOrderId=maker.order_id, makerAccountId=maker.account_id,
                        makerRemaining=maker.remaining, takerOrderId=taker.order_id,
                        takerAccountId=taker.account_id, takerRemaining=taker.remaining, takerSide=taker.side,
                        mintedPairs=minted, burnedPairs=burned, buyerCashCents=buyer_cash,
                        sellerCashCents=seller_cash, makerFeeCents=0, takerFeeCents=0)
        for a in sorted({b, s}):                                         # E-11.4
            self.restore_locks(a, inst)
        if maker.remaining == 0 and maker.resting:
            self.unlink(maker)
            self.release(maker)
        for a in (b, s):
            acc = self.account(a)
            self.roll_second(acc)
            acc.fills_in_second += 1
        self.count_maker(maker, inst, f)                                 # E-13.2
        for x in (taker, maker):                                         # E-13.3
            self.count_group(x, f)
        for x in (taker, maker):
            if x.remaining or x.resting:
                self.refresh_reserved(x)
            elif x.reserved:
                self.release(x)
        self.unit.filled.add((inst.event_id, b))
        self.unit.filled.add((inst.event_id, s))

    def restore_locks(self, account_id, inst):                           # E-11.4
        p = self.position(account_id, inst.instrument_id)
        for side, held in (("SELL", max(0, p)), ("BUY", max(0, -p))):
            excess = self.locked(account_id, inst.instrument_id, side) - held
            if excess <= 0:
                continue
            victims = sorted((o for o in self.orders.values()
                              if o.account_id == account_id and o.instrument_id == inst.instrument_id
                              and o.side == side and o.lock > 0),
                             key=lambda o: (o.accepted_at, o.order_id), reverse=True)
            for v in victims:
                if excess <= 0:
                    break
                take = min(v.lock, excess)
                v.lock -= take
                self.add_locked(account_id, inst.instrument_id, side, -take)
                excess -= take
                if v.reduce_only:
                    self.touch_level(inst.instrument_id, v.side, v.tick)
                    v.remaining -= take
                    if v.remaining == 0:
                        self.remove(v, "POSITION_CLOSED")
                        continue
                    self.emit_event("OrderReduced", orderId=v.order_id, accountId=v.account_id,
                                    instrumentId=v.instrument_id, remainingQuantity=v.remaining)
                self.refresh_reserved(v)
            if excess > 0:
                raise FailStop("lock restoration could not restore holdings (E-11.4)")

    def window_count(self, counter, window_ms, quantity):
        if counter[1] is None or self.now - counter[1] >= window_ms * NS_PER_MILLI:
            counter[0], counter[1] = 0, self.now
        counter[0] += quantity
        return counter[0]

    def count_maker(self, maker, inst, quantity):
        setting = self.maker_protection.get((maker.account_id, inst.event_id)) or \
            self.maker_protection.get((maker.account_id, 0))
        if not setting:
            return
        key = (maker.account_id, inst.event_id)
        counter = self.maker_counters.setdefault(key, [0, None])
        if self.window_count(counter, setting[1], quantity) >= setting[0]:
            self.unit.pulls.add(key)

    def count_group(self, o, quantity):
        if o.client_order_group is None:
            return
        grp = self.order_groups.get((o.account_id, o.client_order_group))
        if not grp or not grp["threshold"]:
            return
        counter = [grp["count"], grp["start"]]
        total = self.window_count(counter, grp["window"], quantity)
        grp["count"], grp["start"] = counter
        if total >= grp["threshold"]:
            self.unit.triggers.add((o.account_id, o.client_order_group))

    # ============================================================ cancels, amendments, batches (§12)

    def find_order(self, account_id, instrument_id, order_id, coid):
        if order_id is not None:
            o = self.orders.get(order_id)
        else:
            o = next((x for x in self.orders.values()
                      if x.account_id == account_id and x.client_order_id == coid and coid), None)
        if o is None or o.account_id != account_id or o.instrument_id != instrument_id:
            return None
        return o

    def on_CancelOrder(self, inp):
        self.begin_unit()
        self.cancel(inp, inp["sessionId"], inp["accountId"])
        self.end_unit()

    def cancel(self, e, session_id, account_id):                         # E-12.2
        o = self.find_order(account_id, e["instrumentId"], e.get("orderId"), e.get("clientOrderId", ""))
        if o is None:
            return self.reject_unit("OrderRejected", accountId=account_id, sessionId=session_id,
                                    clientOrderId=e.get("clientOrderId", ""), instrumentId=e["instrumentId"],
                                    reason="UNKNOWN_ORDER", originalOrderId=None)
        self.unlink(o)
        self.release(o)
        self.emit_outcome("OrderCancelled", orderId=o.order_id, accountId=o.account_id,
                          instrumentId=o.instrument_id, cancelledQuantity=o.remaining, reason="CLIENT_REQUEST")

    def on_AmendOrder(self, inp):                                        # E-12.3 .. E-12.5
        self.begin_unit()
        a = inp["accountId"]

        def rejected(reason):
            self.reject_unit("OrderRejected", accountId=a, sessionId=inp["sessionId"], clientOrderId="",
                             instrumentId=inp["instrumentId"], reason=reason, originalOrderId=None)
            self.end_unit()

        o = self.find_order(a, inp["instrumentId"], inp["orderId"], "")
        if o is None:
            return rejected("UNKNOWN_ORDER")
        new_tick, new_qty, aes = inp.get("newTick"), inp.get("newQuantity"), inp["atEventStart"]
        if (new_tick is None and new_qty is None and aes == "UNCHANGED") or new_qty == 0:
            return rejected("MALFORMED")
        inst = self.instruments[o.instrument_id]
        if new_tick is not None and not (0 <= new_tick < len(self.tick_tables[inst.tick_table_id])):
            return rejected("TICK_NOT_ON_LADDER")
        if new_qty is not None and new_qty % inst.lot_step:
            return rejected("QUANTITY_NOT_ON_LOT_STEP")
        tick = o.tick if new_tick is None else new_tick
        qty = o.remaining if new_qty is None else new_qty
        if tick == o.tick and qty <= o.remaining:                        # E-12.4
            self.touch_level(inst.instrument_id, o.side, o.tick)
            o.remaining = qty
            if aes != "UNCHANGED":
                o.at_event_start = aes
            if o.lock > qty:
                self.add_locked(o.account_id, o.instrument_id, o.side, qty - o.lock)
                o.lock = qty
            self.refresh_reserved(o)
            self.emit_outcome("OrderAmended", orderId=o.order_id, accountId=a, instrumentId=o.instrument_id,
                              tick=o.tick, quantity=o.remaining, atEventStart=o.at_event_start, priorityKept="TRUE")
            return self.end_unit()
        # E-12.5
        if self.venue_mode not in ("NORMAL", "NEW_ORDER_REJECT"):
            return rejected("VENUE_MODE")
        if inst.market_state != "OPEN" or self.events[inst.event_id].suspended:
            return rejected("MARKET_NOT_OPEN")
        if o.reduce_only and qty > o.lock:
            return rejected("REDUCE_ONLY_NO_POSITION")
        opposite_best = self.best(inst, "SELL" if o.side == "BUY" else "BUY")
        if o.post_only and opposite_best is not None and \
                (opposite_best <= tick if o.side == "BUY" else opposite_best >= tick):
            return rejected("POST_ONLY_WOULD_TAKE")
        if self.position_limit_breached(a, inst, o.side, qty, exclude=o):
            return rejected("POSITION_LIMIT")
        new_lock = min(o.lock, qty)
        new_reserved = self.unit_cost(inst, o.side, tick, qty - new_lock)
        if o.spend_cap_remaining is not None:
            new_reserved = min(new_reserved, o.spend_cap_remaining)
        acc = self.account(a)
        if new_reserved - o.reserved > acc.balance - acc.reserved:
            return rejected("INSUFFICIENT_SLEEVE")
        self.unlink(o)
        if o.lock > new_lock:
            self.add_locked(a, o.instrument_id, o.side, new_lock - o.lock)
            o.lock = new_lock
        o.tick, o.remaining = tick, qty
        if aes != "UNCHANGED":
            o.at_event_start = aes
        o.accepted_at = self.last_sequence
        self.refresh_reserved(o)
        self.emit_outcome("OrderAmended", orderId=o.order_id, accountId=a, instrumentId=o.instrument_id,
                          tick=o.tick, quantity=o.remaining, atEventStart=o.at_event_start, priorityKept="FALSE")
        self.match_and_place(o, inst)
        self.finish_order_unit()

    def on_MassCancel(self, inp):                                        # E-12.6
        self.begin_unit()
        a, scope, ref = inp["accountId"], inp["scope"], inp["scopeRef"]
        in_scope = {
            "SESSION": lambda o: o.session_id == inp["sessionId"],
            "ACCOUNT": lambda o: True,
            "INSTRUMENT": lambda o: o.instrument_id == ref,
            "EVENT": lambda o: self.instruments[o.instrument_id].event_id == ref,
            "CLIENT_ORDER_GROUP": lambda o: o.client_order_group == ref,
        }[scope]
        for o in self.resting_where(lambda o: o.account_id == a and in_scope(o)):
            self.remove(o, CANCEL_CAUSE_REASON[inp["cause"]])
        self.end_unit()

    def on_BatchOrders(self, inp):                                       # E-12.8
        for entry in inp.get("entries", []):
            self.begin_unit()
            if entry["entryType"] == "NEW":
                self.new_order(entry, inp["sessionId"], inp["accountId"])
                self.finish_order_unit()
            else:
                self.cancel(entry, inp["sessionId"], inp["accountId"])
                self.end_unit()

    def on_SetOrderGroup(self, inp):                                     # E-12.7
        self.begin_unit()
        key = (inp["accountId"], inp["clientOrderGroup"])
        grp = self.order_groups.setdefault(key, dict(threshold=0, window=0, count=0, start=None, triggered=False))
        grp["threshold"], grp["window"] = inp["maxContractsFilledPerWindow"], inp["windowMillis"]
        self.emit_outcome("OrderGroupUpdated", accountId=key[0], clientOrderGroup=key[1],
                          maxContractsFilledPerWindow=grp["threshold"], windowMillis=grp["window"],
                          triggered="TRUE" if grp["triggered"] else "FALSE")
        self.end_unit()

    def on_ResetOrderGroup(self, inp):
        self.begin_unit()
        key = (inp["accountId"], inp["clientOrderGroup"])
        grp = self.order_groups.setdefault(key, dict(threshold=0, window=0, count=0, start=None, triggered=False))
        grp["triggered"], grp["count"], grp["start"] = False, 0, None
        self.emit_outcome("OrderGroupUpdated", accountId=key[0], clientOrderGroup=key[1],
                          maxContractsFilledPerWindow=grp["threshold"], windowMillis=grp["window"],
                          triggered="FALSE")
        self.end_unit()

    # ============================================================ baskets and burns (§14)

    def on_BasketMintRedeem(self, inp):                                  # E-14.1, E-14.2
        self.begin_unit()
        a, coid, q = inp["accountId"], inp.get("clientOrderId", ""), inp["quantity"]

        def rejected(reason, original=None):
            self.reject_unit("OrderRejected", accountId=a, sessionId=inp["sessionId"], clientOrderId=coid,
                             instrumentId=0, reason=reason, originalOrderId=original)
            self.end_unit()

        if a == 0 or not coid or q <= 0:
            return rejected("MALFORMED")
        dup = self.duplicate_of(a, coid)
        if dup:
            return rejected("DUPLICATE_CLIENT_ORDER_ID", dup[1])
        self.duplicates[(a, coid)] = (self.duplicate_expiry(), None)
        ev = self.events.get(inp["eventId"])
        if ev is None or ev.resolved or not self.is_partition(ev):
            return rejected("UNKNOWN_INSTRUMENT")
        insts = [self.instruments[i] for i in ev.instruments]
        if any(i.market_state != "OPEN" for i in insts) or ev.suspended:
            return rejected("MARKET_NOT_OPEN")
        if any(q % i.lot_step for i in insts):
            return rejected("QUANTITY_NOT_ON_LOT_STEP")
        acc = self.account(a)
        if inp["direction"] == "MINT":
            if self.venue_mode != "NORMAL":
                return rejected("VENUE_MODE")
            if any(self.position_limit_breached(a, i, "BUY", q) for i in insts):
                return rejected("POSITION_LIMIT")
            if 100 * q > acc.balance - acc.reserved:
                return rejected("INSUFFICIENT_SLEEVE")
            sign = 1
        else:
            if any(self.unlocked_holding(a, i, "SELL") < q for i in insts):
                return rejected("INSUFFICIENT_POSITION")
            sign = -1
        # E-14.2: new YES closes NO the account holds, so only (q - K) is newly funded.
        closed = sum(min(q, max(0, -self.position(a, i.instrument_id))) for i in insts) if sign > 0 else 0
        cash = -100 * (q - closed) if sign > 0 else 100 * q
        for i in insts:
            self.set_position(a, i.instrument_id, self.position(a, i.instrument_id) + sign * q)
        self.add_balance(a, cash)
        ev.collateral -= cash
        self.emit_outcome("BasketApplied", accountId=a, sessionId=inp["sessionId"], clientOrderId=coid,
                          eventId=ev.event_id, quantity=q, direction=inp["direction"], cashCents=cash)
        if sign > 0:
            for i in insts:
                self.restore_locks(a, i)
        self.end_unit()

    def set_burns(self):                                                 # E-14.3
        for (event_id, a) in sorted(self.unit.filled):
            ev = self.events[event_id]
            if ev.resolved or not self.is_partition(ev):
                continue
            insts = [self.instruments[i] for i in ev.instruments]
            step = 1
            for i in insts:
                step = lcm(step, i.lot_step)
            n = len(insts)
            k = floor_to(min(self.unlocked_holding(a, i, "SELL") for i in insts), step)
            if k > 0:
                for i in insts:
                    self.set_position(a, i.instrument_id, self.position(a, i.instrument_id) - k)
                self.add_balance(a, 100 * k)
                ev.collateral -= 100 * k
                self.emit_event("CollateralReleased", accountId=a, eventId=event_id, cents=100 * k,
                                kind="YES_SET_BURN")
            k = floor_to(min(self.unlocked_holding(a, i, "BUY") for i in insts), step)
            if k > 0:
                for i in insts:
                    self.set_position(a, i.instrument_id, self.position(a, i.instrument_id) + k)
                self.add_balance(a, 100 * (n - 1) * k)
                ev.collateral -= 100 * (n - 1) * k
                self.emit_event("CollateralReleased", accountId=a, eventId=event_id, cents=100 * (n - 1) * k,
                                kind="NO_SET_BURN")
