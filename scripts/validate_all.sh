#!/usr/bin/env bash
# Full validation: Rust tests + all language implementations + interop.
# The sibling repositories are resolved relative to this checkout; override
# with WORK=... when they live elsewhere.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${WORK:-$(cd "$ROOT/.." && pwd)}"

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
