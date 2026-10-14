# Command Line

```text
terrio <COMMAND>

  info        Summarize a raster or vector dataset
  convert     Convert a dataset to another format, optionally reprojecting it
  translate   Subset, resize, rescale or change the type of a raster (like gdal_translate)
  warp        Reproject and resample rasters (like gdalwarp)
  mosaic      Mosaic rasters into one, or into a virtual raster (like gdalbuildvrt and gdal_merge)
  rasterize   Burn vector geometries into a raster
  polygonize  Turn a raster band into polygons of connected pixels with the same value
  dem         Terrain analysis: hillshade, slope, aspect, color relief, TRI, TPI, roughness (like gdaldem)
  calc        Raster algebra: band math in NumPy syntax over one or more rasters (like gdal_calc.py)
  proximity   Distance from every pixel to the nearest target pixel (like gdal_proximity.py)
  sieve       Merge raster areas smaller than a threshold into their largest neighbour (like gdal_sieve.py)
  crs         Describe a coordinate reference system, or transform coordinates between two
  formats     List the formats terrio reads and writes
```

Every input can be a local path, a URL, a cloud object or a zip archive member; see
[Remote Data](remote-data.md). Outputs are local. Run `terrio <COMMAND> --help` for the full
option list.

## info

```sh
terrio info INPUT [--json] [--stats] [-l LAYER]...
```

Prints the driver, size, bands, data types, nodata, geotransform, CRS and extent of a raster, or
the layers, geometry types, feature counts, CRS, extent and fields of a vector dataset. `--stats`
computes band minimum, maximum, mean and standard deviation over valid pixels. Remote GeoTIFFs are
summarized from their header alone, and their overviews are listed.

## convert

```sh
terrio convert INPUT OUTPUT [-f FORMAT] [options]
```

| Option | Applies to | Meaning |
|---|---|---|
| `-f`, `--format` | both | Output driver short name (`GTiff`, `COG`, `GPKG`, `ESRI Shapefile`...) |
| `--t-srs CRS` | both | Reproject |
| `--compress`, `--tile-size`, `--bigtiff` | rasters | GeoTIFF/COG creation options |
| `--srcwin X Y W H`, `--overview N` | rasters | Read a window or an overview level |
| `-l`, `--layer` | vectors | Only these layers |
| `--where EXPR` | vectors | OGR SQL attribute filter |
| `--spat XMIN YMIN XMAX YMAX`, `--spat-srs` | vectors | Spatial filter |
| `--select FIELDS`, `--limit N` | vectors | Fields to keep, number of features |
| `--sql STATEMENT` | vectors | `SELECT ... FROM ... WHERE ... ORDER BY ... LIMIT` |
| `--nln NAME`, `--nlt TYPE` | vectors | New layer name, geometry type (`PROMOTE_TO_MULTI`...) |
| `--explodecollections` | vectors | One feature per part |
| `--s-srs`, `--a-srs` | vectors | Override or assign the CRS |
| `--append`, `--overwrite` | vectors | Add to or replace layers of an existing GeoPackage |
| `--oo`, `--lco KEY=VALUE` | vectors | Open and layer creation options (`RFC7946=YES`...) |

The `--where` language supports comparisons, `AND`/`OR`/`NOT`, parentheses, `IN`, `LIKE`/`ILIKE`,
`IS [NOT] NULL`, `BETWEEN`, arithmetic, `||` and `FID`, with OGR SQL's null semantics.

## translate

```sh
terrio translate INPUT OUTPUT [options]
```

`--srcwin`, `--projwin` (+ `--projwin-srs`), `-b` (repeatable), `--ot`, `--scale [SRC_MIN SRC_MAX
[DST_MIN DST_MAX]]`, `--exponent`, `--outsize W H` (pixels or `%`), `--tr`, `-r`, `--a-nodata`,
`--a-srs`, `--a-ullr`, and the output format options. Type conversion rounds and clamps as GDAL
does.

## warp

```sh
terrio warp INPUT... OUTPUT [options]
```

`--t-srs`, `--s-srs`, `--tr`, `--te` (+ `--te-srs`), `--ts`, `--tap`, `-r` (`near`, `bilinear`,
`cubic`, `cubicspline`, `lanczos`, `average`, `rms`, `mode`, `min`, `max`, `med`, `q1`, `q3`,
`sum`), `--srcnodata`, `--dstnodata`, `--ot`, `--cutline` (+ `--crop-to-cutline`), `--et`. Several
inputs are mosaicked into one output. The default output grid follows GDAL's
`GDALSuggestedWarpOutput2`.

## mosaic

```sh
terrio mosaic INPUT... OUTPUT [options]
```

