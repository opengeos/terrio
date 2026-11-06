#!/usr/bin/env bash
# SPDX-License-Identifier: MIT
# Builds terrio.dev/web: the command line for wasm32-wasip1 (the npm package's size-optimized
# `wasm-release` profile, stripped, with the 8 MiB stack the native command line runs commands
# on), next to the page and its WASI runtime.
#
# Usage: web/build.sh [OUT_DIR]   (default: site/web)
set -euo pipefail
cd "$(dirname "$0")/.."
out="${1:-site/web}"
target_dir="${CARGO_TARGET_DIR:-target}"
CARGO_PROFILE_WASM_RELEASE_STRIP=symbols \
RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-zstack-size=8388608" \
  cargo build --profile wasm-release -p terrio-cli --bin terrio --target wasm32-wasip1
mkdir -p "$out"
cp web/index.html web/worker.js web/wasi.js web/map.js web/paths.js "$out/"
cp "$target_dir/wasm32-wasip1/wasm-release/terrio.wasm" "$out/terrio.wasm"
ls -l "$out"
