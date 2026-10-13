"""Summarise the memory samples of the 2026-10-13 gateway memory run. Usage: aggregate_memory.py DIR"""
import csv
import glob
import re
import statistics as st
import sys

d = sys.argv[1]


def load(path):
    rows = list(csv.DictReader(open(path)))
    t0 = float(rows[0]["ts"])
    return [{k: float(v) for k, v in r.items()} | {"t": float(r["ts"]) - t0} for r in rows]


def slope(xs, ys):
    mx, my = st.mean(xs), st.mean(ys)
    den = sum((x - mx) ** 2 for x in xs)
    return sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / den if den else 0.0


def mb(kb):
    return kb / 1024


def gb(kb):
    return kb / 1048576


def summary(rows):
    live = [r for r in rows if r["gw_rss_kb"] > 0]
    return {
        "samples": len(rows),
        "gw_first": mb(live[0]["gw_rss_kb"]),
        "gw_peak": mb(max(r["gw_hwm_kb"] for r in rows)),
        "gw_last": mb(live[-1]["gw_rss_kb"]),
        "threads": int(max(r["gw_threads"] for r in rows)),
        "fds": int(max(r["gw_fds"] for r in rows)),
        "node": mb(max(r["node_rss_kb"] for r in rows)),
        "gen_peak": gb(max(r["gen_rss_kb"] for r in rows)),
        "avail_min": gb(min(r["memavail_kb"] for r in rows)),
        "cached_max": gb(max(r["cached_kb"] for r in rows)),
        "dirty_max": mb(max(r["dirty_kb"] for r in rows)),
    }


print("## Representative replicates (32 then 128 connections, 60 s each, small profile)")
print(f"{'rep':>4} {'gw first MB':>12} {'gw peak MB':>11} {'threads':>8} {'fds':>6} {'node MB':>8} {'generator peak GB':>18} {'min avail GB':>13} {'max cached GB':>14} {'max dirty MB':>13}")
for p in sorted(glob.glob(f"{d}/mem-rep-*.csv")):
    s = summary(load(p))
    n = re.search(r"rep-(\d+)", p).group(1)
    print(f"{n:>4} {s['gw_first']:12.0f} {s['gw_peak']:11.0f} {s['threads']:8d} {s['fds']:6d} {s['node']:8.0f} {s['gen_peak']:18.1f} {s['avail_min']:13.1f} {s['cached_max']:14.1f} {s['dirty_max']:13.0f}")

print("\n## Retail-population ladder (launch profile, 33,000 accounts, 32 trading connections + N retail clients, 60 s, fresh server each)")
print(f"{'retail':>7} {'gw idle MB':>11} {'gw peak MB':>11} {'gw end MB':>10} {'peak - idle MB':>15} {'per retail client KB':>21} {'threads':>8} {'fds':>7} {'generator peak GB':>18} {'min avail GB':>13}")
base = None
for p in sorted(glob.glob(f"{d}/mem-ladder-*.csv"), key=lambda p: int(re.search(r"ladder-(\d+)", p).group(1))):
    n = int(re.search(r"ladder-(\d+)", p).group(1))
    s = summary(load(p))
    if n == 0:
        base = s["gw_peak"]
    per = f"{(s['gw_peak'] - base) * 1024 / n:21.0f}" if n and base is not None else f"{'-':>21}"
    print(f"{n:7d} {s['gw_first']:11.0f} {s['gw_peak']:11.0f} {s['gw_last']:10.0f} {s['gw_peak'] - s['gw_first']:15.0f} {per} {s['threads']:8d} {s['fds']:7d} {s['gen_peak']:18.1f} {s['avail_min']:13.1f}")

print("\n## Soak (128 connections, 300 s, small profile)")
rows = load(f"{d}/mem-soak.csv")
live = [r for r in rows if r["gw_rss_kb"] > 0 and r["t"] >= 60]
g_slope = slope([r["t"] for r in live], [mb(r["gw_rss_kb"]) for r in live]) * 60
gen = [r for r in rows if r["gen_rss_kb"] > 0]
gen_slope = slope([r["t"] for r in gen], [gb(r["gen_rss_kb"]) for r in gen]) * 60
s = summary(rows)
print(f"samples {s['samples']} over {rows[-1]['t']:.0f}s")
print(f"gateway RSS: {s['gw_first']:.0f} MB at start, {live[0]['gw_rss_kb'] / 1024:.0f} MB at 60 s, {s['gw_last']:.0f} MB at the end; peak {s['gw_peak']:.0f} MB; slope after 60 s {g_slope:+.2f} MB/min")
print(f"generator RSS: peak {s['gen_peak']:.1f} GB, growing {gen_slope:.1f} GB/min; machine min available {s['avail_min']:.1f} GB of 61")
last_gen = gen[-1]
print(f"generator last seen at t={last_gen['t']:.0f}s with {gb(last_gen['gen_rss_kb']):.1f} GB; run ended at t={rows[-1]['t']:.0f}s")
