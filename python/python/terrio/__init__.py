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

import sys as _sys
from collections.abc import Iterator as _Iterator
from typing import Any as _Any

from terrio._terrio import (
    ArrowTable,
    FeatureStream,
    GeoTiffWriter,
    Layer,
    MultidimArray,
    MultidimDataset,
    PointChunks,
    PointCloud,
    PointCloudFile,
    PointCloudWriter,
    Raster,
    VectorDataset,
    __version__,
    append_points,
    as_features,
    calc,
    clean_collar,
    combine,
    concat,
    concave_hull,
    convert,
    convert_points,
    convex_hull,
    create_vector,
    dem,
    drivers,
    edit_vector,
    enhance,
    enhance_luts,
    export_schema,
    fill_nodata,
    from_arrow,
    grid_points,
    info,
    list_layers,
    make_point,
    merge_points,
    mosaic,
    nearblack,
    neighbors,
    nodata_to_alpha,
    open,
    open_bytes,
    open_catalog,
    open_multidim,
    open_points,
    open_raster,
    open_stream,
    open_vector,
    open_vrt,
    pansharpen,
    polygonize,
    proximity,
    rasterize,
    read_points,
    read_points_bytes,
    reclassify,
    rename_layer,
    segmentize,
    set_field_type,
    set_geom_type,
    set_grid_search_paths,
    set_network_enabled,
    sieve,
    simplify,
    sort,
    sql,
    swap_xy,
    transform_points,
    translate,
    unscale,
    update_vector,
    viewshed,
    warp,
    write_netcdf_groups,
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
    "FeatureStream",
    "GeoTiffWriter",
    "InvalidArgumentError",
    "InvalidDataError",
    "Layer",
    "MultidimArray",
    "MultidimDataset",
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
    "append_points",
    "as_features",
    "calc",
    "combine",
    "concave_hull",
    "convex_hull",
    "concat",
    "clean_collar",
    "convert",
    "convert_points",
    "create_vector",
    "rename_layer",
    "edit_vector",
    "update_vector",
    "grid_points",
    "dem",
    "enhance",
    "enhance_luts",
    "drivers",
    "export_schema",
    "from_arrow",
    "info",
    "iter_features",
    "list_layers",
    "make_point",
    "merge_points",
    "mosaic",
    "nodata_to_alpha",
    "nearblack",
    "neighbors",
    "open",
    "open_bytes",
    "open_catalog",
    "open_points",
    "open_raster",
    "open_multidim",
    "open_vrt",
    "open_stream",
    "open_vector",
    "pansharpen",
    "partition",
    "polygonize",
    "proximity",
    "rasterize",
    "read_points",
    "read_points_bytes",
    "reclassify",
    "segmentize",
    "set_geom_type",
    "set_field_type",
    "sieve",
    "simplify",
    "sort",
    "sql",
    "swap_xy",
    "fill_nodata",
    "transform_points",
    "set_grid_search_paths",
    "set_network_enabled",
    "translate",
    "unscale",
    "viewshed",
    "warp",
    "write_netcdf_groups",
    "write_raster",
    "write_vector",
]

if _sys.platform == "emscripten":
    # Native filesystem partitioning is absent from the Pyodide extension.
    __all__ = [name for name in __all__ if name != "partition"]
else:
    from terrio._terrio import partition as partition


def iter_features(
    path: str,
    layer: str | None = None,
    *,
    batch_size: int = 65536,
    bbox: tuple[float, float, float, float] | None = None,
) -> _Iterator[dict[str, _Any]]:
    """Iterates over the features of a vector layer, reading it a chunk at a time.

    The features come as GeoJSON-like dictionaries (the ``__geo_interface__`` of each
    feature), so that layers larger than memory can be processed feature by feature. See
    ``open_stream`` for the formats read as streams.

    Args:
        path: The dataset's path or URL.
        layer: The layer's name. By default, the first layer.
        batch_size: The most features read at a time.
        bbox: Only the features whose bounding box intersects this rectangle
            ``(min_x, min_y, max_x, max_y)``, in the layer's CRS (GeoPackage and GeoParquet).

    Yields:
        dict: A GeoJSON ``Feature``: ``id``, ``geometry`` and ``properties``.

    Raises:
        InvalidArgumentError: If the dataset has no such layer.
    """
    for chunk in open_stream(path, layer, batch_size=batch_size, bbox=bbox):
        yield from chunk.__geo_interface__["features"]
