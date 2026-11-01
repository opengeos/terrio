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
| `gdaladdo` | `terrio overviews` |
| `gdal_contour` | `terrio contour` |
| `gdal_fillnodata.py` | `terrio fill-nodata` |
| `gdal_grid` | `terrio vector-grid` |
| `gdal_viewshed` | `terrio viewshed` |
| `gdallocationinfo` | `terrio pixel-info` |
| `gdal2xyz.py` | `terrio xyz` |
| `nearblack` | `terrio nearblack` |

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

- terrio reads most raster formats whole into memory. GeoTIFF/COG and JPEG 2000 are read by
  window, Zarr by chunk, and large vector layers and point clouds as streams. See
  [Formats](formats.md).
- Downsampling with `translate` reads full resolution, where GDAL may use overviews. `warp` chooses
  an overview as GDAL does (`--overview AUTO`, the default).
- `warp` and `terrio.rio` handle rotated geotransforms. `translate` refuses them.
- GeoTIFFs are written with terrio's own writer; COGs pass GDAL's COG validator.
- Only the drivers listed in [Formats](formats.md) exist, unless terrio is built with the
  optional GDAL backend (below).

DXF exports use the native CAD writer when the `cad` feature is enabled, including builds
with `gdal`. Use the Rust `gdal_backend` API explicitly to select GDAL's DXF writer.
See [CAD drawings](formats.md#cad-drawings-dwg-and-dxf) for the native subset.

## The optional GDAL backend

For the long tail of formats (HDF4, NITF, KEA, PCIDSK, SQLite, spreadsheets...),
terrio can hand files to GDAL. The backend is a cargo feature, `gdal`, off by default: it adds
a C dependency, and builds need GDAL 3.4 or later installed where `pkg-config` (or
`GDAL_HOME`) finds it.

```sh
cargo install terrio-cli --features gdal
```

```toml
[dependencies]
terrio = { git = "https://github.com/opengeos/terrio", features = ["gdal"] }
```

DWG/DXF also have [native CAD drivers](formats.md#cad-drawings-dwg-and-dxf). They take
priority by default; use the explicit GDAL backend to select GDAL's DXF behavior.

With the feature enabled:

- `terrio::open`, `open_raster`, `open_vector` and `list_layers` (and every command) fall back to
  GDAL when terrio's own drivers cannot open a path. terrio's drivers keep priority for the
  formats they know; when GDAL cannot open a file either, terrio's error is reported.
- A `GDAL:` prefix forces GDAL, even for formats terrio reads itself: `terrio info GDAL:dem.tif`,
  or `--driver gdal` on `terrio info` and `terrio convert`.
- GDAL's dataset names work as they are: `HDF4_SDS:UNKNOWN:"file.hdf":0`, and with a `GDAL:`
  prefix those terrio reads itself (`GDAL:HDF5:"file.h5"://group/dataset`,
  `GDAL:NETCDF:"file.nc":variable`). A container of subdatasets opens
  its first one and lists the others as `SUBDATASET_N_NAME` metadata, as `gdalinfo` does.
- Remote and zipped paths go to GDAL's virtual file systems (`/vsicurl/`, `/vsis3/`, `/vsizip/`).
- Rasters can be written with any GDAL driver: `terrio convert dem.tif dem.ntf -f NITF`
  (`--co KEY=VALUE` passes GDAL creation options). ERDAS Imagine (`.img`), which terrio reads
  itself, is written through GDAL.
- Vector layers can be written with any GDAL vector driver, as `ogr2ogr -f` writes them: ESRI
  File Geodatabases (`terrio convert roads.gpkg roads.gdb`, GDAL's `OpenFileGDB`), DXF, MapInfo,
  SQLite and SpatiaLite, GML, spreadsheets... (`-f "MapInfo File"`, `--dsco KEY=VALUE` and
  `--lco KEY=VALUE` for GDAL's dataset and layer creation options). Fields, geometry types (Z
  included; a layer of unknown type gets the type its features share, single geometries promoted
  to multi ones as `-nlt PROMOTE_TO_MULTI` does) and CRSs are kept; an existing output is
  replaced.
- An output extension terrio has no driver for goes to the GDAL driver that claims it (`.gdb`,
  `.ntf`...), in `terrio convert` and in `terrio::write_vector` and
  `write_raster_dataset`.
- Bytes in memory: `terrio::memory::open_bytes` (and `open_raster_bytes`, `open_vector_bytes`)
  hands what terrio cannot read to GDAL through its `/vsimem/` file system, zip archives through
  `/vsizip/`; `memory::write_raster_bytes` and `write_vector_bytes` write the formats only GDAL
  has to bytes (several files, such as a File Geodatabase, as a zip archive; GDAL's `.aux.xml`
  sidecar left out). `gdal_backend::open_bytes`, `write_raster_bytes` and `write_vector_bytes`
  take GDAL driver names directly.
- Complex bands (`CInt16`, `CInt32`, `CFloat32`, `CFloat64`) are read and written as complex
  datasets, and GDAL's `GEOLOCATION` metadata (swaths) is kept as the dataset's geolocation
  arrays.

What GDAL opens is converted into terrio's data model, so every operation works on it: rasters
are read into memory with all their bands, the geotransform, the CRS (identified as an EPSG code
the way `gdalsrsinfo -e` does), the first band's nodata value, scale and offset, and the dataset
metadata; vector layers keep their schema (list fields become JSON text), geometries (curves are
linearized) and CRS. Datasets opened this way report the `GDAL` driver.

```rust
# #[cfg(feature = "gdal")] {
let roads = terrio::open_vector("roads.gpkg", &Default::default())?;
// An ESRI File Geodatabase, with a layer creation option.
let lco = [("FEATURE_DATASET".to_string(), "transport".to_string())];
terrio::gdal_backend::write_vector(&roads.layers, "roads.gdb", "OpenFileGDB", &[], &lco)?;
// A DXF drawing, as bytes.
let dxf = terrio::gdal_backend::write_vector_bytes(&roads.layers, "roads.dxf", "DXF", &[], &[])?;
// A NITF file from bytes: GDAL reads it from memory.
let ntf = terrio::memory::open_raster_bytes(std::fs::read("image.ntf")?, "image.ntf")?;
# }
# Ok::<(), Box<dyn std::error::Error>>(())
```

Not covered: rotated geotransforms and GCP-only georeferencing (as for terrio's own drivers),
appending to existing datasets through GDAL (`--append`), and updating GDAL datasets in place.

## Python wheels with GDAL

The Python wheels on PyPI are pure Rust, without the backend. The `gdal` feature of the Python
package builds it in from source (see [Installation](installation.md#with-the-gdal-backend)).
No prebuilt wheels bundle GDAL: they would ship GDAL's whole native stack (PROJ, GEOS, curl,
SQLite, HDF5 and the rest) under terrio's name, which works against terrio's purpose of reading
and writing geospatial data without C dependencies. Where GDAL is needed, build the `gdal`
feature against the GDAL you already have.
