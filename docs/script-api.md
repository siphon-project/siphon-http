# Script API

Everything you use from a routing script lives under the `http` namespace:

```python
from siphon import http, log
```

The namespace gives you three decorators for registering handlers —
`@http.route`, `@http.middleware`, `@http.on_startup` — and three objects your
handlers work with — `http.Request`, `http.Response`, and `http.Client`.

Handlers may be sync or async. Prefer `async` for anything that awaits I/O (an
outbound `http.Client` call, a cache lookup); it keeps the loop free.

## Decorators

### `@http.route(path, methods=None)`

Registers a handler for a route. The handler receives a single
[`Request`](#httprequest) and must return a [`Response`](#httpresponse) —
returning anything else (including `None`) is a script error and produces a
`500`.

```python
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

- **`path`** is a pattern with named segments: `/users/{id}`, or a catch-all
  `/static/{*rest}`. Params are extracted, URL-decoded, and exposed on
  `req.path_params`.
- **`methods`** is a list of HTTP method strings; the default is `["GET"]`.
  Methods are upper-cased for you. A route with several methods dispatches to the
  same handler — branch on `req.method`.

### `@http.middleware`

Registers a **request guard**. Middlewares run in registration order *before* the
matched route handler, each receiving the `Request`:

- Return a `Response` to **short-circuit** — the route handler is not called.
- Return `None` to **continue** to the next middleware (or the route handler).

```python
@http.middleware
async def require_token(req):
    if req.header("authorization") != "Bearer s3cr3t":
        return http.Response(status=401, body=b"unauthorized")
    return None            # None → continue to the route handler
```

Typical uses: authentication, IP allow-listing, rate limiting, request logging.

!!! note "Request guard, not a wrapper"
    Middleware today is a **request guard** — it runs before the route and can
    only short-circuit. The wrap-around `(req, call_next)` form that also
    rewrites the *response* is a [roadmap](#roadmap) item; until then, do
    post-processing inside the route handler.

### `@http.on_startup`

Registers a startup hook. It runs once, to completion, after the script loads and
**before any listener accepts**. Use it to preload data, warm caches, or open
shared clients.

```python
@http.on_startup
async def warm():
    log.info("http addon starting up")
```

!!! warning "No `@http.on_shutdown` yet"
    `@http.on_shutdown` is on the [roadmap](#roadmap) — it needs a siphon-side
    shutdown signal exposed to addon tasks. Until then a registered shutdown
    handler is **not** invoked (the runtime warns loudly rather than failing
    silently). Do cleanup elsewhere.

## `http.Request`

The inbound request, passed as the only argument to handlers and middleware.

| Attribute | Type | Description |
|---|---|---|
| `method` | `str` | `"GET"`, `"PUT"`, … |
| `path` | `str` | Request path. |
| `path_params` | `dict[str, str]` | Values extracted from the matched route, URL-decoded. |
| `query_params` | `dict[str, str]` | Parsed query string. |
| `headers` | `dict` | Lowercase-keyed headers. |
| `client` | `str` | Remote socket address as `"ip:port"`. |

| Method | Returns | Description |
|---|---|---|
| `req.body()` | `bytes` | The buffered request body. |
| `req.header(name)` | `str \| None` | A single header, case-insensitive. |

```python
@http.route("/items", methods=["GET"])
async def list_items(req):
    limit = int(req.query_params.get("limit", "100"))
    ...
```

## `http.Response`

The outbound response your handler returns.

```python
http.Response(status=200, headers={"Content-Type": "application/json"}, body=b"{}")
```

| Parameter | Default | Description |
|---|---|---|
| `status` | `200` | HTTP status code. |
| `headers` | `None` | Response headers as a dict. |
| `body` | `None` | `bytes` or `str` (UTF-8 encoded). |

A `Response` also exposes a `.body` property and `.raise_for_status()` — most
useful on the responses you get *back* from an [`http.Client`](#httpclient) call.

## `http.Client`

An outbound HTTP client wrapping a pooled `reqwest::Client`. Two construction
modes:

```python
http.Client("api")                       # named — looks up clients.api in http.yaml
http.Client(base_url="https://example.com",   # inline
            verify="/path/ca.crt",
            cert=("/path/c.crt", "/path/c.key"))
```

Named clients share the pool configured under `clients.<name>` in
[`http.yaml`](configuration.md#clients); prefer them for anything hot. Inline
clients are handy for one-offs.

Constructor keyword arguments:

| Argument | Description |
|---|---|
| `name` | Positional. Look up `clients.<name>` from config. |
| `base_url` | Base URL; relative request paths are joined onto it. |
| `verify` | Path to a custom CA bundle to verify the server certificate. |
| `cert` | Client-cert identity for mTLS — a combined PEM or a `(cert, key)` pair. |
| `timeout_ms` | Per-request timeout in milliseconds. |
| `http2_prior_knowledge` | Start cleartext connections in h2c. |

All request methods are coroutines returning a [`Response`](#httpresponse):

```python
async with http.Client("api") as c:
    resp = await c.get("/v1/things")
    resp.raise_for_status()
```

| Method | Signature |
|---|---|
| `get` | `await c.get(path, *, headers=None)` |
| `post` | `await c.post(path, *, body=None, headers=None)` |
| `put` | `await c.put(path, *, body=None, headers=None)` |
| `patch` | `await c.patch(path, *, body=None, headers=None)` |
| `delete` | `await c.delete(path, *, headers=None)` |

The client supports the `async with` lifecycle (`__aenter__` / `__aexit__`);
`resp.raise_for_status()` raises on a 4xx/5xx.

## A note on state

Python handlers may run across threads (that is what lets siphon scale on
free-threaded CPython). Keep mutable runtime state out of module globals — put it
in Rust (a siphon primitive) or an external store. The
[REST API cookbook](cookbook/rest-api.md) uses a process-local dict purely for
illustration and calls this out.

## Roadmap

- `@http.on_shutdown` — graceful-shutdown hooks. Needs a siphon-side shutdown
  signal exposed to addon tasks; until then a registered handler is not invoked
  (the runtime warns loudly). Do cleanup elsewhere.
- Response-rewriting middleware — the wrap-around `(req, call_next)` form. Today
  middleware is a request guard; post-process inside the route handler.
- Body streaming for large upload/download — v1 buffers whole bodies, capped.
- Live route reload on script hot-reload.
