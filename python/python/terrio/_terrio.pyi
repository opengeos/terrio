"""Type stubs for the `terrio._terrio` extension module."""

import os
from collections.abc import Iterator, Mapping, Sequence
from typing import Any, Literal, Protocol, TypeAlias, final, overload

import numpy as np
from numpy.typing import DTypeLike, NDArray

# PyO3 sets `__all__` to the names added to the module, in registration order.
__all__ = [
    "__version__",
    "Raster",
    "GeoTiffWriter",
    "Layer",
    "VectorDataset",
    "ArrowTable",
    "from_arrow",
    "open",
    "open_raster",
    "open_catalog",
    "open_vrt",
    "open_vector",
    "open_bytes",
    "list_layers",
    "info",
    "drivers",
    "write_raster",
    "write_netcdf_groups",
    "write_vector",
    "convert",
    "sql",
    "rasterize",
    "pansharpen",
    "polygonize",
    "transform_points",
    "set_grid_search_paths",
    "set_network_enabled",
    "translate",
    "warp",
    "mosaic",
    "MultidimDataset",
    "MultidimArray",
    "open_multidim",
    "dem",
    "calc",
    "proximity",
    "viewshed",
    "sieve",
    "fill_nodata",
    "compare",
    "pixel_info",
    "xyz",
    "neighbors",
    "nearblack",
    "clean_collar",
    "enhance",
    "enhance_luts",
    "unscale",
    "nodata_to_alpha",
    "reclassify",
    "as_features",
    "PointCloud",
    "PointCloudFile",
    "PointChunks",
    "PointCloudWriter",
    "grid_points",
    "open_points",
    "read_points",
    "read_points_bytes",
    "convert_points",
    "merge_points",
    "append_points",
    "tiff_tags",
    "crs_info",
    "suggested_grid",
    "gcp_transform",
    "RpcTransformer",
    "FeatureStream",
    "open_stream",
    "simplify",
    "segmentize",
    "convex_hull",
    "concave_hull",
    "swap_xy",
    "set_geom_type",
    "combine",
    "sort",
    "concat",
    "partition",
    "set_field_type",
    "make_point",
    "export_schema",
    "create_vector",
    "rename_layer",
    "edit_vector",
    "update_vector",
    "cli_main",
]

__version__: str

class _SupportsGeoInterface(Protocol):
    @property
    def __geo_interface__(self) -> Any: ...

# The Arrow PyCapsule interface: pyarrow tables, record batches and readers, Polars data frames,
# `GeoDataFrame.to_arrow()`, GDAL layers...
class _ArrowStreamExportable(Protocol):
    def __arrow_c_stream__(self, requested_schema: object | None = None) -> object: ...

class _ArrowArrayExportable(Protocol):
    def __arrow_c_array__(
        self, requested_schema: object | None = None
    ) -> tuple[object, object]: ...

_ArrowData: TypeAlias = _ArrowStreamExportable | _ArrowArrayExportable
_GeometryEncoding: TypeAlias = Literal["wkb", "geoarrow"]
_CoordinateLayout: TypeAlias = Literal["separated", "interleaved"]

_Nodata: TypeAlias = float | Sequence[float] | str
_Bounds: TypeAlias = tuple[float, float, float, float]
_Bounds3D: TypeAlias = tuple[float, float, float, float, float, float]
_Window: TypeAlias = tuple[int, int, int, int]
_XYZ: TypeAlias = tuple[float, float, float]

@final
class GeoTiffWriter:
    def __new__(
        cls,
        path: str,
        width: int,
        height: int,
        count: int,
        dtype: str,
        transform: Sequence[float],
        crs: str | None = None,
        nodata: float | None = None,
        metadata: Mapping[str, Any] | Sequence[tuple[Any, Any]] | None = None,
        *,
        compress: str | None = None,
        tile_size: int | None = None,
        bigtiff: bool = False,
        options: Mapping[str, str] | None = None,
        band_metadata: Sequence[Mapping[str, Any]] | None = None,
        colormap: Sequence[tuple[int, int, int, int]] | None = None,
        gcps: Sequence[tuple[float, float, float, float, float]] | None = None,
        rpc: Sequence[float] | None = None,
        mask: bool = False,
    ) -> GeoTiffWriter: ...
    @property
    def rows_written(self) -> int: ...
    def write(self, array: NDArray[Any], mask: NDArray[Any] | None = None) -> None: ...
    def close(self) -> None: ...

@final
class Raster:
    """A raster dataset: a grid of pixels in one or more bands, with georeferencing."""

    @property
    def width(self) -> int: ...
    @property
    def height(self) -> int: ...
    @property
    def count(self) -> int: ...
    @property
    def shape(self) -> tuple[int, int, int]: ...
    @property
    def dtype(self) -> str: ...
    @property
    def geolocation(self) -> list[tuple[str, str]] | None: ...
    @property
    def metadata(self) -> list[tuple[str, str]]: ...
    @property
    def nodata(self) -> float | None: ...
    @property
    def crs(self) -> str | None: ...
    @property
    def crs_wkt(self) -> str | None: ...
    @property
    def transform(self) -> tuple[float, float, float, float, float, float]: ...
    @property
    def res(self) -> tuple[float, float]: ...
    @property
    def bounds(self) -> _Bounds: ...
    @property
    def driver(self) -> str: ...
    @property
    def overviews(self) -> list[tuple[int, int]]: ...
    @property
    def is_lazy(self) -> bool: ...
    @property
    def has_mask(self) -> bool: ...
    @property
    def block_size(self) -> tuple[int, int] | None: ...
    def read(
        self,
        band: int | Sequence[int] | None = None,
        window: _Window | None = None,
        overview: int | None = None,
    ) -> NDArray[Any]: ...
    def read_mask(
        self, window: _Window | None = None, overview: int | None = None
    ) -> NDArray[Any] | None: ...
    @staticmethod
    def from_array(
        array: NDArray[Any],
        transform: Sequence[float],
        crs: str | None = None,
        nodata: float | None = None,
        metadata: Mapping[str, Any] | Sequence[tuple[Any, Any]] | None = None,
    ) -> Raster: ...
    def write(
        self,
        path: str,
        driver: str | None = None,
        compress: str | None = None,
        tile_size: int | None = None,
        bigtiff: bool = False,
        options: Mapping[str, str] | None = None,
        *,
        band_metadata: Sequence[Mapping[str, Any]] | None = None,
        colormap: Sequence[tuple[int, int, int, int]] | None = None,
        overviews: Sequence[int] | None = None,
        overview_resampling: str | None = None,
        gcps: Sequence[tuple[float, float, float, float, float]] | None = None,
        mask: NDArray[Any] | None = None,
        rpc: Sequence[float] | None = None,
    ) -> None: ...
    def to_bytes(
        self,
        filename: str = "raster.tif",
        driver: str | None = None,
        compress: str | None = None,
        tile_size: int | None = None,
    ) -> bytes: ...
    def __array__(
        self, dtype: DTypeLike | None = None, copy: bool | None = None
    ) -> NDArray[Any]: ...

