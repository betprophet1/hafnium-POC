#!/usr/bin/env bash
# The mini venue on a laptop, in one command (blueprint §16): builds the core image, creates a k3d cluster if
# there is none, imports the image, installs the chart and runs its smoke test.
#
#   deploy/laptop.sh up        build, install or upgrade, test
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
    docker build -f "$ROOT/deploy/images/venue-node.Dockerfile" -t "venue-node:$TAG" "$ROOT"
    k3d cluster get "$CLUSTER" >/dev/null 2>&1 || k3d cluster create "$CLUSTER" --wait
    k3d image import "venue-node:$TAG" --cluster "$CLUSTER"
    # On a fresh cluster the smoke test can start before cluster DNS answers.
    kubectl -n kube-system rollout status deployment/coredns --timeout 2m
    helm upgrade --install "$RELEASE" "$CHART" --set image.tag="$TAG" --wait --timeout 5m
    helm test "$RELEASE" --logs
    echo
    echo "kubectl port-forward svc/$RELEASE-core 7101:7101    then: venue-node send 127.0.0.1:7101 inputs.jsonl"
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
    echo "usage: deploy/laptop.sh [up|restart|down]" >&2
    exit 64
    ;;
esac
