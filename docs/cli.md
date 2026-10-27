# Command Line

```text
terrio <COMMAND>

  info            Summarize a raster, vector dataset or point cloud
  convert         Convert a dataset to another format, optionally reprojecting it
  concat          Concatenate vector layers and datasets (like gdal vector concat and ogrmerge.py)
  partition       Split a vector dataset into files by field values (like gdal vector partition)
  set-field-type  Change field types with GDAL's value conversions (like gdal vector set-field-type)
  make-point      Point geometries from coordinate fields (like gdal vector make-point)
  create          Create a vector dataset from a schema or template (like gdal vector create)
  export-schema   Print a vector dataset's OGR_SCHEMA JSON (like gdal vector export-schema)
  rename-layer    Rename vector layers or normalize their names (like gdal vector rename-layer)
  edit            Assign vector CRS, geometry declarations or metadata (like gdal vector edit)
  update          Update or append vector features by key or FID (like gdal vector update)
  translate       Subset, resize, rescale or change the type of a raster (like gdal_translate)
  warp            Reproject and resample rasters (like gdalwarp)
  catalog         Read an indexed raster mosaic from GeoPackage or FlatGeobuf footprints
  mosaic          Mosaic rasters into one, or into a virtual raster (like gdalbuildvrt and gdal_merge)
  rasterize       Burn vector geometries into a raster
  polygonize      Turn a raster band into polygons of connected pixels with the same value
  centroid        One planar centroid per feature
  singlepart      Split multipart geometries and collections into individual features
  vertices        Extract vertices as points with source attributes and vertex indices
  buffer          Planar buffers with round caps and joins
  overlay         Polygon intersection, union, difference and symmetric difference
  dissolve        Merge polygon features, optionally grouped by attributes
  is-valid        Report geometry validity and a reason per feature
  make-valid      Repair invalid geometry
  clean           Snap, deduplicate, close rings and repair geometry
  simplify        Topology-preserving simplification (like gdal vector simplify)
  segmentize      Limit segment lengths by adding vertices (like gdal vector segmentize)
  convex-hull     Convex hull per feature (like gdal vector convex-hull)
  concave-hull    Concave hull per feature (like gdal vector concave-hull)
  swap-xy         Swap X and Y coordinates (like gdal vector swap-xy)
  set-geom-type   Change geometry types and dimensions (like gdal vector set-geom-type)
  combine         Collect geometries into collections by groups (like gdal vector combine)
  sort            Spatial sort along a Hilbert curve or an STR tree (like gdal vector sort)
  footprint       Exact valid-data footprint of a raster
  sample          Sample multiple raster bands at vector points
  contour         Contour lines at interval or fixed elevation levels
  zonal-stats     Per-polygon statistics across multiple raster bands
  sql             Read-only SELECT queries over vector layers
  dem             Terrain analysis: hillshade, slope, aspect, color relief, TRI, TPI, roughness (like gdaldem)
  calc            Raster algebra: band math in NumPy syntax over one or more rasters (like gdal_calc.py)
  proximity       Distance from every pixel to the nearest target pixel (like gdal_proximity.py)
  pansharpen      Weighted Brovey pansharpening of panchromatic and spectral bands
  compare     Compare two rasters and report the differences (like gdal raster compare, gdalcompare.py)
  pixel-info  Pixel values at positions (like gdal raster pixel-info, gdallocationinfo)
  xyz         Raster pixels as XYZ text (like gdal2xyz.py)

  viewshed        Single-observer visibility or minimum visible height (like gdal_viewshed)
  sieve           Merge raster areas smaller than a threshold into their largest neighbour (like gdal_sieve.py)
  unscale         Apply band scales and offsets to get physical values (like gdal raster unscale)
  nodata-to-alpha Replace nodata values with an alpha band (like gdal raster nodata-to-alpha)
  reclassify      Map values or ranges of values to new values (like gdal raster reclassify)
  as-features     One vector feature per pixel, with band values as fields (like gdal raster as-features)
  neighbors       Focal statistics and convolution kernels (like gdal raster neighbors)
  clean-collar    Set noisy nearly black/white collars to exact black/white (like gdal raster clean-collar)
  nearblack       The same, with nearblack's two-pass defaults (like nearblack)
  enhance         Contrast enhancement by lookup tables: equalization and LUT files (like gdalenhance)
  crs             Describe a coordinate reference system, or transform coordinates between two
  formats         List the formats terrio reads and writes
  grid            Grid a point cloud into a raster (like PDAL's writers.gdal)
  filter          PDAL's point cloud filters (smrf, pmf, csf, elm, hag_nn, crop, expression...)
  vsi             List, copy, delete, move and sync files on local, cloud, HTTP and archive locations (like gdal vsi)
  mcp             Serve these commands to AI agents over the Model Context Protocol
```

Install it with `curl -LsSf https://terrio.opengeos.org/install.sh | sh` (on Windows,
`irm https://terrio.opengeos.org/install.ps1 | iex` in PowerShell), `pip install terrio` or
`cargo install terrio-cli`; see [Installation](installation.md).

Every input can be a local path, a URL, a cloud object or a zip archive member; see
[Remote Data](remote-data.md). Outputs are local, except those of `terrio vsi`. Run `terrio <COMMAND> --help` for the full
option list.

The commands that take any number of inputs (`mosaic`, `warp`, `concat` and `filter merge`) read
them from a file too, as GDAL's `--input @filelist.txt` does. `pansharpen` does not: its inputs
have fixed roles, the panchromatic band first, and spectral inputs may carry `,band=N`. `@FILE`
stands for the paths listed in FILE, one per line, with relative paths taken from the working
directory. Unlike GDAL 3.13, which opens each line as written and fails on a blank line, terrio
skips blank lines, trims whitespace around paths and ignores a byte-order mark at the start of the
file. A list must be a regular file of at most 64 MiB. As in GDAL, every input beginning with `@`
is read as a list: name a file whose name begins with `@` as `./@name.tif`. The [MCP tools](#mcp)
refuse `@FILE`, since errors naming listed paths would let a client read a text file back line by
line; MCP clients pass the inputs as arguments instead. Lists mix with paths given directly:

```sh
terrio mosaic @filelist.txt naip.vrt
terrio warp @filelist.txt extra.tif out.tif --t-srs EPSG:3857
```

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

## Vector geometry commands

```sh
terrio simplify counties.gpkg simple.gpkg --tolerance 500
terrio segmentize borders.gpkg dense.gpkg --max-length 0.1
terrio convex-hull parcels.gpkg hulls.gpkg
terrio concave-hull places_by_country.gpkg hulls.gpkg --ratio 0.3 --allow-holes
terrio swap-xy latlon.gpkg lonlat.gpkg
terrio set-geom-type roads.gpkg roads_multi.gpkg --multi --dim XYZ
terrio combine places.gpkg places_by_country.gpkg --group-by adm0_a3
terrio sort buildings.gpkg buildings_sorted.fgb --method hilbert
terrio convert counties.shp simple.gpkg --simplify 0.01
```

These are pure-Rust equivalents of GDAL 3.13's `gdal vector` commands of the same names; no
GDAL or GEOS installation is needed. Distances are in the input CRS units (degrees for
geographic CRSs). All accept the vector layer selection and output options described above.
The per-feature commands (`simplify`, `segmentize`, `convex-hull`, `concave-hull`, `swap-xy`,
`set-geom-type`) keep attributes, feature IDs and null geometries, and stream a chunk of
features at a time when the input and output formats allow it, as `convert` does; `combine` and
`sort` read whole layers, as GDAL does. The `vector-geom` conformance suite
(`scripts/compare_vector_geom_with_gdal.py`) compares them with GDAL and GEOS on real data.

`simplify --tolerance` is a port of GEOS 3.14's `TopologyPreservingSimplifier`, which GDAL runs
(`OGRGeometry::SimplifyPreserveTopology`): Douglas-Peucker simplification of every line and
ring, where a section is flattened only when the new segment crosses no other segment of the
feature and moves no other part (a hole, say) to its other side. Rings keep at least four
points and their start point may move. Simplification is per feature, so shared boundaries
between features may separate. Outputs are vertex-for-vertex identical to `gdal vector
simplify` on the conformance data. Z is kept and M dropped, as GDAL's GEOS conversion does.
`convert --simplify TOLERANCE` (`ogr2ogr -simplify`) applies the same operation before
reprojection and keeps a geometry whose simplification fails.

`segmentize --max-length` is OGR's `segmentize`: a segment of length L gets
`floor(L / max - 0.01)` evenly spaced points (none when L is within a relative 1e-5 of the
maximum). Lines are processed from the end with the larger X (then Y), so a line and its
reverse get the same points, and added points copy the Z/M of the segment start in that
direction (they are not interpolated). Ring closing segments are divided too. `convert
--segmentize` is `ogr2ogr -segmentize`; it cannot be combined with `--simplify`.
Segmentization allows at most ten million output vertices per feature across its
lines, rings and collection members. It checks each component against the remaining
budget before allocating its output. This limit does not cap a whole layer's total output.

`convex-hull` is GEOS's Graham scan: a polygon starting at the lowest, then leftmost, vertex,
clockwise; a line string for collinear input and a point for one distinct vertex. The layer's
type becomes unknown. `concave-hull --ratio R` (0 most concave, 1 convex) follows GEOS's
`ConcaveHull` for points and lines (erosion of the Delaunay triangulation of the vertices, by
longest border edge) and `ConcaveHullOfPolygons` for polygons (erosion of the constrained
Delaunay triangulation of the space between them, united with them); `--allow-holes` allows
holes and `--tight` makes polygon hulls follow the polygons' outer boundaries. Triangulations
come from the `spade` crate: ring start and orientation may differ from GEOS's, and inputs with
co-circular vertices (grids) can triangulate, and so erode, differently. As GDAL 3.13, polygon
and multipolygon layers keep their declared type and hulls are forced to it, which can join
several polygons into one invalid polygon in a layer declared POLYGON; other layers' type
becomes unknown.

`set-geom-type` ports OGR's `forceTo` for linear geometries: `--geometry-type TYPE` (POINT,
LINESTRING, POLYGON, MULTIPOINT, MULTILINESTRING, MULTIPOLYGON, GEOMETRYCOLLECTION or GEOMETRY,
with an optional Z, M, ZM or 25D suffix), or a combination of `--multi`/`--single`, `--linear`
and `--dim XY|XYZ|XYM|XYZM` (added Z and M values are 0). Conversions that cannot be made leave
the geometry type as it is (OGR may still apply the target's dimensions); `--skip` drops those
features. `--layer-only` changes only the declared layer type, `--feature-only` only the
geometries. terrio has no curve geometries: `--curve` and curve or surface types are refused,
and `--linear` changes nothing. terrio's writers store no M values (they remain in Python and
JavaScript layers), and the declared Z/M of a layer follows its geometries.

