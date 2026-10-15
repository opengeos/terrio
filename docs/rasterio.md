# Coming from rasterio

`terrio.rio` is a rasterio-compatible API on top of terrio. Typical rasterio scripts switch with
an import change:

```python
import terrio.rio as rasterio
from terrio.rio.windows import Window

with rasterio.open("dem.tif") as src:
    profile = src.profile
    dem = src.read(1, window=Window(0, 0, 512, 512), masked=True)

profile.update(count=1, compress="deflate", predictor=2)
with rasterio.open("dem_x2.tif", "w", **profile) as dst:
    dst.write(dem.filled() * 2, 1, window=Window(0, 0, 512, 512))
```

`from rasterio.x import y` becomes `from terrio.rio.x import y`. Nothing else changes: band
indexes start at 1, transforms are `affine.Affine` objects, windows are rasterio's (fractional
offsets and sizes included), reads of datasets with nodata (or an alpha band) give masked arrays,
and errors are rasterio's (`RasterioIOError`, `CRSError`, `WindowError`...), raised for the same
misuse.

It needs only NumPy and [affine](https://github.com/rasterio/affine) (installed with terrio);
`terrio.rio.plot` uses matplotlib when it is installed.

## How it works

- **Reading.** GeoTIFFs and COGs, local or remote (`https://`, `s3://`, `gs://`, `az://`, GDAL's
  `/vsicurl/` and `/vsis3/`), are read lazily by window: opening reads the headers, a read
  decodes (and for remote files fetches) only the tiles it needs. JPEG 2000 files (Sentinel-2
  bands) are read the same way, as GDAL's JP2OpenJPEG driver reads them: their resolution levels
  are the overviews, and a window decodes only the tiles, precincts and code blocks it covers.
  Reads at a lower resolution go through the overviews as GDAL's would, with GDAL's choice of
  overview and its nearest-neighbour arithmetic. Other formats terrio reads (ASCII grids, ERDAS
  Imagine, ENVI, netCDF, Zarr...) are read whole when opened.
- **Georeferencing.** Rotated and south-up transforms are read and written (a GeoTIFF
  `ModelTransformationTag`, as GDAL writes them) and warped as GDAL warps them; GCPs (GeoTIFF tie
  points) are read and written, and warped with GDAL's GCP transformers: a homography from 4 or 5
  GCPs, a polynomial of order 1 to 3 (`MAX_GCP_ORDER`) or thin plate splines
  (`SRC_METHOD=GCP_TPS`), as `gdalwarp` does; `transform.xy`, `rowcol` and `GCPTransformer` work
  with GCPs. RPCs are read (the GeoTIFF `RPCCoefficientTag`, `_RPC.TXT` and `.RPB` sidecars) and
  written.
- **Metadata.** Profiles, tags, band descriptions, scales, offsets, units, color interpretation,
  color tables, block shapes, overviews and mask flags are read from the TIFF tags as GDAL reads
  them (`GDAL_METADATA`, `GDAL_NODATA`, `TIFFTAG_*` items, the `IMAGE_STRUCTURE` domain), and from
  `.aux.xml` sidecars.
- **Writing.** Writers (`w`, `w+`, `r+`) hold their pixels in memory and write the file when
  closed: incremental and windowed writes work in any order. Rasters larger than
  `TERRIO_RIO_IN_MEMORY_LIMIT` bytes (512 MB by default) keep their pixels in a temporary file
  next to the output instead, mapped in memory, and GeoTIFFs are then written in rows of blocks
  (`terrio.GeoTiffWriter`), without the raster in RAM. GeoTIFFs are written with GDAL's
  layout for the profile: strips or tiles (`tiled`, `blockxsize`, `blockysize`), pixel or band
  interleaving, compression (`lzw`, `deflate`, `zstd`, `packbits`) with predictors 2 and 3,
  photometric RGB and alpha bands as GDAL defaults to them, tags, band metadata, color tables,
  overviews (`build_overviews` with `nearest`, `average` or `mode`) and GDAL's internal mask
  (`write_mask`, a 1-bit mask image read back by GDAL as a per-dataset mask). `driver="COG"`
  writes a Cloud Optimized GeoTIFF.
- **Nodata.** Each band's nodata value is read, from the file or an `.aux.xml` sidecar (where GDAL
  keeps per-band values); GeoTIFFs hold one value for all bands, so a writer given different
  values writes the last band's, with GDAL's warning.
