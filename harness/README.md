# Load-test harness

A standalone HTTP load driver (`http-load`) for a server built on siphon-http,
plus a mock server so you can smoke-test the driver — and CI — without standing
up siphon. It's its own little workspace (axum + hyper + reqwest + tokio + clap),
so it builds fast and doesn't pull the siphon stack.

## Self-test (no siphon)

```bash
./run.sh self-test                          # mock server + 50k requests, conc 64
COUNT=200000 CONCURRENCY=128 ./run.sh self-test
```

This is also the CI smoke test: it round-trips real HTTP through the driver and
the mock and fails (non-zero exit) on any error.

```
── results ──────────────────────────────
  requests  : 50000  ok 50000  errors 0
  elapsed   : 0.4xx s
  throughput: ~1xx,xxx req/s
  latency   : p50 …  p90 …  p99 …  p999 …  max …
```

(The mock is a trivial in-process 200, so these numbers measure the *driver* and
the loopback — not a real server. Use the real flow below for meaningful
numbers.)

## Real load test (against a siphon-http server)

The thing the harness exists for — load-test the actual siphon + http dispatch
path. `bench_echo.py` is a logging-free single-route handler (measures dispatch,
not log I/O); `siphon.bench.yaml` wires it plus `http.yaml` (the addon listener).

1. Build the HTTP-enabled siphon binary (once the `http` feature is wired into
   `siphon-bin`):
   ```bash
   cargo build -p siphon-bin --release --features http
   ```
2. Run it, then drive load:
   ```bash
   ./siphon -c harness/siphon.bench.yaml &
   ./run.sh drive --host 127.0.0.1 --port 8080 --count 1000000 --concurrency 128
   ```

### Aggregate + free-threaded

As with any Python-in-the-loop siphon addon, aggregate throughput is bounded by
the per-request Python handler running under the CPython GIL — it serializes to
roughly one core regardless of concurrency or cores. Build siphon against a
**free-threaded** interpreter (`PYO3_PYTHON=python3.14t`, run with that
`libpython3.14t` on `LD_LIBRARY_PATH`) and the same load scales across cores
(free-threaded CPython support is still stabilising, so treat it as
experimental).

## `http-load` reference

```
http-load drive  --host H --port P --path /... --count N --concurrency C
http-load serve  --host H --port P        # mock server (200 OK for any request)
```
