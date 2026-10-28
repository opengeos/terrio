# terrio

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/opengeos/terrio/main/docs/assets/terrio-wordmark-dark.svg">
    <img src="https://raw.githubusercontent.com/opengeos/terrio/main/docs/assets/terrio-wordmark.svg" alt="terrio" height="72">
  </picture>
</p>

[![PyPI](https://img.shields.io/pypi/v/terrio.svg)](https://pypi.org/project/terrio/) [![CI](https://github.com/opengeos/terrio/actions/workflows/ci.yml/badge.svg)](https://github.com/opengeos/terrio/actions/workflows/ci.yml) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/opengeos/terrio/blob/main/LICENSE)

Python bindings for [terrio](https://github.com/opengeos/terrio): read, write, convert and
reproject geospatial rasters, vector datasets and point clouds in pure Rust, without GDAL. Wheels need only
NumPy (and the small `affine` package).

```sh
pip install terrio
```

It installs the `terrio` command line too (`terrio info`, `convert`, `warp`, `translate`...; see
[Command Line](https://terrio.opengeos.org/cli/)):

```sh
terrio convert countries.shp europe.gpkg --where "CONTINENT = 'Europe'"
```

## Coming from rasterio

rasterio scripts run with an import change: `terrio.rio` follows rasterio's API (datasets,
windows, CRSs, features, warping, merging, masking, warped VRTs, memory files), compared with
rasterio on real files. See [Coming from rasterio](https://terrio.opengeos.org/rasterio/).

```python
import terrio.rio as rasterio

with rasterio.open("https://example.com/cog.tif") as src:  # read lazily, by window
    profile = src.profile
    dem = src.read(1, out_shape=(512, 512), masked=True)  # through the overviews
```

## xarray

`terrio.xarray` is rioxarray's API without GDAL (`pip install "terrio[xarray]"`): rasters open
lazily as DataArrays with rioxarray's coordinates, attributes and encoding, and the `.terrio`
accessor has rioxarray's `.rio` methods. See [xarray](https://terrio.opengeos.org/xarray/).

```python
import xarray as xr
import terrio.xarray as txr

da = txr.open_rasterio("https://example.com/cog.tif", masked=True, chunks=True)
ds = xr.open_dataset("dem.tif", engine="terrio")
sub = da.terrio.clip_box(-105.5, 39.5, -105.0, 40.0, crs="EPSG:4326")  # fetches its tiles only
sub.terrio.reproject("EPSG:32613", resampling="bilinear").terrio.to_raster("sub.tif", driver="COG")
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
terrio.write_vector(collection, "countries.geojson")  # GeoJSON dicts, GeoDataFrames...
```

## Point clouds

LAS 1.0 to 1.4 (point formats 0 to 10, extra bytes, waveforms), LAZ, COPC, EPT, PLY and E57.
Remote COPC files and EPT datasets are read in place: only the octree nodes a read needs are
fetched. Files larger than memory are read and written in chunks (`PointCloudFile.chunks`,
`PointCloudWriter`); PDAL's filters (`smrf`, `outlier`, `range`, `sample`, `voxel_downsize`,
`voxel_center_nearest_neighbor`) and TIN and nearest-neighbour gridding are `PointCloud`
methods.

```python
f = terrio.open_points("https://s3.amazonaws.com/hobu-lidar/autzen-classified.copc.laz")
f.format, f.point_count, f.crs, f.bounds, f.header, f.dimensions
f.info(stats=True)  # the summary `terrio info --json --stats` prints: classes, returns...
points = f.read(bounds=(637000, 851000, 637500, 851500), max_level=4)  # or resolution=, max_points=

points = terrio.read_points("tile.laz", classes=[2], returns=["last", "only"])
z = points["z"]  # NumPy arrays: x, y, z (scaled float64), intensity (uint16), classification...
points.to_dict()  # {name: array}
points.to_numpy()  # a structured array
points.write("ground.copc.laz")  # LAS, LAZ, COPC, PLY or E57
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

Coordinate-operation safeguards: `transform_points(..., allow_ballpark=False,
only_best=True)`; `convert(..., ct_opt={"ALLOW_BALLPARK": "NO", "ONLY_BEST": "YES"})`;
`warp(..., to={"ONLY_BEST": "YES"})`. `transform_points(..., promote_to_3d=True)` (or
`"PROMOTE_TO_3D": "YES"` in `ct_opt`) takes 2D CRSs' heights as ellipsoidal heights, as GDAL
does after `PromoteTo3D`. Warnings identify horizontal datum fallbacks,
missing grids and operations outside their area. `warp(..., kernel_version="3.13")`
selects GDAL 3.13 resampling behavior; the default remains `"3.12"`.

`terrio.blend(color, overlay, dst=None, operator="multiply", opacity=75)` composes
same-sized Byte rasters or paths, with GDAL's ten raster blend operators and integer
opacity percentages. Alpha controls transparency; nodata/masks are ignored. Inputs
and output are materialized; see the CLI blend reference for band-count rules.


### Footprint indexes and GTI catalogs

```python
index = terrio.raster_index(
    ["tiles/"], recursive=True, filename_filters=["*.tif"], absolute_path=True,
    output_crs="EPSG:4326", band_count=1, ot="Float32", nodata=[-9999],
)
index.write("catalog.gpkg")
print(index.count, index.metadata)
polygons = index.layer
vectors = terrio.vector_index(
    ["roads.gpkg"], source_layer_indexes=[0], output_crs="EPSG:4326",
)
# In-memory sources use explicit stored-location labels.
index = terrio.raster_index({"tile.tif": raster})
encoded = index.to_bytes("index.fgb")
```

`raster_index`/`vector_index` return `DatasetIndex` (count, layer, metadata,
`to_bytes`, `write`); optional `dst` writes the result. Options follow the
[CLI index subset](../docs/cli.md#rastervector-indexes-and-gti-creation) in snake_case;
repeatable options are lists: `filename_filters`, `source_layer_names`,
`source_layer_indexes`, `color_interpretations`. `fetch_metadata` is a list of
`(key, field, type)` triples, and `metadata` is a dictionary. Non-finite nodata
values use strings (`["nan"]`). Mappings accept Raster/path values for raster
indexes and Layer/VectorDataset/GeoJSON/path values for vector indexes; path discovery
options apply only to path inputs. Retain `DatasetIndex` when writing GTI catalogs:
writing its ordinary `layer` alone does not carry the GTI metadata. Multi-file
outputs require a fresh basename; in-memory Shapefiles use `index.shp.zip`.
