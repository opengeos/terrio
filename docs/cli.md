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
  buffer      Planar buffers with round caps and joins
  overlay     Polygon intersection, union, difference and symmetric difference
  dissolve    Merge polygon features, optionally grouped by attributes
  is-valid    Report geometry validity and a reason per feature
  make-valid  Repair invalid geometry
  clean       Snap, deduplicate, close rings and repair geometry
  footprint   Exact valid-data footprint of a raster
  sample      Sample multiple raster bands at vector points
  contour     Contour lines at interval or fixed elevation levels
  zonal-stats Per-polygon statistics across multiple raster bands
  sql         Read-only SELECT queries over vector layers
  dem         Terrain analysis: hillshade, slope, aspect, color relief, TRI, TPI, roughness (like gdaldem)
  calc        Raster algebra: band math in NumPy syntax over one or more rasters (like gdal_calc.py)
  proximity   Distance from every pixel to the nearest target pixel (like gdal_proximity.py)
  viewshed    Single-observer visibility or minimum visible height (like gdal_viewshed)
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
Rust operations retain source feature IDs except when generating new features. Output formats
can assign new dataset-specific IDs when writing; GeoPackage outputs normally number them from 1.

## Vector topology

```sh
terrio buffer roads_utm.gpkg buffers.gpkg --distance 100 --quad-segs 30
terrio overlay parcels.gpkg zones.gpkg intersections.gpkg --operation intersection
terrio overlay parcels.gpkg zones.gpkg remainder.gpkg --operation difference
terrio dissolve counties.gpkg states.gpkg --field STATEFP
terrio is-valid polygons.gpkg validity.gpkg
terrio make-valid polygons.gpkg repaired.gpkg
terrio clean roads.gpkg cleaned.gpkg --snap-grid 0.01
```

These operations use planar XY coordinates in the source CRS. Buffer distances and cleanup
spacings are in metres for metre-based CRSs, and **degrees for geographic CRSs**; reproject
with `convert --t-srs` first when a linear distance is needed. They use pure Rust and require
no GDAL or GEOS installation. Inputs and outputs are held in memory. All accept the vector
layer selection and output options described above, including append and overwrite.

`buffer` retains attributes, source IDs and null geometries. Other outputs are XY
MultiPolygons with round caps and joins; `--quad-segs` controls segments per quadrant (1–4096,
default 30). Negative distances erode polygons; points and lines then have empty buffers.
Non-finite distances and invalid geometries fail; run `make-valid` first when needed.

`overlay` accepts Polygon/MultiPolygon features in a common CRS; both inputs may also have
unknown CRS. It does not reproject automatically. `--operation` is `intersection` (default),
`union`, `difference` or `symmetric-difference` (alias `xor`). `--layer` selects left layers;
`--overlay-layer` selects the single right layer when its dataset has several. Output fields
have `a_` and `b_` prefixes, followed by text `a_source_fid`/`b_source_fid` fields preserving
64-bit source IDs. Fields of an absent side are null. Collisions receive `_2`, `_3`, etc.
Null and empty geometries contribute no area; touching lines and points are omitted. Results
have sequential IDs from 1. Overlaps within an input retain source feature multiplicity;
dissolve the input first when disjoint coverage is required.

`dissolve` also accepts only valid Polygon/MultiPolygon features. Without `--field` it merges
the whole layer; repeat or comma-separate `--field` (alias `--by`) to group by typed attribute
values. Null values form a group; non-finite numeric group values are rejected. Only grouping
fields and `feature_count` are retained. Null geometries count toward a group but add no area;
an all-null group stays null. Groups follow first-seen order with sequential IDs from 1;
an empty layer produces no features. Output topology is XY MultiPolygon.

`is-valid` (alias `validity`) appends collision-safe Boolean `is_valid` and text `valid_reason`
fields, preserving geometry, Z/M and attributes. Null geometry has null status/reason; empty
geometry is valid. Rings are logically closed even if their driver omits the repeated closing
vertex. Reasons describe the first detected problem; their wording differs from GEOS.
Non-finite XY coordinates are reported as invalid.

`make-valid` preserves already-valid geometry, including Z/M. Invalid polygons use an odd-even
triangulated fill, which can change Polygon to MultiPolygon; rebuilt topology is XY. Collapsed
area parts are dropped, rather than returned as lines or points; a fully collapsed polygon
becomes an empty MultiPolygon. Degenerate lines become empty lines, and collections are
repaired recursively. Attributes, null features and source IDs are retained. Non-finite XY
or an unsuccessful repair returns an error.

`clean` removes consecutive XY duplicates, closes rings, drops empty/collapsed parts and repairs
invalid polygons. `--snap-grid` is a finite nonnegative spacing in CRS units: 0 (default) disables
snapping, otherwise XY coordinates round to the nearest multiple, with ties away from zero.
The first retained duplicate supplies Z/M; polygon repair discards Z/M when it rebuilds topology.
A fully collapsed feature becomes null, retaining its attributes and source ID. Non-finite XY
and coordinate overflow fail. Output formats may assign their own feature IDs on writing.

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
for sampling while the output retains the original point geometries, Z/M, attributes and
CRS. Nodata, non-finite pixels, out-of-bounds locations and null geometries yield null values.
Points that cannot be projected into a raster's CRS yield null samples for that raster too;
the CLI prints one warning per affected output field with the failure count. Errors constructing
the CRS transformation still fail the command.
Non-point geometries are rejected; use `singlepart` for multipart points first. Missing CRSs
are accepted only when both the points and the raster lack a CRS, using coordinates directly.
Complex bands and geolocation arrays are unsupported. Rasters are loaded into memory, and
sample values are represented as 64-bit floats (integers above 2⁵³ may lose precision).

