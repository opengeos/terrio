# Testing Against GDAL

GDAL is terrio's reference: every driver and operation is compared with GDAL 3.12 on real-world
files, and the comparisons run in continuous integration on every pull request. Pull requests
compare the point cloud filters with PDAL's on `autzen_trim.las` only; pushes to `main` on the
Madison lidar (4 million points) too (`scripts/compare_pointclouds.py --quick` skips it).

## Test data

`scripts/fetch_test_data.sh DIR` downloads public data and derives copies in other formats with
GDAL:

- Natural Earth countries and populated places (Shapefile, zipped)
- USGS 3DEP 1 arc-second elevation tiles (GeoTIFF with LZW and a floating-point predictor)
- NASA ECOSTRESS land surface temperature and OPERA DSWx water (user-defined UTM, palette images)
- Landsat and SRTM GeoTIFFs, an NLCD land-cover subset (custom Albers)
- US counties (FlatGeobuf), New York City boroughs (ESRI `.prj` in US feet)
- Las Vegas buildings and roads, a hiking GPS trace (CSV), a Liechtenstein OpenStreetMap extract
- netCDF wind fields, and the same rasters rewritten by GDAL as Zarr, JPEG 2000, ERDAS Imagine,
  ENVI, Esri grids, GeoPackage tiles and more
- HDF5 and netCDF-4 products: JPSS ATMS and VIIRS sensor data records (NOAA open data) and a
  Sentinel-5P methane product with nested groups
- Sentinel-2 L1C and L2A band files (JPEG 2000 with GMLJP2 boxes) and GDAL's JPEG 2000 test files
- GeoTIFFs written by GDAL in each compression it offers beyond the common ones: JPEG (YCbCr and
  RGB), WebP, JPEG XL, LERC (with Deflate and Zstandard, lossy and with NaN masks) and LZMA

## The conformance suite

`scripts/conformance.py` runs every comparison below in one go (and the EPSG accuracy test) and
writes a Markdown report: for each suite and each format or operation, how many cases were
compared and how many came out identical, within tolerance or different, with the details of the
cases that are not identical. The [Conformance](conformance.md) page is that report.

```sh
cargo build --release
python scripts/conformance.py ../terrio-data --output docs/conformance.md
python scripts/conformance.py ../terrio-data --only ops,dem-calc   # some suites
python scripts/conformance.py ../terrio-data --remote --strict     # remote COGs too; fail on a difference
```

## What is compared