@final
class Layer:
    """A vector layer: features with geometries and attributes, in memory."""

    @property
    def name(self) -> str: ...
    @name.setter
    def name(self, name: str) -> None: ...
    @property
    def crs(self) -> str | None: ...
    @property
    def crs_wkt(self) -> str | None: ...
    @property
    def geometry_type(self) -> str: ...
    @property
    def fields(self) -> dict[str, str]: ...
    @property
    def bounds(self) -> _Bounds | None: ...
    @property
    def __geo_interface__(self) -> dict[str, Any]: ...
    def __len__(self) -> int: ...
    def to_geojson(self) -> dict[str, Any]: ...
    def to_wkb(self) -> list[bytes | None]: ...
    def columns(self) -> dict[str, list[Any]]: ...
    def to_geopandas(self) -> Any: ...
    def to_crs(self, crs: str) -> Layer: ...
    def to_arrow(
        self,
        geometry_encoding: _GeometryEncoding = "wkb",
        coordinates: _CoordinateLayout = "separated",
        geometry_column: str = "geometry",
        fid_column: str | None = None,
    ) -> ArrowTable: ...
    def __arrow_c_stream__(self, requested_schema: object | None = None) -> object: ...
    @staticmethod
    def from_arrow(
        data: _ArrowData | _SupportsGeoInterface, name: str | None = None, crs: str | None = None
    ) -> Layer: ...
    @staticmethod
    def from_geojson(
        data: str | _GeoJSON,
        name: str | None = None,
        crs: str | None = None,
    ) -> Layer: ...
    def write(self, path: str, driver: str | None = None) -> None: ...
    def to_bytes(self, filename: str = "layer.geojson", driver: str | None = None) -> bytes: ...

@final
class VectorDataset:
    """A vector dataset: one or more layers."""

    @property
    def layers(self) -> list[Layer]: ...
    @property
    def layer_names(self) -> list[str]: ...
    @property
    def driver(self) -> str: ...
    def __len__(self) -> int: ...
    def __getitem__(self, key: int | str, /) -> Layer: ...
    def __iter__(self) -> Iterator[Layer]: ...
    def write(self, path: str, driver: str | None = None) -> None: ...

@final
class ArrowTable:
    """A layer as Arrow data, exported through the Arrow PyCapsule interface."""

    @property
    def num_rows(self) -> int: ...
    @property
    def num_columns(self) -> int: ...
    @property
    def column_names(self) -> list[str]: ...
    @property
    def num_batches(self) -> int: ...
    def __len__(self) -> int: ...
    def __arrow_c_stream__(self, requested_schema: object | None = None) -> object: ...
    def __arrow_c_schema__(self) -> object: ...
    def to_pyarrow(self) -> Any: ...

