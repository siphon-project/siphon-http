#!/usr/bin/env bash
# Build + run the counting-allocator leak check (examples/leak_check.rs).
# Exits non-zero on FAIL so it can gate CI.
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo run --release --example leak_check "$@"
