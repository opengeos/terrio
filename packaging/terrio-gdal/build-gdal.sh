#!/usr/bin/env bash
# Builds GDAL with vcpkg for the terrio-gdal wheels, then prepares python/ to build them
# (prepare.py). Run from the repository's root, on the build machine (in the manylinux container
# on Linux, as cibuildwheel's before-all), with:
#
#   TRIPLET      the vcpkg triplet, one of packaging/terrio-gdal/triplets (x64-linux-terrio...)
#   VCPKG_ROOT   where vcpkg is cloned (or already is), checked out at the manifest's baseline
#   PYTHON       a Python 3.8+ for prepare.py (default: python3)
#
# vcpkg's binary cache (VCPKG_DEFAULT_BINARY_CACHE, by default ~/.cache/vcpkg/archives) keeps the
# packages it builds: with it restored, a rebuild takes minutes rather than an hour.
set -euo pipefail

here="packaging/terrio-gdal"
: "${TRIPLET:?set TRIPLET to a vcpkg triplet of $here/triplets}"
: "${VCPKG_ROOT:?set VCPKG_ROOT to where vcpkg is (or is to be cloned)}"
python="${PYTHON:-python3}"
baseline=$(sed -n 's/.*"builtin-baseline": *"\([0-9a-f]*\)".*/\1/p' "$here/vcpkg.json")

if [ ! -d "$VCPKG_ROOT/.git" ]; then
  git clone https://github.com/microsoft/vcpkg.git "$VCPKG_ROOT"
fi
git -C "$VCPKG_ROOT" fetch --quiet origin "$baseline" || git -C "$VCPKG_ROOT" fetch --quiet origin
git -C "$VCPKG_ROOT" -c advice.detachedHead=false checkout --force "$baseline"

case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*)
    (cd "$VCPKG_ROOT" && cmd.exe //c bootstrap-vcpkg.bat -disableMetrics)
    vcpkg="$VCPKG_ROOT/vcpkg.exe"
    ;;
  *)
    "$VCPKG_ROOT/bootstrap-vcpkg.sh" -disableMetrics
    vcpkg="$VCPKG_ROOT/vcpkg"
    ;;
esac

if [ -n "${VCPKG_DEFAULT_BINARY_CACHE:-}" ]; then
  mkdir -p "$VCPKG_DEFAULT_BINARY_CACHE"
fi

host=()
case "$TRIPLET" in
  # Release-only host tools too: pkgconf fails to build in debug mode on Arm64 Linux.
  arm64-linux-*) host=(--host-triplet=arm64-linux-release) ;;
esac

"$vcpkg" install \
  --triplet="$TRIPLET" ${host[@]+"${host[@]}"} \
  --overlay-triplets="$here/triplets" \
  --x-manifest-root="$here" \
  --x-install-root="$VCPKG_ROOT/installed" \
  --clean-after-build
ls -l "$VCPKG_ROOT/installed/$TRIPLET/lib" "$VCPKG_ROOT/installed/$TRIPLET/bin" 2> /dev/null || true

"$python" "$here/prepare.py" "$VCPKG_ROOT/installed/$TRIPLET"
