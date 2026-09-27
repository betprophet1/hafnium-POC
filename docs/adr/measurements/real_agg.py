import re, glob, statistics as st

ARMS = ['prestage', 'fix']

def parse(p):
    d = {'loads': {}, 'apply': {}, 'extra': {}}
    lab = None
    for line in open(p):
        m = re.match(r'=== (\w+) ===', line.strip())
        if m:
            lab = m.group(1); continue
        m = re.search(r': (\d+) messages/s', line)
        if m and lab and lab != 'feed_apply':
            d['loads'][lab] = {'rate': int(m.group(1))}
            mp = re.search(r'p50 ([\d.]+)', line)
            mf = re.search(r'(\d+) traded \(1:(\d+)\)', line)
            md = re.search(r'(\d+) connection\(s\) dropped', line)
            if mp: d['loads'][lab]['p50'] = float(mp.group(1))
            if mf: d['loads'][lab]['traded'] = int(mf.group(1)); d['loads'][lab]['ratio'] = int(mf.group(2))
            d['loads'][lab]['dropped'] = int(md.group(1)) if md else 0
            lab = None; continue
        m = re.search(r'feed_apply_duration_seconds_(sum|count)\{half="(\w+)"\} ([\d.]+)', line)
        if m:
            d['apply'].setdefault(m.group(2), {})[m.group(1)] = float(m.group(3))
        m = re.match(r'(too_far_behind|audit_bytes)=(\d+)', line.strip())
        if m:
            d['extra'][m.group(1)] = int(m.group(2))
    return d

arms = {a: [parse(p) for p in sorted(glob.glob(f'real-{a}-*.txt'))] for a in ARMS}

print("Representative load: 60s runs, 8 books (exponential popularity), ~10 orders per trade")
print("n=3 interleaved per arm; only the gateway binary differs.\n")
print(f"{'load':9} {'metric':10} {'pre-stage':>16} {'fix (HEAD)':>16} {'delta':>8}")
for load in ['trade32', 'trade128']:
    for metric, fmt in [('rate', '{:.0f}'), ('p50', '{:.0f}'), ('traded', '{:.0f}')]:
        v = {a: [r['loads'][load][metric] for r in arms[a] if load in r['loads'] and metric in r['loads'][load]]
             for a in ARMS}
        if not all(v[a] for a in ARMS):
            continue
        m = {a: st.mean(v[a]) for a in ARMS}
        c = lambda a: f"{m[a]:9.0f}+/-{st.stdev(v[a]):<5.0f}" if len(v[a]) > 1 else f"{m[a]:9.0f}"
        print(f"{load:9} {metric:10} {c('prestage'):>16} {c('fix'):>16} {m['fix']/m['prestage']-1:+7.1%}")
    print()

print("feed_apply us/unit")
for half in ['public', 'private']:
    v = {a: [r['apply'][half]['sum'] / r['apply'][half]['count'] * 1e6 for r in arms[a] if half in r['apply']]
         for a in ARMS}
    m = {a: st.mean(v[a]) for a in ARMS}
    c = lambda a: f"{m[a]:9.2f}+/-{st.stdev(v[a]):<4.2f}"
    print(f"{'':9} {half:10} {c('prestage'):>16} {c('fix'):>16} {m['fix']/m['prestage']-1:+7.1%}")

print()
for a in ARMS:
    drops = sum(r['loads'][l]['dropped'] for r in arms[a] for l in r['loads'])
    ratios = sorted({r['loads'][l].get('ratio') for r in arms[a] for l in r['loads']} - {None})
    audit = [r['extra'].get('audit_bytes', 0) for r in arms[a]]
    print(f"  {a:9} dropped={drops}  achieved ratios 1:{ratios}  audit {st.mean(audit)/1e9:.1f} GB/replicate")
