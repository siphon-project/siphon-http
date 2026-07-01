# Deployment & operations

siphon-http is a siphon **addon** — the runnable artifact is a `siphon` binary
built with the `http` feature (composed by a siphon binary that registers the
`namespace` and `task` hooks). You deploy that binary with three inputs mounted
at runtime:

- **`siphon.yaml`** — the binary's main config, with an `extensions.http` entry.
- **`http.yaml`** — this addon's config (`servers` + `clients`) — see
  [Configuration](configuration.md).
- **`script.py`** — your `@http.route` handlers — see
  [Script API](script-api.md).

```yaml
# siphon.yaml
extensions:
  http: /etc/siphon/http.yaml
script:
  path: /etc/siphon/script.py
```

Reference templates for all of the below live under
[`deploy/`](https://github.com/siphon-project/siphon-http/tree/main/deploy) in
the repo.

## Docker

A multi-stage build produces the HTTP-enabled siphon binary and ships it with the
config + script mounted at runtime:

```bash
docker build -f deploy/Dockerfile -t siphon-http .
docker run \
  -v $PWD/siphon.yaml:/etc/siphon/siphon.yaml \
  -v $PWD/http.yaml:/etc/siphon/http.yaml \
  -v $PWD/script.py:/etc/siphon/script.py \
  -p 8443:8443 siphon-http
```

The image's entrypoint runs `siphon -c /etc/siphon/siphon.yaml`; it exposes the
HTTPS and localhost ports the example config listens on.

## Docker Compose

For a local bring-up, mount your `siphon.yaml`, `http.yaml`, script, and TLS
material:

```yaml
# docker-compose.yml
services:
  siphon-http:
    build:
      context: ..
      dockerfile: deploy/Dockerfile
    image: siphon-http:local
    ports:
      - "8443:8443"
      - "127.0.0.1:9090:9090"
    volumes:
      - ./siphon.yaml:/etc/siphon/siphon.yaml:ro
      - ./http.example.yaml:/etc/siphon/http.yaml:ro
      - ./script.py:/etc/siphon/script.py:ro
      - ./tls:/etc/siphon/tls:ro
    restart: unless-stopped
```

## Kubernetes

The [`deploy/k8s/`](https://github.com/siphon-project/siphon-http/tree/main/deploy/k8s)
manifests give a horizontally-scaled deployment:

| Manifest | What it does |
|---|---|
| `configmap.yaml` | `siphon.yaml` + `http.yaml`. |
| `deployment.yaml` | 2 replicas, TCP readiness/liveness probes, resource requests/limits, config + script + TLS mounts. |
| `service.yaml` | ClusterIP on `443` → container `8443`. |
| `hpa.yaml` | CPU-target autoscale, `2 → 10` replicas. |
| `pdb.yaml` | Keep ≥ 1 pod during voluntary disruptions. |

Bring it up:

```bash
# Build + push your `siphon --features http` image, then:
kubectl create configmap siphon-http-script --from-file=script.py=./your_routes.py
kubectl create secret tls siphon-http-tls --cert=server.crt --key=server.key
kubectl apply -f configmap.yaml -f deployment.yaml -f service.yaml -f hpa.yaml -f pdb.yaml
```

The routing script is supplied as its **own** ConfigMap (`siphon-http-script`) so
it can be updated independently of the addon config, and TLS material comes from
a `kubernetes.io/tls` Secret.

### Readiness & liveness

The deployment uses TCP probes against the HTTPS container port — the socket
being open means the listener is accepting. `@http.on_startup` runs to completion
before any listener accepts, so a pod is not "ready" until startup work is done.

## Scaling notes

Aggregate throughput **per pod** is bounded by the per-request Python handler
under the CPython GIL — see [Performance & scaling](performance.md). Two levers:

- **Scale out** — add replicas (the HPA does this on CPU). The Rust request path
  is not the limit, so more pods means more aggregate throughput.
- **Scale up within a pod** — build siphon against **free-threaded CPython**, so
  handlers run on every core inside one process.

## Building the binary

siphon-http rides inside a siphon binary; the `http` feature must be wired into
that binary's build. Build the HTTP-enabled binary with the feature enabled:

```bash
cargo build --release --features http
```

Then run it against your config:

```bash
./siphon -c /etc/siphon/siphon.yaml
```
