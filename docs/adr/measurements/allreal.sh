#!/usr/bin/env bash
# Interleaved P/F so host drift falls on both arms. Three replicates, two 60s runs each: ~15 minutes.
set -u
for r in 1 2 3; do
  bash ~/real.sh prestage ~/prestage/core/target/perf/venue-gateway "$r" >> ~/venue/real.log 2>&1
  bash ~/real.sh fix      ~/venue/core/target/perf/venue-gateway "$r" >> ~/venue/real.log 2>&1
done
echo "ALLREALDONE" >> ~/venue/real.log
