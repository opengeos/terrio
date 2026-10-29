# CRS dependency and build footprint

These measurements compare equivalent consumers that parse EPSG:4326 and
EPSG:3857 and export the same coordinate-transform function, returning the
projected x component. The baseline uses
Terrio with `default-features = false`. Standalone configurations use
`terrio-crs` directly; the fallback configuration enables `wbprojection-fallback`.
The final Terrio configuration retains that fallback. All consumers are isolated
from the workspace and use fresh target directories.

| Consumer | Packages native / WASM | Build seconds native / WASM | Raw WASM bytes | Optimized WASM bytes | Optimized gzip bytes |
| --- | ---: | ---: | ---: | ---: | ---: |
| Terrio before extraction | 149 / 130 | 169.29 / 120.63 | 4,197,535 | 3,946,779 | 1,260,844 |
| Standalone native default | 47 / 38 | 33.40 / 21.70 | 1,684,505 | 1,519,218 | 909,147 |
| Standalone with fallback | 53 / 43 | 45.25 / 32.24 | 4,150,368 | 3,919,480 | 1,245,948 |
| Terrio after extraction | 150 / 131 | 163.84 / 115.42 | 4,177,826 | 3,944,419 | 1,260,393 |

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
- After: `69d63717c3256a48a5e0b1d80f56f24fa3123846` (optional fallback and transport concurrency fix).
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

## Native first-use cost

Fifteen fresh processes per configuration called the same compiled probe, loading
the library before the timer. The first timed EPSG:4326-to-3857 call includes
lazy table decompression/indexing, CRS parsing and transformer construction. Each
process then repeated parsing/construction/transformation 100 times. Configurations
were alternated on the same host, with `RAYON_NUM_THREADS=2`, network disabled and
warm filesystem caches. This measures caller-visible first use; it is not an
isolated decoder-throughput measurement.

| Consumer | First-call median | First-call min–max | Repeated-call median |
| --- | ---: | ---: | ---: |
| Terrio before extraction | 10.62 ms | 10.11–11.35 ms | 3.68 µs |
| Standalone native default | 18.01 ms | 17.50–18.65 ms | 3.42 µs |
| Standalone with fallback | 18.12 ms | 17.49–19.43 ms | 3.57 µs |
| Terrio after extraction | 17.94 ms | 17.44–19.36 ms | 3.60 µs |

Portable decoding adds roughly 7 ms to the first call in this experiment; it
removes the duplicate table-decoder dependency and uses the same implementation
on both targets. The measured repeated-call cost is similar. The selected default
keeps that initialization tradeoff explicit. [Raw trials and scope](benchmarks/crs-startup.json)
use the same compiled source revisions and release artifacts as the footprint
matrix above.

To repeat an individual trial against one of the script's native probe libraries
(`libcrs_footprint_probe.so` on Linux), start a fresh interpreter each time:

```sh
RAYON_NUM_THREADS=2 python - /path/to/libcrs_footprint_probe.so <<'PY'
import ctypes, json, sys, time
library = ctypes.CDLL(sys.argv[1])
transform = library.transform
transform.argtypes = [ctypes.c_double, ctypes.c_double]
transform.restype = ctypes.c_double
start = time.perf_counter_ns()
x = transform(10.0, 40.0)
first = time.perf_counter_ns() - start
assert abs(x - 1113194.9079327357) < 1e-8
start = time.perf_counter_ns()
for _ in range(100):
    transform(10.0, 40.0)
steady = (time.perf_counter_ns() - start) / 100
print(json.dumps({"first_ns": first, "steady_ns": steady}))
PY
```

The tables are immutable build-time inputs, and the decoder's existing failure
handling is unchanged. EPSG/grid conformance and package verification exercise
the bundled tables; no caller-supplied table bytes enter this decoder.

## Codec audit and portability

Bundled EPSG/ESRI/datum tables use `ruzstd` on native and WASM targets. Removing
the separate native table decoder removes the standalone crate's direct
`zstd` 0.14 dependency. Native TIFF decoding still uses its existing Zstd 0.13
backend, through the default `libzstd` feature; without it the crate has no C dependency.
Full Terrio retains its own Zstd dependency for its formats, behind its `libzstd` feature, and
does not enable this crate's: within Terrio, Zstandard TIFF grids are not read.

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
