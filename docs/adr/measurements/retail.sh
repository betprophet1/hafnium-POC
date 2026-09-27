#!/usr/bin/env bash
# Does the venue hold up with a lot of customers on it?
#
# Order flow held constant at 32 trading connections while the retail population goes 0 -> 256 -> 1k -> 4k ->
# 16k. Each retail client is two sockets (market data + a session) and sends an order every 10s, so the order
# rate barely moves and the market-data fan-out grows 60-fold. If "market data is free" survives that, it is
# a result; if it does not, the point where it stops is the answer.
#
# 60-second runs, because 8-second ones hid an exhausted sleeve, a slow-consumer disconnect and a third of the
# throughput last time.
set -u
ulimit -n 1048576 2>/dev/null || ulimit -n 65536 2>/dev/null || true
cd ~/venue/core
. ~/.cargo/env

GW=127.0.0.1:18081; ING=127.0.0.1:17101; FEED=127.0.0.1:17102
CLIENT_CORES=9-15,25-31
CAT=~/venue/run/load.json
OUT=~/venue/run/retail.txt

pkill -x venue-gateway 2>/dev/null; pkill -x venue-node 2>/dev/null; sleep 3
rm -rf ~/venue/run/data ~/venue/run/audit; mkdir -p ~/venue/run/data ~/venue/run/audit
: > "$OUT"
echo "fd limit: $(ulimit -n)" | tee -a "$OUT"
# `small` holds 4,096 accounts (engine state.rs) and 16,384 retail customers need more than that: it
# fail-stops with `Capacity at input 4165` partway through seeding. That a retail-sized population needs
# `launch` is part of the answer, not a detail of the harness.
echo "capacity profile: launch (small caps at 4096 accounts)" | tee -a "$OUT"

# 32 trading + 16384 retail needs that many accounts, each with its own token.
./target/perf/venue-sim catalogue --catalogue ../deploy/charts/venue-mini/files/markets.json \
  --out "$CAT" --accounts 16500 >/dev/null 2>&1
nohup ./target/perf/venue-node serve --data ~/venue/run/data --listen "$ING" --feed "$FEED" \
  --tick-ms 50 --capacity launch > ~/venue/run/node.log 2>&1 &
sleep 3
./target/perf/venue-sim seed --ingress "$ING" --catalogue "$CAT" --capacity launch \
  --orders-per-second 40000 > ~/venue/run/seed.log 2>&1
echo "seed applied=$(grep -c APPLIED ~/venue/run/seed.log)" | tee -a "$OUT"
nohup ./target/perf/venue-gateway --feed "$FEED" --ingress "$ING" --catalogue "$CAT" --listen "$GW" \
  --metrics 127.0.0.1:18090 --reactor-threads 8 --pin-reactor-threads true \
  --per-second 2000000 --burst 2000000 --logons-per-minute 100000000 \
  --audit-dir ~/venue/run/audit > ~/venue/run/gateway.log 2>&1 &
sleep 4
PID=$(pgrep -x venue-gateway | head -1)
echo "gateway pid=$PID" | tee -a "$OUT"

./target/perf/venue-sim gwload --gateway "$GW" --ingress "$ING" --instruments 8 --connections 1 --seconds 1 \
  >/dev/null 2>&1

for r in 0 256 1024 4096 16384; do
  echo "=== retail=$r ===" | tee -a "$OUT"
  # The client is not pinned for the big populations: 16k threads onto 14 cores would measure the harness.
  PIN=""
  [ "$r" -le 256 ] && PIN="taskset -c $CLIENT_CORES"
  $PIN ./target/perf/venue-sim gwload --gateway "$GW" --token-prefix dev-load- --ingress "$ING" \
    --instruments 8 --connections 32 --mode trade --orders-per-trade 10 \
    --retail "$r" --retail-order-seconds 10 --seconds 60 2>&1 | grep -v '^warning:' | tail -1 | tee -a "$OUT"
  echo "    gw_rss_kB=$(awk '/VmRSS/{print $2}' /proc/$PID/status 2>/dev/null || echo GONE)" | tee -a "$OUT"
  echo "    gw_fds=$(ls /proc/$PID/fd 2>/dev/null | wc -l)" | tee -a "$OUT"
  echo "    gw_threads=$(ls /proc/$PID/task 2>/dev/null | wc -l)" | tee -a "$OUT"
  echo "    too_far_behind=$(grep -c 'too far behind' ~/venue/run/gateway.log)" | tee -a "$OUT"
  sleep 5
done

echo "=== per-thread CPU at the end ===" | tee -a "$OUT"
top -H -b -p "$PID" -n 2 -d 5 | awk '/COMMAND/{s++} s==2' | head -14 | tee -a "$OUT"
echo "=== feed_apply ===" | tee -a "$OUT"
curl -s --max-time 5 http://127.0.0.1:18090/metrics | grep -E "feed_apply_duration_seconds_(sum|count)" | tee -a "$OUT"
echo "RETAILDONE" | tee -a "$OUT"
pkill -x venue-gateway 2>/dev/null; pkill -x venue-node 2>/dev/null
