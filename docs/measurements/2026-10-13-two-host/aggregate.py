"""Summarise the two-host run: gateway memory from the server's samples, generator memory from the client's,
throughput from the client's output. Usage: aggregate_twohost.py DIR"""
import csv
import re
import statistics as st
import sys

d = sys.argv[1]


def load(path):
    rows = list(csv.DictReader(open(path)))
    t0 = float(rows[0]["ts"])
    return [{k: float(v) for k, v in r.items()} | {"t": float(r["ts"]) - t0} for r in rows]


def slope(rows, key, after=0.0):
    pts = [(r["t"], r[key]) for r in rows if r["t"] >= after and r[key] > 0]
    if len(pts) < 2:
        return 0.0
    mx, my = st.mean(p[0] for p in pts), st.mean(p[1] for p in pts)
    den = sum((x - mx) ** 2 for x, _ in pts)
    return sum((x - mx) * (y - my) for x, y in pts) / den if den else 0.0


def gw(rows):
    live = [r for r in rows if r["gw_rss_kb"] > 0]
    return {"first": live[0]["gw_rss_kb"] / 1024, "peak": max(r["gw_hwm_kb"] for r in rows) / 1024,
            "last": live[-1]["gw_rss_kb"] / 1024, "threads": int(max(r["gw_threads"] for r in rows)),
            "fds": int(max(r["gw_fds"] for r in rows)), "avail": min(r["memavail_kb"] for r in rows) / 1048576}


def gen_peak_mb(rows):
    return max(r["gen_rss_kb"] for r in rows) / 1024


def client_lines(path):
    out, label = {}, None
    for line in open(path):
        s = line.strip()
        m = re.match(r"=== (\S+) ===", s)
        if m:
            label = m.group(1)
            continue
        if label and "messages/s" in s:
            d = {"rate": int(re.search(r": (\d+) messages/s", s).group(1))}
            for key, rx in (("p50", r"round trip µs p50 ([\d.]+)"), ("p99", r" p99 ([\d.]+)"),
                            ("fills_s", r"([\d.]+) fills/s"), ("retail_msgs_s", r"retail took \d+ messages, (\d+)/s"),
                            ("retail_dropped", r"(\d+) dropped")):
                mm = re.search(rx, s)
                if mm:
                    d[key] = float(mm.group(1))
            out[label] = d
        m = re.search(r"client_load=([\d.]+)", s)
        if m and label:
            out.setdefault(label, {})["client_load"] = float(m.group(1))
    return out


cl = client_lines(f"{d}/client.txt")

print("## Representative replicates (gateway on the server, generator on the client)")
print(f"{'load':16} {'messages/s':>10} {'p50 µs':>8} {'p99 µs':>8} {'fills/s':>8} {'client load':>11} {'generator peak MB':>18}")
for r in (1, 2, 3):
    for c in (32, 128):
        lab = f"rep{r}-trade{c}"
        x = cl.get(lab, {})
        g = gen_peak_mb(load(f"{d}/cli-{lab}.csv"))
        print(f"{lab:16} {x.get('rate', 0):10d} {x.get('p50', 0):8.0f} {x.get('p99', 0):8.0f} {x.get('fills_s', 0):8.0f} {x.get('client_load', 0):11.2f} {g:18.0f}")
for c in (32, 128):
    rates = [cl[f"rep{r}-trade{c}"]["rate"] for r in (1, 2, 3) if f"rep{r}-trade{c}" in cl]
    print(f"  trade{c}: mean {st.mean(rates):.0f} messages/s, min {min(rates)}, max {max(rates)}, spread {(max(rates) - min(rates)) / st.mean(rates) * 100:.1f}%")
print(f"{'replicate':10} {'gw first MB':>12} {'gw peak MB':>11} {'gw last MB':>11} {'threads':>8} {'fds':>6} {'server min avail GB':>20}")
for r in (1, 2, 3):
    s = gw(load(f"{d}/srv-rep-{r}.csv"))
    print(f"{r:10d} {s['first']:12.0f} {s['peak']:11.0f} {s['last']:11.0f} {s['threads']:8d} {s['fds']:6d} {s['avail']:20.1f}")

print("\n## Retail-population ladder (launch profile, 33,000 accounts, 32 trading connections + N retail)")
print(f"{'retail':>7} {'gw idle MB':>11} {'gw peak MB':>11} {'gw end MB':>10} {'per client KB':>14} {'threads':>8} {'fds':>7} {'trading msg/s':>14} {'retail msg/s':>13} {'dropped':>8} {'client load':>12} {'generator peak MB':>18}")
base = None
for n in (0, 1024, 4096, 16384):
    s = gw(load(f"{d}/srv-ladder-{n}.csv"))
    if n == 0:
        base = s["peak"]
    per = f"{(s['peak'] - base) * 1024 / n:14.0f}" if n else f"{'-':>14}"
    x = cl.get(f"ladder-{n}", {})
    g = gen_peak_mb(load(f"{d}/cli-ladder-{n}.csv"))
    print(f"{n:7d} {s['first']:11.0f} {s['peak']:11.0f} {s['last']:10.0f} {per} {s['threads']:8d} {s['fds']:7d} {x.get('rate', 0):14d} {x.get('retail_msgs_s', 0):13.0f} {int(x.get('retail_dropped', 0)):8d} {x.get('client_load', 0):12.2f} {g:18.0f}")

print("\n## Soak (128 connections, 300 s)")
srv = load(f"{d}/srv-soak.csv")
cli = load(f"{d}/cli-soak.csv")
s = gw(srv)
at60 = next(r for r in srv if r["t"] >= 60)["gw_rss_kb"] / 1024
print(f"gateway: {s['first']:.0f} MB at start, {at60:.0f} MB at 60 s, {s['last']:.0f} MB at the end, peak {s['peak']:.0f} MB, slope after 60 s {slope(srv, 'gw_rss_kb', 60) / 1024 * 60:+.2f} MB/min")
print(f"generator: peak {gen_peak_mb(cli):.0f} MB, slope after 60 s {slope(cli, 'gen_rss_kb', 60) / 1024 * 60:+.2f} MB/min; client min available {min(r['memavail_kb'] for r in cli) / 1048576:.1f} GB")
x = cl.get("soak", {})
print(f"throughput: {x.get('rate', 0)} messages/s, p50 {x.get('p50', 0):.0f} µs, p99 {x.get('p99', 0):.0f} µs, fills/s {x.get('fills_s', 0):.0f}, client load {x.get('client_load', 0):.2f}")
