#!/usr/bin/env bash
# Installs the observability stack into a namespace of the current kubectl context (deploy/observability/README.md):
# kube-prometheus-stack, Loki and Alloy at the versions pinned in versions.env, then the venue's own chart
# (deploy/charts/venue-observability: data sources, dashboards, alert rules). Safe to repeat; each step is a
# `helm upgrade --install` that waits for its pods.
#
#   deploy/observability/install.sh
#
# Environment:
#   OBSERVABILITY_NAMESPACE   namespace to install into (default monitoring; the venue-mini chart's
#                             metrics.monitoringNamespace must name the same one)
#   OBSERVABILITY_LOKI=0      skip Loki (logs go to a Loki elsewhere: set Alloy's LOKI_WRITE_URL)
#   OBSERVABILITY_ALLOY=0     skip Alloy (logs collected some other way)
#   KPS_VALUES, LOKI_VALUES, ALLOY_VALUES, VENUE_OBSERVABILITY_VALUES
#                             extra values files for that release, applied after the repository's own: an
#                             environment's remote write, receivers, data source URLs, retention or exposure.
#                             Several are separated by colons as in PATH, and the last to set a key wins it:
#                             KPS_VALUES=/tmp/grafana.yaml:deploy/observability/values/prometheus-history.yaml
#   HELM_TIMEOUT              per release (default 10m)
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
# shellcheck source=versions.env
source "$HERE/versions.env"

NS="${OBSERVABILITY_NAMESPACE:-monitoring}"
TIMEOUT="${HELM_TIMEOUT:-10m}"

# The extra values for a release, as `--values` arguments: one path, or several separated by colons as in PATH.
# Several because the settings that vary between clusters are not one subject — exposure, retention and remote write
# are decided separately and belong in separate files — and a release only gets one of these variables. Helm applies
# them left to right after the repository's own, so the last file to set a key wins it.
extra() {
  local paths="${1:-}" path
  while [[ -n "$paths" ]]; do
    path="${paths%%:*}"
    if [[ "$path" == "$paths" ]]; then paths=""; else paths="${paths#*:}"; fi
    [[ -n "$path" ]] && printf -- '--values\n%s\n' "$path"
  done
}

# Said before anything is installed, because `mapfile < <(extra ...)` cannot fail the script from inside a process
# substitution: a values file named but missing would otherwise be left out silently and the release would come up
# without it, which is the one outcome worse than stopping.
missing=""
for paths in "${KPS_VALUES:-}" "${LOKI_VALUES:-}" "${ALLOY_VALUES:-}" "${VENUE_OBSERVABILITY_VALUES:-}"; do
  while [[ -n "$paths" ]]; do
    path="${paths%%:*}"
    if [[ "$path" == "$paths" ]]; then paths=""; else paths="${paths#*:}"; fi
    [[ -n "$path" && ! -f "$path" ]] && missing="$missing  $path"$'\n'
  done
done
if [[ -n "$missing" ]]; then
  printf 'no values file at:\n%s' "$missing" >&2
  exit 1
fi

step() { printf '\n== %s\n' "$*"; }

step "Chart repositories"
helm repo add prometheus-community https://prometheus-community.github.io/helm-charts --force-update >/dev/null
helm repo add grafana https://grafana.github.io/helm-charts --force-update >/dev/null
helm repo update prometheus-community grafana >/dev/null

kubectl get namespace "$NS" >/dev/null 2>&1 || kubectl create namespace "$NS"

step "kube-prometheus-stack $KUBE_PROMETHEUS_STACK_VERSION"
mapfile -t kps_extra < <(extra "${KPS_VALUES:-}")
helm upgrade --install kube-prometheus-stack prometheus-community/kube-prometheus-stack \
  --namespace "$NS" --version "$KUBE_PROMETHEUS_STACK_VERSION" \
  --values "$HERE/values/kube-prometheus-stack.yaml" "${kps_extra[@]}" \
  --wait --timeout "$TIMEOUT"

if [[ "${OBSERVABILITY_LOKI:-1}" != "0" ]]; then
  step "Loki $LOKI_VERSION"
  mapfile -t loki_extra < <(extra "${LOKI_VALUES:-}")
  helm upgrade --install loki grafana/loki \
    --namespace "$NS" --version "$LOKI_VERSION" \
    --values "$HERE/values/loki.yaml" "${loki_extra[@]}" \
    --wait --timeout "$TIMEOUT"
fi

if [[ "${OBSERVABILITY_ALLOY:-1}" != "0" ]]; then
  step "Alloy $ALLOY_VERSION"
  mapfile -t alloy_extra < <(extra "${ALLOY_VALUES:-}")
  helm upgrade --install alloy grafana/alloy \
    --namespace "$NS" --version "$ALLOY_VERSION" \
    --values "$HERE/values/alloy.yaml" "${alloy_extra[@]}" \
    --wait --timeout "$TIMEOUT"
fi

step "venue-observability (data sources, dashboards, alert rules)"
mapfile -t venue_extra < <(extra "${VENUE_OBSERVABILITY_VALUES:-}")
helm upgrade --install venue-observability "$ROOT/deploy/charts/venue-observability" \
  --namespace "$NS" "${venue_extra[@]}" \
  --wait --timeout "$TIMEOUT"

step "Done"
cat <<EOF
Nothing is exposed outside the cluster. To open Grafana from this machine:
  kubectl -n $NS get secret grafana -o jsonpath='{.data.admin-password}' | base64 -d; echo
  kubectl -n $NS port-forward --address 127.0.0.1 svc/grafana 13000:80     # http://127.0.0.1:13000, user admin
The venue's metrics appear once its release runs with metrics.enabled (deploy/README.md, observability).
EOF
