# siphon-http

**An HTTP/HTTPS addon for [siphon](https://github.com/siphon-project/siphon-sip).**

siphon-http plugs an `http` namespace into a siphon binary so your Python routing
scripts can serve HTTP requests the same way they handle SIP — and call out over
HTTP from inside the same asyncio loop. Inbound routing, TLS termination, body
buffering, and the outbound connection pool are Rust
([axum](https://github.com/tokio-rs/axum) +
[hyper](https://github.com/hyperium/hyper) +
[reqwest](https://github.com/seanmonstar/reqwest) +
[rustls](https://github.com/rustls/rustls)); your handlers are Python.

```python
from siphon import http

@http.route("/hello/{name}", methods=["GET"])
async def hello(req):
    return http.Response(status=200, body=f"hi {req.path_params['name']}".encode())
```

## What it is

siphon-http is an **addon**, not a standalone server. It rides inside a siphon
binary and shares that binary's asyncio loop, so a single script can answer SIP
*and* HTTP. The parts that are hard to get right — TCP/TLS termination, HTTP/1.1
and HTTP/2 framing, path routing, capped body buffering, outbound connection
pooling — are Rust. The policy — which route does what, auth, content
negotiation, business rules — is Python.

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

## How it composes

Because it is an addon, siphon-http does not run on its own. A composing siphon
binary registers two paired hooks at startup:

- **`namespace`** — exposes `siphon.http` to scripts (the `@http.route`
  decorators, `http.Response`, `http.Client`, …).
- **`task`** — spawns the axum listener(s) and the outbound client pool against
  the routes the script registered.

Both are needed: the namespace alone is inert, the task alone has nothing to
dispatch to. You don't wire these up in your script — a composing siphon binary
registers the `namespace` and `task` hooks for you. All you do is point
siphon's config at an `http.yaml` and write route handlers.

```yaml
# siphon.yaml
extensions:
  http: http.yaml
```

## Quick example

A single route with a path param and a JSON body:

```python
from siphon import http

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
```

Point your siphon build's script path at that file, add a listener in
`http.yaml`, and requests to `/orders/42` dispatch to `order`.

## Where next

- **[Configuration](configuration.md)** — the `http.yaml` schema: listeners,
  TLS/mTLS, timeouts, body caps, HTTP/2, and named outbound clients.
- **[Script API](script-api.md)** — `@http.route`, `@http.middleware`,
  `@http.on_startup`, and the `http.Request` / `http.Response` / `http.Client`
  objects.
- **[Cookbook](cookbook/index.md)** — complete, runnable starting points: a
  [webhook receiver](cookbook/webhook.md), a [REST API](cookbook/rest-api.md),
  and a [reverse proxy](cookbook/proxy.md).
- **[Deployment & operations](deployment.md)** — Docker, Compose, and Kubernetes.
- **[Performance & scaling](performance.md)** — where the throughput ceiling is
  and how free-threaded CPython lifts it.

## License

MIT.
