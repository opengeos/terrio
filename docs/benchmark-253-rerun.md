# Issue #253 benchmark rerun

All eight affected cases were rerun on 2026-10-25 against GDAL 3.13.3 with a release
build of the fixes in this PR. [Raw samples and commands](benchmark-253-rerun.json)
include tuning, measurement, CPU activity, validation and source/binary provenance.
Samples cover `4aa9762`; subsequent option/container validation and XML parser
review fixes are checked separately by conformance and binding tests.

Each tool was tuned independently at CPU budgets **1 and 2**, with a warm-up and
three samples per budget. Fresh measurements alternate tool order, with seven
small-file samples and five full-mosaic samples per tool. These are bounded CPU
comparisons, not a search across every available CPU budget. Builds were stopped
during measurement.

**No speed ratios are eligible.** Background CPU use exceeded the unchanged
one-CPU limit. Warp and county outputs also fail the unchanged exact gate. The
times below are diagnostic observations from this run, not supported speed claims.

| Task | Terrio median [range], seconds | GDAL median [range], seconds | CPUs T / G | Exact output |
|---|---|---|---|---|
| GeoTIFF → COG (Deflate): USGS 3DEP tile | 1.141001 [1.128120–1.286045] | 0.841566 [0.811347–0.889717] | 2 / 2 | Pass |
| GeoTIFF → Zarr: SRTM 90 m | 0.092859 [0.088120–0.192864] | 0.077722 [0.074042–0.079573] | 2 / 2 | Pass |
| Warp to NAD83 / UTM 13N, bilinear: USGS 3DEP tile | 0.670798 [0.656635–0.719812] | 0.426644 [0.421437–0.471108] | 2 / 2 | Fail: documented numerical residual |
| Mosaic 4 USGS 3DEP tiles → VRT | 0.015807 [0.014210–0.016968] | 0.037937 [0.037166–0.040366] | 1 / 2 | Pass |
| Shapefile → GeoPackage: 7,342 populated places | 0.068513 [0.066878–0.069395] | 0.085215 [0.084758–0.087766] | 2 / 2 | Pass |
| Filter + reproject: US counties → EPSG:5070 | 0.048642 [0.048538–0.050898] | 0.053039 [0.052115–0.053597] | 2 / 1 | Fail: documented numerical residual |
| Hillshade of the mosaic | 3.643706 [3.606036–3.705511] | 4.193234 [4.171679–4.225636] | 2 / 1 | Pass |
| Slope of the mosaic | 4.266117 [4.264566–4.310865] | 5.132763 [5.113271–5.224347] | 2 / 1 | Pass |

COG, Zarr, VRT, populated places, full-mosaic hillshade and full-mosaic slope pass
exact output validation. Warp retains six one-ULP pixels; counties retain
nanometre-scale coordinate rounding. See the [investigation](benchmark-parity.md)
for complete pixel/geometry error counts and supported compatibility profiles.
