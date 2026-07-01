# siphon-http

An **HTTP/HTTPS addon for [siphon](https://github.com/siphon-project/siphon-sip)**.
It plugs an `http` namespace into a siphon binary so your Python routing scripts
can serve HTTP requests the same way they handle SIP — and call out over HTTP
from inside the same asyncio loop. Inbound routing, TLS termination, body
buffering, and the outbound connection pool are Rust ([axum](https://github.com/tokio-rs/axum)
+ [hyper](https://github.com/hyperium/hyper) + [reqwest](https://github.com/seanmonstar/reqwest)
+ [rustls](https://github.com/rustls/rustls)); your handlers are Python.

📖 **Documentation: [http.siphon-sip.org](https://http.siphon-sip.org)** — overview,
configuration, the script API, a cookbook, deployment, and performance.

```python
from siphon import http

@http.route("/hello/{name}", methods=["GET"])
async def hello(req):
    return http.Response(status=200, body=f"hi {req.path_params['name']}".encode())
```

## How it composes

siphon-http is an **addon**, not a standalone server. A composing siphon binary
registers two paired hooks at startup:

- **`namespace`** — exposes `siphon.http` to scripts (the `@http.route`
  decorators, `http.Response`, `http.Client`, …).
- **`task`** — spawns the axum listener(s) + outbound client pool against the
  routes the script registered.

Both are needed: the namespace alone is inert, the task alone has nothing to
dispatch to. Configuration lives in a separate YAML file referenced from
siphon's main config:

```yaml
# siphon.yaml
extensions:
  http: http.yaml
```

## Coverage

### Server (`@http.route`)

| Feature | Status |
|---|---|
| Methods (GET/POST/PUT/PATCH/DELETE/…) | ✅ per-route `methods=[…]` |
| Path params — `/users/{id}`, catch-all `/static/{*rest}` | ✅ extracted + URL-decoded into `req.path_params` |
| Query params | ✅ `req.query_params` |
| Headers (case-insensitive) | ✅ `req.headers`, `req.header(name)` |
| Request body (buffered, capped) | ✅ `req.body()`, `max_body_bytes` → 413 |
| Request timeout | ✅ `request_timeout_ms` → 504 |
| Multiple listeners (e.g. public HTTPS + localhost HTTP) | ✅ `servers: [ … ]` |
| TLS termination | ✅ `tls: { cert_path, key_path }` |
| Mutual TLS (client-cert) | ✅ `tls.client_ca` |
| HTTP/2 | ✅ auto — h2c (cleartext, preface prior-knowledge) + h2 via ALPN on TLS, HTTP/1.1 on the same socket |
| Client address | ✅ `req.client` (left-most `X-Forwarded-For` when the peer is a `trusted_proxies` IP) |
| `@http.middleware` request guards | ✅ run before the route; return a `Response` to short-circuit |
| `@http.on_startup` | ✅ run to completion before any listener accepts |
| `@http.on_shutdown` | ⏳ roadmap — needs a siphon shutdown hook for addon tasks (see below) |

### Client (`http.Client`)

| Feature | Status |
|---|---|
| GET / POST / PUT / PATCH / DELETE | ✅ coroutines returning `http.Response` |
| Named, pooled clients from config | ✅ `http.Client("api")` → `clients.api` |
| Inline clients | ✅ `http.Client(base_url=…, verify=…, cert=…)` |
| Base-URL join | ✅ relative paths joined onto `base_url` |
| Custom CA / server-cert verification | ✅ `verify=` / `clients.<n>.verify` |
| Mutual TLS identity | ✅ `cert=(cert, key)` or combined PEM |
| Connection pooling | ✅ pooled `reqwest::Client` |
| HTTP/2 | ✅ ALPN on TLS; `http2_prior_knowledge` (arg or `clients.<n>.http2_prior_knowledge`) for cleartext h2c |
| `async with` lifecycle | ✅ `__aenter__` / `__aexit__` |
| `resp.raise_for_status()` | ✅ |

## Config (`http.yaml`)

```yaml
servers:
  - listen: "0.0.0.0:8443"
    tls:
      cert_path: "/etc/siphon/tls/server.crt"
      key_path:  "/etc/siphon/tls/server.key"
      client_ca: "/etc/siphon/tls/client-ca.crt"   # optional mTLS
    max_body_bytes: 65536
    request_timeout_ms: 5000
  - listen: "127.0.0.1:9090"                        # plain HTTP, localhost only

clients:
  api:
    base_url: "https://api.example.com"
    timeout_ms: 5000
    verify: "/etc/siphon/tls/ca.crt"                # optional custom CA
    pool_size: 16
```

Values support siphon's `${VAR}` / `${VAR:-default}` environment expansion.

## Script API

```python
from siphon import http, log


# ── Request guard (runs before every route) ─────────────────────────────
@http.middleware
async def require_token(req):
    if req.header("authorization") != "Bearer s3cr3t":
        return http.Response(status=401, body=b"unauthorized")
    return None            # None → continue to the route handler


# ── Preload before listeners accept ─────────────────────────────────────
@http.on_startup
async def warm():
    log.info("http addon starting up")


# ── Route ───────────────────────────────────────────────────────────────
@http.route("/orders/{id}", methods=["GET", "DELETE"])
async def order(req):
    oid = req.path_params["id"]
    if req.method == "DELETE":
        return http.Response(status=204)
    return http.Response(
        status=200,
        headers={"Content-Type": "application/json"},
        body=f'{{"id": "{oid}"}}'.encode(),
    )


# ── Outbound call ───────────────────────────────────────────────────────
@http.route("/proxy/{*rest}", methods=["GET"])
async def proxy(req):
    async with http.Client("api") as c:
        upstream = await c.get("/" + req.path_params["rest"])
    return http.Response(status=upstream.status, body=upstream.body)
```

## Boundary: Rust vs. Python

| Concern | Lives in |
|---|---|
| TCP / TLS termination | Rust |
| HTTP/1.1 + HTTP/2 framing (axum + hyper) | Rust |
| Path routing + path-param extraction | Rust |
| Body buffering (capped) | Rust |
| Connection pooling for outbound | Rust |
| Request handler dispatch | Python |
| Auth, content negotiation, business rules | Python |
| Request/response body parsing | Python |

## Performance

Two layers, benched separately.

**Per-request Rust work** (`cargo bench`, [`benches/parse.rs`](benches/parse.rs)) —
the wire and TLS paths are axum/hyper/rustls; these cover the work this crate
adds. Indicative single-core numbers:

| Path | Time |
|---|---|
| path-param extraction (`/users/{id}/orders/{order}`) | ~135 ns |
| query parsing (4 params) | ~255 ns |
| percent-decode | ~35 ns |
| `http.yaml` parse (boot / hot-reload) | ~5.5 µs |

A counting-allocator [leak check](examples/leak_check.rs)
(`./scripts/mem_leak_test.sh`) hammers these and asserts **live bytes stay flat**
(Δ 0 over 200k cycles). Both run in CI.

**End-to-end** (the [`harness/`](harness/)) — against its in-process mock (no
siphon), the driver + loopback sustain **~270k req/s at sub-100 µs p50**; that's
the driver ceiling, not a real server. Against a live `siphon --features http`,
aggregate throughput is bounded by the **per-request Python handler dispatch**
under the CPython GIL: it serializes to roughly one core regardless of
connections or cores — the Rust request path is not the limit. Keep per-request
handler work minimal (push heavy lifting into Rust) and — the real unlock — run
siphon against **free-threaded CPython** (3.13t / 3.14t), where handlers run on
every core and aggregate scales. (Same scaling characteristic as any
Python-in-the-loop siphon addon.)

## Deploying on Kubernetes

siphon-http serves in-process, so a pod runs one siphon process with the addon's
listener(s). Three levers scale it, in order of reach:

- **Per-core (in-pod).** Handlers run in CPython, so on a stock (GIL) interpreter
  request dispatch serializes to ~one core regardless of the pod's CPU limit. Run
  on **free-threaded CPython (3.13t / 3.14t)** and handlers spread across every
  core — see [Performance](#performance). Size CPU requests/limits to the cores
  you actually want handlers to use.
- **Horizontal.** Run N replicas behind a Service and drive an **HPA off CPU**
  (dispatch is CPU-bound, not I/O-bound). Expose a lightweight route and wire it
  to a `readinessProbe` so rollouts and scale-ups don't blackhole traffic.
- **HTTP/2 load-balancing.** h2 connections are long-lived and multiplexed, so an
  L4 Service pins every stream of a connection to one pod. Balance **per-request**
  with an L7 mesh (Envoy/Istio) or a headless Service with client-side LB. (In a
  5G core this is the SCP's job.)

**State.** Handler state in Python is process-local (per pod). Anything that must
be shared across replicas — a registry, a cache, a session table — belongs in a
siphon primitive (Rust) or an external store, not a module-level dict. The
[`examples/`](examples/) that keep state in a dict are single-replica as written
and say so.

**TLS.** Terminate TLS in the addon (rustls: `tls.cert_path` / `key_path`, plus
`tls.client_ca` for mutual-TLS east-west) or at an ingress; both compose with the
levers above.

## Roadmap

- **`@http.on_shutdown`** — a script-level teardown hook, the counterpart to
  `@http.on_startup`, run once when the server begins graceful shutdown
  (SIGTERM/SIGINT). It's for cleanup that has to run *in Python* before the
  process exits — e.g.:
    - **deregister from a service registry** — a 5G NF sending its `DELETE` to
      the NRF so it doesn't leave a stale entry (see
      [`examples/nrf_5g.py`](examples/nrf_5g.py));
    - flush a pending write buffer / batch, or drain an in-memory queue to
      durable storage;
    - close a database or session pool opened in `@http.on_startup`; release a
      lease or distributed lock;
    - emit a final metric, audit record, or "going away" log line.

  Why it needs siphon support (and can't just be a signal handler): Python's
  `signal` module only works on the main thread, but siphon runs script handlers
  on worker threads — so a script can't install its own SIGTERM handler (it
  raises `ValueError`). Only siphon, which owns the signal, can hand the script a
  shutdown callback. Not wired yet (needs a siphon-side shutdown signal exposed
  to addon tasks); a registered handler is not invoked and the runtime warns
  loudly rather than failing silently. Until then, put must-run-on-exit cleanup
  in the Rust layer, or handle it via your orchestrator (a k8s `preStop` hook /
  termination grace period). Graceful connection draining is separate and can be
  left to k8s readiness + grace period.
- Response-rewriting middleware (the wrap-around `(req, call_next)` form). Today
  middleware is a request guard; post-process inside the route handler.
- Body streaming for large upload/download (v1 buffers whole bodies, capped).
- Live route reload on script hot-reload.

## Development

```bash
cargo build
cargo clippy --all-targets -- -D warnings
cargo test
cargo bench --no-run
./scripts/mem_leak_test.sh   # live-bytes leak check (PASS/FAIL)
cargo deny check             # advisories, licenses, bans, sources
```

## Dependencies

- **[siphon](https://github.com/siphon-project/siphon-sip)** (`siphon-sip`) — the
  host platform. Pinned to a git revision for now (PyO3 0.29; the pin must track
  siphon-sip's, since both link the `python` native library and Cargo allows
  only one version of a `links` crate per graph).
- **axum / hyper / tower-http** — HTTP server. **reqwest** — outbound client.
  **rustls** — TLS. All MIT/Apache-2.0.

## License

MIT — see [LICENSE](LICENSE).
