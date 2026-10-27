# Python

Python bindings for [terrio](https://github.com/opengeos/terrio): read, write, convert and
reproject geospatial rasters and vector datasets in pure Rust, without GDAL. Wheels need only
NumPy (and the small `affine` package).

```sh
pip install terrio
```

The [quickstart notebook](notebooks.md) runs these functions on real data, in Google Colab or locally.

Coming from rasterio? `import terrio.rio as rasterio` runs typical rasterio scripts unchanged: see
[Coming from rasterio](rasterio.md).

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
# PMTiles and MBTiles raster tiles too: zoom levels are overviews (PMTiles read in place).
tiles = terrio.open_raster("https://example.com/tiles.pmtiles", lazy=True)
# JPEG 2000 (Sentinel-2 bands) too: resolution levels are overviews, windows decode their tiles.
b04 = terrio.open_raster("T10SEG_20240630T185919_B04.jp2", lazy=True)
patch = b04.read(1, window=(2000, 6000, 1024, 1024))
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

## Vector data

```python
ds = terrio.open_vector("countries.gpkg")  # or .shp, .geojson, .fgb, .parquet, .kml, .zip...
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
terrio.convert("more.shp", "world.gpkg", layer_name="countries", overwrite=True)
```

The file is changed in place, as SQLite changes it: only the pages that change are written (new
rows, R-tree nodes, the tables describing the layer), in a transaction protected by a rollback
journal, so appending to a large GeoPackage costs about as much as the features appended, and a
process stopped in the middle leaves the file as it was (SQLite, GDAL and terrio play the journal
back).

### Streams

Layers larger than memory are read a chunk of features at a time with `terrio.open_stream`
(GeoPackage, FlatGeobuf, GeoParquet, Shapefile, GeoJSON, GeoJSONSeq and CSV; other formats are
read whole and handed out in chunks), and `terrio.convert` streams vector conversions between
those formats (all but GeoPackage as outputs), as the command line does:

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

## Operations

The operations take the command-line options as keyword arguments. Inputs are paths or objects
in memory; outputs are returned, and also written when a destination is given.

```python
terrio.info("dem.tif", stats=True)  # the same dict as `terrio info --json`
terrio.convert("countries.shp", "europe.gpkg", where="CONTINENT = 'Europe'", select=["NAME"])
terrio.convert("https://example.com/cog.tif", "window.tif", srcwin=(0, 0, 512, 512))
terrio.translate("dem.tif", "small.tif", outsize=("25%", "25%"), resampling="average")
terrio.warp("dem.tif", "dem_4326.tif", t_srs="EPSG:4326", resampling="bilinear")
terrio.mosaic(["n40w106.tif", "n40w107.tif"], "mosaic.tif")
terrio.rasterize("counties.gpkg", "pop.tif", attribute="POPULATION", tr=(0.01, 0.01), ot="Int32")
polygons = terrio.polygonize("landcover.tif", connectedness=8)
shade = terrio.dem("dem.tif", "hillshade", z=2, multidirectional=True)  # like gdaldem
terrio.dem("dem.tif", "color-relief", "relief.tif", color_file="colors.txt", alpha=True)
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
terrio.set_grid_search_paths(["/data/proj-grids"])  # where datum shift grids are looked up

data = open("dem.tif", "rb").read()
raster = terrio.open_bytes(data, "dem.tif")  # from bytes; to bytes with raster.to_bytes("x.tif")
```

`clean_collar` and `nearblack` return a Byte `Raster`; a mask (`add_mask`, or an input's mask
kept by `clean_collar`) needs `dst`, which holds it, and the returned `Raster` is then read from
`dst` (`has_mask`, `read_mask`). In-place updates are a command-line feature
(`terrio clean-collar --update`). See [neighbors](cli.md#neighbors),
[clean-collar](cli.md#clean-collar-and-nearblack) and [enhance](cli.md#enhance) for the
options, GDAL's arithmetic and the known differences.

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
```

`om="NORMAL"` returns visibility; `DEM` and `GROUND` return minimum visible elevation
or height above the DEM. `cc` is 1 minus refraction. See [viewshed](cli.md#viewshed) for
units, curvature, radius cropping and input nodata behavior.

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
copy. See the CLI documentation for atomicity and metadata preservation limits.

The vector dataset copy operations `concat`, `rename_layer` and `edit_vector` refuse an
existing destination path. Choose a new output path; use `in_place=True` for supported
GeoPackage edits.

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

## Typing

The package ships type stubs (`py.typed`), checked with `mypy --strict` and `stubtest`:

```sh
cd python && mypy --strict tests/typing/usage.py
cd "$(mktemp -d)" && python -m mypy.stubtest terrio  # outside the source tree
```

Errors raise `terrio.TerrioError` subclasses, which also derive from the matching built-in
exceptions: `NotFoundError` is a `FileNotFoundError`, `TerrioIOError` an `OSError`, and
`InvalidArgumentError`, `CRSError`, `UnsupportedError` and `InvalidDataError` are `ValueError`s.

## Building from source

```sh
cd python
uv venv && uv pip install maturin numpy pytest
uv run maturin develop --release
uv run pytest
```

## Warp source overviews

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
Calls with loaded inputs reuse their existing pixels, including repeated references.
File GCPs/RPCs of the first path are retained in mixed path/`Raster` calls.

## SQL queries

`terrio.sql(src, query, layers=None)` returns a `Layer`. Inputs may be a vector path,
`VectorDataset`, `Layer`, list of layers, or GeoJSON. It implements the shared
[OGR SQL subset](sql.md), including computed columns, DISTINCT, summaries and attribute joins.

```python
result = terrio.sql("countries.gpkg", "SELECT ADMIN, POP_EST/1000000.0 AS millions FROM countries")
summary = terrio.sql("countries.gpkg", "SELECT COUNT(*) AS n, SUM(POP_EST) AS population FROM countries")
```

Raw GeoJSON inputs use the layer name `layer`; use `Layer.from_geojson(..., name=...)`
(Python) or `fromGeoJSON(src, name)` (JavaScript) to assign another name.

## Vector geometry operations

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

## Geolocation-array warping

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

## Fill nodata

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

## Native multidimensional arrays

See [Native multidimensional arrays](multidim.md) for scientific groups, named dimensions,
lazy coordinate arrays, time/depth slicing, transposition and netCDF/Zarr numeric snapshots.

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

## Serialized VRTs and uploaded sources

```python
virtual = terrio.warp("dem.tif", "projected.vrt", t_srs="EPSG:3857", ts=(512, 512))
window = virtual.read(1, window=(32, 48, 128, 128))
uploaded = terrio.open_vrt(xml, {"dem.tif": image_bytes})
```

`open_vrt` accepts source names mapped to file bytes, paths or `PathLike` objects;
source windows remain lazy. `open_raster(path, lazy=True)` also opens VRT files
lazily. The [serialized VRT support](cli.md#serialized-virtual-rasters) lists the
supported transformers, built-in functions, kernels and masks.
