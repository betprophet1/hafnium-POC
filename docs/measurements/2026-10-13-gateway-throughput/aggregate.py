"""Aggregate three representative-load replicates and compare them with ADR-0002's `fix` arm, parsed the same way."""
import glob
import re
import statistics as st
import sys

new_dir, old_dir = sys.argv[1], sys.argv[2]


def parse(path):
    out, label = {}, None
    for line in open(path):
        s = line.strip()
        m = re.match(r"=== (\w+) ===", s)
        if m:
            label = m.group(1)
            continue
        m = re.search(r": (\d+) messages/s", s)
        if m and label and label != "feed_apply":
            d = {"rate": int(m.group(1))}
            for key, rx in (("p50", r"p50 ([\d.]+)"), ("p99", r"p99 ([\d.]+)"), ("p999", r"p99\.9 ([\d.]+)"),
                            ("fills_s", r"([\d.]+) fills/s"), ("ratio", r"traded \(1:(\d+)\)")):
                mm = re.search(rx, s)
                if mm:
                    d[key] = float(mm.group(1))
            md = re.search(r"(\d+) connection\(s\) dropped", s)
            d["dropped"] = int(md.group(1)) if md else 0
            out[label] = d
            label = None
            continue
        m = re.match(r"too_far_behind=(\d+)", s)
        if m:
            out["too_far_behind"] = int(m.group(1))
        m = re.match(r"audit_bytes=(\d+)", s)
        if m:
            out["audit_bytes"] = int(m.group(1))
        m = re.search(r'feed_apply_duration_seconds_(sum|count)\{half="(\w+)"\} ([\d.]+)', s)
        if m:
            out.setdefault("apply", {}).setdefault(m.group(2), {})[m.group(1)] = float(m.group(3))
    return out


def summarise(files):
    runs = [parse(f) for f in files]
    return runs


new = summarise(sorted(glob.glob(f"{new_dir}/real-main-*.txt")))
old = summarise(sorted(glob.glob(f"{old_dir}/real-fix-*.txt")))
print(f"new replicates: {len(new)}   ADR-0002 'fix' replicates: {len(old)}\n")
print(f"{'load':9} {'metric':16} {'this run (min / mean / max)':>34} {'ADR-0002 fix (mean)':>20} {'change':>8}")
for load in ("trade32", "trade128"):
    for metric, label in (("rate", "messages/s"), ("p50", "p50 round trip µs"), ("p99", "p99 round trip µs"),
                          ("fills_s", "fills/s")):
        a = [r[load][metric] for r in new if load in r and metric in r[load]]
        b = [r[load][metric] for r in old if load in r and metric in r[load]]
        if not a:
            continue
        mean_a = st.mean(a)
        mean_b = st.mean(b) if b else None
        change = f"{(mean_a / mean_b - 1) * 100:+.1f}%" if mean_b else ""
        old_s = f"{mean_b:.0f}" if mean_b else "-"
        print(f"{load:9} {label:16} {min(a):>10.0f} / {mean_a:>8.0f} / {max(a):>8.0f} {old_s:>20} {change:>8}")
    spread = [r[load]["rate"] for r in new if load in r]
    print(f"{'':9} {'rate spread':16} {(max(spread) - min(spread)) / st.mean(spread) * 100:>33.1f}%")
dropped = sum(r[l].get("dropped", 0) for r in new for l in ("trade32", "trade128") if l in r)
print(f"\nconnections dropped across all runs: {dropped}")
print("too_far_behind:", [r.get("too_far_behind") for r in new])
for half in ("public", "private"):
    means = [r["apply"][half]["sum"] / r["apply"][half]["count"] * 1e6 for r in new if "apply" in r]
    print(f"feed_apply mean per result, {half}: {st.mean(means):.2f} µs  (runs: {', '.join(f'{m:.2f}' for m in means)})")
print("audit bytes per replicate (GB):", [round(r.get("audit_bytes", 0) / 1e9, 1) for r in new])
