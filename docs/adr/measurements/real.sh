#!/usr/bin/env bash
# A representative replicate: 60-second runs, ~10 orders per trade, eight books with exponential popularity.
#
# 60s rather than 10s because the same load gives 38.5k over ten seconds and 27.8k over seventy -- a third of
# the short-run figure does not survive going the distance, and the sustained number is the one worth quoting.
#
# Only the gateway binary changes between arms. The load generator and the node are HEAD's in both cases:
# the client is a client whatever the server is, the node is the engine and not under test, and the pre-stage
# tree's venue-sim has neither --orders-per-trade nor the corrected fills counter. Swapping one binary is what
# makes the comparison about the gateway.
#
# $1 = arm label, $2 = gateway binary, $3 = replicate number.
set -u
ulimit -n 65536 2>/dev/null || true
ARM="$1"; GWBIN="$2"; REP="$3"
cd ~/venue/core
. ~/.cargo/env

GW=127.0.0.1:18081; ING=127.0.0.1:17101; FEED=127.0.0.1:17102
CLIENT_CORES=9-15,25-31
CAT=~/venue/run/load.json
OUT=~/venue/run/real-$ARM-$REP.txt

pkill -x venue-gateway 2>/dev/null; pkill -x venue-node 2>/dev/null; sleep 3
rm -rf ~/venue/run/data ~/venue/run/audit; mkdir -p ~/venue/run/data ~/venue/run/audit
: > "$OUT"

./target/perf/venue-sim catalogue --catalogue ../deploy/charts/venue-mini/files/markets.json \
  --out "$CAT" --accounts 512 >/dev/null 2>&1
nohup ./target/perf/venue-node serve --data ~/venue/run/data --listen "$ING" --feed "$FEED" \
  --tick-ms 50 --capacity small > ~/venue/run/node-$ARM-$REP.log 2>&1 &
sleep 3
./target/perf/venue-sim seed --ingress "$ING" --catalogue "$CAT" --capacity small \
  --orders-per-second 40000 > ~/venue/run/seed-$ARM-$REP.log 2>&1
nohup "$GWBIN" --feed "$FEED" --ingress "$ING" --catalogue "$CAT" --listen "$GW" \
  --metrics 127.0.0.1:18090 --reactor-threads 8 --pin-reactor-threads true \
  --per-second 2000000 --burst 2000000 --logons-per-minute 1200000 \
  --audit-dir ~/venue/run/audit > ~/venue/run/gw-$ARM-$REP.log 2>&1 &
sleep 4
PID=$(pgrep -x venue-gateway | head -1)
echo "## arm=$ARM rep=$REP pid=$PID gw=$GWBIN" | tee -a "$OUT"

# provisions the eight synthetic books; without --ingress/--instruments the later runs find no market
./target/perf/venue-sim gwload --gateway "$GW" --ingress "$ING" --instruments 8 --connections 1 --seconds 1 \
  >/dev/null 2>&1

run() {
  echo "=== $1 ===" | tee -a "$OUT"; shift
  taskset -c "$CLIENT_CORES" ./target/perf/venue-sim gwload --gateway "$GW" --token-prefix dev-load- \
    --ingress "$ING" --instruments 8 "$@" 2>&1 | grep -v '^warning:' | tail -1 | tee -a "$OUT"
}

run "trade32"  --connections 32  --seconds 60 --mode trade --orders-per-trade 10
run "trade128" --connections 128 --seconds 60 --mode trade --orders-per-trade 10

echo "=== feed_apply ===" | tee -a "$OUT"
curl -s --max-time 5 http://127.0.0.1:18090/metrics \
  | grep -E "feed_apply_duration_seconds_(sum|count)" | tee -a "$OUT"
echo "too_far_behind=$(grep -c 'too far behind' ~/venue/run/gw-$ARM-$REP.log)" | tee -a "$OUT"
echo "audit_bytes=$(du -sb ~/venue/run/audit | cut -f1)" | tee -a "$OUT"
echo "REALDONE" | tee -a "$OUT"
pkill -x venue-gateway 2>/dev/null; pkill -x venue-node 2>/dev/null; sleep 2
