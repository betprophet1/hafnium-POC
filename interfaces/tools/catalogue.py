#!/usr/bin/env python3
"""Interface catalogue: check it against the blueprint and the SBE schema, or render the blueprint tables.

  catalogue.py check    exit 1 on any disagreement
  catalogue.py render   print the two <tbody> bodies the blueprint's §18 tables should contain
"""
import html
import re
import sys
import tomllib
import xml.etree.ElementTree as ET
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CATALOGUE = ROOT / "interfaces" / "catalogue.toml"
BLUEPRINT = ROOT / "prediction-exchange.html"

STATUS_LABEL = {
    "specified": "Specified, draft v0",
    "next": "Next (step {step})",
    "planned": "Planned (step {step})",
    "after-mm-review": "Drafted; frozen after market-maker review",
    "adopted": "Adopted, not ours to specify",
    "outside": "Outside our control; not specified",
}


def load():
    with open(CATALOGUE, "rb") as f:
        return tomllib.load(f)["interface"]


def status_label(i):
    return STATUS_LABEL[i["status"]].format(step=i.get("step", "?"))


def interface_row(i):
    return (f'      <tr><td class="num">{i["id"]}</td><td class="lbl">{i["name"]}</td>'
            f'<td>{i["transport"]}</td><td>{i["carries"]}</td><td>{i["guarantees"]}</td></tr>')


def spec_row(i):
    spec = f' <code>{i["spec"]}</code>' if i.get("spec") else ""
    return (f'      <tr><td class="num">{i["id"]}</td><td class="lbl">{i["name"]}</td>'
            f'<td>{i["format"]}{spec}</td><td>{status_label(i)}</td></tr>')


def render(items):
    return ("\n".join(interface_row(i) for i in items),
            "\n".join(spec_row(i) for i in items))


def norm(fragment):
    return re.sub(r"\s+", " ", html.unescape(re.sub(r"<[^>]+>", "", fragment))).strip()


def table_after(doc, marker):
    start = doc.index(marker)
    body = doc[doc.index("<tbody>", start) + len("<tbody>"):doc.index("</tbody>", start)]
    return [[norm(c) for c in re.findall(r"<td[^>]*>(.*?)</td>", row, re.S)]
            for row in re.findall(r"<tr>(.*?)</tr>", body, re.S)]


def check(items):
    problems = []
    ids = [i["id"] for i in items]
    if ids != list(range(1, len(items) + 1)):
        problems.append(f"ids must run 1..{len(items)} in order, got {ids}")
    for i in items:
        for key in ("name", "transport", "carries", "guarantees", "format", "status"):
            if not i.get(key):
                problems.append(f"{i.get('id')}: missing {key}")
        if i.get("status") not in STATUS_LABEL:
            problems.append(f"{i['id']}: unknown status {i.get('status')}")
        if i.get("status") in ("next", "planned") and "step" not in i:
            problems.append(f"{i['id']}: {i['status']} needs a step")
        if i.get("status") == "specified" and not i.get("spec"):
            problems.append(f"{i['id']}: specified but no spec file")
        if i.get("spec") and not (ROOT / i["spec"]).exists():
            problems.append(f"{i['id']}: spec file {i['spec']} does not exist")
        if bool(i.get("drawn")) != (i["id"] <= 19):
            problems.append(f"{i['id']}: drawn must be true exactly for the map's 19 interfaces")

    # Messages named by the catalogue must exist in their SBE schema.
    for i in items:
        if i.get("messages"):
            tree = ET.parse(ROOT / i["spec"])
            names = {m.get("name") for m in tree.getroot() if m.tag.endswith("message")}
            missing = sorted(set(i["messages"]) - names)
            if missing:
                problems.append(f"{i['id']}: messages not in {i['spec']}: {missing}")

    doc = BLUEPRINT.read_text(encoding="utf-8")
    want_iface, want_spec = render(items)
    got = table_after(doc, "<!-- interfaces:catalogue -->")
    want = [[norm(c) for c in re.findall(r"<td[^>]*>(.*?)</td>", r)] for r in want_iface.split("\n")]
    if got != want:
        problems.append(diff("interface table", got, want))
    got = table_after(doc, "<!-- interfaces:specifications -->")
    want = [[norm(c) for c in re.findall(r"<td[^>]*>(.*?)</td>", r)] for r in want_spec.split("\n")]
    if got != want:
        problems.append(diff("specification table", got, want))
    return problems


def diff(label, got, want):
    for n, (g, w) in enumerate(zip(got, want), 1):
        if g != w:
            return f"{label}: row {n} differs\n  blueprint: {g}\n  catalogue: {w}"
    return f"{label}: blueprint has {len(got)} rows, catalogue has {len(want)}"


def main():
    items = load()
    if sys.argv[1:] == ["render"]:
        iface, spec = render(items)
        print("<!-- interface table -->\n" + iface + "\n<!-- specification table -->\n" + spec)
        return 0
    if sys.argv[1:] == ["check"]:
        problems = check(items)
        for p in problems:
            print("FAIL", p)
        counts = {}
        for i in items:
            counts[i["status"]] = counts.get(i["status"], 0) + 1
        print(f"{len(items)} interfaces {counts}: {'FAIL' if problems else 'PASS'}")
        return 1 if problems else 0
    print(__doc__)
    return 2


if __name__ == "__main__":
    sys.exit(main())
