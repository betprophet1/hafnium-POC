import re, io, sys
s = io.open('prediction-exchange.html', encoding='utf-8').read()
errs = []

# sections: ids sequential, rail links resolve
ids = re.findall(r'<section id="s(\d+)"', s)
nums = [int(i) for i in ids]
if nums != list(range(1, len(nums)+1)):
    errs.append('section ids not sequential: %s' % nums)
labels = [int(x) for x in re.findall(r'<span class="num">(\d+)</span>', s)]
rail = re.findall(r'href="#s(\d+)"', s)
for r in set(rail):
    if ('id="s%s"' % r) not in s:
        errs.append('rail link #s%s unresolved' % r)

# HTML comment markers aligned with section ids
markers = [int(x) for x in re.findall(r'<!-- =+ (\d+) =+ -->', s)]
if markers and markers != nums:
    errs.append('comment markers desynced: %s vs %s' % (markers, nums))

# SVG markers defined vs referenced
defined = set(re.findall(r'<marker[^>]*\bid="([^"]+)"', s))
used = set(re.findall(r'url\(#([^)]+)\)', s))
if used - defined: errs.append('undefined marker refs: %s' % (used - defined))
if defined - used: errs.append('unused marker defs: %s' % (defined - used))
dupe_ids = [i for i in defined if s.count('id="%s"' % i) > 1]
if dupe_ids: errs.append('duplicate marker ids: %s' % dupe_ids)

# every CSS var used must be defined on bare :root
root = re.search(r':root\s*\{(.*?)\}', s, re.S)
declared = set(re.findall(r'(--[a-z0-9-]+)\s*:', root.group(1))) if root else set()
usedvars = set(re.findall(r'var\((--[a-z0-9-]+)', s))
missing = usedvars - declared
if missing: errs.append('CSS vars not on bare :root: %s' % sorted(missing))

# figures balanced
if s.count('<figure') != s.count('</figure>'):
    errs.append('figure imbalance %d/%d' % (s.count('<figure'), s.count('</figure>')))
if s.count('<svg') != s.count('</svg>'):
    errs.append('svg imbalance')

# dangling section cross-references
for ref in set(re.findall(r'&sect;(\d+)', s)):
    if int(ref) not in nums: errs.append('dangling cross-ref &sect;%s' % ref)
if re.search(r'&sect;@', s): errs.append('unreplaced sentinel present')

# tag balance (void-element aware)
VOID = {'br','hr','img','input','meta','link','path','circle','rect','line','polyline',
        'polygon','ellipse','use','stop','col','source','area','base'}
stack, bad = [], []
for m in re.finditer(r'<(/?)([a-zA-Z][a-zA-Z0-9]*)([^>]*?)(/?)>', s):
    close, tag, attrs, selfc = m.group(1), m.group(2).lower(), m.group(3), m.group(4)
    if tag in VOID or selfc == '/':
        continue
    if close:
        if stack and stack[-1] == tag: stack.pop()
        else: bad.append('unexpected </%s> near offset %d' % (tag, m.start()))
    else: stack.append(tag)
if stack: bad.append('unclosed: %s' % stack[-5:])
errs.extend(bad[:5])

print('sections: %d  labels: %d  figures: %d  bytes: %d' % (len(nums), len(labels), s.count('<figure'), len(s)))
print('FAIL' if errs else 'PASS')
for e in errs: print('  -', e)
sys.exit(1 if errs else 0)