def from_arrow(
    data: _ArrowData | _SupportsGeoInterface, name: str | None = None, crs: str | None = None
) -> Layer: ...
@final
class PointCloud:
    """A point cloud in memory: columns of values, one per dimension."""

    @staticmethod
    def from_dict(arrays: dict[str, Any], crs: str | None = None) -> PointCloud: ...
    def __len__(self) -> int: ...
    def __contains__(self, name: str, /) -> bool: ...
    def __getitem__(self, name: str, /) -> NDArray[Any]: ...
    @property
    def dimensions(self) -> list[str]: ...
    @property
    def crs(self) -> str | None: ...
    @property
    def bounds(self) -> _Bounds3D | None: ...
    @property
    def header(self) -> dict[str, Any]: ...
    def to_dict(self) -> dict[str, NDArray[Any]]: ...
    def to_numpy(self) -> NDArray[np.void]: ...
    def filter(
        self,
        *,
        bounds: _Bounds | None = None,
        z_range: tuple[float, float] | None = None,
        classes: Sequence[int] | None = None,
        returns: Sequence[str] | None = None,
    ) -> PointCloud: ...
    def reproject(self, crs: str) -> PointCloud: ...
    def classification_counts(self) -> dict[int, int]: ...
    def write(
        self,
        path: str,
        *,
        format: str | None = None,
        point_format: int | None = None,
        minor_version: int | None = None,
        scale: _XYZ | None = None,
        offset: _XYZ | None = None,
        ply_encoding: str | None = None,
        ply_precision: int | None = None,
        dimensions: Sequence[str] | None = None,
        ept_data_type: str | None = None,
        ept_bounds: tuple[float, float, float, float, float, float] | None = None,
    ) -> None: ...
    def to_bytes(self, format: str = "LAZ", point_format: int | None = None) -> bytes: ...
    def range(self, limits: str) -> PointCloud: ...
    def sample(
        self,
        radius: float | None = None,
        *,
        cell: float | None = None,
        origin: _XYZ | None = None,
    ) -> PointCloud: ...
    def voxel_center_nearest_neighbor(self, cell: float = 1.0) -> PointCloud: ...
    def voxel_downsize(self, cell: float = 0.001, mode: str = "center") -> PointCloud: ...
    def outlier(
        self,
        method: str = "statistical",
        *,
        min_k: int = 2,
        radius: float = 1.0,
        mean_k: int = 8,
        multiplier: float = 2.0,
        classification: int = 7,
    ) -> PointCloud: ...
    def smrf(
        self,
        *,
        cell: float = 1.0,
        slope: float = 0.15,
        window: float | None = None,
        scalar: float = 1.25,
        threshold: float = 0.5,
        cut: float = 0.0,
        ignore: str | None = None,
        returns: Sequence[str] | None = None,
        ground_class: int = 2,
        other_class: int = 1,
        only_ground: bool = False,
    ) -> PointCloud: ...
    def pmf(
        self,
        *,
        cell_size: float = 1.0,
        exponential: bool = True,
        initial_distance: float = 0.15,
        max_distance: float = 2.5,
        max_window_size: float = 33.0,
        slope: float = 1.0,
        ignore: str | None = None,
        returns: Sequence[str] | None = None,
        ground_class: int = 2,
        other_class: int = 1,
        only_ground: bool = False,
    ) -> PointCloud: ...
    def csf(
        self,
        *,
        resolution: float = 1.0,
        rigidness: int = 3,
        step: float = 0.65,
        threshold: float = 0.5,
        hdiff: float = 0.3,
        iterations: int = 500,
        smooth: bool = True,
        ignore: str | None = None,
        returns: Sequence[str] | None = None,
        ground_class: int = 2,
        other_class: int = 1,
        only_ground: bool = False,
    ) -> PointCloud: ...
    def elm(
        self, *, cell: float = 10.0, threshold: float = 1.0, classification: int = 7
    ) -> PointCloud: ...
    def hag_nn(
        self,
        *,
        count: int = 1,
        max_distance: float = 0.0,
        allow_extrapolation: bool = True,
        ground_class: int = 2,
    ) -> PointCloud: ...
    def hag_delaunay(
        self, *, count: int = 10, allow_extrapolation: bool = True, ground_class: int = 2
    ) -> PointCloud: ...
    def crop(
        self,
        *,
        bounds: Sequence[str] | None = None,
        polygons: Sequence[str] | None = None,
        points: Sequence[Sequence[float]] | None = None,
        distance: float = 0.0,
        outside: bool = False,
        a_srs: str | None = None,
    ) -> PointCloud: ...
    def decimation(
        self, step: float = 1.0, *, offset: int = 0, limit: int | None = None
    ) -> PointCloud: ...
    def expression(self, expression: str) -> PointCloud: ...
    def assign(
        self,
        *,
        assignment: str | Sequence[str] | None = None,
        condition: str | None = None,
        value: str | Sequence[str] | None = None,
    ) -> PointCloud: ...
    def splitter(
        self,
        length: float = 1000.0,
        *,
        origin: tuple[float, float] | None = None,
        buffer: float = 0.0,
    ) -> list[PointCloud]: ...
    def chipper(self, capacity: int = 5000) -> list[PointCloud]: ...
    def normal(
        self, *, knn: int = 8, viewpoint: _XYZ | None = None, always_up: bool = True
    ) -> PointCloud: ...
    def cluster(
        self,
        tolerance: float = 1.0,
        *,
        min_points: int = 1,
        max_points: int | None = None,
        is3d: bool = True,
    ) -> PointCloud: ...
    def dbscan(
        self,
        eps: float = 1.0,
        *,
        min_points: int = 6,
        dimensions: Sequence[str] | None = None,
    ) -> PointCloud: ...
    def covariance_features(
        self,
        *,
        knn: int = 10,
        feature_set: str | Sequence[str] | None = None,
        mode: str = "sqrt",
        stride: int = 1,
        radius: float | None = None,
        min_k: int = 3,
        optimized: bool = False,
    ) -> PointCloud: ...
    def eigenvalues(
        self,
        *,
        knn: int = 8,
        normalize: bool = False,
        stride: int = 1,
        radius: float | None = None,
        min_k: int = 3,
    ) -> PointCloud: ...
    def approximate_coplanar(
        self, *, knn: int = 8, thresh1: float = 25.0, thresh2: float = 6.0
    ) -> PointCloud: ...
    def estimate_rank(self, *, knn: int = 8, thresh: float = 0.01) -> PointCloud: ...
    def lof(self, minpts: int = 10) -> PointCloud: ...
    def nndistance(self, k: int = 10, *, mode: str = "kth") -> PointCloud: ...
    def radial_density(self, radius: float = 1.0) -> PointCloud: ...
    def reciprocity(self, knn: int = 8) -> PointCloud: ...
    def miniball(self, knn: int = 8) -> PointCloud: ...
    def zsmooth(
        self, dim: str, *, radius: float = 1.0, medianpercent: float = 50.0
    ) -> PointCloud: ...
    def skewness_balancing(
        self, *, ground_class: int = 2, other_class: int = 1, only_ground: bool = False
    ) -> PointCloud: ...
    def head(self, count: int = 10, *, invert: bool = False) -> PointCloud: ...
    def tail(self, count: int = 10, *, invert: bool = False) -> PointCloud: ...
    def sort(self, dimensions: str | Sequence[str], *, order: str = "ASC") -> PointCloud: ...
    def locate(self, dimension: str, *, minmax: str = "max") -> PointCloud: ...
    def iqr(self, dimension: str, *, k: float = 1.5) -> PointCloud: ...
    def mad(
        self, dimension: str, *, k: float = 2.0, mad_multiplier: float = 1.4862
    ) -> PointCloud: ...
    def dem(self, raster: str | Raster, limits: str, *, band: int = 1) -> PointCloud: ...
    def colorization(
        self, raster: str | Raster, *, dimensions: str | Sequence[str] | None = None
    ) -> PointCloud: ...
    def overlay(
        self,
        datasource: str | Layer | VectorDataset,
        dimension: str,
        *,
        column: str | None = None,
        layer: str | None = None,
    ) -> PointCloud: ...
    def grid(
        self,
        resolution: float,
        *,
        method: str = "statistics",
        stat: str | Sequence[str] | None = None,
        dimension: str = "z",
        radius: float | None = None,
        power: float = 1.0,
        window_size: int = 0,
        bin_mode: bool = False,
        bounds: _Bounds | None = None,
        nodata: float = ...,  # -9999.0
        ot: str | None = None,
    ) -> Raster: ...
    def to_layer(
        self, dimensions: Sequence[str] | None = None, name: str | None = None
    ) -> Layer: ...

@final
class PointChunks:
    """The chunks of points of a `PointCloudFile`: an iterator of `PointCloud`."""

    def __iter__(self) -> PointChunks: ...
    def __next__(self) -> PointCloud: ...

