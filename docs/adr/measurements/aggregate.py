import re, glob, statistics as st

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

arms = {}
for p in sorted(glob.glob('rep-*.txt')):
    arm = 'prestage' if 'prestage' in p else 'head'
    arms.setdefault(arm, []).append(parse(p))

def fmt(xs):
    return f"{st.mean(xs):8.0f} +/- {st.stdev(xs):5.0f}" if len(xs) > 1 else f"{xs[0]:8.0f}"

print("throughput, messages/s, mean +/- sd over n=3 interleaved replicates")
print(f"{'load':10} {'pre-stage':>18} {'HEAD':>18} {'delta':>8}")
for load in ['trade32', 'trade128', 'amend32', 'amend128']:
    p = [r['loads'][load] for r in arms['prestage'] if load in r['loads']]
    h = [r['loads'][load] for r in arms['head'] if load in r['loads']]
    if not p or not h:
        print(f"{load:10} incomplete"); continue
    d = st.mean(h)/st.mean(p) - 1
    print(f"{load:10} {fmt(p):>18} {fmt(h):>18} {d:+7.1%}")

print()
print("feed_apply, microseconds per unit, mean +/- sd")
print(f"{'half':10} {'pre-stage':>18} {'HEAD':>18} {'delta':>8}")
for half in ['public', 'private']:
    p = [r['apply'][half]['sum']/r['apply'][half]['count']*1e6 for r in arms['prestage']]
    h = [r['apply'][half]['sum']/r['apply'][half]['count']*1e6 for r in arms['head']]
    d = st.mean(h)/st.mean(p) - 1
    pf = f"{st.mean(p):8.2f} +/- {st.stdev(p):5.2f}"
    hf = f"{st.mean(h):8.2f} +/- {st.stdev(h):5.2f}"
    print(f"{half:10} {pf:>18} {hf:>18} {d:+7.1%}")

print()
print("unit counts per replicate (workloads must be comparable for the means above to be)")
for arm in ['prestage', 'head']:
    print(f"  {arm:9}", [int(r['apply']['private']['count']) for r in arms[arm]])
