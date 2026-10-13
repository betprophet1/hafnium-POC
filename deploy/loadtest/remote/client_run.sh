#!/usr/bin/env bash
# On the client: one load against the server, with the generator's own memory sampled.
# $1 = server private IP   $2 = label   $3... = the gwload arguments after --gateway/--ingress/--token-prefix
set -u
ulimit -n 1048576 2>/dev/null || true
SERVER="$1"; LABEL="$2"; shift 2
cd ~/venue/core
. ~/.cargo/env
RUN=~/venue/run
bash ~/sample.sh $RUN/cli-$LABEL.csv & SP=$!
echo "=== $LABEL ===" >> $RUN/client.txt
./target/perf/venue-sim gwload --gateway "$SERVER:18081" --ingress "$SERVER:17101" --token-prefix dev-load- \
  --instruments 8 "$@" 2>&1 | grep -v '^warning:' | tail -1 >> $RUN/client.txt
echo "    client_load=$(cut -d' ' -f1-3 /proc/loadavg)" >> $RUN/client.txt
kill $SP 2>/dev/null; wait $SP 2>/dev/null
echo "client $LABEL done"