- **Operations.** `features.rasterize`, `features.shapes`, `warp.reproject`, `vrt.WarpedVRT` and
  `merge.merge` run terrio's implementations of GDAL's algorithms (`gdal_rasterize`,
  `gdal_polygonize`, the warper); `merge` and `mask` follow rasterio's own Python algorithms.

## API

| Module | Covered |
|---|---|
| `rio.open` | modes `r`, `r+`, `w`, `w+`; paths, `pathlib.Path`, URLs, file objects, `MemoryFile`; creation options as keywords |
| datasets | `name`, `mode`, `closed`, `driver`, `width`, `height`, `count`, `shape`, `dtypes`, `nodata`, `nodatavals`, `crs`, `transform`, `bounds`, `res`, `profile`, `meta`, `indexes`, `block_shapes`, `block_windows`, `block_window`, `overviews()`, `tags()`, `update_tags`, `descriptions`, `set_band_description`, `colorinterp`, `colormap`, `write_colormap`, `scales`, `offsets`, `units`, `set_band_unit`, `mask_flag_enums`, `compression`, `interleaving`, `photometric`, `read` (`indexes`, `window`, `out`, `out_shape`, `resampling`, `masked`, `boundless`, `fill_value`, `out_dtype`), `read_masks`, `dataset_mask`, `write` (`indexes`, `window`, masked arrays), `write_mask`, `sample`, `index`, `xy`, `window`, `window_transform`, `window_bounds`, `build_overviews`, `statistics`, `stats`, `checksum`, `lnglat`, `close` |
| `rio.crs` | `CRS.from_epsg`, `from_string`, `from_wkt`, `from_proj4`, `from_dict`, `from_user_input`, `from_authority`; `to_epsg`, `to_authority`, `to_wkt`, `to_string`, `to_proj4`, `to_dict`; `is_geographic`, `is_projected`, `is_valid`, `linear_units`, `linear_units_factor`, `units_factor`; equality with CRSs, strings and codes |
| `rio.transform` | `Affine`, `from_origin`, `from_bounds`, `array_bounds`, `rowcol`, `xy` (affine transforms or GCPs), `AffineTransformer`, `GCPTransformer`, `guard_transform`, `IDENTITY` |
| `rio.windows` | `Window` (`from_slices`, `round_offsets`, `round_lengths`, `round`, `crop`, `toslices`, `toranges`, `intersection`), `from_bounds`, `bounds`, `transform`, `intersection`, `union`, `intersect`, `crop`, `evaluate`, `subdivide`, `get_data_window`, `round_window_to_full_blocks` |
| `rio.features` | `rasterize` (`replace` and `add`, `all_touched`, `out`, `dst_path`), `shapes` (mask, 4/8 connectivity), `geometry_mask`, `geometry_window`, `bounds`, `is_valid_geom`, `sieve`, `dataset_features` |
| `rio.warp` | `reproject` (arrays, bands, datasets; rotated, south-up and GCP sources; every resampling but `gauss`), `calculate_default_transform` (bounds or GCPs), `aligned_target`, `transform`, `transform_bounds`, `transform_geom` |
| `rio.merge` | `merge` (`bounds`, `res`, `nodata`, `dtype`, `indexes`, `method` first/last/min/max/sum/count or a function, `target_aligned_pixels`, `masked`, `dst_path`) |
| `rio.mask` | `mask` (`crop`, `pad`, `all_touched`, `invert`, `filled`, `nodata`, `indexes`), `raster_geometry_mask` |
| `rio.vrt` | `WarpedVRT` (`crs`, `transform`, `width`, `height`, `resampling`, `src_nodata`, `nodata`, `dtype`) |
| `rio.io` | `MemoryFile`, `ZipMemoryFile`, `DatasetReader`, `DatasetWriter` |
| `rio.control`, `rio.rpc` | `GroundControlPoint`; `RPC` (`to_dict`, `to_gdal`, `from_gdal`); datasets' `gcps` and `rpcs`, read and written |
| `rio.shutil` | `exists`, `copy` (paths or datasets, creation options), `copyfiles`, `delete` |
| `rio.stack` | `stack` (`bounds`, `res`, `nodata`, `dtype`, `indexes`, `resampling`, `target_aligned_pixels`, `use_highest_res`, `masked`, `dst_path`, `dst_kwds`), as `rio stack` |
| `rio.env`, `rio.session` | `Env` (GDAL options and credentials become the environment variables terrio reads; others are accepted and ignored), `AWSSession`, `GSSession`, `AzureSession` |
| others | `rio.plot` (`show`, `show_hist`, `reshape_as_image`...), `rio.errors`, `rio.enums`, `rio.dtypes`, `rio.coords.BoundingBox`, `rio.profiles.DefaultGTiffProfile`, `rio.sample`, `rio.band`, `rio.pad` |

