# Benchmark output investigation (#253)

The discrepancies in [#253](https://github.com/opengeos/terrio/issues/253) were
reproduced on `9ed48c6` against GDAL 3.13.3. The
[raw investigation](benchmark-parity-results.json) preserves baseline measurements
and records the corrected results. Comparisons scan complete rasters and every
overview, including masks, types, georeferencing and metadata. Benchmark pixel and
geometry gates remain exact.

## Corrections

- **GCP-only TIFF to COG:** preserve all 1,599 GCPs, their CRS and pixels without
  inventing an affine transform. Full, window, overview, streamed and byte reads
  retain GCPs; crops/resizes adjust pixel/line coordinates. Assigning an affine
  transform clears inherited GCPs. The real SRTM reproduction exercises the ICEYE
  failure pattern without claiming access to that vendor asset.
- **SRTM to Zarr:** retain `COLOR_INTERPRETATION` as a scalar for a single band
  and an array for multiple bands. All 10,523,085 pixels and masks already agreed;
  the missing Gray interpretation is corrected. TIFF, Zarr and lazy band selection
  preserve explicit color roles.
- **Four-tile VRT:** inherit selected source color interpretations. All 52,012,944
  base pixels and masks agreed. The reference command explicitly disables GDAL's
  synthetic virtual overviews (`VRT_VIRTUAL_OVERVIEWS=NO`) to compare the supported
  base-resolution mosaic; exact overview checks remain enabled.
- **Populated places to GeoPackage:** DBF numeric fields with declared width ten
  or more retain Integer64 columns, even when current values fit 32 bits. All
  7,342 features, field definitions, attributes and normalized geometries agree.
- **COG overview pyramid:** `COG_OVERVIEW_POLICY=GDAL_3_13` recursively reduces
  rounded levels and truncates halved dimensions. Every USGS base/overview pixel
  now agrees, including the final 451² level. An odd-sized real optical crop also
  has a complete, exact pyramid. The default `SOURCE` policy retains independent
  source reduction. The new profile supports nearest/average and nodata, without
  alpha bands or explicit masks. Its streamed implementation retains the current
  slab and an unfinished resampling footprint, rather than the whole raster;
  one-row and uneven slabs produce byte-identical files.
- **Slope and hillshade:** explicit `--kernel-version 3.13` reproduces GDAL's
  Float32 terrain arithmetic for slope and standard hillshade, Horn and
  Zevenbergen–Thorne. CLI, Rust, Python (`kernel_version`) and JavaScript
  (`kernelVersion`) expose the option; default `3.12` preserves existing results.
  Both full 14412² benchmark outputs now agree exactly with GDAL 3.13.3, including
  all 207,648,100 common-valid pixels and their masks. The benchmark chooses the
  installed reference version explicitly. Platform transcendental/SIMD rounding
  can vary; these measurements establish fixture-specific exactness. On x86,
  reciprocal-square-root estimates can differ between Intel and AMD CPUs even
  after refinement; non-x86 builds use scalar square roots. Hillshade is therefore
  not guaranteed bit-reproducible across CPUs or architectures.
- **WebP options:** independently reproduced ignored lossless/quality settings.
  Native pure Rust encoding now honors `WEBP_LOSSLESS=YES` and `WEBP_LEVEL=0..100`
  for Byte RGB/RGBA, including WASM. Lossless output preserves transparent color
  samples; lossy RGBA stores alpha losslessly. Independent GDAL reads verify the
  sample layout, alpha and masks; different quality requests produce different
  decoded images. Lossy WebP is not claimed pixel-identical to a different encoder.
- **JPEG XL:** added native round trips and independent GDAL decoding for Byte
  and UInt16, one/three bands, strips and tiles. All eight Linux variants preserve
  every sample. The matching eight native round trips also pass on macOS and
  Windows in [platform CI](https://github.com/opengeos/terrio/actions/runs/40162521926)
  at `4aa9762`. No decoder defect was reproduced on these fixtures; the unavailable
  vendor commands remain unverified.

## Residual numerical differences

| Case | Before | After |
|---|---|---|
| Bilinear USGS warp | 12,551,806 differing valid pixels; max 7.502197 m | Matching 3.13 kernel: six pixels; max 0.000244140625 m, one Float32 ULP |
| Counties to EPSG:5070 | 95 differing geometries; max 7.21775e-9 m | Modern authalic-q formula: max 3.026798e-9 m; schema, attributes and coordinate counts agree |
| Slope | 95,360,773 differing valid pixels; max four ULPs | Zero differences with 3.13 profile |
| Hillshade | 1,675 differing valid pixels; max one Byte level | Zero differences with 3.13 profile |

The warp and county outputs still fail the exact benchmark gate and receive no
speed ratio. Projection arithmetic uses platform transcendental implementations:
for an identical authalic-q intermediate, Rust's `atanh` and the reference C
library differ by one Float64 ULP. Replacing the older logarithmic ratio with
PROJ's modern `atanh` formulation also avoids cancellation near the equator; a
regression checks that precision independently of the benchmark. These residuals
are documented numerical differences, not a claim of exact parity. Existing warp
conformance retains its explicit 0.002 m tolerance; benchmark gates are not relaxed.

Error statistics use common-valid finite samples; differing masks and nonfinite
samples are counted separately. Relative error divides by the absolute reference
value, with a machine-tiny floor. ULP distances use the stored floating type and
identify signed zeros. Complex differences retain both components. Reports encode
nonfinite measurements explicitly as strings so their JSON remains valid.

## Reproduce

Use matching GDAL CLI/Python installations and `PROJ_DATA`, and keep outputs
outside the repository:

```sh
python scripts/conformance.py ../terrio-data --only benchmark-metadata --strict
python scripts/conformance.py ../terrio-data --only benchmark-compat --strict
python scripts/diagnose_benchmark_outputs.py terrio.tif gdal.tif --output differences.json
python scripts/diagnose_benchmark_outputs.py terrio.gpkg gdal.gpkg --key FIPS
python -m unittest discover -s scripts/tests -v
```

`benchmark-metadata` checks four exact metadata/GCP/conversion cases.
`benchmark-compat` checks recursive DEM/optical COGs, versioned terrain, WebP
lossless/quality/alpha and eight JPEG XL layouts using independent GDAL reads.
See [testing](testing.md#benchmark-mismatch-regressions) for supported scope.

## Remaining external scope

The exact ICEYE, vendor elevation and optical assets/commands were unavailable.
Their reported failure patterns now have independent regressions, but those
particular assets cannot be certified. The native JPEG XL regressions pass on macOS and Windows, but the precise vendor
failure still requires its reproduction.
The GDAL multithreaded statistics observation is an upstream report, not a Terrio
fix. Keep the multi-report issue open while these items or the exact numerical
parity requirement remain outstanding.

## Final validation and timed reruns

Workspace tests (483 core tests passed, one ignored, plus workspace integration
and binding tests), Clippy with `-D warnings`, formatting and full pre-commit
checks pass. The 45 selected GDAL conformance cases are identical, including 19
compatibility/codec cases. Python passes 55 API tests; release WASM passes 99
JavaScript tests with two skips. The CLI and Python bindings were built in
release mode; WASM uses Rust release optimization without the optional wasm-opt
postpass.

[All eight benchmark cases were rerun](benchmark-253-rerun.md) with fresh timings,
independent tuning at one/two CPUs, seven small-file or five large-file samples,
and the original measurement/output gates. Six cases pass exact validation;
warp and counties still fail. Background activity exceeded the one-CPU limit,
so all speed ratios are withheld. Raw samples remain available for review;
these measurements do not establish new performance claims.
