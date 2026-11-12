# Python

Python bindings for [terrio](https://github.com/opengeos/terrio): read, write, convert and
reproject geospatial rasters and vector datasets in Rust, without installing GDAL. Wheels need
only NumPy (and the small `affine` package).

```sh
pip install terrio
```

The [quickstart notebook](notebooks.md) runs these functions on real data, in Google Colab or locally.

Coming from rasterio? `import terrio.rio as rasterio` runs typical rasterio scripts unchanged: see
[Coming from rasterio](rasterio.md). `terrio.compat.install()` goes further: `import rasterio`,
`rioxarray` and `pyogrio` then resolve to `terrio.rio`, `terrio.xarray` and `terrio.pyogrio`.

Working with xarray? `terrio.xarray.open_rasterio` (or `xr.open_dataset(path, engine="terrio")`)
opens rasters lazily as DataArrays, as rioxarray does, and the `.terrio` accessor has rioxarray's
`.rio` methods (`clip`, `reproject`, `to_raster`...): see [xarray](xarray.md).

## Rasters

```python
import terrio

dem = terrio.open_raster("dem.tif")
dem.width, dem.height, dem.count, dem.dtype  # dtype is a NumPy name: "float32", "uint8"...
dem.crs, dem.transform, dem.bounds, dem.nodata
elevation = dem.read(1)  # numpy array (rows, cols) of the raster's own dtype
everything = dem.read()  # (bands, rows, cols)

# GeoTIFFs and COGs, local or remote, can be read by window: only the tiles a window needs are
# decoded, and fetched with HTTP range requests.
cog = terrio.open_raster("https://example.com/cog.tif", lazy=True)
cog.overviews  # [(width, height), ...] of the overviews, largest first
tile = cog.read(1, window=(1024, 1024, 512, 512))  # (col_off, row_off, width, height)
thumb = cog.read(overview=2)
# An output of another size, as GDAL's RasterIO (gdal_translate -outsize, rasterio's out_shape)
# reads it: from the overview it chooses, so only that overview's tiles are fetched.
small = cog.read_resampled((512, 512), resampling="average")  # size is (width, height)
masks = cog.read_resampled_masks((512, 512), resampling="average")  # 0 invalid, 255 valid
pixels, valid = cog.read_resampled_with_masks((512, 512), resampling="average")  # both at once
part = cog.read_resampled((256, 256), band=1, window=(100.5, 200.25, 4096, 4096), overview="AUTO-1")
# PMTiles and MBTiles raster tiles too: zoom levels are overviews (PMTiles read in place).
tiles = terrio.open_raster("https://example.com/tiles.pmtiles", lazy=True)
# JPEG 2000 (Sentinel-2 bands) too: resolution levels are overviews, windows decode their tiles.
b04 = terrio.open_raster("T10SEG_20240630T185919_B04.jp2", lazy=True)
patch = b04.read(1, window=(2000, 6000, 1024, 1024))
# A VRT too: it has its first source's overviews, each read from its sources' own (see
# cli.md#serialized-virtual-rasters).
mosaic = terrio.open_raster("mosaic.vrt", lazy=True)
preview = mosaic.read_resampled((512, 512), resampling="average")
mask = terrio.open_raster("masked.tif", lazy=True).read_mask()  # a GeoTIFF's internal mask, or None

# A GeoTIFF larger than memory, written in rows (band after band, from the top).
w = terrio.GeoTiffWriter("big.tif", 100000, 80000, 1, "uint16", transform, crs="EPSG:32610",
                         compress="DEFLATE", tile_size=512, mask=True)
for rows, valid in row_blocks:  # (1, n, 100000) uint16 and (n, 100000) uint8 arrays
    w.write(rows, valid)
w.close()

out = terrio.Raster.from_array(elevation * 3.28084, dem.transform, crs=dem.crs, nodata=dem.nodata)
out.write("dem_feet.tif", compress="DEFLATE")
out.write("dem_feet_cog.tif", driver="COG")
out.write("dem.zarr", options={"FORMAT": "ZARR_V3", "GEOREFERENCING_CONVENTION": "SPATIAL_PROJ"})  # GeoZarr

# STAC: a mosaic of an asset over a search's items (or an item's asset, 'STAC:"item.json":red'), read by window.
red = terrio.open_raster('STACIT:"https://earth-search.aws.element84.com/v1/search?collections=sentinel-2-l2a&bbox=-105.05,40.5,-104.95,40.6&datetime=2023-07-05":asset=red', lazy=True)
terrio.open_raster("landsat.tif").write("landsat.pmtiles")  # Byte rasters as tiles (or .mbtiles)
```

Complex GeoTIFF/COG writes accept NumPy `complex64` and `complex128` through
`terrio.rio.open`, `Raster.from_array`, and the row writers. Each complex band remains
one TIFF band, with paired real and imaginary components. `terrio.rio.shutil.copy`
preserves CInt16 and CInt32 sample types as well; its CInt32 copy reads precise
components before the normal rasterio-compatible `complex64` conversion can round them.
For explicit integer-complex output, use `dtype="complex_int16"` in `terrio.rio`, or
`Raster.from_array(array, transform, complex_dtype="CInt16")` / `"CInt32"`.
Integer components are rounded and saturated, as GDAL 3.13.3 converts them.
`raster.read(out_dtype="complex128")` (and `np.asarray(raster, dtype="complex128")`)
preserves every Int32 component on direct `Raster` and `terrio.rio` dataset reads;
the default complex dtype remains compatible with rasterio. `WarpedVRT` currently
does not support complex working dtypes, so this guarantee does not cover VRT reads.
Converting CInt32 to `float64` or a 32-bit-or-wider integer output reads the components
without a float32 intermediate. Unsigned outputs preserve exactly representable positive
components; their final signed-to-unsigned conversion follows the existing cast policy.
A narrower real dtype, such as `float32`, first passes through complex64, which can round Int32 components above 2^24. Use
`raster.read(out_dtype="complex128").real` to retain exact real components of any type.
Updates retain the physical complex sample type. Integer-complex writers keep components in complex128 buffers, so CInt16 output rounds
once from the input precision and CInt32 output retains every integer component. Memory,
spill-file selection and slab accounting include their 16 bytes per complex sample.
`Raster.from_array` retains a native component buffer in addition to the input array;
a non-contiguous complex input also needs a contiguous NumPy copy during construction.
These constructor copies are not admitted against the read working-memory limit.
Complex `terrio.rio` reads support unresampled windows and nearest-neighbour resampling;
other interpolation methods compute real and imaginary components separately. Their complex
results have not been validated against GDAL.
Complex output supports NONE, LZW, DEFLATE, PACKBITS and ZSTD compression.

## Vector data

```python
ds = terrio.open_vector("countries.gpkg")  # or .shp, .geojson, .fgb, .parquet, .xlsx, .dxf, .dwg, .city.json, .citygml, .kml, .zip, WFS:, OAPIF:...
layer = ds.layers[0]  # also ds["countries"]
layer.name, layer.crs, layer.geometry_type, layer.fields, len(layer), layer.bounds
collection = layer.to_geojson()  # a GeoJSON FeatureCollection dict
wkb = layer.to_wkb()  # one WKB geometry per feature
columns = layer.columns()  # attribute values by field
gdf = layer.to_geopandas()  # with geopandas installed

terrio.write_vector(layer.to_crs("EPSG:3857"), "countries_3857.fgb")
terrio.write_vector(collection, "countries.geojson")  # GeoJSON dicts, GeoDataFrames, Arrow...

# GeoPackages (and GeoParquet files) are read by bounding box through their spatial index.
europe = terrio.open_vector("counties.gpkg", bbox=(-10, 35, 30, 60))
# Existing GeoPackages are updated: features appended to a layer (new ids, R-tree, extent and
# feature count updated, as GDAL does), layers replaced or added, other tables kept.
terrio.write_vector(layer, "world.gpkg", append=True)
terrio.convert("more.shp", "world.gpkg", nln="countries", overwrite=True)
```

The GeoPackage is changed in place, as SQLite changes it: only the pages that change are written (new
rows, R-tree nodes, the tables describing the layer), in a transaction protected by a rollback
journal, so appending to a large GeoPackage costs about as much as the features appended, and a
process stopped in the middle leaves the file as it was (SQLite, GDAL and terrio play the journal
back).

Driver open options go in `options` on `open_vector` and `info`, by their GDAL names, as `terrio
convert --oo` takes them (`HEADERS`, `FIELD_TYPES`, `HEADER_ROWS`, `FIRST_ROW` and `RANGE` for XLSX;
`X_POSSIBLE_NAMES` and `Y_POSSIBLE_NAMES` for CSV and XLSX; `ZOOM_LEVEL` and `CLIP` for vector tiles;
`WFS_*`; and `DATETIME`, `CRS`, `PAGE_SIZE`, `IGNORE_SCHEMA` and `OAPIF_*` limits for OAPIF);
`write_vector`, `Layer.write`, `Layer.to_bytes`, `VectorDataset.write` and `convert` take
`cityjson_version`, `citygml_version` and `citygml_lod0_surface` for CityJSON and CityGML writes.
HEC-RAS geometry and plan HDF files (the package is built with the `hecras` feature) read as
vector layers this way, with the results HEC-RAS stored joined by open options
(see [HEC-RAS](hecras.md)):

```python
plan = terrio.open_vector(
    "BaldEagleDamBrk.p03.hdf",
    layers=["mesh_cells", "cross_sections"],
    options={"CELL_SUMMARIES": "Maximum Water Surface", "XS_RESULTS": "Water Surface", "TIME_STEP": "12"},
)
terrio.info("BaldEagleDamBrk.p03.hdf", layers=["mesh_cells"], options={"CELL_ATTRIBUTES": "MINIMUM_ELEVATION"})
```

### Layer selection

An empty `layers` request keeps all layers. Duplicate requests do not duplicate
output, and source order is retained. Every requested name must match; readers,
rendering and filtering report `no layer named NAME (layers: AVAILABLE)` for a
missing name. A file with exactly one layer also accepts the filename without its
extension, without renaming the layer. This alias is a Terrio extension to GDAL
3.13.3's layer-name lookup. Native GeoPackage table names remain case-insensitive.
For ZIP archives, selection uses the main dataset member's filename (for example,
`member` for `member.shp` inside `archive.zip`), rather than the archive's name.

