#!/usr/bin/env bash
# Run the wire-format conformance checks across all reference implementations.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${WORK:-/tmp/opencode/bench-work}"

echo "== Rust (mlp-core) =="
(cd "$ROOT" && cargo test --test conformance)

echo "== Go (mlp-go) =="
(cd "$WORK/mlp-go" && go test ./...)

echo "== Python (mlp-py) =="
(cd "$WORK/mlp-py" && python3 -m pytest -q)

echo "== C (mlp-c) =="
(cd "$WORK/mlp-c" && make test)

echo "ALL INTEROP CHECKS PASSED"
