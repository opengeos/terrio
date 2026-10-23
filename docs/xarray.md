# xarray

`terrio.xarray` brings [rioxarray](https://corteva.github.io/rioxarray/)'s API to terrio, without
GDAL: rasters open lazily as xarray DataArrays, with rioxarray's dimensions, coordinates,
attributes and encoding, and the `.terrio` accessor has rioxarray's `.rio` methods.

```sh
pip install "terrio[xarray]"   # terrio, xarray and dask
```

```python
import xarray as xr
import terrio.xarray as txr

url = "https://prd-tnm.s3.amazonaws.com/StagedProducts/Elevation/1/TIFF/current/n40w106/USGS_1_n40w106.tif"
dem = txr.open_rasterio(url, masked=True, chunks=True)  # reads the headers only
ds = xr.open_dataset(url, engine="terrio")              # or through xarray's backend

dem.terrio.crs, dem.terrio.transform(), dem.terrio.bounds(), dem.terrio.resolution()
boulder = dem.terrio.clip_box(-105.35, 39.85, -105.15, 40.0, crs="EPSG:4326")  # fetches its tiles only
utm = boulder.terrio.reproject(boulder.terrio.estimate_utm_crs(), resolution=30, resampling="bilinear")
utm.terrio.to_raster("boulder.tif", driver="COG", compress="DEFLATE")
utm.terrio.to_raster("boulder.zarr")                    # GeoZarr
utm.to_dataset(name="elevation").to_zarr("boulder_xarray.zarr")  # georeferencing kept (CF)

merged = txr.merge_arrays([txr.open_rasterio(p) for p in ("a.tif", "b.tif")])
```

The [xarray notebook](notebooks.md) runs these on real data, in Google Colab or locally.

## Code written for rioxarray

`terrio.xarray.register_rio_accessor()` registers the accessor as `.rio` too (unless rioxarray
is imported, whose accessor stays), so that code written for rioxarray runs unchanged once
`rioxarray.open_rasterio` becomes `terrio.xarray.open_rasterio`:

```python
import terrio.xarray as txr

txr.register_rio_accessor()
da = txr.open_rasterio("dem.tif", masked=True)
clipped = da.rio.clip(geometries, crs="EPSG:4326")  # the same object as da.terrio
```

CRSs are `terrio.rio.crs.CRS` objects, terrio's rasterio-compatible CRS class (they compare equal
to `"EPSG:4326"`, EPSG codes and other CRSs); windows are `terrio.rio.windows.Window` (rasterio's
work too); errors have rioxarray's names (`terrio.xarray.exceptions`: `MissingCRS`,
`NoDataInBounds`, `OneDimensionalRaster`...).

## How it works

