# Formats

Drivers are named as GDAL names them, so `-f GTiff` or `-f "ESRI Shapefile"` mean the same in
terrio. `terrio formats` prints this list.

Builds with the optional `gdal` feature also open every format GDAL reads, and write rasters with
GDAL's drivers (the formats marked * are written that way): see
[the GDAL backend](gdal.md#the-optional-gdal-backend).

## Rasters

| Name | Format | Read | Write | Extensions |
|---|---|---|---|---|
| GTiff | GeoTIFF, BigTIFF | ✓ | ✓ | tif, tiff |
| COG | Cloud Optimized GeoTIFF | ✓ | ✓ | tif, tiff |
| JP2 | JPEG 2000 | ✓ | ✓ | jp2, j2k |
| Zarr | Zarr v2 and v3, GeoZarr (`spatial:`, `proj:`, `multiscales`) | ✓ | ✓ | zarr |
| netCDF | netCDF classic and netCDF-4 (groups included); writes CDF-1/2/5 and netCDF-4 | ✓ | ✓ | nc, nc4, cdf |
| HDF5 | HDF5 datasets, as GDAL's HDF5 driver reads them | ✓ | | h5, hdf5, he5 |
| GRIB | GRIB editions 1 and 2, as GDAL's GRIB driver reads them | ✓ | | grb, grb2, grib, grib2 |
| HFA | ERDAS Imagine (written with the `gdal` feature) | ✓ | ✓* | img |
| GPKG | GeoPackage tiles and gridded coverages | ✓ | ✓ | gpkg |
| PMTiles | PMTiles v3 raster tiles (PNG, JPEG, WebP; AVIF read) | ✓ | ✓ | pmtiles |
| MBTiles | MBTiles raster tiles (PNG, JPEG, WebP; AVIF read) | ✓ | ✓ | mbtiles |
| VRT | GDAL Virtual Raster | ✓ | ✓ | vrt |
| GTI | Indexed raster catalogs, local GeoPackage/FlatGeobuf footprints with lazy local/remote tiles | ✓ | | gti, gti.gpkg, gti.fgb, GTI: |
| STACIT | Mosaics of STAC items' assets (`STACIT:"search or items":asset=name`) | ✓ | | |
| STACTA | STAC items with tiled assets (`item.json`, `STACTA:"item.json":asset:tile_matrix_set`) | ✓ | | json |
| ENVI | ENVI labelled raster | ✓ | ✓ | envi, hdr, dat, bsq, bip |
| EHdr | Esri .hdr labelled (BIL, FLT) | ✓ | ✓ | bil, flt |
| AAIGrid | Esri ASCII grid | ✓ | ✓ | asc |
| AIG | Esri binary grid | ✓ | ✓ | adf |
| GRASSASCIIGrid | GRASS ASCII raster | ✓ | ✓ | |
| GSBG | Surfer grid | ✓ | ✓ | grd |
| PCRaster | PCRaster | ✓ | ✓ | map |
| SAGA | SAGA binary grid | ✓ | ✓ | sdat, sgrd |
| RST | Idrisi / TerrSet | ✓ | ✓ | rst, rdc |
| ERS | ER Mapper | ✓ | ✓ | ers |
| PNG | PNG with world file | ✓ | ✓ | png |
| JPEG | JPEG with world file | ✓ | ✓ | jpg, jpeg |
| XYZ | XYZ ASCII grid | ✓ | ✓ | xyz |
| DTED | DTED elevation | ✓ | ✓ | dt0, dt1, dt2 |
| SRTMHGT | SRTM HGT tiles (`N37W120.hgt`, zipped or not; NASADEM `.hgts`, `.err`, `.img`, `.num` and water body `.raw`, `.swb` read) | ✓ | ✓ | hgt |

Native TIFF readers limit each decoded tile or strip to 256 MiB by default. For trusted
files with larger chunks, use `TiffReadLimits` with `LazyRaster::open_tiff_with_limits`
(or its bytes/sparse counterparts), or `terrio::open_raster_with_tiff_limits` for an
eager read. The override applies to parallel decoders too. It limits decoded chunks;
callers still choose the size of the returned raster/window allocation. The limit
also bounds LERC Deflate/Zstandard inflation and the decoded size declared by
embedded LERC and JPEG headers, before mask or pixel decoding. It is not a cap
on total process memory or the codec's working buffers.

GeoTIFF compression: none, LZW, Deflate (including the legacy code), PackBits, JPEG, WebP,
Zstandard, LZMA, LERC (alone, with Deflate or with Zstandard, lossless or not, with NaN masks) and
JPEG XL, with horizontal and floating-point predictors, tiled or striped, chunky or planar. JPEG
(in GeoTIFFs, GeoPackage and tile archive tiles, and `.jpg` files) is decoded exactly as GDAL's
libjpeg decodes it: the same integer IDCT, chroma upsampling and YCbCr conversion, so the pixels are
GDAL's. Lossy JPEG XL can differ from libjxl's by 1 in a few pixels. LERC, JPEG XL and LZMA are
read, not written. Unsigned samples packed in 9 to 15 or 17 to 31 bits (GDAL's
`NBITS`, as GDAL writes Sentinel-2's 15-bit bands) are read as UInt16 or UInt32, as GDAL reads them,
in either bit order (`FillOrder`); packed signed samples and packed samples with a predictor are
not supported (GDAL writes neither).

JPEG 2000 is decoded and encoded with the Rust port of OpenJPEG, the codec of GDAL's JP2OpenJPEG
driver, so pixels are GDAL's (Sentinel-2 L1C and L2A bands included). Georeferencing comes from
the GeoJP2 box or the GMLJP2 box (Sentinel-2's), a 1-bit alpha channel is promoted to 0 and 255 as
GDAL does, and files are written losslessly with a GeoJP2 box, the nodata value in a `.aux.xml`
sidecar. Files are read in place by window and resolution level, as GDAL reads them: the resolution
levels are the overviews (each halving the size, down to about 128 pixels, with GDAL's block
sizes), and a window decodes only the tiles, precincts and code blocks it covers, its tiles in
parallel, from local files, bytes or HTTP range requests. On a 10980 x 10980 Sentinel-2 band, a
1024 x 1024 window takes 0.05 s and 81 MB where decoding the whole band takes 0.5 s (6.3 s on one
thread) and 1.7 GB, and the smallest overview 0.05 s (GDAL: 0.09 s, 1.1 s and 0.33 s); every
window and overview is identical to GDAL's. GeoTIFFs compressed with JPEG XL are read tile by tile
the same way (JPEG XL files themselves are not a format terrio reads).

netCDF variables are opened as `NETCDF:"file.nc":variable`, as in GDAL, and those below the
root group by their path, `NETCDF:"file.nc":/group/variable`. A file of several variables opens
its first and lists them all as subdatasets, as `gdalinfo` does: every variable of two or more
dimensions, in every group (root first, then each group in definition order), named by its full
path and described by its dimensions, standard name and type. A variable's last two dimensions
are its rows and columns and any others its bands, except for a 3-D variable whose last dimension
is named `bands` or `band`, as EMIT's L1B radiance and L2A reflectance,
`(downtrack, crosstrack, bands)`: that dimension is the bands and the first two the rows and
columns, as GDAL reads them. Coordinates are used for the geotransform when GDAL would (named
or marked as spatial axes, evenly spaced as GDAL checks them; longitudes over 180 shifted), and
the CRS from the grid mapping; otherwise rows are read bottom-up, as GDAL reads them. Without a
`_FillValue`, the type's default netCDF fill value is the nodata value, as GDAL reports it (not
for bytes, nor for netCDF-4 variables written without fill values). Variables georeferenced by
longitude and latitude arrays rather than axes get them as geolocation arrays (GDAL's
`GEOLOCATION` metadata, shown by `terrio info`): the CF `coordinates` attribute (relative or
absolute paths, as Sentinel-5P's), 2D `lon` and `lat` variables, NASA's ocean color level 2
(`navigation_data`) and EMIT (`location`, for 2-D variables and those of a trailing `bands`
dimension whose first two dimensions are named as the arrays') layouts, and irregular longitude
and latitude axes. Compound variables of a real and an imaginary member are read as complex bands
(see below).

netCDF files are written as GDAL writes rasters, with its creation options:

| Creation option | Values |
|---|---|
| `FORMAT` | `NC` (CDF-1, the default), `NC2` (CDF-2), `NC5` (CDF-5), `NC4` (netCDF-4) or `NC4C` (netCDF-4, classic model) |
| `COMPRESS` | `NONE` or `DEFLATE` (with the shuffle filter, as GDAL sets it; a classic format becomes `NC4C`, as in GDAL) |
| `ZLEVEL` | The deflate level, 1 (the default) to 9 |
| `CHUNKING` | `YES` (compressed variables chunked by row, GDAL's default) or `NO` (one chunk per band) |
| `GROUP` | A netCDF-4 group for the variables, such as `/science/grid` (terrio's option; groups are created as needed, `FORMAT` defaults to `NC4`) |
| `APPEND_SUBDATASET` | `YES`: add the raster to an existing file (in its `GROUP`), as GDAL's option adds a variable. The file is written again with its rasters and the new one (one raster per group; its other variables and attributes, and its rasters' chunking and compression, are not kept: the creation options of this write apply to all) |

Several rasters go into one netCDF-4 file, each in its group with its own dimensions,
coordinate variables and grid mapping: with `terrio::write_netcdf_groups` in Rust,
`terrio.write_netcdf_groups` in Python, `NetcdfGroups` in JavaScript, or one `terrio convert`
at a time with `GROUP` and `APPEND_SUBDATASET=YES`. They read back as
`NETCDF:"file.nc":/group/Band1`, in terrio, GDAL (classic and multidimensional APIs), netCDF4
and xarray (`xr.open_dataset("file.nc", group="terrain/dem")`).

```sh
terrio convert dem.tif terrain.nc --co GROUP=/terrain/dem --co COMPRESS=DEFLATE
terrio convert slope.tif terrain.nc --co GROUP=/terrain/slope --co APPEND_SUBDATASET=YES
```

```python
terrio.write_netcdf_groups({"/terrain/dem": dem, "/terrain/slope": slope}, "terrain.nc", options={"COMPRESS": "DEFLATE"})
```

netCDF-4 files are HDF5 files written by terrio itself (no HDF5 or netCDF library): the
coordinate variables as dimension scales, the CF grid mapping, `_FillValue` and the other
attributes as GDAL writes them, unsigned and 64-bit types kept (`NC4`). GDAL and the netCDF
library read them as they read GDAL's own (values, georeferencing, CRS, nodata and attributes
are checked against GDAL's output in the conformance suite).

HDF5 files (`.h5`, `.hdf5`, `.he5`, and HDF5 files without a netCDF extension) are read as GDAL's
HDF5 driver reads them: each dataset of two or three dimensions and of a numeric type is a
subdataset, named `HDF5:"file.h5"://group/dataset` and listed in GDAL's order; a file with one
opens it, a file with several opens the first and lists them all, with every group's and
dataset's attributes as metadata (`group_subgroup_attribute=value`). A three-dimensional
dataset's first dimension is its bands; `_FillValue` is its nodata value. Files with a user
block (JPSS products) are read too. The CF `grid_mapping` of a two-dimensional dataset gives its
CRS, and NISAR's coordinates its geotransform, as GDAL 3.13 reads them. Product-specific
georeferencing is read as GDAL's HDF5 driver reads it:

- HDF-EOS5 grids (`HDFEOS INFORMATION/StructMetadata.0`): the geotransform of the grid's corners
  (in packed degrees, minutes and seconds for geographic grids) and the CRS of its GCTP projection
  and sphere code (geographic, UTM, sinusoidal, polar stereographic, Lambert, Albers, transverse
  Mercator and the other GCTP projections GDAL reads; WGS 84 CRSs as EPSG codes), with the
  field's `XDim` and `YDim` as columns and rows and its other dimension, wherever it is, as bands.
  Checked on VIIRS Black Marble (geographic), VIIRS surface reflectance (sinusoidal) and AMSR2 sea
  ice (polar stereographic) products.
- HDF-EOS5 swaths: rows, columns and bands from the dimension maps, and the swath's `Longitude`
  and `Latitude` fields as geolocation arrays (GDAL's `GEOLOCATION` metadata, with the maps'
  offsets and increments); subdatasets described with their dimensions' names, as `gdalinfo`
  lists them (`[(nscans=1980)x(npixs=392)]`). Checked on an AMSR2 precipitation swath.
- COSMO-SkyMed, KOMPSAT-5 and COSMO-SkyMed Second Generation: the UTM (or UPS) projection and
  geotransform of geocoded products (GEC, GTC), WGS 84 for the others (whose corners GDAL gives
  as GCPs, which terrio does not model), and the I and Q bands of single-look complex products.
- ODIM radar composites (DWD's, for instance): the PROJ definition of `where/projdef` and the
  geotransform of its projected corners.

Compound datasets of a real and an imaginary member of the same type (`r`/`i`, as h5py and
NISAR write them) are complex datasets: GDAL's `CInt16`, `CInt32`, `CFloat32` (from 16- and
32-bit floats) and `CFloat64` bands, described `complex, 32-bit floating-point`. terrio holds
each complex band as two bands, its real parts then its imaginary parts (`RasterDataset::complex`),
reports the complex type and GDAL's band count in `terrio info` (statistics of the real parts, as
GDAL computes them), and writes complex GeoTIFFs (`SampleFormat` 5 and 6) that GDAL reads as
`CInt16`... `CFloat64`. Python's `read()` returns complex arrays. Other formats write the parts as
bands. Groups that contain themselves (hard links to an ancestor) are walked once.

GRIB files (editions 1 and 2, local, remote or in memory) are read as GDAL's GRIB driver reads
them, with GDAL's `GRIB_NORMALIZE_UNITS=YES` default and as with `-oo USE_IDX=NO`: a Float64
band per message (per field of GRIB2 messages that repeat their sections, sub-fields reusing a
bitmap included), the size and georeferencing of the first, rasters north-up whatever the scanning
mode, and global grids from 0 to 360 degrees rewrapped to start at -180. Each band has GDAL's
metadata, `GRIB_ELEMENT`, `GRIB_COMMENT`, `GRIB_UNIT`, `GRIB_SHORT_NAME`, `GRIB_REF_TIME`,
`GRIB_VALID_TIME`, `GRIB_FORECAST_SECONDS` and, for GRIB2, `GRIB_DISCIPLINE`, `GRIB_IDS`,
`GRIB_PDS_PDTN`, `GRIB_PDS_TEMPLATE_NUMBERS` and `GRIB_PDS_TEMPLATE_ASSEMBLED_VALUES`
(`BAND_<n>#GRIB_...` items), and GDAL's description (`2[m] HTGL="Specified height level above
ground"`): terrio names fields with GDAL's tables (the WMO's, NCEP's, NDFD's, MRMS's and the
Canadian ones for GRIB2; NCEP's, ECMWF's, DWD's and the others of degrib for GRIB1) and degrib's
rules (probabilities, percentiles, accumulations, ensemble spreads). Kelvin values are converted to
Celsius (`[C]` units) unless `GRIB_NORMALIZE_UNITS=NO` is set in the environment. Points a bitmap
masks are 9999, and the missing values of complex packing its primary missing value, the band's
nodata value, as GDAL gives them. terrio's raster model holds one nodata value: bands without one
(GDAL reports none) are listed as such in the `TERRIO#BAND_<n>_NODATA` items, which `terrio.rio`
reads as its `nodatavals`.

The grids are those GDAL georeferences: GRIB2 regular and rotated latitude/longitude (3.0, 3.1),
Mercator (3.10), transverse Mercator (3.12), polar stereographic (3.20), Lambert conformal and
Albers (3.30, 3.31), regular Gaussian (3.40), GDAL's full-disk geostationary view (3.90) and
Lambert azimuthal equal area (3.140); GRIB1 latitude/longitude, Gaussian, rotated, polar
stereographic, Lambert conformal and Mercator, with GDAL's earth shapes (NCEP's 6371.2 km sphere
for GRIB1). GRIB2 values are decoded as GDAL's copy of NCEP's g2clib decodes them, in single
precision, so that they are GDAL's to the bit: simple packing (5.0), complex packing with and
without spatial differencing (5.2, 5.3), IEEE floats (5.4), JPEG 2000 (5.40, with the OpenJPEG
port of the `jp2` feature), PNG (5.41) and CCSDS (5.42, with the pure-Rust decoder of
[gribberish](https://github.com/mpiannucci/gribberish)); GRIB1 simple packing in double precision,
as degrib does. Like GDAL, terrio does not read GRIB1 complex (second-order) packing, spherical
harmonics, reduced (quasi-regular) Gaussian grids, or the unstructured and other GRIB2 grids
(GDAL reads GRIB1 complex packing as zeros; terrio fails), and the data of a message is decoded when
its band is read, the decoded bands kept up to 100 MB (GDAL's `GRIB_CACHEMAX`). On GDAL's GRIB
test files, ecCodes' and real forecasts (every field of a GFS file, HRRR's and ECMWF's open data),
every band is identical to GDAL's, metadata and pixels (`scripts/compare_grib_with_gdal.py`); a
1-degree GFS file (696 bands) reads in 0.12 s, GDAL's in 1.0 s.

Terrio's GRIB driver limits each field to 16,777,216 points by default, including constant and
compressed fields. This is 128 MiB of decoded Float64 values; decoder buffers and multi-band reads use
additional memory. To allow larger fields, set `TERRIO_GRIB_MAX_POINTS` to a larger positive
integer. The limit is checked when opening the grid and before decoding it, for local, remote
and in-memory files. A limit error does not trigger the optional GDAL fallback.

```sh
terrio info gfs.t00z.pgrb2.0p25.f000                 # bands, GRIB_* metadata
terrio translate hrrr.t00z.wrfsfcf00.grib2 t2m.tif -b 71
GRIB_NORMALIZE_UNITS=NO terrio convert era.grib1 era.tif
```

GeoPackage rasters are written as GDAL writes them: Byte rasters (1 to 4 bands) as PNG tiles, or
JPEG with `--compress JPEG`; one-band Int16, UInt16 and Float32 rasters as gridded coverages
(16-bit PNG or float TIFF tiles), with every zoom level of the pyramid. A GeoPackage holding
vector layers and rasters opens as its layers, and `terrio info` lists its rasters as
subdatasets, opened as `GPKG:file.gpkg:table`. Written layers get an R-tree spatial index.

Zarr stores are read and written as GDAL writes them (`_CRS` and X/Y coordinate arrays) and as
xarray writes them (a CF `grid_mapping` variable with `crs_wkt` and `GeoTransform`). GeoZarr's
conventions are read too: [`spatial:`](https://github.com/zarr-conventions/spatial)
(`spatial:dimensions`, `spatial:transform` with pixel or node registration, `spatial:bbox`),
[`proj:`](https://github.com/zarr-conventions/proj) (`proj:code`, `proj:wkt2`, `proj:projjson`, and
the EOPF samples' `proj:epsg`) on the array, its group or the multiscales group, and
[`multiscales`](https://github.com/zarr-conventions/multiscales) layouts, whose lower-resolution
levels are the raster's overviews (`terrio info` lists them, `--overview N` reads one). A CF
`grid_mapping` variable without `crs_wkt` gives its CRS by its CF parameters, as GDAL reads them,
and coordinate arrays and grid mappings are looked for in the groups above the array too. Stores
are read chunk by chunk, local or remote (see [Remote data](remote-data.md)), with the codecs
none, zlib, gzip, Zstandard, LZ4 and Blosc, transposition, CRC32C checksums, v3 sharding
(`sharding_indexed`, inner chunks read by range), and the `scale_offset` and `cast_value` codecs
of the EOPF GeoZarr samples; `float16` arrays (NOAA's HRRR) are read as Float32. Arrays of more
than three dimensions (`time, level, y, x`) are rasters whose bands are every combination of the
other dimensions, the last varying fastest, as GDAL numbers them, each band's position given as
GDAL's `DIM_<name>_INDEX`, `DIM_<name>_VALUE` and `DIM_<name>_UNIT` band metadata (terrio's
`BAND_<n>_DIM_<name>_...` items); only the chunks of the bands read are fetched
(`terrio translate -b`). The creation
options say how a written store is georeferenced and whether it holds overviews:

| Creation option | Values |
|---|---|
| `FORMAT` | `ZARR_V2` (default), `ZARR_V3` |
| `GEOREFERENCING_CONVENTION` | `GDAL` (default), `SPATIAL_PROJ` (GeoZarr, plus `_CRS` for GDAL 3.12), `CF` (rioxarray's `spatial_ref`) |
| `OVERVIEW_LEVELS` | `2,4,8`..., or `AUTO` (halving until 256 pixels): `ovr_<N>x` groups and a `multiscales` layout, as GDAL lays out Zarr overviews |
| `OVERVIEW_RESAMPLING` | `nearest` (default, sampled as `gdaladdo` samples), or any `terrio translate` method |

```sh
terrio convert dem.tif dem.zarr --co FORMAT=ZARR_V3 --co GEOREFERENCING_CONVENTION=SPATIAL_PROJ --co OVERVIEW_LEVELS=AUTO
terrio convert dem.zarr preview.tif --overview 0
```

## STAC

[STAC](https://stacspec.org) documents, local or remote, open as datasets, as GDAL's STACIT driver
and GeoJSON driver read them:

- An Item, ItemCollection (a STAC API search result), Collection or Catalog is a vector layer of
  item footprints. Fields are the items' properties, their other members (`stac_version`, `links`,
  `stac_extensions`, `collection`) and each asset's members as `assets.<asset>.<member>`, as
  `ogrinfo` reads them. Search results are followed through their `next` links, collections
  through their `item` and `items` links, up to 1000 items.
- `terrio info item.json` lists the item's assets as subdatasets: `STAC:"item.json":asset` (or
  `item.json!asset`) opens one, its href resolved against the item's location. COGs are read in
  place.
- `terrio info` on an item collection or search lists GDAL STACIT's subdatasets: one mosaic per
  collection, asset and CRS. `STACIT:"source":asset=red` (with `collection=` and `crs=` filters,
  and terrio's `resolution=highest|lowest|average` and `max_items=N`) is a mosaic of the asset over
  the items: items drawn in ascending datetime order, at their average resolution, read by window
  from their files.
- Search pages are followed through `next` links that are `GET` or `POST` requests (with the
  link's `body`, `merge` and `headers`, as STAC API paginates `POST` searches), and
  `STACIT:"search-url":asset=red,method=POST` sends the URL's query parameters as a `POST` search
  body, as pystac-client builds it (`bbox`, `collections`, `ids` and `sortby` as arrays, `query`,
  `filter` and `intersects` as JSON, dates and months as RFC 3339 intervals).
- Assets on Microsoft Planetary Computer are signed: the Azure Blob Storage hrefs of items read
  from its API (or linking to it) get a SAS token from its token endpoint, as
  `planetary_computer.sign()` adds them, one per storage account and container, renewed before
  it expires. `PC_SDK_SUBSCRIPTION_KEY` is sent as the API key when set; `TERRIO_STAC_SIGN=none`
  (or `sign=none` in a `STACIT:` name) turns signing off, and `planetary_computer` signs every
  Azure Blob Storage href, of items read from files too.

- An item of the [tiled-assets](https://github.com/stac-extensions/tiled-assets) extension
  (asset templates such as `./{TileMatrixSet}/{TileMatrix}/{TileRow}/{TileCol}.tif` over OGC tile
  matrix sets) opens as one raster, as GDAL's STACTA driver opens it: its last tile matrix,
  over the tiles its `tiles:tile_matrix_links` limits allow, is the full resolution, and the
  others are its overviews (cut to its extent, as `gdal_translate -projwin` cuts them in GDAL's
  driver, when theirs differs). Windows read only the tiles they touch, local or remote. The band
  count, data type and nodata value are the asset template's `raster:bands` (or STAC 1.1
  `bands`), else those of a first tile; the item's properties (but `tiles:` and `proj:`) are its
  metadata. An item of several asset templates or tile matrix sets lists GDAL's subdatasets,
  `STACTA:"item.json":asset` or `STACTA:"item.json":asset:tile_matrix_set`, and opens as the first.
  A tile that cannot be opened is an error unless `GDAL_STACTA_SKIP_MISSING_METATILE=YES`, which
  reads it as nodata (0 without a nodata value). Bands of different `raster:bands` data types
  are read as one type, `Float64`, with the first band's nodata value. `gdal raster tile
  --webviewer stac` writes such items.

```sh
terrio info "https://earth-search.aws.element84.com/v1/search?collections=sentinel-2-l2a&bbox=-105.05,40.5,-104.95,40.6&datetime=2023-07-05"
terrio convert 'STACIT:"https://earth-search.aws.element84.com/v1/search?collections=sentinel-2-l2a&bbox=-105.05,40.5,-104.95,40.6&datetime=2023-07-05":asset=red' red.tif --srcwin 9980 9980 512 512
terrio info 'STAC:"https://earth-search.aws.element84.com/v1/collections/sentinel-2-l2a/items/S2B_13TDE_20230705_0_L2A":red'
# Planetary Computer: a POST search, its pages followed, its assets signed
terrio convert 'STACIT:"https://planetarycomputer.microsoft.com/api/stac/v1/search?collections=sentinel-2-l2a&bbox=-105.3,39.95,-105.2,40.05&datetime=2024-07-01/2024-07-10":asset=B04,method=POST' b04.tif --srcwin 5000 5000 512 512
# Tiled assets (STACTA): a pyramid written by GDAL, read by window and by overview
gdal raster tile --format GTiff --webviewer stac --max-zoom 12 landsat.tif pyramid
terrio info pyramid/stacta.json
terrio convert pyramid/stacta.json window.tif --srcwin 4000 3000 512 512
terrio convert 'STACTA:"https://example.com/pyramid/stacta.json":bands' preview.tif --overview 2
```

## Vectors

| Name | Format | Read | Write | Extensions |
|---|---|---|---|---|
| GPKG | GeoPackage | ✓ | ✓ | gpkg |
| Parquet | GeoParquet | ✓ | ✓ | parquet, geoparquet |
| FlatGeobuf | FlatGeobuf | ✓ | ✓ | fgb |
| ESRI Shapefile | Shapefile | ✓ | ✓ | shp |
| GeoJSON | GeoJSON | ✓ | ✓ | geojson, json |
| GeoJSONSeq | GeoJSON text sequences (a feature per line) | ✓ | ✓ | geojsonl, geojsons, geojsonseq |
| CSV | CSV with X/Y or WKT columns | ✓ | ✓ | csv, tsv |
| KML | KML | ✓ | ✓ | kml |
| KMZ | Zipped KML | ✓ | ✓ | kmz |
| GML | GML | ✓ | ✓ | gml |
| GPX | GPS Exchange Format | ✓ | ✓ | gpx |
| DXF | AutoCAD DXF (ASCII and binary read; ASCII write) | ✓ | ✓ | dxf |
| DWG | AutoCAD DWG | ✓ | ✓ | dwg |
| TopoJSON | TopoJSON | ✓ | ✓ | topojson |
| MapInfo File | MapInfo TAB and interchange format | ✓ | ✓ | tab, mif |
| OSM | OpenStreetMap PBF and XML | ✓ | | osm.pbf, osm |
| PMTiles | PMTiles v3 vector tiles (MVT) | ✓ | ✓ | pmtiles |
| MBTiles | MBTiles vector tiles (MVT) | ✓ | ✓ | mbtiles |
| MVT | Mapbox Vector Tiles: a tile, or a directory of tiles | ✓ | ✓ | mvt, pbf |

Layer and field models follow GDAL's: GPX has GDAL's five layers, KML has the LIBKML driver's
fields, OSM has the `points`, `lines`, `multilinestrings`, `multipolygons` and `other_relations`
layers, and shapefile field names are truncated and made unique as `ogr2ogr` does it. TopoJSON
files are written with a `crs` member naming their CRS, which GDAL reads (it assumes none
otherwise), and a CSV file's CRS is read from a `.prj` file next to it, as GDAL's CSV driver
reads it.

- **GeoPackage** layers are read with spatial filters through their R-tree index (`terrio convert
  --spat`, `VectorOpenOptions::spatial_filter`, Python's `open_vector(bbox=...)`): the
  candidates the index finds are read by feature id, then tested against the rectangle, giving the
  features `ogr2ogr -spat` gives. Existing files are updated as `ogr2ogr` updates them:
  `--append` adds features to a layer (fields matched by name, new feature ids continuing the
  layer's sequence, its R-tree, `gpkg_contents` extent and `gpkg_ogr_contents` count updated),
  `--overwrite` replaces a layer, and layers are added to files holding other layers or rasters.
  Everything else in the file (tables, rasters, metadata, extensions, triggers, views) is kept.
  Files are changed in place, as SQLite changes them: rows are added to the table's b-tree
  (pages split as SQLite splits them), boxes inserted into the R-tree (nodes split as SQLite's
  R*-tree splits them), dropped layers' pages put on the freelist and reused, and only the pages
  that change are written, in a transaction protected by a rollback journal (`<file>-journal`,
  played back by SQLite, GDAL and terrio after a crash) and under SQLite's file locks. Appending
  1,000 points to a 2-million-point file takes about as long as `ogr2ogr -append`. Files in
  write-ahead-log mode with a pending log, with auto-vacuum, or with triggers on the layer other
  than GDAL's are rewritten whole instead.
- **Streaming**: GeoPackage, FlatGeobuf, GeoParquet, Shapefile, GeoJSON, GeoJSONSeq and CSV
  layers are read a chunk of features at a time (GeoPackages a leaf page of the table at a time,
  through the R-tree with `--spat`; GeoParquet a record batch at a time), and FlatGeobuf,
  GeoParquet, Shapefile, GeoJSON, GeoJSONSeq and CSV are written that way, so `terrio convert`
  between them works in bounded memory, whatever the size of the layer (see
  [convert](cli.md#convert)). GeoJSON and CSV inputs are read twice, the first time to infer the
  fields, as GDAL does; CSV, Shapefile and GeoParquet outputs need the field widths, integer
  ranges and geometry types of all the features before the first is written, so the input is read
  twice for them too (GeoParquet only with integer, date or date-time fields). FlatGeobuf outputs
  read the input once: their features are spooled to a temporary file next to the output, with
  the types of the features seen so far (re-encoded if a later feature widens them), then written
  in the order of the spatial index. A streamed file is identical to the one written from the
  whole layer.
- **GeoJSONSeq** files hold a GeoJSON feature per line (RFC 8142, optionally with record
  separators), written in WGS 84 with counter-clockwise exterior rings, as GDAL's GeoJSONSeq
  driver writes them.
- **GeoParquet** is written as GeoParquet 1.1: the CRS as PROJJSON (none for EPSG:4326, which
  the specification makes the default), a `geometry_bbox` covering column, and row groups of
  65536 rows with statistics. Reading with a bounding box (`terrio convert --spat` on a Parquet
  input, or `VectorOpenOptions::spatial_filter`) skips the row groups and rows whose covering
  bounding box is outside it.
- **MapInfo TAB** native tables (`.tab` with its `.dat`, `.map` and `.id`) are read with every
  object type and field type, in their charset, and seamless tables; they are written with their
  spatial index, which GDAL uses for spatial filters. Views and DBF-backed tables are not read.
- **OSM XML** (`.osm`) gives the same five layers as PBF, with tags in the file's order.
- **KML styles** (`<Style>`, `<StyleMap>`) are read into an `OGR_STYLE` field holding GDAL's style
  strings (`PEN(c:#FF0000FF,w:2.000000px);BRUSH(fc:#00FF0040)`), and written back as KML styles.

### CAD drawings (DWG and DXF)

**The native reader is strict:** an unsupported geometric entity fails the entire drawing.
The optional GDAL backend can provide fallback for entities outside the native subset below.

The default-enabled `cad` Cargo feature uses [acadrust](https://github.com/hakanaktt/acadrust)
without native libraries or its optional thread pool. It is included in the full JavaScript/WASM
module; the lighter `terrio-js/core` module leaves it out. File, remote/downloaded and byte inputs
use the same codecs. CLI/MCP `info` and `convert`, Rust, Python and JavaScript use the existing
vector APIs:

```sh
terrio info drawing.dwg
terrio convert drawing.dwg entities.geojson
terrio convert parcels.gpkg drawing.dxf
terrio convert drawing.dxf drawing.dwg
```

Input versions are those supported by acadrust 0.6.7 (R12 through R2018+); Terrio writes R2018
DWG and ASCII DXF. Each drawing becomes one `entities` layer, including model and paper space.
Fields are `Layer`, `EntityHandle`, `EntityType`, `Linetype`, `Text` and `PaperSpace` (1 for paper
space, null otherwise). Coordinates stay in drawing units, including Z and object-coordinate
system transformations. CAD coordinates are exposed as XYZ, including zero elevations. No CRS
is inferred or assigned. Assign a known source CRS explicitly before reprojection. CAD files are decoded in memory; the expansion budget does not limit the
upstream codec's input file size. Layer filters select `entities`; attribute filters such as
`--where "Layer = 'survey'"` select drawing layers.

Supported entities are POINT, LINE, straight and bulged LWPOLYLINE/POLYLINE, 3D polylines,
CIRCLE, ARC, ELLIPSE, TEXT/MTEXT, SOLID/TRACE, 3DFACE and HATCH with polyline, line or circular-arc
boundaries. INSERT expands block geometry (up to 32 levels, one million output features and a shared
work budget of one million entity visits/clones, including skipped leaves and generated segments);
unused block definitions are excluded. Empty ordinary block references are skipped; missing
blocks, array inserts and external/non-explodable references produce errors. Expanded entities
receive new handles. Viewports and
sequence markers carry no exported geometry.
Closed polylines remain lines. Curves are linearized at a maximum four-degree step; curve samples
can differ from GDAL's. Hatch rings are organized by containment into polygons with holes. Edge boundaries must be
listed in connected sequence; individual edges and the first edge may be reversed, but arbitrary
edge reordering is unsupported. Endpoint joins use an absolute tolerance of 1e-7 drawing units.
Unsupported entities (including splines, dimensions, ACIS solids and mesh entities), unsupported
hatch boundaries, fitted polylines and extruded/thick geometry produce an error identifying the
unsupported data rather than silently discarding it. The optional GDAL backend can read entities
outside the native subset through its existing fallback; there is no skip-and-discard mode. Text is returned as raw CAD text; font styles
and MTEXT formatting are not interpreted. Styling, external references and CAD object dictionaries
are outside Terrio's vector model.

On export, points become POINT (or TEXT when the `Text` attribute is nonempty), lines become LINE
or 3D POLYLINE, and horizontal polygons become HATCH with their holes. Non-horizontal polygons
and measured coordinates are rejected. An explicit point `EntityType` of `POINT`, `TEXT` or
`MTEXT` overrides that default and preserves text entity kinds; an explicitly requested TEXT/MTEXT
can have an empty value. Multi-geometries/collections become separate entities, which can increase
feature counts. The same text/type attributes apply to every point member of a MultiPoint or
GeometryCollection.

Several input layers can share a drawing: the `Layer` attribute names the CAD layer, falling back
to the input layer name. Forbidden characters in `Layer`/`Linetype` names are rejected with a
field-specific error rather than silently renamed. `Text`, `Linetype` and `PaperSpace` are mapped
back; other GIS fields,
feature IDs, CRS and CAD-specific objects/styles are not preserved. Handles are newly allocated.
This is vector geometry interchange, not lossless CAD document editing. The optional STL,
COLLADA, OBJ, glTF/GLB and FBX importers in acadrust are not enabled by Terrio's `cad` feature.

## Tile archives

PMTiles (v3) and MBTiles archives hold raster tiles or Mapbox Vector Tiles (MVT) in the Web
Mercator tiling scheme (EPSG:3857). They open as GDAL's PMTiles, MBTiles and MVT drivers open
them:

- **Raster tiles** (PNG, JPEG, WebP, AVIF) open as a raster in EPSG:3857 at the resolution of the
  highest zoom level, with the lower zoom levels as overviews, read by window (only the tiles a
  window needs are fetched and decoded). MBTiles extents come from the `bounds` metadata, and
  MBTiles open as 4 bands (RGBA), as in GDAL's MBTiles driver; PMTiles extents are the header's
  bounds aligned on the pixel grid, with the bands of their tiles, as in GDAL's PMTiles driver.
- **Vector tiles** open as one layer per MVT layer the metadata declares (`vector_layers`), with
  every feature of the tiles of the highest zoom level, its geometry in EPSG:3857 clipped to its
  tile, an `mvt_id` field and the declared fields, typed with the tile statistics (`tilestats`)
  as GDAL types them. A single tile (`.mvt`, `.pbf`) is georeferenced when its path ends in
  `z/x/y.pbf`; a directory of `z/x/y.pbf` tiles with a `metadata.json` (as `ogr2ogr -f MVT`
  writes) opens like an archive.

AVIF tiles are decoded in pure Rust (WebAssembly included): the HEIF container by terrio, the
AV1 bitstream by `rusty_av1d` (a port of dav1d without its assembly), and YUV converted to RGB as
libavif's built-in converter does (bilinear chroma upsampling), so that the pixels are those GDAL's
AVIF driver reads (identical on 4:2:0, 4:2:2, 4:4:4, lossless, gray and alpha images). 8-bit
images only; grid images (large AVIF made of several tiles) are not read. GDAL's MBTiles driver
does not read AVIF tiles; terrio reads them in MBTiles and PMTiles archives.

Open options (`terrio convert --oo`, `VectorOpenOptions::tiles`): `ZOOM_LEVEL` (the zoom level
read; lower ones are overviews of rasters) and `CLIP=NO` (keep features whole, buffer included).

Rasters are written as GDAL's MBTiles driver writes them: reprojected to EPSG:3857 (bilinear) at
the zoom level whose resolution is closest to the source's (`ZOOM_LEVEL_STRATEGY=AUTO`, `LOWER`
or `UPPER`), with an alpha band when the source has one or is not in EPSG:3857, and placed on the
tile grid by whole pixels. Unlike `gdal_translate -of MBTiles`, the lower zoom levels are written
too, as `gdaladdo` would add them (and as GDAL's PMTiles driver writes them): down to the level
whose single tile covers the raster, or `--minzoom`. Creation options (`--co`): `TILE_FORMAT`
(`PNG`, `JPEG`, `WEBP` lossless), `QUALITY`, `RESAMPLING`, `ZOOM_LEVEL_STRATEGY`, `MINZOOM`,
`MAXZOOM`, `NAME`, `DESCRIPTION`, `TYPE`. Tile archives hold 1 to 4 Byte bands: convert other
data types first (`terrio translate --ot Byte --scale`).

Vector layers are written as GDAL's MVT writer writes them: reprojected to EPSG:3857, cut into
the tiles of zoom levels 0 to 5 (`--minzoom`, `--maxzoom`), each feature clipped to every tile it
touches with a buffer of 80 units (of 4096), quantized to the tile grid, polygons wound as the
specification asks, tiles over 500 kB encoded at lower resolutions then reduced to their largest
features. Invalid polygons are handled as GDAL handles them with GEOS: a polygon that is invalid
once quantized (rounding made its rings cross or touch, its parts overlap, or the source was
invalid) is replaced by GEOS's `MakeValid` of it (the "linework" method: the rings noded, faces
nested an even number of times kept), and an invalid polygon (crossing rings, overlapping parts)
is left out of the tiles where GEOS fails to clip it, as GDAL leaves it out. Layer creation options (`--lco`): `MINZOOM`, `MAXZOOM`, `EXTENT`, `BUFFER`,
`SIMPLIFICATION`, `SIMPLIFICATION_MAX_ZOOM`, `MAX_SIZE`, `MAX_FEATURES`, `NAME`, `DESCRIPTION`,
`TYPE`. The metadata holds `vector_layers` and `tilestats`, so that GDAL and MapLibre read the
fields and their types. `-f MVT` writes a directory of tiles with a `metadata.json`.

Existing archives are updated (`terrio convert --append`, `--overwrite`;
`tiles::update_raster`, `tiles::update_vector`; `write_vector(..., append=True)` in Python):

- **Rasters** are added as `gdalwarp` into an existing MBTiles file followed by `gdaladdo` add
  them: warped onto the pixel grid of the archive's highest zoom level and laid over its tiles
  where they are not transparent, the extent grown to hold them (GDAL keeps the extent), and the
  lower zoom levels computed again. Tiles keep their format (PNG, JPEG or WebP) and the archive
  its metadata.
- **Vector layers** are cut into the archive's zoom levels and tile extent; their features join
  the tiles' layers of the same name (`--append`) or replace those layers in every tile
  (`--overwrite`), other layers stay, and `vector_layers`, `tilestats`, bounds and zoom levels are
  merged. GDAL cannot update vector tiles: the result reads as the layers written at once.

Archives are rewritten whole and replace the original once complete (PMTiles cannot be updated
in place; MBTiles are written by terrio's SQLite writer).

Checked against GDAL 3.12 (`scripts/compare_tiles_with_gdal.py`): rasters written as MBTiles
have GDAL's size, geotransform, zoom levels and tiles, with identical pixels at full resolution
and overviews within one grey level for 99.8% to 100% of the pixels (GDAL's overview filter
rounds a little differently); vector tiles written by GDAL (and by Tippecanoe) read as GDAL reads
them, feature for feature; vector tiles written by terrio read in GDAL with GDAL's feature
counts and areas (and so do archives updated with `--append`: rasters identical at full
resolution to `gdalwarp` into GDAL's MBTiles, vector tiles to the layers written at once), invalid
polygons included: Natural Earth's countries (Sudan is invalid) at zoom
levels 0 to 6 and US counties at zoom levels 0 to 8 give GDAL's features and areas tile by tile
(all but one county at one zoom level, whose `MakeValid` output starts its rings elsewhere). GDAL 3.12 reads PMTiles vector tiles only: raster PMTiles follow GDAL's
development version.

## Arrow and GeoArrow

Vector layers convert to and from Apache Arrow record batches (`terrio::arrow`), with a
[GeoArrow](https://geoarrow.org) geometry column: WKB (`geoarrow.wkb`), or the native encodings
(`geoarrow.point`, `geoarrow.linestring`, `geoarrow.polygon` and their multi-part forms, with
separated or interleaved coordinates), the CRS as PROJJSON in the column's metadata. Python layers
take the Arrow PyCapsule interface (`Layer.to_arrow()`, `terrio.from_arrow()`); JavaScript and
Rust read and write Arrow IPC streams.

## Point clouds

| Name | Format | Read | Write | Extensions |
|---|---|---|---|---|
| LAS | ASPRS LAS 1.0 to 1.4, point formats 0 to 10 | ✓ | ✓ | las |
| LAZ | LASzip-compressed LAS | ✓ | ✓ | laz |
| COPC | Cloud Optimized Point Cloud | ✓ | ✓ | copc.laz |

See [Point Clouds](point-clouds.md).

## In WebAssembly

The browser and Node.js package reads from bytes. GRIB files are read there too, band by band
(the full module). JPEG 2000 is read and written there with the same
codec as natively (written with a nodata value, it comes back as a zip archive of the JP2 file and
its `.aux.xml` sidecar), and GeoTIFFs in every compression above are read. Formats made
of several files (Shapefile, MapInfo MIF and TAB) are read from zip archives of their files;
OSM is read from PBF or XML bytes. GeoPackage (layers and rasters), PMTiles and MBTiles (raster
and vector tiles) are read from and written to bytes; MVT tiles are read from bytes.

## Shapefile ZIP output in memory

Rust, Python and JavaScript/WASM use the same pure-Rust archive writer, including
builds with default features disabled. Request **`places.shp.zip`**, save that
filename, and use MIME type **`application/zip`**. The ZIP contains `places.shp`,
`places.shx`, `places.dbf`, `places.cpg` (`UTF-8`) and, when a CRS is present,
`places.prj`. Members can be obtained with any ZIP reader, without a filesystem.
A `.zip` name also works with an explicit Shapefile driver. The output holds
exactly one layer; select a layer before writing a multi-layer dataset.

```rust
let bytes = terrio::memory::write_vector_bytes(&layers, "places.shp.zip", &Default::default())?;
// Or write a single layer directly, without format inference:
let bytes = terrio::memory::write_shapefile_zip_bytes(&layers[0], "places")?;
```

```js
const filename = "places.shp.zip";
const bytes = dataset.toBytes(filename);
const download = new File([bytes], filename, { type: "application/zip" });
const again = terrio.open(bytes, filename);
```

```python
archive = layer.to_bytes("places.shp.zip")
again = terrio.open_bytes(archive, "places.shp.zip")
```

This enables filesystem-free Shapefile output on `wasm32-unknown-unknown` in both
the full and core JS modules, and in minimal Rust builds. Native builds produce
the same members. The bytes API now refuses bare `.shp` names with an error
pointing to `.shp.zip`, replacing native-only implicit ZIP output under a `.shp`
name. Filesystem writing to `.shp` continues to write separate members.

Supported geometries are Point/MultiPoint, lines and polygons (including
multipart features and polygon holes), XY or Z, plus null geometry. Collections,
mixed geometry families and M coordinates are refused by the archive writer.
Polygon rings are closed and oriented to Shapefile conventions. Shapefile stores
single-part MultiPolygon/Polygon (and MultiLineString/LineString) identically;
a single-part multi geometry can read back as its single form. Feature IDs are
record numbers, so keep stable application IDs in scalar attributes.

DBF supports text, integers, finite reals, booleans, dates and nulls. Names are
laundered to ten bytes with unique suffixes. Text widths grow to fit values up to
254 **UTF-8 bytes**. Longer values, including long URLs, are refused without
truncation: keep those values in a caller-owned sidecar keyed by a stable ID or
choose a format without DBF limits. Empty, trailing-space and NUL text are also
refused because DBF cannot distinguish them reliably from null/padding. Integers
requiring more than 18 decimal columns must be converted explicitly to text to
avoid being read as floating point. Reals use width 24 with 15 decimals and can
round; magnitudes below half of 1e-15 become zero. The archive writer refuses
values that overflow that fixed layout instead of reducing decimals or switching
to scientific notation. Convert those values to text. This is not
arbitrary-precision storage.
Blob, JSON/list and datetime fields require explicit conversion. Schema/value
mismatches and nonfinite real values are errors. These stricter archive policies
are shared by native and WASM; filesystem-member writing keeps its prior attribute conversion policy.
