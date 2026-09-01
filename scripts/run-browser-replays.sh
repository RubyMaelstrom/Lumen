#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
trust_root=${TRUST_REPLAY_ROOT:-"$root/../TRust"}
mode=${1:-check}

case "$mode" in
    quick)
        profile=debug
        warmups=0
        samples=1
        fixtures=("$root/benchmarks/browser-replays/event-loop.html")
        ;;
    check)
        profile=debug
        warmups=0
        samples=1
        fixtures=("$root"/benchmarks/browser-replays/*.html)
        ;;
    benchmark)
        profile=release
        warmups=1
        samples=5
        fixtures=("$root"/benchmarks/browser-replays/*.html)
        ;;
    *)
        echo "usage: scripts/run-browser-replays.sh [quick|check|benchmark]" >&2
        exit 2
        ;;
esac

if [[ ! -f "$trust_root/Cargo.toml" ]]; then
    echo "TRust checkout not found at $trust_root (override with TRUST_REPLAY_ROOT)" >&2
    exit 1
fi

cd "$root"
sha256sum --check benchmarks/browser-replays.sha256

build=(cargo build --offline --locked --manifest-path "$trust_root/Cargo.toml" --bin trust-browser-replay)
binary="$trust_root/target/debug/trust-browser-replay"
if [[ "$profile" == release ]]; then
    build+=(--release)
    binary="$trust_root/target/release/trust-browser-replay"
fi
"${build[@]}"

mkdir -p benchmark-results
stamp=$(date -u +%Y%m%dT%H%M%SZ)
output="$root/benchmark-results/browser-replay-${mode}-${stamp}.json"
command=("$binary" --warmups "$warmups" --samples "$samples" "${fixtures[@]}")
cpu=${LUMEN_REPLAY_CPU:-5}
if [[ "$cpu" != none ]]; then
    command=(taskset -c "$cpu" "${command[@]}")
fi
"${command[@]}" | tee "$output"
echo "report: $output" >&2