@final
class PointCloudWriter:
    """Writes points as they come, in chunks, to a LAS, LAZ, COPC or PLY file."""

    def __new__(
        cls,
        path: str,
        template: PointCloud,
        *,
        format: str | None = None,
        point_format: int | None = None,
        minor_version: int | None = None,
        scale: _XYZ | None = None,
        offset: _XYZ | None = None,
    ) -> PointCloudWriter: ...
    def write(self, points: PointCloud) -> None: ...
    def close(self) -> None: ...
    def __enter__(self) -> PointCloudWriter: ...
    def __exit__(self, *args: object) -> bool: ...

@final
class PointCloudFile:
    """An open point cloud (LAS, LAZ, COPC, EPT, PLY or E57), its points read on demand."""

    @property
    def format(self) -> str: ...
    @property
    def ept_info(self) -> dict[str, Any] | None: ...
    @property
    def wave_packet_descriptors(self) -> list[dict[str, Any]]: ...
    def waveform(self, index: int, offset: int, size: int) -> NDArray[np.uint32]: ...
    def chunks(
        self,
        chunk_points: int = 0,
        *,
        bounds: _Bounds | None = None,
        z_range: tuple[float, float] | None = None,
        classes: Sequence[int] | None = None,
        returns: Sequence[str] | None = None,
        max_level: int | None = None,
        resolution: float | None = None,
        max_points: int | None = None,
    ) -> PointChunks: ...
    def template(self) -> PointCloud: ...
    @property
    def point_count(self) -> int: ...
    @property
    def crs(self) -> str | None: ...
    @property
    def bounds(self) -> _Bounds3D: ...
    @property
    def header(self) -> dict[str, Any]: ...
    @property
    def dimensions(self) -> dict[str, str]: ...
    def info(self, stats: bool = False) -> dict[str, Any]: ...
    def copc_nodes(self) -> list[dict[str, Any]]: ...
    def read(
        self,
        *,
        bounds: _Bounds | None = None,
        z_range: tuple[float, float] | None = None,
        classes: Sequence[int] | None = None,
        returns: Sequence[str] | None = None,
        max_level: int | None = None,
        resolution: float | None = None,
        max_points: int | None = None,
    ) -> PointCloud: ...

# Inputs taken by the operations. Paths are `str` only: `os.PathLike` is not accepted.
_RasterSource: TypeAlias = str | Raster
_Layers: TypeAlias = Layer | VectorDataset | list[Layer] | tuple[Layer, ...]
_GeoJSON: TypeAlias = dict[str, Any] | _SupportsGeoInterface
# A path to a vector dataset, or vector data in memory.
_VectorSource: TypeAlias = str | _Layers | _GeoJSON
# Vector data in memory; a `str` is GeoJSON text, not a path.
_VectorData: TypeAlias = _Layers | str | _GeoJSON | _ArrowData

# Opening and writing.

def open(path: str) -> Raster | VectorDataset: ...
def open_raster(path: str, lazy: bool = False) -> Raster: ...
def open_vrt(xml: str, sources: Mapping[str, bytes | str | os.PathLike[str]]) -> Raster: ...
def open_vector(
    path: str, layers: Sequence[str] | None = None, bbox: _Bounds | None = None
) -> VectorDataset: ...
def open_bytes(
    data: bytes | bytearray, filename: str, lazy: bool = False
) -> Raster | VectorDataset: ...
def list_layers(path: str) -> list[str]: ...
def info(
    path: str, stats: bool = False, layers: Sequence[str] | None = None, hist: bool = False
) -> dict[str, Any]: ...
def drivers() -> list[dict[str, Any]]: ...
def write_raster(
    raster: Raster,
    path: str,
    driver: str | None = None,
    compress: str | None = None,
    tile_size: int | None = None,
    bigtiff: bool = False,
) -> None: ...
def write_netcdf_groups(
    rasters: Sequence[tuple[str, Raster]] | Mapping[str, Raster],
    path: str,
    options: Mapping[str, str] | None = None,
) -> None: ...
def write_vector(
    data: _VectorData,
    path: str,
    driver: str | None = None,
    layer_name: str | None = None,
    crs: str | None = None,
    append: bool = False,
    overwrite: bool = False,
) -> None: ...
@final
class FeatureStream:
    """A vector layer read as a stream of `Layer` chunks."""

    @property
    def name(self) -> str: ...
    @property
    def crs(self) -> str | None: ...
    @property
    def geometry_type(self) -> str: ...
    @property
    def fields(self) -> dict[str, str]: ...
    @property
    def batch_size(self) -> int: ...
    def __iter__(self) -> FeatureStream: ...
    def __next__(self) -> Layer: ...
    def read_all(self) -> Layer: ...
    def __arrow_c_stream__(self, requested_schema: object | None = None) -> object: ...

def open_stream(
    path: str,
    layer: str | None = None,
    *,
    batch_size: int = 65536,
    bbox: _Bounds | None = None,
) -> FeatureStream: ...
def convert(
    src: str,
    dst: str,
    *,
    format: str | None = None,
    layers: Sequence[str] | None = None,
    t_srs: str | None = None,
    compress: str | None = None,
    tile_size: int | None = None,
    bigtiff: bool = False,
    srcwin: _Window | None = None,
    overview: int | None = None,
    where: str | None = None,
    spat: _Bounds | None = None,
    spat_srs: str | None = None,
    select: Sequence[str] | None = None,
    limit: int | None = None,
    sql: str | None = None,
    nln: str | None = None,
    nlt: str | None = None,
    explodecollections: bool = False,
    simplify: float | None = None,
    segmentize: float | None = None,
    s_srs: str | None = None,
    a_srs: str | None = None,
    append: bool = False,
    overwrite: bool = False,
    ct_opt: Mapping[str, str] | None = None,
) -> None: ...

# Operations.

