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
| Multidimensional mosaics | `scripts/compare_mdmosaic_with_gdal.py` | NOAA wind spatial tiles, gaps, overlap/nodata priority, irregular time ranges, nested selection, netCDF/Zarr snapshots and all ten numeric types. Exact values/coordinates/types/nodata/units; source-group and Float32 coordinate-type preservation reported within tolerance. Descending GDAL reference inputs are normalized for its invalid-VRT size/offset bug. Registered as `mdmosaic`. |
| Native multidimensional arrays | `scripts/compare_multidim_with_gdal.py` | GDAL MD and xarray: named axes, indexing variables, native slices/transposes, netCDF/Zarr coordinate/chunk preservation and damaged-unselected-chunk checks |
| NetCDF and HDF5 subdatasets | `scripts/compare_with_gdal.py --subdatasets` | the subdataset lists of netCDF (groups included) and HDF5 files, HDF5 metadata, then every subdataset's summary and pixels |
| Histograms | `scripts/compare_hist_with_gdal.py` | `terrio info --hist` vs `gdalinfo -hist` and `-json -hist`, byte for byte, with `-stats` and with histograms stored in `.aux.xml` sidecars, on every data type |
| Raster round trips | `scripts/raster_roundtrip.py` | GDAL writes → terrio reads; terrio writes → GDAL reads, pixel for pixel; COGs pass GDAL's validator |
| Window and overview reads | `scripts/compare_windows_with_gdal.py` | every overview level of every GeoTIFF, local and remote over `/vsicurl/` |
| Indexed raster catalogs | `scripts/compare_catalogs_with_gdal.py` | Real DEM mosaics in GeoPackage/FlatGeobuf indexes: z-order, nodata, masks, partial alpha, mixed CRS and Terrio-written indexes read by GDAL GTI; an 8x8 HTTP query from 1,002 features checks opens/cache bounds and actual bytes for both implementations |
| Raster operations | `scripts/compare_ops_with_gdal.py` | translate, warp (all resampling methods), mosaic and VRT against `gdal_translate`, `gdalwarp`, `gdalbuildvrt`, `gdal_merge.py` |
| Raster comparison, pixel queries, XYZ | `scripts/compare_raster_query_with_gdal.py` | `terrio compare` vs `gdal raster compare` (report lines and exit status), `terrio pixel-info` vs `gdal raster pixel-info` (GeoJSON, CSV) and `gdallocationinfo` (text, `-valonly`), `terrio xyz` vs `gdal2xyz.py`, on real rasters and derived copies. Registered in conformance as `raster-query`. |
| Palette conversion | `scripts/compare_palette_with_gdal.py` | Landsat quantization at 5/8 bits, 2/17/256 colors and dithering, raster/text map lookup, nodata/masks/alpha, OPERA RGB/RGBA expansion and palette attachment: exact pixels, palette entries, masks, types, nodata, CRS and geotransform. Registered as `palette`. |
| Raster blending | `scripts/compare_blend_with_gdal.py` | Ten composition operators, 1–4 band Landsat images, OPERA GeoTIFF/PNG palettes and alpha/opacity boundaries; registered as `blend`, with documented GDAL reference-path normalizations. |
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

A local HTTP check of XYZ `--skip 64` on the 51,486,544-byte USGS GeoTIFF
transferred 38,248,272 bytes in 9 requests before and after block-aware sampling.
All 3,249 output lines were byte-identical. One run of each optimized CLI took
3.89 s before and 1.07 s after; these are local observations rather than a timing
threshold in the test suite.

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
| `compare`, `pixel-info`, `xyz` | GDAL 3.13.3: compare 20 of 21 and xyz 11 text cases identical, plus one shared refusal of non-finite integer nodata; one documented mask-count report difference; pixel-info 64 of 67 identical, 3 within 4e-14 relative (projection arithmetic, one scale/offset ULP) |
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

### Vector gridding and linear referencing against GDAL

`compare_vector_grid_with_gdal.py` grids Natural Earth's populated places (769 United States
places, above GDAL's 100-point quadtree threshold, and 89 Peruvian ones, below it), GDAL's
`gdal_grid` test points (a regular grid, with many equidistant points) and a 4,400-point subset
of the Autzen lidar (3D) with every algorithm, ellipse, rotation, point limit and per-quadrant
limit, and every option, and compares every pixel, the type, nodata and geotransform with
`gdal_grid` (run with `GDAL_USE_AVX=NO GDAL_USE_SSE=NO`, GDAL's double-precision code; one case
compares its default SIMD code within a tolerance). In a local environment with GDAL 3.13.3: 109
cases, 102 identical, 5 within tolerance (`linear` on scattered points, 4.7e-9 on populations of
1e7 and 1.7e-13 on elevations, identical in Float32; the SIMD `invdist`, single precision, up to
265 on populations) and 2 differing (`linear` on the regular grid off its nodes, 229 of 400 and
1261 of 2451 pixels, where qhull breaks co-circular ties differently). GDAL 3.12 scans every
point for a rotated ellipse where 3.13 searches its quadtree: against 3.12.2 a rotated
`average_distance` also differs, by 1.1e-15.

