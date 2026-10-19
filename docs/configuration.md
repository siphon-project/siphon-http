# Configuration

siphon-http reads its own YAML file, referenced from siphon's main config under
`extensions.http`:

```yaml
# siphon.yaml (the binary's main config)
extensions:
  http: /etc/siphon/http.yaml   # → this addon's own config
script:
  path: /etc/siphon/script.py   # your @http.route handlers
```

The addon config has two top-level sections: `servers` (inbound listeners) and
`clients` (named outbound HTTP clients). Everything is optional except that you
need at least one `server` for anything to listen.

All values support siphon's `${VAR}` / `${VAR:-default}` environment expansion.

## Full example

```yaml
# http.yaml — siphon-http addon config.
servers:
  # Public HTTPS listener.
  - listen: "0.0.0.0:8443"
    tls:
      cert_path: "/etc/siphon/tls/server.crt"
      key_path:  "/etc/siphon/tls/server.key"
      client_ca: "/etc/siphon/tls/client-ca.crt"   # optional mTLS
    max_body_bytes: 65536
    request_timeout_ms: 5000

  # Localhost-only plain-HTTP listener (e.g. health / metrics).
  - listen: "127.0.0.1:9090"

clients:
  api:
    base_url: "https://api.example.com"
    timeout_ms: 5000
    verify: "/etc/siphon/tls/ca.crt"               # optional custom CA
    pool_size: 16
```

## `servers`

A list of listeners. Each entry is one socket; list several to serve, for
example, a public HTTPS endpoint and a localhost-only plain-HTTP endpoint at
once.

| Key | Type | Description |
|---|---|---|
| `listen` | string | Bind address as an IP literal and port (e.g. `0.0.0.0:8443`, `127.0.0.1:9090`, `[::]:8443`). Hostnames are not resolved. |
| `name` | string | Optional. A name for the listener: what a route names to be served on it alone, and what a handler reads as `req.listener.name`. Without one the listener is known by its `listen` value. Two listeners of one name are a load error. See [Routes on one listener](#routes-on-one-listener). |
| `tls` | map | Optional. Present → the listener terminates TLS. Absent → plain HTTP. See [TLS](#tls-termination). |
| `max_body_bytes` | int | Optional. Cap on the buffered request body. A larger body is rejected with `413 Payload Too Large`. |
| `request_timeout_ms` | int | Optional. Per-request time budget in milliseconds. |

HTTP/1.1 and HTTP/2 are both served on every listener automatically — there is
no per-listener switch. See [HTTP/2](#http2).

A `listen` value that is not an IP address and port is rejected when the file
is loaded. Every listener is then bound at startup, before `@http.on_startup`
runs, and only starts accepting once those hooks finish. If any listener cannot
bind (port in use, missing or unreadable TLS file) and the script registered at
least one `@http.route`, siphon exits with an error instead of running with
routes nobody can reach.

### TLS termination

Add a `tls` block to a server to terminate TLS on that socket:

```yaml
servers:
  - listen: "0.0.0.0:8443"
    tls:
      cert_path: "/etc/siphon/tls/server.crt"
      key_path:  "/etc/siphon/tls/server.key"
```

| Key | Type | Description |
|---|---|---|
| `cert_path` | string | Server certificate chain (PEM). |
| `key_path` | string | Server private key (PEM). |
| `client_ca` | string | Optional. A CA bundle (PEM); its presence turns on **mutual TLS** — clients must present a certificate that chains to it. |

### Mutual TLS (mTLS)

Set `tls.client_ca` to require and verify a client certificate:

```yaml
servers:
  - listen: "0.0.0.0:8443"
    tls:
      cert_path: "/etc/siphon/tls/server.crt"
      key_path:  "/etc/siphon/tls/server.key"
      client_ca: "/etc/siphon/tls/client-ca.crt"   # require + verify client certs
```

A handler reads the certificate the client presented from `req.tls`: its
subject, its subject alternative names and its SHA-256 fingerprint. That is
what to tie a peer's identity to, not its source address. With `client_ca`
set the handshake of a client without a certificate of that CA fails, so no
request of such a client reaches a handler.

### Routes on one listener

By default every route is served on every listener. A route that belongs on
one of them, such as an internal interface that must not answer on the public
port, names its listeners:

```yaml
servers:
  - listen: "0.0.0.0:8080"
    name: public
  - listen: "192.0.2.10:8443"
    name: internal
    tls:
      cert_path: "/etc/siphon/tls/server.crt"
      key_path:  "/etc/siphon/tls/server.key"
      client_ca: "/etc/siphon/tls/client-ca.crt"
```

```python
@http.route("/internal/keys", methods=["POST"], listeners=["internal"])
async def keys(req):
    ...
```

On `public` the path answers `404`, as a path no route has. A route that
names a listener the file does not have stops siphon at startup. Middleware
runs on every listener; it can branch on `req.listener.name`.

## `clients`

A map of **named, pooled** outbound HTTP clients. A handler picks one up by name
with `http.Client("api")`, which resolves to `clients.api` here. Each named
client wraps a pooled `reqwest::Client`, so connections are reused across
requests.

```yaml
clients:
  api:
    base_url: "https://api.example.com"
    timeout_ms: 5000
    verify: "/etc/siphon/tls/ca.crt"
    pool_size: 16
    http2_prior_knowledge: false
    follow_redirects: true
```

| Key | Type | Description |
|---|---|---|
| `base_url` | string | Base URL; relative paths passed to the client are joined onto it. |
| `timeout_ms` | int | Optional. Per-request timeout in milliseconds. |
| `verify` | string | Optional. Path to a CA bundle (PEM, every certificate in it) trusted to verify the server certificate, beside the built-in roots. |
| `verify_only` | bool | Optional. Trust the `verify` bundle only, not the built-in roots (an operator PKI, where a public CA must not vouch for a peer). Needs `verify`. |
| `cert` | string / pair | Optional. Client-certificate identity for mutual TLS — a combined PEM, or a `(cert, key)` pair. |
| `pool_size` | int | Optional. Max pooled connections. |
| `http2_prior_knowledge` | bool | Optional. Start cleartext connections directly in HTTP/2 (h2c) instead of HTTP/1.1. TLS uses ALPN regardless. |
| `follow_redirects` | bool | Optional, default `true`: a 3xx is followed to its `Location` (up to 10 hops) inside the request. `false` returns the 3xx to the script as received, `Location` and the other headers included, to relay or follow itself. |

Handlers can also build **inline** clients that don't need a config entry —
`http.Client(base_url=…, verify=…, cert=…)`. Named clients are preferable for
anything hot, because they share a pool. See the
[Script API](script-api.md#httpclient) for the client object itself.

## HTTP/2

Every listener auto-negotiates the protocol:

- **On TLS** — h2 or HTTP/1.1 via ALPN.
- **On cleartext** — h2c (HTTP/2 over cleartext, using the connection preface for
  prior-knowledge) or HTTP/1.1, on the same socket.

There is no per-listener HTTP/2 switch; you don't configure it. Outbound clients
use ALPN on TLS the same way; for cleartext h2c set
`clients.<name>.http2_prior_knowledge: true` (or pass `http2_prior_knowledge=True`
to an inline `http.Client`).

## Environment expansion

Every string value goes through siphon's environment expansion, so you can keep
secrets and per-environment paths out of the file:

```yaml
servers:
  - listen: "0.0.0.0:${HTTP_PORT:-8443}"
    tls:
      cert_path: "${TLS_DIR}/server.crt"
      key_path:  "${TLS_DIR}/server.key"

clients:
  api:
    base_url: "${API_BASE_URL:-https://api.example.com}"
```
