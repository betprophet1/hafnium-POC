#!/usr/bin/env bash
# What batch size to apply the feed in.
#
# The whole venue is rebuilt at each setting -- node, seed and gateway -- not just the gateway. Restarting
# only the gateway makes it replay the feed from the beginning, which both saturates the feed thread with
# catch-up and gives each later arm a longer backlog than the one before: the first attempt at this measured
# 21k/s against a known 41k, with feed_apply at 97% of wall.
#
# $1 = server private ip, $2 = server public ip (to restart it over ssh).
set -u
SERVER="$1"; SERVER_PUB="$2"
GW="$SERVER:18081"; ING="$SERVER:17101"; M="http://$SERVER:18090/metrics"
OUT=~/batch.txt
cd ~/venue/core
ulimit -n 1048576 2>/dev/null || true
: > "$OUT"

snap() { curl -s --max-time 5 "$M" | grep -E "(conflate_publish|hub_read_wait|feed_apply)_duration_seconds_(sum|count)"; }

for b in 1 8 32; do
  echo "=== feed-batch=$b ===" | tee -a "$OUT"
  ssh -o StrictHostKeyChecking=no -i ~/key.pem "ubuntu@$SERVER_PUB" "bash ~/server-up.sh 20000 $b" >/dev/null 2>&1
  sleep 3
  ./target/perf/venue-sim gwload --gateway "$GW" --ingress "$ING" --instruments 8 --connections 1 --seconds 1 \
    >/dev/null 2>&1
  snap > /tmp/b0.txt
  ./target/perf/venue-sim gwload --gateway "$GW" --token-prefix dev-load- --ingress "$ING" \
    --instruments 8 --connections 32 --mode trade --orders-per-trade 10 \
    --retail 16384 --retail-books 2 --watchers 8 --retail-order-seconds 10 --seconds 60 2>&1 \
    | grep -v '^warning:' | tail -1 | tee -a "$OUT"
  snap > /tmp/b1.txt
  python3 - >> "$OUT" <<'PY'
import re
def load(p):
    d = {}
    for line in open(p):
        m = re.match(r'venue_gateway_(\w+)_duration_seconds_(sum|count)\{half="(\w+)"\} ([\d.]+)', line.strip())
        if m:
            d[(m.group(1), m.group(3), m.group(2))] = float(m.group(4))
    return d
b, a = load('/tmp/b0.txt'), load('/tmp/b1.txt')
for metric in ('feed_apply', 'hub_read_wait', 'conflate_publish'):
    for half in ('private', 'public'):
        ds = a.get((metric, half, 'sum'), 0) - b.get((metric, half, 'sum'), 0)
        dc = a.get((metric, half, 'count'), 0) - b.get((metric, half, 'count'), 0)
        if dc <= 0:
            continue
        print(f"    {metric:16} {half:8} n={int(dc):>9} mean {ds/dc*1e6:9.1f}us  total {ds:7.2f}s "
              f"= {ds/60*100:6.1f}% of wall")
PY
  sleep 5
done
echo "BATCHDONE" | tee -a "$OUT"