`combine` collects each group's geometries into one feature: without `--group-by` the whole
layer (an empty layer produces no features, as in GDAL), otherwise one feature per combination of the fields' values (repeat or comma-separate).
Groups are ordered by their values as OGR formats them as text, nulls as empty text, as GDAL
orders them. Output fields are the grouping fields; `--add-extra-fields sometimes-identical`
also copies the fields identical within at least one group (null for the groups where they
differ) and `always-identical` those identical within every group. Layers of single-part types
give MultiPoint, MultiLineString or MultiPolygon features, others GeometryCollections; parts of
multi-part inputs are added one by one unless `--keep-nested` adds them whole (the output then
becomes a GeometryCollection). A geometry that does not fit the layer's collection type is an
error, as in GDAL. Null and empty geometries add nothing (a group of them gets an empty
collection); when any geometry has Z, all outputs do (0 where missing). Feature IDs are
sequential from 1.

`sort --method hilbert` (default) orders features by GDAL's Hilbert code of the centre of each
bounding box on a 65535 × 65535 grid over the layer's extent; `--method strtree` by the leaf
order of GEOS's sort-tile-recursive tree with nodes of 10 (slices by box centre X, each sorted
by box centre Y). Null and empty geometries go last. terrio's sorts are stable, GDAL's are not:
features whose keys tie may come in another order. Writing a sorted layer to FlatGeobuf
re-sorts it for the spatial index.

Not supported: GDAL's `--active-layer`, `--active-geometry` and `--geometry-field` (terrio
layers have one geometry), `--use-tempfile` for `sort`, and the `--skip-errors`, `--upsert`
and creation-option flags of `gdal vector` writes.

## footprint

```sh
terrio footprint image.tif footprint.gpkg
terrio footprint image.tif footprint.gpkg --band 1,2 --intersection
terrio footprint image.tif footprint.gpkg --mask valid.tif
terrio footprint image.tif footprint.shp --singlepart --attribute survey=2026 --attribute "label=North field"
```

Writes one multipolygon of all finite, non-nodata pixels, preserving holes, disconnected areas,
the full affine geotransform (including rotated/south-up grids) and the source CRS. Boundaries
follow exact pixel edges, without overviews or simplification. Pixels connect through their sides
(4-connectedness); regions touching only at a corner stay separate polygon members, as in GDAL's
default footprint. An entirely invalid raster
produces an empty layer. By default, any band may be valid; `--intersection` requires all selected
bands to be valid. `--band` selects bands from 1; omitted, it uses all bands.

The output includes a text `location` field containing the input path. Local paths are stored exactly as supplied;
URL sources (inputs containing `://`, including GDAL virtual-path prefixes) omit
user information, query strings and fragments.
Use `--location-field NAME` to rename it or `--no-location` to omit it. Repeated
`--attribute NAME=VALUE` options add text fields to every feature; values may be empty or
contain `=`. Field names must be nonempty, have no surrounding whitespace, and be unique
ignoring case, including the location field. Output format limits still apply (Shapefile field names are limited to 10 characters
and text values are truncated to at most 254 bytes, at a UTF-8 boundary); use
GeoPackage to retain long names or values.

`--singlepart` writes one polygon feature per disconnected region, keeping holes, CRS and
all attributes on each feature. Without it, the output remains a single multipolygon.

`--mask` further restricts the footprint to finite, nonzero, non-nodata pixels of the mask's first
band; its grid and CRS must match the input. Alpha bands and external mask files are not used as
implicit masks. Complex bands and geolocation arrays are unsupported.

## sample

```sh
terrio sample sites.gpkg samples.gpkg --raster dem.tif --raster rain.tif
terrio sample sites.gpkg samples.gpkg --raster image.tif --band 1,2,3 --prefix image
terrio point-sample sites.gpkg samples.gpkg --raster dem.tif --raster rain.tif \
  --t-srs EPSG:3857 --decimal-places 2
```

`sample` (alias `point-sample`) is the multi-raster point sampling tool. It samples the pixel
containing each point, adding a Float field for each raster/band pair. Repeat
`--raster` for multiple rasters; `--band` selects the same bands (from 1, default 1) from each.
Fields are named `FILE_STEM_bN`, or `PREFIX_bN` when one `--prefix` is supplied per raster in the
same order. Name collisions get `_2`, `_3`, etc.; existing attributes remain intact.

Each raster may have a different resolution, affine grid or CRS. Coordinates are transformed
for sampling while the output retains the original point geometries, Z/M, attributes and
CRS by default. `--t-srs CRS` reprojects the output points after sampling; it requires a declared
input CRS. Output reprojection failure is an error. `--decimal-places N` rounds newly sampled
values to 0–15 decimal places, with halfway values rounded away from zero. Existing attributes
are unchanged. Values remain numeric floats; output formats control trailing-zero display.
Rounding uses binary64 scaling, so decimal values very close to a halfway boundary may
fall on either side because of floating-point representation. Coordinates retain full precision.
Nodata, non-finite pixels, out-of-bounds locations and
null geometries yield null values.
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
terrio zonal-stats counties.gpkg stats.gpkg --raster dem.tif \
  --t-srs EPSG:3857 --decimal-places 2
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
unchanged by default. All non-null geometries must be polygons or multipolygons.

`--t-srs CRS` reprojects output zones after statistics are computed, so it does not change
pixel membership or the analytical grid. The input must declare a CRS, and output reprojection
failure is an error. `--decimal-places N` rounds only newly calculated floating statistics to
0–15 decimal places (halfway values away from zero); integer counts and original attributes
are unchanged. This controls numeric values, not coordinate precision or trailing-zero display.
Rounding uses the same binary64 scaling as `sample`; decimal values near halfway boundaries
are subject to floating-point representation. For point inputs, use `sample`/`point-sample`, which accepts multiple rasters in one command.

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

## Vector datasets

These commands are GDAL 3.13's `gdal vector concat`, `partition`, `set-field-type`,
`make-point`, `create`, `export-schema`, `rename-layer`, `edit` and `update`. Their results are compared with GDAL in the
`vector-dataset` conformance suite (see [Testing](testing.md)).

### concat

```sh
terrio concat countries.gpkg us_counties.gpkg merged.gpkg                  # one layer per layer name
terrio concat a.shp b.shp c.shp all.gpkg --mode single --source-layer-field-name source
terrio concat *.gpkg stacked.gpkg --mode stack --output-layer "{DS_BASENAME}_{LAYER_NAME}"
terrio concat a.gpkg b.geojson out.gpkg --mode single --field-strategy intersection --dst-crs EPSG:3857
```