`--resolution highest|lowest|average|user`, `--tr`, `--te`, `--tap`, `--separate`, `-b`,
`--nodata`, `--srcnodata`, `--dstnodata`, `-r`, `--t-srs`. A `.vrt` output writes a GDAL VRT that
references the inputs; other outputs hold the mosaicked pixels.

## rasterize

```sh
terrio rasterize INPUT OUTPUT (--burn VALUE | -a FIELD) (--tr X Y | --ts W H | --like RASTER) [options]
```

`-l`, `--sql`, `--where`, `--te`, `--tap`, `--ot`, `--init`, `--a-nodata`, `--at` (all touched),
`--add`, `--a-srs`. Points, lines and polygons are burned with GDAL's algorithms, pixel for pixel.

## polygonize

```sh
terrio polygonize INPUT OUTPUT [options]
```

`-b`, `--field NAME` (default `DN`), `--nln`, `--8` (8-connectedness), `--nomask`, `--mask RASTER`,
`-f`, `--append`, `--overwrite`. Polygons have holes; exteriors are counter-clockwise and holes
clockwise.

## dem

```sh
terrio dem MODE INPUT [COLOR_FILE] OUTPUT [options]
```

`MODE` is `hillshade`, `slope`, `aspect`, `color-relief` (which takes a color file), `TRI`, `TPI`
or `roughness`. Options: `-b`, `--compute-edges`, `--alg Horn|ZevenbergenThorne` (hillshade,
slope, aspect) or `--alg Riley|Wilson` (TRI); for hillshade `-z`, `-s` (`111120` for a geographic
DEM in metres), `--xscale`/`--yscale`, `--az`, `--alt`, `--combined`, `--multidirectional`,
`--igor`; `-p` (slope in percent); `--trigonometric`, `--zero-for-flat` (aspect); `--alpha`,
`--exact-color-entry`, `--nearest-color-entry` (color relief). Without `-s`, geographic DEMs are
scaled from the ellipsoid at their centre latitude, as `gdaldem` does since GDAL 3.11. Color files
are GDAL's: `value R G B [A]` or `value colorname [A]`, where the value may be `nv` (the nodata
value) or a percentage of the band's range; GMT `.cpt` files work too. Outputs are identical to
`gdaldem`'s, pixel for pixel.

## calc

```sh
terrio calc -A INPUT [--A_band N] [-B INPUT ...] --calc EXPR [--calc EXPR...] OUTPUT [options]
```

The arguments of `gdal_calc.py` work as they are (`-A file --A_band 2`, `--outfile`), and inputs
can also be named freely with `--input NAME=PATH` and `--input-band NAME=N`. Expressions use NumPy
syntax: `+ - * / // % **`, comparisons, `& | ^ ~`, `and`/`or`/`not`, and functions (`where`,
`clip`, `minimum`, `maximum`, `min`/`max` of several values, `abs`, `sqrt`, `exp`, `log`, `log10`,
trigonometric functions, `floor`, `ceil`, `round`, `isnan`, `logical_and`..., also written
`numpy.where` or `np.where`), `A.astype(float32)` and the constants `pi`, `e`, `nan`, `inf`.
Nothing else is evaluated. Values follow NumPy's typing rules, as in `gdal_calc.py`: integer
arithmetic wraps around (`A + B` of two Byte bands is Byte), `/` gives float64, and Python
numbers adapt to the array's type. A repeated `--calc` gives one band each. Options: `--type`,
`--NoDataValue` (or `none`), `--hideNoData`, `--extent union|intersect|ignore|fail`, `--projwin`,
`--allBands NAME`, `--projectionCheck`, and the output format options. Any input pixel equal to
its nodata value gives an output nodata pixel.

## proximity

```sh
terrio proximity INPUT OUTPUT [--values 1,2] [--distunits PIXEL|GEO] [--maxdist N] [options]
```

`--srcband`, `--nodata` (the value of pixels beyond `--maxdist`, 65535 by default),
`--use-input-nodata`, `--fixed-buf-val`, `--ot` (Float32 by default). Targets are the non-zero
pixels unless `--values` lists them.

## sieve

```sh
terrio sieve INPUT OUTPUT [--st N] [-4|-8] [--nomask | --mask RASTER]
```

Areas of connected pixels with the same value smaller than `--st` pixels (2 by default) take the
value of their largest neighbour. The band's nodata pixels are left out unless `--nomask`.

## crs

```sh
terrio crs CRS [--wkt]
terrio crs SRC --to DST X Y [X Y ...]
echo "X Y" | terrio crs SRC --to DST
```

## formats

```sh
terrio formats [--json]
```