- **Lazy reads.** `open_rasterio` reads a raster's headers and metadata. Its pixels are an xarray
  `BackendArray` indexed through `explicit_indexing_adapter`: an index or slice of the array
  reads that window of the bands it selects, and nothing else. GeoTIFFs and COGs (local, or
  `https://`, `s3://`, `gs://`, `az://` and GDAL's `/vsicurl/` paths) are read tile by tile, and
  remote ones fetch only the tiles a window needs, with HTTP range requests; other formats terrio
  reads (Zarr, netCDF, ERDAS Imagine, JPEG 2000...) are read into memory when first read.
- **Dask.** `chunks` (an int, a dict, `True` or `"auto"` for chunks aligned on the file's blocks,
  as rioxarray chooses them) makes dask arrays, with rioxarray's chunks. Reads are thread-safe
  and parallel: each concurrent reader borrows a terrio raster of its own (lazily read files are
  opened again, headers only). Each raster caches the bytes it fetched, so `lock=True` reads
  through a single raster, one chunk at a time, which fetches the tiles chunks share (the bands
  of a pixel-interleaved COG) once. Arrays pickle (paths, not file handles), for dask's process
  and distributed schedulers.
- **Metadata.** As rioxarray: `band`, `y` and `x` dimensions, coordinates at pixel centres (2-D
  `xc` and `yc` for rotated grids), a `spatial_ref` coordinate with `crs_wkt`, `spatial_ref`,
  `GeoTransform` and the CF grid mapping attributes of the CRS (as `pyproj.CRS.to_cf()` gives
  them, derived here from the CRS's WKT), a `grid_mapping` encoding, tags (dataset and first
  band) as attributes with numbers parsed, `_FillValue`, `scale_factor` and `add_offset`,
  `long_name` from band descriptions, `units`, `AREA_OR_POINT`. `masked=True` masks nodata (or
  alpha) pixels with NaN and moves `_FillValue` to the encoding; `mask_and_scale=True` also
  applies scales and offsets. `overview_level` opens an overview (with GDAL's overview
  geotransform and band metadata), `band_as_variable=True` opens bands as variables.
- **Operations.** `clip_box` and `isel_window` select windows (lazily); `clip` rasterizes the
  geometries with terrio's rasterizer (as `gdal_rasterize`); `reproject` and `reproject_match`
  warp with terrio's implementation of GDAL's warper; `merge_arrays` runs `terrio.rio.merge`.
  South-up arrays are flipped for the warper and the writers, and back.
- **Writing.** `to_raster` writes with terrio's writers: GeoTIFF (`driver="COG"` for Cloud
  Optimized GeoTIFFs, GDAL creation options such as `compress`, `tiled`, `blockxsize`,
  `predictor`), Zarr (GeoZarr, with `.zarr`), netCDF, PNG and the other formats terrio writes,
  with the CRS, transform, nodata, scales, offsets, band descriptions, tags and the source's
  layout. Dask arrays are computed chunk by chunk into the writer's buffer (`compute=False`
  returns a `dask.delayed`); terrio writers hold the whole output in memory and write the file
  when closed.
- **Zarr.** DataArrays opened or reprojected with terrio keep their georeferencing through
  xarray's own `to_zarr` (the `spatial_ref` coordinate and `grid_mapping`, as rioxarray writes
  them): terrio, rioxarray and GDAL read it back (open with `decode_coords="all"`).
  `to_raster("x.zarr")` writes GeoZarr with terrio's Zarr writer, read by GDAL and rioxarray.

## API

| | Covered |
|---|---|
| `open_rasterio` | `filename` (paths, URLs, `terrio.rio` datasets), `parse_coordinates`, `chunks`, `cache`, `lock`, `masked`, `mask_and_scale` (with `_Unsigned`), `default_name`, `decode_times`, `decode_timedelta`, `band_as_variable`, `overview_level`; netCDF extra dimensions and ENVI metadata as coordinates |
| `xr.open_dataset(..., engine="terrio")`, `xr.open_dataarray` | rioxarray's backend options (`mask_and_scale=True` by default), `overview_level`, `open_kwargs`, `drop_variables` |
| DataArray and Dataset `.terrio` | `crs`, `write_crs`, `set_crs`, `estimate_utm_crs`, `grid_mapping`, `write_grid_mapping`, `transform(recalc=)`, `write_transform`, `write_coordinate_system`, `bounds`, `transform_bounds`, `resolution`, `shape`, `width`, `height`, `count`, `x_dim`, `y_dim`, `set_spatial_dims`, `set_attrs`, `update_attrs`, `set_encoding`, `update_encoding`, `isel_window` (`pad`), `slice_xy`, `clip_box` (`crs`, `auto_expand`), `clip` (`crs`, `all_touched`, `drop`, `invert`, `from_disk`), `pad_box`, `reproject` (`dst_crs`, `resolution`, `shape`, `transform`, `resampling`, `nodata`), `reproject_match`, `interpolate_na` (with SciPy), `to_raster` (`driver`, `dtype`, `tags`, `windowed`, `recalc_transform`, `compute`, creation options); Datasets: `vars` |
| DataArray `.terrio` only | `nodata`, `encoded_nodata`, `write_nodata` (`encoded`), `set_nodata`, `pad_xy`, `to_rasterio_dataset` |
| `terrio.xarray` | `merge_arrays`, `merge_datasets` (`bounds`, `res`, `nodata`, `method`, `crs`), `register_rio_accessor`, `set_options` (`export_grid_mapping`, `skip_missing_spatial_dims`), `crs_to_cf`, `exceptions` |