## Agreement with rasterio

`scripts/compare_rio_with_rasterio.py` runs every check twice, with rasterio 1.5 (GDAL 3.12) and
with `terrio.rio`, and compares the results: arrays bit for bit, masks, profiles, tags,
transforms, windows, CRSs, polygons. It covers synthetic rasters, real files of `terrio-data`
(GeoTIFFs and COGs of several data types, multiband Landsat, DEMs with nodata, a palette raster,
other formats), a remote COG, and five rasterio documentation examples run unchanged but for the
import (`scripts/rio_examples`: reading, windowed reading and writing, reprojection, masking by a
polygon, merging tiles), whose output and files are compared. The pytest suite
(`python/tests/test_rio_vs_rasterio.py`) runs the same checks when rasterio is installed.

With the real files and the remote COG, 160 checks give 128 identical results, 17 within
tolerance and 15 known differences:

| Result | What |
|---|---|
| identical | metadata of every GeoTIFF (profile, tags, band metadata, masks' sources, blocks, overviews, color tables); full, windowed, fractional-window, masked, boundless and out-of-range reads; masks; reads at lower resolution through overviews and upsampled (nearest); `out_dtype`; `sample`, `index`, `xy` and window helpers; files written by `terrio.rio` read back by rasterio (12 profiles, with windowed writes, tags, band metadata, color tables, overviews, `r+` updates) and files written by rasterio read by `terrio.rio`; `MemoryFile`; `features.rasterize` (polygons, lines, points, holes, `all_touched`, `add`), `geometry_mask`, `geometry_window`; `features.shapes` (identical polygons and values, compared as sets); `warp.reproject` with nearest neighbour; rotated and south-up files, GCPs (written, read, `xy` and `rowcol` with GCPs), RPCs, internal masks (tiled, striped, `r+`) and per-band nodata from sidecars; `shutil` and `stack`; `WarpedVRT` reads; `merge` (every method and option); `mask`; the remote COG; CRS helpers, transforms and windows |
| within tolerance | coordinates transformed by terrio's projection code and PROJ, and the output grids of `calculate_default_transform` and `WarpedVRT` (reprojections of rotated, south-up and GCP sources included), differ in the last digits (relative 1e-15), and so do statistics (sums in another order); `reproject` with bilinear, cubic and Lanczos kernels differs next to nodata pixels (under 1% of pixels); overviews built with `average` and `mode` differ by 1 on a few pixels (GDAL's rounding and tie-breaking) |
| known differences | reads resampled with bilinear, cubic or average kernels (`out_shape` with `resampling`) differ next to nodata pixels: GDAL 3.12's masked convolution is not the one terrio's kernels implement, and GDAL averages the colors of palette rasters; with a `-inf` nodata, rasterio's boundless masked read masks nothing (terrio.rio masks the pixels outside); GDAL warps a VRT window in its own chunks, so its windowed reads differ from its full reads on 0.01% of pixels (terrio.rio cuts windows from the full warp); GDAL identifies an EPSG code in an ESRI `.prj` where terrio keeps the WKT (the same CRS) |

Run it with:

```sh
pip install rasterio shapely
python scripts/compare_rio_with_rasterio.py ../terrio-data --remote --output report.md
```

## Not covered

- External `.msk` mask files (internal masks are read and written), and mask overviews: an
  overview's mask is the full-resolution mask sampled by nearest neighbour.
- RPC transformers (`TransformMethod.rpcs`, `reproject` with `rpcs`), the `gauss` resampling,
  PROJJSON and WKT2 output: terrio does not implement them. rasterio's `GCPTransformer` with
  `tps=True` gives coordinates a few centimetres from GDAL's own thin plate splines (which
  terrio's match, as `gdalwarp -tps` does).
- Lossy GeoTIFF codecs and YCbCr JPEG: GeoTIFFs compressed with JPEG in YCbCr decode differently
  from GDAL, and LERC, LZMA and JPEG XL are not decoded.
- `r+` rewrites the file (overviews are rebuilt from the updated pixels, where GDAL keeps the old
  ones); large COGs and GeoTIFFs with overviews are written from the whole raster.
- Band-level color tables only on band 1, as terrio's raster model holds them.
- Custom openers (`opener=`) and `rasterio.rio` (the command line).
