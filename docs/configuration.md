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
| `listen` | string | Bind address as `host:port` (e.g. `0.0.0.0:8443`, `127.0.0.1:9090`). |
| `tls` | map | Optional. Present → the listener terminates TLS. Absent → plain HTTP. See [TLS](#tls-termination). |
| `max_body_bytes` | int | Optional. Cap on the buffered request body. A larger body is rejected with `413 Payload Too Large`. |
| `request_timeout_ms` | int | Optional. Per-request time budget in milliseconds. |

HTTP/1.1 and HTTP/2 are both served on every listener automatically — there is
no per-listener switch. See [HTTP/2](#http2).

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
```

| Key | Type | Description |
|---|---|---|
| `base_url` | string | Base URL; relative paths passed to the client are joined onto it. |
| `timeout_ms` | int | Optional. Per-request timeout in milliseconds. |
| `verify` | string | Optional. Path to a custom CA bundle (PEM) used to verify the server certificate. |
| `cert` | string / pair | Optional. Client-certificate identity for mutual TLS — a combined PEM, or a `(cert, key)` pair. |
| `pool_size` | int | Optional. Max pooled connections. |
| `http2_prior_knowledge` | bool | Optional. Start cleartext connections directly in HTTP/2 (h2c) instead of HTTP/1.1. TLS uses ALPN regardless. |

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