## contour

```sh
terrio contour dem.tif contours.gpkg --interval 10
terrio contour dem.tif contours.gpkg --levels 100,150,200 --3d
terrio contour dem.tif contours.gpkg --interval 20 --offset 5 --levels 64.5
```

`contour` (alias `contours`) extracts line strings with `ID` and `ELEV` attributes; `--field`/`-a`
renames `ELEV`. Supply a positive `--interval`/`-i`, fixed `--levels`, or both. Interval levels
are `offset + k * interval`; explicit levels are sorted and deduplicated. The elevation field
must be nonempty and different from `ID`. `--band`/`-b` selects a band from 1 (default 1).
`--3d` puts the elevation in every vertex's Z coordinate.

Interpolation follows GDAL's marching squares, including its exact-level perturbation, fixed
saddle connectivity and nodata-boundary subdivision. Boundaries extend from pixel centres to
pixel edges. The source CRS and full affine geotransform are retained; feature ordering and
line direction can differ from GDAL. Declared nodata values and non-finite pixels are excluded.
`--nodata VALUE` overrides the nodata value; `--ignore-nodata` includes declared nodata as data.
These two options cannot be combined. Non-finite pixels are always excluded.

The raster and generated segments are held in memory. At most 100,000 levels are allowed.
This command creates contour lines; contour polygons are not provided. Complex bands and
geolocation arrays are unsupported. Standard vector output options apply.

## zonal-stats

```sh
terrio zonal-stats counties.gpkg stats.gpkg --raster elevation.tif --raster rain.tif
terrio zonal-stats counties.gpkg stats.gpkg --raster image.tif --band 1,2 --prefix image \
  --stats count,min,max,mean,sum,stddev,median --all-touched
```

`zonal-stats` (alias `zonal`) adds statistics independently for each polygon or multipolygon.
Repeat `--raster` for multiple rasters and use `--band` for comma-separated bands from each.
Default statistics are `count,min,max,mean,sum,stddev`; `median` is available explicitly.
Standard deviation uses the population definition. Fields are `FILE_STEM_bN_STATISTIC`, or
`PREFIX_bN_STATISTIC` when one `--prefix` is supplied per raster. Name collisions get `_2`,
`_3`, etc. `count` is an integer; the other statistics use 64-bit floats.

By default, a pixel contributes when its centre lies in the zone, following GDAL's rasterization
boundary rules. `--all-touched` includes every touched pixel. Holes are excluded. Overlapping
zones are evaluated independently, so a pixel can contribute to multiple zones. Declared nodata
and non-finite values are excluded; alpha bands and external masks are not applied implicitly.
With no valid contributing pixels, including null/empty geometries and zones outside the raster,
`count` is 0 and every other statistic is null. Source geometry, Z/M, attributes and CRS remain
unchanged. All non-null geometries must be polygons or multipolygons.

Rasters may use different resolutions, CRSs, rotated grids or south-up grids. Boundaries are
transformed for each raster; a boundary transformation failure is an error. Missing CRSs are
accepted only when both the zones and the raster have no CRS. Reprojection transforms stored
vertices without densifying curved edges. The rasters are loaded into memory; masks cover only
each zone's intersecting window and are shared by requested bands of the same raster. Median
needs extra memory for a zone's contributing values. Integers above 2⁵³ can lose precision.
Complex bands and geolocation arrays are unsupported. Standard vector input/output options apply.

## sql

```sh
terrio sql countries.gpkg selected.gpkg --query \
  "SELECT ADMIN, POP_EST AS population FROM countries WHERE POP_EST > 10000000 ORDER BY POP_EST DESC LIMIT 10"
```

Runs the existing read-only OGR SQL subset as a standalone command. `FROM` must name an input
layer, matched case-insensitively; `--layer` can restrict the available layers. Supports selected
fields and aliases, `WHERE`, one `ORDER BY` input field, and `LIMIT`. `WHERE` supports arithmetic,
comparisons, `AND`/`OR`/`NOT`, parentheses, `IN`, `BETWEEN`, `IS NULL`, `LIKE` and `ILIKE`.
Output field names must be unique, ignoring case. `FID` can be used in `WHERE` to filter IDs.
The `convert --sql` path keeps its existing fallback to the sole input layer when `FROM` does
not match a layer name.

Joins, aggregates, computed `SELECT` columns and spatial functions are unsupported. Geometries,
CRS and attribute types are retained. The Rust `ops::sql::query` API retains source feature IDs;
output drivers apply their normal file-ID conventions. Standard vector output options apply.

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

