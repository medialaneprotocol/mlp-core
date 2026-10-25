#!/usr/bin/env bash
# Run the wire-format conformance checks across all reference implementations.
# The sibling repositories are resolved relative to this checkout; override
# with WORK=... when they live elsewhere.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${WORK:-$(cd "$ROOT/.." && pwd)}"

echo "== Rust (mlp-core) =="
(cd "$ROOT" && cargo test --test conformance)

echo "== TypeScript (mlp-ts) =="
(cd "$WORK/mlp-ts" && npm test)

echo "== Go (mlp-go) =="
(cd "$WORK/mlp-go" && go test ./...)

echo "== Python (mlp-py) =="
(cd "$WORK/mlp-py" && python3 -m pytest -q)

echo "== C (mlp-c) =="
(cd "$WORK/mlp-c" && make test)

echo "ALL INTEROP CHECKS PASSED"
