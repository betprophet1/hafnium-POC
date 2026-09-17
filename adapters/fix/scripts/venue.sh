#!/usr/bin/env bash
# A throwaway local venue for the FIX adapter: a node, seeded with the mini venue's catalogue, an identity service for
# API keys, a gateway serving both halves, and a second gateway serving the private half beside it, as a venue with
# several gateways has, all on 127.0.0.1. Built binaries are taken from core/target/release
# (build them with `cargo build --release -p venue-node -p venue-gateway -p venue-sim -p venue-identity` in core/).
#
#   scripts/venue.sh start DIR   starts in DIR (wiped first), writes DIR/pids, DIR/gateway.address and
#                                DIR/second-gateway.address, waits until seeded
#   scripts/venue.sh stop DIR    stops exactly the processes recorded in DIR/pids
#
# Ports come from VENUE_FIX_PORTS_BASE (default 17410): base is the node's ingress, base+1 its feed, base+2 the gateway,
# base+3 the identity service, base+4 the second gateway.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
BIN="$ROOT/core/target/release"
CATALOGUE="$ROOT/deploy/charts/venue-mini/files/markets.json"
BASE="${VENUE_FIX_PORTS_BASE:-17410}"

wait_port() { # port what
  for _ in $(seq 1 300); do
    if (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null; then return 0; fi
    sleep 0.1
  done
  echo "$2 never listened on 127.0.0.1:$1" >&2
  return 1
}

case "${1:-}" in
  start)
    dir="${2:?usage: venue.sh start DIR}"
    for b in venue-node venue-gateway venue-sim venue-identity; do
      [[ -x "$BIN/$b" ]] || { echo "missing $BIN/$b: build it first" >&2; exit 2; }
    done
    rm -rf "$dir" && mkdir -p "$dir"
    ingress="127.0.0.1:$BASE"; feed="127.0.0.1:$((BASE + 1))"; gateway="127.0.0.1:$((BASE + 2))"
    "$BIN/venue-node" serve --listen "$ingress" --feed "$feed" --data "$dir/node" >"$dir/node.log" 2>&1 &
    echo $! >>"$dir/pids"
    wait_port "$BASE" "the node"
    "$BIN/venue-sim" seed --ingress "$ingress" --catalogue "$CATALOGUE" >"$dir/seed.log" 2>&1
    identity="127.0.0.1:$((BASE + 3))"
    "$BIN/venue-identity" --catalogue "$CATALOGUE" --store "$dir/identity.jsonl" --fsync false --listen "$identity" \
      >"$dir/identity.log" 2>&1 &
    echo $! >>"$dir/pids"
    wait_port "$((BASE + 3))" "the identity service"
    # Generous flood limits: a test sends in bursts no client would.
    "$BIN/venue-gateway" --feed "$feed" --ingress "$ingress" --catalogue "$CATALOGUE" --listen "$gateway" \
      --identity "$identity" \
      --per-second 10000 --burst 10000 --logons-per-minute 10000 --audit-dir "$dir/gateway-audit" --audit-fsync false \
      >"$dir/gateway.log" 2>&1 &
    echo $! >>"$dir/pids"
    wait_port "$((BASE + 2))" "the gateway"
    echo "$gateway" >"$dir/gateway.address"
    # Its own audit directory and instance: each gateway process keeps its own log.
    second="127.0.0.1:$((BASE + 4))"
    "$BIN/venue-gateway" --serve private --feed "$feed" --ingress "$ingress" --catalogue "$CATALOGUE" --listen "$second" \
      --identity "$identity" --audit-instance second-gateway \
      --per-second 10000 --burst 10000 --logons-per-minute 10000 --audit-dir "$dir/second-gateway-audit" \
      --audit-fsync false >"$dir/second-gateway.log" 2>&1 &
    echo $! >>"$dir/pids"
    wait_port "$((BASE + 4))" "the second gateway"
    echo "$second" >"$dir/second-gateway.address"
    echo "venue up: gateway $gateway, second gateway $second (logs in $dir)"
    ;;
  stop)
    dir="${2:?usage: venue.sh stop DIR}"
    if [[ -f "$dir/pids" ]]; then
      while read -r pid; do kill "$pid" 2>/dev/null || true; done <"$dir/pids"
      rm -f "$dir/pids"
    fi
    ;;
  *)
    echo "usage: venue.sh start|stop DIR" >&2
    exit 64
    ;;
esac
