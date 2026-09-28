import re, glob, statistics as st

ARMS = ['prestage', 'lazy', 'typed']

def parse(p):
    d = {'loads': {}, 'apply': {}}
    lab = None
    for line in open(p):
        m = re.match(r'=== (\w+) ===', line.strip())
        if m:
            lab = m.group(1); continue
        m = re.search(r': (\d+) messages/s', line)
        if m and lab and lab != 'feed_apply':
            d['loads'][lab] = int(m.group(1)); lab = None; continue
        m = re.search(r'feed_apply_duration_seconds_(sum|count)\{half="(\w+)"\} ([\d.]+)', line)
        if m:
            d['apply'].setdefault(m.group(2), {})[m.group(1)] = float(m.group(3))
    return d

# reps 4-6 are the bisect round
arms = {a: [parse(p) for p in sorted(glob.glob(f'rep-{a}-[456].txt'))] for a in ARMS}

print("BISECT (reps 4-6, interleaved): ea4724c -> 0bf4a7b lazy Frame -> f982b3a typed View")
print(f"{'load':9} {'prestage':>15} {'lazy':>15} {'typed':>15}    vs prestage")
for load in ['trade32', 'trade128', 'amend32', 'amend128']:
    v = {a: [r['loads'][load] for r in arms[a] if load in r['loads']] for a in ARMS}
    if not all(len(v[a]) for a in ARMS):
        print(f"{load:9} incomplete"); continue
    m = {a: st.mean(v[a]) for a in ARMS}
    c = lambda a: f"{m[a]:7.0f}+/-{st.stdev(v[a]):<5.0f}"
    print(f"{load:9} {c('prestage'):>15} {c('lazy'):>15} {c('typed'):>15}    "
          f"lazy {m['lazy']/m['prestage']-1:+6.1%}, typed {m['typed']/m['prestage']-1:+6.1%}")

print()
print("feed_apply us/unit")
for half in ['public', 'private']:
    v = {a: [r['apply'][half]['sum'] / r['apply'][half]['count'] * 1e6 for r in arms[a]] for a in ARMS}
    m = {a: st.mean(v[a]) for a in ARMS}
    c = lambda a: f"{m[a]:6.2f}+/-{st.stdev(v[a]):<4.2f}"
    print(f"{half:9} {c('prestage'):>15} {c('lazy'):>15} {c('typed'):>15}    "
          f"lazy {m['lazy']/m['prestage']-1:+6.1%}, typed {m['typed']/m['prestage']-1:+6.1%}")

print()
for a in ARMS:
    print(f"  {a:9} units", [int(r['apply']['private']['count']) for r in arms[a]])
