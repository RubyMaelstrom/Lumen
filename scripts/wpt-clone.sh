#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="${WPT:-$ROOT/wpt}"
REPO="https://github.com/web-platform-tests/wpt.git"
REV="54078e9ec9d5c73f8815ff38b42b8fcbf1f3200b"

if [[ ! -d "$DEST/.git" ]]; then
  git clone --depth 1 --filter=blob:none --sparse "$REPO" "$DEST"
fi

# WPT is a moving standards corpus. Keep the default gate reproducible and update this revision
# deliberately alongside any resulting expectation or implementation changes.
if ! git -C "$DEST" cat-file -e "$REV^{commit}" 2>/dev/null; then
  git -C "$DEST" fetch --depth 1 origin "$REV"
fi
git -C "$DEST" checkout --detach "$REV"

# Keep the checkout small while including the authoritative harness, every default focused API
# area, and their shared helper scripts. `WPT=/path` may point at a pre-existing full checkout.
git -C "$DEST" sparse-checkout set \
  resources common dom/events encoding html/webappapis/atob \
  html/infrastructure/safe-passing-of-structured-data url hr-time WebCryptoAPI \
  fetch/api fetch/data-urls xhr/resources streams compression webmessaging websockets eventsource workers

echo "wpt ready at $DEST ($(git -C "$DEST" rev-parse HEAD))"
