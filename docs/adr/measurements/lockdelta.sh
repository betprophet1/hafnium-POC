#!/usr/bin/env bash
# Lock cost at one population, as a before/after delta rather than a cumulative mean smeared across a sweep.
# $1 = server private ip, $2 = retail count. Run from the client box.
set -u
SERVER="$1"; R="$2"
GW="$SERVER:18081"; ING="$SERVER:17101"; M="http://$SERVER:18090/metrics"
cd ~/venue/core
ulimit -n 1048576 2>/dev/null || true

snap() { curl -s --max-time 5 "$M" | grep -E "(conflate_hold|conflate_publish|hub_read_wait|feed_apply)_duration_seconds_(sum|count)"; }

snap > /tmp/before.txt
./target/perf/venue-sim gwload --gateway "$GW" --token-prefix dev-load- --ingress "$ING" \
  --instruments 8 --connections 32 --mode trade --orders-per-trade 10 \
  --retail "$R" --retail-books 2 --watchers 8 --retail-order-seconds 10 --seconds 60 2>&1 | grep -v '^warning:' | tail -1
snap > /tmp/after.txt

python3 - "$R" <<'PY'
import re, sys
def load(p):
    d = {}
    for line in open(p):
        m = re.match(r'venue_gateway_(\w+)_duration_seconds_(sum|count)\{half="(\w+)"\} ([\d.]+)', line.strip())
        if m:
            d[(m.group(1), m.group(3), m.group(2))] = float(m.group(4))
    return d
b, a = load('/tmp/before.txt'), load('/tmp/after.txt')
retail = sys.argv[1]
print(f"--- lock cost over one 60s run at retail={retail} ---")
for metric in ('conflate_hold', 'conflate_publish', 'hub_read_wait', 'feed_apply'):
    for half in ('public', 'private'):
        ds = a.get((metric, half, 'sum'), 0) - b.get((metric, half, 'sum'), 0)
        dc = a.get((metric, half, 'count'), 0) - b.get((metric, half, 'count'), 0)
        if dc <= 0:
            continue
        mean_us = ds / dc * 1e6
        # What share of one wall-clock second this accounts for, over the 60s run.
        share = ds / 60.0
        print(f"  {metric:14} {half:8} n={int(dc):>9}  mean {mean_us:9.1f}us  total {ds:7.2f}s  = {share*100:6.1f}% of wall time")
PY
