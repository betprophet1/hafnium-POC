# Observability

Metrics, logs, dashboards and alerts for the venue (blueprint §14 and §20: Prometheus, Grafana, HdrHistogram for the
distributions it publishes, per-hop histograms derived from the journal). The same pieces run in a laptop's k3d
cluster and plug into managed services by configuration alone.

| Path | What it is |
|---|---|
| [`metrics.md`](metrics.md) | Every metric the venue's processes export, with units and labels; the per-hop latencies measurable today; the log format |
| `versions.env` | The pinned versions of the community charts |
| `values/kube-prometheus-stack.yaml` | Prometheus, Alertmanager, Grafana, the Prometheus Operator and kube-state-metrics, sized for a laptop |
| `values/loki.yaml` | Loki as a single binary on a small volume |
| `values/alloy.yaml` | Grafana Alloy on every node, shipping pods' logs to Loki |
| `install.sh` | Installs all of the above and `deploy/charts/venue-observability` into one namespace |
| `../charts/venue-observability/` | The venue's Grafana data sources, dashboards (`dashboards/`) and alert rules (`templates/rules.yaml`) |

## What runs

In namespace `monitoring` (`OBSERVABILITY_NAMESPACE`):

| Release | Chart | Resources that matter |
|---|---|---|
| `kube-prometheus-stack` | prometheus-community/kube-prometheus-stack | Service `monitoring-prometheus:9090`, `monitoring-alertmanager:9093`, `grafana:80`; Secret `grafana` |
| `loki` | grafana/loki (single binary) | Service `loki:3100` (push `/loki/api/v1/push`) |
| `alloy` | grafana/alloy (DaemonSet) | tails every pod's logs through the Kubernetes API |
| `venue-observability` | `deploy/charts/venue-observability` | ConfigMaps for Grafana's sidecar, PrometheusRule `venue-observability-venue` |

The venue's own processes serve `/metrics` on internal ports named `metrics`, which the venue-mini chart's PodMonitor
scrapes when `metrics.enabled` is on; its network policy `<release>-metrics` lets only the monitoring namespace reach
them. The PodMonitor adds `env`, `profile` and `component` to every series. Each venue container writes JSON log lines
to standard output (`VENUE_LOG_FORMAT=json`); Alloy labels venue streams with `namespace`, `pod`, `container`,
`component`, `level` and `service`, and the rest of each line stays queryable with `| json`.

## Install

Needs Helm 4.3 and kubectl against the cluster (for the laptop, `deploy/laptop.sh up` first):

```sh
deploy/observability/install.sh                               # the stack, then the venue's chart
helm upgrade venue deploy/charts/venue-mini --reuse-values --set metrics.enabled=true   # if not already on
```

Repeating it upgrades in place. Each release can take one more values file of an environment's own, applied after
the repository's: `KPS_VALUES`, `LOKI_VALUES`, `ALLOY_VALUES`, `VENUE_OBSERVABILITY_VALUES`. To remove it all:
`helm -n monitoring uninstall venue-observability alloy loki kube-prometheus-stack`, then delete the namespace and the
`monitoring.coreos.com` CRDs if nothing else uses them.

The laptop setup keeps two days of metrics in the Prometheus pod (lost when it is replaced) and three days of logs on
Loki's 2 GiB volume, uses about 1 GiB of memory, and leaves out node-exporter and the control-plane scrapes k3s does
not expose.

## Exposure is configuration

By default nothing is reachable from outside the cluster. Grafana's admin password is generated on first install and
kept in the Secret `grafana`; no values file holds it.

```sh
kubectl -n monitoring get secret grafana -o jsonpath='{.data.admin-password}' | base64 -d; echo
kubectl -n monitoring port-forward --address 127.0.0.1 svc/grafana 13000:80        # http://127.0.0.1:13000, user admin
kubectl -n monitoring port-forward --address 127.0.0.1 svc/monitoring-prometheus 19090:9090
kubectl -n monitoring port-forward --address 127.0.0.1 svc/monitoring-alertmanager 19093:9093
```

Pick any free local port. To serve Grafana to others on a private network, give it an Ingress in an environment's own
values file (`KPS_VALUES`) and map the ingress port to a private address of the machine, as the venue's page is
(deploy/README.md, on an address of this machine):

```yaml
grafana:
  ingress:
    enabled: true
    ingressClassName: traefik
    hosts: [<grafana-host-name>]
  grafana.ini:
    server:
      root_url: http://<grafana-host-name>:<port>/
```

`<private-address>` and the host name are the operator's to choose: an address of this machine on a private network,
never `0.0.0.0` and never one the public internet reaches. Change the admin password, or put Grafana behind the
organisation's single sign-on (`grafana.ini` `auth.*`), before anyone else reaches it.

## Managed services

Nothing in the venue or these charts is specific to a provider; switching is values.

**Metrics to a managed Prometheus** (Amazon Managed Service for Prometheus, Grafana Cloud Metrics, any remote-write
endpoint): keep the in-cluster Prometheus as the scraper and add a remote write in `KPS_VALUES`. Credentials come
from a Secret in the namespace or, on EKS, from the pod's IAM role (IRSA) for SigV4; never from a values file.

```yaml
prometheus:
  prometheusSpec:
    externalLabels:
      cluster: <cluster-name>
    remoteWrite:
      - url: https://aps-workspaces.<region>.amazonaws.com/workspaces/<workspace-id>/api/v1/remote_write
        sigv4:
          region: <region>
      # or
      - url: https://<prometheus-endpoint>/api/prom/push
        basicAuth:
          username: {name: <secret>, key: username}
          password: {name: <secret>, key: password}
    retention: 6h        # the managed service keeps the history
```

