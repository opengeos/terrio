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
- Only the drivers listed in [Formats](formats.md) exist; there is no plugin mechanism.
