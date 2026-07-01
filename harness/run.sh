#!/usr/bin/env bash
# Load-test harness orchestration.
#
#   ./run.sh self-test [extra drive args…]   # build, run mock server, drive it
#   ./run.sh drive --host H --port P …        # drive an already-running server
#   ./run.sh serve --port P                   # just the mock server
#
# self-test needs no siphon — it drives the built-in mock. For a REAL test,
# run your `siphon --features http` (with harness/bench_echo.py + siphon.bench.yaml),
# then: ./run.sh drive --host <host> --port 8080 --count 1000000 --concurrency 128
set -euo pipefail
cd "$(dirname "$0")"

cmd="${1:-self-test}"; shift || true

echo "[*] building http-load (release)…"
cargo build --release --quiet
BIN=./target/release/http-load

case "$cmd" in
  self-test)
    port=18080
    echo "[*] starting mock server on :$port"
    "$BIN" serve --port "$port" &
    serve_pid=$!
    trap 'kill $serve_pid 2>/dev/null || true' EXIT
    sleep 1
    echo "[*] driving load…"
    "$BIN" drive --port "$port" --count "${COUNT:-50000}" --concurrency "${CONCURRENCY:-64}" "$@"
    ;;
  drive|serve)
    exec "$BIN" "$cmd" "$@"
    ;;
  *)
    echo "usage: ./run.sh [self-test|drive|serve] [args…]" >&2
    exit 2
    ;;
esac
