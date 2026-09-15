#!/usr/bin/env python3
"""Seeded random input sequences against the reference model.

    python3 spec/engine/reference/fuzz.py [--seeds N] [--steps N] [--first SEED]

Besides the invariants the model checks after every input (conservation per event and state, uncrossed
books, non-negative available cash, lock and reservation consistency, lot steps), this checks:

  money       the sum of balances and unresolved event collateral changes only by applied SleeveAdjust
  readers     positions, sleeves and book levels rebuilt only from emitted results equal the model's state
  replay      the same seed produces the same results, byte for byte

It ends with a coverage line naming every result kind, reject reason and cancel reason never produced, so a
passing run that exercised little is visible as such.
"""
import argparse
import random
import sys
from collections import Counter

from model import FailStop
from run_scenarios import Driver, expand_setup

ACCOUNTS = [1, 2, 3, 4, 5, 6]
FUNDING = {1: 20_000, 2: 20_000, 3: 20_000, 4: 20_000, 5: 3_000, 6: 800}
BINARY_PT, BINARY_PR, PARLAY = 1000, 1010, 3000
CATEGORICAL = [2001, 2002, 2003]
OVERLAP = [4001, 4002]
INSTRUMENTS = [BINARY_PT, BINARY_PR, PARLAY] + CATEGORICAL + OVERLAP
EVENT_OF = {BINARY_PT: 100, BINARY_PR: 101, PARLAY: 300, 2001: 200, 2002: 200, 2003: 200, 4001: 400, 4002: 400}
LOT = {BINARY_PT: 1, BINARY_PR: 2, PARLAY: 1, 2001: 1, 2002: 1, 2003: 1, 4001: 5, 4002: 5}
STATES = {100: 2, 101: 2, 200: 3, 300: 2, 400: 4}

ALL_RESULTS = {"OrderAccepted", "OrderRejected", "Fill", "OrderCancelled", "OrderAmended", "OrderReduced",
               "PositionChanged", "SleeveChanged", "CollateralReleased", "BookLevelChanged", "MarketStateChanged",
               "SettlementPayout", "OrderGroupUpdated", "CommandApplied"}
ALL_REJECTS = {"MALFORMED", "UNKNOWN_INSTRUMENT", "UNKNOWN_ORDER", "MARKET_NOT_OPEN", "VENUE_MODE",
               "TICK_NOT_ON_LADDER", "QUANTITY_NOT_ON_LOT_STEP", "EXPIRY_IN_PAST", "INSUFFICIENT_SLEEVE",
               "POST_ONLY_WOULD_TAKE", "FOK_NOT_FILLABLE", "REDUCE_ONLY_NO_POSITION", "CLOSED_ONLY_MODE",
               "SPEND_CAP_BELOW_ONE_CONTRACT", "DUPLICATE_CLIENT_ORDER_ID", "RULE_LIMIT", "POSITION_LIMIT",
               "ORDER_CAPACITY", "ORDER_GROUP_TRIGGERED"}
ALL_CANCELS = {"CLIENT_REQUEST", "MASS_CANCEL", "SESSION_DISCONNECT", "HEARTBEAT_MISSED", "EVENT_START", "EXPIRED",
               "IOC_REMAINDER", "SELF_MATCH_PREVENTED", "MAKER_AUTO_PULL", "KILL_SWITCH", "MARKET_RESOLVED",
               "POSITION_CLOSED", "MARKET_CLOSED", "ORDER_GROUP_TRIGGERED", "SPEND_CAP_REACHED"}
FILL_KINDS = {"mint", "burn", "transfer", "mixed"}


