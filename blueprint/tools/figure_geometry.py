#!/usr/bin/env python3
"""Label geometry for every figure in the blueprint.

    python3 blueprint/tools/figure_geometry.py

Fails if any label sits outside its viewBox, crosses a line or box edge, or overlaps another label. Label width
is estimated with the monospace advance of 0.6 em that the page's figure font uses; bare numbers are ignored.
"""
import html
import re
import sys
from pathlib import Path

DOC = Path(__file__).resolve().parent.parent / "prediction-exchange.html"
SIZE = {"lbl-hd": 12, "lbl-sub": 10.5, "lbl-edge": 10}


def attrs(a):
    return dict(re.findall(r'([\w:-]+)="([^"]*)"', a))


def hit(bb, seg):
    (x1, y1), (x2, y2) = seg
    X0, Y0, X1, Y1 = bb
    if x1 == x2:
        return X0 < x1 < X1 and max(min(y1, y2), Y0) < min(max(y1, y2), Y1)
    if y1 == y2:
        return Y0 < y1 < Y1 and max(min(x1, x2), X0) < min(max(x1, x2), X1)
    return any(X0 < x1 + (x2 - x1) * i / 40 < X1 and Y0 < y1 + (y2 - y1) * i / 40 < Y1 for i in range(41))


def check(svg):
    svg = re.sub(r"<defs>.*?</defs>", "", svg, flags=re.S)
    vb = list(map(float, re.search(r'viewBox="([^"]+)"', svg).group(1).split()))
    texts, segs = [], []
    for m in re.finditer(r"<text\b([^>]*)>(.*?)</text>", svg, re.S):
        at = attrs(m.group(1))
        cls = (at.get("class", "").split() or [""])[0]
        size = float(at.get("font-size", SIZE.get(cls, 10.5)))
        t = html.unescape(re.sub(r"<[^>]*>", "", m.group(2)))
        if "x" not in at or "y" not in at or t.strip().isdigit():
            continue
        x, y = float(at["x"]), float(at["y"])
        w = len(t) * 0.6 * size
        x0 = {"start": x, "middle": x - w / 2, "end": x - w}[at.get("text-anchor", "start")]
        texts.append((t, (x0, y - size * 0.78, x0 + w, y + size * 0.22)))
    for m in re.finditer(r"<rect\b([^>]*)/?>", svg):
        at = attrs(m.group(1))
        if all(k in at for k in ("x", "y", "width", "height")):
            x, y, w, h = (float(at[k]) for k in ("x", "y", "width", "height"))
            segs += [((x, y), (x + w, y)), ((x + w, y), (x + w, y + h)), ((x, y + h), (x + w, y + h)), ((x, y), (x, y + h))]
    for m in re.finditer(r"<path\b([^>]*)/?>", svg):
        pts = [tuple(map(float, q)) for q in re.findall(r"[ML]\s*([-\d.]+)[ ,]+([-\d.]+)", attrs(m.group(1)).get("d", ""))]
        segs += list(zip(pts, pts[1:]))
    for m in re.finditer(r"<line\b([^>]*)/?>", svg):
        at = attrs(m.group(1))
        if all(k in at for k in ("x1", "y1", "x2", "y2")):
            segs.append(((float(at["x1"]), float(at["y1"])), (float(at["x2"]), float(at["y2"]))))
    issues = []
    for i, (t, bb) in enumerate(texts):
        if bb[0] < vb[0] or bb[2] > vb[0] + vb[2]:
            issues.append("outside %r" % t)
        issues += ["%r crosses %s" % (t, g) for g in segs if hit(bb, g)]
        issues += ["%r overlaps %r" % (t, t2) for t2, b2 in texts[i + 1:]
                   if bb[0] < b2[2] and b2[0] < bb[2] and bb[1] < b2[3] and b2[1] < bb[3]]
    return len(texts), issues


def main():
    s = DOC.read_text(encoding="utf-8")
    per_section, figures, total = {}, 0, 0
    for m in re.finditer(r"<figure>.*?</figure>", s, re.S):
        sec = re.findall(r'<section id="s(\d+)">', s[:m.start()])[-1]
        per_section[sec] = per_section.get(sec, 0) + 1
        svg = re.search(r"<svg.*?</svg>", m.group(0), re.S)
        if not svg:
            continue
        labels, issues = check(svg.group(0))
        figures += 1
        total += len(issues)
        print("s%s figure %d: %d labels, %d issues%s" % (sec, per_section[sec], labels, len(issues),
                                                          " " + str(issues[:4]) if issues else ""))
    print("%d figures: %s" % (figures, "FAIL" if total else "PASS"))
    return 1 if total else 0


if __name__ == "__main__":
    sys.exit(main())