def rasterize(
    src: _VectorSource,
    dst: str | None = None,
    *,
    layers: Sequence[str] | None = None,
    sql: str | None = None,
    burn: float | Sequence[float] | None = None,
    attribute: str | None = None,
    where: str | None = None,
    tr: tuple[float, float] | None = None,
    ts: tuple[int, int] | None = None,
    te: _Bounds | None = None,
    tap: bool = False,
    like: _RasterSource | None = None,
    ot: str | None = None,
    init: float | Sequence[float] | None = None,
    nodata: float | None = None,
    all_touched: bool = False,
    add: bool = False,
    a_srs: str | None = None,
    format: str | None = None,
    compress: str | None = None,
) -> Raster: ...
def polygonize(
    src: _RasterSource,
    dst: str | None = None,
    *,
    band: int = 1,
    field: str = "DN",
    layer_name: str | None = None,
    connectedness: int = 4,
    nomask: bool = False,
    mask: _RasterSource | None = None,
    format: str | None = None,
    append: bool = False,
    overwrite: bool = False,
) -> Layer: ...
@overload
def transform_points(
    xs: Any,
    ys: Any,
    src_crs: str,
    dst_crs: str,
    zs: None = None,
    *,
    s_coord_epoch: float | None = None,
    t_coord_epoch: float | None = None,
    allow_ballpark: bool | None = None,
    only_best: bool = False,
    promote_to_3d: bool = False,
) -> tuple[NDArray[np.float64], NDArray[np.float64]]: ...
@overload
def transform_points(
    xs: Any,
    ys: Any,
    src_crs: str,
    dst_crs: str,
    zs: Any,
    *,
    s_coord_epoch: float | None = None,
    t_coord_epoch: float | None = None,
    allow_ballpark: bool | None = None,
    only_best: bool = False,
    promote_to_3d: bool = False,
) -> tuple[NDArray[np.float64], NDArray[np.float64], NDArray[np.float64]]: ...
def set_grid_search_paths(paths: Sequence[str | os.PathLike[str]] | None) -> None: ...
def set_network_enabled(enabled: bool | None) -> None: ...
def translate(
    src: _RasterSource,
    dst: str | None = None,
    *,
    bands: Sequence[int] | None = None,
    ot: str | None = None,
    srcwin: _Bounds | None = None,
    projwin: _Bounds | None = None,
    projwin_srs: str | None = None,
    outsize: tuple[int | str, int | str] | None = None,
    tr: tuple[float, float] | None = None,
    resampling: str | None = None,
    scale: bool | Sequence[float] | None = None,
    exponent: float | None = None,
    a_nodata: float | str | None = None,
    a_srs: str | None = None,
    a_ullr: _Bounds | None = None,
    format: str | None = None,
    compress: str | None = None,
    tile_size: int | None = None,
    bigtiff: bool = False,
) -> Raster: ...
def warp(
    src: _RasterSource | Sequence[_RasterSource],
    dst: str | None = None,
    *,
    s_srs: str | None = None,
    t_srs: str | None = None,
    tr: tuple[float, float] | None = None,
    te: _Bounds | None = None,
    te_srs: str | None = None,
    ts: tuple[int, int] | None = None,
    dst_transform: tuple[float, float, float, float, float, float] | None = None,
    tap: bool = False,
    resampling: str = "near",
    overview: int | str | None = None,
    srcnodata: _Nodata | None = None,
    dstnodata: _Nodata | None = None,
    ot: str | None = None,
    cutline: _VectorSource | None = None,
    crop_to_cutline: bool = False,
    et: float = 0.125,
    format: str | None = None,
    compress: str | None = None,
    tile_size: int | None = None,
    bigtiff: bool = False,
    gcps: Sequence[tuple[float, float, float, float, float]] | None = None,
    order: int = 0,
    gcp_method: str = "auto",
    rpc: Mapping[str, str] | None = None,
    to: Mapping[str, str] | None = None,
    wo: Mapping[str, str] | None = None,
    geoloc: bool = False,
    geolocation: Mapping[str, Any] | None = None,
    kernel_version: str = "3.12",
) -> Raster: ...
def mosaic(
    srcs: _RasterSource | Sequence[_RasterSource],
    dst: str | None = None,
    *,
    resolution: str = "average",
    tr: tuple[float, float] | None = None,
    te: _Bounds | None = None,
    tap: bool = False,
    separate: bool = False,
    bands: Sequence[int] | None = None,
    nodata: _Nodata | None = None,
    srcnodata: _Nodata | None = None,
    dstnodata: _Nodata | None = None,
    resampling: str = "near",
    t_srs: str | None = None,
    format: str | None = None,
    compress: str | None = None,
    tile_size: int | None = None,
    bigtiff: bool = False,
) -> Raster | None: ...

# Terrain analysis and raster algebra.

def dem(
    src: _RasterSource,
    processing: str,
    dst: str | None = None,
    *,
    color_file: str | None = None,
    band: int = 1,
    z: float = 1.0,
    scale: float | None = None,
    xscale: float | None = None,
    yscale: float | None = None,
    azimuth: float | None = None,
    altitude: float | None = None,
    combined: bool = False,
    multidirectional: bool = False,
    igor: bool = False,
    alg: str | None = None,
    percent: bool = False,
    trigonometric: bool = False,
    zero_for_flat: bool = False,
    compute_edges: bool = False,
    alpha: bool = False,
    exact_color_entry: bool = False,
    nearest_color_entry: bool = False,
    format: str | None = None,
    compress: str | None = None,
    tile_size: int | None = None,
    bigtiff: bool = False,
    kernel_version: str = "3.12",
) -> Raster:
    """Compute terrain; GDAL 3.13 hillshade rounding varies across CPUs/architectures."""
    ...

def calc(
    calc: str | Sequence[str],
    dst: str | None = None,
    *,
    type: str | None = None,
    nodata: float | str | None = None,
    hide_nodata: bool = False,
    extent: str | None = None,
    projwin: _Bounds | None = None,
    all_bands: str | None = None,
    projection_check: bool = False,
    format: str | None = None,
    compress: str | None = None,
    tile_size: int | None = None,
    bigtiff: bool = False,
    **inputs: _RasterSource | int,
) -> Raster: ...
def viewshed(
    src: str | Raster,
    ox: float,
    oy: float,
    dst: str | None = None,
    *,
    oz: float = 2.0,
    tz: float = 0.0,
    md: float = 0.0,
    cc: float | None = None,
    om: str = "NORMAL",
    ov: int = 0,
    band: int = 1,
    format: str | None = None,
    compress: str | None = None,
    tile_size: int | None = None,
    bigtiff: bool = False,
) -> Raster:
    """Single-observer visibility or minimum visible height on a projected DEM."""
    ...

