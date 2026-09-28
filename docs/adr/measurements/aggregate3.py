import re, glob, statistics as st

ARMS = ['prestage', 'oldhead', 'fix']

def parse(path):
    d = {'loads': {}, 'apply': {}}
    label = None
    for line in open(path):
        m = re.match(r'=== (\w+) ===', line.strip())
        if m:
            label = m.group(1); continue
        m = re.search(r': (\d+) messages/s', line)
        if m and label and label != 'feed_apply':
            d['loads'][label] = int(m.group(1)); label = None; continue
        m = re.search(r'feed_apply_duration_seconds_(sum|count)\{half="(\w+)"\} ([\d.]+)', line)
        if m:
            d['apply'].setdefault(m.group(2), {})[m.group(1)] = float(m.group(3))
    return d

arms = {a: [parse(p) for p in sorted(glob.glob(f'rep-{a}-*.txt'))] for a in ARMS}

def cell(xs):
    return f"{st.mean(xs):7.0f}+/-{st.stdev(xs):<5.0f}" if len(xs) > 1 else f"{xs[0]:7.0f}"

print("throughput, messages/s, mean +/- sd, n=3 interleaved per arm")
print(f"{'load':9} {'pre-stage':>14} {'old HEAD':>14} {'fix':>14}   {'fix vs old':>10} {'fix vs pre':>10}")
for load in ['trade32', 'trade128', 'amend32', 'amend128']:
    v = {a: [r['loads'][load] for r in arms[a] if load in r['loads']] for a in ARMS}
    if not all(v.values()):
        print(f"{load:9} incomplete"); continue
    m = {a: st.mean(v[a]) for a in ARMS}
    print(f"{load:9} {cell(v['prestage']):>14} {cell(v['oldhead']):>14} {cell(v['fix']):>14}   "
          f"{m['fix']/m['oldhead']-1:+9.1%} {m['fix']/m['prestage']-1:+9.1%}")

print()
print("feed_apply, microseconds per unit")
print(f"{'half':9} {'pre-stage':>14} {'old HEAD':>14} {'fix':>14}   {'fix vs old':>10} {'fix vs pre':>10}")
for half in ['public', 'private']:
    v = {a: [r['apply'][half]['sum'] / r['apply'][half]['count'] * 1e6 for r in arms[a]] for a in ARMS}
    m = {a: st.mean(v[a]) for a in ARMS}
    f = lambda a: f"{m[a]:7.2f}+/-{st.stdev(v[a]):<5.2f}"
    print(f"{half:9} {f('prestage'):>14} {f('oldhead'):>14} {f('fix'):>14}   "
          f"{m['fix']/m['oldhead']-1:+9.1%} {m['fix']/m['prestage']-1:+9.1%}")

print()
print("units applied per replicate (comparable workloads are what make the means comparable)")
for a in ARMS:
    print(f"  {a:9}", [int(r['apply']['private']['count']) for r in arms[a]])
