#!/usr/bin/env bash
# The mini venue on a laptop, in one command (blueprint §16): builds the images, creates a k3d cluster if there is
# none, imports the images, installs the chart and runs its smoke test.
#
#   deploy/laptop.sh up [--split] [--no-fix] [HELM FLAGS...]
#                              build, install or upgrade, test; --split serves the gateway's public and private halves
#                              from StatefulSets of their own (VENUE_SPLIT=1 does the same), --no-fix leaves the FIX
#                              adapter out (VENUE_FIX=0), and helm flags such as --set pass through
#   deploy/laptop.sh web [PORT]
#                              port-forward the web page to http://127.0.0.1:8080 (or PORT)
#   deploy/laptop.sh expose ADDRESS [HTTP_PORT] [FIX_PORT]
#                              publish the ingress (default port 8088) and FIX (default 9878) on ADDRESS of this
#                              machine through k3d's load balancer; only that address, never 0.0.0.0 unless given
#   deploy/laptop.sh scale public|private REPLICAS
#                              scale one half of a split gateway, keeping every other value
#   deploy/laptop.sh restart   replace the core pod after an image rebuild (the StatefulSet is OnDelete)
#   deploy/laptop.sh down      delete the cluster and everything in it, journal included
#
# Needs Docker, k3d, helm and kubectl. VENUE_CLUSTER, VENUE_RELEASE and VENUE_TAG override the defaults.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CLUSTER="${VENUE_CLUSTER:-venue}"
RELEASE="${VENUE_RELEASE:-venue}"
TAG="${VENUE_TAG:-dev}"
CHART="$ROOT/deploy/charts/venue-mini"

case "${1:-up}" in
  up)
    shift || true
    split="${VENUE_SPLIT:-0}"
    fix="${VENUE_FIX:-1}"
    helm_flags=()
    for arg in "$@"; do
      case "$arg" in
        --split) split=1 ;;
        --no-split) split=0 ;;
        --no-fix) fix=0 ;;
        *) helm_flags+=("$arg") ;;
      esac
    done
    split_value=false; [[ "$split" == 1 ]] && split_value=true
    fix_value=false; [[ "$fix" == 1 ]] && fix_value=true
    docker build -f "$ROOT/deploy/images/venue-node.Dockerfile" -t "venue:$TAG" "$ROOT"
    images=("venue:$TAG")
    if [[ "$fix" == 1 ]]; then
      docker build -f "$ROOT/deploy/images/venue-fix.Dockerfile" -t "venue-fix:$TAG" "$ROOT"
      images+=("venue-fix:$TAG")
    fi
    k3d cluster get "$CLUSTER" >/dev/null 2>&1 || k3d cluster create "$CLUSTER" --wait
    # Straight into the nodes' containerd: the default mode, through a tools container and a tarball, can fail to import
    # an image of this size ("short read") and still report success.
    k3d image import "${images[@]}" --cluster "$CLUSTER" --mode direct
    # On a fresh cluster the smoke test can start before cluster DNS answers.
    kubectl -n kube-system rollout status deployment/coredns --timeout 2m
    helm upgrade --install "$RELEASE" "$CHART" --set image.tag="$TAG" --set gateway.split.enabled="$split_value" \
      --set fix.enabled="$fix_value" --set ingress.enabled=true "${helm_flags[@]}" --wait --timeout 5m
    # A rebuilt image with the same tag reaches pods only when they are replaced. The core is OnDelete, so it would
    # otherwise keep the old build indefinitely; replacing it here is safe on a laptop because it replays its journal.
    "$0" restart
    kubectl rollout restart deployment -l "app.kubernetes.io/instance=$RELEASE"
    kubectl rollout status deployment -l "app.kubernetes.io/instance=$RELEASE" --timeout 3m
    # The split gateway's halves and the FIX adapter are StatefulSets: replaced pod by pod, each under its own name.
    for set in $(kubectl get statefulset -l "app.kubernetes.io/instance=$RELEASE,app.kubernetes.io/component in (gateway,fix)" -o name); do
      kubectl rollout restart "$set"
      kubectl rollout status "$set" --timeout 3m
    done
    helm test "$RELEASE" --logs
    echo
    echo "deploy/laptop.sh web                      then open http://127.0.0.1:8080"
    echo "deploy/laptop.sh expose 127.0.0.1         or the ingress on http://127.0.0.1:8088 and FIX on 9878"
    ;;
  web)
    exec kubectl port-forward "svc/$RELEASE-web" "${2:-8080}:8080"
    ;;
  expose)
    address="${2:?usage: deploy/laptop.sh expose ADDRESS [HTTP_PORT] [FIX_PORT]}"
    # k3d's load balancer forwards the host's ADDRESS:HTTP_PORT to Traefik's port 80 on the nodes, and
    # ADDRESS:FIX_PORT to the FIX Service's port there (fix.service.port, a LoadBalancer Service that k3s publishes on
    # each node). The load balancer container is recreated with the new ports; the cluster keeps running.
    fix_port="$(kubectl get service "$RELEASE-fix" -o jsonpath='{.spec.ports[0].port}' 2>/dev/null || true)"
    flags=(--port-add "$address:${3:-8088}:80@loadbalancer")
    if [[ -n "$fix_port" ]]; then
      flags+=(--port-add "$address:${4:-9878}:$fix_port@loadbalancer")
    fi
    k3d cluster edit "$CLUSTER" "${flags[@]}"
    ;;
  scale)
    half="${2:?usage: deploy/laptop.sh scale public|private REPLICAS}"
    replicas="${3:?usage: deploy/laptop.sh scale public|private REPLICAS}"
    [[ "$half" == public || "$half" == private ]] || { echo "the half is public or private, not $half" >&2; exit 64; }
    helm upgrade "$RELEASE" "$CHART" --reuse-values --set "gateway.split.$half.replicas=$replicas" --wait --timeout 5m
    kubectl rollout status "statefulset/$RELEASE-gateway-$half" --timeout 3m
    ;;
  restart)
    kubectl delete pod "$RELEASE-core-0"
    # rollout status does not apply to OnDelete: wait for the controller's new pod, then for its journal replay.
    until kubectl get pod "$RELEASE-core-0" >/dev/null 2>&1; do sleep 1; done
    kubectl wait --for=condition=Ready "pod/$RELEASE-core-0" --timeout 5m
    ;;
  down)
    k3d cluster delete "$CLUSTER"
    ;;
  *)
    echo "usage: deploy/laptop.sh [up [--split] [--no-fix] [HELM FLAGS...]|web [PORT]|expose ADDRESS [HTTP_PORT] [FIX_PORT]|scale public|private REPLICAS|restart|down]" >&2
    exit 64
    ;;
esac