**Logs to a managed Loki** (Grafana Cloud Logs, a Loki elsewhere): install with `OBSERVABILITY_LOKI=0` and point Alloy
at it in `ALLOY_VALUES`, with credentials from a Secret:

```yaml
alloy:
  extraEnv:
    - {name: NODE_NAME, valueFrom: {fieldRef: {fieldPath: spec.nodeName}}}
    - {name: LOKI_WRITE_URL, value: https://<loki-endpoint>/loki/api/v1/push}
    - {name: LOKI_USERNAME, valueFrom: {secretKeyRef: {name: <secret>, key: username}}}
    - {name: LOKI_PASSWORD, valueFrom: {secretKeyRef: {name: <secret>, key: password}}}
    - {name: CLUSTER_NAME, value: <cluster-name>}
```

**Grafana pointed at managed data sources** (still in-cluster): set the data source URLs in
`VENUE_OBSERVABILITY_VALUES` (`datasources.prometheus.url`, `datasources.loki.url`, `jsonData` such as
`{sigV4Auth: true, sigV4AuthType: default, sigV4Region: <region>}`, or `basicAuthUser` with `basicAuthPasswordEnv`
naming an environment variable set on Grafana from a Secret with `grafana.envValueFrom`).

**A managed Grafana** (Amazon Managed Grafana, Grafana Cloud): set `grafana.enabled: false` in `KPS_VALUES` and
`datasources.enabled: false` and `dashboards.enabled: false` in `VENUE_OBSERVABILITY_VALUES`; create its data sources
there, and import the JSON files under `deploy/charts/venue-observability/dashboards/` (through its API or the Grafana
Operator). Every dashboard picks its data sources through its `datasource` and `logs` variables, so the UIDs need not
match. The alert rules stay in-cluster with Prometheus and Alertmanager, or are loaded into the managed service's rule
engine from `helm template` of the chart.

## Dashboards

In Grafana, folder **System**: *Venue overview* (every component up, restarts, firing alerts, errors, saturation,
capacity headroom), *Gateways* (connections, sessions, REST by route, WebSocket ops, refusals, flood limits, event loop
turns, outbound backlog, feed lag, audit writer, upstream calls, logs), *Core and journal* (inputs by type, journal
append and fsync, feed subscribers and backlog, capacity used against limits), *FIX adapter*, *Kubernetes resources*
(CPU, memory, restarts, volumes), *Services* (treasury, identity, parlays and the oracle as the gateway and the journal
see them, with placeholders for their own metrics, which come next), and *Latency (per hop)*. Folder **Business**:
*Trading* (volume, notional, fills, top markets, order-to-trade ratio, rejections and cancels), *Market quality*
(spread, depth at the touch, time two-sided), *Fees and rebates*, *Lifecycle* (markets and events by state, listing and
resolution rates, the oracle's cadence as the journal saw it).

The latency dashboard never plots a series labelled `profile="functional"` (blueprint §16): an environment on shared
cores that sleep between messages cannot produce a latency number, and the dashboard says so instead of drawing one.
Operational timings on the system dashboards (event loop turns, journal fsync) are there to find saturation, not to
quote.

Dashboards are files in the chart; Grafana shows them read-only. Change the JSON and upgrade the release.

## Alerts

`deploy/charts/venue-observability/templates/rules.yaml`, thresholds in the chart's values (`rules.*`):

| Alert | Fires when |
|---|---|
| `VenueComponentDown` | a core, gateway, observer or FIX pod fails its scrape for 1 min |
| `VenueComponentAbsent` | no core, gateway or observer target exists for 2 min (scaled to zero, unschedulable) |
| `VenueContainerRestarted` | a venue container restarted in the last 15 min (a gateway exits when its audit log fails) |
| `VenueGatewayServerErrors` | more than 5% of a half's REST answers are 5xx for 5 min |
| `VenueGatewayRefusalSpike` | more than 25% of a half's requests are refused (auth, permission, flood, unavailable) for 5 min |
| `VenueFeedBehind` | a gateway or the observer is more than 5 s behind the journal for 2 min |
| `VenueGatewayAuditQueueBackingUp`, `VenueGatewayAuditQueueFull` | an audit queue holds over 10,000 records, or an event loop waited on a full one |
| `VenueJournalFsyncSlow`, `VenueAuditFsyncSlow` | journal or audit fsync p99 above 50 ms |
| `VenueCapacityHigh` | any engine table or sequenced listing limit above 80% for 5 min |
| `VenueFixLogonRefusalSpike`, `VenueFixAuditQueueBackingUp` | more than 10 FIX logons refused in 5 min; the FIX audit queue backing up |
| `VenueVolumeFilling` | a volume above 80% full (k3d's local-path volumes report no usage) |

Alertmanager is installed with no receiver: alerts show in Alertmanager and in Grafana's alerting view and go nowhere
else. Receivers are configuration, in an environment's `KPS_VALUES`, with secrets from Secrets:

```yaml
alertmanager:
  config:
    route:
      receiver: "null"
      routes:
        - matchers: ['severity = critical']
          receiver: on-call
    receivers:
      - name: "null"
      - name: on-call
        pagerduty_configs:
          - routing_key_file: /etc/alertmanager/secrets/<secret>/routing-key
  alertmanagerSpec:
    secrets: [<secret>]
```

## Licences

Prometheus, Alertmanager, the Prometheus Operator and kube-state-metrics are Apache-2.0. **Grafana, Loki and Alloy are
AGPL-3.0.** Running them unmodified, as these charts do, puts no obligation on the venue's own code, which only talks to
them over their APIs; modifying them and offering the modified version to others over a network would oblige the
venue to publish those modifications. Managed offerings are covered by the provider's terms instead. This is the
engineering reading; counsel should confirm it before production.