Swaths from netCDF and HDF-EOS use their longitude/latitude geolocation arrays automatically
when no GCP/RPC method is supplied. `--geoloc` (or `--to METHOD=GEOLOC_ARRAY`) requires arrays
on every source. Pixel/line offsets, separate sampling steps and RFC 4's `PIXEL_CENTER` or
`TOP_LEFT_CORNER` conventions are honored. The center shift is half a **geolocation-array
step**, including subsampled arrays. Coordinates interpolate bilinearly; an indexed inverse
maps output pixels through the same resampling kernels as other warps.

```sh
terrio warp 'NETCDF:"swath.nc":/PRODUCT/qa_value' swath.tif --geoloc --t-srs EPSG:4326 --tr 0.05 0.05
```

Coordinate nodata, NaNs, infinities and invalid geographic latitudes invalidate the neighboring
cells; no interpolation bridges those holes. Adjacent geographic longitudes are unwrapped
across the antimeridian; use canonical longitude extents on either side for GDAL comparisons.
Arrays must have matching dimensions of at least 2 by 2 and positive steps. Cutlines with
geolocation arrays are currently unsupported. Automatic selection preserves an existing affine
cutline workflow when the dataset also has a usable affine transform; explicitly selecting
geolocation arrays still rejects cutlines. Coordinate arrays and their cell index are held
in memory. The index stores only cell envelopes and array positions; its size is linear
in the number of valid cells, in addition to the two coordinate arrays. The inverse solves bilinear cells directly; GDAL's inverse backmap can differ by a
few boundary pixels, particularly with sparse arrays or invalid nodes. The conformance suite
reports those mask differences explicitly. Specify `--te` and `--ts` to compare identical grids.

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

## viewshed

```sh
terrio viewshed dem_utm.tif visible.tif --ox 500000 --oy 4400000 --oz 2 --tz 0 --md 5000
terrio viewshed dem_utm.tif height.tif --ox 500000 --oy 4400000 --om GROUND --cc 0
```

Single-observer viewshed using GDAL's default edge recurrence. `--om NORMAL` produces Byte
visibility (255 visible, 0 hidden); `DEM` produces the minimum visible elevation and `GROUND`
the minimum visible height above the DEM surface, both Float64. Height modes ignore `--tz`.
`--md` is maximum distance, zero for unlimited, and crops the output rectangle. Its corner
pixels outside the radius receive `--ov` (default 0). `--band` selects a source band.

The DEM must be north-up in a projected CRS. Heights use DEM units and maximum distance
uses projected CRS units. Observer coordinates are converted to pixels by truncating toward
zero, as in GDAL. Outside observers start at an unobstructed nearest edge. Their height is
absolute when X is outside; when only Y is outside, GDAL samples elevation on the nearest row.
For physically consistent curvature, use horizontal and vertical units in metres: GDAL's
curvature formula uses the ellipsoid diameter in metres even for other projected units. `--cc` is the curvature
coefficient, equal to 1 minus the atmospheric refraction coefficient: 0 gives a flat Earth,
1 disables refraction. The default is 0.85714 for Earth ellipsoids, 1 for other bodies.

Like GDAL, numeric nodata values are treated as elevations and masks are ignored; this can
produce misleading occlusion. Nonfinite elevations in the processed rectangle produce an
error. Fill or crop missing elevations before use. Output has no nodata marker. Cumulative
visibility is not supported. The command is also an MCP tool.

Terrio applies the radius mask deterministically. GDAL 3.12 can race its left/right row jobs
for an outside observer, occasionally leaving an out-of-range boundary pixel visible. The
conformance suite checks Terrio's mask independently and compares in-radius GDAL pixels for
that case; all other viewshed cases compare every pixel.

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

## fill-nodata

```sh
terrio fill-nodata dem.tif filled.tif --max-distance 100 --smoothing-iterations 1
terrio fill-nodata image.tif filled.tif --band 2 --mask validity.tif --interpolation nearest
```

Fills holes in one band using GDAL's four-quadrant inverse-distance (`inv_dist`, the
default) or nearest interpolation. The output contains the selected band, retaining its
type, nodata and georeferencing. Valid source pixels are preserved exactly. The mask's
first band is zero at holes and nonzero at donor pixels; without `--mask`, the source's
internal/sidecar validity mask is used, otherwise its nodata value. Mask dimensions must
match the input; mask georeferencing is ignored. `--max-distance` is in pixels (100 by
default, 0 for unlimited search). `--smoothing-iterations` applies 3×3 mean-filter passes
only to filled pixels, leaving the first/last rows unchanged as GDAL does. Inputs are
processed in memory; interpolation uses Float32 samples, including for Float64 rasters.
For nearest interpolation, GDAL's search fringe can write zero where the closest
quadrant candidate is less than one pixel beyond the distance limit; Terrio follows
that behavior. Complex bands are unsupported. Standard raster output
options (`--format`, `--compress`, `--co`, etc.) apply.

## Native multidimensional arrays

See [Native multidimensional arrays](multidim.md) for scientific groups, named dimensions,
lazy coordinate arrays, time/depth slicing, transposition and netCDF/Zarr numeric snapshots.
