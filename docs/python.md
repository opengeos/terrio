# Python

Python bindings for [terrio](https://github.com/opengeos/terrio): read, write, convert and
reproject geospatial rasters and vector datasets in pure Rust, without GDAL. Wheels need only
NumPy.

```sh
pip install terrio
```

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

The updated file is written anew and then replaces the original (terrio's SQLite writer writes
whole files), so appending to a large GeoPackage costs a rewrite of it.

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
xs, ys = terrio.transform_points([699960], [3900000], "EPSG:32610", "EPSG:4326")

data = open("dem.tif", "rb").read()
raster = terrio.open_bytes(data, "dem.tif")  # from bytes; to bytes with raster.to_bytes("x.tif")
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

## Building from source

```sh
cd python
uv venv && uv pip install maturin numpy pytest
uv run maturin develop --release
uv run pytest
```
