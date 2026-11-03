# Getting Started

This walk-through uses public data: Natural Earth countries and a USGS 3DEP elevation
tile. [Install the `terrio` command](installation.md) first, or use `uvx terrio` in its place with
[uv](https://docs.astral.sh/uv/). Run the steps in order, in an empty directory: later steps use
the files earlier ones write.

## Look at a dataset

```sh
terrio info https://naciscdn.org/naturalearth/110m/cultural/ne_110m_admin_0_countries.zip
```

```text
Driver: ESRI Shapefile

Layer: ne_110m_admin_0_countries
Geometry: Polygon
Features: 177
CRS: EPSG:4326 (WGS 84)
...
```

terrio downloaded the archive, found the shapefile inside, and identified its `.prj` as EPSG:4326.
Add `--json` for machine-readable output, and `--stats` for band statistics on rasters.

A Cloud Optimized GeoTIFF is read in place: only its header is fetched.

```sh
terrio info https://prd-tnm.s3.amazonaws.com/StagedProducts/Elevation/1/TIFF/current/n40w106/USGS_1_n40w106.tif
```

## Convert between formats

The output format comes from the extension, or from `-f`:

```sh
terrio convert https://naciscdn.org/naturalearth/110m/cultural/ne_110m_admin_0_countries.zip countries.gpkg
terrio convert countries.gpkg countries.fgb
terrio convert countries.gpkg europe.parquet --where "CONTINENT = 'Europe'" --select NAME,POP_EST
terrio convert countries.gpkg countries_3857.geojson --t-srs EPSG:3857
```

Rasters convert the same way:

```sh
terrio convert https://prd-tnm.s3.amazonaws.com/StagedProducts/Elevation/1/TIFF/current/n40w106/USGS_1_n40w106.tif dem.tif -f COG --compress DEFLATE
```

## Process rasters

```sh
# A window and a type change
terrio translate dem.tif subset.tif --srcwin 1000 1000 512 512 --ot Int16

# Reproject to NAD83 / UTM zone 13N at 30 m with bilinear resampling
terrio warp dem.tif dem_utm.tif --t-srs EPSG:26913 --tr 30 30 -r bilinear

# Shaded relief; -s converts degrees to metres for a DEM in geographic coordinates
terrio dem hillshade subset.tif shade.tif -s 111120

# Mosaic overlapping or adjacent rasters into a virtual raster, then a GeoTIFF
terrio translate dem.tif east.tif --srcwin 1512 1000 512 512 --ot Int16
terrio mosaic subset.tif east.tif mosaic.vrt
terrio mosaic subset.tif east.tif mosaic.tif
```

## Between rasters and vectors

```sh
# Burn country populations into a 0.1° grid
terrio rasterize countries.gpkg population.tif -a POP_EST --tr 0.1 0.1 --ot Float64

# Turn the raster back into polygons, one per run of equal values
terrio polygonize population.tif population.gpkg --field POP_EST
```

## Vector operations

```sh
# Repair invalid geometries, which topology operations refuse, then buffer in the CRS's units
terrio make-valid countries_3857.geojson valid.gpkg
terrio buffer valid.gpkg buffers.gpkg --distance 50000

terrio dissolve countries.gpkg continents.gpkg --field CONTINENT
# SQL names the layer, which keeps the shapefile's name
terrio sql countries.gpkg largest.csv --query "SELECT NAME, POP_EST FROM ne_110m_admin_0_countries ORDER BY POP_EST DESC LIMIT 5"
```

## Point clouds

A [COPC](point-clouds.md) file is read in place too, so a small part of a large survey downloads
quickly:

```sh
terrio info https://s3.amazonaws.com/hobu-lidar/autzen-classified.copc.laz
terrio convert https://s3.amazonaws.com/hobu-lidar/autzen-classified.copc.laz sub.laz --max-level 2
terrio grid sub.laz dsm.tif --tr 10 --stat max
```

## Coordinates

```sh
terrio crs EPSG:32613
terrio crs EPSG:4326 --to EPSG:32613 -105.5 39.5
```

## Next

- [Command Line](cli.md) for every option
- [Coming from GDAL](gdal.md) for the commands and options that replace GDAL's
- [Benchmarks](benchmarks.md) for timings against GDAL
- [Python](python.md) and [JavaScript](javascript.md) for scripting
- [Playground](playground.md) and [Notebooks](notebooks.md) to try terrio in your browser