## Agreement with rioxarray

`scripts/compare_xarray_with_rioxarray.py` runs every check twice, with rioxarray 0.23 (rasterio
1.5, GDAL 3.12) and with `terrio.xarray`, and compares the results with
`xarray.testing.assert_identical` (dimensions, coordinates, values with NaNs, attributes, the
`spatial_ref` coordinate and its attributes), the encodings (`_FillValue`, `dtype`,
`scale_factor`, `add_offset`, `grid_mapping`, `preferred_chunks`, `rasterio_dtype`) and dask
chunks. Files written by `to_raster` are read back by rioxarray, terrio and rasterio; reads of
files served over HTTP count the bytes fetched by each library. It covers synthetic GeoTIFFs
written by GDAL (uint8 RGB with nodata and descriptions, int16 with scale, offset and overviews,
float32 with NaN nodata, uint16, float64), the real files of `terrio-data` (DEMs with NaN,
-999999, -inf and -32768 nodata, multiband Landsat, COGs with overviews, four USGS 3DEP tiles to
merge, NAD83, UTM, Web Mercator and Albers CRSs) and the 3DEP COG on Amazon S3. The pytest suite
(`python/tests/test_xarray_vs_rioxarray.py`) runs the same checks when rioxarray is installed.

With the real files and the remote COG, 257 checks:

| Section | Checks | Identical | Identical but the CRS's WKT text | Within tolerance | Differ |
|---|---|---|---|---|---|
| open (default, masked, mask_and_scale, chunks, overviews, band_as_variable, engine) | 130 | 122 | 8 | 0 | 0 |
| isel and sel (slices, steps, points, fancy indexing) | 12 | 10 | 2 | 0 | 0 |
| windows read over HTTP (bytes fetched) | 11 | 8 | 3 | 0 | 0 |
| properties and metadata writers | 15 | 15 | 0 | 0 | 0 |
| clip_box and clip | 25 | 22 | 3 | 0 | 0 |
| pad_box and pad_xy | 9 | 8 | 1 | 0 | 0 |
| reproject and reproject_match | 33 | 13 | 0 | 20 | 0 |
| merge_arrays and merge_datasets | 3 | 1 | 2 | 0 | 0 |
| to_raster (read back by rioxarray, terrio and rasterio) | 13 | 13 | 0 | 0 | 0 |
| Zarr (xarray's to_zarr, GeoZarr) | 2 | 1 | 0 | 1 | 0 |
| interpolate_na | 3 | 3 | 0 | 0 | 0 |
| remote COG | 1 | 0 | 1 | 0 | 0 |
| **all** | **257** | **216** | **20** | **21** | **0** |

- **The CRS's WKT text.** For two of the GeoTIFFs (the NAD83 3DEP tiles, an Albers CRS stored as
  ESRI WKT), terrio writes the CRS's WKT differently from GDAL (`298.257222101` against GDAL's
  `298.257222101004` for GRS 1980's inverse flattening, other names): the CRSs are the same, and
  so are the CF attributes but the ones derived from the text (`inverse_flattening`, names).
  Everything else is identical.
- **Within tolerance.** Reprojections compute GDAL's suggested output grid with terrio's
  arithmetic: transforms agree to 1e-15 (relative), which moves pixel-centre coordinates by as
  much; with nearest-neighbour resampling, pixels are identical. Bilinear and cubic kernels sum
  their weights in another order than GDAL (values within 1e-9, relative; integer outputs within
  float rounding of x.5 can round either way, by 1) and, next to nodata pixels, GDAL's warper
  renormalizes weights differently (under 1% of the pixels of rasters with nodata).
