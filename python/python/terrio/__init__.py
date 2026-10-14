"""terrio: pure-Rust geospatial raster and vector data I/O, for Python.

Read, write and convert rasters and vector datasets in many formats, local or remote, without
GDAL::

    import terrio

    dem = terrio.open_raster("dem.tif")
    array = dem.read(1)  # numpy array, (rows, cols)

    countries = terrio.open_vector("countries.gpkg")
    layer = countries.layers[0]
    collection = layer.to_geojson()

    terrio.convert("countries.shp", "europe.gpkg", where="CONTINENT = 'Europe'")
    terrio.warp("dem.tif", "dem_4326.tif", t_srs="EPSG:4326", resampling="bilinear")
"""

from terrio._terrio import (
    Layer,
    Raster,
    VectorDataset,
    __version__,
    convert,
    drivers,
    info,
    list_layers,
    mosaic,
    open,
    open_bytes,
    open_raster,
    open_vector,
    polygonize,
    rasterize,
    transform_points,
    translate,
    warp,
    write_raster,
    write_vector,
)
from terrio.errors import (
    CRSError,
    InvalidArgumentError,
    InvalidDataError,
    NotFoundError,
    TerrioError,
    TerrioIOError,
    UnsupportedError,
)

__all__ = [
    "CRSError",
    "InvalidArgumentError",
    "InvalidDataError",
    "Layer",
    "NotFoundError",
    "Raster",
    "TerrioError",
    "TerrioIOError",
    "UnsupportedError",
    "VectorDataset",
    "__version__",
    "convert",
    "drivers",
    "info",
    "list_layers",
    "mosaic",
    "open",
    "open_bytes",
    "open_raster",
    "open_vector",
    "polygonize",
    "rasterize",
    "transform_points",
    "translate",
    "warp",
    "write_raster",
    "write_vector",
]
