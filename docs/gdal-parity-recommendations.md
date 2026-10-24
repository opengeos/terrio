# Recommended GDAL parity work

Initial assessment date: 2026-10-24. Based on the checkout before PR #239 and official GDAL
documentation. Priorities and effort are engineering judgments, not measured estimates.
Implementation is tracked in [#238](https://github.com/opengeos/terrio/issues/238)
and the [roadmap](../ROADMAP.md#next-practical-gdal-feature-parity).

## Existing foundation

Terrio already implements common raster/vector formats, cloud and archive access,
COGs, window reads, streaming vector conversion, streaming warp/mosaic/terrain
outputs, GCP/RPC transformers, the full warp resampler list, STAC/STACIT/STACTA,
GRIB, netCDF/HDF5/Zarr, Arrow, topology, contours and zonal statistics. These are
documented in [the roadmap](../ROADMAP.md), [formats](formats.md) and
[CLI documentation](cli.md). Recommendations below extend these implementations.
The optional GDAL backend provides additional format access; it does not constitute
native implementation of the proposed operations.

## Prioritized additions

| Order | Feature | Suggested first deliverable | Relative effort |
|---|---|---|---|
| 1 | Fill nodata (implemented in [#239](https://github.com/opengeos/terrio/pull/239)) | GDAL-compatible inverse-distance and nearest filling, validity mask, maximum pixel distance and smoothing | Medium |
| 2 | Overview-aware warping | Explicit overview selection and automatic selection for downsampling, with a full-resolution override | Medium |
| 3 | Geolocation-array warping | Warp swaths using longitude/latitude arrays already exposed by the drivers | Large |
| 4 | Richer VRT interoperability | Warped VRTs, followed by built-in derived bands and kernel filters | Large, incremental |
| 5 | Richer vector SQL | Computed output columns, DISTINCT, summary aggregates and limited attribute joins | Medium–large |
| 6 | Viewshed | Single-observer visibility and minimum visible height, with observer/target heights and curvature controls | Medium–large |
| 7 | Indexed raster catalogs | A GTI-compatible GeoPackage/FlatGeobuf tile index, spatial source selection and bounded source cache | Large |
| 8 | Pansharpening | GDAL-compatible weighted Brovey operation, band weights, nodata and resampling controls | Medium–large |
| 9 | Native multidimensional API | Named dimensions, coordinate arrays and lazy time/depth slicing across netCDF/HDF5/Zarr | Large, architectural |

### 1. Fill nodata

Implemented in [PR #239](https://github.com/opengeos/terrio/pull/239): a shared Rust
operation exposed through CLI/MCP, Python and JavaScript, with GDAL comparison tests.
The following describes the gap identified in the initial assessment.

Before PR #239, the CLI command inventory and core operations contained no fill-nodata operation.
The rasterio-compatible CLI explicitly rejects `fillnodata` in
[`rio/cli.py`](../python/python/terrio/rio/cli.py). The xarray accessor's
`interpolate_na` is an existing interpolation surface, but does not supply this
GDAL-compatible core operation.

The shared Rust operation repairs holes in DEMs and continuous imagery before contours, terrain
analysis or reprojection. GDAL supports inverse-distance or nearest filling,
an explicit mask, search distance and smoothing.
[GDAL fill-nodata](https://gdal.org/en/stable/programs/gdal_fillnodata.html).

Acceptance: compare pixels with GDAL for interior holes, edge holes, disconnected
valid regions, NaN nodata and explicit masks. Valid source pixels must be preserved.

### 2. Overview-aware warping

Implemented in [PR #242](https://github.com/opengeos/terrio/pull/242): the warper
selects `AUTO`, `AUTO-n`, `NONE` or a forced source level after planning the output
grid from full-resolution metadata. CLI/MCP, Python paths/lazy handles and
JavaScript bytes/datasets retain stored pyramids. The conformance suite checks
GDAL's selected pixels and measures actual HTTP response bytes for a USGS COG.
Mosaics and cutlines currently load each selected level before compositing;
single-source warps read only the required source windows.
[GDAL warp](https://gdal.org/en/stable/programs/gdalwarp.html).

Acceptance: compare selected levels and output pixels with GDAL under matching
settings; measure transferred bytes for remote COG downsampling. Preserve an
explicit full-resolution path because overview generation changes sampled values.

### 3. Geolocation-array warping

Implemented in [PR #241](https://github.com/opengeos/terrio/pull/241): the warper
uses indexed bilinear geolocation cells for scientific swaths, including
subsampled arrays, pixel-center conventions, invalid coordinates and antimeridian
crossings. Rust, CLI/MCP, Python and JavaScript expose the capability. The
supported subset and in-memory indexing are documented. Sentinel-5P and
full-resolution valid arrays match GDAL exactly; subsampled/invalid-node cases
retain 9–32 boundary-mask differences per 160×100 band, with common-valid values
identical. [GDAL warp](https://gdal.org/en/stable/programs/gdalwarp.html).

Acceptance: use real swath products already represented in the scientific-format
fixtures; compare bounds, valid-data coverage and resampled values. Include
subsampled coordinate arrays, pixel-center conventions, invalid coordinates and
antimeridian crossings.

### 4. Richer VRT interoperability

The `VirtualRaster` reader now extends existing simple/complex/averaged sources
with serialized warped VRTs (affine/reprojection, GCP/TPS/homography/RPC), selected
scalar derived functions, full/separable kernel filters and lazy masks. Rust
parses/serializes the descriptions; CLI/MCP and Python write warped VRT files
without materializing pixels, and Python/JavaScript accept uploaded source maps.
Whole/cropped GDAL cross-reads are registered in the `vrt` conformance suite;
a counted source regression checks bounded pixel reads. See the
[supported subset](cli.md#serialized-virtual-rasters) for remaining serialized
cutline/alpha/geolocation and external-function limitations. Pansharpened VRTs
follow the pansharpening implementation. [GDAL VRT](https://gdal.org/en/stable/drivers/raster/vrt.html).

Acceptance: open GDAL-generated VRT fixtures and compare window reads, masks and
pixels; verify Terrio-generated files in GDAL. Keep evaluation lazy and implement
built-in operations before considering arbitrary user-supplied pixel functions.

### 5. Richer vector SQL

Implemented in [PR #245](https://github.com/opengeos/terrio/pull/245):
the [documented SQL subset](sql.md) supports computed SELECT columns,
single-field DISTINCT, COUNT/SUM/AVG/MIN/MAX summaries and same-dataset attribute LEFT JOINs.
Duplicate join keys use OGR's first matching secondary row; null keys and unmatched
rows retain the primary feature with null joined fields. Rust, CLI/MCP, Python
and JavaScript share the subset. Queries retain primary geometry except DISTINCT
and summaries. GROUP BY and spatial SQL remain separate extensions.
[GDAL OGR SQL](https://gdal.org/en/stable/user/ogr_sql_dialect.html).

Acceptance: compare values, nulls, field types and geometry retention with OGR SQL,
including mixed-case fields and duplicate join keys. Preserve streaming for
operations that do not require materialization.

### 6. Viewshed

Implemented in [PR #240](https://github.com/opengeos/terrio/pull/240): shared Rust
single-observer binary visibility and minimum visible height, with observer/target
heights, maximum distance, curvature/refraction and projected-unit handling.
CLI/MCP, Python and JavaScript expose the operation. Real projected DEM comparisons
are registered in conformance; numeric input nodata is treated as elevation, as in
GDAL, and nonfinite elevations must be filled or cropped first. Cumulative
visibility remains outside this subset.
[GDAL viewshed](https://gdal.org/en/stable/programs/gdal_viewshed.html).

Acceptance: compare with GDAL on projected DEMs, hills/ridges, maximum distances
and curvature settings. Define input nodata behavior explicitly: GDAL documents
that it does not specially process input nodata cells.

### 7. Indexed raster catalogs

Terrio has mosaics and STAC mosaics, but its native driver registry has no GTI
driver. Add a spatially indexed raster catalog using existing GeoPackage or
FlatGeobuf readers. Store source paths and footprints, query only intersecting
sources, and expose ordering and cache limits. GDAL's GTI driver uses vector
indexes for large virtual mosaics and supports mixed source CRSs and compositing
order. This is a scale-oriented extension to mosaics.
[GDAL GTI](https://gdal.org/en/stable/drivers/raster/gti.html).

Acceptance: cross-read tile indexes with GDAL; compare overlap, alpha and nodata
behavior. Benchmark catalog startup and a small window over a large source list,
including files opened and bytes fetched, rather than only whole-mosaic throughput.

### 8. Pansharpening

No native pansharpen command appears in the inventory. Build on the existing
resamplers and raster I/O. Start with GDAL-compatible weighted Brovey processing,
explicit spectral weights, nodata, bit depth and extent handling. Add virtual
output after the VRT extensions. GDAL produces either materialized rasters or
pansharpened VRTs.
[GDAL pansharpen](https://gdal.org/en/stable/programs/gdal_pansharpen.html),
[pansharpened VRT algorithm](https://gdal.org/en/stable/drivers/raster/vrt.html#pansharpened-vrt).

Acceptance: compare against GDAL on real panchromatic/multispectral pairs, unequal
resolutions, masked borders, explicit weights and integer output clipping.

### 9. Native multidimensional API

The native [multidimensional API](multidim.md) now exposes groups, named dimensions,
coordinate arrays and lazy positive-stride slicing/transposition across numeric
netCDF/HDF5 and Zarr arrays. Rust, CLI/MCP, Python and JavaScript bindings retain
packed values and native integer precision. Existing raster views remain available.
General string/compound arrays are listed as unsupported; netCDF snapshots currently
materialize numeric arrays, while Zarr snapshots iterate chunks.

Conformance compares real NOAA wind, JPSS geolocation, Sentinel-5P and SRTM arrays
with GDAL MD and xarray. A wind-derived four-dimensional cube checks time/depth
slices, coordinate/chunk/unlimited-axis preservation across netCDF–Zarr snapshots,
and reads that skip damaged unselected chunks. A counting-storage regression measures
selected hyperslab I/O. The documented numeric scope follows
[GDAL's multidimensional model](https://gdal.org/en/stable/user/multidim_raster_data_model.html).

## Suggested sequence

Start with overview-aware warping. Next implement geolocation
warping and richer VRT support to complete existing reader workflows. SQL,
viewshed and pansharpening can ship independently. Schedule indexed catalogs and
the multidimensional model according to demand for continental mosaics and
scientific data cubes, respectively.

Other worthwhile follow-ups are weighted/fractional zonal statistics, OGC API
Features access and overview refresh tooling. Avoid prioritizing proprietary
drivers and core database connections: the current roadmap explicitly excludes
them and provides an optional GDAL backend for the long tail.

The initial recommendation-only assessment used source and documentation inspection.
Fill-nodata implementation validation now includes Rust regression tests, Python
comparisons with GDAL 3.12.2, real USGS terrain comparisons in the `dem-calc`
conformance suite, and JavaScript tests of the WebAssembly build. The separate
`terrio.rio` compatibility CLI still rejects `fillnodata`; the new operation is
available through `terrio.fill_nodata` and `terrio fill-nodata`. Effort, demand and
performance benefits for the remaining recommendations are estimates.
