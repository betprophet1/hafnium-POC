#!/usr/bin/env bash
# The venue, on its own box: node and gateway bound to every interface so the client machine can reach them.
# $1 = number of accounts the catalogue should hold.
set -u
ulimit -n 1048576 2>/dev/null || true
ACCOUNTS="${1:-33000}"
cd ~/venue/core
. ~/.cargo/env

ING=0.0.0.0:17101; FEED=0.0.0.0:17102; GW=0.0.0.0:18081
CAT=~/venue/run/load.json
OUT=~/venue/run/server.txt

pkill -x venue-gateway 2>/dev/null; pkill -x venue-node 2>/dev/null; sleep 3
rm -rf ~/venue/run/data ~/venue/run/audit; mkdir -p ~/venue/run/data ~/venue/run/audit
: > "$OUT"

# `launch`, not `small`: small holds 4,096 accounts and fail-stops partway through seeding a retail-sized
# population (`Capacity at input 4165`, spec §18).
./target/perf/venue-sim catalogue --catalogue ../deploy/charts/venue-mini/files/markets.json \
  --out "$CAT" --accounts "$ACCOUNTS" >/dev/null 2>&1
echo "catalogue accounts=$ACCOUNTS" | tee -a "$OUT"

nohup ./target/perf/venue-node serve --data ~/venue/run/data --listen "$ING" --feed "$FEED" \
  --tick-ms 50 --capacity launch > ~/venue/run/node.log 2>&1 &
sleep 4
./target/perf/venue-sim seed --ingress 127.0.0.1:17101 --catalogue "$CAT" --capacity launch \
  --orders-per-second 40000 > ~/venue/run/seed.log 2>&1
echo "seed rc=$? applied=$(grep -c APPLIED ~/venue/run/seed.log)" | tee -a "$OUT"

nohup ./target/perf/venue-gateway --feed 127.0.0.1:17102 --ingress 127.0.0.1:17101 --catalogue "$CAT" \
  --listen "$GW" --metrics 0.0.0.0:18090 --reactor-threads 8 --pin-reactor-threads true \
  --per-second 2000000 --burst 2000000 --logons-per-minute 100000000 \
  --audit-dir ~/venue/run/audit > ~/venue/run/gateway.log 2>&1 &
sleep 5
PID=$(pgrep -x venue-gateway | head -1)
echo "gateway pid=$PID node=$(pgrep -x venue-node | head -1)" | tee -a "$OUT"
echo "SERVERUP" | tee -a "$OUT"
