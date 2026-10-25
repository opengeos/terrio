# Benchmark output investigation (#253)

The benchmark discrepancies in [#253](https://github.com/opengeos/terrio/issues/253)
were reproduced on `9ed48c6` with GDAL 3.13.3 and the fetched real datasets.
[Full measurements](benchmark-parity-results.json) count every base-resolution
pixel and every same-sized overview, rather than stopping at the first strip.
No benchmark pixel or geometry tolerance has been relaxed, and this investigation
does not establish new performance ratios.

## Confirmed defects and corrections

- **GCP-only TIFF to COG:** a new independent 64×64 reproduction with 1,599
  GDAL-written GCPs lost all points and acquired an invented affine transform.
  GCPs now survive lazy, whole-dataset and WASM byte copy paths in shared Rust.
  Cropped/resized reads adjust GCP pixel/line positions. An explicitly assigned
  affine transform takes precedence. Conformance uses a real SRTM crop with
  1,599 assigned GCPs and checks every GCP coordinate, GCP CRS, pixels and absence
  of a replacement affine against GDAL. This confirms the failure pattern without
  claiming reproduction of the unavailable ICEYE asset.

- **SRTM to Zarr:** all 10,523,085 pixels and masks agreed. Terrio omitted GDAL's
  `COLOR_INTERPRETATION` array attribute, producing `Undefined` instead of `Gray`.
  The writer now stores the scalar attribute for a single band and a string array
  for multiple bands, in both Zarr versions. Explicit color choices survive
  TIFF/lazy/window reads, band selection, and Zarr reads/writes.
- **Four-tile USGS VRT:** all 52,012,944 base pixels and masks agreed. The VRT
  omitted `ColorInterp`, which now follows each selected source band, including
  separate-band mosaics. GDAL 3.13 also adds a synthetic `OverviewList` by default;
  Terrio's base-resolution mosaic does not. The benchmark now explicitly disables
  GDAL's synthetic virtual overviews with `VRT_VIRTUAL_OVERVIEWS=NO`, keeping both
  commands within the same supported scope. Exact overview checks remain enabled.
- **Populated places to GeoPackage:** `pop_max`, `pop_min`, `pop_other`, and `ne_id`
  were narrowed from declared Integer64 to Integer because their actual values fit
  32 bits. Numeric DBF widths of ten or more now retain 64-bit GeoPackage columns,
  including the shared streaming writer. All 7,342 features are compared against
  `ogr2ogr`, including attributes and normalized geometries.
- **Bilinear warp benchmark:** the default Terrio kernel retains GDAL 3.12 behavior,
  whereas the reference was GDAL 3.13. The benchmark now selects the existing
  matching kernel explicitly, for small and large warps. On the full USGS tile,
  differing valid pixels fall from **12,551,806** to **6**, and maximum elevation
  difference from **7.502197 m** to **0.000244140625 m** (one Float32 ULP).
  The remaining six pixels still fail the benchmark's exact gate. Shared warp
  conformance already uses an explicit version and a documented 0.002 m
  interpolation tolerance; this has not been changed.

## Numerical and documented representation differences

| Case | Full result against GDAL 3.13.3 | Classification |
|---|---|---|
| COG base, 3612² | 0 different pixels; matching masks, type, nodata, CRS and transform | Exact |
| COG first overview, 1806² | 0 different pixels | Exact |
| COG second overview, 903² | 107,679 different; max absolute 0.00048828125 m, max relative 1.19208e-7, one ULP | Direct-source versus recursive averaging |
| COG final overview | Terrio 452²; GDAL 451² | Different pyramid dimension policy; pixelwise comparison has no common grid |
| Counties to EPSG:5070 | All 95 geometries differ; no attribute, schema, feature-key or coordinate-count differences; max coordinate error 7.21775e-9 m | Projection arithmetic rounding |
| Slope, 14412² | 95,360,773 valid pixels differ; max absolute 0.00000762939453125 degrees, max relative 2.84565e-7, max four ULPs | GDAL terrain arithmetic changed between 3.12 and 3.13 |
| Hillshade, 14412² | 1,675 valid pixels differ by at most one Byte level | GDAL terrain arithmetic changed between 3.12 and 3.13 |

The matching terrain grids have 207,648,100 common-valid pixels. No terrain or warp
validity masks differ. The absolute/relative errors above use only common-valid,
finite samples; the diagnostic also counts differences outside that set separately.
Relative error divides by the absolute GDAL value, with a machine-tiny floor.
ULP distance is measured in each stored floating type, treating signed zeros equally.

Terrio's [documented COG policy](rust.md#writing) reduces each overview directly
from original source blocks with unrounded sums and original nodata/alpha weights.
GDAL recursively averages previously rounded overviews: averaging the measured
1806² level in 2×2 groups reproduces every GDAL 903² pixel. GDAL 3.13 repeatedly
halves integer dimensions with truncation; Terrio uses ceiling division. These
outputs are not equivalent under the benchmark contract. The direct-source policy
has been retained; a GDAL-compatible COG pyramid policy remains a separate extension.

A 512×512 real clip from the same mosaic is pixel-identical to GDAL 3.12.2 for
both slope and hillshade. Against GDAL 3.13.3, that clip has 108,806 slope differences
(max 0.000003814697265625 degrees) and one hillshade difference (one Byte level).
This isolates the reference-version effect without treating GDAL as an infallible
accuracy oracle. Full-mosaic terrain files generated at one and two threads were
byte-identical within each implementation. This is a thread check for these
terrain fixtures, not a claim about every operation or platform.

## Reproduce and validate

Use matching GDAL commands, Python bindings and `PROJ_DATA`. Keep generated files
on disk outside the repository. The benchmark case catalog supplies the original
commands; use `--kernel-version 3.12` to reproduce the original warp discrepancy
against GDAL 3.13, or `3.13` for the corrected comparison.

```sh
python scripts/diagnose_benchmark_outputs.py terrio.tif gdal.tif --output differences.json
python scripts/diagnose_benchmark_outputs.py terrio.gpkg gdal.gpkg --key FIPS
python scripts/conformance.py ../terrio-data --only benchmark-metadata --strict
python -m unittest discover -s scripts/tests -v
```

The `benchmark-metadata` conformance suite checks GCP-only COG preservation, every SRTM Zarr pixel, every
four-tile VRT base pixel, all masks and descriptive comparison properties, and all
populated-place fields/features exactly. It disables only the GDAL-only synthetic
VRT pyramid, explicitly in the reference command. Existing COG source-order tests
and versioned warp conformance retain their documented semantics.

## Reports requiring external reproductions

The ICEYE 1,599-GCP case, the external exact elevation warps, the optical overview
pyramid, WebP settings/checksums and cross-platform JPEG XL reports lack the precise
assets and commands. The GCP-loss pattern was independently reproduced and fixed, but the particular
ICEYE asset remains unverified. The other external reports cannot be confirmed or
cleared by these benchmark fixtures.
The issue should remain open for those reports and any desired additional GDAL
compatibility policies. The separate GDAL 3.13.3 multithreaded statistics observation
has not been folded into a Terrio defect or weakened validation gate.

## Validation of the corrections

Workspace tests, workspace Clippy with `-D warnings`, formatting and pre-commit
checks pass. The Rust library has 476 passing tests and one ignored test. All four
registered GDAL conformance cases are exact, including the GCP CRS and every GCP
coordinate. The Python bindings pass 14 selected real-data/GCP tests, the benchmark
harness and diagnostics pass 31 tests, and the release WebAssembly module passes
98 JavaScript tests with two skips. An additional JavaScript byte-to-COG copy of
the independent 1,599-GCP TIFF preserves the GCPs, CRS and pixels without an affine.

The final CLI and Python bindings were built in release mode. WebAssembly was
built with Rust release optimizations, without the optional `wasm-opt` post-pass.
These are correctness checks; no new performance or package-size claim is made.
