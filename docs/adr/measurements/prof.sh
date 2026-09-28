#!/usr/bin/env bash
# Where the trading load's time goes, across every thread of the gateway.
#
# Deliberately not just `gw-feed`: the bisect showed `feed_apply` moved +3.4% while throughput fell 31%,
# so whatever costs the throughput is mostly NOT inside `apply`. Sampling one thread would have missed it.
#
# $1 = arm label, $2 = tree dir.
set -u
ulimit -n 65536 2>/dev/null || true
ARM="$1"; TREE="$2"
cd "$TREE/core"
. ~/.cargo/env

GW=127.0.0.1:18081; ING=127.0.0.1:17101; FEED=127.0.0.1:17102
CLIENT_CORES=9-15,25-31
CAT=~/venue/run/load.json
OUT=~/venue/run/prof-$ARM.txt

pkill -x venue-gateway 2>/dev/null; pkill -x venue-node 2>/dev/null; sleep 3
rm -rf ~/venue/run/data ~/venue/run/audit; mkdir -p ~/venue/run/data ~/venue/run/audit
: > "$OUT"

./target/perf/venue-sim catalogue --catalogue ../deploy/charts/venue-mini/files/markets.json \
  --out "$CAT" --accounts 512 >/dev/null 2>&1
nohup ./target/perf/venue-node serve --data ~/venue/run/data --listen "$ING" --feed "$FEED" \
  --tick-ms 50 --capacity small > ~/venue/run/node-$ARM.log 2>&1 &
sleep 3
./target/perf/venue-sim seed --ingress "$ING" --catalogue "$CAT" --capacity small \
  --orders-per-second 40000 > ~/venue/run/seed-$ARM.log 2>&1
nohup ./target/perf/venue-gateway --feed "$FEED" --ingress "$ING" --catalogue "$CAT" --listen "$GW" \
  --metrics 127.0.0.1:18090 --reactor-threads 8 --pin-reactor-threads true \
  --per-second 2000000 --burst 2000000 --logons-per-minute 1200000 \
  --audit-dir ~/venue/run/audit > ~/venue/run/gw-$ARM.log 2>&1 &
sleep 4
PID=$(pgrep -x venue-gateway | head -1)
echo "## arm=$ARM pid=$PID" | tee -a "$OUT"

# provisions instrument 9500000 -- without --ingress/--instruments every later run says "no such market"
taskset -c "$CLIENT_CORES" ./target/perf/venue-sim gwload --gateway "$GW" --ingress "$ING" \
  --instruments 8 --connections 1 --seconds 1 >/dev/null 2>&1

# 32 trading connections: the point where the bisect showed the biggest gap (-31.6%).
taskset -c "$CLIENT_CORES" ./target/perf/venue-sim gwload --gateway "$GW" --token-prefix dev-load- \
  --instrument 9500000 --connections 32 --seconds 70 --mode trade > ~/venue/run/load-$ARM.txt 2>&1 &
LOAD=$!
sleep 12

echo "=== per-thread CPU ===" | tee -a "$OUT"
top -H -b -p "$PID" -n 2 -d 5 | awk '/COMMAND/{s++} s==2' | head -16 | tee -a "$OUT"

# Whole process, flat self-time. No call graph: frame pointers are unreliable here and dwarf unwinding
# at this rate distorts what it measures.
sudo perf record -F 997 -p "$PID" -o ~/venue/run/perf-$ARM.data -- sleep 30 2>>"$OUT"
sudo chown ubuntu ~/venue/run/perf-$ARM.data

echo "=== self time by symbol ===" | tee -a "$OUT"
perf report -i ~/venue/run/perf-$ARM.data --stdio --no-children --percent-limit 0.4 2>/dev/null \
  | grep -E "^ +[0-9]" | head -40 | tee -a "$OUT"
echo "=== self time by object ===" | tee -a "$OUT"
perf report -i ~/venue/run/perf-$ARM.data --stdio --no-children --sort dso 2>/dev/null \
  | grep -E "^ +[0-9]" | head -8 | tee -a "$OUT"
echo "=== by thread ===" | tee -a "$OUT"
perf report -i ~/venue/run/perf-$ARM.data --stdio --no-children --sort comm 2>/dev/null \
  | grep -E "^ +[0-9]" | head -10 | tee -a "$OUT"

wait "$LOAD"
echo "=== load result ===" | tee -a "$OUT"
tail -1 ~/venue/run/load-$ARM.txt | tee -a "$OUT"
echo "=== feed_apply ===" | tee -a "$OUT"
curl -s --max-time 5 http://127.0.0.1:18090/metrics | grep -E "feed_apply_duration_seconds_(sum|count)" | tee -a "$OUT"
echo "PROFDONE" | tee -a "$OUT"
pkill -x venue-gateway 2>/dev/null; pkill -x venue-node 2>/dev/null; sleep 2
