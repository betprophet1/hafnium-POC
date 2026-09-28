#!/usr/bin/env bash
# Where the fan-out's 41ms a flush actually goes.
#
# Samples the busier `gw-conflate` thread only -- the public half's, which is the one doing the book fan-out.
# The suspicion is the `sessions.get(key)` BTreeMap lookup per subscriber, but that is a guess from reading
# `send_to_topic`, and guesses from reading code have been wrong twice in this ADR. The alternatives are the
# per-message channel node allocation and the atomics in `Outbox::put`.
#
# Run on the server while the client drives the load.
set -u
PID=$(pgrep -x venue-gateway | head -1)
OUT=~/venue/run/conflate.txt
: > "$OUT"

# Both conflate threads are called gw-conflate; the public half's does the book fan-out, so it is the busier.
BEST=""; BESTC=-1
for t in /proc/$PID/task/*; do
  [ "$(cat "$t/comm" 2>/dev/null)" = "gw-conflate" ] || continue
  tid=$(basename "$t")
  c=$(awk '{print $14 + $15}' "$t/stat")
  echo "gw-conflate tid=$tid cpu_ticks=$c" | tee -a "$OUT"
  if [ "$c" -gt "$BESTC" ]; then BESTC=$c; BEST=$tid; fi
done
echo "profiling tid=$BEST" | tee -a "$OUT"

# Flat first: undistorted, and enough to tell a map walk from an allocator from atomics.
sudo perf record -F 4999 -t "$BEST" -o ~/venue/run/conflate.data -- sleep 40 2>>"$OUT"
sudo chown ubuntu ~/venue/run/conflate.data
echo "=== self time, flat ===" | tee -a "$OUT"
perf report -i ~/venue/run/conflate.data --stdio --no-children --percent-limit 0.5 2>/dev/null \
  | grep -E "^ +[0-9]" | head -30 | tee -a "$OUT"

# Then with callers, to attribute the leaves to publish_books rather than anything else on the thread.
sudo perf record -F 999 -g --call-graph dwarf,8192 -t "$BEST" -o ~/venue/run/conflate-g.data -- sleep 30 2>>"$OUT"
sudo chown ubuntu ~/venue/run/conflate-g.data
echo "=== inclusive, by caller ===" | tee -a "$OUT"
perf report -i ~/venue/run/conflate-g.data --stdio --children --percent-limit 3 2>/dev/null \
  | grep -E "^ +[0-9]" | head -25 | tee -a "$OUT"
echo "CONFLATEDONE" | tee -a "$OUT"
