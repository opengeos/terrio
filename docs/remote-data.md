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

An archive named without a member opens its main dataset: the first file, by name, of the first
kind found among vector layers, rasters and point clouds (LAS, LAZ, COPC, PLY, E57), so
`terrio info tile.las.zip` reads the LAS file inside.

## Reading in place

GeoTIFFs and COGs are opened lazily: `terrio info` fetches the header only, and window or overview
reads fetch only the tiles they touch, merging adjacent ranges into one request. PMTiles archives
and Zarr stores are read in place too (below). Other formats are downloaded to a temporary
directory first; a remote shapefile's `.shx`, `.dbf`, `.prj` and `.cpg`
come with it.

```sh
terrio info https://prd-tnm.s3.amazonaws.com/StagedProducts/Elevation/1/TIFF/current/n40w106/USGS_1_n40w106.tif
terrio convert https://.../B04.tif window.tif --srcwin 4096 4096 512 512
terrio convert https://.../B04.tif preview.tif --overview 2
```

PMTiles archives are read in place too: opening fetches the header, root directory and metadata;
reads fetch the leaf directories and tiles they need, by range requests. A raster archive's zoom
levels are its overviews; a vector archive's layers are read from the tiles of one zoom level
(`--oo ZOOM_LEVEL=N`), only those in a spatial filter (`--spat`) when there is one. MBTiles are
SQLite files and are downloaded first.

```sh
# A 30 GB archive of terrain tiles: the header and one tile for info, a few tiles for a window.
terrio info https://r2-public.protomaps.com/protomaps-sample-datasets/terrarium_z9.pmtiles
terrio convert https://r2-public.protomaps.com/protomaps-sample-datasets/terrarium_z9.pmtiles \
  window.tif --srcwin 30000 45000 512 512
terrio convert https://.../tiles.pmtiles world.tif --overview 6
terrio convert --oo ZOOM_LEVEL=5 https://.../zcta.pmtiles zcta_z5.gpkg
```

Zarr stores (v2 and v3, GeoZarr included) are read in place over HTTP(S), S3, Google Cloud
Storage and Azure: opening reads the metadata (all of it in one request when the store has
consolidated metadata, v2's `.zmetadata` or the `consolidated_metadata` of a v3 root group), and a
window reads only the chunks it touches, in parallel. In a sharded v3 array, a window reads each
shard's index, then the inner chunks it needs, nearby ones in one range request. Missing chunks
read as the fill value. A store is recognized by a path component ending in `.zarr`, or named as
GDAL names it: `ZARR:"store"` or `ZARR:"store":/group/array`, the store a URL, an `s3://`, `gs://`
or `az://` object or a `/vsicurl/`, `/vsis3/`, `/vsigs/` or `/vsiaz/` name. A path may also name
an array or a group inside the store (`.../S2A_....zarr/measurements/reflectance/r10m/b02`); the
coordinate arrays and the GeoZarr attributes of the groups above it are found as GDAL finds them.
Without consolidated metadata, a remote group is listed as GDAL lists it on `/vsis3/`,
`/vsigs/` and `/vsiaz/`: by the object store's listing of its "subdirectories" (Amazon S3 and
S3-compatible services, signed when credentials are set; Google Cloud Storage; Azure Blob
Storage, with a SAS token that allows listing). A plain HTTP server cannot be listed: there,
name the array, unless the store holds one named after it, as GDAL writes stores. `terrio info`
on a group lists its arrays of two or more dimensions as GDAL's subdatasets
(`ZARR:"store":/path`), unless it holds a single 2D array.

```sh
# An OME-Zarr image on EBI's S3-compatible store, without consolidated metadata
AWS_S3_ENDPOINT=https://uk1s3.embassy.ebi.ac.uk AWS_NO_SIGN_REQUEST=YES AWS_VIRTUAL_HOSTING=FALSE \
  terrio info s3://idr/zarr/v0.4/idr0062A/6001240.zarr    # subdatasets /0, /1, /2, /labels/0/0...
```

```sh
# Sentinel-2 L2A from the EOPF sample service (Zarr v2): a window fetches 1 chunk of 2048 x 2048.
terrio info "https://data.eodc.eu/collections/EOPF_ZARR/products/cpm_v270/S02MSIL2A/.../S2C_MSIL2A_....zarr/measurements/reflectance/r10m/b02"
terrio translate "https://.../S2C_MSIL2A_....zarr/measurements/reflectance/r10m/b02" b02.tif --srcwin 4900 5100 1000 700
# NOAA HRRR on S3, CMIP6 on Google Cloud Storage (time steps as bands), Daymet on Azure.
AWS_NO_SIGN_REQUEST=YES terrio info 'ZARR:"/vsis3/hrrrzarr/sfc/20240101/20240101_00z_anl.zarr/2m_above_ground/TMP/2m_above_ground/TMP"'
terrio translate 'ZARR:"gs://cmip6/CMIP6/CMIP/NOAA-GFDL/GFDL-CM4/historical/r1i1p1f1/Amon/tas/gr1/v20180701":/tas' tas.tif -b 1980 --srcwin 0 0 288 180
AZURE_STORAGE_SAS_TOKEN=... terrio info 'ZARR:"az://daymeteuwest/daymet-zarr/daily/hi.zarr":/tmax'
```