HEC-RAS, WFS and OAPIF retain their driver-specific selection rules and error
messages. HEC-RAS requires stored layer names; it does not accept filename aliases.

The filename alias is available while opening a path. A `VectorDataset` or `Layer`
does not retain its source filename: use its actual layer names in subsequent
operations. For a file `map.geojson` containing one layer named `roads`:

```python
ds = terrio.open_vector("map.geojson", layers=["map"])  # selects roads
terrio.render("map.geojson", layers=["map"])           # filename alias works
terrio.render(ds, layers=["roads"])                   # actual name on a dataset
# terrio.render(ds, layers=["map"]) raises: no layer named map (layers: roads)
```

### pyogrio inputs

`terrio.pyogrio` accepts bytes, byte arrays, memory views and readable file objects for recognized
vector formats, including GeoPackage, FlatGeobuf, GeoParquet, GeoJSON and recognized XML formats
such as KML, GML, GPX and OSM. The core's byte identifiers choose the temporary filename; the
ordinary reader validates the dataset and applies the same driver restrictions as a named file.
Content without a recognizable signature, such as CSV, still needs a named file. For formats
whose layer name comes from the filename, such as GeoParquet, anonymous buffers use `data`.

```python
import io
from pathlib import Path
from terrio import pyogrio

data = Path("roads.gpkg").read_bytes()
frame = pyogrio.read_dataframe(io.BytesIO(data))
layers = pyogrio.list_layers("published.zip")  # dataset beside a README or license
```

Archives without a member use the core's main-dataset selection, so ancillary files do not hide
the dataset. Collections of root-level shapefiles still expose one layer per shapefile.
Datetime parsing preserves uniform UTC offsets, converts mixed offsets to UTC by default, and
preserves individual offsets with `mixed_offsets_as_utc=False`. Terrio does not change warning
filters during parsing on pandas 2 or 3; pandas can still manage its own warning filters.
Preserving individual offsets also accepts the ISO UTC suffix `Z` on Python 3.10.
When naive and timezone-aware strings appear together, the default treats naive
values as UTC before conversion. With `mixed_offsets_as_utc=False`, naive values
remain naive and aware values retain their offsets in the object column.

### Streams

Layers larger than memory are read a chunk of features at a time with `terrio.open_stream`
(GeoPackage, FlatGeobuf, GeoParquet, Shapefile, GeoJSON, GeoJSONSeq and CSV; other formats are
read whole and handed out in chunks), and `terrio.convert` streams vector conversions between
those formats (GeoPackage outputs included), as the command line does:

```python
stream = terrio.open_stream("buildings.gpkg", "buildings", batch_size=100_000)
stream.name, stream.crs, stream.fields, stream.geometry_type
for chunk in stream:  # terrio.Layer chunks of at most 100,000 features
    gdf = chunk.to_geopandas()

for feature in terrio.iter_features("buildings.fgb"):  # GeoJSON-like dicts, one at a time
    print(feature["properties"])

# Arrow record batches, read as the consumer asks for them.
reader = pa.RecordBatchReader.from_stream(terrio.open_stream("buildings.parquet"))
for batch in reader:
    ...

terrio.convert("buildings.gpkg", "buildings.parquet", where="height > 10")  # in bounded memory
```

`bbox=(min_x, min_y, max_x, max_y)` reads only the features a GeoPackage's R-tree or a
GeoParquet file's bounding boxes find. The Arrow stream has the schema `Layer.to_arrow()` gives
(WKB geometries); a layer with date fields is read twice, once to choose their Arrow types.

`columns=[...]` keeps only those fields (the geometry is always read).

A GeoParquet stream's Arrow export hands out the record batches the file holds, without
converting them to features, so it is the fastest way to load GeoParquet into pyarrow, GeoPandas,
Polars or DuckDB. Its row groups are decoded in parallel as the consumer asks for batches, a
window at a time, and a URL is read by range requests: the footer, then only the column chunks of
the row groups the filter keeps.

```python
import geopandas
import pyarrow as pa

stream = terrio.open_stream("buildings.parquet", columns=["height"], bbox=(5.0, 45.0, 6.0, 46.0))
gdf = geopandas.GeoDataFrame.from_arrow(stream)
for batch in pa.RecordBatchReader.from_stream(terrio.open_stream("https://host/buildings.parquet")):
    ...
```

