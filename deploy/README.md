# Deploy

The mini venue of blueprint §16: one Helm chart, run on a laptop under k3d and, later, in ephemeral namespaces
through ArgoCD. There is no compose file; the laptop runs the same chart.

| Path | What it is |
|---|---|
| `images/venue-node.Dockerfile` | The core's image: `venue-node` from `core/node`, built from the repository root |
| `charts/venue-mini/` | The chart. So far the core: one StatefulSet (one sequencer group, one node), its journal volume, a Service and a `helm test` smoke test |
| `laptop.sh` | `up`, `restart` and `down` for a local k3d cluster |

## On a laptop

Needs Docker, [k3d](https://k3d.io) 5.9, Helm 4.3 and kubectl 1.37.

```sh
deploy/laptop.sh up
kubectl port-forward svc/venue-core 7101:7101
core/target/release/venue-node send 127.0.0.1:7101 inputs.jsonl
```

`up` is safe to repeat. After rebuilding the image, `deploy/laptop.sh restart` replaces the core pod, which
replays its journal before it is ready. `down` deletes the cluster and the journal with it.

## What the chart does and does not do

- **One writer.** `replicas: 1` with `updateStrategy: OnDelete`: a sequencer group never rolls on its own.
- **No liveness probe.** A fail-stop (spec §18) exits the process; the restart replays to the same stop and shows
  as `CrashLoopBackOff`, which is the signal to look at the journal (`venue-node dump`).
- **Not production.** Production runs the core on dedicated hosts behind Aeron Cluster (blueprint §5, §9). The
  ingress has no authentication; never expose the Service outside the cluster.
- **Not yet in the chart:** the gateway, bots, dummy oracle and environment manifest of §16.