| Area | Script | Checks |
|---|---|---|
| Dataset summaries | `scripts/compare_with_gdal.py` (`--pixels`) | size, bands, data types, nodata, geotransform, EPSG code, statistics, pixels; layers, feature counts, fields, extents, geometry types |
| Native multidimensional arrays | `scripts/compare_multidim_with_gdal.py` | GDAL MD and xarray: named axes, indexing variables, native slices/transposes, netCDF/Zarr coordinate/chunk preservation and damaged-unselected-chunk checks |
| NetCDF and HDF5 subdatasets | `scripts/compare_with_gdal.py --subdatasets` | the subdataset lists of netCDF (groups included) and HDF5 files, HDF5 metadata, then every subdataset's summary and pixels |
| Histograms | `scripts/compare_hist_with_gdal.py` | `terrio info --hist` vs `gdalinfo -hist` and `-json -hist`, byte for byte, with `-stats` and with histograms stored in `.aux.xml` sidecars, on every data type |
| Raster round trips | `scripts/raster_roundtrip.py` | GDAL writes → terrio reads; terrio writes → GDAL reads, pixel for pixel; COGs pass GDAL's validator |
| Window and overview reads | `scripts/compare_windows_with_gdal.py` | every overview level of every GeoTIFF, local and remote over `/vsicurl/` |
| Indexed raster catalogs | `scripts/compare_catalogs_with_gdal.py` | Real DEM mosaics in GeoPackage/FlatGeobuf indexes: z-order, nodata, masks, partial alpha, mixed CRS and Terrio-written indexes read by GDAL GTI; an 8x8 HTTP query from 1,002 features checks opens/cache bounds and actual bytes for both implementations |
| Raster operations | `scripts/compare_ops_with_gdal.py` | translate, warp (all resampling methods), mosaic and VRT against `gdal_translate`, `gdalwarp`, `gdalbuildvrt`, `gdal_merge.py` |
| Pixel-wise raster commands | `scripts/compare_raster_pixel_with_gdal.py` | `gdal raster unscale`, `nodata-to-alpha`, `reclassify` (inline and `@file` mappings, ranges, `NO_DATA`, `NAN`, `DEFAULT`, `PASS_THROUGH`, `--ot`) and `as-features` (polygons, points, tables, `--skip-nodata`, `--include-xy`, `--include-row-col`) on USGS, Landsat, ECOSTRESS, NLCD and Sentinel-5P data: size, type, nodata, CRS, color interpretations, scales and every pixel, or every feature's fields and vertices. Registered in conformance as `raster-pixel`. |
| Focal statistics, collars, enhancement | `scripts/compare_focal_with_gdal.py` | `gdal raster neighbors` (every kernel and method, matrices, types, nodata, the whole USGS DEM in strips), `nearblack` and `gdal raster clean-collar` (both algorithms, colors, thresholds, distances, alpha, masks, in-place updates) and `gdalenhance` (equalization, LUT file output and input) on Sentinel-2, Landsat 7 and USGS data: pixels, masks, type, nodata, geotransform, LUT text |
| Vector operations | `scripts/compare_vector_ops_with_gdal.py` | `ogr2ogr` filters, `gdal_rasterize` and `gdal_polygonize.py` |
| Vector topology | `scripts/compare_topology_with_gdal.py` | Tennessee county polygons and Las Vegas roads projected to metre-based CRSs: buffer, all four polygon overlays, dissolve, validity, repair of a deliberately corrupted county boundary and snap cleanup against GDAL/GEOS; attributes, source IDs and CRS, with relative symmetric-difference area tolerance 1e-7 (5e-4 for buffer curves). Registered in conformance as `topology`. |
| Vector dataset commands | `scripts/compare_vector_dataset_with_gdal.py` | `concat` (all modes, source-layer fields, intersection, reprojection; also `ogrmerge.py -single`), `partition` (Hive and flat, string and integer fields, feature limits, patterns), `set-field-type` (a 77-conversion matrix of edge values, every type to every type, and conversions of real fields), `make-point` (GPS trace, places with Z), `create` (`--like`, `--schema`, fields) and `export-schema` (GeoPackage, Shapefile, GeoJSON, FlatGeobuf, GeoParquet) plus `rename-layer`, `edit` and `update`, against GDAL 3.13.3's vector commands/API, OGRLayer::Rename and `ogrinfo -json`; GeoPackage validity and R-tree checks: values exact, coordinates within 1e-9 degrees (or 1e-6 metres) after reprojection; Int16, Float32 and JSON subtypes are reported within tolerance. Registered in conformance as `vector-dataset`. |
| Terrain and raster algebra | `scripts/compare_dem_calc_with_gdal.py` | every `gdaldem` mode and option, `gdal_calc.py` expressions, types, nodata and extents, `gdal_proximity.py`, `gdal_sieve.py` |
| STAC and GeoZarr | `scripts/compare_stac_geozarr.py` (`stac`, `stacta`, `geozarr`) | STACIT subdatasets, mosaic sizes, geotransforms, CRSs and windows on Earth Search; STACTA items (GDAL's test data and pyramids `gdal raster tile` writes from real rasters, local, over HTTP and with a missing tile): summaries, subdatasets, metadata, whole rasters, windows and overviews vs GDAL's STACTA driver; STAC footprint fields vs `ogrinfo`; GeoZarr stores read by GDAL and rioxarray, overviews vs `gdaladdo` |
| Remote Zarr stores | `scripts/compare_remote_zarr.py` | public cloud stores read in place (EOPF Sentinel-2 Zarr v2, EOPF GeoZarr v3 with shards and value codecs, NOAA HRRR on S3, CMIP6 on Google Cloud Storage, Daymet on Azure, a sharded store on a local port): sizes, geotransforms, nodata vs `gdalinfo` (`/vsicurl/`, `/vsis3/`, `/vsigs/`, `/vsiaz/`), windows vs `gdal_translate` and xarray, overviews vs xarray, bytes and requests fetched |
| GRIB | `scripts/compare_grib_with_gdal.py` | GDAL's GRIB test files, ecCodes' and real forecasts (GFS, HRRR, ECMWF open data), band by band: sizes, geotransforms, CRSs, descriptions, `GRIB_*` metadata, nodata values and pixels, with units normalized and as stored |
| Tile archives | `scripts/compare_tiles_with_gdal.py` | MBTiles written by `gdal_translate`/`gdaladdo` and by terrio, pixel for pixel per zoom level; PMTiles pixels; vector tiles read by both tools and written by `ogr2ogr` and terrio: feature counts, areas, lengths per feature key |
| Point clouds | `scripts/compare_pointclouds.py` | every value vs laspy; headers, COPC queries, `writers.gdal` grids and reprojection vs PDAL |
| Arrow | `scripts/compare_arrow_with_gdal.py` | `Layer.to_arrow()` (WKB and native GeoArrow) vs GDAL's Arrow streams, through pyarrow and GeoPandas; `terrio.from_arrow()` of GDAL's, GeoPandas' and terrio's tables; GDAL's `WriteArrow` of terrio's tables |
| GeoPackage updates and spatial filters | `scripts/compare_gpkg_with_gdal.py` | `--append`, `--overwrite` and added layers vs `ogr2ogr` (features, extents, counts, sequences, R-trees, `integrity_check`, `validate_gpkg.py`, rasters kept); `--spat` vs `ogr2ogr -spat`; timings on a large file |
| CAD drawings | `scripts/compare_cad_with_gdal.py` (`cad`) | Independent AutoCAD R2000 DWG files and public GDAL DXF fixtures (binary DXF, OCS, faces, solids, hatches and blocks), real OSM roads through DWG/DXF, and Terrio-authored DXF read by GDAL. Counts, types, layer names and coordinates; linear tolerance 1e-8 (zero-Z promotion to XYZ reported), curve Hausdorff tolerance 0.001 times diagonal and relative length/area tolerance 0.002; Z bounds separately. Independent DWG coordinates were checked with GDAL/libopencad; generated DWG is cross-read through DXF. GDAL uses unmerged blocks and an explicit UTF-8 override for the mislabeled Russian hatch fixture. |
| Vector conversions | `scripts/compare_with_gdal.py --roundtrip` | files converted by terrio, read back by GDAL |
| Streamed vector conversions | `scripts/compare_streams_with_gdal.py` | conversions read and written as streams vs the same conversions of whole layers (identical files) and read back by GDAL; with `--features N`, a synthetic layer converted by terrio and `ogr2ogr`, with time and peak memory |
| rasterio API | `scripts/compare_rio_with_rasterio.py` | `terrio.rio` vs rasterio: metadata, reads, masks, writes read back by rasterio, features, warping, merging, masking, warped VRTs, rasterio's documentation examples, and `terrio-rio` against rasterio's `rio` command line (see [Coming from rasterio](rasterio.md)) |
| Coordinate transformations | `crates/terrio-crs/tests/epsg_accuracy.rs` | a point per EPSG CRS, forward, inverse and between CRSs, vs GDAL |
| Chains through intermediate datums | `crates/terrio-crs/tests/crs_chains.rs`, `scripts/compare_chains_with_proj.py` | every pair of EPSG geographic 2D CRSs on two datums no transformation links directly whose areas overlap, and a projected CRS of each datum, without and with grids, vs GDAL |
| Datum shift grids, geoid models, epochs | `crates/terrio-crs/tests/epsg_accuracy_grids.rs`, `crates/terrio-crs/tests/crs_grids.rs`, `scripts/compare_crs_grids_with_gdal.py` | the same EPSG points with the grids of `scripts/proj_grids.txt`; random points through every grid (forward and inverse), legacy NTv2 and GTX files, NADCON5 and geocentric grids, compound CRSs and coordinate epochs, vs GDAL with the same grids |

## Results

The [Conformance](conformance.md) page has the full, generated results; in short:

| Area | Agreement with GDAL |
|---|---|
| Raster drivers | Pixel-identical reads for every test file; terrio-written GTiff, COG, JP2, Zarr and Esri formats read back identically |
| Vector drivers | Identical layers, counts, fields, CRS, extents and geometry types for every file; files terrio writes read back by GDAL as `ogr2ogr`'s own conversions read back |
| Window reads | 160 of 160 local and 22 of 22 remote windows identical |
| `translate` | 100% identical pixels, nearest, bilinear, cubic and average alike (GDAL's resampling arithmetic) |
| `warp` | 21 of 28 cases identical; in the other 7, at most 7 pixels one float32 rounding step apart (source coordinates come from terrio's projection arithmetic, not PROJ's) |
| `mosaic` | 100% identical to `gdal_merge.py` and `gdalbuildvrt` |
| `convert` filters | 28 of 28 option cases identical to `ogr2ogr`, feature by feature |
| `rasterize` | 0 differing pixels in 18 cases |
| `polygonize` | Same polygon counts and areas per value; re-rasterizing reproduces the input |
| `dem` | 81 of 81 cases identical (every mode and option, on four DEMs); color relief VRTs read identically by GDAL to `gdaldem`'s, and `gdaldem`'s read by terrio as by GDAL |
| netCDF-4 and HDF5 | 116 of 116 subdataset checks identical (Sentinel-5P groups, JPSS ATMS and VIIRS products: lists, metadata, summaries, pixels); netCDF-4 written by terrio (`NC4`, `NC4C`, deflate) read by GDAL identically to the source and with the attributes GDAL writes |
| Histograms | 84 of 84 cases identical to `gdalinfo -hist` (21 rasters of every type) |
| `calc` | 35 of 37 cases identical; the other two within one float32 rounding step (NumPy's vectorized float32 `sin`, `exp`, `log10`, `arctan2`) |
| `proximity`, `sieve` | 13 of 13 cases identical |
| `unscale`, `nodata-to-alpha`, `reclassify`, `as-features` | 30 of 31 cases identical to GDAL 3.13.3's `gdal raster` commands (pixels or features, types, nodata, CRS, color interpretations, scales and affine transforms); the Sentinel-5P case has identical pixels and metadata but a known default-grid difference (reported as within tolerance). An unexpected affine transform fails comparison. Known differences: netCDF default geotransform, selected bands vs GDAL reporting all bands, default layer names, unsupported as-features update/layer options and nodata-to-alpha masks (see [CLI scope](cli.md#as-features)); a missing reference netCDF driver is an explicit case error, and repeated bands, Float32 nodata-adjacent samples, packed grayscale color interpretations, Float64 input converted to Float32 working pixels, and OGR field types are covered. |
| `neighbors`, `nearblack`/`clean-collar`, `enhance` | 103 of 103 cases identical (27, 58 and 18) to `gdal raster neighbors`, `nearblack`, `gdal raster clean-collar` and `gdalenhance`, pixels and masks; one in-place alpha case compared with GDAL's new-file output (GDAL writes stale alpha values) |
| STAC and GeoZarr | 16 of 16 STAC checks (subdatasets, mosaics and windows identical to GDAL STACIT's, 328 footprint fields as `ogrinfo`'s), 99 of 99 STACTA checks (summaries, subdatasets and metadata as `gdalinfo`'s; whole rasters, windows and every overview identical to GDAL STACTA's on its 7 test items and on `gdal raster tile` pyramids of a DEM and a Landsat scene, local, over HTTP and with a missing tile) and 60 of 60 GeoZarr checks |
| Remote Zarr stores | 45 of 45 checks on six public and served stores: sizes, geotransforms and nodata as `gdalinfo`'s; every window identical to `gdal_translate`'s (`/vsicurl/`, `/vsis3/`, `/vsigs/`, `/vsiaz/`) and xarray's, GeoZarr multiscales overviews identical to xarray's levels; windows 2 to 15 times faster than GDAL's |
| Point clouds | 485 of 485 checks: every value identical to laspy; COPC queries and grids identical to PDAL |
| Arrow | 171 of 180 comparisons identical (geometries WKB-equal, values, CRS) on 9 files; the other 9 are 40 OSM multipolygons whose rings start at another vertex than GDAL's |
| GeoPackage updates, `--spat` | 12 of 12 cases identical |
| Coordinate transformations | Every EPSG CRS's forward and inverse points within 1 cm, without and with grids; 99.7 % of CRS-to-CRS points within 1 m |
| Datum shift grids, geoids, epochs | 250 of 250 cases (50,000 points) within 0.02 mm of GDAL with the same grids |

## Running the tests

```sh
scripts/fetch_test_data.sh ../terrio-data
TERRIO_TEST_DATA=../terrio-data cargo test --workspace
python scripts/compare_with_gdal.py ../terrio-data
```

The Rust tests hold expected values taken from GDAL, so they run without GDAL installed; the
comparison scripts need GDAL's Python bindings.

The `overviews` conformance suite compares `gdalwarp -ovr` with Terrio's automatic,
finer, full-resolution and explicit overview choices on synthetic level markers
and real USGS terrain, including forced levels with GCP, RPC and geolocation-array
georeferencing (the geolocation cases use the installed Python bindings with explicit arrays).
It also measures response bytes while downsampling a USGS
COG over HTTP; AUTO must fetch fewer than half the raster bytes of NONE.

```sh
python scripts/conformance.py ../terrio-data --only overviews --strict
```

Fill-nodata comparisons are part of the `dem-calc` conformance suite. To run just
these cases on the real USGS terrain, with holes and explicit validity masks:

```sh
python scripts/compare_dem_calc_with_gdal.py ../terrio-data --only fill-nodata
```

The catalog suite also supplies a lazy HTTP reader and a deferred URL through Python's
source mapping. Catalog construction transfers zero additional tile bytes; an 8x8 real-DEM
query must match GDAL exactly and transfer less than half of the 1.6 MB tile.
Catalog/VRT regressions additionally verify physical indexes and supplied source aliases,
nested masks and netCDF container overwrites; unknown inventories and cyclic references
must fail before opening the destination, without decoding source pixels.

The `vrt` conformance suite cross-reads GDAL/Terrio virtual rasters on USGS terrain,
including whole and cropped requests, derived bands, nodata, masks, full and
separable kernels, and affine/reprojection/GCP/TPS/homography/RPC transforms:

```sh
python scripts/conformance.py ../terrio-data --only vrt --strict
```

### Focal statistics, collars and enhancement against GDAL

```sh
cargo test -p terrio --lib -- neighbors nearblack enhance kernel
cargo test -p terrio-cli --test focal --test mcp
python -m pytest python/tests/test_focal.py
node --test --test-name-pattern='neighbors, collar' js/tests/node.test.mjs
python scripts/compare_focal_with_gdal.py ../terrio-data
python scripts/conformance.py ../terrio-data --only focal --strict
```

The `focal` suite runs 27 `gdal raster neighbors` cases (every built-in kernel, `equal` with
every method on a DEM with deliberate nodata holes, matrices, several kernels and methods at
once, output types and nodata, and two cases on the whole 3612x3612 USGS DEM, which Terrio
reads and writes in four strips), 58 `nearblack`/`gdal raster clean-collar` cases (the
Sentinel-2 L2A true-color image's orbit-edge collar and the Landsat 7 scene, also with an alpha
band and with a mask; both algorithms, colors, thresholds, distances, alpha, masks and in-place
updates, including preserving an existing mask without mask flags) and 18 `gdalenhance` cases (equalization, LUT file text and enhancement with a LUT
file read back, on Byte, UInt16 and Float32 inputs, plus six real-DEM-derived Float64 nodata conversion cases). All 103 are identical with tolerance 0:
every pixel, mask, type, nodata and geotransform, and LUT files token by token. One case is
compared differently: GDAL 3.12.2's in-place flood fill of a raster with an alpha band reads only
the color bands into its line buffer and writes the buffer's stale alpha values back on every
modified line (2,715,128 of the Landsat 7 scene's 4,180,977 alpha values become 0). That case
compares the color bands with GDAL's update and Terrio's alpha band with GDAL's output of the
same cleaning to a new file.

### Pansharpening against GDAL

The fetch script downloads Landsat 8 B8/B4/B3/B2 real image pairs from the public
[rio-pansharpen test fixtures](https://github.com/mapbox/rio-pansharpen/tree/f23d1edbedddf157ca5014ec805918cdf713bb7f/tests/fixtures/tiny_20_tiffs/LC81070352015122LGN00).
The comparison retains unequal pan/spectral resolutions and checks all six interpolation
methods, weights, output selection, bit depth, nodata borders, internal masks, integer
clipping and union/intersection extents against `gdal_pansharpen`. It also cross-reads
GDAL/Terrio pansharpened VRTs as full images and cropped requests, and exercises all six
resamplers on images spanning multiple 512-pixel interpolation blocks. No tolerance is allowed
for integer cases; the Float32 comparison allows 0.002, one ULP at the scene's radiometric scale.

```sh
python scripts/compare_pansharpen_with_gdal.py ../terrio-data
python scripts/conformance.py ../terrio-data --only pansharpen
```

A local HTTP range-server measurement on a 1024×1024 UInt16 Landsat pan and three
512×512 spectral TIFFs fetched 853,965 bytes for a 64×64 output window, versus
3,676,141 bytes for the whole VRT. Both touched four source TIFFs; the window made
eight source GETs and the whole image twelve (plus the VRT description). Source
headers are read at open; the Rust counting-reader regression verifies no pixel
reads at open and bounded interpolation margins for small windows. On that scene,
the development CLI's peak RSS fell from 125,544 KiB to 101,824 KiB with the shared
block implementation and streamed TIFF output, measured with two Rayon threads.
These are fixture measurements, not memory or transfer guarantees for every codec.

## Build size and iteration speed

Development and test builds omit debug symbols by default. Enable full debugger
information with `CARGO_PROFILE_DEV_DEBUG=2 cargo test`; this increases disk use.
Incremental compilation remains available locally for repeated edits.

For several independent worktrees, use a shared compiler cache such as `sccache`
with `RUSTC_WRAPPER=sccache CARGO_INCREMENTAL=0`. Rust incremental compilation
cannot be cached by sccache. Keep each active worktree's target directory separate:
a single shared Cargo target directory serializes concurrent builds on its lock.
Put build targets on disk rather than a memory-backed `/tmp`, and limit aggregate
build parallelism when running multiple worktrees (for example, `CARGO_BUILD_JOBS=2`
per worktree). Use development WASM/Python builds for iteration; run optimized
package builds for final validation.

CI saves dependency caches for PR updates as well as main, disables incremental
compilation, and cancels obsolete runs when a PR receives a newer push. Release
profiles and the full validation matrix are unchanged.

Every push to main runs every job. Pull requests skip the slowest ones unless
asked for them:

| Job | Pull requests |
| --- | --- |
| real data vs GDAL (`ci.yml`) | Runs unless the PR changes only docs, Markdown, `js/` (other than its Rust code), `packaging/` or other workflows |
| npm package (`ci-extended.yml`) | Label `ci:npm` or `ci:all`; the `wasm` job tests the Node.js package on every PR |
| point clouds and conformance vs GDAL (`ci-extended.yml`) | Label `ci:conformance` or `ci:all` |
| macOS and Windows (`ci-platforms.yml`) | Label `ci:macos`, `ci:windows` or `ci:all` |

A label starts its jobs when added and reruns them on each push while it stays.
`ci-extended.yml` and `ci-platforms.yml` can also be run by hand from GitHub
Actions. Add
`ci:npm` to PRs that change the npm build or WebAssembly-sensitive code, and
`ci:conformance` to GDAL-parity work.

## Benchmark harness regressions

### CRS benchmark regressions (#266, #268)

```sh
cargo test -p terrio-crs --test crs_benchmark
```

The fixture `crates/terrio-crs/tests/data/crs_benchmark.json` retains the source
revision and oracle environment from rust-proj-capability: revision
`ddd9fac70ec370acd9651ebb8b46035207347ad5`, PROJ development version 9.10.0,
commit `5fa31c29f0a806568ed7bc76c1a557604db4a804`, pyproj 3.8.0. This is the
projection-suite operation oracle; the upstream CRS study uses a separate frozen
PROJ 9.9.0 oracle without grids.

Five 91-point graticules check Mollweide, Wagner IV/V, oblique ellipsoidal
orthographic and explicit-parallel Winkel Tripel. Forward tolerance is 1 mm;
inverse tolerance is 1e-7 degrees with longitude weighted by cos(latitude), or
1e-6 degrees at exact input poles (one last bit before asin moves Wagner IV
latitude by 8.5e-7 degrees between native/WASM math implementations).
Where PROJ refuses its own valid forward result at a pole or map boundary,
Terrio may refuse too or recover the original point within that tolerance.
The orthographic inverse uses an analytic ray–ellipsoid intersection and can
recover visible poles where the reference iteration fails. Additional round-trip
checks cover seven origins, three figures, local scales/rotations and false
offsets (1e-5 degrees at the ill-conditioned limb); outside-limb queries fail.
The five oracle comparisons are registered in the `crs` conformance suite.

PROJ's Mollweide iteration leaves a small nonzero x at the pole (112.5 m at
longitude -180 on a 6,371 km sphere). Terrio preserves this reference behavior;
this is not a claim of mathematical exactness. Winkel Tripel retains the CRS
default `lat_1=0`; an explicit `lat_1=acos(2/pi)` in degrees is used to compare
with the operation oracle. The separate default test guards this distinction.

With GDAL's Python bindings and NumPy available, run:

```sh
python -m unittest discover -s scripts/tests -v
```

These tests exercise benchmark correctness gates, independent CPU-budget selection,
interleaved measurements and Linux affinity/background-load handling. They run in
CI's real-data job without downloading benchmark datasets or asserting timing
thresholds. Performance measurements remain an explicit local task; see
[Benchmarks](benchmarks.md) for the methodology and latest GDAL version used.

The VRT conformance suite also compares ordinary GeoTIFF warp output against GDAL
for multiband inputs with distinct per-band nodata, both jointly invalid and
independently invalid pixels, and a separate destination fill. Nearest and bilinear
cases require exact pixel agreement (tolerance 0).

Pansharpened VRT comparisons include repeated/reordered Int64 and UInt64 pan
passthrough bands above 2^53, checked in their native integer dtype with tolerance
zero. A counted-reader regression requests three passthrough output bands
sourced from two pan bands in a 2×2 window with one two-band read (64 native sample bytes),
rather than three one-band reads (96 bytes); no spectral pixels are fetched.

The `warp-policy` suite compares versioned kernels and PixelIsPoint metadata on
real USGS terrain. It selects Terrio's `3.12` or `3.13` kernel to match the installed
GDAL, checks nearest and four interpolation kernels at 10/25/60 m plus rotated
output grids, and checks GeoTIFF/COG translation. Raster type, CRS, corner transform,
nodata and validity are checked; interpolated Float32 elevations allow 0.002 m.

```sh
python scripts/conformance.py ../terrio-data --only warp-policy --strict
cargo test -p terrio --test crs_policy
cargo test -p terrio-cli --test mcp coordinate_operation_warnings_and_controls_reach_mcp
```

Known numerical limitation: on rotated grids whose output centres land exactly on
source centres, GDAL's optimized stretched Lanczos recurrence can differ from
Terrio's direct sinc evaluation. A 40×30 USGS case showed a maximum 2.54 m
difference; the fractional-origin rotated cases above agree exactly. This is
separate from the 3.13 scale change and is not claimed as pixel-identical parity.

## Benchmark mismatch regressions

The `benchmark-metadata` conformance suite compares real SRTM Zarr output, a
four-tile USGS VRT and Natural Earth populated places in GeoPackage against GDAL.
It also checks a real SRTM crop with 1,599 assigned GCPs through COG conversion:

```sh
python scripts/conformance.py ../terrio-data --only benchmark-metadata --strict
```

It requires exact GCP coordinates and CRS, no invented affine, and exact colors,
field schemas, pixels, masks, attributes and normalized
geometries. The VRT reference explicitly disables GDAL 3.13's synthetic virtual
overviews to match Terrio's supported base-resolution output. The separate
[benchmark output investigation](benchmark-parity.md) records full numerical
errors and intentional differences; benchmark equivalence checks remain exact.

The `benchmark-compat` suite independently decodes native WebP and JPEG XL output
with GDAL, verifies lossless pixels and alpha, checks that WebP quality changes
lossy output, and compares both GDAL terrain arithmetic profiles (Horn and
Zevenbergen–Thorne). With GDAL 3.13 it also compares every COG overview on a real
DEM and an odd-sized optical crop:

```sh
python scripts/conformance.py ../terrio-data --only benchmark-compat --strict
```

The GDAL 3.13 COG profile supports nearest/average without alpha or explicit masks.
The GDAL 3.13 terrain profile covers slope and standard hillshade. Exactness on
these fixtures does not promise identical transcendental/SIMD rounding across
architectures. JPEG XL cases cover Byte/UInt16, one/three bands, strips and tiles;
macOS/Windows CI runs the matching native round-trip regression.

The benchmark compatibility suite also preserves entity text from GDAL-authored
TIFF metadata and asserts actual JPEG XL block dimensions. The portable GDAL 3.13
slope golden unit check permits one Float32 ULP for platform `atan` rounding; this
allowance does not apply to exact benchmark or conformance comparisons.

### Footprint attributes and singleparts (#275)

```sh
cargo test -p terrio -p terrio-cli --test processing
python scripts/conformance.py ../terrio-data --only footprint --strict
```

The GDAL comparison writes real DEM and Landsat footprints as Shapefiles in both
multipart and singlepart modes, with union/intersection band validity. Temporary
copies add a nodata stripe and a hole to exercise disconnected regions. It checks
feature counts, source-path and custom text fields, CRS and symmetric-difference
area (at most `max(1e-6, reference area * 1e-12)` per feature). Rust regressions
also cover holes, corner-touching regions, empty output schemas, field validation
and sequential singlepart IDs. Python's bundled CLI uses the same options; there
is currently no direct footprint operation in the Python or JavaScript bindings.

### Zonal and point-sampling output controls (#280)

```sh
cargo test -p terrio -p terrio-cli --test processing --test raster_analysis
python scripts/conformance.py ../terrio-data --only analysis-output --strict
```

The `analysis-output` suite checks real Tennessee polygons and points across DEM,
Landsat and NLCD rasters. Independent GDAL/NumPy baseline checks cover pixel
selection, nodata and statistics. Output CRS comparisons use GDAL transformations
with a 1e-5 metre tolerance in EPSG:3857; rounded values, original attributes,
integer counts and nulls must match exactly. Rust/CLI regressions cover positive
and negative halfway rounding, huge finite values, Z/M preservation, precision
limits and errors before output creation. Decimal places affect only computed
numeric attributes, not coordinates or format-specific trailing-zero display.
The options also apply to Python's bundled CLI and MCP; there are currently no
direct point-sampling or zonal-statistics functions in Python/JavaScript bindings.

## Standalone CRS feature coverage

Run the standalone suite separately from the workspace to avoid Cargo feature
unification with Terrio, which enables the optional projection fallback:

```sh
TERRIO_TEST_DATA=../terrio-data cargo test -p terrio-crs
TERRIO_TEST_DATA=../terrio-data cargo test -p terrio-crs --all-features
```

Both runs exercise EPSG tables and grids; the data-dependent tests require the
reference datasets described above. The 117-method inventory explicitly checks
42 native methods with default features and 88 with `wbprojection-fallback`.
Unsupported methods must be refused in every tested encoding. The projection
conformance runner enables the fallback to retain Terrio's full coverage.

## Geocentric endpoints (#269)

`crates/terrio-crs/tests/data/geocentric_benchmark.json` retains the
`pair-4979-4978` fixture from rust-proj-capability revision
`ddd9fac70ec370acd9651ebb8b46035207347ad5`: its CRS-study oracle is PROJ 9.9.0,
pyproj 3.8.0, without grids. This is distinct from the projections-suite oracle.
The source point is longitude 147, latitude -42, height 100 on EPSG:4979,
transformed to EPSG:4978 with all three expected Cartesian components retained.

`geocentric_reference.json` adds 26 cases / 63 points frozen with GDAL 3.12.2 /
PROJ 9.8.1: forward/inverse, poles, centre/interior conventions, GRS80 and spherical
ellipsoids, linear units, axis options, prime meridians, WKT1/WKT2 without IDs,
projected composition, datum transformations and an ITRF2014-to-GDA2020 coordinate
epoch. Two custom geocentric TOWGS84 cases use explicit GDAL pipelines because
PROJ's CRS factory drops that declared transformation. Both the operation and
the exception are recorded; tolerances are not widened to hide it. Zero-valued
Helmert regressions retain the Cartesian datum operation across different
ellipsoids; an explicit GDAL no-op checks Cartesian vector preservation.

```sh
cargo test -p terrio-crs --test geocentric
cargo test -p terrio --test geocentric -p terrio-cli --test mcp
python scripts/conformance.py ../terrio-data --only geocentric --strict
python -m pytest python/tests/test_geocentric.py
node --test --test-name-pattern='geocentric XYZ' js/tests/node.test.mjs
```

The conformance suite compares the CLI to live GDAL for every stored case. It
reports nonzero component errors as within tolerance (1 mm Cartesian/projected
and height; 1e-8 degrees geographic), never as identical. The Rust tests also
check malformed definitions, nonfinite inputs, round trips, coordinate metadata,
vector Z and measure retention, and an actual MCP XYZ call through `--dim 3`.

## Shapefile archives (#278)

The public archive regression runs without optional Terrio features. It checks
point, Z point, line and polygon-with-hole geometries, scalar attributes/nulls,
EPSG:4326, required ZIP members, UTF-8 at the 254-byte boundary, overlong-URL
refusal and explicit unsupported output. Native runs also compare every member
with the existing filesystem writer. Z-only output must not invent M=0 values;
all-asterisk text must remain text rather than becoming a numeric null marker.
Z-only point, multipoint, line and polygon records omit optional M sections, as
GDAL does, and keep zero M bounds in the file header. Where filesystem output
has real M samples, the record M range excludes missing samples.

```sh
cargo test -p terrio --no-default-features --test shapefile_archive
CARGO_TARGET_WASM32_WASIP1_RUNNER=wasmtime cargo test -p terrio \
  --target wasm32-wasip1 --no-default-features --test shapefile_archive
# After rebuilding the JS package: actual wasm32-unknown-unknown execution in Node
node --test --test-name-pattern='Shapefile ZIP' js/tests/node.test.mjs
python scripts/conformance.py ../terrio-data --only shapefile-archives --strict
```

The `shapefile-archives` suite cross-reads the ZIPs with GDAL and Terrio on 16
Natural Earth places, Las Vegas roads and Tennessee counties each, using explicit
scalar test attributes. It checks nulls, EPSG:4326 and exact coordinates after
normalizing ring orientation/start, part order and the Polygon/single-part
MultiPolygon wrapper (the Shapefile record stores those identically). The Z/text
case also checks independent GDAL reads of Z without an invented M, and all-asterisk
text. These comparisons use exactly
representable real attributes; general DBF real precision is documented in
[the archive format policy](formats.md#shapefile-zip-output-in-memory).

## PMTiles decoded-byte limits (#279)

With `default-features = false, features = ["tiles"]`, native and WebAssembly
builds support uncompressed, gzip and Zstandard PMTiles payloads through the same
pure-Rust decoders. Brotli is unsupported. The sparse caller supplies fetched
ranges, including in browsers; no native filesystem or network support is needed.

```rust
use terrio::sparse::SparseFile;
use terrio::tiles::{PmTilesReadLimits, TileArchive};

let file = SparseFile::new(archive_length);
let limits = PmTilesReadLimits {
    root_directory: Some(8 * 1024 * 1024),
    leaf_directory: Some(8 * 1024 * 1024),
    metadata: Some(8 * 1024 * 1024),
    tile: Some(16 * 1024 * 1024),
};
// Fetch file.missing() ranges and insert them, retrying as described in sparse's docs.
let archive = TileArchive::from_sparse_with_limits(file.clone(), limits);
```

These are example caller policies, not defaults. `None` means unlimited; existing
`from_sparse` calls retain their behavior. Each limit applies separately to a
payload, including construction-time root and metadata decoding and later leaf,
single-tile and batched-tile decoding. Exactly-at-limit output succeeds;
`terrio::Error::DecodedLimitExceeded { resource, limit }` distinguishes refusal
from malformed data, unsupported codecs and missing sparse ranges. The bounded
reader probes one byte beyond the limit and stops without collecting the rest.
Limits cover decoded bytes before parsing, not codec working memory, parsed
objects, cumulative caches, image/MVT expansion or compressed input transfers.
Normal sparse block sizing/read-ahead still applies. These configurable limits
are currently a Rust API (including Rust compiled to WASM). Python, CLI and the
JavaScript remote wrappers retain their existing unlimited defaults. Malformed
PMTiles offset/range arithmetic is refused with `InvalidData`, including root,
metadata, leaf and tile locations and directory-relative offsets/tile runs.

The public-API regression constructs independent PMTiles with leaf directories,
raw/gzip/Zstandard payloads and a 1 MiB repeated tile. It checks all four limits
at the exact boundary and one byte over, a zero tile budget, unlimited defaults,
missing-range retries and measured fetched bytes (construction fetches under
20 KiB and the trailing 256 KiB is never fetched). Run natively or in WASI:

```sh
cargo test -p terrio --no-default-features --features tiles --test pmtiles_limits
CARGO_TARGET_WASM32_WASIP1_RUNNER=wasmtime cargo test -p terrio \
  --target wasm32-wasip1 --no-default-features --features tiles --test pmtiles_limits
cargo check -p terrio --target wasm32-unknown-unknown --no-default-features --features tiles
```

## PROJJSON parsing (#270)

`projjson_benchmark.json` retains `form-27700-projjson` from rust-proj-capability
revision `ddd9fac70ec370acd9651ebb8b46035207347ad5`, including its CRS-study
PROJ 9.9.0 / pyproj 3.8.0 no-grid oracle. The source is OGC:CRS84 at longitude
-1.5, latitude 52.5; the target is the complete British National Grid PROJJSON.
This reference is independent of the projections-suite nightly oracle.

`projjson_reference.json` freezes 18 cases / 40 points with GDAL 3.12.2 /
PROJ 9.8.1. Geographic/projected/ensemble and bound definitions are tested with
and without IDs, including US survey feet, north-first and west/south projected
axes, Paris prime meridian/grads and zero/three/seven-parameter Helmert cases.
Equivalent GDAL-exported WKT2 definitions and representable PROJ strings are
also compared with those coordinates. The tests isolate grid search paths for
the no-grid reference. Both oracle scripts copy the configured `proj.db` into
a temporary directory and exclude installed/cached grids with networking off. Component
allowances are 1 mm projected and 1e-8 degrees geographic; nonzero discrepancies
are reported as within tolerance rather than identical.

```sh
cargo test -p terrio-crs --test projjson
CARGO_TARGET_WASM32_WASIP1_RUNNER=wasmtime cargo test -p terrio-crs \
  --target wasm32-wasip1 --test projjson
cargo test -p terrio-cli --test mcp projjson
python -m pytest python/tests/test_projjson.py
node --test --test-name-pattern='PROJJSON CRS' js/tests/node.test.mjs
python scripts/conformance.py ../terrio-data --only projjson --strict
```

The live comparison sends full JSON definitions through the CLI and GDAL,
normalizing both to traditional GIS coordinate order. It also imports and validates
Terrio-exported WKT with GDAL, then compares its transformed coordinates with
the original JSON definitions. Rust regressions also
check file loading, malformed/unsupported definitions, wrong units/dimensions,
and that a root authority ID cannot overwrite a caller's false easting.


## IAU_2015 authority support (#271)

`iau-source.json` records the registry version, PROJ source revision and
attribution. `scripts/gen_iau_table.py` exports all 2,201 definitions from the
pinned PROJ 9.8.1 database, retaining their full WKT2 and celestial-body IDs.
`iau_benchmark.json` separately retains `pair-mars-iau` and its PROJ 9.9.0 oracle
from rust-proj-capability revision `ddd9fac70ec370acd9651ebb8b46035207347ad5`.

`iau_reference.json` contains 2,074 geographic/projected CRS pairs with two points
each, frozen against GDAL 3.12.2 / PROJ 9.8.1. It records forward and inverse
outputs separately, plus four cases for kilometres, US survey feet, offsets, a
Mars polar-radius sphere and a conformal sphere at a nonzero latitude. Projected
component errors are converted to metres before applying the 1 mm allowance;
inverse angular components allow 1e-8 degrees. Native Rust, Python and Node/WASM
exercise the full corpus, while CLI/MCP also verify the pinned Mars case and
cross-body refusal. The live `iau` conformance suite uses the public CLI.

The reference's own inverse is approximate for some definitions. Five inverse
samples are refused; the error text is retained in the fixture. Native refusal
or recovery of the original point within the angular allowance is permitted
only for these recorded reference refusals. The largest successful GDAL inverse
round-trip residual is 3.834276481 degrees for Eros `200043361`; tests compare
its actual inverse output with PROJ and do not claim exact original-point
recovery. Other boundary/domain differences retain the policies documented for
the individual projections.

```sh
cargo test -p terrio-crs --test iau
CARGO_TARGET_WASM32_WASIP1_RUNNER=wasmtime cargo test -p terrio-crs \
  --target wasm32-wasip1 --test iau
cargo test -p terrio-cli --test mcp iau
python -m pytest python/tests/test_iau.py
node --test --test-name-pattern='IAU_2015' js/tests/node.test.mjs
python scripts/conformance.py ../terrio-data --only iau --strict
```

The regression also checks every registry definition can initialize and reopen
its WKT, unknown codes/years, versioned authority forms, west-positive axes,
planetocentric latitude conversion and rejection of Earth datum operations on
planetary coordinates. The numerical fixes use PROJ's sixth-order meridian
series, inverse authalic-latitude refinement and inverse normalization order;
the copied formulas and tables are attributed in `NOTICE`.


The sixth-order meridian series also changes Earth calculations. A WGS 84
latitude sweep from -90 to 90 degrees in 0.001-degree increments, comparing
`Mlfn` before this change (d8411cd2) with the new series, measured a maximum
meridian-distance change of 4.920 micrometres and inverse-latitude change of
4.409e-11 degrees. Earth projections remain covered by the `proj_strings`
regressions registered in the conformance runner's CRS suite. These small
coordinate changes can affect integer raster rounding near half-integer
boundaries; the typed-storage warp regression compares every pixel exactly with
the previous Float64 buffer representation. Both runs retain the same logical output type,
resampling, nodata rules and chunk plan; only buffer storage changes. This keeps
multiple-source overlays and both memory budgets covered without coupling the
storage invariant to historical projection checksums. No pixel tolerance is
used in this differential storage test. An independent value guard,
`earth_meridian_and_inverse_values_match_proj` in `proj_strings.rs`, checks
nine frozen GDAL 3.12.2 / PROJ 9.8.1 points for UTM 10N, equidistant conic and
sinusoidal, covering both hemispheres and high latitudes. It uses reference
projected points for inverse checks, with allowances of 1 micrometre forward
and 1e-11 degrees inverse, and is registered separately in the CRS conformance suite.

## Perspective projection coverage (#272)

`perspective_benchmark.json` retains `seed-tpers` and `edge-tpers-back` from
rust-proj-capability revision `ddd9fac70ec370acd9651ebb8b46035207347ad5`, with
its original PROJ 9.9.0 oracle and complete parameters. The separate
`perspective_reference.json` is generated by `scripts/gen_perspective_reference.py`
with GDAL 3.12.2 / PROJ 9.8.1, without network access. It covers 20 near-sided and
tilted perspective configurations and 2,500 geographic samples. All aspects,
spherical/WGS84 figures, non-default centres, heights, tilt/azimuth, units,
false offsets and horizon-side/inverse-domain refusals are exercised.

Rust checks the full corpus, WKT2 imports, WKT1 reopening, the frozen benchmark
and invalid setup parameters. Python and Node/WASM check all defined coordinate
samples and representative domain failures. CLI/MCP check the benchmark and
backside refusal; the live `perspective` conformance suite compares PROJ-string
and WKT2 CLI inputs with GDAL over the full corpus. Projected errors allow 1 mm
in physical metres; inverse latitude and longitude weighted by cos(latitude)
allow 1e-8 degrees. No approximate inverse input recovery is claimed at poles.

```sh
cargo test -p terrio-crs --test perspective --test proj_strings
cargo test -p terrio-cli --test mcp tilted_perspective
python -m pytest python/tests/test_perspective.py
node --test --test-name-pattern='perspective projections' js/tests/node.test.mjs
python scripts/conformance.py ../terrio-data --only perspective --strict
```

## Vector geometry commands (#289)

`simplify`, `segmentize`, `convex-hull`, `concave-hull`, `swap-xy`, `set-geom-type`,
`combine` and `sort` have unit tests of the GEOS/OGR behaviours they port (ring endpoint
simplification, jump checks, OGR's segmentize direction and Z/M copies, `forceTo` dimensions,
Graham scan ring order, concave hull areas, Hilbert codes from GDAL's C code), CLI tests of the
streamed and whole-layer paths and refusals, and Python, Node/WASM and MCP tests. The
`vector-geom` conformance suite compares them on Tennessee and US counties, Las Vegas roads and
Natural Earth countries and places with `gdal vector` (GDAL 3.12.2) for simplify, segmentize,
swap-xy, set-geom-type and `ogr2ogr -simplify/-segmentize`, coordinate by coordinate; with
OGR's `ConvexHull` (canonical WKB), `ConcaveHull` (edge sets) and GEOS's `GEOSConcaveHullOfPolygons_r` (ctypes) for polygon hulls,
by edge sets; and, as GDAL 3.12 has no combine or sort, with Python references of GDAL 3.13's
algorithms (GEOS's own STR tree for `--method strtree`). It needs the `gdal` command and the
osgeo bindings.

```sh
cargo test -p terrio --lib -- ops::simplify ops::hull ops::vector_geom
cargo test -p terrio-cli --test processing
cargo test -p terrio-cli --test mcp vector_geometry
python -m pytest python/tests/test_vector_geom.py
node --test --test-name-pattern='vector geometry' js/tests/node.test.mjs
python scripts/conformance.py ../terrio-data --only vector-geom
```

## Lazy GeoTIFF metadata and external overviews

The `lazy-geotiff` conformance suite runs the Rust regressions for sample bit
sizes, Float32 tag nodata, PAM overrides, band properties, RPC tags/sidecars,
file lists and external `.ovr` images. These are frozen GDAL reference checks;
they do not regenerate references with the GDAL installed at test time.
Metadata, selected pixel values and explicit masks use exact comparisons.
Float16 samples remain widened to Float32 in Terrio, with `sample_bits() == 16`;
GDAL 3.11 and later expose Float16 as a distinct data type.

GDAL 3.12.2 accepts external overview images of irregular aspect ratios,
equal dimensions and larger dimensions, when band count and sample type match.
The overview retains its own nodata and internal or `.ovr.msk` mask; a base
TIFF mask is not inherited by the external dataset. Its georeferencing is
scaled from the base raster. Internal overviews take precedence over `.ovr`.
External overview discovery applies to local files and adds one `.ovr` existence
probe per local open, and probes `.ovr.msk` only when `.ovr` exists. It does
not decode pixel chunks or probe remote paths.

```sh
cargo test -p terrio --test external_overviews --test geotiff_nodata
python scripts/conformance.py ../terrio-data --only lazy-geotiff --strict
```

`LazyRaster::rpc`, `file_list` and `sample_bits` are Rust APIs. Python and
JavaScript exposure of these additional lazy-reader facts remains follow-up
work; the existing bindings retain their behavior.

## Vector dataset commands (#289)

`concat`, `partition`, `set-field-type`, `make-point`, `create`, `export-schema`,
`rename-layer`, `edit` and `update` are checked against GDAL 3.13.3. The complete suite
requires GDAL 3.13 or newer and reports an explicit skip for older references. Renames use
OGRLayer::Rename, edits use `gdal vector edit`, and updates use GDAL's update API, with
GeoPackage validity and R-tree checks. The datasets include Las Vegas roads, countries,
counties, places and a GPS trace. GDAL's union layers merge later fields into the wrong
field when a source-layer field is added; that case compares with GDAL's output without
the source-layer field. The integer-subtype waiver applies only to values in field `i`;
feature counts, other values, field names, CRS and geometries remain strict gates.

```sh
cargo test -p terrio --lib -- vector_schema concat partition make_point
cargo test -p terrio-cli --test vector_dataset --test mcp
python scripts/conformance.py ../terrio-data --only vector-dataset --strict
python -m pytest python/tests/test_vector_dataset.py
node --test --test-name-pattern='vector dataset commands' js/tests/node.test.mjs
```

The vector-dataset comparison gives GDAL and Terrio the same installed PROJ grid
paths, disables grid downloads, and isolates per-user grid caches. This avoids
comparing results from different datum grids. Grid availability and network grid
reads are validated separately by the coordinate transformation suites. Unicode
rename fixtures are inventoried with SQLite and OGR's WKB parser: GDAL 3.13.3 can
raise `Cannot find table` during GPKG introspection for these names even in its own
outputs; table names, schema, attributes, CRS and geometries are still compared.

## Release rehearsal

The full release dry run is manual: select **Release dry run → Run workflow** in
GitHub Actions before tagging a release. It builds the complete cross-platform
wheel, CLI, installer and npm matrix without publishing. Pull requests retain the
normal CI checks, including Python, WASM, API documentation and package validation.



## Pixel-operation buffer checks

When changing typed output-buffer paths, measure peak RSS before and after with the same
multiband fixture, build profile and thread count. Compare every output pixel, type, nodata
value and georeferencing value as well as memory use. Record fixture details and observations
in the PR validation notes; they are not runtime or general memory guarantees. These operations
still materialize input and output, so eliminating intermediates does not make them streaming.
