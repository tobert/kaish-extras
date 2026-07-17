#!/usr/bin/env bash
# Build the try-kaish site into site/: wasm bundle + JS bindings + source seed.
# Requires: rustup target wasm32-unknown-unknown, wasm-bindgen-cli matching
# the locked wasm-bindgen version, python3. Optional: wasm-opt (binaryen).
#
# Usage: scripts/build-site.sh [path-to-kaish-checkout]
set -euo pipefail
cd "$(dirname "$0")/.."

kaish_repo="${1:-../kaish}"

cargo build --release --target wasm32-unknown-unknown -p kaish-web
wasm-bindgen --target web --out-dir site/pkg \
  target/wasm32-unknown-unknown/release/kaish_web.wasm

if command -v wasm-opt >/dev/null; then
  wasm-opt -Oz --all-features \
    -o site/pkg/kaish_web_bg.wasm site/pkg/kaish_web_bg.wasm
  echo "wasm-opt: $(du -h site/pkg/kaish_web_bg.wasm | cut -f1)"
fi

python3 scripts/make_seed.py site/seed.json \
  "kaish=${kaish_repo}" "kaish-extras=."
