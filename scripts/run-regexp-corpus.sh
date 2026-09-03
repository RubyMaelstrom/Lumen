#!/usr/bin/env bash
#
# Run the stable RegExp corpus (benchmarks/regexp-corpus/manifest.json) on lumen (jit and
# bytecode tiers) and, when available, node, and print a per-entry comparison table.
#
#   scripts/run-regexp-corpus.sh                 # lumen (both tiers) + node
#   LUMEN_BIN=/path/to/lumen scripts/run-regexp-corpus.sh   # skip the build
#   CORPUS_ENGINES="lumen-jit node" scripts/run-regexp-corpus.sh  # engine selection
#
# The generated harness is self-verifying: any match-count divergence from the locked
# manifest exits nonzero via the `if !ok` quit(3) convention. Adversarial entries may end
# through the engine step budget, which the harness counts as termination.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CORPUS="$ROOT/benchmarks/regexp-corpus"
HARNESS="$CORPUS/generated-harness.js"
ENGINES="${CORPUS_ENGINES:-lumen-jit lumen-bytecode}"

if [ ! -f "$CORPUS/manifest.json" ]; then
  echo "error: corpus manifest not found at $CORPUS/manifest.json" >&2
  exit 1
fi

# Lock subject hashes on first use (immutable thereafter; a tampered manifest is rejected).
if ! python3 "$ROOT/scripts/gen-regexp-corpus.py" check >/dev/null 2>&1; then
  echo "locking corpus subject hashes ..." >&2
  python3 "$ROOT/scripts/gen-regexp-corpus.py" lock >&2
fi

python3 "$ROOT/scripts/gen-regexp-corpus.py" harness "$HARNESS" >&2

LUMEN_BIN="${LUMEN_BIN:-}"
if [[ " $ENGINES " == *" lumen-"* ]] && [ -z "$LUMEN_BIN" ]; then
  echo "Building lumen (release) ..." >&2
  (cd "$ROOT" && cargo build --release -q -p lumen --bin lumen)
  LUMEN_BIN="$ROOT/target/release/lumen"
fi

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

run_engine() {
  local tag="$1"; shift
  if timeout 300 "$@" > "$TMP/$tag.txt" 2>&1; then
    :
  else
    echo "engine $tag timed out or failed (see $TMP/$tag.txt)" >&2
  fi
}

if [[ " $ENGINES " == *" lumen-jit "* ]]; then
  echo "Running lumen (jit) ..." >&2
  run_engine lumen-jit "$LUMEN_BIN" --tier=jit "$HARNESS"
fi
if [[ " $ENGINES " == *" lumen-bytecode "* ]]; then
  echo "Running lumen (bytecode) ..." >&2
  run_engine lumen-bytecode "$LUMEN_BIN" --tier=bytecode "$HARNESS"
fi
if [[ " $ENGINES " == *" node "* ]]; then
  if command -v node >/dev/null; then
    echo "Running node ..." >&2
    run_engine node node "$HARNESS"
  else
    echo "warning: node not found, skipping" >&2
  fi
fi

# One combined pass line per engine; collect entries from the first available run.
FIRST=""
for tag in lumen-jit lumen-bytecode node; do
  [ -f "$TMP/$tag.txt" ] || continue
  FIRST="$tag"
  break
done
if [ -z "$FIRST" ]; then
  echo "error: no engine produced output" >&2
  exit 1
fi

FAIL=0
entries=$(grep -c "^[a-z0-9-]* matches=" "$TMP/$FIRST.txt" || true)
echo
printf '%-24s %-12s' "entry" "expected"
for tag in lumen-jit lumen-bytecode node; do
  [ -f "$TMP/$tag.txt" ] && printf ' %-18s' "$tag"
done
echo
printf '%s\n' '------------------------------------------------------------------------'
while IFS= read -r line; do
  id=$(echo "$line" | cut -d' ' -f1)
  matches=$(echo "$line" | sed -n 's/^[a-z0-9-]* matches=\([0-9]*\).*/\1/p')
  verdict=$(echo "$line" | grep -o "MISMATCH expected [0-9]*" || echo OK)
  printf '%-24s %-12s' "$id" "$matches"
  for tag in lumen-jit lumen-bytecode node; do
    if [ -f "$TMP/$tag.txt" ]; then
      own=$(grep "^$id matches=" "$TMP/$tag.txt" | head -1)
      [ -n "$own" ] || own="missing"
      ms=$(echo "$own" | sed -n 's/.* ms=\([0-9]*\).*/\1/p')
      budget=$(echo "$own" | sed -n 's/.*budget=\([0-9]*\).*/\1/p')
      [ -n "$budget" ] || budget="-"
      printf ' %-13s ms/b%-2s' "${ms:-?}" "$budget"
    fi
  done
  echo " $verdict"
  if [ "$verdict" != "OK" ]; then
    FAIL=1
  fi
done < "$TMP/$FIRST.txt"

if grep -q "CORPUS FAIL" "$TMP/$FIRST.txt"; then
  FAIL=1
fi
echo
if [ "$FAIL" = 0 ]; then
  echo "corpus: PASS ($entries entries)"
else
  echo "corpus: FAIL (divergence from the locked manifest)" >&2
  exit 3
fi