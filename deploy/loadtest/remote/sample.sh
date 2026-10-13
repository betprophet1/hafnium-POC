#!/usr/bin/env bash
# Samples memory once a second until killed. $1 = output csv.
# The gateway, node and generator are found by name every tick, because the representative runs restart them.
OUT="$1"
echo "ts,gw_rss_kb,gw_hwm_kb,gw_threads,gw_fds,node_rss_kb,gen_rss_kb,memavail_kb,cached_kb,dirty_kb" > "$OUT"
i=0
fds=0
while true; do
  ts=$(date +%s.%N)
  gw=$(pgrep -x venue-gateway | head -1)
  nd=$(pgrep -x venue-node | head -1)
  sm=$(pgrep -x venue-sim | head -1)
  g="0,0,0"
  [ -n "$gw" ] && g=$(awk '/^VmRSS/{r=$2}/^VmHWM/{h=$2}/^Threads/{t=$2}END{print r+0","h+0","t+0}' /proc/$gw/status 2>/dev/null || echo "0,0,0")
  # fds are counted every fifth second only: listing tens of thousands of descriptors is not free.
  if [ $((i % 5)) -eq 0 ] && [ -n "$gw" ]; then fds=$(ls /proc/$gw/fd 2>/dev/null | wc -l); fi
  n=0; [ -n "$nd" ] && n=$(awk '/^VmRSS/{print $2}' /proc/$nd/status 2>/dev/null || echo 0)
  s=0; [ -n "$sm" ] && s=$(awk '/^VmRSS/{print $2}' /proc/$sm/status 2>/dev/null || echo 0)
  m=$(awk '/^MemAvailable/{a=$2}/^Cached:/{c=$2}/^Dirty/{d=$2}END{print a+0","c+0","d+0}' /proc/meminfo)
  echo "$ts,$g,$fds,${n:-0},${s:-0},$m" >> "$OUT"
  i=$((i + 1))
  sleep 1
done