`compare_lineref_with_gdal.py` runs `ogrlineref` and `terrio lineref` on GDAL's test route and
milestones (geographic, reprojected to UTM, and as two routes told apart by a field, one drawn
backwards): the parts feature by feature, the printed positions and coordinates, and sublines
vertex by vertex. 72 cases: 69 identical, 3 within tolerance (the UTM lengths of a geographic
route's end parts, including the first-NULL-route case, 1.1e-11 to 3.8e-11, from
terrio's projection rather than PROJ). NULL middle parts are skipped identically to GDAL.

Linear referencing skips infinite milestone positions with a warning before part generation;
the GDAL 3.13.3 probe did not terminate within five seconds. NULL begin fields in position
lookup follow OGR numeric NULL-as-zero semantics. Coordinate/subline selection retains OGR
filter semantics, so NULL starts are not selected there. A GDAL NaN-scale GeoJSON probe returns
the final vertex for coordinates and the complete last part for multi-part sublines.
The core in-memory algorithm retains those results. Terrio's GeoJSON reader rejects bare
NaN tokens; SQLite/GeoPackage persists NaN as NULL.

A 10,001-point fixture with 10,000 coincident XY positions completes a 2x2 nearest grid with
a search ellipse in 0.094 seconds (78,364 KiB peak child RSS on this machine). This limited
probe does not establish bounds for larger duplicate-heavy inputs; quadtree order remains GDAL-compatible.

```sh
python scripts/compare_vector_grid_with_gdal.py ../terrio-data
python scripts/compare_lineref_with_gdal.py ../terrio-data
python scripts/conformance.py ../terrio-data --only vector-grid,lineref
cargo test -p terrio --lib -- vector_grid lineref
cargo test -p terrio-cli --test mcp vector_grid
python -m pytest python/tests/test_vector_grid.py
node --test --test-name-pattern='vectorGrid' js/tests/node.test.mjs
```

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
| real data vs GDAL (`ci.yml`) | Skipped only when every changed path (renamed-from paths included) is under `docs/` or `site/`, a root-level Markdown file, `js/` (except its Rust code and `Cargo.toml`), `packaging/`, or `.github/` (except `ci.yml`), or is one of `zensical.toml`, `requirements_docs.txt`, `CITATION.cff`, `about.hbs` or `about.toml`; a PR listing 3000 or more paths always runs |
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
43 native methods with default features and 89 with `wbprojection-fallback`.
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

## Shapefile self-touching rings (#308)

Esri allows a polygon ring that touches itself at a vertex to enclose a hole. Since
3.13.0, GDAL splits such a ring into an exterior and holes in `organizePolygons`,
for records of several parts; GDAL 3.12 keeps it as stored. Terrio reads these
rings as GDAL 3.13 does.

```sh
cargo test -p terrio --lib shapefile
python scripts/conformance.py ../terrio-data --only shapefile-rings --strict
```

The `shapefile-rings` suite writes 18 synthetic records with OGR (Esri holes, a
single-part record, rings stored from inside a hole, figure-eights, spikes,
same-direction loops, duplicate points, hole parts that are figure-eights or enclose an island, and stored holes touching
the exterior), checks in the `.shp` that the rings are stored as given, and compares
OGR's reading with Terrio's GeoPackage output, ISO WKB byte for byte. With GDAL 3.12
the reference is GDAL 3.13.1's reading, frozen in the script. One case is within
tolerance rather than identical: a hole part enclosing an island gives the same valid
polygons, but GEOS `MakeValid` returns both rings in the other direction and from
another start vertex.

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

PROJJSON export (`SpatialRef::to_projjson`) has unit tests of each kind of CRS and refusal
(`cargo test -p terrio-crs --test projjson_export`); `--only projjson-export` compares it with
live PROJ's PROJJSON of 52 CRSs by meaning and has GDAL read those and every IAU_2015 CRS, whose
coordinates must match the originals' (`scripts/compare_projjson_export_with_gdal.py`); a case
with no point the original CRS transforms is reported as `not compared`, and a point only the
original transforms as `differs`.

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

## Interrupted Goode homolosine coverage (#272)

`igh_benchmark.json` retains `seed-igh` from rust-proj-capability revision
`ddd9fac70ec370acd9651ebb8b46035207347ad5`, with its PROJ 9.9.0 oracle.
`igh_reference.json` is generated by `scripts/gen_igh_reference.py` with pyproj
3.8.0 / PROJ 9.8.1, without network access: 8 configurations on a sphere and on
WGS 84 (central meridians 0, 147, -160 and 20; metres, kilometres and US survey
feet; false offsets) and 5,120 geographic samples, every 10 degrees of longitude
from the central meridian at 17 latitudes including the poles and each side of
the sinusoidal/Mollweide boundary, and each side of every interruption and of
the antimeridian. Each configuration has GDAL's WKT1, ESRI WKT1 and WKT2 as PROJ
writes them, and inverse points in each interruption, past the poles, on PROJ's
southern bound and past 180 degrees.

Rust checks the corpus in every encoding, the inverse refusals of the PROJ
strings, terrio's own WKT1, the fixture, and the refusal of the other Goode
homolosines; Python and Node/WASM check every sample and the inverse refusals;
CLI/MCP check the fixture and an interruption refusal. The live `igh`
conformance suite compares the PROJ string and the three WKT encodings with
GDAL, and the inverse refusals of the PROJ string. Inverse refusals are compared
on the PROJ strings only: a WKT's rounded unit (US survey feet) moves points on
an interruption's edge to either side of it.

```sh
cargo test -p terrio-crs --test igh --test proj_strings
cargo test -p terrio-cli --test mcp interrupted_goode
python -m pytest python/tests/test_igh.py
node --test --test-name-pattern='interrupted Goode' js/tests/node.test.mjs
python scripts/conformance.py ../terrio-data --only igh --strict
```

## General oblique transformation coverage (#272)

`ob_tran_benchmark.json` retains `seed-ob_tran-moll` from rust-proj-capability revision
`ddd9fac70ec370acd9651ebb8b46035207347ad5`, with its PROJ 9.9.0 oracle.
`ob_tran_reference.json` is generated by `scripts/gen_ob_tran_reference.py` with pyproj
3.8.0 / PROJ 9.8.1, without network access: 15 configurations on a sphere and on WGS 84 of
eight rotated projections (`moll`, `merc` with `lat_ts`, `eqc`, `sinu`, `ortho` with `lat_0`,
`robin`, `lcc` with two parallels, and `utm` on WGS 84 only, which PROJ refuses on a sphere),
with the pole given as `o_lat_p`/`o_lon_p`, as a centre and azimuth and as two equator points,
a transverse rotation, metres, kilometres and US survey feet, and false offsets. Each has 312
geographic samples, every 15 degrees from the central meridian at 13 latitudes including the
poles, with PROJ's forward refusals (`ortho`'s far side, `utm` far from its meridian), and
inverse points inside and beyond the rotated projection's domain. Each configuration has GDAL's
WKT1 and WKT2 as PROJ writes them. The generator checks that pyproj's CRS route is PROJ's
`ob_tran` operation itself, and that PROJ refuses the definitions the corpus lists as refused.

