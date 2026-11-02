# Scientific WASM builds

The existing `js/Cargo.toml` feature `netcdf-hdf5` enables classic NetCDF,
NetCDF-4 and HDF5. On upstream a2a359a4, Cargo.lock selects netcdf-reader
0.10.1, hdf5-reader 0.10.1 and wasm-bindgen 0.2.129. The separate `hecras`
feature includes `netcdf-hdf5`; this minimal recipe does not enable `hecras`.
No new API or published preset is required. The build script's
`--no-default-features` selects a single module; `--features` forwards a Cargo
feature list. Supplying features without disabling defaults is rejected to avoid
accidentally shipping the full build. Selective builds do not build core,
threads, memory64 or the automatic loader. They use `--locked`.

## Reproduction

Run from the repository root. Record the source SHA, Cargo.lock hash, `rustc -Vv`,
`node --version`, `wasm-pack --version` and wasm-opt version with each artifact.
Install the stable toolchain, `wasm32-unknown-unknown` target and wasm-pack before
building. Pin those tool versions in the release environment; Cargo.lock alone
cannot pin wasm-pack's downloaded wasm-bindgen/wasm-opt tools. The workspace's
`wasm-release` profile and wasm-pack's configured `-Oz` optimize for size.

PowerShell (each output directory must be new):

```powershell
$env:CARGO_PROFILE_DEV_DEBUG = '0'
$env:CARGO_PROFILE_TEST_DEBUG = '0'
$env:CARGO_BUILD_JOBS = '2'
$env:CARGO_TARGET_DIR = Join-Path (Get-Location) 'target/scientific-package'
rustup target add wasm32-unknown-unknown
node js/build.mjs --target web --out-dir pkg-scientific --no-default-features --features netcdf-hdf5
node --test js/tests/build*.test.mjs
npm pack --dry-run --json ./js/pkg-scientific
```

Use `--target nodejs --out-dir pkg-scientific-node` with the same feature flags
for a CommonJS package for Node conformance. `--target bundler` is also accepted.
Use a worktree-specific disk target directory, never a shared or memory-backed
build directory. If sccache is installed, set `RUSTC_WRAPPER=sccache` and
`CARGO_INCREMENTAL=0`. No nightly Rust, shared memory or COOP/COEP headers are
needed by this single-threaded build. A dedicated browser worker keeps synchronous
decoding off the UI thread; worker acceptance belongs to the browser test suite.

For comparison, build the standard full/core package separately:

```powershell
node js/build.mjs --target web --out-dir pkg-comparison --no-threads --no-memory64
npm pack --dry-run --json ./js/pkg-comparison
```

Do not call core a NetCDF build. For a core-plus-scientific selection, explicitly
use `--features core,netcdf-hdf5`; it is larger than the minimal recipe.

## Package and hosting checks

The package must contain matching `terrio.js`, `terrio_bg.wasm`, `terrio.d.ts`,
and wasm-bindgen's WASM declarations, plus any generated snippets. The script
preserves wasm-pack's generated file list and adds `LICENSE`, `NOTICE` and
`LICENSE-THIRD-PARTY.md`. The third-party inventory is the repository's all-feature
inventory, a conservative superset rather than a claim that every crate is linked.
Review it against the locked dependency graph before distributing modified builds.

The selective package exports the root module, package metadata and direct asset
paths. It does not advertise absent core/load/threads/memory64 modules. Existing
output directories are refused to prevent stale variants entering the package.
Inspect the real `npm pack --dry-run --json` file list before redistribution;
light tests substitute wasm-pack and cannot verify generated code or npm behavior.
Do not publish a custom feature build under the standard package identity without
coordinator release review.

For unbundled web use, keep JS and WASM side by side and serve WASM as
`application/wasm`:

```js
import init, { features, openMultidim } from './pkg-scientific/terrio.js';
await init({ module_or_path: new URL('./pkg-scientific/terrio_bg.wasm', import.meta.url) });
console.log(features()); // confirm netcdf-hdf5 is enabled in the actual artifact
const dataset = openMultidim(new Uint8Array(await response.arrayBuffer()));
// Use the existing multidimensional API; dispose wrappers when finished.
```

Keep packed integer values and attributes as returned by the existing API. This
recipe does not apply scale/offset, change nodata semantics or alter array lifetime.
It still copies file bytes into WASM; it does not provide remote lazy HDF5 access.

## Included and excluded scope

GeoTIFF/COG, the CRS engine, text raster/vector formats and operations remain
compiled in. This is not a NetCDF-only binary. Optional GeoParquet/Arrow, CAD,
FlatGeobuf, SQLite/GeoPackage, tile archives, LZMA, LERC, AVIF, OSM, JPEG 2000,
point clouds, Zarr, GRIB and HEC-RAS are not enabled by this recipe. Always inspect
`features()` and test representative files: compile-time declarations can retain
methods whose implementations return feature-disabled errors. Numeric reads,
metadata and excluded-format error messages require tests against the real WASM;
packaging tests do not establish decoder conformance. HDF5 support does not imply
every HDF5 datatype/filter or HDF4 support.

## Measurement and final validation handoff

Record exact raw and compressed bytes for scientific, full and core artifacts:

```powershell
node --input-type=module -e 'import{readFileSync}from"node:fs";import{gzipSync,brotliCompressSync}from"node:zlib";for(const p of process.argv.slice(1)){const b=readFileSync(p);console.log(JSON.stringify({path:p,raw:b.length,gzip:gzipSync(b,{level:9}).length,brotli:brotliCompressSync(b).length}));}' js/pkg-scientific/terrio_bg.wasm js/pkg-comparison/terrio_bg.wasm js/pkg-comparison/core/terrio_bg.wasm
```

Measure initialization and selected-slice decoding separately in a browser-worker
harness using identical fixtures and browser versions. Report cold versus warm
runs, sample count, medians and tail times; compare full/core only on formats each
supports. Check numeric type/values, attributes, malformed data, excluded-format
errors, declarations, worker URLs and all requested assets. Never infer decoder
performance or correctness from compressed size.

Final coordinator checks after integration include the two web builds above, the
Node scientific build, real npm pack inspection, browser tests and:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

This packaging change does not require a Rust algorithm change. Heavy build and
browser measurements are deliberately deferred from the package worker phase;
no size or runtime benefit is asserted without those artifacts.
