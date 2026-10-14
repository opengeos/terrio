# terrio

Python bindings for [terrio](https://github.com/opengeos/terrio): read, write, convert and
reproject geospatial rasters, vector datasets and point clouds in pure Rust, without GDAL. Wheels need only
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

out = terrio.Raster.from_array(elevation * 3.28084, dem.transform, crs=dem.crs, nodata=dem.nodata)
out.write("dem_feet.tif", compress="DEFLATE")
out.write("dem_feet_cog.tif", driver="COG")
out.write("dem.zarr", options={"FORMAT": "ZARR_V3", "GEOREFERENCING_CONVENTION": "SPATIAL_PROJ"})  # GeoZarr

# STAC: a mosaic of an asset over a search's items (or an item's asset, 'STAC:"item.json":red'), read by window.
red = terrio.open_raster('STACIT:"https://earth-search.aws.element84.com/v1/search?collections=sentinel-2-l2a&bbox=-105.05,40.5,-104.95,40.6&datetime=2023-07-05":asset=red', lazy=True)
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
terrio.write_vector(collection, "countries.geojson")  # GeoJSON dicts, GeoDataFrames...
```

## Point clouds

LAS 1.0 to 1.4 (point formats 0 to 10, extra bytes), LAZ and COPC. Remote COPC files are read in
place: only the octree nodes a read needs are fetched, with HTTP range requests.

```python
f = terrio.open_points("https://s3.amazonaws.com/hobu-lidar/autzen-classified.copc.laz")
f.format, f.point_count, f.crs, f.bounds, f.header, f.dimensions
f.info(stats=True)  # the summary `terrio info --json --stats` prints: classes, returns...
points = f.read(bounds=(637000, 851000, 637500, 851500), max_level=4)  # or resolution=, max_points=

points = terrio.read_points("tile.laz", classes=[2], returns=["last", "only"])
z = points["z"]  # NumPy arrays: x, y, z (scaled float64), intensity (uint16), classification...
points.to_dict()  # {name: array}
points.to_numpy()  # a structured array
points.write("ground.copc.laz")  # LAS, LAZ or COPC
dem = points.grid(1.0, stat="idw")  # a Raster (min, max, mean, idw, count, stdev; like PDAL)
points.to_layer(dimensions=["z", "intensity"]).to_geopandas()
terrio.convert_points("tile.laz", "tile_wgs84.laz", t_srs="EPSG:4326")
terrio.convert_points("tile.laz", "ground.gpkg", classes=[2])  # a layer of points

terrio.PointCloud.from_dict({"x": xs, "y": ys, "z": zs, "intensity": i}, crs="EPSG:32610")
```

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

Errors raise `terrio.TerrioError` subclasses, which also derive from the matching built-in
exceptions: `NotFoundError` is a `FileNotFoundError`, `TerrioIOError` an `OSError`, and
`InvalidArgumentError`, `CRSError`, `UnsupportedError` and `InvalidDataError` are `ValueError`s.

## Development

```sh
cd python
uv venv && uv pip install maturin numpy pytest
uv run maturin develop
uv run pytest                                    # synthetic tests
TERRIO_TEST_DATA=../../terrio-data uv run pytest # real files, compared with GDAL when installed
```
