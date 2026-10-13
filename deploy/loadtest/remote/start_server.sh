#!/usr/bin/env bash
# On the server: a fresh node and gateway on empty data, reachable from the client box (bound to every interface;
# the security group admits only the pair and ssh from the operator). As server-up.sh and real.sh start them.
# $1 = capacity (small|launch)   $2 = accounts in the catalogue   $3 = gateway logons per minute
set -u
ulimit -n 1048576 2>/dev/null || true
CAPACITY="$1"; ACCOUNTS="$2"; LOGONS="$3"
cd ~/venue/core
. ~/.cargo/env
CAT=~/venue/run/load.json
RUN=~/venue/run
pkill -x venue-gateway 2>/dev/null; pkill -x venue-node 2>/dev/null; sleep 3
rm -rf $RUN/data $RUN/audit; mkdir -p $RUN/data $RUN/audit
./target/perf/venue-sim catalogue --catalogue ../deploy/charts/venue-mini/files/markets.json \
  --out "$CAT" --accounts "$ACCOUNTS" >/dev/null 2>&1
nohup ./target/perf/venue-node serve --data $RUN/data --listen 0.0.0.0:17101 --feed 0.0.0.0:17102 \
  --tick-ms 50 --capacity "$CAPACITY" > $RUN/node.log 2>&1 &
sleep 4
./target/perf/venue-sim seed --ingress 127.0.0.1:17101 --catalogue "$CAT" --capacity "$CAPACITY" \
  --orders-per-second 40000 > $RUN/seed.log 2>&1
nohup ./target/perf/venue-gateway --feed 127.0.0.1:17102 --ingress 127.0.0.1:17101 --catalogue "$CAT" \
  --listen 0.0.0.0:18081 --metrics 127.0.0.1:18090 --reactor-threads 8 --pin-reactor-threads true \
  --per-second 2000000 --burst 2000000 --logons-per-minute "$LOGONS" \
  --audit-dir $RUN/audit > $RUN/gateway.log 2>&1 &
sleep 5
./target/perf/venue-sim gwload --gateway 127.0.0.1:18081 --ingress 127.0.0.1:17101 --instruments 8 \
  --connections 1 --seconds 1 >/dev/null 2>&1
echo "server up: capacity=$CAPACITY accounts=$ACCOUNTS gateway pid=$(pgrep -x venue-gateway | head -1)"