def compare(
    reference: _RasterSource,
    input: _RasterSource,
    *,
    skip_all_optional: bool = False,
    skip_binary: bool = False,
    skip_crs: bool = False,
    skip_geotransform: bool = False,
    skip_overview: bool = False,
    skip_metadata: bool = False,
    skip_rpc: bool = False,
    skip_geolocation: bool = False,
    skip_subdataset: bool = False,
) -> dict[str, Any]:
    """Compare two rasters as ``gdal raster compare``: report, count and pixel statistics."""
    ...

def pixel_info(
    src: _RasterSource,
    positions: Sequence[tuple[float, float] | tuple[float, float, str]] | None = None,
    *,
    position_crs: str | None = None,
    position_dataset: str | Layer | VectorDataset | _GeoJSON | None = None,
    layer: str | None = None,
    include_fields: Sequence[str] | None = None,
    bands: Sequence[int] | None = None,
    overview: int | None = None,
    resampling: Literal["nearest", "bilinear", "cubic", "cubicspline"] = "nearest",
    promote_pixel_value_to_z: bool = False,
    format: Literal["geojson", "json", "csv", "text", "value"] = "geojson",
    field_sep: str = "\n",
    dst: str | None = None,
    output_format: str | None = None,
) -> Any:
    """Pixel values at positions, as ``gdal raster pixel-info`` and ``gdallocationinfo``."""
    ...

def xyz(
    src: _RasterSource,
    dst: str | os.PathLike[str] | None = None,
    *,
    bands: Sequence[int] | None = None,
    all_bands: bool = False,
    skip: int | tuple[int, int] | None = None,
    srcwin: _Window | None = None,
    skip_nodata: bool = False,
    src_nodata: float | Sequence[float] | None = None,
    dst_nodata: float | Sequence[float] | None = None,
    delimiter: str = " ",
) -> int | str:
    """Raster pixels as XYZ text, as ``gdal2xyz.py``: lines written to ``dst``, or the text."""
    ...

def proximity(
    src: _RasterSource,
    dst: str | None = None,
    *,
    band: int = 1,
    values: Sequence[int] | str | None = None,
    distunits: str = "PIXEL",
    maxdist: float | None = None,
    nodata: float | None = None,
    use_input_nodata: bool = False,
    fixed_buf_val: float | None = None,
    ot: str = "Float32",
    format: str | None = None,
    compress: str | None = None,
    tile_size: int | None = None,
    bigtiff: bool = False,
) -> Raster: ...
def fill_nodata(
    src: _RasterSource | os.PathLike[str],
    dst: str | None = None,
    *,
    band: int = 1,
    mask: _RasterSource | os.PathLike[str] | None = None,
    max_distance: float = 100.0,
    interpolation: str = "inv_dist",
    smoothing_iterations: int = 0,
    format: str | None = None,
    compress: str | None = None,
    tile_size: int | None = None,
    bigtiff: bool = False,
) -> Raster: ...
def sieve(
    src: _RasterSource,
    dst: str | None = None,
    *,
    band: int = 1,
    threshold: int = 2,
    connectedness: int = 4,
    mask: _RasterSource | None = None,
    format: str | None = None,
    compress: str | None = None,
    tile_size: int | None = None,
    bigtiff: bool = False,
) -> Raster: ...
def unscale(
    src: _RasterSource,
    dst: str | None = None,
    *,
    ot: str | None = None,
    format: str | None = None,
    compress: str | None = None,
    tile_size: int | None = None,
    bigtiff: bool = False,
) -> Raster:
    """Applies band scales and offsets (GDAL's ``gdal raster unscale``)."""
    ...

def nodata_to_alpha(
    src: _RasterSource,
    dst: str | None = None,
    *,
    nodata: float | Sequence[float] | None = None,
    format: str | None = None,
    compress: str | None = None,
    tile_size: int | None = None,
    bigtiff: bool = False,
) -> Raster:
    """Replaces nodata values with an alpha band (GDAL's ``gdal raster nodata-to-alpha``)."""
    ...

def reclassify(
    src: _RasterSource,
    mapping: str,
    dst: str | None = None,
    *,
    ot: str | None = None,
    format: str | None = None,
    compress: str | None = None,
    tile_size: int | None = None,
    bigtiff: bool = False,
) -> Raster:
    """Maps values or ranges to new values (GDAL's ``gdal raster reclassify``)."""
    ...

def as_features(
    src: _RasterSource,
    dst: str | None = None,
    *,
    bands: Sequence[int] | None = None,
    geometry_type: Literal["none", "point", "polygon"] = "none",
    skip_nodata: bool = False,
    include_xy: bool = False,
    include_row_col: bool = False,
    layer_name: str = "pixels",
    format: str | None = None,
    append: bool = False,
    overwrite: bool = False,
) -> Layer:
    """One vector feature per pixel (GDAL's ``gdal raster as-features``)."""
    ...

def neighbors(
    src: _RasterSource,
    dst: str | None = None,
    *,
    kernel: str | Sequence[str],
    method: str | Sequence[str] | None = None,
    size: int | None = None,
    band: int | None = None,
    ot: str = "Float64",
    nodata: float | str | None = None,
    format: str | None = None,
    compress: str | None = None,
    tile_size: int | None = None,
    bigtiff: bool = False,
) -> Raster: ...
def nearblack(
    src: _RasterSource,
    dst: str | None = None,
    *,
    colors: str | Sequence[int] | Sequence[str | Sequence[int]] | None = None,
    white: bool = False,
    color_threshold: int = 15,
    pixel_distance: int = 2,
    add_alpha: bool = False,
    add_mask: bool = False,
    algorithm: str = "twopasses",
    format: str | None = None,
    compress: str | None = None,
    tile_size: int | None = None,
    bigtiff: bool = False,
) -> Raster: ...
def clean_collar(
    src: _RasterSource,
    dst: str | None = None,
    *,
    colors: str | Sequence[int] | Sequence[str | Sequence[int]] | None = None,
    white: bool = False,
    color_threshold: int = 15,
    pixel_distance: int = 2,
    add_alpha: bool = False,
    add_mask: bool = False,
    algorithm: str = "floodfill",
    format: str | None = None,
    compress: str | None = None,
    tile_size: int | None = None,
    bigtiff: bool = False,
) -> Raster: ...
def enhance(
    src: _RasterSource,
    dst: str | None = None,
    *,
    equalize: bool = False,
    config: str | None = None,
    src_scale: tuple[float, float] | None = None,
    lut_bins: int = 256,
    ot: str = "Byte",
    format: str | None = None,
    compress: str | None = None,
    tile_size: int | None = None,
    bigtiff: bool = False,
) -> Raster: ...
def enhance_luts(src: _RasterSource, *, config: str | None = None, lut_bins: int = 256) -> str: ...

