#!/usr/bin/env bash
# The load, from a machine that is not the one under test.
#
# Every earlier retail figure had the generator and the gateway sharing 32 vCPU, so at 16,384 clients the
# harness had 16k threads and ~33k sockets competing with the process it was measuring -- and per-client
# delivery fell, which is the harness starving rather than the venue saturating. This separates them.
#
# $1 = server private IP.
set -u
SERVER="$1"
GW="$SERVER:18081"
ING="$SERVER:17101"
OUT=~/sweep.txt
cd ~/venue/core
. ~/.cargo/env

ulimit -n 1048576 2>/dev/null || true
: > "$OUT"
echo "client fd limit $(ulimit -n), $(nproc) vCPU, ports $(cat /proc/sys/net/ipv4/ip_local_port_range)" | tee -a "$OUT"

# Provisions the eight synthetic books on the server.
./target/perf/venue-sim gwload --gateway "$GW" --ingress "$ING" --instruments 8 --connections 1 --seconds 1 \
  >/dev/null 2>&1

# Two sockets per retail client against one remote ip:port, so the ceiling is the local ephemeral range:
# ~64.5k ports means about 30k clients before the client machine runs out, not the venue. The population is
# now multiplexed over `nproc` epoll workers rather than a thread apiece, so the client should stay far below
# its cores -- client_load is recorded precisely because that is the assumption to check.
for r in 0 1024 4096 16384; do
  echo "=== retail=$r ===" | tee -a "$OUT"
  ./target/perf/venue-sim gwload --gateway "$GW" --token-prefix dev-load- --ingress "$ING" \
    --instruments 8 --connections 32 --mode trade --orders-per-trade 10 \
    --retail "$r" --retail-books 2 --watchers 8 --retail-order-seconds 10 --seconds 60 2>&1 \
    | grep -v '^warning:' | tail -1 | tee -a "$OUT"
  # Whether the client still had headroom is the thing that invalidated the last run, so record it.
  echo "    client_load=$(cut -d' ' -f1-3 /proc/loadavg)  client_sockets=$(ss -s 2>/dev/null | awk '/TCP:/{print $2; exit}')" | tee -a "$OUT"
  sleep 8
done
echo "SWEEPDONE" | tee -a "$OUT"
