#!/usr/bin/env bash
# The mini venue on a laptop, in one command (blueprint §16): builds the image, creates a k3d cluster if there is
# none, imports the image, installs the chart and runs its smoke test.
#
#   deploy/laptop.sh up        build, install or upgrade, test
#   deploy/laptop.sh web       port-forward the web page to http://127.0.0.1:8080
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
    docker build -f "$ROOT/deploy/images/venue-node.Dockerfile" -t "venue:$TAG" "$ROOT"
    k3d cluster get "$CLUSTER" >/dev/null 2>&1 || k3d cluster create "$CLUSTER" --wait
    k3d image import "venue:$TAG" --cluster "$CLUSTER"
    # On a fresh cluster the smoke test can start before cluster DNS answers.
    kubectl -n kube-system rollout status deployment/coredns --timeout 2m
    helm upgrade --install "$RELEASE" "$CHART" --set image.tag="$TAG" --wait --timeout 5m
    # A rebuilt image with the same tag reaches pods only when they are replaced. The core is OnDelete, so it would
    # otherwise keep the old build indefinitely; replacing it here is safe on a laptop because it replays its journal.
    "$0" restart
    kubectl rollout restart deployment -l "app.kubernetes.io/instance=$RELEASE"
    kubectl rollout status deployment -l "app.kubernetes.io/instance=$RELEASE" --timeout 3m
    helm test "$RELEASE" --logs
    echo
    echo "deploy/laptop.sh web    then open http://127.0.0.1:8080"
    ;;
  web)
    kubectl port-forward "svc/$RELEASE-web" 8080:8080
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
    echo "usage: deploy/laptop.sh [up|web|restart|down]" >&2
    exit 64
    ;;
esac