`--mode merge-per-layer-name` (the default) makes one output layer per distinct layer name,
`stack` one per input layer (named by the `--output-layer` template, `{AUTO_NAME}` by
default: `{DS_BASENAME}_{LAYER_NAME}`, or the layer name when both are the same), and
`single` one layer (`merged` by default). Templates also take `{DS_NAME}`, `{DS_INDEX}` and
`{LAYER_INDEX}` (from 0). Output layers are ordered by name, as GDAL orders them.
The generated source-layer field must have an unused name; collisions with source fields
are rejected before creating an output (GDAL's writer may instead rename a colliding field).
`--field-strategy union` (the default) keeps every field, matched by exact name;
`intersection` the fields every layer has, ignoring case. Fields of the same name merge into a
type holding both (reals over integers, 64-bit over 32-bit integers, strings otherwise).
Union fields such as `Name` and `name` remain distinct; formats requiring case-insensitive
unique column names may reject that schema (GDAL may rename columns when writing).
`--source-layer-field-name`/`--source-layer-field-content` add a first field naming each
feature's source (`source_ds_lyr` and `{AUTO_NAME}` by default). Each output layer has the
geometry type and CRS of its first source layer; features of layers in another CRS are
reprojected to it, or to `--dst-crs`. `-s/--src-crs` overrides the inputs' CRS. `-l` selects
input layers; `--append`/`--overwrite` update an existing GeoPackage as elsewhere. GDAL's
`--upsert`, `--skip-errors`, `--if`, `--oo`, `--co` and `--lco` are not supported.

### partition

```sh
terrio partition counties.gpkg out --field STATE                         # out/counties/STATE=TN/part_0000000001.gpkg
terrio partition places.shp out --field country --field city --feature-limit 1000 -f GPKG
terrio partition places.shp flat --field featurecla --scheme flat --pattern "{FIELD_VALUE}_%03d" -f FlatGeobuf
terrio partition big.parquet out --max-file-size 500MB                     # parts of an estimated size
```

`--scheme hive` (the default) writes `layer/field=value/.../part_%010d.ext` directories, with
null values as `__HIVE_DEFAULT_PARTITION__`; `flat` writes the files in the output directory,
named `{LAYER_NAME}_{FIELD_VALUE}_%010d` (several values joined by `_`, nulls as `__NULL__`,
empty strings as `__EMPTY__`). `--pattern` sets the name, with `{LAYER_NAME}`, `{FIELD_VALUE}`
and one `%d`, `%5d` or `%05d` part number. Names and values are percent-encoded as GDAL encodes
them. Fields must be String or Integer fields; `OGR_GEOMETRY` partitions on the geometry type
(as GDAL 3.13 does). `--feature-limit` and `--max-file-size` split partitions into numbered
parts; the size estimate begins at 64 KiB plus the bytes each feature adds, and the minimum
size limit is 1 MiB. Without `--field`, they split the layer alone (GDAL 3.13). `--omit-partitioned-field` leaves the partitioning fields out
(automatic for Hive GeoParquet). The format defaults to the input's; files take GDAL's
extension of the format (`.json` for GeoJSON). An existing output is refused unless
`--overwrite` (only when it looks like a partitioned dataset) or `--append` (new parts are
numbered after the existing ones; GDAL also fills the last existing part while it is below the
limits) is given. GDAL's `--skip-errors`, `--co` and `--lco` are not supported, nor GDAL's
Parquet `_metadata` file.

Partition output directories carry a `.terrio-partition` ownership marker. Overwrite
validates options and every descendant entry before removal, refuses a directory containing
the path input, and requires a matching marker; unmarked directories must be removed
explicitly. Flat output accepts only the selected format's files and sidecars. Empty and
dot-only names are escaped. The Rust `partition_to_dir` API receives materialized layers,
so callers must keep any source files outside an overwrite target; path-based CLI/Python
calls check that relation. Partition and concat currently materialize feature copies;
large datasets can require roughly input plus output memory.

### set-field-type

```sh
terrio set-field-type countries.gpkg out.gpkg --field-name POP_EST --dst-field-type Integer64
terrio set-field-type countries.gpkg out.gpkg --src-field-type Real --dst-field-type String
terrio set-field-type places.csv out.gpkg --field-name visited --dst-field-type Date --active-layer places
```

`--dst-field-type` (alias `--field-type`) is a type (`Integer`, `Integer64`, `Real`,
`String`, `Date`, `Time`, `DateTime`, `Binary`) or a subtype (`Boolean`, `Int16`, `Float32`,
`JSON`, `UUID`) implying its type; `--field-name` names the field, or `--src-field-type` changes
every field of a type. Values are converted as GDAL converts them: strings by their numeric
prefix (`"12abc"` is 12, `"abc"` 0, with warnings), reals truncated and clamped to 32 or 64
bits, 64-bit integers clamped to 32 bits, booleans from `true`/`yes`/`on`/numbers, dates and
date-times reformatted (dates become `YYYY/MM/DD` strings), text parsed as dates or times.
Features whose value cannot be converted (a date or blob to a number, a date to a blob) are left
out, with a warning, as in GDAL. Widths and precisions are kept. terrio's data model has no
separate Int16, Float32 or UUID storage: Int16 values are clamped to 16 bits in 64-bit
integers, Float32 values are doubles, UUIDs text, times text (`HH:MM:SS[.sss]`), and an
Integer field changed to Boolean holds 0 or 1 where GDAL keeps the integer. List types are not
supported.

Numeric-prefix conversion of unusually long text values currently involves repeated parsing
and can be expensive.

### make-point

```sh
terrio make-point trace.csv points.gpkg --x longitude --y latitude --dst-crs EPSG:4326
terrio make-point stations.gpkg points.gpkg --x easting --y northing --z elevation -l stations
```

Replaces the geometries of the first layer (or `-l`) with points read from numeric or text
fields (text must hold a number, surrounding spaces allowed; null numbers are 0, as in GDAL),
keeping every field. `--z` and `--m` add Z and M. `--dst-crs` assigns a CRS without
reprojecting; without it the points have none, as in GDAL. Unlike GDAL, Integer64 fields are
accepted. terrio's GeoPackage writer writes XY and XYZ geometries, so M values are kept in
memory (Rust, Python and JavaScript) but not in GeoPackages.

Text coordinates accept NaN and infinity as GDAL 3.13.3 does; use finite coordinates for
valid extents and spatial indexes. GDAL can report an extent-update error for infinite
GeoPackage coordinates even while its command exits successfully.

### create

```sh
terrio create sites.gpkg --output-layer sites --geometry-type POINT --crs EPSG:4326 \
  --field "name:STRING(80)" --field visits:INTEGER --field total:INTEGER64 --fid site_id
terrio create empty.gpkg --like countries.gpkg --layer countries --output-layer countries_new
terrio create copy.gpkg --schema countries.schema.json
terrio create sites.gpkg --update --output-layer more --geometry-type LINESTRING --crs EPSG:4326
```

Creates a dataset of empty layers from an `OGR_SCHEMA` document (`--schema`, a file or JSON
text, such as `terrio export-schema` writes), from the layers of a template dataset
(`--like`, `-l` choosing layers), or from `--geometry-type`, `--crs`, `--field
NAME:TYPE[(WIDTH[,PRECISION])]` and `--fid`. `--output-layer` names a single layer.
`--update` adds the layers to an existing GeoPackage (a layer of the same name is refused);
`--overwrite` replaces an existing dataset. `--fid` names the FID column of new GeoPackages.
Field types are mapped onto terrio's (see set-field-type); 32-bit `INTEGER(10)` fields keep no
width of 10 or more, which marks 64-bit integers. Z and M suffixes of geometry types are
accepted but not recorded in empty layers (a warning says so). Curve, surface, TIN and list
types, `--geometry-field` and GDAL's `--oo` are not supported.

Vector schema/template creation orders fields by name, matching GDAL 3.13; explicit
`--field` arguments keep their order.

### export-schema

```sh
terrio export-schema countries.gpkg
terrio export-schema natural_earth.gpkg --layer ne_50m_admin_0_countries -o countries.json
```

Prints the dataset's `OGR_SCHEMA` (GDAL's `ogr/data/ogr_fields_override.schema.json`), as
`gdal vector export-schema` does: per layer its name, `"schemaType": "Full"`, geometry fields
(name, camel-case type with `Z`/`M`, nullability, `coordinateSystem` as `{"authid":
"EPSG:4326"}`, or `{"wkt": ...}` without an authority code, `null` without a CRS), the FID
column and the fields (name, type, subtype, width, precision, nullability, unique constraint,
default). Local GeoPackages are described from their table declarations (exact column types,
`NOT NULL`, `UNIQUE` and defaults); other formats from the layers read (integer fields are
`Integer64` when 10 or more digits wide or holding 64-bit values, as GDAL's DBF reader and
GeoJSON reader type them).

### rename-layer

```sh
terrio rename-layer data.gpkg --input-layer roads --output-layer transport
terrio rename-layer source.gpkg renamed.gpkg --ascii --lower-case
```

Renames a chosen layer or normalizes every layer name. `--ascii`, `--lower-case`,
`--filename-compatible`, `--reserved-characters`, `--replacement-character` and
`--max-length` control normalization; colliding results receive unique suffixes.
A selected rename retains the other input layers; name collisions are rejected clearly.
GDAL's CLI `--input-layer` selection can instead produce only the selected layer.
Without an output, local GeoPackages are changed in place, including their indexes,
triggers and metadata references. Other formats require an output.

### edit

```sh
terrio edit data.gpkg --crs EPSG:4326 --metadata SOURCE=survey
terrio edit data.gpkg edited.gpkg --active-layer roads --geometry-type LINESTRING
```

Assigns CRS without reprojecting, changes declared geometry type (including Z/M), or
sets/removes dataset and layer metadata. Metadata edits and Z/M declarations require
GeoPackage outputs; other output formats reject those options. `--active-layer` selects one
layer, matching exact names first and otherwise ignoring ASCII case. Default CRS and
geometry edits skip attribute-only tables. `--crs none` removes a CRS. `--unset-fid`
requires an output. Geometries are not converted by a declaration change. An output copy
retains default-domain GeoPackage metadata; named domains are not copied, matching GDAL 3.13.3.

In-place GeoPackage edits materialize the database and replace it with a unique temporary
file beside the resolved input. Mode bits and symlink targets are preserved; hard links
are detached, and ownership, ACLs and extended attributes are not preserved. Exclude
concurrent writers: journal/WAL checks are not transaction locks. Atomic replacement
is not a power-loss durability guarantee. Output creation and metadata application are
separate steps, so a failed output edit can leave a file.

### update

```sh
terrio update incoming.gpkg existing.gpkg --key code --mode merge
```

Matches features by FID, or by repeated `--key` fields of matching types. `--input-layer`
and `--output-layer` select layers. `merge` updates matched features and appends unmatched
ones; `update-only` and `append-only` restrict that behavior. NULL keys match nothing, so
merge and append-only append them, while update-only skips them. A key matching several
output features matches none. Shared fields and geometry are updated without reprojection;
output-only fields are retained and input-only fields are not added.

GeoPackages use the in-place mechanism above. Other formats stage new files then replace
the originals. Multi-file replacements are not atomic across sidecars and a failure may
leave a partial replacement. Successful Shapefile replacements remove obsolete QIX/SBN/SBX
indexes and optional PRJ/CPG sidecars not produced by the new write.

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

GeoTIFF/COG WebP output accepts `--compress WEBP --co WEBP_LOSSLESS=YES` or
`--co WEBP_LEVEL=0..100` for lossy quality. Supported inputs are Byte RGB/RGBA;
alpha is retained losslessly. COG output accepts
`--co COG_OVERVIEW_POLICY=GDAL_3_13` for recursive, floor-sized overviews with
`--co RESAMPLING=NEAREST|AVERAGE` and nodata, without alpha or explicit masks. The default
`SOURCE` policy reduces each overview directly from source samples.
`QUALITY` aliases WebP quality (0–100) or JPEG quality (1–100).
`JPEG_QUALITY` always accepts 1–100. For compatibility, quality keys are validated
even when an unrelated codec ignores them. Invalid `WEBP_LOSSLESS` values are
rejected; accepted booleans are `YES/NO`, `TRUE/FALSE`, `ON/OFF`, and `1/0`.

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
terrio mosaic @filelist.txt OUTPUT [options]    # inputs listed in a file, one per line
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
value) or a percentage of the band's range; GMT `.cpt` files work too.

`--kernel-version 3.12|3.13` selects terrain arithmetic (default `3.12`). The
`3.13` profile supports slope and standard hillshade, including both gradient
algorithms. Other products and hillshade variants require `3.12`. Use the version
matching the GDAL reference; transcendental and SIMD rounding can vary by platform.

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

## compare

```sh
terrio compare reference.tif output.tif            # exit status: the number of differences
terrio compare reference.tif output.tif --skip-binary --json
terrio compare golden.tif new.tif --skip-all-optional
```

