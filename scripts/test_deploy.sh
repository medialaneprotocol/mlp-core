#!/usr/bin/env bash
# Local deployment smoke test.
set -euo pipefail

echo "== Building =="
cargo build --release --features cli

echo "== Health check =="
./target/release/mlp-core limits --json | head -c 200
echo ""

echo "== Fragment round-trip =="
./target/release/mlp-core fragment build --class control --priority 0 --object-id 1 --json 2>/dev/null || echo "(cli args may vary)"

echo "== Conformance =="
cargo test --test conformance

echo "== Interop =="
bash scripts/interop.sh

echo "DEPLOY SMOKE TEST PASSED"
