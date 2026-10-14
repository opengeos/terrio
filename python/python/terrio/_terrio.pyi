"""Type stubs for the `terrio._terrio` extension module."""

from collections.abc import Iterator, Mapping, Sequence
from typing import Any, Literal, Protocol, TypeAlias, final

import numpy as np
from numpy.typing import DTypeLike, NDArray

# PyO3 sets `__all__` to the names added to the module, in registration order.
__all__ = [
    "__version__",
    "Raster",
    "Layer",
    "VectorDataset",
    "ArrowTable",
    "from_arrow",
    "open",
    "open_raster",
    "open_vector",
    "open_bytes",
    "list_layers",
    "info",
    "drivers",
    "write_raster",
    "write_vector",
    "convert",
    "rasterize",
    "polygonize",
    "transform_points",
    "translate",
    "warp",
    "mosaic",
    "dem",
    "calc",
    "proximity",
    "sieve",
    "PointCloud",
    "PointCloudFile",
    "open_points",
    "read_points",
    "read_points_bytes",
    "convert_points",
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
    def read(
        self,
        band: int | Sequence[int] | None = None,
        window: _Window | None = None,
        overview: int | None = None,
    ) -> NDArray[Any]: ...
    @staticmethod
    def from_array(
        array: NDArray[Any],
        transform: Sequence[float],
        crs: str | None = None,
        nodata: float | None = None,
    ) -> Raster: ...
    def write(
        self,
        path: str,
        driver: str | None = None,
        compress: str | None = None,
        tile_size: int | None = None,
        bigtiff: bool = False,
        options: Mapping[str, str] | None = None,
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
    ) -> None: ...
    def to_bytes(self, format: str = "LAZ", point_format: int | None = None) -> bytes: ...
    def grid(
        self,
        resolution: float,
        *,
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
class PointCloudFile:
    """An open LAS, LAZ or COPC file: its header is read, its points on demand."""

    @property
    def format(self) -> str: ...
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
def open_vector(
    path: str, layers: Sequence[str] | None = None, bbox: _Bounds | None = None
) -> VectorDataset: ...
def open_bytes(
    data: bytes | bytearray, filename: str, lazy: bool = False
) -> Raster | VectorDataset: ...
def list_layers(path: str) -> list[str]: ...
def info(path: str, stats: bool = False, layers: Sequence[str] | None = None) -> dict[str, Any]: ...
def drivers() -> list[dict[str, Any]]: ...
def write_raster(
    raster: Raster,
    path: str,
    driver: str | None = None,
    compress: str | None = None,
    tile_size: int | None = None,
    bigtiff: bool = False,
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
    s_srs: str | None = None,
    a_srs: str | None = None,
    append: bool = False,
    overwrite: bool = False,
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
def transform_points(
    xs: Any, ys: Any, src_crs: str, dst_crs: str
) -> tuple[NDArray[np.float64], NDArray[np.float64]]: ...
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
    tap: bool = False,
    resampling: str = "near",
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
) -> Raster: ...
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
) -> None: ...