Rust checks the corpus in every encoding with its forward and inverse refusals, the fixture with
terrio's own WKT1, and the refused definitions; the rotated-pole tests check the other pole forms
for `longlat` against PROJ. Python and Node/WASM check every sample and PROJ's refusals; CLI/MCP
check the fixture and an inverse refusal. The live `ob_tran` conformance suite compares the PROJ
string and both WKT encodings with GDAL. Two inverse points of the spherical sinusoidal case are
refused by terrio where PROJ returns a point (see the CRS page); the Rust test and the
conformance suite check that terrio refuses them, and Python and Node/WASM leave them out.

```sh
cargo test -p terrio-crs --test ob_tran
cargo test -p terrio --test rotated_pole
cargo test -p terrio-cli --test mcp oblique_mollweide
python -m pytest python/tests/test_ob_tran.py
node --test --test-name-pattern='general oblique' js/tests/node.test.mjs
python scripts/conformance.py ../terrio-data --only ob_tran --strict
```

## Quadrilateralized spherical cube coverage (#272)

`qsc_benchmark.json` retains `method-qsc` from rust-proj-capability revision
`ddd9fac70ec370acd9651ebb8b46035207347ad5`, with its PROJ 9.9.0 oracle.
`qsc_reference.json` is generated by `scripts/gen_qsc_reference.py` with pyproj 3.8.0 / PROJ
9.8.1, without network access: 16 configurations on a sphere and on WGS 84, the six cube face
centres, and two centres off them, in the front and back faces (PROJ picks the face and keeps
the central meridian), with metres, kilometres and US survey feet and false offsets. Each has 268
geographic samples, every 30 degrees from the central meridian at 17 latitudes including the
poles and each side of the cube corners' latitude, and each side of the face and area edges
(every 45 degrees from the central meridian), and inverse points on the face, on its areas'
edges and beyond it. PROJ refuses none of them. Each configuration has GDAL's WKT1, ESRI WKT1
and WKT2 as PROJ writes them. The generator checks that pyproj's CRS route is PROJ's `qsc`
operation itself.

