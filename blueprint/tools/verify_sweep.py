#!/usr/bin/env python3
"""Consistency sweep over the blueprint: table column counts, undefined CSS classes, quotation marks, dangling
section references, unexpected non-ASCII characters and wording left over from earlier drafts.

    python3 blueprint/tools/verify_sweep.py

Exits non-zero on any finding. Figure label geometry is figure_geometry.py.
"""
import re
import sys
from pathlib import Path

s = (Path(__file__).resolve().parent.parent / "prediction-exchange.html").read_text(encoding="utf-8")
body = s[s.index("</style>"):]
sections = len(re.findall(r'<section id="s\d+"', s))
findings = 0

mismatches = 0
for t in re.findall(r"<table>.*?</table>", s, re.S):
    hd = re.search(r"<thead>.*?</thead>", t, re.S)
    if not hd:
        continue
    n = len(re.findall(r"<th[ >]", hd.group(0)))
    for row in re.findall(r"<tr>.*?</tr>", t[hd.end():], re.S):
        c = len(re.findall(r"<td[ >]", row))
        if c and c != n:
            mismatches += 1
            print(" MISMATCH", n, c, re.sub(r"<[^>]*>", " ", row)[:90])

css = s[:s.index("</style>")]
defined = set(re.findall(r"\.([a-z][a-z0-9-]*)", css))
used = set()
for m in re.finditer(r'class="([^"]+)"', body):
    used |= set(m.group(1).split())
undefined = sorted(used - defined)
curly = sum(s.count(c) for c in "‘’“”")
ldquo, rdquo = s.count("&ldquo;"), s.count("&rdquo;")
dangling = [x for x in re.findall(r"&sect;(\d+)", s) if int(x) > sections]
nonascii = {c for c in body if ord(c) > 127} - {"—", "→", "−", "×", "≤", "·"}
print("table col mismatches", mismatches, "| undefined classes", undefined, "| curly", curly,
      "| ldquo/rdquo", ldquo, rdquo, "| dangling refs", dangling)
print("non-ascii beyond the existing set:", nonascii)

leftovers = 0
for w in ["shard", "Shard", "v2 &middot;", "Five nodes", "Four groups", "previous draft", "earlier draft", "tier-3",
          "target rate", "per session per day", "three or five", "price 1..99", "This was missing",
          "Nine of the open", "on throughput grounds", "dedicated core slot"]:
    c = body.count(w)
    if c:
        leftovers += 1
        print("  LEFTOVER %r %d" % (w, c))

findings = mismatches + len(undefined) + curly + (ldquo != rdquo) + len(dangling) + len(nonascii) + leftovers
print("PASS" if not findings else "FAIL (%d findings)" % findings)
sys.exit(1 if findings else 0)
