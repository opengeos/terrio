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
- **COG overview pyramid:** COG overviews recursively reduce rounded levels and
  truncate halved dimensions, as GDAL's COG driver does. Every USGS base/overview
  pixel agreed, including the final 451² level. An odd-sized real optical crop also
  had a complete, exact pyramid. Streamed writes do not retain the whole raster;
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

| Case | Before | After, GDAL 3.13.3 and PROJ 9.9.0 on glibc 2.44 |
|---|---|---|
| Bilinear USGS warp | 12,551,806 differing valid pixels; max 7.502197 m | Matching 3.13 kernel: zero of 12,777,305 pixels; masks identical |
| Counties to EPSG:5070 | 95 differing geometries; max 7.21775e-9 m | Zero differing geometries; every coordinate bit-identical |
| Slope | 95,360,773 differing valid pixels; max four ULPs | Zero differences with 3.13 profile |
| Hillshade | 1,675 differing valid pixels; max one Byte level | Zero differences with 3.13 profile |

Two causes accounted for the last warp and county differences:

- **C library functions:** PROJ calls the C library's `asinh` and `atanh`, whose
  results change between glibc releases. Terrio carried translations matching glibc
  2.39, which rounded differently from glibc 2.44. On linux-gnu it now calls the
  installed C library, so it rounds as a PROJ using that library does. The same
  binary can therefore give different last bits under different glibc releases. Other
  platforms keep Rust's `f64` functions, so these results are not claimed there.
- **Inverted pipelines:** GDAL's reverse transformation inverts the forward pipeline,
  so PROJ divides by the forward `unitconvert` factor; multiplying by the reciprocal
  rounds about a tenth of the degrees differently. The warper's reverse
  transformation now does the same. Web Mercator and WGS 84 are excepted both ways, as GDAL does not invert a pipeline
  for that pair.
  Other Terrio transformations are unchanged, as `ogr2ogr` runs a forward pipeline.

These results are fixture-specific. Other CRS pairs can retain last-bit differences:
a NAD83 to EPSG:5070 warp of the same DEM still has 11 of 15,160,628 pixels differing
by one Float32 ULP (0.000244140625 m). Existing warp conformance keeps its explicit
0.002 m tolerance; the two cases above are checked exactly by `benchmark-compat`.

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
Keep the multi-report issue open while these items remain outstanding.

## GDAL multithreaded statistics

On `srtm90.tif` (Int16, Deflate, 512×512 tiles), GDAL 3.13.3's `gdalinfo -stats`
reads only about one thread's share of the pixels. Each row is a fresh run with PAM
disabled:

| `GDAL_NUM_THREADS` | Minimum | Maximum | `STATISTICS_VALID_PERCENT` |
|---:|---:|---:|---:|
| 1 | 15 | 4338 | 100 |
| 2 | 18 | 4320 | 50.02 |
| 3 | 23 | 4237 | 33.39 |
| 4 | 25 | 4148 | 25.03 |
| 8 | 26 | 4148 | 12.54 |
| 16 | 28 | 4148 | 6.288 |

Terrio and a NumPy reduction both give 15 and 4338, so this is not a Terrio defect.
`usgs_dem.tif` (Float32) reports 100 percent at eight threads, so not every raster is
affected. The cause inside GDAL has not been isolated, and this has not been reported
upstream.

## Final validation and timed reruns

Workspace tests (484 core tests passed, one ignored, plus workspace integration
and binding tests), Clippy with `-D warnings`, formatting and full pre-commit
checks pass. The 46 selected GDAL conformance cases are identical, including 20
compatibility/codec cases. Python passes 55 API tests; release WASM passes 99
JavaScript tests with two skips. The CLI and Python bindings were built in
release mode; WASM uses Rust release optimization without the optional wasm-opt
postpass.

The rerun below predates the warp and county corrections above.

**Recheck on `afc7539a`.** All eight cases pass the exact output gate against GDAL
3.13.3 ([raw results](benchmark-253-recheck.json)). Other jobs were running, so seven of
the eight exceeded the background CPU limit and no new speed ratios are claimed from this
run. The recheck found two faults in the comparison scripts, not in Terrio:

- `benchmark.py` asked only GDAL for average COG overviews. Terrio's default became cubic,
  matching GDAL's, in #406, so the overview comparison failed. Both now request average,
  and every level agrees. With neither requesting it, both use cubic and also agree.
- `compare_warp_policy_with_gdal.py` turned the network off only for Terrio. With
  `PROJ_NETWORK=ON`, GDAL fetched a Colorado datum grid for NAD83 to WGS 84, and 15 of the
  22 cases differed. The reference now runs with the network off, and all 22 are identical.

[All eight benchmark cases were rerun](benchmark-253-rerun.md) with fresh timings,
independent tuning at one/two CPUs, seven small-file or five large-file samples,
and the original measurement/output gates. Six cases pass exact validation;
warp and counties still fail. Background activity exceeded the one-CPU limit,
so all speed ratios are withheld. Raw samples remain available for review;
these measurements do not establish new performance claims.

The review regressions also check GCP-only sources converted to AAIGrid through
whole and lazy paths, preserve explicitly supplied unsupported-option errors,
compare decoded WebP pixels at both quality levels, and assert JPEG XL tile/strip
block dimensions. A GDAL-authored metadata case verifies its additional escaped
text layer, including literal entity strings. The portable 3×3 slope unit check
allows one Float32 ULP for libm rounding; conformance and benchmark gates stay exact.
