# Command Line

```text
terrio <COMMAND>

  info        Summarize a raster, vector dataset or point cloud
  convert     Convert a dataset to another format, optionally reprojecting it
  translate   Subset, resize, rescale or change the type of a raster (like gdal_translate)
  warp        Reproject and resample rasters (like gdalwarp)
  mosaic      Mosaic rasters into one, or into a virtual raster (like gdalbuildvrt and gdal_merge)
  rasterize   Burn vector geometries into a raster
  polygonize  Turn a raster band into polygons of connected pixels with the same value
  centroid    One planar centroid per feature
  singlepart  Split multipart geometries and collections into individual features
  vertices    Extract vertices as points with source attributes and vertex indices
  footprint   Exact valid-data footprint of a raster
  sample      Sample multiple raster bands at vector points
  dem         Terrain analysis: hillshade, slope, aspect, color relief, TRI, TPI, roughness (like gdaldem)
  calc        Raster algebra: band math in NumPy syntax over one or more rasters (like gdal_calc.py)
  proximity   Distance from every pixel to the nearest target pixel (like gdal_proximity.py)
  sieve       Merge raster areas smaller than a threshold into their largest neighbour (like gdal_sieve.py)
  crs         Describe a coordinate reference system, or transform coordinates between two
  formats     List the formats terrio reads and writes
  grid        Grid a point cloud into a raster (like PDAL's writers.gdal)
  filter      PDAL's point cloud filters (smrf, pmf, csf, elm, hag_nn, crop, expression...)
  mcp         Serve these commands to AI agents over the Model Context Protocol
```

Install it with `curl -LsSf https://terrio.opengeos.org/install.sh | sh` (on Windows,
`irm https://terrio.opengeos.org/install.ps1 | iex` in PowerShell), `pip install terrio` or
`cargo install terrio-cli`; see [Installation](installation.md).

Every input can be a local path, a URL, a cloud object or a zip archive member; see
[Remote Data](remote-data.md). Outputs are local. Run `terrio <COMMAND> --help` for the full
option list.

## Geometry operations

```sh
terrio centroid countries.gpkg centroids.gpkg
terrio singlepart countries.gpkg parts.gpkg --layer countries
terrio vertices roads.gpkg nodes.gpkg
```

All three preserve the input attributes and CRS. `centroid` (alias `centroids`) computes a 2D,
planar centroid: area-weighted for polygons with holes subtracted, length-weighted for lines,
and equally weighted for points. Collections use the highest non-degenerate dimension. Empty
and null geometries yield null centroids, retaining the feature. The calculation uses the input
coordinate units; reproject geographic data first when a centroid in linear units is needed.

`singlepart` (alias `multipart-to-singlepart`) recursively splits multipart geometries and
collections, retaining Z/M and assigning new sequential feature IDs from 1. Null geometries
and empty single geometries are retained; empty multipart geometries yield no features.

`vertices` (alias `extract-vertices`) retains Z/M and adds `source_fid` (text, preserving all
64-bit IDs), `part_index`, `ring_index` and `vertex_index`. Indices start at 0; polygon exteriors
are ring 0, while non-polygons have a null ring index. Parts follow recursive traversal order.
The repeated closing vertex of each polygon ring is included, including when the format reader
omits it internally. Null and empty geometries yield no vertices. Field name collisions are
resolved case-insensitively with `_2`, `_3`, etc.
The result is held in memory, with a separate copy of the source attributes for each vertex.

Vector inputs accept repeated `--layer`/`-l`. These commands, `footprint` and `sample` accept
`-f FORMAT`, `--nln NAME` (one output layer), `--append` and `--overwrite`. As with `convert`,
the default writes a new dataset, replacing an existing file; `--overwrite` replaces only
the named layers of a GeoPackage, preserving its other layers.

## footprint

```sh
terrio footprint image.tif footprint.gpkg
terrio footprint image.tif footprint.gpkg --band 1,2 --intersection
terrio footprint image.tif footprint.gpkg --mask valid.tif
```

Writes one multipolygon of all finite, non-nodata pixels, preserving holes, disconnected areas,
the full affine geotransform (including rotated/south-up grids) and the source CRS. Boundaries
follow exact pixel edges, without overviews or simplification. Pixels connect through their sides
(4-connectedness); regions touching only at a corner stay separate polygon members, as in GDAL's
default footprint. An entirely invalid raster
produces an empty layer. By default, any band may be valid; `--intersection` requires all selected
bands to be valid. `--band` selects bands from 1; omitted, it uses all bands.

`--mask` further restricts the footprint to finite, nonzero, non-nodata pixels of the mask's first
band; its grid and CRS must match the input. Alpha bands and external mask files are not used as
implicit masks. Complex bands and geolocation arrays are unsupported.

## sample

```sh
terrio sample sites.gpkg samples.gpkg --raster dem.tif --raster rain.tif
terrio sample sites.gpkg samples.gpkg --raster image.tif --band 1,2,3 --prefix image
```

