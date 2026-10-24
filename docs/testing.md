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
| NetCDF and HDF5 subdatasets | `scripts/compare_with_gdal.py --subdatasets` | the subdataset lists of netCDF (groups included) and HDF5 files, HDF5 metadata, then every subdataset's summary and pixels |
| Histograms | `scripts/compare_hist_with_gdal.py` | `terrio info --hist` vs `gdalinfo -hist` and `-json -hist`, byte for byte, with `-stats` and with histograms stored in `.aux.xml` sidecars, on every data type |
| Raster round trips | `scripts/raster_roundtrip.py` | GDAL writes → terrio reads; terrio writes → GDAL reads, pixel for pixel; COGs pass GDAL's validator |
| Window and overview reads | `scripts/compare_windows_with_gdal.py` | every overview level of every GeoTIFF, local and remote over `/vsicurl/` |
| Indexed raster catalogs | `scripts/compare_catalogs_with_gdal.py` | Real DEM mosaics in GeoPackage/FlatGeobuf indexes: z-order, nodata, masks, partial alpha, mixed CRS and Terrio-written indexes read by GDAL GTI; an 8x8 HTTP query from 1,002 features checks opens/cache bounds and actual bytes for both implementations |
| Raster operations | `scripts/compare_ops_with_gdal.py` | translate, warp (all resampling methods), mosaic and VRT against `gdal_translate`, `gdalwarp`, `gdalbuildvrt`, `gdal_merge.py` |
| Vector operations | `scripts/compare_vector_ops_with_gdal.py` | `ogr2ogr` filters, `gdal_rasterize` and `gdal_polygonize.py` |
| Vector topology | `scripts/compare_topology_with_gdal.py` | Tennessee county polygons and Las Vegas roads projected to metre-based CRSs: buffer, all four polygon overlays, dissolve, validity, repair of a deliberately corrupted county boundary and snap cleanup against GDAL/GEOS; attributes, source IDs and CRS, with relative symmetric-difference area tolerance 1e-7 (5e-4 for buffer curves). Registered in conformance as `topology`. |
| Terrain and raster algebra | `scripts/compare_dem_calc_with_gdal.py` | every `gdaldem` mode and option, `gdal_calc.py` expressions, types, nodata and extents, `gdal_proximity.py`, `gdal_sieve.py` |
| STAC and GeoZarr | `scripts/compare_stac_geozarr.py` (`stac`, `stacta`, `geozarr`) | STACIT subdatasets, mosaic sizes, geotransforms, CRSs and windows on Earth Search; STACTA items (GDAL's test data and pyramids `gdal raster tile` writes from real rasters, local, over HTTP and with a missing tile): summaries, subdatasets, metadata, whole rasters, windows and overviews vs GDAL's STACTA driver; STAC footprint fields vs `ogrinfo`; GeoZarr stores read by GDAL and rioxarray, overviews vs `gdaladdo` |
| Remote Zarr stores | `scripts/compare_remote_zarr.py` | public cloud stores read in place (EOPF Sentinel-2 Zarr v2, EOPF GeoZarr v3 with shards and value codecs, NOAA HRRR on S3, CMIP6 on Google Cloud Storage, Daymet on Azure, a sharded store on a local port): sizes, geotransforms, nodata vs `gdalinfo` (`/vsicurl/`, `/vsis3/`, `/vsigs/`, `/vsiaz/`), windows vs `gdal_translate` and xarray, overviews vs xarray, bytes and requests fetched |
| GRIB | `scripts/compare_grib_with_gdal.py` | GDAL's GRIB test files, ecCodes' and real forecasts (GFS, HRRR, ECMWF open data), band by band: sizes, geotransforms, CRSs, descriptions, `GRIB_*` metadata, nodata values and pixels, with units normalized and as stored |
| Tile archives | `scripts/compare_tiles_with_gdal.py` | MBTiles written by `gdal_translate`/`gdaladdo` and by terrio, pixel for pixel per zoom level; PMTiles pixels; vector tiles read by both tools and written by `ogr2ogr` and terrio: feature counts, areas, lengths per feature key |
| Point clouds | `scripts/compare_pointclouds.py` | every value vs laspy; headers, COPC queries, `writers.gdal` grids and reprojection vs PDAL |
| Arrow | `scripts/compare_arrow_with_gdal.py` | `Layer.to_arrow()` (WKB and native GeoArrow) vs GDAL's Arrow streams, through pyarrow and GeoPandas; `terrio.from_arrow()` of GDAL's, GeoPandas' and terrio's tables; GDAL's `WriteArrow` of terrio's tables |
| GeoPackage updates and spatial filters | `scripts/compare_gpkg_with_gdal.py` | `--append`, `--overwrite` and added layers vs `ogr2ogr` (features, extents, counts, sequences, R-trees, `integrity_check`, `validate_gpkg.py`, rasters kept); `--spat` vs `ogr2ogr -spat`; timings on a large file |
| Vector conversions | `scripts/compare_with_gdal.py --roundtrip` | files converted by terrio, read back by GDAL |
| Streamed vector conversions | `scripts/compare_streams_with_gdal.py` | conversions read and written as streams vs the same conversions of whole layers (identical files) and read back by GDAL; with `--features N`, a synthetic layer converted by terrio and `ogr2ogr`, with time and peak memory |
| rasterio API | `scripts/compare_rio_with_rasterio.py` | `terrio.rio` vs rasterio: metadata, reads, masks, writes read back by rasterio, features, warping, merging, masking, warped VRTs, rasterio's documentation examples, and `terrio-rio` against rasterio's `rio` command line (see [Coming from rasterio](rasterio.md)) |
| Coordinate transformations | `crates/terrio/tests/epsg_accuracy.rs` | a point per EPSG CRS, forward, inverse and between CRSs, vs GDAL |
| Chains through intermediate datums | `crates/terrio/tests/crs_chains.rs`, `scripts/compare_chains_with_proj.py` | every pair of EPSG geographic 2D CRSs on two datums no transformation links directly whose areas overlap, and a projected CRS of each datum, without and with grids, vs GDAL |
| Datum shift grids, geoid models, epochs | `crates/terrio/tests/epsg_accuracy_grids.rs`, `crates/terrio/tests/crs_grids.rs`, `scripts/compare_crs_grids_with_gdal.py` | the same EPSG points with the grids of `scripts/proj_grids.txt`; random points through every grid (forward and inverse), legacy NTv2 and GTX files, NADCON5 and geocentric grids, compound CRSs and coordinate epochs, vs GDAL with the same grids |

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

The `vrt` conformance suite cross-reads GDAL/Terrio virtual rasters on USGS terrain,
including whole and cropped requests, derived bands, nodata, masks, full and
separable kernels, and affine/reprojection/GCP/TPS/homography/RPC transforms:

```sh
python scripts/conformance.py ../terrio-data --only vrt --strict
```

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

## Benchmark harness regressions

With GDAL's Python bindings and NumPy available, run:

```sh
python -m unittest discover -s scripts/tests -v
```

These tests exercise benchmark correctness gates, independent CPU-budget selection,
interleaved measurements and Linux affinity/background-load handling. They run in
CI's conformance job without downloading benchmark datasets or asserting timing
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
