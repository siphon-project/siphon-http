# Changelog

All notable changes to `siphon-http` are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [1.2.0] — 2026-10-15

### Added

- **`req.tls`: the TLS session of the connection a request arrived on.** `None`
  on a cleartext listener. Otherwise `version` (`"TLSv1.3"`), `cipher_suite`
  (the IANA registry name, `"TLS_AES_128_GCM_SHA256"`), `cipher_suite_id`
  (`0x1301`), `cipher_suite_bytes` (the two octets), `server_name` (SNI),
  `alpn`, and the client's certificate chain as `peer_certificate` /
  `peer_certificates`, each with `subject` and `issuer` (RFC 4514),
  `subject_alt_names` (`(kind, value)` tuples), `fingerprint_sha256` and
  `der`. `peer_verified` says whether a chain verified against `client_ca`. A
  handler can now bind a key to the negotiated cipher suite and a peer to the
  name in its certificate, where before it had the source address only.
- **`req.listener`: the listener a request arrived on**, with `name`, `address`
  (the local end of the connection), `port` and `tls`.
- **`servers[].name` and `@http.route(..., listeners=[...])`.** A route that
  names listeners is served on those alone and answers `404` on any other, as
  a path no route has. Without `listeners` a route is served on every listener,
  as before. A route naming a listener that is not configured stops siphon at
  startup; two listeners of one name are a load error.
- **`req.peer`: the TCP peer of the connection** as `"ip:port"`. Unlike
  `req.client` it never follows `X-Forwarded-For`, so behind a trusted proxy
  it still names the host that completed the handshake.
- **`req.host`**: the `:authority` of an HTTP/2 request, or else the `Host`
  header. An HTTP/2 request has no `Host` header in `req.headers`.

### Changed

- `ServerConfig` has a new field, `name`; a struct literal needs it.
  `ConfigError` has a new `ListenerName` variant; an exhaustive `match` on it
  needs an extra arm.
- The options a route registers hold a `listeners` key next to `path` and
  `methods`.

### Fixed

- **The namespace no longer hides the standard library's `http` package.** The
  module was created under the name `http`, which put it in `sys.modules` in
  place of the standard library package for the whole interpreter. Any script
  module that imported `http.client`, `urllib.request`, `xml.sax.saxutils` or
  another user of them failed with `No module named 'http.client'; 'http' is
  not a package`. The module is now named `siphon.http` and is registered
  under that name only. `from siphon import http` is unchanged. A bare
  `import http` in a script, which was never documented, now gives the
  standard library package and not the namespace.

### Security

- **rustls 0.23.46** for RUSTSEC-2026-0346 (`Debug` of client session values
  printed TLS secrets), RUSTSEC-2026-0347 (application data sent to the outer
  server after an ECH rejection), RUSTSEC-2026-0348 (brotli certificate
  decompression could allocate up to 1 GiB before authentication) and
  RUSTSEC-2026-0349 (an unauthenticated ServerHello could downgrade later
  connections to classical key exchange).

## [1.1.0] — 2026-09-25

### Fixed

- **`${VAR}` / `${VAR:-default}` expansion in `http.yaml` now actually happens.**
  The docs promised it but the loader handed the raw text to the YAML parser,
  so e.g. `listen: "0.0.0.0:${HTTP_PORT:-8443}"` failed at bind. The file is now
  expanded with siphon's own expander (`siphon::config::expand_env_vars`), so
  the rules are exactly those of `siphon.yaml`.
- **A listener that cannot bind no longer leaves the process running without
  it.** Listeners are now bound at startup, before `@http.on_startup`, instead
  of in a background task after it. If one fails (port in use, missing TLS
  file) and the script registered routes, the process exits with an error,
  matching how siphon treats a SIP listener it cannot bind. Previously this was
  one `listener failed` log line while the routes sat unreachable behind an
  otherwise healthy process. Sockets still accept nothing until the startup
  hooks finish, so TCP readiness probes keep their meaning.
- **`servers[].listen` is validated at config load.** A value that is not an IP
  address and port is a `ConfigError::Listen` naming the entry, rather than a
  runtime `invalid socket address syntax`.

### Changed

- `ConfigError` has a new `Listen` variant; an exhaustive `match` on it needs
  an extra arm.
- `siphon-sip` moves to current `main` for `siphon::config::expand_env_vars`.

### Security