- **Bytes fetched.** A window read over HTTP fetches only the tiles it needs: 0.4 to 1.6 MB of 6
  to 52 MB files, within 15% of what GDAL fetches; opening fetches 0.13 MB of headers (GDAL:
  0.02 MB). With dask, each concurrent reader opens the file again (headers only) and caches its
  own tiles, so the four band chunks of a pixel-interleaved Landsat COG fetch a window's tiles
  four times (2.8 MB, against 0.64 MB for GDAL, whose block cache is shared); with `lock=True`,
  reads go through one raster and fetch them once (0.66 MB).

## Subdatasets, GCPs and RPCs

A file of subdatasets (netCDF, HDF5, Zarr groups) opens as rioxarray opens it: a Dataset of its
variables, one per shape (a list of Datasets when their shapes differ), filtered with `group=` and
`variable=` as rioxarray filters them, the file's global attributes on the Dataset and each
variable's own on its array, with `time` and other netCDF dimensions as coordinates. The
netCDF reader gives GDAL's metadata (`NC_GLOBAL#...`, `var#attr`, `NETCDF_DIM_*`, band items),
values outside a variable's `valid_range` read as nodata, and packed variables' scales and
offsets, as GDAL reads them.

Images georeferenced by ground control points or RPCs open without `x` and `y` coordinates, their
GCPs and RPCs stored in the grid mapping variable as rioxarray stores them (`get_gcps`,
`write_gcps`, `get_rpcs`, `write_rpcs`); `to_raster` writes them back, rows as they are, and
`reproject` warps through the GCPs or the RPCs (with GDAL's `RPC_*` options, such as
`RPC_HEIGHT` or `RPC_DEM`), as rioxarray does. An image with both (ICEYE quicklooks) is warped
through its GCPs, as GDAL picks them, where rioxarray passes both to rasterio, which refuses
them.

```python
from terrio.xarray import open_rasterio

ds = open_rasterio("ESACCI-SOILMOISTURE-L3S-SSMV-COMBINED-20200101000000-fv09.1.nc")  # sm, flag, t0...
sm = open_rasterio("soil.nc", variable=["sm", "flag"])
grd = open_rasterio("https://iceye-open-data-catalog.s3.amazonaws.com/.../ICEYE_..._GRD.tif")
grd.terrio.get_gcps()                     # 100 ground control points
utm = grd.terrio.reproject("EPSG:32633", resolution=50)
```

On an ESA CCI soil moisture netCDF (8 variables), ICEYE GRD (GCPs, a 1 GB COG read by window) and
quicklook (RPCs) images, every variable, coordinate and attribute, the GCPs and RPCs, the files
`to_raster` writes and a reprojection through the GCPs are identical to rioxarray's.

## Differences

- Only the CF convention is written (`convention="CF"`); Zarr `proj:` and `spatial:` attributes
  are read. A grid mapping without `crs_wkt` or `spatial_ref` (CF attributes only) gives no CRS.
- Metadata of formats other than GeoTIFF comes from terrio's readers: tags, band descriptions,
  block shapes (`preferred_chunks`) and `.aux.xml` statistics of netCDF, Zarr, JPEG 2000 or
  GeoPackage rasters can differ from GDAL's; pixels, coordinates and CRSs are the same. Over
  HTTP, terrio does not read `.aux.xml` sidecar files.
- terrio writes north-up or south-up grids, not rotated ones. Writers other than GeoTIFF do not
  store band scales and offsets: `to_raster` warns when it would lose them.
- `clip(from_disk=True)` of an overview clips in memory. `estimate_utm_crs` knows the WGS 84,
  NAD83 and NAD27 UTM zones. Complex data types are not supported.