Samples the pixel containing each point, adding a Float field for each raster/band pair. Repeat
`--raster` for multiple rasters; `--band` selects the same bands (from 1, default 1) from each.
Fields are named `FILE_STEM_bN`, or `PREFIX_bN` when one `--prefix` is supplied per raster in the
same order. Name collisions get `_2`, `_3`, etc.; existing attributes remain intact.

Each raster may have a different resolution, affine grid or CRS. Coordinates are transformed
for sampling while the output retains the original point geometries, Z/M, IDs, attributes and
CRS. Nodata, non-finite pixels, out-of-bounds locations and null geometries yield null values.
Points that cannot be projected into a raster's CRS yield null samples for that raster too;
the CLI prints one warning per affected output field with the failure count. Errors constructing
the CRS transformation still fail the command.
Non-point geometries are rejected; use `singlepart` for multipart points first. Missing CRSs
are accepted only when both the points and the raster lack a CRS, using coordinates directly.
Complex bands and geolocation arrays are unsupported. Rasters are loaded into memory, and
sample values are represented as 64-bit floats (integers above 2⁵³ may lose precision).

## info

```sh
terrio info INPUT [--json] [--stats] [--hist] [-l LAYER]...
```

Prints the driver, size, bands, data types, nodata, geotransform, CRS and extent of a raster, or
the layers, geometry types, feature counts, CRS, extent and fields of a vector dataset. `--stats`
computes band minimum, maximum, mean and standard deviation over valid pixels. Remote GeoTIFFs are
summarized from their header alone, and their overviews are listed. The subdatasets of netCDF,
HDF5 and GeoPackage files are listed as `gdalinfo` lists them (`SUBDATASET_N_NAME` and
`SUBDATASET_N_DESC`).

`--hist` computes each band's histogram as `gdalinfo -hist` does, and prints it as it does
(`256 buckets from -0.5 to 255.5:` and the counts; a `histogram` object of `count`, `min`, `max`
and `buckets` in the JSON): 256 buckets over -0.5 to 255.5 for Byte bands and -128.5 to 127.5 for
Int8 bands; for the other types over GDAL's approximate minimum and maximum (statistics stored
in the file's metadata or `.aux.xml` sidecar, the format's own range, the smallest overview of more
than 2500 pixels, or a sample of the blocks), widened by half a bucket on each side; with
`--stats`, over the exact minimum and maximum. Nodata and NaN pixels are not counted, values
outside the range fall in the first or last bucket, and a histogram GDAL stored in the `.aux.xml`
sidecar is used as it is. The results are identical to `gdalinfo -hist`'s for GeoTIFFs (strips,
tiles, overviews) and the formats GDAL reads by scanline.

## convert

```sh
terrio convert INPUT OUTPUT [-f FORMAT] [options]
```

| Option | Applies to | Meaning |
|---|---|---|
| `-f`, `--format`, `--of` | both | Output driver short name (`GTiff`, `COG`, `GPKG`, `ESRI Shapefile`...) |
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
| `--append`, `--overwrite` | vectors, rasters (`--append`) | Add to or replace layers of an existing GeoPackage or vector tile archive; add a raster to a PMTiles or MBTiles archive of image tiles |
| `--oo`, `--lco KEY=VALUE` | vectors | Open and layer creation options (`RFC7946=YES`...) |
| `--co KEY=VALUE` | rasters | Creation options (Zarr; netCDF: `FORMAT=NC4`, `COMPRESS=DEFLATE`, `ZLEVEL`, `CHUNKING`; PMTiles and MBTiles: `TILE_FORMAT=WEBP`...) |
| `--minzoom N`, `--maxzoom N` | both | Zoom levels of PMTiles, MBTiles and MVT outputs |

