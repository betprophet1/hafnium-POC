#!/usr/bin/env python3
"""Run engine scenarios against the reference model.

    python3 spec/engine/reference/run_scenarios.py [scenario.json ...]

With no arguments every file in spec/engine/scenarios is run. A scenario fails if any checked step's
results differ from its expectation in any field or in order. See spec/engine/scenarios/README.md.
"""
import json
import re
import sys
from pathlib import Path

from model import Engine, FailStop

HERE = Path(__file__).resolve().parent
SCENARIOS = HERE.parent / "scenarios"
T0 = 1_800_000_000_000_000_000
STEP_NS = 1_000_000
ORDER_ID_KEYS = {"orderId", "makerOrderId", "takerOrderId", "originalOrderId"}
CLIENT_MESSAGES = {"NewOrder", "CancelOrder", "AmendOrder", "MassCancel", "BatchOrders", "BasketMintRedeem",
                   "SetOrderGroup", "ResetOrderGroup"}
NON_COMMANDS = CLIENT_MESSAGES | {"TimeTick", "EngineStarted", "SeedSet"}

NEW_ORDER_DEFAULTS = dict(enteredContract="YES", timeInForce="GTC", atEventStart="CANCEL", flags=[],
                          protectionTicks=0, spendCapCents=None, enteredStakeCents=None, clientOrderGroup=None,
                          expiresAt=None)
RESULT_DEFAULTS = {
    "Fill": dict(makerFeeCents=0, takerFeeCents=0),
    "OrderRejected": dict(originalOrderId=None),
    "CommandApplied": dict(rejectReason="NONE"),
}


def placeholder_ids(obj, group):
    """O<n> and T<n> in id fields stand for the n-th order and trade ID of the group (E-2.8)."""
    if isinstance(obj, list):
        return [placeholder_ids(x, group) for x in obj]
    if not isinstance(obj, dict):
        return obj
    out = {}
    for k, v in obj.items():
        if k in ORDER_ID_KEYS and isinstance(v, str) and re.fullmatch(r"O\d+", v):
            v = (group << 48) + int(v[1:])
        elif k == "tradeId" and isinstance(v, str) and re.fullmatch(r"T\d+", v):
            v = (group << 48) + int(v[1:])
        else:
            v = placeholder_ids(v, group)
        out[k] = v
    return out


def expand_setup(item, counter):
    """Setup shorthands; anything with "msg" is a raw input."""
    def cmd(msg, **fields):
        counter[0] += 1
        return dict(msg=msg, commandId=fields.pop("commandId", f"setup-{counter[0]}"), **fields)

    if "msg" in item:
        if item["msg"] not in NON_COMMANDS and "commandId" not in item:
            fields = dict(item)
            return [cmd(fields.pop("msg"), **fields)]
        return [item]
    op = item["op"]
    if op == "centTable":                  # ticks 0..98 are 1c..99c
        ticks = [dict(tick=k, numerator=k + 1, denominator=100, complement=98 - k) for k in range(99)]
        return [cmd("DefineTickTable", tickTableId=item["tickTableId"], ticks=ticks)]
    if op == "fund":
        return [cmd("SleeveAdjust", accountId=item["accountId"], deltaCents=item["cents"], reason="GRANT")]
    listing = dict(tickTableId=item.get("tickTableId", 1), lotStep=item.get("lotStep", 1),
                   allocationPolicy=item.get("allocationPolicy", "PRICE_TIME"),
                   positionLimit=item.get("positionLimit", 0), legs=[])
    if op == "binary":
        return [cmd("DefineEvent", eventId=item["eventId"], stateCount=2, scheduledStart=0),
                cmd("ListInstrument", instrumentId=item["instrumentId"], eventId=item["eventId"],
                    instrumentClass="BINARY", stateMask=2, **listing),
                cmd("SetMarketState", instrumentId=item["instrumentId"], state="OPEN")]
    if op == "categorical":
        ids = item["instrumentIds"]
        out = [cmd("DefineEvent", eventId=item["eventId"], stateCount=len(ids), scheduledStart=0)]
        for k, i in enumerate(ids):
            out.append(cmd("ListInstrument", instrumentId=i, eventId=item["eventId"],
                           instrumentClass="CATEGORICAL_OUTCOME", stateMask=1 << k, **listing))
            out.append(cmd("SetMarketState", instrumentId=i, state="OPEN"))
        return out
    if op == "parlay":
        listing["legs"] = item["legs"]
        return [cmd("DefineEvent", eventId=item["eventId"], stateCount=2, scheduledStart=0),
                cmd("ListInstrument", instrumentId=item["instrumentId"], eventId=item["eventId"],
                    instrumentClass="PARLAY", stateMask=2, **listing),
                cmd("SetMarketState", instrumentId=item["instrumentId"], state="OPEN")]
    raise ValueError(f"unknown setup op {op}")


def order_defaults(fields, now):
    out = dict(NEW_ORDER_DEFAULTS)
    out.update(fields)
    if "expiresAfterNs" in out:
        out["expiresAt"] = now + out.pop("expiresAfterNs")
    return out