Compares two rasters as `gdal raster compare` (and `gdalcompare.py`) do, and prints GDAL's
report: one line per difference, worded and ordered as GDAL's, and exits with the number of
lines (capped at 255). The first raster is the reference. Identical files have no difference
and nothing else is compared; otherwise the report covers the file sizes and bytes, the
subdataset count, CRS, geotransform (relative tolerance 1e-10), band count and size, the
dataset's default, `RPC` and `GEOLOCATION` metadata, then per band its size, description, data
type, nodata value, color interpretation, pixels, overviews (each compared as a band), mask
flags and per-dataset mask (compared as a band), and band metadata; then each subdataset.
Metadata includes `.aux.xml` (PAM) items, GeoTIFF `TIFFTAG_*` items and RPC tags, as GDAL
reports them. Differing pixels add two lines per band: `1: pixels differing: N` and
`1: maximum pixel value difference: X`, computed in the union of both types (GDAL's
`GDALDataTypeUnion`); a NaN in one band only differs, NaNs in both do not.

`--skip-all-optional`, `--skip-binary`, `--skip-crs`, `--skip-geotransform`,
`--skip-overview`, `--skip-metadata`, `--skip-rpc`, `--skip-geolocation` and
`--skip-subdataset` skip what GDAL's options skip; sizes, band counts, descriptions, types,
nodata, color interpretations and pixels are always compared. `--json` prints
`{difference_count, differences: [{kind, band, subdataset, message}], bands: [{band,
data_type, pixels, pixels_differing, max_abs_diff}]}`, with every band, overview and mask
compared, including those without differences.

Rasters are read lazily: strips of rows by window (of every band at once), overviews from
their own levels, and the binary comparison compares sizes first and then reads a megabyte at a
time. Comparing two copies of a 51 MB USGS GeoTIFF served over HTTP with `--skip-binary
--skip-overview` fetched 76.5 MB in 10 requests; the binary comparison of two identical copies
read both files (103 MB). The MCP tool reports query statuses as results, not execution failures.
On the command line, status 1 can mean either one difference or an execution error;
status 2 can mean two differences or a command-line usage error. Check stderr and the
report for errors. `pixel-info` similarly uses status 1 for off-raster positions.
`--skip-all-optional` takes precedence over individual `--skip-*` flags.

Compared with GDAL 3.13.3 in the `raster-query` conformance suite: 20 of 21 cases give the
same lines and exit status. One known mask-count reporting difference is described below.

Differences: CRS mismatch lines print Terrio's WKT where GDAL
prints WKT2_2019; GDAL's metadata value line swaps the two values (`Reference ... has value
<input's>`), Terrio names them in reference, input order. Not supported: complex bands,
`--input-format`/`--open-option`, pipelines, and GDAL's per-dataset comparison of more than
10 pixel-interleaved bands (whose lines read `Band 1: ...`); `RPC` comes from GeoTIFF tags
only (not `_RPC.TXT`). Terrio opens a multi-variable netCDF/HDF5 container as its first
variable, so the comparison of two containers differs from GDAL's empty container datasets;
compare their subdatasets.

With GDAL 3.13.3, the internal-mask comparison reports 1,863,354 differing pixels
per band on the Landsat fixture, while direct GDAL mask reads and Terrio both count
2,568,634. Terrio retains the direct pixel count. The full 100-case rerun has 96
identical cases, three within the documented numeric tolerances, and this one
mask-report difference.

## pixel-info

`-o -` writes text formats to standard output; binary/vector formats require a file path.

```sh
terrio pixel-info dem.tif 100 200                          # column, line; GeoJSON
terrio pixel-info --position-crs WGS84 -r bilinear dem.tif -105.5 39.5
echo "-105.5 39.5 site A" | terrio pixel-info -f text --position-crs EPSG:4326 dem.tif
terrio pixel-info --position-dataset sites.gpkg -f csv dem.tif
terrio pixel-info --position-dataset sites.gpkg dem.tif -o values.gpkg
```

