"""Print a memory CSV as a coarse time series. Usage: series.py FILE [STEP_SECONDS]"""
import csv
import sys

path = sys.argv[1]
step = int(sys.argv[2]) if len(sys.argv) > 2 else 30
rows = list(csv.DictReader(open(path)))
t0 = float(rows[0]["ts"])
print(f"{path}: {len(rows)} samples over {float(rows[-1]['ts']) - t0:.0f}s")
print(f"{'t(s)':>5} {'gw_rss_MB':>10} {'gw_hwm_MB':>10} {'threads':>8} {'fds':>7} {'node_MB':>8} {'gen_MB':>8} {'avail_GB':>9} {'cached_GB':>9} {'dirty_MB':>9}")
last = -step
for r in rows:
    t = float(r["ts"]) - t0
    if t - last >= step:
        last = t
        print(f"{t:5.0f} {int(r['gw_rss_kb'])/1024:10.0f} {int(r['gw_hwm_kb'])/1024:10.0f} {r['gw_threads']:>8} {r['gw_fds']:>7} "
              f"{int(r['node_rss_kb'])/1024:8.0f} {int(r['gen_rss_kb'])/1024:8.0f} {int(r['memavail_kb'])/1048576:9.1f} "
              f"{int(r['cached_kb'])/1048576:9.1f} {int(r['dirty_kb'])/1024:9.0f}")
