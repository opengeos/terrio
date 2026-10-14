# Formats

Drivers are named as GDAL names them, so `-f GTiff` or `-f "ESRI Shapefile"` mean the same in
terrio. `terrio formats` prints this list.

## Rasters

| Name | Format | Read | Write | Extensions |
|---|---|---|---|---|
| GTiff | GeoTIFF, BigTIFF | ✓ | ✓ | tif, tiff |
| COG | Cloud Optimized GeoTIFF | ✓ | ✓ | tif, tiff |
| JP2 | JPEG 2000 | ✓ | ✓ | jp2, j2k |
| Zarr | Zarr v2 and v3 | ✓ | ✓ | zarr |
| netCDF | netCDF classic and netCDF-4 (writes classic CDF-1/2/5) | ✓ | ✓ | nc, nc4, cdf |
| HFA | ERDAS Imagine | ✓ | | img |
| GPKG | GeoPackage tiles and gridded coverages | ✓ | ✓ | gpkg |
| VRT | GDAL Virtual Raster | ✓ | ✓ | vrt |
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

Layer and field models follow GDAL's: GPX has GDAL's five layers, KML has the LIBKML driver's
fields, OSM has the `points`, `lines`, `multilinestrings`, `multipolygons` and `other_relations`
layers, and shapefile field names are truncated as GDAL truncates them.

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
OSM is read from PBF or XML bytes. GeoPackage (layers and rasters) is read from and written to
bytes.