def setup_inputs():
    items = [
        {"op": "centTable", "tickTableId": 1},
        {"op": "binary", "eventId": 100, "instrumentId": BINARY_PT, "positionLimit": 15},
        {"op": "binary", "eventId": 101, "instrumentId": BINARY_PR, "allocationPolicy": "PRO_RATA", "lotStep": 2},
        {"op": "parlay", "eventId": 300, "instrumentId": PARLAY,
         "legs": [{"legEventId": 100, "legInstrumentId": BINARY_PT, "legOutcome": "YES"}]},
        {"op": "categorical", "eventId": 200, "instrumentIds": CATEGORICAL},
        {"msg": "DefineEvent", "eventId": 400, "stateCount": 4, "scheduledStart": 0},
        {"msg": "ListInstrument", "instrumentId": 4001, "eventId": 400, "instrumentClass": "SCALAR_BAND",
         "tickTableId": 1, "lotStep": 5, "allocationPolicy": "PRICE_TIME", "stateMask": 0b0011,
         "positionLimit": 0, "legs": []},
        {"msg": "ListInstrument", "instrumentId": 4002, "eventId": 400, "instrumentClass": "SCALAR_BAND",
         "tickTableId": 1, "lotStep": 5, "allocationPolicy": "PRICE_TIME", "stateMask": 0b0110,
         "positionLimit": 0, "legs": []},
        {"msg": "SetMarketState", "instrumentId": 4001, "state": "OPEN"},
        {"msg": "SetMarketState", "instrumentId": 4002, "state": "OPEN"},
        {"msg": "SetRuleLimits", "accountId": 0, "maxRestingOrders": 12, "maxNewOrdersPerSecond": 0,
         "maxOrderToTradeRatioMilli": 0, "ratioFreeOrdersPerSecond": 0},
    ]
    for a in ACCOUNTS:
        items.append({"op": "fund", "accountId": a, "cents": FUNDING[a]})
    counter = [0]
    out = []
    for item in items:
        out.extend(expand_setup(item, counter))
    return out


