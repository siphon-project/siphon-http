# siphon-http on Kubernetes

Manifests for a horizontally-scaled siphon-http deployment.

```bash
# Build + push your siphon --features http image, then:
kubectl create configmap siphon-http-script --from-file=script.py=./your_routes.py
kubectl create secret tls siphon-http-tls --cert=server.crt --key=server.key
kubectl apply -f configmap.yaml -f deployment.yaml -f service.yaml -f hpa.yaml -f pdb.yaml
```

- **configmap.yaml** — `siphon.yaml` + `http.yaml`.
- **deployment.yaml** — 2 replicas, TCP readiness/liveness probes, resource
  requests/limits, config + script + TLS mounts.
- **service.yaml** — ClusterIP on 443 → container 8443.
- **hpa.yaml** — CPU-target autoscale 2→10.
- **pdb.yaml** — keep ≥1 pod during voluntary disruptions.

Notes:
- Aggregate throughput per pod is bounded by the Python handler under the CPython
  GIL (see the top-level README's Performance section) — scale out with replicas,
  or build against free-threaded CPython to scale within a pod.
- The routing script (`@http.route` handlers) is supplied as its own ConfigMap
  (`siphon-http-script`) so it can be updated independently of the addon config.
