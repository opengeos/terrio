# Coming from GDAL

terrio's commands map onto GDAL's utilities, with GDAL's option names in kebab case.

| GDAL | terrio |
|---|---|
| `gdalinfo`, `ogrinfo -so` | `terrio info` |
| `gdalinfo -json -stats` | `terrio info --json --stats` |
| `gdal_translate` | `terrio translate`, or `terrio convert` for plain format changes |
| `gdalwarp` | `terrio warp` |
| `gdalbuildvrt` | `terrio mosaic ... out.vrt` |
| `gdal_merge.py` | `terrio mosaic ... out.tif` |
| `ogr2ogr` | `terrio convert` |
| `gdal_rasterize` | `terrio rasterize` |
| `gdal_polygonize.py` | `terrio polygonize` |
| `gdaldem` | `terrio dem` |
| `gdal_calc.py` | `terrio calc` (same arguments) |
| `gdal_proximity.py` | `terrio proximity` |
| `gdal_sieve.py` | `terrio sieve` |
| `gdalsrsinfo`, `gdaltransform` | `terrio crs` |
| `gdalinfo --formats`, `ogrinfo --formats` | `terrio formats` |

## Options

| GDAL | terrio |
|---|---|
| `-of GTiff` / `-f GPKG` | `-f GTiff` / `-f GPKG` |
| `-co COMPRESS=DEFLATE -co TILED=YES -co BLOCKXSIZE=512` | `--compress DEFLATE --tile-size 512` |
| `-co BIGTIFF=YES` | `--bigtiff` |
| `-t_srs`, `-s_srs`, `-a_srs` | `--t-srs`, `--s-srs`, `--a-srs` |
| `-srcwin`, `-projwin`, `-projwin_srs` | `--srcwin`, `--projwin`, `--projwin-srs` |
| `-ot`, `-scale`, `-exponent`, `-outsize`, `-a_nodata`, `-a_ullr` | `--ot`, `--scale`, `--exponent`, `--outsize`, `--a-nodata`, `--a-ullr` |
| `-tr`, `-te`, `-te_srs`, `-ts`, `-tap`, `-r` | `--tr`, `--te`, `--te-srs`, `--ts`, `--tap`, `-r` |
| `-srcnodata`, `-dstnodata`, `-cutline`, `-crop_to_cutline`, `-et` | `--srcnodata`, `--dstnodata`, `--cutline`, `--crop-to-cutline`, `--et` |
| `-where`, `-spat`, `-spat_srs`, `-select`, `-limit`, `-sql` | `--where`, `--spat`, `--spat-srs`, `--select`, `--limit`, `--sql` |
| `-nln`, `-nlt`, `-explodecollections`, `-append`, `-overwrite` | `--nln`, `--nlt`, `--explodecollections`, `--append`, `--overwrite` |
| `-oo`, `-lco` | `--oo`, `--lco` |
| `-burn`, `-a`, `-at`, `-add`, `-init` | `--burn`, `-a`, `--at`, `--add`, `--init` |
| `gdal_polygonize.py -8 -f GPKG in.tif out.gpkg layer field` | `terrio polygonize in.tif out.gpkg --8 --nln layer --field field` |
| `gdaldem hillshade -compute_edges -multidirectional -az -alt -s` | `terrio dem hillshade --compute-edges --multidirectional --az --alt -s` |
| `gdaldem color-relief -alpha -exact_color_entry -nearest_color_entry` | `terrio dem color-relief --alpha --exact-color-entry --nearest-color-entry` |
| `gdal_proximity.py -values -distunits -maxdist -use_input_nodata YES` | `terrio proximity --values --distunits --maxdist --use-input-nodata` |
| `gdal_sieve.py -st 10 -8 -nomask` | `terrio sieve --st 10 -8 --nomask` |

## Paths

GDAL's virtual file system prefixes work as they are: `/vsicurl/`, `/vsis3/`, `/vsigs/`,
`/vsiaz/` and `/vsizip/`, nested or not. `s3://`, `gs://` and `az://` URLs work too. Remote outputs
are not supported.

## Differences to know

- terrio reads datasets into memory, except GeoTIFFs, which can be read window by window. Very
  large non-GeoTIFF inputs need enough memory.
- Downsampling with `warp` or `translate` reads full resolution; GDAL may use overviews.
- Rotated geotransforms are not supported.
- GeoTIFFs are written with terrio's own writer; COGs pass GDAL's COG validator.
- Only the drivers listed in [Formats](formats.md) exist, unless terrio is built with the
  optional GDAL backend (below).

## The optional GDAL backend

For the long tail of formats (HDF4, HDF5, GRIB, NITF, KEA, PCIDSK, SQLite, DXF, spreadsheets...),
terrio can hand files to GDAL. The backend is a cargo feature, `gdal`, off by default: it is
terrio's only C dependency, and builds need GDAL 3.4 or later installed where `pkg-config` (or
`GDAL_HOME`) finds it.

```sh
cargo install terrio-cli --features gdal
```

```toml
[dependencies]
terrio = { git = "https://github.com/opengeos/terrio", features = ["gdal"] }
```

With the feature enabled:

- `terrio::open`, `open_raster`, `open_vector` and `list_layers` (and every command) fall back to
  GDAL when terrio's own drivers cannot open a path. terrio's drivers keep priority for the
  formats they know; when GDAL cannot open a file either, terrio's error is reported.
- A `GDAL:` prefix forces GDAL, even for formats terrio reads itself: `terrio info GDAL:dem.tif`,
  or `--driver gdal` on `terrio info` and `terrio convert`.
- GDAL's dataset names work as they are: `HDF5:"file.h5"://group/dataset`,
  `HDF4_SDS:UNKNOWN:"file.hdf":0`, `NETCDF:"file.nc":variable`. A container of subdatasets opens
  its first one and lists the others as `SUBDATASET_N_NAME` metadata, as `gdalinfo` does.
- Remote and zipped paths go to GDAL's virtual file systems (`/vsicurl/`, `/vsis3/`, `/vsizip/`).
- Rasters can be written with any GDAL driver: `terrio convert dem.tif dem.ntf -f NITF`
  (`--co KEY=VALUE` passes GDAL creation options). ERDAS Imagine (`.img`), which terrio reads
  itself, is written through GDAL.

What GDAL opens is converted into terrio's data model, so every operation works on it: rasters
are read into memory with all their bands, the geotransform, the CRS (identified as an EPSG code
the way `gdalsrsinfo -e` does), the first band's nodata value, scale and offset, and the dataset
metadata; vector layers keep their schema (list fields become JSON text), geometries (curves are
linearized) and CRS. Datasets opened this way report the `GDAL` driver.

Not covered: complex-valued bands, rotated geotransforms and GCP-only georeferencing (as for
terrio's own drivers), writing vector layers through GDAL, and opening bytes in memory
(`terrio::memory`) through GDAL.
