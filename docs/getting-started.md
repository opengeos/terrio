# Getting Started

This walk-through uses public data: Natural Earth countries, a USGS 3DEP elevation tile and a
Sentinel-2 image on AWS.

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
terrio convert ne_110m_admin_0_countries.zip countries.gpkg
terrio convert countries.gpkg countries.fgb
terrio convert countries.gpkg europe.parquet --where "CONTINENT = 'Europe'" --select NAME,POP_EST
terrio convert countries.gpkg countries_3857.geojson --t-srs EPSG:3857
```

Rasters convert the same way:

```sh
terrio convert USGS_1_n40w106.tif dem.tif -f COG --compress DEFLATE
```

## Process rasters

```sh
# A window, a type change and rescaling
terrio translate dem.tif subset.tif --srcwin 1000 1000 512 512 --ot Int16

# Reproject to UTM zone 13N at 30 m with bilinear resampling
terrio warp dem.tif dem_utm.tif --t-srs EPSG:32613 --tr 30 30 -r bilinear

# Mosaic tiles into a virtual raster, then a GeoTIFF
terrio mosaic tile1.tif tile2.tif tile3.tif mosaic.vrt
terrio mosaic tile1.tif tile2.tif tile3.tif mosaic.tif
```

## Between rasters and vectors

```sh
# Burn country populations into a 0.1° grid
terrio rasterize countries.gpkg population.tif -a POP_EST --tr 0.1 0.1 --ot Float64

# Turn a classified raster into polygons
terrio polygonize landcover.tif landcover.gpkg --field class
```

## Coordinates

```sh
terrio crs EPSG:32613
terrio crs EPSG:4326 --to EPSG:32613 -105.5 39.5
```

## Next

- [Command Line](cli.md) for every option
- [Benchmarks](benchmarks.md) for timings against GDAL
- [Python](python.md) and [JavaScript](javascript.md) for scripting
