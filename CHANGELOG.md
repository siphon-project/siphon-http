# Changelog

All notable changes to `siphon-http` are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Fixed

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
