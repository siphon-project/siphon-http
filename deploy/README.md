# Deploying siphon-http

siphon-http is a siphon **addon** — the runnable artifact is a `siphon` binary
built with the `http` feature (composed via siphon-sip's `siphon-bin`). These
templates package that binary with a config + routing script.

- **[Dockerfile](Dockerfile)** — multi-stage build of `siphon --features http`.
- **[docker-compose.yml](docker-compose.yml)** — local bring-up; mount your
  `siphon.yaml`, `http.yaml`, script, and TLS material.
- **[http.example.yaml](http.example.yaml)** — the addon config schema
  (`servers` + `clients`).
- **[k8s/](k8s/)** — Deployment + Service + ConfigMap + HPA + PDB.

The `http` feature must be wired into `siphon-bin` first (a siphon-sip change,
mirroring how `smpp` was wired). Until then, build against a branch that carries
it.

## Config layering

```yaml
# siphon.yaml (the binary's main config)
extensions:
  http: /etc/siphon/http.yaml   # → this addon's own config
script:
  path: /etc/siphon/script.py   # your @http.route handlers
```
