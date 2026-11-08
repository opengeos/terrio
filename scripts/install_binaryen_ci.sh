#!/usr/bin/env bash
# Install the same Binaryen used by wasm-pack, before wasm-pack's direct-download fallback.
# The release API avoids the failing browser-download URL; retries handle transient failures.
set -euo pipefail

: "${RUNNER_TEMP:?RUNNER_TEMP must name the runner temporary directory}"
: "${GITHUB_PATH:?GITHUB_PATH must name the runner path file}"

if [[ $(uname -s) != Linux || $(uname -m) != x86_64 ]]; then
  echo "This CI installer supports Linux x86_64 runners only." >&2
  exit 1
fi

readonly binaryen_version=130
# Published SHA-256 of binaryen-version_130-x86_64-linux.tar.gz.
readonly binaryen_sha256=0a18362361ad05465118cd8eeb72edaeec89de6894bc283576ef4e07aa3babcc
binaryen_dir=$(mktemp -d "$RUNNER_TEMP/terrio-binaryen.XXXXXX")
binaryen_archive="$binaryen_dir/binaryen-version_${binaryen_version}-x86_64-linux.tar.gz"

for binaryen_attempt in 1 2 3; do
  if timeout 180s gh release download "version_$binaryen_version" \
      --repo WebAssembly/binaryen \
      --pattern "binaryen-version_${binaryen_version}-x86_64-linux.tar.gz" \
      --output "$binaryen_archive" --clobber \
      && printf '%s  %s\n' "$binaryen_sha256" "$binaryen_archive" | sha256sum --check; then
    break
  fi
  if [[ $binaryen_attempt == 3 ]]; then
    echo "Could not download and verify Binaryen after three attempts." >&2
    exit 1
  fi
  sleep 5
done

tar -xzf "$binaryen_archive" -C "$binaryen_dir" "binaryen-version_$binaryen_version/bin/wasm-opt"
rm "$binaryen_archive"
binaryen_bin="$binaryen_dir/binaryen-version_$binaryen_version/bin"
"$binaryen_bin/wasm-opt" --version
printf '%s\n' "$binaryen_bin" >> "$GITHUB_PATH"
