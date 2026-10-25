# CRS dependency and build footprint

These measurements compare equivalent consumers that parse EPSG:4326 and
EPSG:3857 and export the same coordinate-transform function. The baseline uses
Terrio with `default-features = false`. Standalone configurations use
`terrio-crs` directly; the fallback configuration enables `wbprojection-fallback`.
The final Terrio configuration retains that fallback. All consumers are isolated
from the workspace and use fresh target directories.

| Consumer | Packages native / WASM | Build seconds native / WASM | Raw WASM bytes | Optimized WASM bytes | Optimized gzip bytes |
| --- | ---: | ---: | ---: | ---: | ---: |
| Terrio before extraction | 149 / 130 | 169.29 / 120.63 | 4,197,535 | 3,946,779 | 1,260,844 |
| Standalone native default | 47 / 38 | 32.61 / 21.49 | 1,683,729 | 1,518,533 | 908,412 |
| Standalone with fallback | 53 / 43 | 44.73 / 31.58 | 4,147,954 | 3,917,282 | 1,244,987 |
| Terrio after extraction | 150 / 131 | 161.19 / 114.69 | 4,175,412 | 3,942,218 | 1,259,334 |

Counts include the consumed library plus unique name/version pairs in normal
and build dependencies. They exclude the probe and development dependencies.
Terrio's complete graph adds the new crate; extraction does not remove its
raster/vector functionality. The standalone fallback adds six packages:
`wbprojection`, `wide`, `safe_arch`, `bytemuck` and the 1.x `thiserror` pair.

The native default handles 40 methods in the 117-method conformance corpus;
the fallback configuration handles 86. These are different supported scopes.
The direct [standalone benchmark validation](crs-benchmark-validation.md)
records the 93-fixture CRS and 54-projection study results for both configurations.

## Method and source versions

- Before: `0e58a72a7a287b97142e6457920226a7e0e2d510` (numerical fixes merged, before extraction).
- After: `53e90f922cd7b7993b8b8d75d10069f836260975` (optional fallback and transport concurrency fix).
- Rust: 1.99.0 (`b940084d7`, LLVM 23.1.1).
- Targets: `x86_64-unknown-linux-gnu` and `wasm32-unknown-unknown`.
- Host: Intel(R) Xeon(R) w7-3445; Linux, glibc 2.44.
- Release settings: `opt-level = "s"`, LTO, one codegen unit, panic abort, stripped.
- Two Cargo jobs; incremental compilation disabled; sccache forced to recache.
- `wasm-opt` 117, `-Oz --all-features`, `BINARYEN_CORES=2`.
- Gzip uses Python's default level with `mtime=0` for a reproducible header.

These are single serial observations on a shared desktop with warm downloaded
packages/filesystem caches. Timings vary; no CI timing threshold is asserted.
The native baseline was repeated after an overlapping lint check; only the
uncontaminated repeat is included. The unchanged baseline WASM observation is
reused from the initial serial matrix. The
[machine-readable report](benchmarks/crs-footprint.json) records commands,
settings, source cleanliness and exact sizes. Its child-process resource counters
exclude the persistent sccache compiler workers and do not measure whole-build
peak memory.

Reproduce from separate before/after checkouts with no concurrent builds:

```sh
python scripts/measure_crs_footprint.py \
  --before /path/to/before --after /path/to/after \
  --output /path/to/new-disk-directory --wasm-opt /path/to/wasm-opt
```

The script creates the matching manifests, seeds each lockfile from its source
checkout, records dependency trees and build logs, and runs every build serially.
`--configurations` and `--targets` allow an isolated configuration to be repeated
in a fresh output directory. No source checkout is modified or package uploaded.

## Codec audit and portability

Bundled EPSG/ESRI/datum tables use `ruzstd` on native and WASM targets. Removing
the separate native table decoder removes the standalone crate's direct
`zstd` 0.14 dependency. Native TIFF decoding still uses its existing Zstd 0.13
backend. Full Terrio retains its own Zstd dependency for its formats.

TIFF grids support Float32/Float64 and Int16/UInt16/Int32/UInt32 samples. Dropping
codecs based only on Float32 grids would narrow that contract; for example,
12-bit JPEG can decode to UInt16. TIFF's existing codec features are retained,
including native Zstd and the workspace's WebP feature. The algorithm engine is
Rust; this audit does not claim the native dependency graph contains no C codecs.

Networking remains optional through the host-supplied `grid-transport` interface,
with no HTTP dependency in the standalone graph. Terrio supplies its existing
range reader. After the cache concurrency fix, four EGM2008 queries fetched
851,968 bytes from an 80 MB grid and a direct standalone cached query fetched
zero additional bytes. Height comparisons allow 1e-9 m; full-grid downloads are
explicit and are not used by this regression.
