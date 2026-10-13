#!/usr/bin/env bash
# On the server, after one load: what real.sh recorded, plus the gateway's memory. $1 = label.
LABEL="$1"
RUN=~/venue/run
OUT=$RUN/server.txt
GP=$(pgrep -x venue-gateway | head -1)
{
  echo "=== $LABEL ==="
  curl -s --max-time 5 http://127.0.0.1:18090/metrics | grep -E "feed_apply_duration_seconds_(sum|count)"
  echo "too_far_behind=$(grep -c 'too far behind' $RUN/gateway.log)"
  echo "audit_bytes=$(du -sb $RUN/audit | cut -f1)"
  echo "gateway VmHWM_kb=$(awk '/^VmHWM/{print $2}' /proc/$GP/status) VmRSS_kb=$(awk '/^VmRSS/{print $2}' /proc/$GP/status) threads=$(awk '/^Threads/{print $2}' /proc/$GP/status)"
  echo "server_load=$(cut -d' ' -f1-3 /proc/loadavg)"
} >> $OUT