# Point clouds.

def open_points(path: str) -> PointCloudFile: ...
def read_points(
    path: str,
    *,
    bounds: _Bounds | None = None,
    z_range: tuple[float, float] | None = None,
    classes: Sequence[int] | None = None,
    returns: Sequence[str] | None = None,
    max_level: int | None = None,
    resolution: float | None = None,
    max_points: int | None = None,
) -> PointCloud: ...
def read_points_bytes(data: bytes | bytearray) -> PointCloud: ...
def convert_points(
    src: str,
    dst: str,
    *,
    bounds: _Bounds | None = None,
    z_range: tuple[float, float] | None = None,
    classes: Sequence[int] | None = None,
    returns: Sequence[str] | None = None,
    max_level: int | None = None,
    resolution: float | None = None,
    max_points: int | None = None,
    a_srs: str | None = None,
    t_srs: str | None = None,
    point_format: int | None = None,
    minor_version: int | None = None,
    scale: _XYZ | None = None,
    offset: _XYZ | None = None,
    dimensions: Sequence[str] | None = None,
    chunk_points: int = 0,
    ply_encoding: str | None = None,
    ply_precision: int | None = None,
    ept_data_type: str | None = None,
    ept_bounds: tuple[float, float, float, float, float, float] | None = None,
    append: bool = False,
) -> None: ...
def merge_points(clouds: Sequence[PointCloud]) -> PointCloud: ...
def append_points(
    dataset: str,
    inputs: Sequence[str],
    *,
    bounds: _Bounds | None = None,
    z_range: tuple[float, float] | None = None,
    classes: Sequence[int] | None = None,
    returns: Sequence[str] | None = None,
    a_srs: str | None = None,
    t_srs: str | None = None,
    chunk_points: int = 0,
    leaf_points: int | None = None,
    memory_points: int | None = None,
) -> None: ...
def grid_points(
    path: str,
    resolution: float,
    *,
    method: str = "statistics",
    stat: str | Sequence[str] | None = None,
    dimension: str = "z",
    radius: float | None = None,
    power: float = 1.0,
    window_size: int = 0,
    bin_mode: bool = False,
    bounds: _Bounds | None = None,
    nodata: float = ...,  # -9999.0
    ot: str | None = None,
    classes: Sequence[int] | None = None,
    returns: Sequence[str] | None = None,
    a_srs: str | None = None,
) -> Raster: ...

# Helpers of terrio.rio.

def tiff_tags(path: str) -> list[dict[int, str | bytes | list[int] | list[float]]] | None: ...
def crs_info(crs: str) -> dict[str, Any]: ...
def gcp_transform(
    gcps: Sequence[tuple[float, float, float, float, float]],
    xs: Sequence[float],
    ys: Sequence[float],
    inverse: bool = False,
    method: str = "polynomial",
    order: int = 0,
) -> tuple[list[float], list[float]]: ...
def suggested_grid(
    width: int,
    height: int,
    transform: Sequence[float],
    src_crs: str,
    dst_crs: str,
    gcps: Sequence[tuple[float, float, float, float, float]] | None = None,
    order: int = 0,
    gcp_method: str = "auto",
    rpc: Mapping[str, str] | None = None,
    rpc_options: Mapping[str, str] | None = None,
) -> tuple[tuple[float, float, float, float, float, float], int, int]: ...
@final
class RpcTransformer:
    def __new__(
        cls, rpc: Mapping[str, str], options: Mapping[str, str] | None = None
    ) -> RpcTransformer: ...
    def transform(
        self,
        xs: Sequence[float],
        ys: Sequence[float],
        zs: Sequence[float],
        to_pixel: bool = False,
    ) -> tuple[list[float], list[float], list[bool]]: ...

def cli_main(args: Sequence[str], terrio: Sequence[str] | None = None) -> int: ...
@final
class MultidimDataset:
    @property
    def array_paths(self) -> list[str]: ...
    @property
    def group_paths(self) -> list[str]: ...
    def info(self) -> dict[str, Any]: ...
    def array(self, name: str) -> MultidimArray: ...
    def __getitem__(self, key: str, /) -> MultidimArray: ...
    def write(self, destination: str | os.PathLike[str]) -> None: ...
    def to_bytes(self) -> bytes: ...
    def to_zarr_store(self) -> dict[str, bytes]: ...

@final
class MultidimArray:
    @property
    def path(self) -> str: ...
    @property
    def shape(self) -> tuple[int, ...]: ...
    @property
    def dimension_names(self) -> list[str]: ...
    @property
    def dtype(self) -> str: ...
    @property
    def attributes(self) -> dict[str, Any]: ...
    @property
    def coordinates(self) -> list[str]: ...
    def info(self) -> dict[str, Any]: ...
    def isel(self, dimension: str, index: int) -> MultidimArray: ...
    def isel_axis(self, axis: int, index: int) -> MultidimArray: ...
    def slice_axis(self, axis: int, start: int, count: int, step: int = 1) -> MultidimArray: ...
    def slice(self, dimension: str, start: int, count: int, step: int = 1) -> MultidimArray: ...
    def transpose(self, dimensions: list[str] | None = None) -> MultidimArray: ...
    def read(self) -> NDArray[Any]: ...

