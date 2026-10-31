#!/usr/bin/env bash
# Builds the Python wheel for Linux with glibc 2.28 or later (manylinux_2_28), on any Linux host,
# without paths of the build machine.
#
# A plain `maturin build` links with the host's C library and tags the wheel with the newest glibc
# symbol version it then needs: on a glibc 2.39 host the wheel is manylinux_2_39, and neither pip
# nor the dynamic loader accepts it on an older glibc ("version `GLIBC_2.39' not found"). The
# release workflow (.github/workflows/release-build.yml) avoids this with maturin-action, which
# builds the Linux x86_64 wheel in the manylinux2014 container (glibc 2.17). Without Docker,
# maturin's `--zig` links with zig against the symbols of the glibc version the tag names.
#
# A build also leaves its paths in the wheel. Rust names a source file in each panic location and
# debug string, so a plain build embeds the Cargo home (registry sources, git checkouts), the
# rustup home (the standard library's sources, when the rust-src component is installed), the
# target directory (files that build scripts generate in OUT_DIR) and the checkout. rustc's
# --remap-path-prefix maps them to /cargo, /rustup, /target and /src: the flags go to Cargo in
# CARGO_ENCODED_RUSTFLAGS (a path may hold spaces), after the caller's own (CARGO_ENCODED_RUSTFLAGS,
# else RUSTFLAGS), and maturin passes them on. As any RUSTFLAGS do, they make Cargo ignore the
# `rustflags` of its configuration files (this workspace sets some for wasm32 only). maturin's SBOM
# names the workspace's crates by their Cargo package IDs, `path+file://` URLs in the checkout
# that no rustc flag reaches: scripts/remap_wheel_sbom.py maps those to /src as well. The flags do
# not reach C compilers either: the C sources that maturin's zig wrapper compiles (with -g) leave
# their paths in debug information, which the release link strips; scripts/check_wheel_paths.py
# would find a path that stayed (one in __FILE__, say).
#
# Usage: scripts/build_wheel_manylinux_2_28.sh [OUT_DIR]    (default: python/dist)
#
# Needs Rust and maturin with zig: `pip install "maturin[zig]"` (tested with maturin 1.15.0 and
# ziglang 0.17.0). MATURIN names another maturin command; Cargo's environment applies
# (CARGO_TARGET_DIR, CARGO_BUILD_JOBS, CARGO_HOME, RUSTUP_HOME). As in the release workflow, the
# wheel carries the notices and is checked for its license files; then for its glibc
# (scripts/check_wheel_glibc.py) and for paths of the build machine (scripts/check_wheel_paths.py,
# with the four directories forbidden). Only a wheel that passes all three lands in OUT_DIR.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/.." && pwd)"
mkdir -p "${1:-$root/python/dist}"
out="$(cd "${1:-$root/python/dist}" && pwd)"
build="$(mktemp -d)"
# The notices, which maturin puts in the wheel's .dist-info/licenses/ from python/ (the release
# workflow copies them there too); removed afterwards when this script added them.
added=()
cleanup() {
  rm -rf "$build"
  if [ ${#added[@]} -gt 0 ]; then rm -f "${added[@]}"; fi
}
trap cleanup EXIT
for name in NOTICE LICENSE-THIRD-PARTY.md; do
  if [ ! -e "$root/python/$name" ]; then
    cp "$root/$name" "$root/python/$name"
    added+=("$root/python/$name")
  fi
done
# The directories, as Cargo names them to rustc (its metadata's: absolute, links kept).
metadata="$(cd "$root/python" && "${CARGO:-cargo}" metadata --format-version 1 --no-deps)"
field() { python3 -c 'import json, sys; print(json.load(sys.stdin)[sys.argv[1]])' "$1" <<<"$metadata"; }
checkout="$(field workspace_root)"
target="$(field target_directory)"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
rustup_home="${RUSTUP_HOME:-$HOME/.rustup}"
# rustc's flags: the caller's (Cargo takes CARGO_ENCODED_RUSTFLAGS, else RUSTFLAGS split at
# spaces), then a --remap-path-prefix per directory. rustc applies the last prefix that matches, so
# the target directory, by default inside the checkout, comes last.
sep=$'\x1f'
if [ -n "${CARGO_ENCODED_RUSTFLAGS+set}" ]; then
  flags="$CARGO_ENCODED_RUSTFLAGS"
else
  IFS=' ' read -r -a words <<<"${RUSTFLAGS:-}"
  flags="$(IFS="$sep" && printf '%s' "${words[*]:-}")"
fi
remap() { flags="${flags:+$flags$sep}--remap-path-prefix=${1%/}=$2"; }
remap "$cargo_home" /cargo
remap "$rustup_home" /rustup
remap "$checkout" /src
remap "$target" /target
export CARGO_ENCODED_RUSTFLAGS="$flags"
(cd "$root/python" &&
  ${MATURIN:-maturin} build --release --locked --zig --compatibility manylinux_2_28 --out "$build")
wheel="$(ls "$build"/*.whl)"
python3 "$here/remap_wheel_sbom.py" "$wheel" "$checkout" /src
python3 "$root/.github/scripts/check-license-files.py" "$wheel"
python3 "$here/check_wheel_glibc.py" "$wheel" --max 2.28
python3 "$here/check_wheel_paths.py" "$wheel" --forbid "$target" --forbid "$checkout" \
  --forbid "$cargo_home" --forbid "$rustup_home"
mv "$wheel" "$out/"
echo "$out/$(basename "$wheel")"
