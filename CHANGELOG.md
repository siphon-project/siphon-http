# Changelog

All notable changes to `siphon-http` are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
