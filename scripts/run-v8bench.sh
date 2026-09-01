#!/usr/bin/env bash
#
# Run the classic V8 benchmark suite (v8-v7, from mozilla/arewefastyet) on the lumen engine.
# Verifies the benchmark JS against benchmarks/engine-matrix.json (fetching the pinned revision on
# first use), builds the `lumen` CLI in release mode, and prints per-benchmark scores plus the
# composite score. Higher is better; scores are normalized to a 2008 reference machine at 100.
#
#   scripts/run-v8bench.sh              # full suite
#   scripts/run-v8bench.sh richards     # one benchmark (any of the .js basenames)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="$ROOT/v8-v7"
"$ROOT/scripts/fetch-v8-v7.py" >&2

# The upstream driver uses the shell `load()`; the lumen CLI takes files in sequence instead.
sed '/^load(/d' "$DEST/run.js" > "$DEST/driver.js"

cargo build --release -q -p lumen --bin lumen

if [ $# -ge 1 ]; then
  SUITES=("$@")
else
  SUITES=(richards deltablue crypto raytrace earley-boyer regexp splay navier-stokes)
fi

ARGS=("$DEST/base.js")
for s in "${SUITES[@]}"; do
  ARGS+=("$DEST/${s%.js}.js")
done
ARGS+=("$DEST/driver.js")

exec "$ROOT/target/release/lumen" "${ARGS[@]}"
