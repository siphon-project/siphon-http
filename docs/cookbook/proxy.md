# Reverse proxy

Forward inbound requests to an upstream API over `http.Client`. This is the
outbound side: a named, pooled client (configured under `clients.api`) is reused
across requests, and a catch-all route forwards the remaining path to the
upstream and relays the response.

This mirrors
[`examples/proxy.py`](https://github.com/siphon-project/siphon-http/blob/main/examples/proxy.py).

## Script

```python
from siphon import http, log


@http.route("/proxy/{*rest}", methods=["GET"])
async def forward(req):
    path = "/" + req.path_params["rest"]
    async with http.Client("api") as c:
        upstream = await c.get(path)
    log.info(f"proxied {path} -> {upstream.status}")
    content_type = upstream.headers.get("content-type", "application/octet-stream")
    return http.Response(
        status=upstream.status,
        headers={"Content-Type": content_type},
        body=upstream.body,
    )
```

## Listener + client

The route needs an inbound listener **and** a named outbound client:

```yaml
# http.yaml
servers:
  - listen: "127.0.0.1:8080"

clients:
  api:
    base_url: "https://api.example.com"
    timeout_ms: 5000
```

## Try it

```bash
curl -sS 127.0.0.1:8080/proxy/v1/status
```

That forwards to `https://api.example.com/v1/status` and relays the upstream
status, content type, and body back to the caller.

## How it works

- **Catch-all route.** `/proxy/{*rest}` captures everything after `/proxy/` into
  `req.path_params["rest"]`. See
  [Script API → `@http.route`](../script-api.md#httproutepath-methodsnone).
- **Named, pooled client.** `http.Client("api")` resolves to `clients.api` in the
  config and reuses a pooled connection; `base_url` means the handler passes only
  the relative path. See
  [Configuration → clients](../configuration.md#clients) and
  [Script API → `http.Client`](../script-api.md#httpclient).
- **Relaying the response.** The upstream `Response` exposes `.status`,
  `.headers`, and `.body`, which the handler copies into its own `http.Response`.

!!! note "Response rewriting happens in the handler"
    Because middleware is a [request guard](../script-api.md#httpmiddleware)
    today, any response rewriting — header stripping, body transforms — is done
    inline in the route handler, as above.
