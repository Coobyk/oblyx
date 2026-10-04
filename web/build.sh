#!/usr/bin/env bash
# Build the oblyx WASM module with size-oriented release settings and stage
# it into web/www/ (the directory Cloudflare Pages deploys).
set -euo pipefail
cd "$(dirname "$0")/.."

CARGO_PROFILE_RELEASE_OPT_LEVEL=z \
CARGO_PROFILE_RELEASE_LTO=fat \
CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
CARGO_PROFILE_RELEASE_STRIP=symbols \
  cargo build --release --target wasm32-unknown-unknown -p oblyx-web

cp target/wasm32-unknown-unknown/release/oblyx_web.wasm web/www/
hash=$(sha256sum web/www/oblyx_web.wasm | awk '{print substr($1,1,12)}')
sed -i "s|oblyx_web\\.wasm?v=[0-9a-f]*|oblyx_web.wasm?v=${hash}|" web/www/worker.js
ls -lh web/www/oblyx_web.wasm