Tile archives (PMTiles, MBTiles, MVT) take `--oo ZOOM_LEVEL=N` and `--oo CLIP=NO` as inputs; see
[Formats](formats.md#tile-archives).

Point clouds (LAS, LAZ, COPC, EPT, PLY, E57) convert to LAS, LAZ, COPC, PLY, E57, an EPT
dataset (an output named `ept.json`, `--ept-data-type laszip|binary|zstandard`) or a vector
layer of points, with `--bounds`, `--z-range`, `--class`, `--returns`, `--max-level`,
`--resolution`, `--max-points`, `--t-srs`, `--a-srs`, `--point-format`, `--las-version`,
`--scale`, `--offset` and `--dims`; see [Point Clouds](point-clouds.md).

Vector layers convert as streams, a chunk of features at a time, when the input is a local
GeoPackage, FlatGeobuf, GeoParquet, Shapefile, GeoJSON, GeoJSONSeq, CSV or OpenStreetMap PBF
file, the output a GeoPackage, FlatGeobuf, GeoParquet, Shapefile, GeoJSON, GeoJSONSeq or CSV
file other than the input, neither `--sql`, `--append` nor `--overwrite` is given, and `--lco`
gives only CSV and GeoJSON options: memory stays bounded whatever the size of the layer (a
15-million-polygon, 3.8 GB GeoPackage converts to GeoParquet in 192 MB, as `ogr2ogr` does in
169 MB). Several layers go to a GeoPackage one after another; the other formats hold one layer,
chosen with `-l`. OpenStreetMap PBF files stream when sorted by type then id, as osmium,
Geofabrik and the planet files are: only the nodes (about 6 bytes each) and the ways relations
use are kept (Colorado's 385 MB extract converts to a GeoPackage in 19 s and 1.5 GB, where
`ogr2ogr` takes 44 s and 0.45 GB). The other conversions read the layers whole. See
[Formats](formats.md#vectors).

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
`sum`), `--srcnodata`, `--dstnodata`, `--ot`, `--cutline` (+ `--crop-to-cutline`), `--et`,
`--order`, `--tps`, `--rpc`, `--to` and `--wo`. Several inputs are mosaicked into one output. The default output grid
follows GDAL's `GDALSuggestedWarpOutput2`; `--dst-transform X0 A B Y0 D E` with `--ts` sets any
affine output grid instead, rotated ones included (GDAL's warper API, which `gdalwarp` does not
expose). Rotated and south-up inputs are warped through their
geotransform; a GeoTIFF georeferenced by GCPs is warped with GDAL's GCP transformers: a
homography from 4 or 5 GCPs, otherwise a polynomial (of order 2 from 6 GCPs, else 1, or
`--order 1|2|3`), or thin plate splines (`--tps`), giving `gdalwarp`'s pixels.

Images with RPCs (the GeoTIFF RPC tag, or a `_RPC.TXT` or `.RPB` sidecar) are warped with GDAL's
RPC transformer when they have neither a geotransform nor GCPs, or with `--rpc` (`gdalwarp
-rpc`). `--to KEY=VALUE` passes its options as `gdalwarp -to` does: `RPC_HEIGHT`,
`RPC_HEIGHT_SCALE`, `RPC_DEM` (a DEM giving the ground height, read whole), `RPC_DEMINTERPOLATION`
(`near`, `bilinear`, `cubic`), `RPC_DEM_MISSING_VALUE`, `RPC_DEM_SRS`,
`RPC_DEM_APPLY_VDATUM_SHIFT`, `RPC_PIXEL_ERROR_THRESHOLD` and `RPC_MAX_ITERATIONS`. As `gdalwarp`
for a source with RPCs, 5 more source pixels are read around each chunk, an RPC DEM makes the
transformations exact (`--et 0`) and samples every edge point, and a GeoTIFF output is warped only
where the source's footprint falls. `--wo SOURCE_EXTRA=n`, `--wo SAMPLE_STEPS=ALL` and
`--wo SKIP_NOSOURCE=NO` set those warp options (other warp options are ignored, with a warning). On ICEYE images with a Copernicus DEM, the output
grids and pixels are `gdalwarp`'s (with `-ovr NONE`: terrio does not warp from overviews).

```sh
terrio warp scene.tif scene_4326.tif --rpc --to RPC_HEIGHT=120
terrio warp scene.tif scene_utm.tif --rpc --to RPC_DEM=dem.tif --t-srs EPSG:32720 -r bilinear
```

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

A color relief written as a VRT (`-f VRT`, `--of VRT`, or a `.vrt` output) is a virtual raster of
lookup tables over the DEM, as `gdaldem color-relief -of VRT` writes it: three Byte bands (four
with `--alpha`), each a `ComplexSource` of the DEM's band with a `LUT` built from the color file
(the midpoints between entries with `--nearest-color-entry`, zero around them with
`--exact-color-entry`). The lookup tables are those `gdaldem` writes, so GDAL reads the same
pixels from both, and terrio reads such VRTs (`gdaldem`'s too) as GDAL reads them.

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
terrio crs SRC --to DST [--s_coord_epoch YEAR] [--t_coord_epoch YEAR] X Y [X Y ...]
echo "X Y [Z [T]]" | terrio crs SRC --to DST [--s_coord_epoch YEAR] [--t_coord_epoch YEAR]
```

As `gdaltransform`: lines of standard input with a height transform it too, with a time (a decimal
year) use it for time-dependent transformations, and a point that cannot be transformed prints
`transformation failed.`. Datum shift grids and geoid models are used when they can be found (see
[Coordinate Systems](crs.md#datum-shift-grids)).

## formats

```sh
terrio formats [--json]
```

## grid

```sh
terrio grid INPUT OUTPUT --tr RES [--stat min,max,mean,idw,count,stdev|all] [--dim z]
            [--radius R] [--power P] [--window-size N] [--bin] [--te XMIN YMIN XMAX YMAX]
            [--class ...] [--returns ...] [--bounds ...]
```

Grids a point cloud into a raster as PDAL's `writers.gdal` does: a DSM with `--stat max`, a DTM
with `--class 2 --stat idw`, a density map with `--stat count --bin`. See
[Point Clouds](point-clouds.md).

## mcp

```sh
terrio mcp
```

Serves the other commands to AI agents (Claude Code, Claude Desktop, Cursor, VS Code) as a Model
Context Protocol server over standard input and output. See [AI Agents](mcp.md).