def with_defaults(inp, now):
    inp = dict(inp)
    if inp["msg"] in CLIENT_MESSAGES:
        inp.setdefault("sessionId", inp.get("accountId", 0) * 10)
    if inp["msg"] == "NewOrder":
        inp = order_defaults(inp, now)
    if inp["msg"] == "AmendOrder":
        inp.setdefault("atEventStart", "UNCHANGED")
    if inp["msg"] == "BatchOrders":
        inp["entries"] = [order_defaults(e, now) if e["entryType"] == "NEW" else e for e in inp["entries"]]
    if inp["msg"] == "ResolveEvent":
        inp.setdefault("winningState", 0)
        inp.setdefault("caseId", 0)
    return inp


def expected_with_defaults(result, group):
    r = dict(RESULT_DEFAULTS.get(result["msg"], {}))
    if result["msg"] in ("OrderAccepted", "OrderRejected", "BasketApplied"):
        r["sessionId"] = result.get("accountId", 0) * 10
    r.update(result)
    return placeholder_ids(r, group)


class Driver:
    """Stamps record headers the way E-17 requires and checks result headers (E-5.1)."""

    def __init__(self, group, engine=None):
        self.engine = engine or Engine(group=group)
        self.group = group
        self.sequence = 0
        self.now = T0
        self.last_input = None

    def send(self, inp, advance_ns=None):
        self.sequence += 1
        self.now += STEP_NS if advance_ns is None else advance_ns
        inp = placeholder_ids(with_defaults(inp, self.now), self.group)
        inp["record"] = dict(sequence=self.sequence, sequencedAt=self.now, sequencerGroup=self.group)
        self.last_input = inp
        results = self.engine.apply(dict(inp))
        for n, r in enumerate(results):
            h = r["result"]
            assert h == dict(inputSequence=self.sequence, resultIndex=n, sequencedAt=self.now,
                             sequencerGroup=self.group), f"bad result header {h}"
        return [{k: v for k, v in r.items() if k != "result"} for r in results]


def run(path):
    sc = json.loads(Path(path).read_text())
    group = sc.get("group", 1)
    d = Driver(group)
    counter = [0]
    for item in sc.get("setup", []):
        for inp in expand_setup(item, counter):
            for r in d.send(inp):
                if r["msg"] == "CommandApplied" and r["outcome"] != "APPLIED":
                    return f"setup command {inp['msg']} not applied: {r}"
    for n, step in enumerate(sc["steps"], 1):
        inp = step["input"]
        if "commandId" not in inp and inp["msg"] not in NON_COMMANDS:
            return f"step {n}: commands in steps must give a commandId"
        try:
            got = d.send(inp, step.get("advanceNs"))
        except FailStop as e:
            if step.get("expectFailStop"):
                continue
            return f"step {n} ({inp['msg']}): unexpected fail-stop: {e}"
        if step.get("expectFailStop"):
            return f"step {n}: expected a fail-stop"
        if step.get("expect") is None:
            continue
        want = [expected_with_defaults(r, group) for r in step["expect"]]
        if got != want:
            lines = [f"step {n} ({inp['msg']}) results differ:"]
            for k in range(max(len(got), len(want))):
                g = got[k] if k < len(got) else None
                w = want[k] if k < len(want) else None
                mark = "  " if g == w else "!!"
                lines.append(f"  {mark} [{k}] model:    {g}")
                if g != w:
                    lines.append(f"  {mark} [{k}] expected: {w}")
            return "\n".join(lines)
    return None


def compile_scenario(path):
    """The scenario with every input and expected result fully explicit: record headers stamped, defaults
    applied, O<n>/T<n> resolved. This is what implementations in other languages consume."""
    sc = json.loads(Path(path).read_text())
    group = sc.get("group", 1)
    d = Driver(group)
    counter = [0]
    steps = []
    for item in sc.get("setup", []):
        for inp in expand_setup(item, counter):
            d.send(inp)
            steps.append(dict(input=d.last_input, expect=None, setup=True))
    for step in sc["steps"]:
        try:
            d.send(step["input"], step.get("advanceNs"))
        except FailStop:
            pass
        expect = None if step.get("expect") is None else [expected_with_defaults(r, group) for r in step["expect"]]
        steps.append(dict(input=d.last_input, expect=expect, expectFailStop=bool(step.get("expectFailStop"))))
    return dict(name=sc["name"], source=Path(path).name, normative=True, group=group, steps=steps)


def main(argv):
    if argv[:1] == ["--compile"]:
        out = Path(argv[1])
        out.mkdir(parents=True, exist_ok=True)
        paths = argv[2:] or sorted(str(p) for p in SCENARIOS.glob("*.json"))
        for p in paths:
            (out / Path(p).name).write_text(json.dumps(compile_scenario(p), indent=1))
        print(f"compiled {len(paths)} scenarios into {out}")
        return 0
    paths = argv or sorted(str(p) for p in SCENARIOS.glob("*.json"))
    failures = 0
    for p in paths:
        problem = run(p)
        name = Path(p).name
        if problem:
            failures += 1
            print(f"FAIL {name}\n{problem}")
        else:
            print(f"ok   {name}")
    print(f"{len(paths) - failures}/{len(paths)} scenarios pass")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