class Fuzzer:
    def __init__(self, seed):
        self.rng = random.Random(seed)
        self.driver = Driver(1)
        self.engine = self.driver.engine
        self.coid = 0
        self.cmd = 0
        self.recent_coids = {a: [] for a in ACCOUNTS}
        self.money = None
        self.mirror_pos, self.mirror_sleeve, self.mirror_level = {}, {}, {}
        self.transcript = []
        self.compiled = []

    # ---- input generators

    def next_coid(self, a):
        r = self.rng
        if self.recent_coids[a] and r.random() < 0.03:
            return r.choice(self.recent_coids[a])                    # a resend
        self.coid += 1
        c = f"c{self.coid}"
        self.recent_coids[a] = (self.recent_coids[a] + [c])[-5:]
        return c

    def next_cmd(self):
        self.cmd += 1
        return f"f{self.cmd}"

    def resting_of(self, a):
        return [o for o in self.engine.orders.values() if o.account_id == a]

    def pick_tick(self, i, side):
        r = self.rng
        inst = self.engine.instruments[i]
        opposite = self.engine.best(inst, "SELL" if side == "BUY" else "BUY")
        if opposite is not None and r.random() < 0.7:
            return max(0, min(98, opposite + r.randint(-3, 3)))
        return max(0, min(98, int(r.gauss(49, 12))))

    def order_fields(self, a, i=None, side=None):
        r = self.rng
        i = i or r.choice(INSTRUMENTS)
        side = side or r.choice(["BUY", "SELL"])
        tif = r.choices(["GTC", "IOC", "FOK", "GTD"], [6, 3, 1, 1])[0]
        flags = []
        if r.random() < 0.1 and tif in ("GTC", "GTD"):
            flags.append("POST_ONLY")
        if r.random() < 0.15:
            flags.append("REDUCE_ONLY")
        e = dict(entryType="NEW", clientOrderId=self.next_coid(a), instrumentId=i, side=side,
                 enteredContract=r.choice(["YES", "NO"]), tick=self.pick_tick(i, side),
                 quantity=LOT[i] * r.randint(1, 12), timeInForce=tif, flags=flags,
                 atEventStart=r.choice(["CANCEL", "KEEP"]))
        if tif == "GTD":
            e["expiresAfterNs"] = r.choice([0] + [n * 1_000_000 for n in range(1, 40)])
        if r.random() < 0.15:
            e["protectionTicks"] = r.randint(1, 5)
        if r.random() < 0.1:
            e["spendCapCents"] = r.randint(1, 800)
            e["side"], e["enteredContract"] = r.choice([("BUY", "YES"), ("SELL", "NO")])
        if r.random() < 0.2:
            e["clientOrderGroup"] = r.randint(1, 2)
        if r.random() < 0.02:
            e["tick"] = 120                                          # off the ladder
        if r.random() < 0.03 and LOT[i] > 1:
            e["quantity"] += 1                                       # off the lot step
        if r.random() < 0.01:
            e["instrumentId"] = 9999                                 # never listed
        return e

    def random_input(self):
        r = self.rng
        a = r.choice(ACCOUNTS)
        if self.engine.venue_mode != "NORMAL" and r.random() < 0.3:
            return dict(msg="SetVenueMode", commandId=self.next_cmd(), mode="NORMAL"), None
        kind = r.choices(
            ["new", "cancel", "amend", "mass", "batch", "sweep", "tick", "grant", "reclaim", "controls",
             "protect", "group", "reset", "market", "phase", "venue", "limits", "params", "seed"],
            [40, 10, 8, 2, 4, 3, 8, 2, 2, 1, 3, 3, 1, 2, 1, 1, 1, 1, 1])[0]
        if kind == "new":
            e = self.order_fields(a)
            e.pop("entryType")
            return dict(msg="NewOrder", accountId=a, **e), None
        if kind == "cancel":
            mine = self.resting_of(a)
            if not mine:
                return dict(msg="CancelOrder", accountId=a, instrumentId=BINARY_PT, orderId=12345), None
            o = r.choice(mine)
            if r.random() < 0.5:
                return dict(msg="CancelOrder", accountId=a, instrumentId=o.instrument_id, orderId=o.order_id), None
            return dict(msg="CancelOrder", accountId=a, instrumentId=o.instrument_id,
                        clientOrderId=o.client_order_id), None
        if kind == "amend":
            mine = self.resting_of(a)
            if not mine:
                return dict(msg="TimeTick"), None
            o = r.choice(mine)
            lot = LOT[o.instrument_id]
            f = dict(msg="AmendOrder", accountId=a, instrumentId=o.instrument_id, orderId=o.order_id)
            if r.random() < 0.6:
                f["newQuantity"] = lot * r.randint(1, 12)
            if r.random() < 0.5:
                f["newTick"] = max(0, min(98, o.tick + r.randint(-4, 4)))
            if r.random() < 0.2:
                f["atEventStart"] = r.choice(["CANCEL", "KEEP"])
            return f, None
        if kind == "mass":
            scope = r.choice(["SESSION", "ACCOUNT", "INSTRUMENT", "EVENT", "CLIENT_ORDER_GROUP"])
            ref = {"INSTRUMENT": r.choice(INSTRUMENTS), "EVENT": r.choice(list(STATES)),
                   "CLIENT_ORDER_GROUP": r.randint(1, 2)}.get(scope, 0)
            return dict(msg="MassCancel", accountId=a, scope=scope, scopeRef=ref,
                        cause=r.choice(["CLIENT_REQUEST", "SESSION_DISCONNECT", "HEARTBEAT_MISSED"])), None
        if kind == "batch":
            entries = []
            for _ in range(r.randint(0, 4)):
                mine = self.resting_of(a)
                if mine and r.random() < 0.3:
                    o = r.choice(mine)
                    entries.append(dict(entryType="CANCEL", instrumentId=o.instrument_id, orderId=o.order_id))
                else:
                    entries.append(self.order_fields(a))
            return dict(msg="BatchOrders", accountId=a, entries=entries), None
        if kind == "sweep":                                          # the same side in every outcome of event 200
            side = r.choice(["BUY", "SELL"])
            qty = r.randint(1, 4)
            entries = []
            for i in CATEGORICAL:
                e = self.order_fields(a, i, side)
                e.update(quantity=qty, timeInForce="IOC", flags=[], tick=98 if side == "BUY" else 0)
                e.pop("spendCapCents", None)
                e.pop("expiresAfterNs", None)
                entries.append(e)
            return dict(msg="BatchOrders", accountId=a, entries=entries), None
        if kind == "tick":
            return dict(msg="TimeTick"), r.choice([1, 2, 5, 20, 1500]) * 1_000_000
        if kind == "grant":
            return dict(msg="SleeveAdjust", commandId=self.next_cmd(), accountId=a,
                        deltaCents=r.randint(0, 3000), reason="GRANT"), None
        if kind == "reclaim":
            # Withdrawing everything, sale proceeds included, is what lets a later void leave an account below zero
            # (E-3.3, D-1).
            gains = [(max(x.event_cash.values(), default=0), k) for k, x in sorted(self.engine.accounts.items()) if k]
            best, richest = max(gains, default=(0, 0))
            if best > 0 and r.random() < 0.4:
                a = richest
            acc = self.engine.accounts.get(a)
            available = acc.balance - acc.reserved if acc else 0
            delta = available if available > 0 and r.random() < 0.6 else r.randint(1, 3000)
            return dict(msg="SleeveAdjust", commandId=self.next_cmd(), accountId=a,
                        deltaCents=delta, reason="RECLAIM"), None
        if kind == "controls":
            return dict(msg="SetAccountControls", commandId=self.next_cmd(), accountId=a,
                        closedOnly=r.choice(["FALSE", "FALSE", "TRUE"]),
                        selfMatchPrevention=r.choice(["CANCEL_RESTING", "CANCEL_INCOMING", "CANCEL_BOTH"])), None
        if kind == "protect":
            return dict(msg="SetMakerProtection", commandId=self.next_cmd(), accountId=a,
                        eventId=r.choice([0] + list(STATES)), maxContractsFilledPerWindow=r.randint(0, 30),
                        windowMillis=r.randint(1, 50)), None
        if kind == "group":
            return dict(msg="SetOrderGroup", accountId=a, clientOrderGroup=r.randint(1, 2),
                        maxContractsFilledPerWindow=r.randint(0, 20), windowMillis=r.randint(1, 50)), None
        if kind == "reset":
            return dict(msg="ResetOrderGroup", accountId=a, clientOrderGroup=r.randint(1, 2)), None
        if kind == "market":
            i = r.choice(INSTRUMENTS)
            state = r.choices(["OPEN", "HALTED", "CLOSED", "RESOLVING"], [6, 2, 1, 1])[0]
            return dict(msg="SetMarketState", commandId=self.next_cmd(), instrumentId=i, state=state), None
        if kind == "phase":
            return dict(msg="SetEventPhase", commandId=self.next_cmd(), eventId=r.choice(list(STATES)),
                        phase=r.choices(["STARTED", "SUSPENDED", "RESUMED", "ENDED"], [2, 1, 3, 1])[0],
                        source="OPERATOR"), None
        if kind == "venue":
            mode = r.choices(["NORMAL", "NEW_ORDER_REJECT", "CANCEL_ONLY", "KILL_SWITCH"], [6, 1, 1, 1])[0]
            return dict(msg="SetVenueMode", commandId=self.next_cmd(), mode=mode), None
        if kind == "limits":
            return dict(msg="SetRuleLimits", commandId=self.next_cmd(), accountId=r.choice([0] + ACCOUNTS),
                        maxRestingOrders=r.randint(0, 6), maxNewOrdersPerSecond=r.choice([0, 0, 3, 10]),
                        maxOrderToTradeRatioMilli=r.choice([0, 0, 2000, 5000]),
                        ratioFreeOrdersPerSecond=r.randint(0, 5)), None
        if kind == "params":
            return dict(msg="SetGroupParameters", commandId=self.next_cmd(),
                        activeOrderLimit=r.choice([0, 0, 8, 30]),
                        clientOrderIdWindowSeconds=r.choice([1, 5, 86_400])), None
        return (dict(msg="SeedSet", purpose="PRNG", seedHigh=r.getrandbits(32), seedLow=r.getrandbits(32))
                if r.random() < 0.5 else
                dict(msg="EngineStarted", nodeId=r.randint(0, 2), protocolVersion=1, binaryHash="")), None

    def resolution(self):
        r = self.rng
        if r.random() < 0.2:
            return dict(msg="ExpireParlay", commandId=self.next_cmd(), instrumentId=PARLAY, causingEventId=100)
        ev = r.choice(list(STATES))
        if r.random() < 0.5:
            return dict(msg="ResolveEvent", commandId=self.next_cmd(), eventId=ev, kind="RESOLVED",
                        winningState=r.randrange(STATES[ev]), caseId=1, voidWeights=[])
        # Prefer voiding an event whose reversal takes back more than some account holds, so accounts below zero
        # keep trading afterwards (E-3.3, D-1).
        e = self.engine
        exposed = [x for x in sorted(STATES) if x in e.events and not e.events[x].resolved and any(
            acc.event_cash.get(x, 0) > acc.balance - acc.reserved for k, acc in sorted(e.accounts.items()) if k)]
        if exposed and r.random() < 0.5:
            ev = r.choice(exposed)
        # A void reverses the event and refuses weights (E-15.3, D-1); send some anyway.
        weights = [dict(state=0, weight=1)] if r.random() < 0.1 else []
        return dict(msg="ResolveEvent", commandId=self.next_cmd(), eventId=ev, kind="VOID", caseId=1,
                    voidWeights=weights)

    # ---- checks

    def total_money(self):
        e = self.engine
        return sum(acc.balance for acc in e.accounts.values()) + \
            sum(ev.collateral for ev in e.events.values() if not ev.resolved)

    def absorb(self, inp, results):
        for res in results:
            m = res["msg"]
            if m == "PositionChanged":
                self.mirror_pos[(res["accountId"], res["instrumentId"])] = res["position"]
            elif m == "SleeveChanged":
                self.mirror_sleeve[res["accountId"]] = (res["availableCents"], res["reservedCents"])
            elif m == "BookLevelChanged":
                self.mirror_level[(res["instrumentId"], res["side"], res["tick"])] = (res["quantity"], res["orderCount"])
            elif m == "CommandApplied" and inp.get("msg") == "SleeveAdjust" and res["outcome"] == "APPLIED":
                self.money += inp["deltaCents"] if inp["reason"] == "GRANT" else -inp["deltaCents"]

    def check(self, step):
        e = self.engine
        if self.total_money() != self.money:
            raise AssertionError(f"step {step}: money {self.total_money()} != expected {self.money}")
        for a, acc in e.accounts.items():
            for i, p in acc.positions.items():
                if self.mirror_pos.get((a, i), 0) != p:
                    raise AssertionError(f"step {step}: position ({a},{i}) {p} but results say "
                                         f"{self.mirror_pos.get((a, i), 0)}")
            if self.mirror_sleeve.get(a, (0, 0)) != (acc.balance - acc.reserved, acc.reserved):
                raise AssertionError(f"step {step}: sleeve {a} differs from results")
        for i in e.instruments:
            for side in ("BUY", "SELL"):
                for t in range(99):
                    if self.mirror_level.get((i, side, t), (0, 0)) != e.level(i, side, t):
                        raise AssertionError(f"step {step}: level ({i},{side},{t}) differs from results")

    def run(self, steps):
        for inp in setup_inputs():
            results = self.driver.send(inp)
            self.compiled.append(dict(input=self.driver.last_input, expect=results, setup=True))
            self.transcript.append(results)
            self.absorb({}, results)
        self.money = self.total_money()
        for step in range(1, steps + 1):
            if step > steps * 0.8 and self.rng.random() < 0.05:
                inp, adv = self.resolution(), None
            else:
                inp, adv = self.random_input()
            results = self.driver.send(inp, adv)
            self.compiled.append(dict(input=self.driver.last_input, expect=results, expectFailStop=False))
            self.transcript.append(results)
            self.absorb(inp, results)
            self.check(step)
        return self.transcript