def open_multidim(src: str | os.PathLike[str] | bytes | dict[str, bytes]) -> MultidimDataset: ...
def open_catalog(
    index: _VectorSource | str,
    sources: Mapping[str, _RasterSource] | None = None,
    *,
    layer: str | None = None,
    location_field: str | None = None,
    sort_field: str | None = None,
    sort_descending: bool | None = None,
    tr: tuple[float, float] | None = None,
    te: tuple[float, float, float, float] | None = None,
    srs: str | None = None,
    bands: int | None = None,
    ot: str | None = None,
    nodata: float | None = None,
    alpha_band: int | None = None,
    cache_size: int | None = None,
    resampling: str | None = None,
) -> Raster: ...
def pansharpen(
    pan: str | Raster,
    spectral: str | Raster | Sequence[str | Raster],
    dst: str | None = None,
    *,
    weights: Sequence[float] | None = None,
    bands: Sequence[int] | None = None,
    pan_band: int = 1,
    resampling: str = "cubic",
    bitdepth: int | None = None,
    nodata: float | str | None = None,
    spat_adjust: str = "union",
    ot: str | None = None,
    format: str | None = None,
    compress: str | None = None,
    tile_size: int | None = None,
    bigtiff: bool = False,
) -> Raster:
    """Weighted Brovey pansharpening; persistent inputs and a .vrt destination create a lazy VRT."""
    ...

def sql(src: Any, query: str, *, layers: list[str] | None = None) -> Layer:
    """Run the documented OGR SQL subset; DISTINCT and summaries discard geometry."""
    ...

def simplify(src: Any, tolerance: float, *, layer: str | None = None) -> Layer:
    """Topology-preserving simplification (GEOS TopologyPreservingSimplifier); Z kept, M dropped."""
    ...

def segmentize(src: Any, max_length: float, *, layer: str | None = None) -> Layer:
    """Add vertices so that no segment is longer than ``max_length`` (OGR segmentize)."""
    ...

def convex_hull(src: Any, *, layer: str | None = None) -> Layer:
    """The convex hull of each geometry."""
    ...

def concave_hull(
    src: Any,
    ratio: float,
    *,
    allow_holes: bool = False,
    tight: bool = False,
    layer: str | None = None,
) -> Layer:
    """The concave hull of each geometry, of its polygons for polygonal input."""
    ...

def swap_xy(src: Any, *, layer: str | None = None) -> Layer:
    """Swap the X and Y coordinates of every geometry."""
    ...

def set_geom_type(
    src: Any,
    *,
    geometry_type: str | None = None,
    multi: bool = False,
    single: bool = False,
    linear: bool = False,
    dim: str | None = None,
    layer_only: bool = False,
    feature_only: bool = False,
    skip: bool = False,
    layer: str | None = None,
) -> Layer:
    """Change geometry types and dimensions, as gdal vector set-geom-type."""
    ...

def combine(
    src: Any,
    *,
    group_by: Sequence[str] | None = None,
    keep_nested: bool = False,
    add_extra_fields: str = "no",
    layer: str | None = None,
) -> Layer:
    """Combine geometries into collections, one feature per group of field values."""
    ...

def sort(src: Any, *, method: str = "hilbert", layer: str | None = None) -> Layer:
    """Sort features spatially (``"hilbert"`` or ``"strtree"``); null geometries last."""
    ...

def concat(
    srcs: Sequence[Any],
    dst: str | None = None,
    *,
    layers: list[str] | None = None,
    mode: Literal["merge-per-layer-name", "stack", "single"] = "merge-per-layer-name",
    output_layer: str | None = None,
    source_layer_field_name: str | None = None,
    source_layer_field_content: str | None = None,
    field_strategy: Literal["union", "intersection"] = "union",
    src_crs: str | None = None,
    dst_crs: str | None = None,
    format: str | None = None,
) -> VectorDataset:
    """Concatenate vector datasets (gdal vector concat, ogrmerge.py)."""
    ...

def partition(
    src: Any,
    dst: str,
    fields: list[str] | None = None,
    *,
    layers: list[str] | None = None,
    scheme: Literal["hive", "flat"] = "hive",
    pattern: str | None = None,
    feature_limit: int | None = None,
    max_file_size: int | None = None,
    omit_partitioned_fields: bool = False,
    format: str | None = None,
    overwrite: bool = False,
    append: bool = False,
) -> list[str]:
    """Partition a vector dataset into files by field values (gdal vector partition)."""
    ...

def set_field_type(
    src: Any,
    field_type: str,
    *,
    field_name: str | None = None,
    src_field_type: str | None = None,
    layers: list[str] | None = None,
    active_layer: str | None = None,
) -> VectorDataset:
    """Change field types with GDAL's value conversions (gdal vector set-field-type)."""
    ...

def make_point(
    src: Any,
    x: str,
    y: str,
    *,
    z: str | None = None,
    m: str | None = None,
    crs: str | None = None,
    layer: str | None = None,
) -> Layer:
    """Point geometries from coordinate fields (gdal vector make-point)."""
    ...

def export_schema(src: Any, *, layers: list[str] | None = None) -> dict[str, Any]:
    """The OGR_SCHEMA document of a vector dataset (gdal vector export-schema)."""
    ...

def create_vector(
    dst: str,
    *,
    schema: Mapping[str, Any] | str | os.PathLike[str] | None = None,
    like: Any = None,
    layers: list[str] | None = None,
    output_layer: str | None = None,
    geometry_type: str | None = None,
    crs: str | None = None,
    fid: str | None = None,
    fields: list[str] | None = None,
    format: str | None = None,
    update: bool = False,
    overwrite: bool = False,
) -> None:
    """Create a vector dataset of empty layers (gdal vector create)."""
    ...

def rename_layer(
    src: Any,
    dst: str | None = None,
    *,
    in_place: bool = False,
    input_layer: str | None = None,
    output_layer: str | None = None,
    ascii: bool = False,
    lower_case: bool = False,
    filename_compatible: bool = False,
    reserved_characters: str | None = None,
    replacement_character: str | None = None,
    max_length: int | None = None,
    format: str | None = None,
) -> VectorDataset: ...
def edit_vector(
    src: Any,
    dst: str | None = None,
    *,
    in_place: bool = False,
    active_layer: str | None = None,
    crs: str | None = None,
    geometry_type: str | None = None,
    metadata: list[str] | None = None,
    unset_metadata: list[str] | None = None,
    layer_metadata: list[str] | None = None,
    unset_layer_metadata: list[str] | None = None,
    unset_fid: bool = False,
    format: str | None = None,
) -> VectorDataset: ...
def update_vector(
    src: Any,
    dst: Any,
    *,
    input_layer: str | None = None,
    output_layer: str | None = None,
    key: list[str] | None = None,
    mode: str = "merge",
) -> VectorDataset: ...