Pixel values at positions, as `gdal raster pixel-info` and `gdallocationinfo` (`locationinfo`
is an alias). Positions are pairs of arguments (`X Y` or `X,Y`), `-p X,Y`, lines of standard
input (an `X Y` pair, then any text, reported as extra content; GDAL's tokenizing, quotes
kept together) or the points of a vector dataset with `--position-dataset` (GDAL 3.13; centroids
of other geometries, `-l` for the layer, `--include-field NAME|ALL|NONE`). `--position-crs` is
`pixel` (column and line, the default), `dataset` (the raster's CRS) or any CRS, X being the
longitude or easting; with `--position-dataset` it defaults to the layer's CRS. `-b` selects
bands, `--overview N` (from 0) queries an overview with full-resolution positions, and
`-r nearest|bilinear|cubic|cubicspline` interpolates as GDAL's `GDALRasterInterpolateAtPoint`:
pixel centres at half-integers, kernels shifted inside at edges, no value when a bilinear kernel
touches nodata, nodata skipped by cubic kernels. Values are reported raw and with the band's
scale and offset applied.

`-f` selects the output: `geojson` (the default: `gdal raster pixel-info`'s feature collection,
points in the raster's CRS when it has an EPSG code, else in WGS 84;
`--promote-pixel-value-to-z` puts the value as Z), `csv` (GDAL 3.13's columns `geom_x`,
`geom_y`, the layer's fields or `extra_content`, `column`, `line`, `band_N_raw_value`,
`band_N_unscaled_value`, numbers as `%.15g`), `text` (`gdallocationinfo`'s report),
`value` (`gdallocationinfo -valonly`, with `--field-sep`) and `json` (a list of
`{input, extra, column, line, xy, bands: [{band, value, unscaled}]}`). With `text` and `value`,
the exit status is 1 when a position is off the raster, as `gdallocationinfo`'s. `-o` writes
the GeoJSON, CSV or a vector layer of points (any vector format) to a file. Vector-file output records
missing/off-raster samples as null and returns normal write success.

Only the 64 x 64 pixel blocks around the positions are read: two cubic queries of a 51 MB
USGS GeoTIFF served over HTTP fetched 1.8 MB in 3 requests.

Compared with GDAL 3.13.3 (`gdal raster pixel-info` and `gdallocationinfo`) in the
`raster-query` conformance suite: of 67 cases, 64 identical (GeoJSON and CSV values parsed;
text and `-valonly` byte for byte) and 3 within tolerance: positions in WGS 84 on a custom Albers
raster (column, line and geometry within 4e-14 relative, from Terrio's projection arithmetic)
and one scale/offset product one ULP from GDAL's build. Formatting differences: GeoJSON
whitespace (values are identical); CSV follows GDAL 3.13 (3.12's columns were `input_x`,
`input_y`, `extra_input` with 17 significant digits). Not supported: complex bands, VRT
`LocationInfo` (source files), GDAL pipelines, `-xml`, `-lifonly` and `-E`.

Pixel-info edge cases follow GDAL 3.13.3: a position layer without a CRS uses pixel
coordinates; interpolated integer bands truncate GeoJSON `raw_value` while retaining
fractional `unscaled_value`. Copied attributes can overwrite GeoJSON `column`, `line`,
`input_coordinate` and `extra_content`, and CSV can repeat those names. Rename those
input fields or select them out to retain computed properties. Vector-file output rejects
field names colliding with computed columns rather than writing an ambiguous schema.

The diagnostic `json` format uses JSON `null` for non-finite numbers; GDAL-compatible
GeoJSON can emit its non-standard `NaN`/`Infinity` tokens. Text prints non-finite locations
as `nan`/`inf`, not as pixel zero. A source-CRS transformation failure aborts the query,
matching GDAL 3.13.3; it is distinct from an ordinary off-raster position.

## xyz

```sh
terrio xyz dem.tif dem.xyz
terrio xyz landsat.tif --allbands --csv --skipnodata > landsat.csv
terrio xyz dem.tif --srcwin 100 200 50 50 --skip 2 --dstnodata -9999
```

Writes a line per pixel with the X and Y of its centre and the band values, as `gdal2xyz.py`:
coordinates as `%.10g` when the raster's origin and extent are within 180 units, `%.3f`
otherwise, values as `%d` when the first band is Int32 or UInt32, else `%g`, converted to the
first band's type. `-b` (band 1 by default; repeat or comma-separate), `--allbands`,
`--skip N` or `--skip X,Y`, `--srcwin XOFF YOFF XSIZE YSIZE`, `--skipnodata`,
`--srcnodata` and `--dstnodata` (one value, or one per band), `--csv` and `--delimiter`. Output
goes to the file given, or standard output. The raster is read and the text written a strip of
rows at a time.

Compared with GDAL 3.13.3's `gdal2xyz.py` in the `raster-query` conformance suite: 11 of 11
text cases are byte-identical; an additional mixed-type non-finite integer nodata case
is rejected by both tools before sample data is written. Difference: `--skipnodata` and `--dstnodata` match NaN nodata values,
which `gdal2xyz.py` never matches (so it keeps those lines). `gdal2xyz.py`'s `-skip` takes no
value on its 3.12 command line; Terrio's `--skip N` is its function's `skip`. Not supported:
returning NumPy arrays (use `Raster.read`).

XYZ rejects non-finite source nodata when the first selected band's output type is integer,
as GDAL's NumPy conversion does.

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

## neighbors


```sh
terrio neighbors INPUT OUTPUT --kernel KERNEL [--kernel ...] [--method METHOD ...] [--size N]
                 [-b BAND] [--ot TYPE] [--nodata VALUE|none] [output options]
terrio neighbors dem.tif max5.tif --kernel equal --size 5 --method max
terrio neighbors image.tif edges.tif --kernel gaussian --kernel edge2 -b 2 --ot Float32
```

Focal statistics, as GDAL 3.12's `gdal raster neighbors`. Kernels: `u` and `v` (horizontal
and vertical derivatives), `edge1`, `edge2`, `sharpen`, `gaussian` (3x3 or 5x5),
`unsharp-masking` (5x5), `equal` (any odd size from 3 to 99), or a matrix written row by row,
`[[0,1,0],[1,1,1],[0,1,0]]`. Explicit matrices accept any positive odd square size,
as GDAL 3.13.3 does; the 99 limit applies to `--size`, not matrix dimensions. Filtering
work grows with pixels times coefficients, and strip margins grow with the kernel radius.
`--method` combines the weighted neighbours: `sum`, `mean`, `min`,
`max`, `stddev`, `median` or `mode`; give one for every kernel or one per kernel. The default is
`sum` for kernels whose coefficients sum to zero (`u`, `v`, `edge1`, `edge2`, zero-sum
matrices) and `mean` otherwise. Each input band (or the `-b` band) gives one output band per
kernel. The output type defaults to Float64 and the output nodata to the input band's.

Values follow GDAL's `VRTKernelFilteredSource` exactly: inputs are read as Float32, nodata and
NaN neighbours (with a nodata value) and zero coefficients are skipped, finite nodata
centres keep the input nodata value, even when `--nodata` changes the output tag
or removes it with `none`. NaN nodata centres are interpolated, matching GDAL 3.13.3;
pixels past the raster's edges repeat the edge pixels, and results are Float32 values
converted to the output type. `mode` breaks ties for the first value to reach the highest count,
in row order. The input is read and GeoTIFF/COG outputs are written a strip of rows at a time,
with the kernel's margin rows. The real-data comparisons with GDAL are identical
(`scripts/compare_focal_with_gdal.py`). GDAL's GDALG/VRT pipeline output and `--append` are not
supported.

## clean-collar and nearblack

```sh
terrio clean-collar INPUT [OUTPUT] [--update] [--color black|white|R,G,B ...] [--white]
                    [--color-threshold N] [--pixel-distance N] [--add-alpha | --add-mask]
                    [--algorithm floodfill|twopasses] [output options]
terrio clean-collar scene.tif clean.tif --add-alpha
terrio nearblack scene.tif clean.tif --nb 0 --near 10 --color 0,0,0 --color 255,255,255
terrio clean-collar --update scene.tif
```

Sets the noisy, nearly black (or white, or `--color`) collar of an image to exact black (or
white, with `--white`), as GDAL's `gdal raster clean-collar` and `nearblack` do. Pixels within
`--color-threshold` (`-near`, 15) of a color in every non-alpha band are collar pixels; the
search inwards goes past up to `--pixel-distance` (`-nb`, 2) non-collar pixels. `--add-alpha`
adds an alpha band (transparent in the collar) and `--add-mask` a per-dataset mask (GeoTIFF:
internal). `clean-collar` runs a flood fill from the edges by default and retains the input's
alpha/mask representation; requested or retained alpha/mask processing regenerates
opacity from the collar rather than intersecting it with the old alpha/mask. `nearblack` runs nearblack's two passes (top-down then bottom-up, each scanning
lines from both sides) by default and adds alpha or masks only when asked. Both algorithms are
ports of GDAL's, in GDAL's scan order, and agree pixel for pixel with GDAL on the real
Sentinel-2 and Landsat 7 comparisons. Output is a new Byte raster with the input's
georeferencing and no nodata value; values are read as Byte (rounded and clamped).

`--update` (or an output equal to the input) updates a local GeoTIFF in place: its bands and
type are kept, with `--add-alpha` its last alpha band is updated (it must have one), and an
existing mask is kept unless mask processing regenerates it from the collar. The result is written next to the input, then replaces it, so a failure
leaves the input as it was; remote files, other formats and read-only files are refused. The
file is rewritten by Terrio's writer: overviews are dropped, and compression and tiling are those
of the output options. GDAL 3.12's in-place flood fill of a raster with an alpha band writes stale
alpha values on every modified line (2,715,128 alpha values of the Landsat 7 case become 0);
Terrio's alpha band matches GDAL's output to a new file instead. Images are processed in memory
as Byte values (one byte per band and pixel); the flood fill visits lines out of order. New-file output holds an interleaved
working image and a planar output image at once (about two Byte output buffers,
plus input data and any mask); this keeps the GDAL scan order and contiguous scanlines.
In-place updates follow input symlinks and retain mode bits. Replacement detaches
hard links and does not preserve ownership, ACLs or extended attributes.

`nearblack --update` and `clean-collar --update` follow GDAL's Byte processing:
non-Byte samples across the image are rounded/clamped to 0–255 before being written
back to the original data type. Terrio emits a warning for these inputs.
For RGBA sources, `nearblack --color black` compares four bands unless `--add-alpha`
or `--add-mask` selects alpha/mask processing; `clean-collar` keeps existing alpha.
With mask processing on RGBA, new-file output drops the alpha band, while in-place
output retains it, matching GDAL 3.13.3.

## enhance

```sh
terrio enhance INPUT OUTPUT --equalize
terrio enhance INPUT --equalize --config lut.txt     # write the tables
terrio enhance INPUT OUTPUT --config lut.txt          # apply tables
terrio enhance INPUT OUTPUT --src-scale 0 4000 [--lut-bins 256] [--ot Byte]
```

Contrast enhancement by lookup tables, as GDAL's `gdalenhance`. `--equalize` computes
histogram-equalization tables from each band's default histogram (`gdalinfo -hist`'s: stored
histograms and GDAL's approximate minimum and maximum included), without its first and last
buckets. Without an output, the tables are written to `--config` (or printed) in
`gdalenhance`'s format: a line per band, `1:Band -0.5:ScaleMin 255.5:ScaleMax` and the 256
entries. Without `--equalize`, `--config` reads such a file. A pixel's bin is
`(value - ScaleMin) * bins / (ScaleMax - ScaleMin)`, truncated and clamped; nodata pixels keep
their value after conversion to the output type. NaN nodata uses the first LUT bin, matching GDAL 3.13.3. The output keeps the georeferencing, metadata, band descriptions and color
interpretations, and the nodata value of bands of the output type; it is read and written in
strips (equalization reads the bands whole for their histograms). Equalization, LUT files and
LUT input are identical to `gdalenhance` on the real comparisons. Terrio extensions:
`--src-scale MIN MAX` (a linear stretch over the bins, without tables; `gdalenhance` parses
`-src_scale` but ignores it), `--lut-bins` (256 in GDAL) and `--ot` other than Byte (`gdalenhance`
refuses it). Byte output casts LUT results modulo 256, matching GDAL (for example,
a table value of 300 becomes 44). Byte nodata also truncates and wraps through a signed
32-bit integer (for example, -9999 becomes 241); values outside that intermediate range
and infinities become zero. This defines the behavior observed in the GDAL 3.13.3
reference build; C++ out-of-range conversion can vary by build. Non-Byte outputs preserve
exact source nodata before output conversion, including Float64 precision.
Select a wider `--ot` to retain larger LUT values.
Unlike `gdalenhance`, Terrio does not save the histograms it computes in an
`.aux.xml` sidecar.

## crs

```sh
terrio crs CRS [--wkt] [--projjson]
terrio crs SRC --to DST [--s_coord_epoch YEAR] [--t_coord_epoch YEAR] X Y [X Y ...]
echo "X Y [Z [T]]" | terrio crs SRC --to DST [--s_coord_epoch YEAR] [--t_coord_epoch YEAR]
```

As `gdaltransform`: lines of standard input with a height transform it too, with a time (a decimal
year) use it for time-dependent transformations, and a point that cannot be transformed prints
`transformation failed.`. Datum shift grids and geoid models are used when they can be found (see
[Coordinate Systems](crs.md#datum-shift-grids)). `--projjson` prints only the CRS as PROJJSON, so that
`terrio crs CRS --projjson > crs.json` writes a PROJJSON file (see
[Writing PROJJSON](crs.md#writing-projjson)); it cannot be combined with `--to` or `--wkt`.

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
[Point Clouds](point-clouds.md). For vector points, see [vector-grid](#vector-grid).

## vector-grid

```sh
terrio vector-grid invdist stations.gpkg temp.tif --zfield temp --power 2 --radius 50000 --max-points 12
terrio vector-grid linear samples.gpkg dem.tif --extent 636000 848930 637180 849500 --resolution 5 5
terrio vector-grid count places.shp density.tif --zfield pop_max --radius 3 --ot UInt16
terrio vector-grid "invdistnn:radius=4:max_points=8" places.shp idw.tif --zfield pop_max --where "adm0name = 'Peru'"
```

Grids scattered vector points into a raster, as GDAL's `gdal vector grid` and `gdal_grid` do.
It is a separate command from [`grid`](#grid), which grids point clouds the way PDAL's
`writers.gdal` does. Every vertex of every geometry is a point (lines' and polygons' too), with
the value of `--zfield` or its Z (0 for 2D points); `--zoffset` is added, then `--zmultiply`
multiplies. The algorithm is the first argument, or a GDAL algorithm string
(`invdist:power=3:radius1=500:radius2=300`) whose parameters the options override:

| Algorithm | GDAL | Parameters |
|---|---|---|
| `invdist` (default) | inverse distance to a power | `--power` (2), `--smoothing`, `--radius` or `--radius1`/`--radius2`, `--angle`, `--min-points`, `--max-points` (with a radius), per-quadrant limits (then `invdistnn`) |
| `invdistnn` | inverse distance of the nearest points | `--power`, `--smoothing`, `--radius` (1), `--min-points`, `--max-points` (12), per-quadrant limits |
| `average` | moving average | ellipse, `--angle`, `--min-points`, per-quadrant limits, `--max-points` (with them) |
| `nearest` | nearest neighbour | ellipse, `--angle` |
| `linear` | linear in the Delaunay triangulation | `--radius`: the nearest point outside it within this distance; 0 for the nodata value; by default any distance |
| `minimum`, `maximum`, `range`, `count`, `average-distance`, `average-distance-points` | data metrics | ellipse, `--angle`, `--min-points`, per-quadrant limits (not `average-distance-points`) |

The per-quadrant limits are `--min-points-per-quadrant` and `--max-points-per-quadrant`.
`--nodata` (0 by default) fills nodes without enough points; when it is given (here or in the
string), it is declared as the bands' nodata value, as `gdal_grid` does. Nodes are the cell
centres. The grid is `--extent` (by default the layer's, before `--where` and `--bbox`, as
Shapefile and GeoPackage report it), `--size` (256 × 256 by default) or `--resolution` (with
`--extent`: `(|max - min| + res / 2) / res` cells, the cell size then the extent over that).
`--layer` grids several layers into a band each; `--sql` grids a query result; `--bbox` keeps
the points strictly inside a rectangle (`-clipsrc`). `--ot` sets the type (Float64 by default),
`--crs` the CRS (by default the layer's; points are not reprojected).

NaN geometry Z values are skipped; NaN values supplied through `--zfield` can
propagate into the result, matching GDAL 3.13.3. Input layers requested by name must
all exist. Native path-based GeoPackage selections are case-insensitive; in-memory and JavaScript
byte/Dataset selections use exact names. Validating a file-stem alias can reread the source to confirm that it originally
has only one layer; use the actual layer name to avoid that extra read on large inputs.

Without a search radius, `invdist` visits all input points for each output cell;
its work grows with cells times points. Output allocation is bounded, but runtime
is not bounded by a separate work budget. Use a smaller grid or an appropriate
search radius for large inputs.

`average-distance-points` evaluates every pair in each node's search neighborhood;
work is quadratic in that neighborhood's point count. Float32 and other typed outputs are converted from
an f64 working grid, so both buffers can coexist (about 768 MiB for f64 plus Float32 at
the 2^26-cell limit, before inputs and indexes). Limit the search radius for
dense inputs.

The algorithms are ported from GDAL 3.13's `gdalgrid.cpp`: the search ellipse and its
rotation, GDAL's switch from scanning every point to searching its quadtree above 100 points,
and the order the quadtree returns points in (which decides ties and the rounding of sums) are
reproduced. With one zero ellipse semi-axis, the scan accepts an unbounded line through
the node while the quadtree search clips its candidates to a square of half-width equal
to the other semi-axis; Terrio preserves this GDAL 3.13.3 behavior across the threshold.
Unbounded `average`, `minimum`, `maximum`, `range` and `count` compute their constant
value once per output block. Many cases agree exactly; linear triangulation and SIMD arithmetic have the
tolerances and known differences reported in
[Testing](testing.md#vector-gridding-and-linear-referencing-against-gdal). Known differences:

- GDAL computes the default `invdist` (power 2, no smoothing, no radius) with AVX or SSE, in
  single precision with approximate reciprocals, unless `GDAL_USE_AVX=NO` and
  `GDAL_USE_SSE=NO`; terrio computes in double precision, as GDAL's scalar code.
- Grids are materialized in memory and limited to 67,108,864 cells across all bands
  (512 MiB of Float64 values before conversion); allocation failure returns an error.
- `linear` triangulates with spade rather than qhull. The triangles of scattered points are
  the same, but their vertex order (and so the last bits of the interpolation) can differ, and
  the ties of co-circular points (a regular grid's squares) are broken differently.
- GDAL 3.12 scanned every point for a rotated ellipse (and refused rotation with per-quadrant
  limits); 3.13, as terrio, searches its quadtree: sums can differ in their last bits from 3.12.
- Like GDAL 3.13.3, `linear` skips triangles whose barycentric determinant has absolute
  value below 1e-5. Small geographic triangles can therefore produce nodata with radius 0;
  use a suitable projected CRS for those inputs. Coordinates outside the triangulator's
  supported numeric range are rejected. Collinear points have no triangulation:
  Terrio returns nodata, while GDAL 3.13.3 reports a triangulation failure and leaves
  its initialized nodata output. The default nearest fallback does not fill that case.
- Zero-width or zero-height extents are rejected before allocating; GDAL can emit a
  noninvertible transform for these inputs. Reversed x bounds retain negative pixel width,
  matching GDAL.
- With `--sql`, `gdal_grid` writes no CRS; terrio keeps the query result's.
- `-clipsrc` with a geometry or a datasource, `-spat` and `-co` other than terrio's raster
  output options are not supported.

Search-axis signs follow GDAL: for more than 100 points, negative semi-axes can use
a different candidate path from positive axes and yield a different result. They
are not normalized to magnitudes.

## lineref

```sh
terrio lineref create --lines path.shp --points mstones.shp --position-field pos --step 1000 parts.gpkg
terrio lineref get-pos --parts parts.gpkg -x -1.4345 -y 51.9497
terrio lineref get-coord --parts parts.gpkg -m 15977.724709
terrio lineref get-subline --parts parts.gpkg --begin 13300 --end 17400 subline.gpkg
```

Linear referencing, as GDAL's `ogrlineref` (`-create`, `-get_pos`, `-get_coord`,
`-get_subline`). `create` cuts the route (the first feature of `--lines`, or with
`--line-field` and `--point-field` each route with its reference points; `--id-field`, default
`uniq_uid`, receives the route identifier) into parts `--step` positions long, from reference
points (milestones) carrying their positions in `--position-field`. Points farther than 10 units
(0.00008983153 degrees for a geographic route) from the route are ignored, the route is reversed
when it runs against the points, and the parts before the first and after the last point are
measured in the UTM zone of their middle for a geographic route. Each part has its first and
last positions (`beg`, `end`) and its length per position unit (`scale`).

`get-pos` prints the position of a point on the nearest part, `get-coord` the `x,y,z` of a
position on each part strictly spanning it (both with six decimals, as `ogrlineref -quiet`;
`--json` for full precision), and `get-subline` writes the route between two positions. As
ogrlineref, positions are rounded to six decimals in its part lookups. `--layer-name` names
the output layer (by default the file's stem); `--lines-layer`, `--points-layer` and
`--parts-layer` choose input layers. Like GDAL, an initial route stretch before the
first retained picket can produce a zero position span and a non-finite `scale`;
avoid such inputs when using the resulting parts for position lookup. With exactly two
milestones at the route endpoints, creation produces no parts, matching GDAL 3.13.3;
include an intermediate milestone when using this case. Creation is limited to 100,000
pickets to bound memory use; increase the step for longer routes. Subline extraction rejects
duplicate start positions rather than silently mixing routes. With route identifiers, the first
feature for each identifier is used even if its geometry is missing; a later line does not
replace it, matching GDAL 3.13.3. Only single-LineString parts contribute to `get-subline`;
missing geometries are skipped as in GDAL, so validate externally edited parts to avoid gaps.
Coordinate lookups interpret null scale as zero, as GDAL 3.13.3 does; zero and negative
scales clamp to the part's first vertex. Position lookups currently copy each part's vertices, and text-to-number conversion tries
numeric prefixes; repeated lookups and unusually long numeric strings can be expensive.

For repeated queries, open parts once as a Python VectorDataset/Layer or JavaScript Dataset.
Path, byte and GeoJSON inputs are read/parsed anew on each call. Lookup edge behavior follows
OGR: positive-infinite scale clamps coordinates to the last vertex and contributes zero
position offset; null scale is zero. Malformed single-vertex parts can produce OGR's -1
projection value. Use normally generated parts for meaningful reference positions.

Grouped route failures are reported as per-route warnings; as with ogrlineref,
creation can succeed with an empty output if every selected route fails.

## mcp

```sh
terrio mcp
```

Serves the other commands to AI agents (Claude Code, Claude Desktop, Cursor, VS Code) as a Model
Context Protocol server over standard input and output. See [AI Agents](mcp.md).

## vsi

```sh
terrio vsi list -lR s3://bucket/prefix                 # gdal vsi list (ls)
terrio vsi list -R --depth 2 -f json --tree /vsizip/data.zip
terrio vsi copy -r data/ s3://bucket/prefix            # gdal vsi copy (cp)
terrio vsi copy /vsizip//vsicurl/https://host/a.zip/a.shp .
terrio vsi sync -r --strategy ETag data/ s3://bucket/prefix   # gdal vsi sync
terrio vsi move s3://bucket/old s3://bucket/new        # gdal vsi move (mv, ren, rename)
terrio vsi delete -r s3://bucket/prefix                # gdal vsi delete (rm, rmdir, del)
```

File management across locations, as GDAL's `gdal vsi` commands do on its virtual file systems:

| Location | Forms | list | read | write, delete |
|---|---|---|---|---|
| Local files | `dir/file` | yes | yes | yes |
| In memory | `/vsimem/dir/file` | yes | yes | yes (within one process: Python, JavaScript) |
| Amazon S3, S3-compatible services | `s3://bucket/key`, `/vsis3/` | yes | yes | yes, signed (see [Remote Data](remote-data.md)) |
| Google Cloud Storage | `gs://bucket/key`, `/vsigs/` | public buckets | yes | no: use its S3-compatible API (`s3://` with `AWS_S3_ENDPOINT=https://storage.googleapis.com` and HMAC keys) |
| Azure Blob Storage | `az://container/blob`, `/vsiaz/` | yes | yes | with a SAS token (`AZURE_STORAGE_SAS_TOKEN`) |
| HTTP(S) | `https://...`, `/vsicurl/` | from an HTML index page (Apache, nginx) | yes | no |
| Zip archives | `/vsizip/a.zip/dir`, also `.kmz`, `.shz`; nested in remote locations | yes | yes | no |
| Tar archives | `/vsitar/a.tar`, `.tar.gz`, `.tgz` | yes | yes | no |

`list` prints a directory's entries (a file lists as itself), directories before what they hold
and siblings by name; `-l` adds permissions, size and date as `ls -l` (and `gdal vsi list -l`)
print them, `-R` recurses (`--depth N` levels), `--abs` prints absolute paths, `-f json` prints
GDAL's JSON (names, or with `-l` objects with `name`, `type`, `size`,
`last_modification_date`, `permissions`, and `etag` for objects), `--tree` nests it. An object
store with unlimited depth is listed recursively in one paged listing (1,000 keys per request on S3). Finite depths use delimiter listings only down to the requested levels, including JavaScript `listUrl`. Remote zip archives are listed by range requests: the archive's end and central
directory, about 32 KB for Natural Earth's 4.9 MB `ne_10m_admin_0_countries.zip`; tar archives
by seeking from header to header. For an archive URL with a query string, braces keep the
query attached to the archive, for example
`/vsizip/{/vsicurl/https://host/data.zip?sig=TOKEN}/dir/member.tif`.

`copy SRC DST` copies a file to `DST`, or into it when `DST` is an existing directory (or ends
with `/`; a local destination directory named this way must already exist); a directory needs `-r` and is copied as `DST`, or into an existing `DST` as
`DST/<name>`, as GDAL does. `--skip-errors` goes on past the files that fail, reporting them.
Copies within S3 are server-side (`x-amz-copy-source`), uploads are single `PUT`s (up to 5 GiB),
and other remote copies go through a temporary file. Local files are written to a temporary file
renamed into place, so a failed copy leaves an existing target as it was.

`sync SRC DST` copies the files that are missing or out of date in `DST` and deletes nothing; a
source directory ending with `/` is synchronized into `DST`, without one as `DST/<name>`
(rsync's convention, as GDAL's). `--strategy timestamp` (the default) copies when the sizes differ
or the source is newer; `ETag` compares entity tags, or a local file's MD5 with an S3 object's
(single-part uploads; multipart entity tags fall back to `timestamp`); `overwrite` always copies.

`move SRC DST` renames on one local file system; elsewhere it copies, then deletes the source only
once all of it was copied. `delete PATH` deletes a file; `-r` deletes a directory and what it
holds.

Safety: `delete` and `move` refuse an empty path, a file system root, the home directory, the
current directory or one of its parents, the root of `/vsimem/`, a whole bucket or container,
and archive and HTTP locations. Recursive deletion never follows symbolic links (the links are
deleted), checks that every directory it enters is below the path given, and on object stores
deletes only the keys below `prefix/` (not `prefix2/...`). Like GDAL 3.13.3, recursive object-store deletion preserves an exact object named `prefix`, even when it coexists with `prefix/` children; delete that exact object without `-r`. Recursive deletion of a lone object fails because there is no directory prefix to remove. `copy`, `sync` and `move` refuse to
copy a directory into itself.
Local recursive removal uses the standard library's symlink-race-protected removal on
Linux, macOS and Windows. Counting makes a discovery pass before the removal pass;
`deleted` reflects that discovery count, which requires an unchanged tree to be exact.

`-q` prints nothing, `--json` prints what was done (`copied`, `bytes`, `unchanged`, `deleted`,
`errors`), `--stats` the HTTP requests and bytes received.

VSI moves replace existing destination files, as GDAL does, and refuse existing
destination directories. Local filesystem renames preserve native links and names;
copy-delete moves between stores or filesystems refuse symbolic links and non-UTF-8
entries before copying. Keep the source unchanged during a copy-delete move.

Recursive extraction and copy reject parent traversal, Windows drive-style child
names from archives or remote/memory listings (on all platforms for portable paths), and symlinks escaping the output root. Local-to-local copies on Unix preserve legal colons and backslashes in file names. Tar and tar.gz copies and sync stream payloads in one pass after discovery. ZIP
central directories are indexed once per operation with a bounded read buffer.
Local file writes sync the temporary file before renaming it into place; many small files
therefore pay one file sync each. Containment checks resolve the root and existing target
ancestors per entry, retaining current filesystem-state checks at that cost. File sync does
not include a directory sync and is not a complete power-loss durability guarantee.
Tar listings expose regular files and directories; copying a tar link or other unsupported
entry fails, or records an error with `--skip-errors`. Untimed memory targets are compared
by content during sync. Local atomic writes follow destination
symlinks and preserve mode bits, while replacement detaches hard links and does not
preserve ownership, ACLs or extended attributes.

HTTP directory listings reject index pages larger than 4 MiB and stop with an error
at the object-listing page budget. Binary HTTP responses are not read as index
pages. Recursive copy/sync refuses nested directory symlinks rather than silently creating
an empty directory; `--skip-errors` records the refusal in its error report.

Bucket/container roots are synthetic directories across S3, GCS and Azure URL forms;
`stat` does not verify their existence or access. Listing performs that verification.
Recursive deletes stop at the first failure and can leave a partially deleted tree or prefix;
the error does not include a partial deletion count. Inspect the remaining entries before retrying.
An untyped HTTP file copy currently uses three requests: a bounded 4096-byte index sniff,
a one-byte size probe, and the payload GET. A server may send additional bytes before the
sniff connection closes; `--stats` counts bytes consumed by Terrio, not packets on the wire.
Credential-bearing URL queries are redacted in diagnostics and listing names.
VSI mutation/listing operations require complete object-store listings; capped
read-only subdirectory discovery retains its existing warning and partial results
for readers such as Zarr.

Recursive object-store listings include directories implied by keys; GDAL omits those
implied directory entries. `--skip-errors` continues copying but exits nonzero when
any files fail. Signed S3 file uploads read the source once to calculate its SHA-256 and once
to send the body (twice the file size in local reads); ETag sync can add a separate MD5 pass.
Payload hashes are retained for compatibility with signed endpoints that require them.

Differences from GDAL (checked with 3.13.3): entries are sorted (GDAL prints local directories in the order
the file system returns them); dates are UTC (a zip member's date is its stored DOS time, as
GDAL prints it); HTTP index names stay percent-encoded, as GDAL keeps them. Not supported:
`sync -j/--num-threads` (requests are sequential, including one S3 delete request per key), multipart uploads (objects over 5 GiB), Azure
shared-key authentication, writing to Google Cloud Storage's own API, empty directory
creation on destinations that cannot represent them (local directories are supported; S3 uses marker keys), progress bars, and the virtual file
systems `/vsigzip/`, `/vsi7z/`, `/vsirar/`, `/vsiswift/`, `/vsioss/`, `/vsiadls/`, `/vsihdfs/`
and `/vsiwebhdfs/`.

Byte copies and copy-delete moves refuse pipes, sockets and devices. Recursive copies with
`--skip-errors` report those entries and continue with regular files. ZIP member names retain
valid UTF-8; unflagged non-UTF-8 legacy names are decoded as CP437 into Unicode. GDAL can instead emit
raw non-UTF-8 bytes for those names.

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

## unscale

```sh
terrio unscale packed.tif physical.tif
terrio unscale packed.tif physical.tif --ot Float64
```

As `gdal raster unscale` (`gdal_translate -unscale`): each band becomes `value * scale +
offset` with its own scale and offset (GeoTIFF band scales and offsets, netCDF `scale_factor`
and `add_offset`; bands without them are converted unchanged). `--ot` is the output type,
Float32 by default (Float64 for Float64 inputs). Nodata pixels keep the band's nodata value,
which the output declares unchanged, and the output has no scale or offset. Values are computed
and rounded as GDAL's virtual rasters compute them: working pixels are Float32 unless the input
or output is a 32- or 64-bit integer, or the output is Float64. A Float64 input with
`--ot Float32`, Byte or Int16 still uses Float32 working pixels, matching GDAL 3.13.3.
Byte outputs round half up. Complex bands are not
supported. As in GDAL 3.13.3, if the output type cannot represent the original nodata
value, pixels are clamped to that type while the original nodata declaration is retained
(for example, nodata -9999 becomes a zero pixel in Byte output, still tagged -9999).
Choose an output type that can hold the nodata value when those pixels must remain invalid.
The standard raster output options apply.

## nodata-to-alpha

```sh
terrio nodata-to-alpha landsat.tif landsat_rgba.tif
terrio nodata-to-alpha image.tif out.tif --nodata 0 --nodata 0 --nodata 0
```

As `gdal raster nodata-to-alpha`: the bands are copied without nodata values and followed by an
alpha band of their type, 0 where the first band's mask is 0 and 255 elsewhere (Int8 alpha
saturates at 127, as GDAL converts it). The mask is the first band's nodata value; one
`--nodata` overrides the value used for the first-band mask; other bands do not
participate in that mask. One `--nodata` per band (`nan`, `inf` and `-inf`
are accepted) makes a pixel transparent only where every band has its value, as GDAL's
`NODATA_VALUES`. Without explicit `--nodata`, a raster whose first band has no nodata
value, or that already ends with an alpha band (two or four bands), is copied unchanged.
Explicit nodata adds a new alpha band even to RGBA input, matching GDAL 3.13.3. Copied
bands retain their scales and offsets, including driver-recorded packing. Internal and `.msk`
masks are not used.
The result is an owned raster held in memory. An unchanged result also clones the input's
pixel buffer; this path does not stream file conversion.

## reclassify

```sh
terrio reclassify nlcd.tif groups.tif -m "11=1;[21,25)=2;[41,44)=4;DEFAULT=0"
terrio reclassify dem.tif zones.tif -m "NO_DATA=0;[0,1500)=1;[1500,inf)=2" --ot Byte
terrio reclassify nlcd.tif groups.tif -m @mapping.txt
```

As `gdal raster reclassify`, with GDAL's mapping syntax: `from=to` entries separated by `;`.
`from` is a value, an interval (`[a,b)`, `(a,b]`, `(-inf,b]`, `[a,inf)`...), `NO_DATA` (the
band's nodata value), `NAN` or `DEFAULT` (any other value); `to` is a value, `NO_DATA` or
`PASS_THROUGH` (the value unchanged). Intervals must not overlap. `@file` reads one entry per
line; leading spaces and `#` comments are removed. A value that no entry maps, or an output value
the output type cannot represent exactly, is an error. `--ot` is the output type (by default the
input's). The output keeps each band's nodata value; like GDAL's, it has no color table, band
descriptions or scales. Like GDAL 3.13.3, `NO_DATA` intervals with NaN nodata
can fail overlap validation and do not match NaN pixels: `NO_DATA=0;DEFAULT=PASS_THROUGH`
leaves NaN unchanged. Use `NAN` as the source mapping key for NaN pixels.
As with `unscale`, retained nodata pixels are clamped when the output type cannot hold
the original nodata declaration; choose a suitable output type to preserve their validity.

## as-features

```sh
terrio as-features dem.tif pixels.gpkg --geometry-type polygon --include-xy --include-row-col
terrio as-features landsat.tif pixels.csv --skip-nodata -b 3 -b 2
```

As `gdal raster as-features`: one feature per pixel, row by row, in a layer named `pixels`
(`--output-layer`). Fields: `CENTER_X` and `CENTER_Y` (pixel centres, `--include-xy`), `ROW`
and `COL` (`--include-row-col`), then `BAND_<n>` (Float) per band. `--geometry-type` is `none`
(an attribute table, the default), `point` (pixel centres) or `polygon` (pixel squares, in the
raster's CRS). `--skip-nodata` leaves out the pixels whose first reported band has its nodata
value after conversion to the source type; nearby Float32 values remain valid. Repeated band
selections are reported once, in their first-selected order. `-b` selects bands (GDAL 3.13.3 accepts `-b` but reports every band). `--append`
and `--overwrite` update an existing output, as for `polygonize`. The features are built in
memory; an estimated expansion above 512 MiB is refused before generating features.
Crop large inputs first. GDAL's `--update`, `--overwrite-layer` and
layer creation options are not supported.

## Native multidimensional arrays

See [Native multidimensional arrays](multidim.md) for scientific groups, named dimensions,
lazy coordinate arrays, time/depth slicing, transposition and netCDF/Zarr numeric snapshots.

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

Catalog exports enumerate nested VRT references before creating their destination, including
supplied aliases, masks, dataset containers and archive files. Inventory errors and cyclic
references fail before opening the output. A destination within a referenced directory store
also triggers a completed read before writing. Supported container selectors are `NETCDF:`,
`HDF5:`, `ZARR:` and `GPKG:`; other driver-style selectors report an unknown inventory instead
of claiming they read no files. Materialize such a tile first when its provider cannot enumerate
its inputs. An encoded tile that previously failed to open also blocks whole-catalog
export, even when other windows remain readable; materialize a successful window first
to export only that data. Literal local filenames resembling selectors, such as
`DATA:1.tif`, must use an explicit filesystem path (`./DATA:1.tif` or an absolute
path) to disambiguate them from an unknown driver selector.
Remote VRTs require fetching their XML descriptions during export inventory;
ordinary remote raster pixels stay unread. Duplicate locations and shared nested VRTs are
inventoried once per export traversal; its metadata cache is released when the traversal ends.

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

A 1×1 unnormalized VRT kernel is applied once, matching GDAL 3.13.3
(coefficient 2 maps sample 3 to 6).

`KernelFilteredSource` supports odd full/separable kernels, normalization and
`min`, `max`, `stddev`, `median` and `mode`. Kernels read the necessary halo and
replicate source edges as GDAL does. The 3–99 limit applies to `--size`; explicit matrices
can be larger odd squares, matching GDAL. Their work per pixel grows with the
number of coefficients. Explicit dataset masks, `SourceBand mask,N`
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

### Coordinate operation safeguards and warp kernel versions

`crs` and `convert` accept repeated `--ct-opt KEY=VALUE`; `warp` accepts the same
coordinate operation controls through its existing `--to KEY=VALUE` option:

```sh
terrio crs EPSG:4610 --to EPSG:4326 110 35 --ct-opt ALLOW_BALLPARK=NO
terrio convert input.shp output.geojson --t-srs EPSG:4326 --ct-opt ONLY_BEST=YES
terrio warp input.tif output.tif --t-srs EPSG:2056 --to ONLY_BEST=YES
```

`ALLOW_BALLPARK=NO` excludes approximate datum operations; a CRS pair with no
remaining operation fails. `ONLY_BEST=YES` requires the best registered operation
for each point assuming all datum grids were available; it fails if that operation
needs a missing grid. Ordinary out-of-domain coordinates remain invalid pixels in a warp. `ONLY_BEST` alone still permits
a ballpark when no better operation exists. Defaults preserve fallback behavior.
Warnings report ballparks between different ellipsoids, missing best-operation
grids (sampled at the first coordinate with default options, with
`TERRIO_NETWORK=ON` / `TERRIO_GRID_DIR` guidance), and operations used
outside their area of use. Parallel work aggregates warnings, which appear once
on stderr and in MCP tool result text. Heights follow GDAL too: between CRSs with
heights, keeping a vertical CRS's heights as they are (PROJ's ballpark vertical
transformation) fails with `ALLOW_BALLPARK=NO`, and with `ONLY_BEST=YES` when a
geoid model or vertical transformation exists but does not apply (its grid is
missing, or the point is outside it).
`PROMOTE_TO_3D=YES` takes the heights of 2D CRSs as ellipsoidal heights, so that
datum transformations change them, as GDAL does after `PromoteTo3D` on both CRSs
(`terrio crs EPSG:28992 --to EPSG:4979 --dim 3 85000 447000 0 --ct-opt PROMOTE_TO_3D=YES`).
Point-cloud reprojection currently rejects `--ct-opt` because it does not support
these controls.

GDAL 3.13 changed warp kernel scales and the bilinear/cubic downsampling threshold.
Use `warp --kernel-version 3.13` to reproduce that behavior. The default `3.12`
retains Terrio's earlier results and the repository's GDAL 3.12 conformance baseline.
Both modes keep lazy window/overview reads. Versioned kernels and explicit
coordinate-operation policies currently require materialized output rather than
serialized warped VRTs.

GeoTIFF and COG translation and warping preserve `AREA_OR_POINT=Point`, including
point tie points and rotated affine transforms, without moving pixel corners.

Writing a PixelIsPoint GeoTIFF requires a CRS. A write with an unknown CRS is
rejected before creating the file, avoiding a half-pixel origin shift without
a corresponding raster-type GeoKey.

Remote listings limit each HTML index to 4 MiB and each object-store XML page to 16 MiB; larger pages fail explicitly.

Recursive copy destinations must remain within their destination tree, including ordinary
local copies: directory symlinks that escape it are refused. Same-device directory moves
rename the directory without copying payloads and report zero copied files/bytes.
Plain HTTP source discovery currently probes the directory-index URL before the file
range request; a file can therefore cost an extra GET, usually a 404.


Raster comparison reads TIFFTAG items, embedded RPC tags and PAM sidecar metadata
from local filesystem inputs. HTTP and archive comparisons use the reader's
metadata and do not add those local-file extras. Comparing a local file to its
remote/archive copy can therefore report metadata differences; use the applicable
metadata/RPC skip options for a pixel-focused comparison across those locations.

### Raster management

The vector commands `create`, `edit` and `update` retain their existing names.
Use `create-raster`, `edit-raster` and `update-raster` for raster management:

```sh
terrio create-raster filled.tif --size 256,256 --ot Float32 --burn 7 --crs EPSG:3857 --bbox 0,0,256,256
terrio create-raster template.tif --like dem.tif --copy-metadata --copy-overviews
terrio edit-raster filled.tif --nodata -9999 --metadata 'SOURCE=A & B' --stats
terrio edit-raster filled.tif --t-srs EPSG:4326 --error-threshold 0.1
terrio overviews filled.tif 2 4 --resampling average
terrio overviews filled.tif --refresh --bbox 0,0,128,128
terrio update-raster replacement.tif filled.tif --resampling bilinear
terrio overviews filled.tif --clean
```

Overview methods are nearest, average, min, max and mode. Existing unrequested
levels are retained. `--external` manages the supplied filename's `.ovr` sidecar,
including when that filename is a symlink. Explicit compression applies to
requested rebuilt levels. Updates retain destination pixels outside valid source
coverage and refresh affected overviews unless `--no-update-overviews` is set.
Georeferencing moves change the affine transform and CRS without resampling;
`--t-srs` conflicts with other edits, and requires `--error-threshold` to write.
A raster without a geotransform of its own (unreferenced, or georeferenced by
ground control points) cannot be moved. With `--like`, a template without a
geotransform gives its GCPs, or no georeferencing, instead of a unit-pixel grid.

In-place operations require writable little-endian GeoTIFFs. Updating a COG
requires `--ignore-cog-layout-break` and can invalidate its optimized layout.
Edits use a temporary file and rename; a failure before replacement preserves
the original. A tag of a field type the TIFF specification does not define is
refused rather than dropped from the rewritten file. Updating external overviews
occurs after the base TIFF commits, so an error there can leave stale overviews.
Update working buffers span full-width rows intersecting the update window; memory
can be large for wide rasters.

Raster update requires equal source/destination band counts and treats alpha as
an ordinary band; automatic RGB-to-RGBA alpha synthesis is not supported. Existing
per-dataset masks remain unchanged, as in the GDAL 3.13.3 update probes. Valid
values can saturate to integer type bounds or collide with destination nodata,
also matching those probes. Average updates between identical affine pixel grids use direct copying when
their origin offsets are integer to within 1e-7 pixels; fractional offsets retain
area-based resampling.
Min/max overview methods are Terrio extensions; GDAL 3.13.3 gdaladdo refuses them.

Raster in-place edits copy the whole TIFF, including for metadata-only changes, and
need free disk space for that copy. Symlink targets and mode bits are preserved;
replacement detaches hard links and does not preserve ownership, ACLs or extended
attributes. The containing directory must be writable. The TIFF is committed before
its histogram or external-overview sidecar: a later sidecar error can leave the TIFF
updated with an unchanged sidecar.

Building or wholly refreshing overviews reads the full-resolution sources the levels need
(the image, its mask, or both) once, forward, in full-width strips of whole blocks (about
64 MB of values at a time, and at least a row of blocks, which on very wide tiled rasters is
larger). Every level computed from full resolution is computed in that
pass: in GDAL's chunks of rows (`GDALRegenerateOverviewsEx`, 5 to 10 MB of values per
band), or in strips holding every source window (`GDALRegenerateOverviewsMultiBand`'s
schedule), with the pixels of a whole read. A level computed from the previous one reads
that level, held in memory. Memory is therefore bounded by a read strip, a chunk and the
levels: the first level (a quarter of the pixels for factor 2) and the one being computed,
or every level at once (about a third of the pixels for 2, 4, 8...) when GDAL computes
each from full resolution. That is the case for nearest overviews, and overviews of rasters
with a mask, which GDAL computes band by band: planar images, and uncompressed external
overviews of images without an alpha band. A striped TIFF is read in whole strips, so a
raster stored as a single strip is read whole. On a 16384x16384 Float32 DEM, five internal
levels peaked at 478 MiB instead of 4.1 GiB. Partial refreshes (`--refresh --bbox`,
`--like`) read the full-width rows over their region, and the whole image once when the
region spans its height. Overview and raster-update
arithmetic uses Float64 and warns for Int64/UInt64: values above 2^53 may lose precision,
including nearest/mode overviews. Raster update holds the warped window and the full
width of the destination rows it rewrites in memory.