def fill_kind(r):
    if r["mintedPairs"] and r["burnedPairs"]:
        return "mixed"
    if r["mintedPairs"]:
        return "mint"
    if r["burnedPairs"]:
        return "burn"
    return "transfer"


def main(argv):
    ap = argparse.ArgumentParser()
    ap.add_argument("--seeds", type=int, default=50)
    ap.add_argument("--steps", type=int, default=300)
    ap.add_argument("--first", type=int, default=1)
    ap.add_argument("--export", help="write each seed's transcript as a compiled scenario into this directory")
    args = ap.parse_args(argv)
    results, rejects, cancels, fills = Counter(), Counter(), Counter(), Counter()
    for seed in range(args.first, args.first + args.seeds):
        try:
            fz = Fuzzer(seed)
            transcript = fz.run(args.steps)
            if args.export:
                import json
                from pathlib import Path
                out = Path(args.export)
                out.mkdir(parents=True, exist_ok=True)
                (out / f"fuzz-{seed}.json").write_text(json.dumps(dict(
                    name=f"fuzz seed {seed}", source="reference-model", normative=False, group=1,
                    steps=fz.compiled)))
        except (FailStop, AssertionError) as exc:
            print(f"FAIL seed {seed}: {type(exc).__name__}: {exc}")
            return 1
        if seed == args.first and Fuzzer(seed).run(args.steps) != transcript:
            print(f"FAIL seed {seed}: replay produced different results")
            return 1
        for batch in transcript:
            for res in batch:
                results[res["msg"]] += 1
                if res["msg"] == "OrderRejected":
                    rejects[res["reason"]] += 1
                elif res["msg"] == "OrderCancelled":
                    cancels[res["reason"]] += 1
                elif res["msg"] == "Fill":
                    fills[fill_kind(res)] += 1
    print(f"{args.seeds} seeds x {args.steps} steps: PASS")
    print(f"  fills {dict(fills)}; set burns {results['CollateralReleased']}; payouts {results['SettlementPayout']}")
    print(f"  rejects {dict(rejects.most_common())}")
    print(f"  cancels {dict(cancels.most_common())}")
    missing = sorted(ALL_RESULTS - set(results)) + sorted("reject:" + x for x in ALL_REJECTS - set(rejects)) + \
        sorted("cancel:" + x for x in ALL_CANCELS - set(cancels)) + sorted("fill:" + x for x in FILL_KINDS - set(fills))
    print(f"  never produced: {missing if missing else 'nothing'}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