From Python, `terrio.open_raster(url, lazy=True).read(1, window=(x, y, w, h), overview=0)`; Zarr
stores, local or remote, open lazily with `terrio.open` too.

JPEG 2000 files (`.jp2`, `.j2k`) are read in place too, by window and resolution level: decoding a
window reads the header of each tile part before those it needs, skipping their data, and fetches
the tiles it decodes. A 1024 x 1024 window of a 26 MB Sentinel-2 band fetches 3.4 MB (GDAL: 3.0 MB);
files with TLM markers are not read faster, as the OpenJPEG port does not use them.

```sh
B04=https://storage.googleapis.com/gcp-public-data-sentinel-2/tiles/10/S/EG/S2B_MSIL1C_20240630T185919_N0510_R013_T10SEG_20240630T211017.SAFE/GRANULE/L1C_T10SEG_A038220_20240630T190812/IMG_DATA/T10SEG_20240630T185919_B04.jp2
terrio info $B04                                          # overviews 5490, 2745, 1373, 687
terrio convert $B04 window.tif --srcwin 2000 6000 1024 1024
terrio convert $B04 small.tif --overview 3
```

GRIB files (`.grib2`, `.grb`, and NOAA's names such as `gfs.t00z.pgrb2.0p25.f000`) are read in
place too: opening reads each message's first sections with a small range request, the requests
sent in parallel at the offsets of the file's NOAA `.idx` or ECMWF `.index` sidecar when it has one
(otherwise the messages are found one after another by their lengths first), and reading a band
fetches its message alone. Opening a 1-degree GFS file (696 messages) fetches 3 MB of its 42 MB;
HRRR's surface file (170 messages), 0.7 MB.

```sh
GFS=https://noaa-gfs-bdp-pds.s3.amazonaws.com/gfs.20241022/00/atmos/gfs.t00z.pgrb2.1p00.f000
terrio info $GFS                                         # 696 bands with GDAL's GRIB metadata
terrio translate $GFS q.tif -b 581                       # one message fetched and decoded
```

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

Microsoft Planetary Computer's assets are signed with its SAS tokens, as the `planetary-computer`
Python package signs them, so its searches and items open as they are (`PC_SDK_SUBSCRIPTION_KEY`
is sent as the API key when set; `TERRIO_STAC_SIGN=none` turns signing off):

```sh
PC="https://planetarycomputer.microsoft.com/api/stac/v1"
terrio info "STAC:\"$PC/collections/sentinel-2-l2a/items/S2B_MSIL2A_20240709T174909_R141_T13TDE_20240710T000454\":B04"
terrio convert "STACIT:\"$PC/search?collections=sentinel-2-l2a&bbox=-105.3,39.95,-105.2,40.05&datetime=2024-07-01/2024-07-10\":asset=B04,method=POST" b04.tif --srcwin 5000 5000 512 512
```

`method=POST` sends the search as a `POST` request, as pystac-client does, and `POST` pages are
followed through their `next` links as `GET` pages are.

A STAC item of tiled assets (GDAL's STACTA, see [Formats](formats.md#stac)) opens from its URL
or as `STACTA:"item-url":asset`: the item is fetched, then the headers of the first tile when the
item does not declare its bands, and a window read fetches only the tiles (and, for COG tiles, the
blocks of them) it touches, at full resolution or at an overview's tile matrix:

```sh
terrio info "https://example.com/pyramid/stacta.json"
terrio convert "STACTA:\"https://example.com/pyramid/stacta.json\":bands" w.tif --srcwin 4000 3000 512 512
```

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

## Managing files

`terrio vsi` lists, copies, synchronizes, moves and deletes files on these locations and in
`/vsimem/` and tar archives (`/vsitar/`), as `gdal vsi` does: S3 objects are written with signed
requests (single `PUT`s up to 5 GiB, server-side copies within S3), Azure blobs with a SAS token;
Google Cloud Storage and plain HTTP servers are read only. Write redirects stay on the
same origin (scheme, host and port); a redirect to another origin is refused before
forwarding the body or signing another request. S3 bucket-region discovery still uses
the store’s own endpoint. See [vsi](cli.md#vsi).

```sh
terrio vsi list -lR s3://bucket/prefix
terrio vsi sync -r --strategy ETag local_dir/ s3://bucket/prefix
```
