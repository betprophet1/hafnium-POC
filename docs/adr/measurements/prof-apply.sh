#!/usr/bin/env bash
# Where `Hub::apply` spends the two thirds of every second it holds the private write lock.
#
# It is called per feed unit on the one thread per half that applies them, at 15us a unit and ~44k units a
# second, and requests queue behind it at ~40us a go. That is the largest known source of order latency, and
# nothing in the fan-out work touched it.
#
# Four mechanisms named from reading code in this ADR were wrong and every profile was right, so: profile.
set -u
PID=$(pgrep -x venue-gateway | head -1)
OUT=~/venue/run/apply.txt
: > "$OUT"

# The private half's feed thread is the one whose lock the order path waits on.
BEST=""; BESTC=-1
for t in /proc/$PID/task/*; do
  [ "$(cat "$t/comm" 2>/dev/null)" = "gw-feed" ] || continue
  tid=$(basename "$t")
  c=$(awk '{print $14 + $15}' "$t/stat")
  echo "gw-feed tid=$tid cpu_ticks=$c" | tee -a "$OUT"
  if [ "$c" -gt "$BESTC" ]; then BESTC=$c; BEST=$tid; fi
done
echo "profiling tid=$BEST (the busier feed thread)" | tee -a "$OUT"

sudo perf record -F 4999 -t "$BEST" -o ~/venue/run/apply.data -- sleep 40 2>>"$OUT"
sudo chown ubuntu ~/venue/run/apply.data
echo "=== self time, flat ===" | tee -a "$OUT"
perf report -i ~/venue/run/apply.data --stdio --no-children --percent-limit 0.8 2>/dev/null \
  | grep -E "^ +[0-9]" | head -28 | tee -a "$OUT"

sudo perf record -F 999 -g --call-graph dwarf,8192 -t "$BEST" -o ~/venue/run/apply-g.data -- sleep 30 2>>"$OUT"
sudo chown ubuntu ~/venue/run/apply-g.data
echo "=== inclusive, by caller ===" | tee -a "$OUT"
perf report -i ~/venue/run/apply-g.data --stdio --children --percent-limit 4 2>/dev/null \
  | grep -E "^ +[0-9]" | head -22 | tee -a "$OUT"
echo "APPLYDONE" | tee -a "$OUT"