- **Cargo.lock refresh** for RUSTSEC-2026-0258 (h2 0.4.19, unbounded empty
  DATA frames) and RUSTSEC-2026-0285 (rustls 0.23.45, TLS 1.3 handshake
  messages accepted across encryption levels), plus the yanked chacha20 0.10.1.

## [1.0.2] — 2026-08-18

### Added

- **WhatsApp Cloud API example** (`examples/whatsapp_cloud_api.py`) + cookbook
  recipe: a messaging bridge built entirely on the addon — outbound Graph POSTs
  over a pooled `http.Client("graph")`, and the inbound webhook (GET verify
  handshake + POST events, with `X-Hub-Signature-256` HMAC verified over the raw
  `req.body()`) on `@http.route`, with message-id dedup via the `cache` namespace.
  No addon code — it is a worked script showing the client + server halves
  together.

### Changed

- **TLS certificate and key loading moved off `rustls-pemfile`** onto the PEM
  decoder built into `rustls-pki-types` (`PemObject`), dropping a dependency
  whose upstream repository has been archived since August 2025 (RUSTSEC-2025-0134
  — unmaintained, no vulnerability). The crate was already a thin wrapper over
  the same parsing code, so loading behaviour is unchanged: a cert file's
  non-certificate sections are still skipped, and a key is still accepted as
  PKCS#8, PKCS#1 or SEC1. A malformed PEM now reports `InvalidInput` rather than
  `InvalidData` and names the offending file in the message; nothing matches on
  the kind, so this only affects what gets logged. This also lets the advisory
  ignore be removed from `deny.toml` rather than carried indefinitely, and clears
  the last audit failure for binaries composing this addon.

## [1.0.1] — 2026-07-01

### Added

- **SDK testing support for HTTP scripts** — the `siphon-sip` SDK now mocks the
  `http` namespace, so scripts can be unit-tested with `HttpTestHarness` (route +
  middleware dispatch, canned outbound `http.Client` responses) and authored with
  full type hints/docstrings via `pip install siphon-sip` (no listener). Documented
  under **Testing your scripts** in the script API reference. A CI parity check
  (`scripts/check_sdk_parity.py`) fails the build if the mock drifts from the
  runtime `http` surface.

## [1.0.0] — 2026-07-01

First open-source release — an HTTP/HTTPS addon for
[siphon](https://github.com/siphon-project/siphon-sip) that lets routing scripts
serve and call HTTP from the same asyncio loop they use for SIP. Built on
axum + hyper + reqwest + rustls.

### Composition

- `namespace(cfg)` + `task(cfg)` hooks that plug an `http` Python namespace and a
  tokio HTTP runtime into a composing siphon binary.
- YAML configuration (`HttpConfig`) referenced from siphon's main config under
  `extensions.http`, with `${VAR}` / `${VAR:-default}` expansion.

### Server (`@http.route`)

- Path + method routing (axum 0.8), path params (`{name}`, catch-all `{*rest}`,
  URL-decoded), query params, case-insensitive headers, capped request bodies,
  multiple listeners, TLS termination, and mutual TLS.
- **HTTP/2**: listeners auto-negotiate — h2c on cleartext (preface prior-knowledge)
  and h2 via ALPN on TLS, HTTP/1.1 on the same socket; no per-listener switch.
- `@http.middleware` request guards — run in registration order before the route
  handler; return a `Response` to short-circuit, `None` to continue.
- `@http.on_startup` — run to completion before any listener accepts.

### Client (`http.Client`)

- GET/POST/PUT/PATCH/DELETE coroutines returning `http.Response`.
- Named, pooled clients from config (`http.Client("api")`) and inline clients
  (`base_url=`, `verify=`, `cert=`), custom-CA verification, mutual-TLS identity,
  base-URL join, and `async with` lifecycle. HTTP/2 via ALPN on TLS, or
  `http2_prior_knowledge` for cleartext h2c.

### Quality & ops

- Criterion benches (`benches/parse.rs`) over the per-request Rust hot paths and
  a counting-allocator leak check (`examples/leak_check.rs` +
  `scripts/mem_leak_test.sh`). Both gated in CI.
- Deployment templates (`deploy/`) and a load harness (`harness/`).
- Examples: `examples/webhook.py`, `examples/rest_api.py`, `examples/proxy.py`.

### Not yet (roadmap)

- `@http.on_shutdown` (needs a siphon shutdown hook for addon tasks),
  response-rewriting `(req, call_next)` middleware, body streaming, live route
  reload.
