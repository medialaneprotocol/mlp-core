#!/usr/bin/env bash
# Full validation: Rust tests + all language implementations + interop.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${WORK:-/tmp/opencode/bench-work}"

echo "=== Rust (mlp-core) ==="
(cd "$ROOT" && cargo test)

echo ""
echo "=== Go (mlp-go) ==="
(cd "$WORK/mlp-go" && go build ./... && go test ./...)

echo ""
echo "=== Python (mlp-py) ==="
(cd "$WORK/mlp-py" && python3 -m pytest -q)

echo ""
echo "=== C (mlp-c) ==="
(cd "$WORK/mlp-c" && make test)

echo ""
echo "=== Interop ==="
(cd "$ROOT" && bash scripts/interop.sh)

echo ""
echo "=== ALL VALIDATIONS PASSED ==="
