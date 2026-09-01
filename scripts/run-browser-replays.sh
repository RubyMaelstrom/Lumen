#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
trust_root=${TRUST_REPLAY_ROOT:-"$root/../TRust"}
mode=${1:-check}
asset_root="$root/browser-replay-fixtures/speedometer-3.1/resources/todomvc/architecture-examples/vue/dist"

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

asset_args=()
if [[ "$mode" != quick ]]; then
    # Verification is deliberately separate from provisioning: --verify-only
    # cannot fetch, copy, or replace bytes during a measured run.
    python3 scripts/fetch-browser-replay-assets.py --verify-only
    asset_args=(
        --external "js/chunk-vendors.b4ac9361.js=$asset_root/js/chunk-vendors.b4ac9361.js"
        --external "js/app.ad36df07.js=$asset_root/js/app.ad36df07.js"
        --sheet "css/app.319576e1.css=$asset_root/css/app.319576e1.css"
    )
fi

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
command=("$binary" --warmups "$warmups" --samples "$samples" "${asset_args[@]}" "${fixtures[@]}")
cpu=${LUMEN_REPLAY_CPU:-5}
if [[ "$cpu" != none ]]; then
    command=(taskset -c "$cpu" "${command[@]}")
fi
"${command[@]}" | tee "$output"
echo "report: $output" >&2
