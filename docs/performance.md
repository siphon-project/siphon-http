# Performance & scaling

This page describes *where* the performance ceiling is and *why*. Numbers below
are indicative (a developer laptop, loopback) — reproduce your own with the
[harness](#reproducing-it).

## The wire path is Rust

The wire and TLS paths are axum / hyper / rustls, and the per-request Rust work
this crate adds — path-param extraction, query parsing, percent-decode, config
parse — is covered by the crate's benches
([`benches/parse.rs`](https://github.com/siphon-project/siphon-http/blob/main/benches/parse.rs),
`cargo bench`). Indicative single-core numbers:

| Path | Time |
|---|---|
| path-param extraction (`/users/{id}/orders/{order}`) | ~135 ns |
| query parsing (4 params) | ~255 ns |
| percent-decode | ~35 ns |
| `http.yaml` parse (boot / hot-reload) | ~5.5 µs |

A counting-allocator
[leak check](https://github.com/siphon-project/siphon-http/blob/main/examples/leak_check.rs)
(`./scripts/mem_leak_test.sh`) hammers these and asserts **live bytes stay flat**
(Δ 0 over 200k cycles). Against the harness's in-process mock, the driver +
loopback sustain **~270k req/s at sub-100 µs p50** — the driver ceiling, not a
real server. **The Rust request path is not the throughput limit.**

## The limit is Python handler dispatch

Under load, aggregate throughput is bounded by the **per-request Python handler
dispatch**, which runs in CPython. On a standard (GIL) interpreter it serialises
to roughly one core, regardless of how many connections or cores you have.

Two things follow:

- **Keep per-request handler work minimal.** Push heavy lifting into Rust; do as
  little as possible in the Python handler on the hot path.
- **Run against free-threaded CPython** — this is the real unlock. On a
  free-threaded interpreter (3.13t / 3.14t) handlers run on **every core** and
  aggregate throughput scales with cores.

This is the same scaling characteristic as any Python-in-the-loop siphon addon —
nothing HTTP-specific.

!!! warning "Free-threaded CPython is experimental"
    Free-threaded CPython support is still stabilising; treat it as experimental.

## Scaling out vs. scaling up

| Lever | How | Effect |
|---|---|---|
| **Scale out** | Add replicas (a Kubernetes HPA on CPU does this — see [Deployment](deployment.md#kubernetes)). | The Rust path isn't the limit, so more pods means more aggregate throughput. |
| **Scale up in a pod** | Build siphon against free-threaded CPython. | Handlers run on every core inside one process. |

## Reproducing it

The
[`harness/`](https://github.com/siphon-project/siphon-http/tree/main/harness)
reproduces both the GIL-bound and the free-threaded behaviour. It ships a
standalone load driver (`http-load`) and a mock server, so you can smoke-test the
driver on loopback and then drive real load at a siphon-http server.

Self-test (no siphon — measures the driver + loopback, not a real server):

```bash
./run.sh self-test
```

Real load against a siphon-http server (build the HTTP-enabled binary, run it,
then drive):

```bash
./run.sh drive --host 127.0.0.1 --port 8080 --count 1000000 --concurrency 128
```

To exercise the free-threaded path, build siphon against a free-threaded
interpreter and drive the same load; handlers then run across cores instead of
serialising to one. See the harness README for the interpreter/`LD_LIBRARY_PATH`
details.