The columns keep the file's Arrow types (structs and lists included), as GDAL's Arrow stream
hands them out; the geometry columns are tagged as GeoArrow extension types (`geoarrow.wkb`, or
the file's native encoding) with their CRS, and the bounding-box covering column is left out
unless `columns` names it. Iterating the stream instead gives `Layer` chunks, whose struct
columns are flattened into `name.member` fields, as GDAL's features have them.

### Arrow

Layers convert to and from [Apache Arrow](https://arrow.apache.org) through the
[Arrow PyCapsule interface](https://arrow.apache.org/docs/format/CDataInterface/PyCapsuleInterface.html),
so pyarrow, GeoPandas, Polars, DuckDB and GDAL exchange them without copying through Python and
terrio needs none of them:

```python
table = layer.to_arrow()  # a terrio.ArrowTable
pyarrow_table = pa.table(table)  # or table.to_pyarrow(); pa.table(layer) works too
gdf = geopandas.GeoDataFrame.from_arrow(table)  # geometries and CRS included
df = polars.DataFrame(table)

native = layer.to_arrow(geometry_encoding="geoarrow")  # geoarrow.multipolygon...
interleaved = layer.to_arrow(geometry_encoding="geoarrow", coordinates="interleaved")

layer = terrio.from_arrow(pyarrow_table)  # also GeoDataFrame.to_arrow(), Polars, record batches
layer = terrio.from_arrow(gdal_layer)  # an osgeo.ogr.Layer exports its Arrow stream
terrio.write_vector(pyarrow_table, "out.gpkg", layer_name="countries")
```

The attributes become typed columns (integers as `int64`, reals as `double`, `bool`, `string`,
`binary`, dates as `date32`, date-times as millisecond timestamps, UTC when they have time zones)
and the geometries a [GeoArrow](https://geoarrow.org) column: WKB (`geoarrow.wkb`) by default,
which holds any geometry, or the native encodings for layers of one geometry type (a type and its
multi-part form are written as the multi-part form). The CRS is in the column's metadata, as
PROJJSON. On import, the geometry column is found by its GeoArrow metadata (WKB, WKT or native),
GDAL's `ogc.wkb`, GeoParquet `geo` metadata (as `pyarrow.parquet` keeps it), or a binary column
named `geometry` or `wkb_geometry`; struct columns are flattened and lists become JSON, as in GDAL.

`scripts/compare_arrow_with_gdal.py` checks both directions on real files against GDAL's Arrow
streams (`ogr.Layer.GetArrowStreamAsPyArrow`, `ogr.Layer.WriteArrow`), pyarrow and GeoPandas:
geometries are WKB-identical and attribute values equal.

### Vector datasets

```python
merged = terrio.concat(["countries.gpkg", "us_counties.gpkg"], "merged.gpkg", mode="single",
                       source_layer_field_name="source", dst_crs="EPSG:3857")
files = terrio.partition("counties.gpkg", "out", ["STATE"], feature_limit=1000)  # paths written
typed = terrio.set_field_type("countries.gpkg", "Integer64", field_name="POP_EST")
points = terrio.make_point("trace.csv", "longitude", "latitude", crs="EPSG:4326")  # a Layer
schema = terrio.export_schema("countries.gpkg")  # the OGR_SCHEMA dict
terrio.create_vector("empty.gpkg", schema=schema)
terrio.create_vector("sites.gpkg", output_layer="sites", geometry_type="POINT", crs="EPSG:4326",
                     fields=["name:STRING(80)", "visits:INTEGER"])
```

These are `terrio concat`, `partition`, `set-field-type`, `make-point`, `export-schema` and
`create` (GDAL's `gdal vector` commands of the same names); sources are paths or vector data in
memory (`VectorDataset`, `Layer`, lists of layers, GeoJSON). Conversion warnings are issued as
`UserWarning`s. See [Vector datasets](cli.md#vector-datasets) for the options, GDAL's rules
and the differences. Filesystem `partition` is unavailable in Pyodide;
the remaining imported APIs remain available.

#### Renaming and updating vector datasets

`rename_layer`, `edit_vector` and `update_vector` accept the same vector sources as
`concat`. They return a new `VectorDataset` for memory inputs. For local files,
`rename_layer(path, in_place=True, ...)` and `edit_vector(path, in_place=True, ...)`
change a GeoPackage; `update_vector(source, destination_path, ...)` updates an
existing destination. Read-only files and active SQLite journals are refused.

```python
renamed = terrio.rename_layer(countries, ascii=True, lower_case=True)
assigned = terrio.edit_vector(renamed, crs="EPSG:4326")
updated = terrio.update_vector(changes, renamed, key=["name"], mode="merge")
terrio.edit_vector("countries.gpkg", in_place=True, metadata=["SOURCE=survey"])
```

Rename options mirror the CLI: `input_layer`, `output_layer`, `ascii`, `lower_case`,
`filename_compatible`, `reserved_characters`, `replacement_character`, `max_length`.
Edit supports `active_layer`, `crs` (`"none"` removes it), `geometry_type`,
`unset_fid`, dataset/layer `metadata` and `unset_metadata` lists. Metadata edits
require file paths because the memory model does not retain dataset metadata.
Assigning a CRS changes its declaration without transforming coordinates.
Update supports `input_layer`, `output_layer`, `key` and `mode` (`merge`,
`update-only`, `append-only`); missing layer selections in multilayer datasets fail.
File edits materialize the dataset and require space for a neighboring temporary
copy. See the [CLI documentation](cli.md#edit) for atomicity and metadata preservation limits.

The vector dataset copy operations `concat`, `rename_layer` and `edit_vector` refuse an
existing destination path. Choose a new output path; use `in_place=True` for supported
GeoPackage edits.

### SQL queries

`terrio.sql(src, query, layers=None)` returns a `Layer`. Inputs may be a vector path,
`VectorDataset`, `Layer`, list of layers, or GeoJSON. It implements the shared
[SQL subset](sql.md), including computed columns, DISTINCT, grouped summaries,
attribute joins and the documented spatial functions.

```python
result = terrio.sql("countries.gpkg", "SELECT ADMIN, POP_EST/1000000.0 AS millions FROM countries")
summary = terrio.sql("countries.gpkg", "SELECT COUNT(*) AS n, SUM(POP_EST) AS population FROM countries")
```

Raw GeoJSON inputs use the layer name `layer`; use `Layer.from_geojson(..., name=...)`
(Python) or `fromGeoJSON(src, name)` (JavaScript) to assign another name.

### Vector geometry operations

`simplify`, `segmentize`, `convex_hull`, `concave_hull`, `swap_xy`, `set_geom_type`, `combine`
and `sort` run the [vector geometry commands](cli.md#vector-geometry-commands) (GDAL 3.13's
`gdal vector` commands of those names) and return a `Layer`. Inputs may be a vector path,
`VectorDataset`, `Layer`, list of layers, or GeoJSON, with a single layer, or the one `layer=`
names. `convert(..., simplify=TOLERANCE)` and `convert(..., segmentize=MAX_LENGTH)` are
`ogr2ogr`'s `-simplify` and `-segmentize`.

```python
groups = terrio.combine("places.gpkg", group_by=["adm0_a3"])
hulls = terrio.concave_hull(groups, 0.3, allow_holes=True)
simple = terrio.simplify("counties.gpkg", 0.01)
dense = terrio.segmentize(simple, 0.1)
multi = terrio.set_geom_type("roads.gpkg", multi=True, dim="XYZ")
ordered = terrio.sort(multi, method="strtree")
```

## Operations

The operations take the command-line options as keyword arguments. Inputs are paths or objects
in memory; outputs are returned, and also written when a destination is given.

```python
terrio.info("dem.tif", stats=True)  # the same dict as `terrio info --json`
terrio.convert("countries.shp", "europe.gpkg", where="CONTINENT = 'Europe'", select=["NAME"])
terrio.convert("https://example.com/cog.tif", "window.tif", srcwin=(0, 0, 512, 512))
terrio.translate("dem.tif", "small.tif", outsize=("25%", "25%"), resampling="average")
terrio.translate("cog.tif", "full.tif", outsize=(512, 512), resampling="average", overview="NONE")
terrio.warp("dem.tif", "dem_4326.tif", t_srs="EPSG:4326", resampling="bilinear")
terrio.mosaic(["n40w106.tif", "n40w107.tif"], "mosaic.tif")
terrio.rasterize("counties.gpkg", "pop.tif", attribute="POPULATION", tr=(0.01, 0.01), ot="Int32")
polygons = terrio.polygonize("landcover.tif", connectedness=8)
shade = terrio.dem("dem.tif", "hillshade", z=2, multidirectional=True)  # like gdaldem
terrio.dem("dem.tif", "color-relief", "relief.tif", color_file="colors.txt", alpha=True)  # by strips, lazy result
ndvi = terrio.calc("(B - A * 1.0) / (B + A * 1.0)", A="landsat.tif", A_band=3, B="landsat.tif", B_band=4,
                   type="Float32")  # like gdal_calc.py, with NumPy's typing rules
dist = terrio.proximity("roads.tif", values=[1], distunits="GEO")
clean = terrio.sieve("landcover.tif", threshold=10, connectedness=8)
max5 = terrio.neighbors("dem.tif", kernel="equal", size=5, method="max")  # like gdal raster neighbors
edges = terrio.neighbors("image.tif", kernel=["gaussian", "edge2"], band=2, ot="Float32")
collar = terrio.clean_collar("scene.tif", "clean.tif", add_alpha=True)  # like gdal raster clean-collar
masked = terrio.nearblack("scene.tif", "nb.tif", colors=["0,0,0"], add_mask=True)  # like nearblack
text = terrio.enhance_luts("image.tif", config="lut.txt")  # gdalenhance -equalize -config lut.txt
eq = terrio.enhance("image.tif", "eq.tif", equalize=True)  # or config="lut.txt", src_scale=(0, 4000)
xs, ys = terrio.transform_points([699960], [3900000], "EPSG:32610", "EPSG:4326")
xs, ys, hs = terrio.transform_points([-100], [40], "EPSG:4979", "EPSG:4326+5773", [100])  # EGM96
xs, ys, hs = terrio.transform_points([135], [-25], "EPSG:7912", "EPSG:7843", [0], s_coord_epoch=2010.0)
terrio.set_grid_search_paths(["/data/proj-grids"])  # grids are then read only from these (and registered ones)
albers = terrio.list_crs("albers", authority="EPSG", kind="projected")  # registry CRSs, for a CRS picker

data = open("dem.tif", "rb").read()
raster = terrio.open_bytes(data, "dem.tif")  # from bytes; to bytes with raster.to_bytes("x.tif")
```

Raster operations with `dst` and the standard output keywords accept `options={...}`, just
like `Raster.write(options=...)` and CLI `--co`. This includes streamed terrain/warp outputs,
mask-writing operations, palette operations and `create_raster`. The options apply to the
destination write; they do not change an in-memory result when `dst` is omitted.
Raster creation-option mappings (including `Raster.write`, grouped netCDF and streaming writers)
reject duplicate names that differ only in case, such as `COMPRESS` and
`compress`, so conflicting aliases cannot depend on a writer's precedence rules.
`create_raster` requires `dst` whenever file-output options, including `options`, are supplied.
For GeoTIFF/COG, `compress` overrides `options["COMPRESS"]`, and `bigtiff=True` forces BigTIFF
even with `options={"BIGTIFF": "NO"}`. Explicit block-size creation options override `tile_size`:
`BLOCKXSIZE`/`BLOCKYSIZE` for tiled GeoTIFF, or `BLOCKSIZE` for COG. Otherwise `tile_size`
supplies the tile dimensions. These GeoTIFF/COG precedence rules also apply to `Raster.write`.
Other formats use their own writer rules; for example, netCDF's `options["COMPRESS"]`
takes precedence over `compress`.
In `calc`, `options` is an output keyword rather than a raster variable name.
VRT outputs from `pansharpen`, `mosaic`, `warp` and color-relief reject `options` because they
describe a virtual operation rather than a driver's pixel storage. Treat creation options as trusted writer
configuration: internal `@`-prefixed keys can name local source files, as with `Raster.write`
and CLI `--co`. Services accepting untrusted dictionaries should allowlist supported options.

```python
terrio.dem("dem.tif", "slope", "slope.tif", options={"COMPRESS": "DEFLATE", "TILED": "YES"})
terrio.translate("dem.tif", "terrain.nc", options={"FORMAT": "NC4", "COMPRESS": "DEFLATE"})
terrio.translate("dem.tif", "terrain.zarr", options={"FORMAT": "ZARR_V3"})
```

`translate` reads rasters by window as `terrio translate` does: a path or a lazy `Raster` is read
for the output alone (the tiles its window touches), from the stored resolution `overview` selects,
as `gdal_translate -ovr`: `"AUTO"` (the default, the overview GDAL's RasterIO chooses for the output
size), `"AUTO-n"`, `"NONE"` (full resolution) or a level from 0. A `Raster` in memory is
translated at full resolution: open it with `terrio.open_raster(path, lazy=True)` for its
overviews to be used, since `open_raster(path)` reads it into memory without them.
Whatever its source, a window read at its own size at a fractional offset is resampled there by
`resampling`, as `terrio translate` describes.
With `"AUTO-n"` and no `outsize` or `tr`, the output keeps the full-resolution window size,
matching GDAL 3.13.3; explicit level numbers use the selected level's resolution.
`"AUTO-0"` is `"AUTO"` in translate and all other resampled readers, including whole-read
fallbacks where `"AUTO"` is accepted. A full-resolution output can use more memory than the
previous overview-sized output; specify `outsize` or `tr` to limit it.
With `dst`, the output is translated whole in memory and then written, where `terrio translate`
writes a GeoTIFF or COG a strip at a time: use the command for outputs larger than memory.

`clean_collar` and `nearblack` return a Byte `Raster`; a mask (`add_mask`, or an input's mask
kept by `clean_collar`) needs `dst`, which holds it, and the returned `Raster` is then read from
`dst` (`has_mask`, `read_mask`). In-place updates are a command-line feature
(`terrio clean-collar --update`). See [neighbors](cli.md#neighbors),
[clean-collar](cli.md#clean-collar-and-nearblack) and [enhance](cli.md#enhance) for the
options, GDAL's arithmetic and the known differences.

### Warp source overviews

```python
lazy = terrio.open_raster("large-cog.tif", lazy=True)
preview = terrio.warp(lazy, ts=(256, 256))  # AUTO by default
finer = terrio.warp(lazy, ts=(256, 256), overview="AUTO-1")
full = terrio.warp(lazy, ts=(256, 256), overview="NONE")
level = terrio.warp(lazy, overview=1)       # levels start at 0
```

Paths, lazy `Raster` objects, and mixed/repeated inputs in mosaics retain their
stored overviews. Selection precedes pixel reads and does not alter the output grid
computed from the original metadata. Only the source resolution changes. Loaded rasters
have no stored overviews: a requested numeric level warns and uses full resolution.
With `resampling="sum"`, an overview has one pixel per block of the source's pixels, which
counts for the source pixels it stands for, `(W / w) * (H / h)` for a `w` x `h` overview of a
`W` x `H` source, so that the sum estimates the source's total at the overview's cost:
`"AUTO"`, `"AUTO-n"` and a level (`overview=1`) read an overview so, and `"NONE"` sums the
full resolution exactly. The estimate's accuracy is described under
[Warp source overviews](cli.md#warp-source-overviews): exact up to rounding for an `average`
overview of floating-point data without nodata when the footprint covers complete overview
cells. Cropping through those cells makes even the total approximate; rounded integer means,
partly nodata blocks and other overview resamplings also introduce approximation.
In a partly valid average-overview block, the valid-pixel mean is also applied to
invalid pixels; small nodata holes can disappear into it. The measured 1.4% to 2.6%
excess in the CLI guide is not a bound: the separate raster in
[#707](https://github.com/opengeos/terrio/issues/707) has 7.9% nodata and overcounts by
7.1% at ×64, versus 0.03% at ×8. An internal mask has the same limitation when its
overview stores only binary validity, without the fraction of valid source pixels. For
complete cells holding exact averages of constant positive valid values, an invalid
fraction `p` wholly in partly valid cells gives relative excess `p / (1 - p)` over the
valid total, rather than `p`; with variable values the nodata fraction alone does not
bound the relative error. Output pixels can also become valid where a full-resolution
sum has no valid source contribution. Select a finer `overview=n` (zero is the first
and finest overview) or `"AUTO-n"`, which can reduce the effect, or `overview="NONE"` for valid
totals and full-resolution nodata coverage. Finer independently built overviews need not
improve the error monotonically when values vary. The mean-based formulas do not apply
to nearest overviews, which may select nodata from a partly valid block.

Give a coarse `sum` a floating-point `ot`: a
`"UInt16"` output stores 65535 for any larger sum. For paths and lazy inputs, a `sum` of one source without a cutline
reads the window of an output too coarse to be split into chunks a strip of rows at a time, as
`terrio warp` does (see [Warp source overviews](cli.md#warp-source-overviews)). The working-memory estimate applies per strip, with at least one row; two strips overlap during reading and computation, in addition to output and decoder buffers. An unmasked, already loaded `Raster` reuses its resident pixels and does not use strip reads or this source-read budget. For paths and lazy inputs, a `sum` whose
source pixels held at once would exceed 4 GiB, as several sources or a cutline hold every source
whole, raises `ResourceLimitError` (a `MemoryError`) before they are read, and so do a `sum` read
of a `WarpedVRT` and a `sum` `terrio.rio.warp.reproject` of a band or dataset: `overview=n` sums a
level (for the `terrio.rio` ones, a source opened with `overview_level=n`, whose pixels count
the same way), and the `TERRIO_WARP_SUM_MAX_BYTES` environment variable, a positive integer
byte count, changes the budget. A `sum` read of a `WarpedVRT` warps a region whose source
window's working memory exceeds its `warp_mem_limit` (64 MB by default) a part at a time, each
part from its own window, with the same pixels, bit for bit: a reduced read of a large source
without overviews holds one part's window at a time, and the budget applies to each part's
window. A single output pixel cannot be split. A region remains whole when either half cannot
be transformed, or when it has several bands with source and output nodata values; these
exceptions can exceed `warp_mem_limit` or cause the sum budget to reject the read. A `sum` read
of a `WarpedVRT` takes a source window in its own type when the VRT's `dtype` holds every value of it (uint16 or
float32 in float64), as its kernel reads each value as a float64 either way; a `dtype` that does
not (float64 in float32, floats in an integer type) converts the window first, and counts it in
both types, which the conversion holds at once. Part sizing deliberately uses the working type's
width, following GDAL's working-memory estimate even when the source type is narrower.
Splitting can read overlapping source tiles repeatedly. The decoded-tile cache normally reuses
them, but a tile larger than its capacity is decoded for each part that needs it. For example,
a 4096 × 4096 Float32 tile exceeds the default 64 MiB cache once entry overhead is included.
Set `TERRIO_CACHEMAX` before opening the source (for example, `128M`) to retain such tiles,
with that additional memory cost, or use a source with smaller tiles or overviews. The warp
memory limit bounds part windows, not codec buffers or the separate decoded-tile cache.
Calls with loaded inputs reuse their existing pixels, including repeated references.
File GCPs/RPCs of the first path are retained in mixed path/`Raster` calls.

A `sum` transforms the corners of each output pixel exactly, whatever the error threshold (`et`,
or `tolerance` in `terrio.rio`), so that each source pixel counts for its share of the output
pixel's area: see [warp](cli.md#warp).

### Geolocation-array warping

`terrio.warp(path, geoloc=True, t_srs="EPSG:4326")` requires the longitude/latitude arrays
already exposed by netCDF/HDF-EOS drivers. Arrays are also used automatically when present.
For coordinate rasters held separately or in memory, supply the first source's arrays directly:

```python
out = terrio.warp(source, t_srs="EPSG:4326", geolocation={
    "x": longitude_raster, "y": latitude_raster,
    "pixel_step": 2, "line_step": 2, "pixel_center": True,
})
```

`x` and `y` each accept a raster or a path. `x_band` and `y_band` select their 1-based
coordinate bands (default 1). Optional `srs` defaults to EPSG:4326;
`pixel_offset`/`line_offset` default to 0 and steps to 1. The center shift is half an array
step. Invalid coordinate cells remain empty. See [CLI warping](cli.md#warp) for the supported
subset and comparison behavior at swath boundaries.

### Fill nodata

```python
filled = terrio.fill_nodata("dem.tif", "filled.tif", max_distance=100,
                           smoothing_iterations=1)
nearest = terrio.fill_nodata("image.tif", band=2, mask="validity.tif",
                            interpolation="nearest")
```

Inputs and masks may also be `terrio.Raster` objects. The result is a new, single-band
raster, retaining the selected band's type, nodata and georeferencing. The mask is zero
at holes and nonzero at donors; without it, the source mask or nodata defines holes.
Distances are in pixels; zero searches the whole image. Smoothing affects only filled
pixels and preserves the first/last rows. Valid source samples remain unchanged.

### Pansharpening

```python
sharp = terrio.pansharpen("pan.tif", "rgbnir.tif", bands=[1, 2, 3], bitdepth=12, nodata=0)
sharp = terrio.pansharpen("pan.tif", ["red.tif", "green.tif", "blue.tif"], weights=[0.6, 0.3, 0.1])
virtual = terrio.pansharpen("pan.tif", ["red.tif", "green.tif", "blue.tif"], "sharp.vrt")
window = virtual.read(window=(100, 200, 64, 64))  # pixel windows read on demand
```

All spectral inputs contribute to intensity; `bands` selects/reorders output bands from
the flattened input sequence. See [pansharpen](cli.md#pansharpen) for interpolation,
working types, nodata, bit depth and extent handling.

### Viewshed

```python
visible = terrio.viewshed("dem_utm.tif", 500000, 4400000, oz=2, tz=0, md=5000)
height = terrio.viewshed("dem_utm.tif", 500000, 4400000, om="GROUND", cc=0)
cumulative = terrio.viewshed("dem_utm.tif", 0, 0, om="ACCUM", os=10)
```

`om="NORMAL"` returns visibility; `DEM` and `GROUND` return minimum visible elevation
or height above the DEM. `cc` is 1 minus refraction. For cumulative visibility, `os`
sets observer spacing in pixels (default 10). Python retains its positional
coordinates: pass `0, 0` placeholders for ACCUM; the CLI permits omitting them.
See [viewshed](cli.md#viewshed) for units, curvature, radius cropping and input nodata behavior.

### Vector gridding and linear referencing

```python
temp = terrio.vector_grid("stations.gpkg", "temp.tif", algorithm="invdist", zfield="temp",
                          power=2, radius=50000, max_points=12)
dem = terrio.vector_grid("samples.gpkg", algorithm="linear", extent=(636000, 848930, 637180, 849500),
                         resolution=(5, 5))
parts = terrio.lineref_create("path.shp", "mstones.shp", "pos", 1000, "parts.gpkg")
pos = terrio.lineref_position(parts, -1.4345, 51.9497)
(x, y, z), = terrio.lineref_coordinates(parts, 15977.724709)
sub = terrio.lineref_subline(parts, 13300, 17400)
```

`vector_grid` takes the algorithm (a name or a GDAL string) and its parameters as keywords
(`power`, `smoothing`, `radius`, `radius1`, `radius2`, `angle`, `min_points`, `max_points`,
`min_points_per_quadrant`, `max_points_per_quadrant`, `nodata`), with `zfield`, `zoffset`,
`zmultiply`, `layers`, `sql`, `where`, `bbox`, `extent`, `size`, `resolution`, `ot` and `crs`.
See [vector-grid](cli.md#vector-grid) and [lineref](cli.md#lineref).

Repeated lookups should reuse `terrio.open_vector(parts_path)` or a Layer. Passing a path,
serialized bytes or GeoJSON each time reopens/reparses the input.

### Compare, pixel info and XYZ

```python
report = terrio.compare("reference.tif", "output.tif", skip_binary=True)
print(report["difference_count"], [d["message"] for d in report["differences"]])
print(report["bands"][0]["pixels_differing"], report["bands"][0]["max_abs_diff"])

values = terrio.pixel_info("dem.tif", [(-105.5, 39.5, "site A")], position_crs="EPSG:4326", resampling="bilinear")
print(values["features"][0]["properties"]["bands"][0]["raw_value"])
print(terrio.pixel_info("dem.tif", position_dataset="sites.gpkg", format="csv"))

terrio.xyz("dem.tif", "dem.xyz", skip_nodata=True)  # number of lines written
text = terrio.xyz("dem.tif", srcwin=(0, 0, 10, 10))
```

`compare` takes paths or `Raster`s (two paths for the binary, RPC and subdataset comparisons)
and returns `terrio compare --json`'s report. `pixel_info` returns GeoJSON (a dict), `json` (a
list), or `csv`, `text` and `value` strings; `xyz` returns the line count written to `dst`, or
the text. See [compare](cli.md#compare), [pixel-info](cli.md#pixel-info) and [xyz](cli.md#xyz)
for options, GDAL agreement and differences.

`pixel_info(..., format="value", field_sep="|")` selects the separator between
band values. `pixel_info(..., dst="samples.gpkg", output_format="GPKG")` also
writes a vector layer of sample points and returns the requested report as usual.
Missing/off-raster samples are retained as null values. Input paths cannot be used
as the output. JavaScript hosts can serialize the GeoJSON result through a Dataset
when they need file bytes; direct local file paths are a native API feature.

### Pixel-wise operations

```python
physical = terrio.unscale("packed.tif", ot="Float64")          # value * scale + offset
rgba = terrio.nodata_to_alpha("landsat.tif", "rgba.tif")       # nodata to an alpha band
groups = terrio.reclassify("nlcd.tif", "11=1;[21,25)=2;DEFAULT=0", ot="Byte")
groups = terrio.reclassify("nlcd.tif", "@mapping.txt")         # one entry per line
pixels = terrio.as_features("dem.tif", "pixels.gpkg", geometry_type="polygon", include_xy=True)
```

These follow GDAL's `gdal raster unscale`, `nodata-to-alpha`, `reclassify` and `as-features`;
see [unscale](cli.md#unscale), [nodata-to-alpha](cli.md#nodata-to-alpha),
[reclassify](cli.md#reclassify) and [as-features](cli.md#as-features).

### Raster management

`create_raster(dst=None, *, like=None, size=None, band_count=None, ot=None,
crs=None, bbox=None, nodata=None, burn=None, metadata=None, ...)` creates an
in-memory raster or a file. File creation accepts writer options, overwrite,
copy_metadata and copy_overviews. `edit_raster(path, ...)` changes CRS, transform,
GCPs, nodata, metadata, statistics and histograms in place. `move_raster(path,
t_srs, s_srs=None, error_threshold=None, ...)` reports georeferencing changes,
and applies them only when the affine error is below the supplied threshold.
Both operations preserve pixel values.

`add_overviews`, `delete_overviews` and `refresh_overviews` manage internal or
external GeoTIFF levels. `update_raster(src, dst, ...)` warps valid source pixels
into an existing file and returns its changed window, or None for no overlap.
Their keyword options follow the CLI, including compression, partial refresh,
clipping geometry and overview updates. Local writable little-endian GeoTIFFs
are required for in-place changes; external overview updates can fail after the
base update commits. External overviews (`external=True`) write only the `.ovr`
sidecar, so the GeoTIFF may be read-only, as with `gdaladdo -ro`. They read that
`.ovr` under `TERRIO_DISABLE_READDIR_ON_OPEN=EMPTY_DIR` too; the `.msk`, `.aux.xml`
and `.ovr.msk` beside the GeoTIFF still follow the option. Overviews are
managed for GeoTIFFs only.

`add_overviews(..., progress=callback)` calls `callback(fraction)` as the
overviews are computed, with a fraction from 0 to 1 that never decreases (1 once
every overview is computed, before they are written). A falsy return value other
than `None` cancels, as with GDAL's Python callbacks, raising
`terrio.errors.CancelledError`; an exception the callback raises cancels too and
propagates. A cancellation changes no file: an existing `.ovr` stays as it was.

```python
def report(fraction):
    print(f"{fraction:.0%}")
    return not stop_requested()

terrio.add_overviews("dem.tif", resampling="average", external=True,
                     compression="DEFLATE", progress=report)
```

COG validation also accepts uploaded TIFF `bytes`. Use
`terrio.validate_cog(data, profile="gdal", full_check=True)` for GDAL 3.13
structural rules and compressed-block leader, trailer, order and mask checks.
The result remains `(valid, errors, warnings)`; the default `rio-cogeo` profile
preserves existing behavior. Automatic full checking applies to local paths
and bytes, and stays off for URLs.

GDAL COG full checking examines blocks in the main TIFF and its internal overviews.
External `.ovr` sidecars make the report invalid and receive header checks only.
Explicit full checks of URLs can make several boundary requests per stored block;
the cost grows with the number of blocks, so automatic mode leaves them off.
Remote full checks stop with an error after 10,000 boundary read calls (excluding
header/table reads and configured HTTP retries); download larger files to validate
their full layout locally.
Full block validation stops at 1,000 findings and reports the limit; remaining
blocks need not be checked once the file is already invalid.
The GDAL profile limits total parsed GDAL metadata to 4 MiB per file and XML nesting
to 128 levels. The default rio-cogeo profile ignores this metadata tag.

### Pictures of data

`terrio.render(src, *, width=None, height=None, max_size=1024, bands=None, colormap=None,
stretch="auto", layers=None, attribute=None)` returns a PNG of a raster or vector dataset. `src`
accepts a path (a raster, or else a vector dataset), a `Raster`, or vector layers (`VectorDataset`,
`Layer`, a list of layers, or GeoJSON as a mapping or as text). A raster is read from its coarsest overview that still has
the pixels needed, so a remote COG transfers a small part of itself. The options are those of
[`terrio preview`](cli.md#preview). See [Layer selection](#layer-selection) for
filename aliases and selecting layers from materialized datasets.

```python
png = terrio.render("https://example.com/dem.tif", colormap="terrain", max_size=512)
open("dem.png", "wb").write(png)
terrio.render("countries.gpkg", attribute="POP_EST", colormap="viridis")
```

In Jupyter, a `Raster` and a `terrio.rio` dataset display as a picture of up to 512 pixels a side
(their `_repr_png_`), without matplotlib; a closed dataset, or one that cannot be drawn, falls back
to its text form.

### Indexed color conversion

`terrio.rgb_to_palette(src, dst=None, *, color_count=256, bit_depth=5,
 dither=True, color_map=None, output_nodata=None, mask=None)` returns an indexed
Byte `Raster`. `src` accepts a path or Raster; `color_map` accepts a palette
raster/path, encoded color-file bytes, or a sequence of RGBA integer entries.
`mask` is a two-dimensional uint8 validity array. `palette_to_rgb(src, dst=None,
rgba=False)` expands to RGB/RGBA; `attach_palette(src, color_map, dst=None)`
changes only the table. `read_color_table(source, filename=None)` returns integer
RGBA lists; `filename` supplies the format hint for encoded bytes.

The three conversion functions accept output format/compression/tile options
and `overwrite=False`. Existing outputs need `overwrite=True`; source path
aliases are protected. Operations materialize the input/output. See
[Palette conversion](cli.md#palette-conversion) for validity rules, formats and
GDAL differences.

Python `Raster.write` and `to_bytes` retain dataset masks for GeoTIFF/COG
outputs. Other writers omit separate dataset masks, as before; explicit masks
passed to unsupported writers are rejected. Any raster with a dataset mask, including materialized
results, is refused for PMTiles/MBTiles writes and their byte serialization,
since that streaming path supports alpha bands rather than separate masks.
Normal raster writes materialize the full raster and its mask; remote inputs
therefore fetch the selected image blocks and mask blocks.

### Serialized VRTs and uploaded sources

```python
virtual = terrio.warp("dem.tif", "projected.vrt", t_srs="EPSG:3857", ts=(512, 512))
window = virtual.read(1, window=(32, 48, 128, 128))
uploaded = terrio.open_vrt(xml, {"dem.tif": image_bytes})
```

`open_vrt` accepts source names mapped to file bytes, paths or `PathLike` objects;
source windows remain lazy. `open_raster(path, lazy=True)` also opens VRT files
lazily. The [serialized VRT support](cli.md#serialized-virtual-rasters) lists the
supported transformers, built-in functions, kernels and masks.

## Analysis and topology

`contour`, `footprint`, `zonal_stats`, `buffer`, `dissolve`, `make_valid`, `check_geometry`,
`singlepart`, `clip_vector`, `layer_algebra`, `check_coverage`, `simplify_coverage`,
`clean_coverage`, `blend`, `identify`, `check_dataset`, `pipeline`, `raster_tiles`, `retile`,
`raster_index`, `vector_index` and `rpftoc_create` follow the matching
[commands](cli.md); see `help(terrio.contour)` for each one's arguments. `list_layers(path)` and
`write_raster(raster, path, ...)` are the plain I/O helpers.

`zonal_stats` reads rasters lazily (paths, URLs, STAC assets or `Raster` objects) and accepts
labelled inputs: dicts, or a pandas DataFrame, with `raster`, `time`, `label` and `band`:

```python
# items: pystac Items, e.g. from pystac_client's search(...).items()
rows = [{"raster": item.assets[band].href, "time": item.datetime.date().isoformat(), "label": band}
        for item in items for band in ["red", "nir"]]
stats = terrio.zonal_stats(fields, rows, stats=["mean", "quantile_25", "quantile_75", "mode"])
# fields named 2024_07_01_red_mean, 2024_07_01_nir_mean, ...
```

Statistics include `variance`, `cv`, `quantile_P`, `mode`, `minority` and `variety`; see the
[command](cli.md#zonal-stats). For coverage-weighted zonal statistics, use `fractional=True`. A second aligned raster
can be supplied as `weights=...`, with `weight_band=1`, then select `weighted_mean`,
`weighted_sum` or `weighted_stddev` in `stats`. See the
[formulas and limits](cli.md#fractional-coverage-and-weighting-rasters).

### Computing statistics from arrays you produce

`ZonalReducer` gives the results of `zonal_stats` for arrays you compute yourself, a strip at a
time, such as a band index over a scene, without writing them to a raster first. Zones are
computed once for the grid; `windows()` lists the `(x, y, width, height)` strips that hold zone
pixels (in strips of `strip_rows` rows, set when the reducer is created: by default terrio's
strip size for one float64 band of this width; give a tiled source's block height to read each
block once), and `stream()` reduces
one band fed those strips in order. Statistics that keep values (median, quantiles, mode, minority,
variety) hold each open zone's values in each stream, 8 bytes per pixel (16 with `fractional`): feed
streams one at a time to bound them. In a stream fed arrays of one 8- or 16-bit integer type, a zone
of at least 256 or 65,536 pixels counts them in a histogram of 2 or 512 KiB instead.

```python
reducer = terrio.ZonalReducer(fields, width, height, transform, "EPSG:32631", stats=["mean", "median"])
stream = reducer.stream()
for i, (x, y, w, h) in enumerate(reducer.windows()):
    red, nir = (a.astype("float64") for a in read_strip(x, y, w, h))  # shape (h, w); no uint16 wrap
    stream.add(i, (nir - red) / (nir + red))  # 0/0 is NaN, left out
ndvi = stream.finish()  # {"mean": array, "median": array}, one value per field
```

`finish()` returns one array per statistic: `count` and `variety` are int64 (`count` is float64
with `fractional=True`), the others float64 with NaN where a zone has no valid pixels. Weighted
statistics need `zonal_stats`, and `t_srs` and `decimal_places` are not supported: the results are
unrounded arrays, without geometries.

## Native multidimensional arrays

See [Native multidimensional arrays](multidim.md) for scientific groups, named dimensions,
lazy coordinate arrays, time/depth slicing, transposition and netCDF/Zarr numeric snapshots.

### Multidimensional mosaics

`terrio.mosaic_multidim(sources, dst=None, *, arrays=None)` builds a lazy
coordinate-aligned mosaic. Each source can be a `MultidimDataset`, a scientific
path/bytes, or a Zarr key/value dictionary. Read coordinate-aligned views through
`output.array(name)`, or write a new netCDF/Zarr snapshot with `dst` or `write`.
Source handles stay alive through the mosaic. See
[Multidimensional mosaics](cli.md#multidimensional-mosaics) for alignment,
priority, type preservation and supported scope.

## Indexed raster catalogs

```python
catalog = terrio.open_catalog("tiles.gti.gpkg", cache_size=16, sort_field="priority")
window = catalog.read(1, window=(100, 200, 256, 256))
```

Catalogs select features through the GeoPackage/FlatGeobuf spatial index and cache a bounded
number of source datasets. `terrio.open_raster("GTI:tiles.gpkg", lazy=True)` also works with
catalog defaults. For in-memory indexes and named source rasters:

```python
catalog = terrio.open_catalog(index_layer, {"left.tif": left, "right.tif": right},
                             tr=(30, 30), sort_field="priority", cache_size=16)
```

The source mapping accepts `Raster` objects and paths, including `PathLike` values. Paths
are opened only when selected and close on cache eviction. Supplied lazy rasters retain their
window/overview reads; loaded buffers and existing readers remain shared with the caller.
Replacing or deleting the original Python object does not replace the catalog's handle.

Options include `layer`, `location_field`, `sort_field`, `sort_descending`, `tr`, `te`, `srs`,
`bands`, `ot`, `nodata`, `alpha_band`, `cache_size` and `resampling`. Missing grid characteristics
come from index metadata or one tile header. Dataset masks, source nodata and zero alpha are
transparent. See [CLI catalogs](cli.md#catalog) for the supported GTI subset.

## File management

```python
terrio.vsi_list("s3://bucket/prefix", recursive=True)        # gdal vsi list
terrio.vsi_list("/vsizip/data.zip", recursive=True, depth=2)
terrio.vsi_copy("data", "s3://bucket/prefix", recursive=True)  # gdal vsi copy
terrio.vsi_sync("data/", "s3://bucket/prefix", recursive=True, strategy="ETag")
terrio.vsi_move("s3://bucket/old", "s3://bucket/new")
terrio.vsi_delete("s3://bucket/prefix", recursive=True)
terrio.vsi_write("/vsimem/a.txt", b"bytes"); terrio.vsi_read("/vsimem/a.txt")
terrio.vsi_stat("gs://bucket/key")  # None when nothing is there
```

The `terrio vsi` commands as functions: entries are dictionaries (`name`, `type`, `size`,
`modified` in seconds since the epoch, `last_modification_date`, `permissions`, `etag`), and the
copy, sync, move and delete functions return a report (`copied`, `bytes`, `unchanged`,
`deleted`, `errors`). `/vsimem/` files last as long as the process. Locations, semantics and the
refusals of dangerous deletions are those of [vsi](cli.md#vsi).

VSI listing bindings return structured flat entries. Nested tree presentation (`--tree`)
is currently available through the CLI; JSON serialization flags apply to CLI output.

### Seek-optimized ZIP archives

```python
terrio.sozip_create(["roads.gpkg", "dem.tif"], "data.zip", junk_paths=True)
terrio.sozip_list("data.zip")  # dictionaries with names, sizes and SOZip chunk sizes
terrio.sozip_validate("data.zip")  # valid, sozip_files, messages, errors
terrio.sozip_optimize("plain.zip", "seek.zip", enable="yes", chunk_size=65536)
```

The four functions share the [CLI options and limitations](cli.md#vsi-sozip).
Creation and optimization return `added`, `seek_optimized` and `bytes` counts.
Native outputs are replaced only after the entire operation succeeds.

### Dataset file management

`terrio.dataset_files(source, *, format=None)` returns the driver, owned files and a
`directory` flag. `dataset_copy(source, destination, *, format=None, overwrite=False, allow_external_references=False)`
and `dataset_rename(source, destination, *, format=None, overwrite=False, allow_external_references=False)` manage the complete file set and return affected paths
and copied bytes. Paths may be strings or `os.PathLike` values, including VSI paths.
`dataset_delete(sources, *, format=None)` preflights all inputs and returns a
list of reports. VRT references stay in the source directory tree by default;
`allow_external_references=True` permits trusted external assets. Deleting a VRT
preserves referenced rasters. Reports include `recovery_files`: a nonempty list
means the operation committed but cleanup left staged files. I/O releases the GIL.
See the [CLI file-management section](cli.md#dataset-copy-dataset-rename-dataset-delete-dataset-files) for staging, rollback, overwrite and format rules.

`parquet_create_metadata(inputs, output, *, overwrite=False)` accepts a sequence of string
or pathlib partition paths and returns a dictionary containing `files`, `row_groups`,
`rows`, `bytes` and `omitted_metadata_keys`. It writes a metadata-only file with relative
partition references, merged GeoParquet bounds/types and regenerated Arrow schema metadata.
Inputs, including referenced column files, are protected from overwrite. Schemas and CRS
must agree; data pages are not validated. See [CLI supported scope](cli.md#parquet-metadata-files).

### GeoPackage maintenance

`gpkg_repack(path)` compacts a native or `/vsimem/` GeoPackage in place and returns
`bytes_before`, `bytes_after`, `page_size`, `pages_before` and `pages_after`.
`gpkg_repack_bytes(data)` returns a compacted `bytes` copy of a self-contained GeoPackage,
leaving the input unchanged. Both preserve encoded table/index records and schema SQL.
See [supported scope and storage safety](cli.md#geopackage-repacking).

## COG validation

```python
ok, errors, warnings = terrio.validate_cog("dem.tif")  # rio_cogeo.cog_validate's result
terrio.validate_cog("https://example.com/dem.tif", strict=True)  # warnings fail too
```

The checks and messages of `rio_cogeo.cogeo.cog_validate`, read from the file's structure without
GDAL, for local files (a `str` or `os.PathLike`), URLs and S3 objects (range requests). See
[Rust](rust.md#reading-windows-lazily).

## GeoPackage validation

```python
report = terrio.validate_gpkg("data.gpkg", full_check=True, strict=False)
report_from_bytes = terrio.validate_gpkg(gpkg_bytes, unicode_case=False)
print(report["valid"], report["complete"], report["errors"])
```

Returns a dictionary with `valid`, `complete`, `version`, `errors`, `warnings` and
`unsupported_checks`. Paths accept native or `/vsimem/` files; uploaded bytes are
also supported. The [CLI reference](cli.md#geopackage-validation) documents the
supported rules, SQL subset, case options, memory use and checkpoint requirement.

## Runtime configuration

Configuration options are environment variables, process-wide, or scoped with
`terrio.rio.Env`. These functions inspect and limit what a process reads:

```python
from terrio.rio import Env

with Env(GDAL_HTTP_TIMEOUT="30"):
    terrio.config_options()  # {"GDAL_HTTP_TIMEOUT": "30"}: what this thread's scopes set

# Options for the files under one prefix, as GDAL's VSISetPathSpecificOption.
terrio.set_path_specific_option("/vsis3/my-bucket/", "AWS_REGION", "us-west-2")
terrio.get_path_specific_option("/vsis3/my-bucket/a.tif", "AWS_REGION")  # "us-west-2"
terrio.clear_path_specific_options()

# Count what remote reads and writes transfer, in total and by host and file.
terrio.reset_network_stats()
terrio.info("https://example.com/cog.tif")
terrio.network_stats()  # {"request_count": 1, "received_bytes": 16384, "sent_bytes": 0, "hosts": {...}, ...}
terrio.clear_http_cache()

# The cache of decoded GeoTIFF/COG chunks (GDAL's GetCacheMax/SetCacheMax): TERRIO_CACHEMAX, else
# GDAL_CACHEMAX, else 64 MiB; 0 turns it off.
terrio.block_cache_max()  # 67108864
terrio.set_block_cache_max(256 << 20)
terrio.clear_block_cache()

# Limit the drivers this process may open, for files you do not trust.
terrio.identify_drivers("data.gpkg")  # (["GPKG"], "GPKG"): by name, by content
terrio.check_drivers("data.gpkg", ["GPKG"])  # raises UnsupportedError for another driver
terrio.set_allowed_drivers(["GTiff"], kind="raster")
terrio.allowed_drivers("raster")  # ["GTIFF"]
```

`set_allowed_drivers` lasts for the process: later calls narrow the list, and lifting it with
`None` raises `InvalidArgumentError`. Without `kind`, the list applies to every kind of dataset:
rasters, vectors and point clouds (`kind` is `"raster"`, `"vector"` or `"pointcloud"`).
`GDAL_HTTP_HEADERS` set for a prefix is sent only to that prefix, never to another host a redirect
leads to. `TERRIO_HTTP_FOLLOW_CROSS_ORIGIN=NO`, in the environment or in an `Env`, keeps reads from
following a redirect to another origin at all. See [Remote Data](remote-data.md) for the options
themselves.

The `TERRIO_ALLOWED_DRIVERS` option limits the drivers too, in the environment or in an `Env`. A
driver must then be in both lists, and `allowed_drivers()` gives the drivers both allow. The
option's value is a comma-separated list of driver names in any case. Set but empty, it allows no
driver. In an `Env` it only narrows the list outside, and unset there, it leaves that list in
force. `allowed_drivers()` is the way to see what the option allows: `terrio.pyogrio`'s
`get_gdal_config_option` reports a set but empty value as unset (`None`), and `config_options()`
shows only the values `Env` scopes hold, `''` where a scope allows nothing. See
[Allowed drivers](remote-data.md#allowed-drivers).

GeoTIFF readers refuse a tile or strip that decodes to more than 256 MiB
(`ResourceLimitError`), a bound on what a malformed header can make a read allocate. A
pixel-interleaved file of many bands reaches it with ordinary tiles. For trusted files, the
`TERRIO_TIFF_MAX_CHUNK_BYTES` option, a positive number of bytes, raises it for the rasters
opened while it is set, in the environment or an `Env`. Size it to the file's chunks (tile
width x height x bands x bytes per sample when pixel-interleaved), not to an unbounded value:
a read may hold about the limit times the chunks it decodes in parallel. The refusal names the
option:

```python
with Env(TERRIO_TIFF_MAX_CHUNK_BYTES=str(1 << 30)):
    da = terrio.xarray.open_rasterio("pixel_interleaved_1488_bands.tif")
```

A GeoTIFF read whole or by window (`terrio.open_raster`, `Raster.read`, `terrio.rio`'s `read`)
raises `ResourceLimitError` before its buffer is allocated when the process cannot hold it (more
than the physical memory, or than the limit of its cgroup or its address space), and so does
`terrio.translate` or `convert` to a GeoTIFF or COG whose writer would hold a row of blocks of
every band that cannot fit. `terrio.rio` preserves `ResourceLimitError` (a `MemoryError`)
through open, read, resampled read and mask reads, so callers can distinguish a resource refusal
from an ordinary `RasterioIOError`. For files you do not trust, `TERRIO_MAX_DECLARED_BYTES` (width x height x bands x bytes
per sample) and `TERRIO_MAX_BANDS`, positive integers in the environment or an `Env`, refuse a
raster that declares more when it is opened, whatever its format, with `ResourceLimitError`
naming the option. See [Rasters](formats.md#rasters) for declaration checks and reader details:

```python
with Env(TERRIO_MAX_DECLARED_BYTES=str(1 << 30), TERRIO_MAX_BANDS="64"):
    src = rio.open("upload.tif")  # ResourceLimitError when it declares more
```

## Typing

The package ships type stubs (`py.typed`), checked with `mypy --strict` and `stubtest`:

```sh
cd python && mypy --strict tests/typing/usage.py
cd "$(mktemp -d)" && python -m mypy.stubtest terrio  # outside the source tree
```

Errors raise `terrio.TerrioError` subclasses, which also derive from the matching built-in
exceptions: `NotFoundError` is a `FileNotFoundError`, `TerrioIOError` an `OSError`, and
`InvalidArgumentError`, `CRSError`, `UnsupportedError` and `InvalidDataError` are `ValueError`s.
`ResourceLimitError` is a `MemoryError`: reading exceeded a resource budget or available
allocation capacity, including decoded-byte limits.

## Building from source

```sh
cd python
uv venv && uv pip install maturin numpy pytest
uv run maturin develop --release
uv run pytest
```

## OGC API Features

`open_vector` reads explicit `OAPIF:` URLs through the shared Rust reader:

```python
places = terrio.open_vector(
    "OAPIF:https://demo.pygeoapi.io/master/collections/lakes",
    options={"PAGE_SIZE": "3", "OAPIF_MAX_FEATURES": "4"},
)
places.layers[0].write("lakes.gpkg")
```

Use `layers` and `bbox` for collection and spatial selection. The GDAL-style
open options, CRS/axis rules, schema handling and bounded eager paging are
described in the [CLI OGC API Features section](cli.md#ogc-api-features). `OAPIF_SKIP_FEATURES` and
`OAPIF_MAX_FEATURES` apply a per-collection range.
