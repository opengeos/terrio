"""terrio: pure-Rust geospatial raster, vector and point cloud data I/O, for Python.

Read, write and convert rasters, vector datasets and point clouds (LAS, LAZ, COPC) in many
formats, local or remote, without GDAL::

    import terrio

    dem = terrio.open_raster("dem.tif")
    array = dem.read(1)  # numpy array, (rows, cols)

    countries = terrio.open_vector("countries.gpkg")
    layer = countries.layers[0]
    collection = layer.to_geojson()
    table = layer.to_arrow()  # Arrow PyCapsule interface: pyarrow.table(table)

    terrio.convert("countries.shp", "europe.gpkg", where="CONTINENT = 'Europe'")
    terrio.warp("dem.tif", "dem_4326.tif", t_srs="EPSG:4326", resampling="bilinear")

    points = terrio.read_points("lidar.copc.laz", bounds=(x0, y0, x1, y1), classes=[2])
    z = points["z"]  # numpy array
    dem = points.grid(1.0, stat="idw")  # a Raster
"""

from terrio._terrio import (
    ArrowTable,
    Layer,
    PointCloud,
    PointChunks,
    PointCloudFile,
    PointCloudWriter,
    Raster,
    VectorDataset,
    __version__,
    calc,
    convert,
    convert_points,
    grid_points,
    dem,
    drivers,
    from_arrow,
    info,
    list_layers,
    mosaic,
    open,
    open_bytes,
    open_points,
    open_raster,
    open_vector,
    polygonize,
    proximity,
    rasterize,
    read_points,
    read_points_bytes,
    set_grid_search_paths,
    set_network_enabled,
    sieve,
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
    "ArrowTable",
    "CRSError",
    "InvalidArgumentError",
    "InvalidDataError",
    "Layer",
    "NotFoundError",
    "PointCloud",
    "PointChunks",
    "PointCloudFile",
    "PointCloudWriter",
    "Raster",
    "TerrioError",
    "TerrioIOError",
    "UnsupportedError",
    "VectorDataset",
    "__version__",
    "calc",
    "convert",
    "convert_points",
    "grid_points",
    "dem",
    "drivers",
    "from_arrow",
    "info",
    "list_layers",
    "mosaic",
    "open",
    "open_bytes",
    "open_points",
    "open_raster",
    "open_vector",
    "polygonize",
    "proximity",
    "rasterize",
    "read_points",
    "read_points_bytes",
    "sieve",
    "transform_points",
    "set_grid_search_paths",
    "set_network_enabled",
    "translate",
    "warp",
    "write_raster",
    "write_vector",
]
