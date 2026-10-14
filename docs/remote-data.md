# Remote Data

## Locations

| Form | Example |
|---|---|
| Local path | `data/dem.tif` |
| HTTP(S) | `https://example.com/dem.tif` |
| Amazon S3 | `s3://bucket/key.tif` |
| Google Cloud Storage | `gs://bucket/key.tif` (public buckets) |
| Azure Blob Storage | `az://container/blob.tif` (public, or with a SAS token) |
| Zip archive | `roads.zip` (its main dataset), `roads.zip!dir/roads.shp` |
| GDAL prefixes | `/vsicurl/URL`, `/vsis3/bucket/key`, `/vsigs/...`, `/vsiaz/...`, `/vsizip/archive.zip/member` |

Prefixes nest as in GDAL: `/vsizip//vsicurl/https://example.com/data.zip/roads.shp`.

## Reading in place

GeoTIFFs and COGs are opened lazily: `terrio info` fetches the header only, and window or overview
reads fetch only the tiles they touch, merging adjacent ranges into one request. Other formats are
downloaded to a temporary directory first; a remote shapefile's `.shx`, `.dbf`, `.prj` and `.cpg`
come with it.

```sh
terrio info https://prd-tnm.s3.amazonaws.com/StagedProducts/Elevation/1/TIFF/current/n40w106/USGS_1_n40w106.tif
terrio convert https://.../B04.tif window.tif --srcwin 4096 4096 512 512
terrio convert https://.../B04.tif preview.tif --overview 2
```

From Python, `terrio.open_raster(url, lazy=True).read(1, window=(x, y, w, h), overview=0)`.

## STAC

STAC items, item collections, collections and STAC API searches are read from their URLs (see
[Formats](formats.md#stac)). Opening an asset (`STAC:"item-url":asset`) or a mosaic
(`STACIT:"search-url":asset=red`) fetches the STAC JSON (every page of a search, up to 1000 items)
and then only the headers of the COGs, then a window read fetches only the tiles of the items it
touches:

```sh
Q="https://earth-search.aws.element84.com/v1/search?collections=sentinel-2-l2a&bbox=-105.05,40.5,-104.95,40.6&datetime=2023-07-05"
terrio info "STACIT:\"$Q\""                                  # subdatasets: one per asset (and CRS)
terrio info "STACIT:\"$Q\":asset=red"                        # a 20982 x 20982 mosaic of four tiles
terrio convert "STACIT:\"$Q\":asset=red" red.tif --srcwin 9980 9980 512 512
```

The mosaics and windows are those of GDAL's STACIT driver (`gdal_translate -srcwin` on the same
subdataset returns the same pixels).

## Amazon S3

Requests are signed (AWS Signature Version 4) when credentials are found, and unsigned otherwise:

- `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`;
- or a profile in `~/.aws/credentials` and `~/.aws/config` (`AWS_PROFILE`,
  `AWS_SHARED_CREDENTIALS_FILE`, `AWS_CONFIG_FILE`);
- `AWS_NO_SIGN_REQUEST=YES` forces unsigned requests for public buckets;
- `AWS_REGION` sets the region; requests are retried in the bucket's region when S3 reports
  another;
- `AWS_S3_ENDPOINT`, `AWS_HTTPS` and `AWS_VIRTUAL_HOSTING` target S3-compatible services.

## Azure

`AZURE_STORAGE_ACCOUNT` names the account; `AZURE_STORAGE_SAS_TOKEN` authorizes private
containers.

## HTTP settings

The GDAL variable is read when the terrio one is not set.

| Variable | GDAL equivalent | Default |
|---|---|---|
| `TERRIO_HTTP_TIMEOUT` (seconds, per request) | `GDAL_HTTP_TIMEOUT` | none |
| `TERRIO_HTTP_CONNECTTIMEOUT` (seconds) | `GDAL_HTTP_CONNECTTIMEOUT` | 30 |
| `TERRIO_HTTP_MAX_RETRY` | `GDAL_HTTP_MAX_RETRY` | 3 |
| `TERRIO_HTTP_RETRY_DELAY` (seconds, doubled each retry) | `GDAL_HTTP_RETRY_DELAY` | 0.5 |
| `TERRIO_HTTP_BLOCK_SIZE` (bytes) | `CPL_VSIL_CURL_CHUNK_SIZE` | 65536 |
| `TERRIO_HTTP_CACHE_SIZE` (bytes) | `CPL_VSIL_CURL_CACHE_SIZE` | 67108864 |
| `TERRIO_HTTP_USERAGENT` | `GDAL_HTTP_USERAGENT` | `terrio/VERSION` |
| `TERRIO_HTTP_LOG=YES` (log each request) | `CPL_CURL_VERBOSE` | off |

Transient errors (connection failures, HTTP 429, 500, 502, 503, 504) are retried with backoff.
Signatures and SAS tokens are hidden in logs and error messages.
