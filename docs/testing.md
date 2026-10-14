# Testing Against GDAL

GDAL is terrio's reference: every driver and operation is compared with GDAL 3.12 on real-world
files, and the comparisons run in continuous integration on every pull request.

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

## What is compared

| Area | Script | Checks |
|---|---|---|
| Dataset summaries | `scripts/compare_with_gdal.py` (`--pixels`) | size, bands, data types, nodata, geotransform, EPSG code, statistics, pixels; layers, feature counts, fields, extents, geometry types |
| Raster round trips | `scripts/raster_roundtrip.py` | GDAL writes → terrio reads; terrio writes → GDAL reads, pixel for pixel; COGs pass GDAL's validator |
| Window and overview reads | `scripts/compare_windows_with_gdal.py` | every overview level of every GeoTIFF, local and remote over `/vsicurl/` |
| Raster operations | `scripts/compare_ops_with_gdal.py` | translate, warp (all resampling methods), mosaic and VRT against `gdal_translate`, `gdalwarp`, `gdalbuildvrt`, `gdal_merge.py` |
| Vector operations | `scripts/compare_vector_ops_with_gdal.py` | `ogr2ogr` filters, `gdal_rasterize` and `gdal_polygonize.py` |
| Terrain and raster algebra | `scripts/compare_dem_calc_with_gdal.py` | every `gdaldem` mode and option, `gdal_calc.py` expressions, types, nodata and extents, `gdal_proximity.py`, `gdal_sieve.py` |
| STAC and GeoZarr | `scripts/compare_stac_geozarr.py` (`stac`, `geozarr`) | STACIT subdatasets, mosaic sizes, geotransforms, CRSs and windows on Earth Search; STAC footprint fields vs `ogrinfo`; GeoZarr stores read by GDAL and rioxarray, overviews vs `gdaladdo` |
| Tile archives | `scripts/compare_tiles_with_gdal.py` | MBTiles written by `gdal_translate`/`gdaladdo` and by terrio, pixel for pixel per zoom level; PMTiles pixels; vector tiles read by both tools and written by `ogr2ogr` and terrio: feature counts, areas, lengths per feature key |
| Point clouds | `scripts/compare_pointclouds.py` | every value vs laspy; headers, COPC queries, `writers.gdal` grids and reprojection vs PDAL |

## Results

| Area | Agreement with GDAL |
|---|---|
| Raster drivers | Pixel-identical reads for every test file; terrio-written GTiff, COG, JP2, Zarr and Esri formats read back identically |
| Vector drivers | Identical layers, counts, fields, CRS, extents and geometry types for 31 of 32 files |
| Window reads | 160 of 160 local and 22 of 22 remote windows identical |
| `translate` | 100% identical pixels (nearest); 99.99% for bilinear, cubic and average |
| `warp` | 99.999–100% identical (nearest); others within one float32 rounding step |
| `mosaic` | 100% identical to `gdal_merge.py` and `gdalbuildvrt` |
| `convert` filters | 28 of 28 option cases identical to `ogr2ogr`, feature by feature |
| `rasterize` | 0 differing pixels in 18 cases |
| `polygonize` | Same polygon counts and areas per value; re-rasterizing reproduces the input |
| `dem` | 81 of 81 cases identical (every mode and option, on four DEMs) |
| `calc` | 35 of 37 cases identical; the other two within one float32 rounding step (NumPy's vectorized float32 `sin`, `exp`, `log10`, `arctan2`) |
| `proximity`, `sieve` | 13 of 13 cases identical |
| STAC and GeoZarr | 16 of 16 STAC checks (subdatasets, mosaics and windows identical to GDAL STACIT's, 328 footprint fields as `ogrinfo`'s) and 60 of 60 GeoZarr checks |
| Point clouds | 485 of 485 checks: every value identical to laspy; COPC queries and grids identical to PDAL |

## Running the tests

```sh
scripts/fetch_test_data.sh ../terrio-data
TERRIO_TEST_DATA=../terrio-data cargo test --workspace
python scripts/compare_with_gdal.py ../terrio-data
```

The Rust tests hold expected values taken from GDAL, so they run without GDAL installed; the
comparison scripts need GDAL's Python bindings.
