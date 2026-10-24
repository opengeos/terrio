# Command Line

```text
terrio <COMMAND>

  info        Summarize a raster, vector dataset or point cloud
  convert     Convert a dataset to another format, optionally reprojecting it
  translate   Subset, resize, rescale or change the type of a raster (like gdal_translate)
  warp        Reproject and resample rasters (like gdalwarp)
  catalog     Read an indexed raster mosaic from GeoPackage or FlatGeobuf footprints
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
  pansharpen  Weighted Brovey pansharpening of panchromatic and spectral bands

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

Runs the read-only [OGR SQL subset](sql.md). `FROM` must name an input layer,
matched case-insensitively; `--layer` can restrict the available layers. Supports computed
columns, aliases, single-field `DISTINCT`, `COUNT`/`SUM`/`AVG`/`MIN`/`MAX`, and attribute
`LEFT JOIN` within the input dataset, plus `WHERE`, one primary `ORDER BY` field and `LIMIT`.

```sh
terrio sql countries.gpkg population.gpkg --query \
  "SELECT ADMIN, POP_EST/1000000.0 AS millions FROM countries"
terrio sql countries.gpkg summary.gpkg --query \
  "SELECT COUNT(*) AS countries, SUM(POP_EST) AS population FROM countries"
```

Computed columns and joins retain primary geometries, CRS and feature IDs. `DISTINCT` and
summaries produce attribute-only results. Output drivers apply their normal file-ID conventions.
String predicates (comparisons, `IN` and `BETWEEN`) now ignore ASCII case to match
current OGRSQL; `ORDER BY` and `MIN`/`MAX` retain case-sensitive ordering.
Output field names must be unique, ignoring case. The `convert --sql` path retains its legacy
single-layer fallback for `FROM` and allows duplicate output field names.

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
grids and pixels follow `gdalwarp`. `--overview` (`--ovr`) selects `AUTO` (the default),
`AUTO-n` (n levels finer), `NONE` (full resolution), or a stored level numbered from 0.
The target grid is always planned from full-resolution metadata before choosing source
overviews. Downsampling remote COGs therefore fetches coarse tiles instead of full-resolution
pixels. An unavailable explicit level falls back to the last available level, as GDAL does.

```sh
terrio warp scene.tif scene_4326.tif --rpc --to RPC_HEIGHT=120
terrio warp scene.tif scene_utm.tif --rpc --to RPC_DEM=dem.tif --t-srs EPSG:32720 -r bilinear
```

### Warp source overviews

```sh
terrio warp large-cog.tif preview.tif --ts 256 256 --overview AUTO
terrio warp large-cog.tif finer.tif --ts 256 256 --overview AUTO-1
terrio warp large-cog.tif full.tif --ts 256 256 --overview NONE
terrio warp large-cog.tif level.tif --overview 1
```

Overview selection changes which stored pixels are resampled; the final grid remains
based on the full-resolution source. Use `NONE` when the source pyramids were built
with a filter unsuitable for the analysis. Mosaics select a level independently for
each source. GCP/RPC transformations retain their full-resolution coordinate meaning.

Forced GCP overviews can give slightly different automatically suggested bounds than GDAL,
which refits GCPs at the selected resolution. Specify `--te` and `--ts` for an identical
target grid; Terrio keeps its full-resolution grid while selecting source pixels.

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

## pansharpen

```sh
terrio pansharpen pan.tif rgb.tif sharpened.tif
terrio pansharpen pan.tif red.tif green.tif blue.tif sharpened.tif --weight 0.6 --weight 0.3 --weight 0.1
terrio pansharpen pan.tif rgbnir.tif rgb.tif --band 1 --band 2 --band 3 --bitdepth 12 --nodata 0
```

GDAL-compatible weighted Brovey: `spectral * pan / sum(weight * spectral)` at the pan
resolution. Inputs must have the same CRS (or all omit it) and north-up geotransforms. Spectral inputs may
be multiband files or `PATH,band=N`; `--band` selects/reorders outputs in their flattened
input order. Every spectral input contributes intensity, including unselected output bands.
Weights default to equal weights and are used as provided, without normalization.

`--resampling` selects nearest, bilinear, cubic (default), cubicspline, lanczos or average.
Direct reads resample in the spectral native type; temporary upsampling first converts
spectral samples to the pan working type. Brovey works in the pan type for Byte/UInt16 and
Float64 otherwise. Intermediate integer results are rounded/clipped even with `--ot Float64`.
`--bitdepth` clamps Byte/UInt16 pan samples/results to `2^N-1`; other pan types ignore it,
as in GDAL. NBITS metadata is not inferred.

`--spat-adjust union` (default) pads unequal footprints with zero at their native resolutions,
`intersection` crops them, and `none` preserves the pan footprint. Extents align to the pan
pixel grid with GDAL's one-spectral-pixel tolerance. `--nodata VALUE` sets common nodata for
resampling and sharpening; `none` disables common nodata processing. By default GDAL 3.12's common value is inferred
from the selected pan band; spectral declarations do not override it. When common nodata
processing is disabled, GDAL's temporary upsampling buffer treats numeric source nodata as
ordinary sample values, and Terrio follows that behavior. Like GDAL, footprint padding/cropping drops the
source nodata declaration; use an explicit `--nodata` to preserve common nodata processing
with adjusted extents. A nodata contributor or zero weighted intensity
produces nodata in every output band when enabled; otherwise zero intensity produces zero.
Path and virtual reads follow GDAL's mask behavior: direct resampling honors explicit
dataset masks; temporary upsampling buffers, footprint-adjustment VRTs and common-nodata
override wrappers drop those masks. Nearest sampling copies pixels without applying masks.
Zero padding and nodata are separate: use `--nodata 0`
when padded margins should be missing. An output ending in `.vrt` (or `-f VRT`) writes a GDAL-compatible
`VRTPansharpenedDataset`, opening only source headers. Sources are stored as absolute paths
or URLs; moving the inputs requires updating those references. Declared VRT block sizes
must be positive integers and are treated as hints: Terrio uses fixed 512×512 internal
processing blocks. Opening the description and reading a window fetches the matching pan pixels and
spectral pixels with interpolation margins. GeoTIFF/COG outputs stream row strips with
bounded interpolation buffers; other output formats may gather the output in memory.
Inputs remain separate files needed by the VRT. Source formats without a lazy window
reader may load their pixels at open. As in GDAL, nodata metadata retains the supplied
number even when the working sentinel is rounded/clamped to an integer type. Rotated grids, cross-CRS alignment and
mixed output data types within a VRT are unsupported.
The command is also an MCP tool.

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

## catalog

```sh
terrio catalog tiles.gti.gpkg mosaic.tif --cache-size 32
terrio catalog tiles.fgb window.tif --window 100 200 256 256 --stats
terrio info GTI:tiles.gpkg --json
```

GTI-compatible GeoPackage/FlatGeobuf indexes contain polygon footprints and a string
`location` field naming each source. Relative tile paths resolve against the index directory;
absolute paths, URLs and supported VSI references select those sources directly. GTI XML
`IndexDataset` likewise accepts a relative or absolute index path.
`GTI:` forces catalog interpretation; `.gti.gpkg`, `.gti.fgb` and GTI XML `.gti` files open
through ordinary raster APIs too. Indexes are local files; referenced tiles may be remote COGs.

The catalog retains one index reader and parsed schema, queries it spatially for each window,
opens only selected sources and
keeps a bounded LRU cache (default 32 datasets). Later sources in composition order overwrite
earlier ones;
`--sort-field FIELD` sorts ascending before composition, and `--sort-descending` reverses it.
Source nodata, dataset masks and zero alpha are transparent; TIFF/PNG alpha is detected from
the source, and `--alpha-band` or catalog color-interpretation metadata identifies it otherwise.
Nonzero alpha copies source values without blending. Mixed source CRSs are reprojected into the index CRS.

`--layer`, `--location-field`, `--sort-field`, `--tr X Y`, `--te XMIN YMIN XMAX YMAX`,
`--srs`, `--band-count`, `--ot`, `--nodata`, `--alpha-band`, `--cache-size` and `-r`
override catalog characteristics. GTI layer metadata and XML supply their equivalents;
missing resolution or band count requires inspecting one tile. GeoTIFF/COG pixels stay
lazy; other formats follow their existing readers. Complete metadata opens zero tiles at startup.
When a tile is inspected, its type and optional nodata/alpha can also be inferred. Otherwise,
output type defaults to Byte and output nodata/alpha comes from catalog metadata or explicit
options. Source nodata/alpha always controls tile validity when pixels are requested.
`--stats` with `--window` reports persistent index reader opens/queries, logical source opens,
cache hits, selected features, source pixels (excluding separate mask samples), warped destination
samples and peak cached datasets. Each tile is warped only over its intersection with the request.
Band-subset reads (including `translate -b`) load and warp only requested bands plus source alpha.
Generating output alpha needs validity from all source data bands; a separate dataset-mask request
likewise computes coverage from every data band, without allocating a final pixel mosaic.
Untagged extra source bands are ignored rather than treated as alpha. Whole mosaics write in strips.
Window buffers use fallible allocation so capacity errors return an error.

The supported GTI subset includes `IndexDataset`, `IndexLayer`, `LocationField`, sort order,
SRS, resolution, extent, band count/type/nodata and alpha color interpretation.
The `XSIZE`/`YSIZE`/`GEOTRANSFORM` metadata triplet (XML `XSize`/`YSize`/`GeoTransform`)
also defines a north-up grid. Sizes must be positive integers; incomplete triplets and rotated
output transforms are rejected. Tile sources
must provide affine geotransforms; pre-warp GCP/RPC or swath sources, or wrap them in warped
VRTs. Index and output CRSs must match; use `warp` on the catalog to change the output CRS.
Mosaic bands share one data type and nodata value; per-band nodata/type differences, nested GTI catalogs,
GTI SQL/filters, explicit GTI overviews, color-table expansion and rotated GTI output grids are
not implemented. Compositing uses f64; Int64/UInt64 values beyond 2^53 may lose precision.
Mark a TIFF alpha band on export with `--co ALPHA=YES` when needed.

Catalogs supplied with lazy raster handles retain their storage and read only selected source
windows. Sharing a handle serializes access to its reader and preserves masks, alpha and
overviews. Supplied handles remain alive with the catalog; the cache limit bounds selected
reader handles, not the caller-owned source map. Native supplied paths open only when selected
and close on eviction; encoded files open on first selection.

Catalog mask caching retains at most 1 MiB of coverage. Larger mask requests return
their buffer directly and release the cache instead of retaining and cloning it.

## Serialized virtual rasters

```sh
terrio warp dem.tif projected.vrt --t-srs EPSG:3857 --ts 512 512
terrio convert projected.vrt crop.tif --srcwin 32 48 128 128
```

A `.vrt` warp destination (or `--format VRT`) writes a GDAL-compatible virtual
warp without reading the source pixels. Source references are absolute, so the
file can be written in a different directory. GDAL can read the result. Reading a
VRT resolves relative sources against its original location, including URLs and
archive members, and reads only windows needed for the requested output.

Serialized affine/reprojection, polynomial GCP, TPS, homography and RPC
transformers are supported. Warped reads use the declared VRT block boundaries,
preserving GDAL's approximation and resampling behavior for cropped requests.
Band mappings may reorder or repeat source bands; nodata is retained per band,
including `UNIFIED_SRC_NODATA=YES`, `NO` and `PARTIAL`. GeoTIFF file output uses
a `.aux.xml` sidecar when nodata differs between bands or a Float32 nodata
value needs its original double precision; keep that sidecar with the TIFF. A standalone TIFF byte buffer has only the shared embedded nodata tag.
Raster creation options (compression, tiles, masks, overviews and metadata overrides)
are rejected for a serialized warp; set the warp grid through the warp options.
GDAL 3.12's generic `sum` polygon path can lose overlap area on pixel boundaries,
including when a redundant identity CRS transformer disables its affine fast path.
Terrio preserves conservative affine overlap sums; its results match GDAL's affine
path within one Float32 ULP, and can differ from that generic clipping behavior.
Serialized cutlines, alpha warps and geolocation transformers are currently
reported as unsupported; use a materialized warp for those supported by the
normal warper. Custom `INIT_DEST` values are also unsupported.

`VRTDerivedRasterBand` supports the scalar built-ins `sum`, `diff`, `mul`, `div`,
`mean`, `min`, `max`, `sqrt`, `real`, `imag`, `conj`, `mod`, `inv`, `intensity`,
`log10`, `dB`, `dB2amp`, `dB2pow`, `pow` and `exp`, with numeric arguments,
source transfer types and applicable nodata behavior. Complex-valued derived
buffers, external pixel functions and expression languages are unsupported.
`KernelFilteredSource` supports odd full/separable kernels, normalization and
`min`, `max`, `stddev`, `median` and `mode`. Kernels read the necessary halo and
replicate source edges as GDAL does. Explicit dataset masks, `SourceBand mask,N`
and `ComplexSource` with `UseMaskBand` are read lazily.

Mixed native band types use a common numeric buffer selected with GDAL-style
`DataTypeUnion` promotion, preserving Float64 precision. Plain and derived bands
round to their declared type before entering that buffer. A selected lazy band
retains its native type. Warped VRTs convert source windows and compute shared
blocks in the XML `WorkingDataType`; if omitted, it is inferred from source,
destination and nodata types. The working block converts directly to the requested
common buffer, as GDAL's warped dataset RasterIO does. This can retain fractional
working values for an integer-declared band in a multiband read; reading that band
alone converts to its native integer type. Python GDAL's default `ReadAsArray`
chooses Float32 for any mixed dataset; use its explicit `buf_type` argument to
compare a promoted Float64 buffer.

VRT references use the caller’s normal file and network access. Supplied source maps
override matching names; unresolved names are opened by the usual drivers.
For heterogeneous plain/derived bands, a shared buffer uses the common numeric
type needed by its requested bands. Each band's declared rounding is retained;
a leading Byte band does not truncate later floating-point bands.

The Rust `VirtualRaster` API parses and serializes these descriptions. Its
`into_lazy` source map also permits filesystem-free reads in WebAssembly.


Warped VRT block evaluation is limited to 8,388,608 samples across all bands per
declared block. Larger blocks return an unsupported error, including for small
subwindow reads, to keep temporary memory bounded while preserving GDAL block semantics.
