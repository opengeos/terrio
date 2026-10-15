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
| netCDF | netCDF classic and netCDF-4 (writes classic CDF-1/2/5) | ✓ | ✓ | nc, nc4, cdf |
| HFA | ERDAS Imagine (written with the `gdal` feature) | ✓ | ✓* | img |
| GPKG | GeoPackage tiles and gridded coverages | ✓ | ✓ | gpkg |
| PMTiles | PMTiles v3 raster tiles (PNG, JPEG, WebP) | ✓ | ✓ | pmtiles |
| MBTiles | MBTiles raster tiles (PNG, JPEG, WebP) | ✓ | ✓ | mbtiles |
| VRT | GDAL Virtual Raster | ✓ | ✓ | vrt |
| STACIT | Mosaics of STAC items' assets (`STACIT:"search or items":asset=name`) | ✓ | | |
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

GeoTIFF compression: none, LZW, Deflate (including the legacy code), PackBits, JPEG, WebP and
Zstandard, with horizontal and floating-point predictors, tiled or striped, chunky or planar.
netCDF variables are opened as `NETCDF:"file.nc":variable`, as in GDAL.

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
levels are the raster's overviews (`terrio info` lists them, `--overview N` reads one). The
creation options say how a written store is georeferenced and whether it holds overviews:

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

```sh
terrio info "https://earth-search.aws.element84.com/v1/search?collections=sentinel-2-l2a&bbox=-105.05,40.5,-104.95,40.6&datetime=2023-07-05"
terrio convert 'STACIT:"https://earth-search.aws.element84.com/v1/search?collections=sentinel-2-l2a&bbox=-105.05,40.5,-104.95,40.6&datetime=2023-07-05":asset=red' red.tif --srcwin 9980 9980 512 512
terrio info 'STAC:"https://earth-search.aws.element84.com/v1/collections/sentinel-2-l2a/items/S2B_13TDE_20230705_0_L2A":red'
```

## Vectors

| Name | Format | Read | Write | Extensions |
|---|---|---|---|---|
| GPKG | GeoPackage | ✓ | ✓ | gpkg |
| Parquet | GeoParquet | ✓ | ✓ | parquet, geoparquet |
| FlatGeobuf | FlatGeobuf | ✓ | ✓ | fgb |
| ESRI Shapefile | Shapefile | ✓ | ✓ | shp |
| GeoJSON | GeoJSON | ✓ | ✓ | geojson, json |
| CSV | CSV with X/Y or WKT columns | ✓ | ✓ | csv, tsv |
| KML | KML | ✓ | ✓ | kml |
| KMZ | Zipped KML | ✓ | ✓ | kmz |
| GML | GML | ✓ | ✓ | gml |
| GPX | GPS Exchange Format | ✓ | ✓ | gpx |
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
  Everything else in the file (tables, rasters, metadata, extensions, triggers, views) is kept;
  the file is written anew and replaces the original.
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

## Tile archives

PMTiles (v3) and MBTiles archives hold raster tiles or Mapbox Vector Tiles (MVT) in the Web
Mercator tiling scheme (EPSG:3857). They open as GDAL's PMTiles, MBTiles and MVT drivers open
them:

- **Raster tiles** (PNG, JPEG, WebP) open as a raster in EPSG:3857 at the resolution of the
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
features. Layer creation options (`--lco`): `MINZOOM`, `MAXZOOM`, `EXTENT`, `BUFFER`,
`SIMPLIFICATION`, `SIMPLIFICATION_MAX_ZOOM`, `MAX_SIZE`, `MAX_FEATURES`, `NAME`, `DESCRIPTION`,
`TYPE`. The metadata holds `vector_layers` and `tilestats`, so that GDAL and MapLibre read the
fields and their types. `-f MVT` writes a directory of tiles with a `metadata.json`.

Checked against GDAL 3.12 (`scripts/compare_tiles_with_gdal.py`): rasters written as MBTiles
have GDAL's size, geotransform, zoom levels and tiles, with identical pixels at full resolution
and overviews within one grey level for 99.8% to 100% of the pixels (GDAL's overview filter
rounds a little differently); vector tiles written by GDAL (and by Tippecanoe) read as GDAL reads
them, feature for feature; vector tiles written by terrio read in GDAL with GDAL's feature
counts and areas. GDAL 3.12 reads PMTiles vector tiles only: raster PMTiles follow GDAL's
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

The browser and Node.js package reads from bytes. JPEG 2000 is not available there. Formats made
of several files (Shapefile, MapInfo MIF and TAB) are read from zip archives of their files;
OSM is read from PBF or XML bytes. GeoPackage (layers and rasters), PMTiles and MBTiles (raster
and vector tiles) are read from and written to bytes; MVT tiles are read from bytes.