Rust checks the corpus in every encoding and the fixture with terrio's own WKT1; Python and
Node/WASM check every sample and inverse point; CLI/MCP check the fixture forward and back. The
live `qsc` conformance suite compares the PROJ string and the three WKT encodings with GDAL. At
the antipode of the face centre (listed in the corpus as `antipodal_samples`) the inverse goes
through acos near 180 degrees, so the last bit of the northing (a WKT's rounded US survey foot,
or another platform's libm) moves it by about 1e-6 degrees: every check takes the inverse there
within 1e-5 degrees and checks it by its projection (1 mm).

```sh
cargo test -p terrio-crs --test qsc
cargo test -p terrio-cli --test mcp quadrilateralized
python -m pytest python/tests/test_qsc.py
node --test --test-name-pattern='quadrilateralized' js/tests/node.test.mjs
python scripts/conformance.py ../terrio-data --only qsc --strict
```

## HEALPix and rHEALPix coverage (#272)

`healpix_benchmark.json` retains `method-healpix` and `method-rhealpix` from rust-proj-capability
revision `ddd9fac70ec370acd9651ebb8b46035207347ad5`, with their PROJ 9.9.0 oracles.
`healpix_reference.json` is generated by `scripts/gen_healpix_reference.py` with pyproj 3.8.0 /
PROJ 9.8.1, without network access: 10 configurations (`healpix` with `rot_xy` 0, 30, -45 and
90; `rhealpix` with each polar square; central meridians; metres, kilometres and US survey feet;
false offsets) on a sphere and on WGS 84, and two on an ellipsoid of flattening 1/40, where PROJ
computes the authalic latitude in closed form and refines its inverse. Each has 300 geographic
samples, every 30 degrees from the central meridian at 17 latitudes including the poles and each
side of the polar caps' edge latitude, and each side of the caps' edges and middles; and 14
inverse points inside, on the edge of and outside the image. Each configuration has GDAL's WKT1,
ESRI WKT1 and WKT2 as PROJ writes them. The generator checks that pyproj's CRS route is PROJ's
operation itself.

Rust checks the corpus in every encoding and terrio's own WKT1, with PROJ's inverse refusals,
the fixtures, and the refusal of polar squares outside 0 to 3; Python and Node/WASM check every
sample and the inverse refusals; CLI/MCP check the fixtures. The live `healpix` conformance
suite compares the PROJ string and the three WKT encodings with GDAL; with a PROJ whose authalic
latitude differs from PROJ 9.8.1's (PROJ 9.4, for one), it leaves the ellipsoidal cases not
compared. On the samples on the edge of the image (listed in the corpus) an inverse may be
refused, by the platform's libm or by the US survey foot (see the CRS page), and an inverse that
is not refused must return the projected point. On Linux, 101 of the 1,386 edge samples are decided otherwise than PROJ, all in
the US survey foot PROJ strings.

```sh
cargo test -p terrio-crs --test healpix
cargo test -p terrio-cli --test mcp healpix
python -m pytest python/tests/test_healpix.py
node --test --test-name-pattern='HEALPix' js/tests/node.test.mjs
python scripts/conformance.py ../terrio-data --only healpix --strict
```

## ISEA coverage (#272)

`isea_reference.json` is generated by `scripts/gen_isea_reference.py` with pyproj 3.8.0 /
PROJ 9.8.1, without network access: 7 configurations (the default; `+orient=pole`; `+lon_0=10`;
`+orient=pole` with `+lon_0` and `+lat_0`; `+azi=30`; `+resolution=5 +aperture=4`; and
`+lon_0=11.25`, which is the default orientation's own) with metres, kilometres and US survey
feet and false offsets, on the authalic sphere of WGS 84 and on WGS 84. Each has 556 geographic
samples, every 15 degrees of longitude at 19 latitudes including the poles and the latitudes of
the face centres and vertices, and 100 points spread over the sphere; and PROJ's inverse of each
projected point. Inverse points over and around the unfolded icosahedron (a grid of 377 per
configuration where PROJ's inverse is defined, 105 elsewhere; 3,102 in all) are checked too. Each configuration has GDAL's WKT1, ESRI
WKT1 and WKT2 as PROJ writes them. The generator checks that pyproj's CRS route is PROJ's
operation itself, and that PROJ's inverse is defined only for the expected configurations: on
the sphere at every sample, on WGS 84 at most samples (its forward takes the semi-major axis and
its inverse the authalic radius, so some projected points fall outside the triangles).
The `method-isea` fixture of rust-proj-capability is from PROJ 9.9.0, whose `isea` is a new
method (see the CRS page), and is not used.

Rust checks the corpus in every encoding and terrio's own WKT1, with PROJ's inverse refusals,
and the refusal of invalid options and grid-address modes; Python and Node/WASM check every
sample and the inverse refusals; CLI/MCP check a point forward and back. The live `isea`
conformance suite compares the PROJ string and the three WKT encodings with GDAL; with a PROJ
whose `isea` differs from PROJ 9.8.1's (PROJ 9.4 or 9.9, for two), it leaves every case not
compared.

```sh
cargo test -p terrio-crs --test isea
cargo test -p terrio-cli --test mcp isea
python -m pytest python/tests/test_isea.py
node --test --test-name-pattern='ISEA' js/tests/node.test.mjs
python scripts/conformance.py ../terrio-data --only isea --strict
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

### Chunk tables of lazy GeoTIFF reads (#348)

```sh
cargo test -p terrio --test tiff_chunk_tables
```

A lazy GeoTIFF keeps the tile or strip offsets and byte counts of each image
(full resolution or overview, `.ovr` levels included) after its first window
read, with the codec terrio decodes its chunks with, or the lack of one. The
tests drop every block of a sparse file but those of chunk data after a level's
first read: the next reads of the level (Deflate and LZW, pixel- and
band-interleaved, tiles and strips, classic and BigTIFF, sparse and edge chunks,
reordered and repeated bands, overviews, masks) must read nothing else and match
the source. Counting the bytes each thread allocates, a one-strip read of an
image of 65,536 strips must allocate less than a quarter of one copy of its
tables, as must a mask read (at full resolution, and sampled for an overview);
the first read of a level keeps one copy, shared with its codec; and returning
to a level after reading another must allocate at least its tables less than
its first read, for internal and `.ovr` overviews. Tables over the decoding
limits still fail every read. The table-reuse and allocation checks fail on the
code before the change. Additional tests switch between IFDs with different
codecs and strip geometry, and read windows, masks and overviews concurrently
through shared lazy handles, checking pixels, nodata, type and georeferencing.

Moving to another level still reads that image's directory and tables (the
`tiff` crate's `seek_to_image`), and the threads decoding the chunks of a
multi-chunk window of a local file or bytes in memory each open a decoder,
which reads the first image's tables (and the level's). Opening is unchanged.

## File management (#289)

`scripts/compare_vsi_with_gdal.py` (the `vsi` conformance suite) compares `terrio vsi` with
GDAL's `gdal vsi` (GDAL 3.13.3; the results are the same with 3.12.2 and 3.13.0): recursive long
listings (`-lR -f json`) of the test data directory (742 entries), its three Natural Earth and
NYC zip archives, a Zarr store and tar and tar.gz archives of it, with names, types and file
sizes identical and dates identical to the second; trees copied, extracted from a zip and
synchronized (with and without a trailing slash) by both tools, byte-identical; and, on a local
S3-compatible server, the same tree uploaded (directory markers included), listed, copied
server-side, downloaded, synchronized twice (no upload on the second run, as GDAL), deleted
recursively without touching the sibling `data2/` and `dirkeep` keys, and the whole bucket
refused. GDAL 3.13.3 review regressions also compare a query-bearing remote ZIP listing
and extracted real-file bytes, and recursive S3 deletion when an exact object and its
`prefix/` children coexist: only children are deleted, preserving the exact object and
neighboring prefixes. File moves remove the exact source object and preserve coexisting prefix children. Recursive deletion of a lone object fails without deleting it, matching GDAL 3.13.3. A missing local destination with a trailing slash fails without creating the directory. The local suite has 25 cases. Uploading the 7-file Natural Earth tree took terrio 10 requests and GDAL 20; the
recursive listing 3 requests each. With `--remote`, listings of public S3 (hrrrzarr), Google
Cloud Storage (CMIP6), Azure (Daymet, with a Planetary Computer SAS token), an HTML index page
(download.osgeo.org) and a remote zip archive compare identically; GDAL omits the directories
implied by object keys in recursive object store listings, which terrio lists as it does for
local directories. Listing Natural Earth's 4.9 MB remote `ne_10m_admin_0_countries.zip` takes
terrio 4 requests and about 32 KB.

A review regression fixture with an 8 MiB member served over local HTTP compares the previous
and updated VSI paths (body bytes counted by the client): copying a stored ZIP member uses
131 requests instead of 515, with the same 8,421,608 bytes received; listing the ZIP still uses
4 requests and 32,885 bytes. Recursive tar.gz extraction uses 3 requests instead of 273,
receiving 8,847,427 versus 8,847,361 bytes, including archive discovery. Both extracted members
have the source SHA-256. These are I/O counts, not timing benchmarks. ZIP directory parsing
uses a 64 KiB read buffer; compressed tar payload passes use a sequential response. A separate
HTTP regression withholds the response body and checks that metadata-only stat returns from
200 headers, preserving an unknown size when Content-Length is absent.

A three-member tar fixture (a small first member, an 8 MiB incompressible member, then a
small last member) checks targeted reads. Copying the first gzip member consumes 65,536
response-body bytes instead of 8,880,407, with two requests in both versions; metadata stops
at its header, and extraction makes a separate prefix pass. Copying the last plain-tar member
uses 4 requests and 98,304 bytes instead of 131 requests and 8,421,376 bytes, because prior
payloads are skipped with seeks. Both outputs match the source SHA-256. These client body-read
counts exclude any transport prefetch beyond the consumed stream and are not timing claims.

Request and I/O costs from #335, measured with the previous and updated CLI (`--stats`, a
connection-counting proxy, and `/proc` read counts) against nginx 1.29 (ranges honored) and
Python's `http.server` (ranges ignored): copying a typed 1 MB file uses 2 requests instead of 3
on both, with the same bytes; listings of `dir` and `dir/` still use 1. Extracting a 20-member
remote zip uses 3 requests and 4,465 bytes instead of 23 and 46,873, over 1 connection instead
of 23; a 5-file directory copy uses 1 connection instead of 6. Against a moto S3 server,
recursively deleting 200 objects uses 4 requests instead of 204, and a signed 6 MB upload reads
6.0 MB locally instead of 12.0 MB (a 20 MB upload still reads 40 MB). The mock S3 server of
`terrio-cli/tests/vsi.rs` checks 1,504 keys in 4 `DeleteObjects` requests and one `DELETE` (a key XML cannot carry), per-key failures,
the fallback to one `DELETE` per key, a 400 `MalformedXML` that fails without that fallback, and 36 upload requests over at most 3 connections; unit
regressions count requests and connections for plain HTTP stats and remote zip members, and
copies of `/vsimem/` archives and moves (none). `compare_vsi_with_gdal.py` reports the same 25
identical rows before and after.


```sh
python scripts/compare_vsi_with_gdal.py ../terrio-data --terrio target/release/terrio
# Add --remote to compare public cloud stores and HTTP sources.
python scripts/conformance.py ../terrio-data --only vsi
cargo test -p terrio --lib vsi::        # listings, copies, sync, refusals, links, archives
cargo test -p terrio-cli --test vsi     # S3 (local server): paging, uploads, ETag sync, bounded delete
pytest python/tests/test_vsi.py
node --test --test-name-pattern='vsi' js/tests/node.test.mjs
```


## Raster query I/O (#289)

A before/after local HTTP range-server probe uses the real 610,612-byte `cog.tif` fixture.
Ordinary lazy info opens use one request and 65,536 served body bytes in both builds.
Comparison of two distinct URLs for the same raster uses four requests and 1,221,224 bytes;
XYZ export with skip 3 uses two requests and 610,612 bytes. Reports and XYZ text agree in
the two builds. These are measurements for this fixture, not a claim for every raster.
Comparison-only out-of-line TIFF tags are now read by the comparison helper; ordinary lazy
opens do not fetch them. Comparison and XYZ strips reuse the shared block-aligned row sizing.

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

## Raster management (#289)

`scripts/compare_raster_manage_with_gdal.py` is registered as `raster-manage`.
With GDAL 3.13.3 it checks 21 cases: six creation types with two bands; nearest,
average and mode internal/external overview add/delete; metadata XML escaping;
nearest/bilinear/average updates of real DEM subsets; internal/external destination
masks; genuine fractional-cell average overlaps; and integer bounds/nodata
collisions. Values, masks, types, nodata, CRS, transforms and overview levels match
exactly in the final run. The script retains explicit numeric tolerances for future
non-exact fixtures and reports failures as differences.

A tiled 65,536 by 64 Float32 destination updated from a 128 by 16 source reports
78,608 KiB peak child RSS on this machine. The changed block window is 512 by 48;
its destination-row buffers still span the full width. This fixture is a measured
example, not a general memory bound. In-place edits require a whole-file copy.

```sh
python scripts/conformance.py ../terrio-data --only raster-manage --strict
pytest python/tests/test_raster_manage.py
cargo test -p terrio-cli --test raster_manage
```

## Raster blending (#289)

The `blend` conformance suite requires GDAL 3.13 or newer (older references are
explicitly skipped). It compares all ten operators with GDAL 3.13.3 on
Landsat-derived grayscale/RGB images, all 1–4 band combinations allowed by the
operator, controlled alpha and byte boundary samples, and opacity 0/37/100.
There are 420 cases, including OPERA-derived GeoTIFF/PNG palettes with transparent
nodata. Every pixel, type, nodata, CRS, geotransform and color interpretation is checked.
Source-over/HSV references use lazy GDALG per-band reads; generic references use
whole-dataset kernels. Gray-alpha source-over and unsafe mixed-band reference
inputs are expanded to RGBA and their output channels selected explicitly. These
normalizations avoid GDAL 3.13.3's documented bugs described in the CLI reference;
they do not waive numerical differences.

```sh
python scripts/conformance.py ../terrio-data --only blend --strict
cargo test -p terrio --lib ops::blend
cargo test -p terrio-cli --test focal blend
pytest python/tests/test_blend.py
node --test --test-name-pattern='raster blend' js/tests/node.test.mjs
```

A local HTTP range-server measurement of lazy `info` opened the 610,612-byte
`cog.tif` and the OPERA palette GeoTIFF with one GET and 65,536 response-body bytes
each after color-table retention. The table does not require pixel decoding; these
are observations for those two fixtures, not a bound for every palette TIFF.


## Footprint index creation (#289)

`compare_indexes_with_gdal.py` is registered as `indexes` and requires GDAL 3.13.
It checks real COG/USGS rasters, rotated DEM subsets, mixed source CRSs, typed metadata,
resolution filters, recursive discovery, multi-layer vector selection and GPKG/FGB/SHP
cross-reads. Schema, CRS and text/integer fields match exactly; real metadata fields
use 1e-12 relative/absolute tolerance. Vertex tolerance is 1e-8 index
CRS units, with common installed datum grids and network grid downloads disabled.
GTI band metadata, numeric grid-metadata values (independent of decimal spelling)
and shared-window pixel queries are exact. GDAL-authored GPKG cached
bounds round to 15 significant digits; GTI may consequently add one border row/column.
This default-grid difference is reported, limited to one pixel with affine coefficients
within 1e-12, and does not waive shared-window pixel equality. An explicit resolution/
bbox case requires identical dimensions. Supply these options for stable GTI extents.

```sh
python scripts/conformance.py ../terrio-data --only indexes --strict
cargo test -p terrio --lib ops::tile_index
cargo test -p terrio-cli --test tile_index --test mcp
pytest python/tests/test_tile_index.py
node --test --test-name-pattern='dataset indexes' js/tests/node.test.mjs
```

A local HTTP range-server probe of raster-index creation on `cog.tif` (610,612 bytes)
and `USGS_1_n40w106.tif` each used one GET and 65,536 served body bytes, matching
ordinary lazy info opens. Raster index construction never called the pixel source in
its panic-on-read unit fixture. These observations apply to lazy-capable readers;
formats whose reader lacks a metadata-only path may materialize the input.

## Palette validation

A local HTTP range test of Python masked-raster writes (512 × 512 RGB COG,
internal mask, uncompressed image blocks) transferred 853,046 bytes from one
source both for a pixel-only read and a mask-preserving write: two GET responses
versus three. Range caching covered the mask bytes in the pixel-read ranges.
The output mask and pixels were checked independently with GDAL 3.13.3.
This fixture is an I/O observation, not a general zero-overhead guarantee;
`test_remote_mask_write_io_and_tile_archive_refusal` measures response bytes and
source counts and confirms that the unsupported lazy tile-archive mask path
refuses the write without creating output.

Palette lookup uses a dense u16 cache at 5-bit precision (64 KiB). At 8 bits,
images up to 65,536 pixels use a pixel-bounded hash cache; larger images use a
32 MiB dense table. On the complete 2127 × 1564 Landsat RGB fixture, using a supplied
256-entry grayscale table and 8-bit diffusion, two local optimized CLI runs took
0.517/0.519 s with the previous capped hash cache and 0.284/0.289 s with the dense
cache. The output files were byte-identical. These timings are local observations,
not performance thresholds; the fixed-size cache avoids repeated linear palette
scans after 65,536 distinct query colors.

For full-precision palette mapping, images of up to 65,536 pixels use a sparse
cache bounded by their pixel count. Larger images retain the dense 32 MiB table;
5-bit mapping uses the 64 KiB table. This avoids the large fixed allocation on
small conversions without reintroducing capped-cache misses on large images.

Color-file reads enforce the 10 MiB accepted-text limit while streaming,
including sources without a size. A 64 MiB size-less HTTP response is cancelled
before completion; counters include the existing VSI metadata/index probes.
Local, in-memory and decompressed ZIP member regressions check exact-limit
success, oversized rejection and transport-error preservation. Malformed
text/XML channels outside 0–255 are rejected rather than wrapped.

## COG validation

The registered `cog-validate` suite compares GDAL 3.13.3's unified COG validator
with `validate-cog --profile gdal`, with full checks both enabled and disabled.
Real Landsat and USGS-derived COGs cover pixel/band/tile interleave, BigTIFF,
internal masks, external overviews and deliberate leader/trailer corruption.
Validity, exit status, finding counts, IFD offsets and first-block offsets must
agree exactly; diagnostic wording is not compared. Rust, Python and WASM
regressions exercise uploaded bytes and corrupt block boundaries. A counting
HTTP regression verifies automatic remote validation reads headers only while
explicit full checking detects a damaged trailer.

On the 1024×1024 random Float32 COG regression, automatic remote validation
transferred 65,536 bytes in one GET. Explicit full checking transferred
4,955,322 bytes in 76 GETs from the same source and detected the damaged final
trailer. These are observed transfer counts, not timing targets.

Local full validation also measures reads from the underlying file. On a
2048×2048 Float32 COG with 128-pixel tiles, the buffered path read 5,688,654
bytes; direct boundary reads read 7,897 bytes, with identical validation
results and one source-file open per call. The GDAL profile now uses direct
local reads for full checking. Python uploaded-byte validation borrows immutable
bytes instead of duplicating the full payload.

## GeoPackage repacking

`scripts/compare_gpkg_repack_with_gdal.py` is registered as `gpkg-repack`. GDAL 3.13.3
repacking (SQLite VACUUM) is compared on Natural Earth vector and USGS raster derivatives.
The suite checks all table records and SQL objects, rowids, text encoding, application/user
versions, auto-vacuum mode, page size, GDAL features/fields/CRS, and every raster pixel/mask.
R-tree and application triggers are exercised after repacking. Cases include WITHOUT ROWID,
expression/partial/collated indexes, 512/4096/65536-byte pages, both UTF-16 encodings, mixed
vector/raster files, and active/orphan-WAL and truncated-input refusal. An actual GDAL feature
update committed to an orphan WAL also exercises append refusal and post-checkpoint preservation.
Physical layouts/sizes may differ.

`interrupted_repack_restores_truncated_pages` stops before/during/after database writes,
checking exact recovery including truncated pages. Set `TERRIO_SQLITE_REFERENCE_PYTHON` to a
Python interpreter with the standard sqlite3 module to additionally recover these journals
with SQLite itself, for both rollback- and checkpointed-WAL file headers. Separate regressions
check renamed database refusal before journal creation, directory-sync failure before page
writes, planted journal links, and concurrent in-memory writes. The Python
`test_repack_refuses_database_locked_by_another_process` holds an exclusive SQLite
transaction in a separate process, verifies refusal without changing database bytes,
and retries successfully after release. It runs on the native macOS/Windows CI matrix
as well as Linux, exercising Windows byte-range locking when that matrix runs.

A local repack memory check used a Natural Earth GeoPackage derivative with 2,048 one-MiB
zero-filled BLOB rows and incremental auto-vacuum (2,155,819,008 bytes). Peak RSS measured
per child process with `wait4` fell from 8.27 to 6.04 GiB when all rows were live, and from
4.12 to 2.06 GiB after retaining every 128th BLOB row (19,722,240-byte compacted result).
Both before/after implementations passed SQLite integrity checks and streamed hashes of
every table value. These are single local observations, not a general memory bound; record
materialization still depends on the largest tree and row shape. The large generated files
are excluded from commits and CI.

## Multidimensional mosaic validation

The multidimensional mosaic HTTP regression serves two chunked numeric Zarr
stores and counts response bodies after coordinate planning. Reading the full
6 × 2 × 4 Int64 mosaic fetches 384 bytes in 12 chunk requests/files; reading one
time step and two X samples fetches 32 bytes in one chunk request/file. Values
are checked exactly. A separate damaged-chunk regression proves that metadata
and views outside that source extent do not read the damaged payload.

The multidimensional mosaic suite includes a Float32 0.1-degree coordinate grid
near 40 degrees with four missing rows, and CF bounds arrays with a shared,
unindexed size-two axis. Data values require zero tolerance. Float32 coordinate
preservation checks input samples exactly and permits reference-rounded generated gap
coordinate values; numeric differences and their maximum magnitude are reported.
Descending duplicate coordinates, coordinate memory limits before payload reads,
packing/unit mismatches and scalar Python input rejection have dedicated regressions.

Mosaic regressions reject mismatched CF validity bounds, missing-value markers,
unsigned interpretation and grid-mapping references. An off-grid singleton mixed
with a regular series is rejected by both GDAL 3.13.3 and Terrio, while axes
made entirely from singleton slices retain irregular-coordinate support. The
registered suite verifies this refusal without creating a Terrio destination.

Mosaic coordinate regressions preserve every supplied Float64 coordinate bit. The GDAL
comparison checks source coordinate samples exactly; generated gap coordinates match the
reference after rounding to the source type. GDAL's regenerated grid can differ from stored
Float32 samples by 3.05e-05 in the NOAA derivative; those coordinate-only differences are
reported within tolerance, while payload values still require exact equality.

Mosaic selection retains CF coordinate bounds and their indexing variables, rejecting
missing bounds targets. Nonzero per-row bounds are compared with GDAL's complete
output: GDAL 3.13.3 drops these dependencies for explicit array selection. Duplicate
and unsorted common axes are refused by both implementations. Nested selection checks
require the original `/science/wind` path and exclude a second eligible payload.
Equivalent integer/float metadata values are accepted without rounding large integers;
conflicting or missing `_CRS` attributes are rejected.

Mosaic dependency planning retains two-dimensional auxiliary coordinates, bounds and
scalar CRS definitions before accessing payloads. Bounds/auxiliary references must resolve
to consistent array paths across contributors; scalar CRS aliases are accepted only when
their metadata agrees. Extended grid-mapping coordinate tuples retain their declared order.
Tests cover conflicting CRS aliases, shadowed group names, missing dependencies and damaged
auxiliary payloads that remain unread until requested. Python/Node tests and netCDF/Zarr
round trips verify the dependency closure. The GDAL comparison checks actual bounds links,
source samples even where GDAL agrees, and exclusion of unselected two-dimensional arrays.

References retain local names when unambiguous and use absolute paths to prevent rebinding
by merged groups, following [CF group scoping](https://cfconventions.org/Data/cf-conventions/cf-conventions-1.8/cf-conventions.html#groups).
Terrio reopens these paths correctly. GDAL 3.13.3's `GetCoordinateVariables` does not resolve
absolute coordinate names; automatic discovery can omit such cross-group references,
although the arrays and reference attributes remain readable. Same-group auxiliary
coordinate discovery is checked independently through GDAL for both snapshot formats.

## Dataset identification (#289)

The `identify` conformance suite compares driver identification with GDAL 3.13.3
on real Landsat, USGS, OPERA, SRTM, netCDF/HDF5 and vector files, and raster encodings
derived from real imagery. The local run has 37 cases: 33 exact labels, three accepted
generic JP2/KML driver labels and one explicit non-comparison because the
reference build lacks Parquet. Companion-file encodings use `--deep`; renamed
GeoTIFFs and unknown inputs are also checked. Identification does not validate
payload values or metadata. Missing mandatory datasets fail the suite.

Core/API regressions check bounded prefixes, filename hints, compiled driver
availability, recursive scans stopping at Zarr directories, symlink loops, unknowns,
errors and MCP invocation. A local HTTP server serving a 64 MiB padded real GeoTIFF
checks one GET and 16,384 served bytes with ranges, and cancellation before the full
file is sent when ranges are ignored. Server buffering can send more than the
16 KiB retained by the identifier in the latter case.

```sh
python scripts/conformance.py ../terrio-data --only identify --strict
cargo test -p terrio --lib inspection::
cargo test -p terrio --lib bounded_prefixes_stop_even_when_ranges_are_ignored
cargo test -p terrio-cli --test inspection
pytest python/tests/test_inspection.py
node --test --test-name-pattern='identify inspects' js/tests/node.test.mjs
```


## Dataset file management (#289)

The `dataset-files` suite uses GDAL 3.13.3 and real Landsat/Natural Earth derivatives.
It compares encoded payload bytes, owned file inventories, every pixel or feature,
field types, nodata, masks, overviews, CRS, transforms and metadata after copies and
renames into a different folder, then checks deletion and unrelated siblings.
Coverage includes TIFF/PAM/overviews/masks, world-file-only TIFFs, RPCs, EHdr, ENVI
(including `.img` data), ERS, SAGA, Idrisi, HFA spill files, PNG/JPEG world files,
ASCII and Surfer grids, Zarr, Shapefile, MapInfo TAB/MIF, CSV, GeoPackage, GML,
MVT pyramids and VRT TIFF/netCDF references. GDAL reads MVT zoom-level directories;
the whole-pyramid reference copy uses `gdal vsi copy --recursive`.

The local run has 51 cases: 46 identical and five explicit metadata/reference
corrections. GDAL's generic copy loses the CSV CRS `.prj`, omits GML XSD schemas,
leaves the Idrisi reference-system name stale, and does not make the two VRT
reference cases independent of their originals. Terrio preserves source values
and metadata in those cases; they are reported within tolerance as behavior
differences rather than as identical. The derived WGS84 Idrisi fixture explicitly
sets zero WGS84 deltas because GDAL's writer can emit uninitialized tiny values.

A local HTTP server serves a padded 8 MiB real TIFF with overview, mask and PAM
sidecars. The suite measures successful/missing requests, bytes served and distinct
existing files, and bounds metadata overhead without assuming a content type.
The data, masks, overviews, georeferencing and metadata must remain exact.

GDAL source-resolution comparisons cover both integer `relativeToVRT` flags for sourced
bands and boolean flags for raw bands (relative by default when omitted), with distinct same-named assets in the working
and VRT directories. Native paths and encoded filename whitespace retain exact identity.

Core regressions cover source/hard-link aliases, batch preflight, stale sidecars,
file/directory collisions, nonregular inputs, symlink deletion, partial-stage
rollback, unavailable VRT references, raw VRT sources and CDATA/entity handling.
CLI/MCP, release-wheel Python and release WASM tests exercise the public surfaces.

VRT rename moves owned definitions but copies referenced assets, preserving other
users of those assets. The suite verifies native and read-only HTTP dependencies; HTTP references use
explicit external-reference permission. Core and binding regressions reject default
outside-tree references, symlink escapes, linked directory payloads and unrecognized
non-raw files. A permission-denied cleanup regression checks successful committed
copy/delete reports and structured recovery paths. Shared companion overwrite tests
include an absent target primary with a `.prj` still owned by another dataset.
VRT/GML definitions with no rebased references retain their original bytes.

Archive inventory probes serve the same padded 8 MiB TIFF from stored and deflated
ZIPs. The stored ZIP metadata probe transferred 360,684 bytes in 16 GETs, compared
with 8,716,760 bytes in 142 GETs before the bounded member reader. The deflated
probe transferred 120,211 bytes in 13 GETs. These are fixture-specific metadata
measurements; partial probes do not validate whole-member CRCs. The reader caches
at most 16 KiB of member content and streams/discards preceding bytes when a
compressed member needs a noncontiguous read. Complete copies retain archive
extractor integrity checks.

A separate local benchmark uses a generated 2,000-tile VRT from real 32×32 Landsat
subsets. Copying its 2,001 files took 155.04 seconds before path indexing and
2.62 seconds afterward, with all GDAL-read pixels and checksum 32088 unchanged.
Batch deletion of the 2,000 TIFFs took 13.66 versus 0.28 seconds and preserved an
unrelated file. These are local observations rather than general timing guarantees.

```sh
python scripts/conformance.py ../terrio-data --only dataset-files --strict
python -m pytest python/tests/test_dataset_files.py
```

## Parquet metadata aggregation

`scripts/compare_parquet_metadata_with_gdal.py` is registered as `parquet-metadata`.
With GDAL 3.13.3 and PyArrow, it compares Natural Earth partitions' row groups, column
paths/offsets/statistics, all Arrow values and GDAL properties/geometries. GeoParquet bounds
and encoded Arrow schema metadata are checked separately. GDAL's loss of CRS/type metadata
is reported as a reference correction rather than metadata-identical agreement. A local S3
server measures GET requests and downloaded bytes for a larger real-data derivative and
checks the uploaded metadata file by reading every value through Arrow.

The metadata command was [added in GDAL 3.13](https://gdal.org/en/stable/programs/gdal_driver_parquet_create_metadata_file.html).
The comparison suite explicitly skips its reference when that command is unavailable;
Rust and binding regressions remain independent of that reference command.

## Dataset content checking (#289)

The registered `dataset-check` suite compares with GDAL 3.13.3 on Landsat, USGS,
OPERA, SRTM, scientific wind arrays and vector datasets, plus derived real-data
fixtures. It has 30 cases: 29 matching read outcomes/I/O checks and one explicit Parquet
non-comparison because the reference build lacks that driver. It checks base-band
reads, damage in the last compressed tile, truncated GeoJSON, all requested
overviews/masks and every array in grouped scientific data. Auxiliary reads also
compare sample/mask counts with GDAL's Python API. The suite compares readability,
not pixel equality or checksums. Both readers accepting a malformed GeoPackage
geometry blob is recorded explicitly rather than claimed as corruption detection.

A local range-server probe of the real 610,612-byte `cog.tif` used two GETs and
610,612 served bytes for `info --stats`, and three GETs and 626,996 bytes for
`check`, with one raster worker. The extra GET and 16,384 bytes are the bounded
identification prefix before the ordinary lazy reader opens the source. These
are fixture-specific I/O counts, not a general request or memory guarantee.

A real wind-derived 2048×2048 Float32 Zarr array with four 1024×1024 storage
chunks checks HTTP payload requests. Before storage-chunk alignment, each chunk
was fetched 16 times (64 chunk GETs); the updated checker fetches each once
(four chunk GETs, 16,777,216 encoded payload bytes, excluding metadata).
Chunks that exceed the 8 MiB value budget can still be decoded repeatedly;
this bounds output buffers rather than codec working memory.

Core tests check late-window failures, bounded planning for million-column rasters,
repeated reads of oversized strips and preservation of every fallback reader error,
all-band/level/mask counts, scalar/empty scientific arrays and complete mixed
GeoPackage contents. CLI tests verify failure status and MCP discovery/invocation.
Python additionally checks an HDF5 user block beyond the identification prefix
(using GDAL and `h5repack`); uploaded and open scientific handles must read all arrays.
Focused Node tests read real COG, FlatGeobuf and scientific uploads when test data
is available.

```sh
python scripts/conformance.py ../terrio-data --only dataset-check --strict
cargo test -p terrio --lib check::
cargo test -p terrio-cli --test inspection
pytest python/tests/test_check.py
TERRIO_TEST_DATA=../terrio-data node --test --test-name-pattern='checkDataset' js/tests/node.test.mjs
```
