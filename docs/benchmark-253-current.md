# Current #253 benchmark recheck (2026-11-02)

Terrio 0.6.1 at `328c3cc2` was built in release mode and compared with GDAL
3.13.3 / PROJ 9.9.0 on Linux/glibc 2.44, Intel Xeon w7-3445. All eight original
public-fixture cases pass the unchanged output gate: pixels, masks and checked band
properties are exact; affine transforms allow absolute 1e-8 / relative 1e-12 error,
and CRSs are compared semantically. This report supersedes
the earlier six-pass timing snapshot for these cases; it does not reproduce the
five unavailable vendor assets. [Raw evidence](benchmark-253-current.json) retains
commands, timing samples, CPU tuning, load estimates and full output diagnostics.

## Timed rerun

Each tool independently selected its lowest median from CPU budgets **1 and 2**,
using a warm-up and three tuning samples per setting. Fresh measurements used
seven samples for each small case and five for each large case, alternating tool
order. Every invocation used a fresh output directory. Linux affinity bounded
all child threads. The one-CPU background-load threshold was unchanged. It applies
to elapsed-time-weighted background estimates pooled across both tools in each
paired series, separately for each tuning budget and for fresh measurements;
it does not cap each tool or invocation independently. All eight rows passed.
These are limited-budget, machine-specific measurements, not a
search for the best settings across all 40 available logical CPUs.

| Task | Terrio median [range] | GDAL median [range] | CPUs T / G | Terrio vs GDAL | Validation |
|---|---|---|---|---|---|
| GeoTIFF → COG (Deflate): USGS 3DEP tile | 1.24 s [1.21 s–1.28 s] | 816 ms [787 ms–844 ms] | 2 / 2 | 1.52× slower | pass |
| GeoTIFF → Zarr: SRTM 90 m | 96 ms [93 ms–97 ms] | 72 ms [70 ms–80 ms] | 2 / 2 | 1.33× slower | pass |
| Warp to NAD83 / UTM 13N, bilinear: USGS 3DEP tile | 675 ms [663 ms–689 ms] | 416 ms [408 ms–421 ms] | 2 / 2 | 1.62× slower | pass |
| Mosaic 4 USGS 3DEP tiles → VRT | 23 ms [22 ms–24 ms] | 35 ms [34 ms–37 ms] | 1 / 1 | 1.51× faster | pass |
| Shapefile → GeoPackage: 7,342 populated places | 75 ms [70 ms–78 ms] | 81 ms [79 ms–83 ms] | 2 / 1 | 1.08× faster | pass |
| Filter + reproject: US counties → EPSG:5070 | 58 ms [55 ms–59 ms] | 51 ms [48 ms–52 ms] | 2 / 1 | 1.14× slower | pass |
| Hillshade of the mosaic | 3.50 s [3.49 s–3.55 s] | 4.04 s [4.00 s–4.05 s] | 2 / 1 | 1.15× faster | pass |
| Slope of the mosaic | 4.19 s [4.12 s–4.47 s] | 4.87 s [4.80 s–5.24 s] | 2 / 1 | 1.16× faster | pass |

The VRT row's fresh-measurement background mean was 0.94 CPUs, close to the 1.0
limit; individual estimates reached about 1.7 CPUs. Its 1.51× observed median
ratio passes the documented pooled gate, and does not establish a speed advantage
under stronger isolation or on another run.

Both COG commands request fresh average overviews; Terrio selects its GDAL 3.13
COG policy. Warp and terrain select the 3.13 arithmetic profile. Datum-grid
networking is off for both tools. The VRT reference disables synthetic virtual
overviews, preserving the established base-mosaic scope. See the
[benchmark method](benchmarks.md#method) and [correctness gates](benchmarks.md#correctness-gates).

## Complete output and thread checks

All eight cases were separately generated with each tool at **1, 2, 4 and 12
CPUs**, with matching pool settings and Linux affinity. The 32 cross-engine
comparisons pass the same output gate. Each higher-thread output also passes against
that tool's single-thread output: 24 Terrio and 24 GDAL comparisons. This checks
pixels and metadata, not file-byte identity, and makes no claim about other
machines or CPU architectures.

The single-thread pairs were additionally scanned with
`diagnose_benchmark_outputs.py` across every pixel and overview: nine raster
levels, 508,051,812 stored samples and 507,793,492 common-valid finite samples.
All pixel, mask and nonfinite difference counts are zero. Maximum absolute and
relative common-valid errors are zero; Float32 ULP distances are zero (ULP is not
applicable to the integer outputs). Types, nodata, color roles, scale/offset and
units pass the output gate; CRSs are semantically equal and affine transforms use
the established absolute 1e-8 / relative 1e-12 allowance. GeoTIFF byte layouts and
arbitrary metadata are outside that gate.

The populated-places conversion matches all 7,342 features, field definitions,
attributes and normalized geometries. The county conversion matches all 95
features; its keyed diagnostic also records zero differing fields/geometries,
zero coordinate error and no unmatched FIPS values. Generated FIDs, layer names,
ring orientation/start and multipart order remain excluded as documented.

## Regression evidence and remaining scope

The current release CLI passes all 26 `benchmark-metadata` / `benchmark-compat`
conformance cases against GDAL 3.13.3. These cover GCP-only georeferencing, color
and schema preservation, COG pyramids, versioned terrain, WebP options/alpha and
eight JPEG XL sample-depth/layout combinations. Current-main
[macOS and Windows CI](https://github.com/opengeos/terrio/actions/runs/41385648562)
also passed the native GCP, WebP and eight-layout JPEG XL regressions at the same
source commit. These fixtures do not certify the vendor's original files.

The [vendor article](https://bitruvius.com/blog/engineered-not-prompted/) was
rechecked for this audit. It offers raw data and scripts on request and provides
no download links for the five reproductions. Their exact source assets,
commands, expected outputs and, for the codec failures, platform/decoder details
remain unavailable. Issue #253 must remain open for that scope. The separate
[GDAL statistics observation](benchmark-parity.md#gdal-multithreaded-statistics)
is not a Terrio defect and is outside these eight timed cases.
