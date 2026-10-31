# terrio

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/opengeos/terrio/main/docs/assets/terrio-wordmark-dark.svg">
    <img src="https://raw.githubusercontent.com/opengeos/terrio/main/docs/assets/terrio-wordmark.svg" alt="terrio" height="72">
  </picture>
</p>

[![PyPI](https://img.shields.io/pypi/v/terrio.svg)](https://pypi.org/project/terrio/) [![CI](https://github.com/opengeos/terrio/actions/workflows/ci.yml/badge.svg)](https://github.com/opengeos/terrio/actions/workflows/ci.yml) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](https://github.com/opengeos/terrio/blob/main/LICENSE)

Python bindings for [terrio](https://github.com/opengeos/terrio): read, write, convert and
reproject geospatial rasters, vector datasets and point clouds in Rust, without installing
GDAL. Wheels need only NumPy (and the small `affine` package).

```sh
pip install terrio
# or, from conda-forge
conda install -c conda-forge terrio
mamba install -c conda-forge terrio
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

`terrio.xarray` is rioxarray's API without installing GDAL (`pip install "terrio[xarray]"`):
rasters open lazily as DataArrays with rioxarray's coordinates, attributes and encoding, and the
`.terrio` accessor has rioxarray's `.rio` methods. See
[xarray](https://terrio.opengeos.org/xarray/).

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

Remote query strings are retained in stored locations, so sharing such a catalog
can share signed access credentials. Python emits a `UserWarning` without echoing
the query. Use credential-free references and environment credentials for shared
catalogs, or separate a stored locator from the signed URL used to inspect it:

```python
index = terrio.raster_index({"/vsis3/bucket/tile.tif": signed_url})
```

This stores the credential-free mapping key; readers need their own credentials.

### Contours, footprints and geometry analysis

```python
lines = terrio.contour(raster, interval=10, with_z=True, field_name="elevation")
area = terrio.footprint(raster, bands=[1], singlepart=True, attributes={"source": "scene"})
report = terrio.check_geometry(layer)
repaired = terrio.make_valid(layer)
buffered = terrio.buffer(repaired, 100, quadrant_segments=30)
groups = terrio.dissolve(buffered, fields=["region"])
```

These functions return a `Layer`. Raster inputs accept a path or open raster,
including a lazy raster; contour and footprint materialize the pixels for the
full operation. Contour supports band selection, interval/offset, additional
fixed levels, nodata override or ignoring nodata, 3D vertices and custom
elevation/layer names. Footprint supports selected-band union or intersection,
a mask on the same grid, singlepart output and explicit text attributes. No
source-path attribute is added automatically in the bindings.

Omitted or empty footprint bands selects every band, combined by union unless
intersection is requested. Footprint attribute fields are sorted alphabetically.
Footprint validity uses finite samples and the dataset nodata value. Alpha bands
and external mask files are not interpreted implicitly; supply an explicit mask
when those pixels should be excluded.

Vector inputs accept paths, layers, datasets or GeoJSON; pass `layer="name"`
when the input has several layers. These functions return one selected `Layer`;
JavaScript deliberately processes all layers and returns a `Dataset`. Shared
byte exports to single-layer formats now reject multiple layers instead of
silently writing the first; select a layer explicitly before exporting.
Buffers use planar CRS units and round
joins/caps. Dissolve optionally groups by field names. Geometry checks retain
source geometries and add validity/reason fields. Repair preserves attributes,
IDs, CRS and already-valid Z/M; repaired geometry is constructed in XY. These
call the same Rust engines as the CLI and retain their documented limits.

### Chained processing

`terrio.pipeline(command, inputs=None, *, write=False, base_file=None, input_kind=None)` evaluates
supported GDAL-style steps through the Rust pipeline engine. `command` is a step
string or saved GDALG JSON text. Steps follow POSIX quoting, so a backslash outside
quotes escapes the next character: quote Windows paths in single quotes
(`'C:\data\in.tif'`) or use forward slashes. Inputs can use native paths or aliases mapped to
`Raster`, vector objects, or paths:

```python
result = terrio.pipeline(
    "read counties ! filter --where 'population > 10000' "
    "! select name,_ogr_geometry_ ! write selected.gpkg",
    {"counties": counties},
)
selected = result.dataset  # Raster or VectorDataset
payload = result.output_bytes()["selected.gpkg"]  # creates no file
result.write_outputs()  # explicit persistence, with overwrite checks
```

`write=True` persists requested outputs before returning. `output_names` lists
write and named materialize destinations. A saved-definition output has no final
dataset and can be exported with `output_bytes()` without reading its sources.
For imported JSON text, `base_file` supplies the saved file's location for relative
references. Raster band/crop views retain lazy window access; transformations and
vector results reside in memory. Include `_ogr_geometry_` in vector `select` to
retain geometry and CRS. See the CLI pipeline reference for supported steps and
operation limits.

Use `input_kind="raster"` or `input_kind="vector"` to force the input family;
`"auto"` chooses detection. A `raster` or `vector` prefix in the command string
also selects a family. An explicit `input_kind` overrides that prefix.

## Retile and publish raster folders

```python
report = terrio.retile(
    ["west.tif", "east.tif"], "retiled",
    tile_size=[512, 512], overlap=16, levels=3,
    resampling="bilinear", csv="tiles.csv", tile_index="tiles.gpkg",
)
```

Inputs may be paths or open `Raster` objects, including lazy rasters. Later
inputs overwrite earlier ones, including nodata. Retile requires matching CRS,
band counts and north-up affine grids. `dry_run=True` returns the metadata plan;
`resume=True` fills missing files. GTiff and COG, row directories, pyramid-only
output, type conversion and writer options are supported. Pyramids read the
previous emitted level with a bounded cache; source overviews are ignored.
Existing resumed parents supply pixels for missing children. The default filename
prefix is the first source path stem, or `input_1` when the first source is an
in-memory `Raster`. Set `prefix="name"` to choose it explicitly.

`terrio.raster_tiles(source, output, publication={"viewers": ["all"], "kml": True})`
adds applicable Leaflet/OpenLayers pages, MapML, STACTA and KML companions.
`publication.url` names the tile directory itself. Serve the directory over HTTP
to use the viewers. See the [CLI reference](../docs/cli.md#retile) for supported
options, memory limits, resume rules and measured GDAL differences.
