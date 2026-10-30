# SPDX-FileCopyrightText: Copyright (c) 2019-2025, Corteva Agriscience™
# SPDX-FileCopyrightText: Terrio contributors
# SPDX-License-Identifier: MIT AND Apache-2.0
#
# Derived from (adapted and modified):
#   rioxarray 0.23.0:
#     rioxarray/_spatial_utils.py
#     rioxarray/raster_array.py

"""Spatial helpers of the accessors: coordinates from transforms, attributes, clipping.

They follow rioxarray's `_spatial_utils` (Apache-2.0, Copyright (c) 2019-2023 rioxarray
Contributors), with terrio's rasterio-compatible API (`terrio.rio`) in place of rasterio.
"""

from __future__ import annotations

import copy
import math
from collections.abc import Hashable, Iterable
from typing import TYPE_CHECKING, Any

import numpy as np
import xarray
from affine import Affine

from terrio.rio import features, warp, windows
from terrio.rio.enums import Resampling
from terrio.xarray.exceptions import MissingSpatialDimensionError

if TYPE_CHECKING:
    from terrio.rio.crs import CRS

XarrayObject = xarray.DataArray | xarray.Dataset

FILL_VALUE_NAMES = ("_FillValue", "missing_value", "fill_value", "nodata")
UNWANTED_RIO_ATTRS = ("nodatavals", "is_tiled", "res")
DEFAULT_GRID_MAP = "spatial_ref"

# The nodata value of each data type, when none is set (GDAL's gdal_calc defaults).
NODATA_DTYPE_MAP: dict[str, float | int | None] = {
    "uint8": 255,
    "uint16": 65535,
    "int16": -32768,
    "uint32": 4294967295,
    "int32": -2147483648,
    "float16": math.nan,
    "float32": math.nan,
    "float64": math.nan,
    "complex64": math.nan,
    "complex128": math.nan,
    "uint64": 18446744073709551615,
    "int64": -9223372036854775808,
    "int8": -128,
}


def index_variable(dims: Any, data: Any, attrs: Any = None, encoding: Any = None) -> Any:
    """An `xarray.IndexVariable` (whose constructor is not typed).

    Args:
        dims: Its dimension.
        data: Its values.
        attrs: Its attributes.
        encoding: Its encoding.

    Returns:
        xarray.IndexVariable: The variable.
    """
    return xarray.IndexVariable(dims, data, attrs, encoding)  # type: ignore[no-untyped-call]


def matmul(a: Affine, b: Any) -> Any:
    """Applies a transform to another, or to points (`@`, or `*` before affine 2.4).

    Args:
        a: The transform.
        b: A transform, or `(xs, ys)`.

    Returns:
        Affine | tuple: The composed transform, or the transformed points.
    """
    if hasattr(Affine, "__matmul__"):
        return a @ b
    return a * b  # pragma: no cover


def affine_has_rotation(affine: Affine) -> bool:
    """Whether a transform has rotation.

    Args:
        affine: The transform.

    Returns:
        bool: True when rotated.
    """
    return bool(affine.b == affine.d != 0)


def resolution_of(affine: Affine) -> tuple[float, float]:
    """The resolution of a transform (signs are lost when it is rotated).

    Args:
        affine: The transform.

    Returns:
        tuple: `(x_resolution, y_resolution)`.
    """
    if not affine_has_rotation(affine):
        return affine.a, affine.e
    return math.sqrt(affine.a**2 + affine.d**2), math.sqrt(affine.b**2 + affine.e**2)


def affine_to_coords(
    affine: Affine, width: int, height: int, *, x_dim: str = "x", y_dim: str = "y"
) -> dict[str, np.ndarray]:
    """Pixel-centre coordinates of a grid.

    Args:
        affine: The grid's transform.
        width: Its width.
        height: Its height.
        x_dim: The name of the x dimension.
        y_dim: The name of the y dimension.

    Returns:
        dict: The x and y coordinates (1-D, or 2-D for rotated grids).
    """
    transform: Any = matmul(affine, affine.translation(0.5, 0.5))
    if affine.is_rectilinear and not affine_has_rotation(affine):
        x_coords, _ = matmul(transform, (np.arange(width), np.zeros(width)))
        _, y_coords = matmul(transform, (np.zeros(height), np.arange(height)))
    else:
        x_coords, y_coords = matmul(
            transform, tuple(np.meshgrid(np.arange(width), np.arange(height)))
        )
    return {y_dim: y_coords, x_dim: x_coords}


def generate_spatial_coords(*, affine: Affine, width: int, height: int) -> dict[Hashable, Any]:
    """The spatial coordinates of a grid, as xarray variables.

    Args:
        affine: The grid's transform.
        width: Its width.
        height: Its height.

    Returns:
        dict: `x` and `y` index variables, or 2-D `xc` and `yc` for rotated grids.
    """
    new = affine_to_coords(affine, width, height)
    if new["x"].ndim == 1:
        return {
            "x": index_variable("x", new["x"]),
            "y": index_variable("y", new["y"]),
        }
    return {"xc": (("y", "x"), new["x"]), "yc": (("y", "x"), new["y"])}


def get_nonspatial_coords(src: XarrayObject) -> dict[Hashable, xarray.Variable]:
    """The coordinates of an object other than its spatial ones and the grid mapping.

    Args:
        src: The object.

    Returns:
        dict: The coordinates.
    """
    acc = src.terrio
    coords: dict[Hashable, xarray.Variable] = {}
    for coord in set(src.coords) - {acc.x_dim, acc.y_dim, DEFAULT_GRID_MAP}:
        if acc.x_dim in src[coord].dims and acc.y_dim in src[coord].dims:
            continue
        if src[coord].ndim == 1:
            coords[coord] = index_variable(src[coord].dims, src[coord].values, src[coord].attrs)
        else:
            coords[coord] = xarray.Variable(src[coord].dims, src[coord].values, src[coord].attrs)
    return coords


def make_coords(
    *,
    src_data_array: XarrayObject,
    dst_affine: Affine,
    dst_width: int,
    dst_height: int,
    force_generate: bool = False,
) -> dict[Hashable, Any]:
    """The coordinates of a new grid, keeping the source's other coordinates.

    Args:
        src_data_array: The source.
        dst_affine: The new transform.
        dst_width: The new width.
        dst_height: The new height.
        force_generate: Generate spatial coordinates even when the source has none.

    Returns:
        dict: The coordinates.
    """
    coords: dict[Hashable, Any] = dict(get_nonspatial_coords(src_data_array))
    acc = src_data_array.terrio
    if (
        force_generate
        or (acc.x_dim in src_data_array.coords and acc.y_dim in src_data_array.coords)
        or ("xc" in src_data_array.coords and "yc" in src_data_array.coords)
    ):
        new_coords = generate_spatial_coords(affine=dst_affine, width=dst_width, height=dst_height)
        new_coords.update(coords)
        return new_coords
    return coords


def get_data_var_message(obj: Any) -> str:
    """The data variable named in error messages.

    Args:
        obj: The object.

    Returns:
        str: The message part, or an empty string.
    """
    try:
        return f" Data variable: {obj.name}" if obj.name else ""
    except AttributeError:
        return ""


def get_spatial_dims(obj: xarray.Dataset, *, var: Hashable) -> tuple[Hashable, Hashable]:
    """The spatial dimensions of a variable of a Dataset.

    Args:
        obj: The Dataset.
        var: The variable.

    Returns:
        tuple: `(x_dim, y_dim)`.

    Raises:
        MissingSpatialDimensionError: If they cannot be found.
    """
    try:
        return obj[var].terrio.x_dim, obj[var].terrio.y_dim
    except MissingSpatialDimensionError as err:
        try:
            obj[var].terrio.set_spatial_dims(
                x_dim=obj.terrio.x_dim, y_dim=obj.terrio.y_dim, inplace=True
            )
            return obj.terrio.x_dim, obj.terrio.y_dim
        except MissingSpatialDimensionError:
            raise err from None


def has_spatial_dims(obj: xarray.Dataset, *, var: Hashable) -> bool:
    """Whether a variable of a Dataset has spatial dimensions.

    Args:
        obj: The Dataset.
        var: The variable.

    Returns:
        bool: True when it has.
    """
    try:
        get_spatial_dims(obj, var=var)
    except MissingSpatialDimensionError:
        return False
    return True


def order_bounds(
    *,
    minx: float,
    miny: float,
    maxx: float,
    maxy: float,
    resolution_x: float,
    resolution_y: float,
) -> tuple[float, float, float, float]:
    """Bounds ordered as the grid's axes run.

    Args:
        minx: The minimum x.
        miny: The minimum y.
        maxx: The maximum x.
        maxy: The maximum y.
        resolution_x: The x resolution.
        resolution_y: The y resolution.

    Returns:
        tuple: `(left, bottom, right, top)`.
    """
    top, bottom = (maxy, miny) if resolution_y < 0 else (miny, maxy)
    left, right = (maxx, minx) if resolution_x < 0 else (minx, maxx)
    return left, bottom, right, top


def convert_str_to_resampling(name: str) -> Resampling:
    """A resampling method from its name.

    Args:
        name: The name, such as `bilinear`.

    Returns:
        Resampling: The method.

    Raises:
        ValueError: If the name is unknown.
    """
    try:
        return Resampling[name.lower()]
    except KeyError:
        raise ValueError(f"Bad resampling parameter: {name}") from None


def generate_attrs(*, src_data_array: xarray.DataArray, dst_nodata: Any) -> dict[Hashable, Any]:
    """The attributes of a new array: the source's, with its nodata value.

    Args:
        src_data_array: The source.
        dst_nodata: The nodata value when the source has none.

    Returns:
        dict: The attributes.
    """
    new_attrs = copy.deepcopy(src_data_array.attrs)
    for unwanted in FILL_VALUE_NAMES + UNWANTED_RIO_ATTRS:
        new_attrs.pop(unwanted, None)
    acc = src_data_array.terrio
    fill_value = acc.nodata if acc.nodata is not None else dst_nodata
    if acc.encoded_nodata is None and fill_value is not None:
        new_attrs["_FillValue"] = fill_value
    return new_attrs


def add_attrs_proj(
    *, new_data_array: xarray.DataArray, src_data_array: xarray.DataArray
) -> xarray.DataArray:
    """Copies the source's attributes, CRS, transform and encoding to a new array.

    Args:
        new_data_array: The new array (changed in place).
        src_data_array: The source.

    Returns:
        xarray.DataArray: The new array.
    """
    new_acc = new_data_array.terrio
    src_acc = src_data_array.terrio
    if new_acc._x_dim is None:
        new_acc._x_dim = src_acc.x_dim
    if new_acc._y_dim is None:
        new_acc._y_dim = src_acc.y_dim
    new_attrs = generate_attrs(src_data_array=src_data_array, dst_nodata=None)
    if "_FillValue" in new_data_array.encoding:
        new_attrs.pop("_FillValue", None)
    new_acc.set_attrs(new_attrs, inplace=True)
    new_acc.write_crs(src_acc.crs, grid_mapping_name=src_acc.grid_mapping, inplace=True)
    new_acc.write_coordinate_system(inplace=True)
    new_acc.write_transform(inplace=True)
    new_data_array.encoding = src_data_array.encoding.copy()
    return new_data_array


def make_dst_affine(
    *,
    src_data_array: xarray.DataArray,
    src_crs: CRS,
    dst_crs: Any,
    dst_resolution: float | tuple[float, float] | None = None,
    dst_shape: tuple[int, int] | None = None,
    **kwargs: Any,
) -> tuple[Affine, int, int]:
    """The grid of a reprojected array, as GDAL suggests it.

    Args:
        src_data_array: The source.
        src_crs: Its CRS.
        dst_crs: The target CRS.
        dst_resolution: The target resolution.
        dst_shape: The target `(height, width)`.
        **kwargs: Other options of `calculate_default_transform`.

    Returns:
        tuple: `(transform, width, height)`.
    """
    acc = src_data_array.terrio
    # Georeferenced by GCPs or RPCs: the grid suggested from them, not from bounds.
    src_bounds = () if "gcps" in kwargs or "rpcs" in kwargs else acc.bounds()
    src_height, src_width = acc.shape
    dst_height, dst_width = dst_shape if dst_shape is not None else (None, None)
    resolution: Any = dst_resolution
    if isinstance(resolution, Iterable):
        resolution = tuple(abs(v) for v in resolution)
    elif resolution is not None:
        resolution = abs(resolution)
    for key, value in (
        ("resolution", resolution),
        ("dst_height", dst_height),
        ("dst_width", dst_width),
    ):
        if value is not None:
            kwargs[key] = value
    return warp.calculate_default_transform(
        src_crs, dst_crs, src_width, src_height, *src_bounds, **kwargs
    )


def clip_xarray(
    xds: xarray.DataArray,
    *,
    geometries: Iterable[Any],
    all_touched: bool,
    drop: bool,
    invert: bool,
) -> xarray.DataArray:
    """Masks (and crops) an array to geometries.

    Args:
        xds: The array.
        geometries: GeoJSON-like geometries in its CRS.
        all_touched: Keep every pixel a geometry touches.
        drop: Crop to the geometries' extent.
        invert: Keep the pixels outside the geometries instead.

    Returns:
        xarray.DataArray: The clipped array.
    """
    acc = xds.terrio
    clip_mask_arr = features.geometry_mask(
        geometries=geometries,
        out_shape=(int(acc.height), int(acc.width)),
        transform=acc.transform(recalc=True),
        invert=not invert,
        all_touched=all_touched,
    )
    clip_mask_xray = xarray.DataArray(clip_mask_arr, dims=(acc.y_dim, acc.x_dim))
    cropped_ds = xds.where(clip_mask_xray)
    if drop:
        cropped_ds.terrio.set_spatial_dims(x_dim=acc.x_dim, y_dim=acc.y_dim, inplace=True)
        cropped_ds = cropped_ds.terrio.isel_window(
            windows.get_data_window(np.ma.masked_array(clip_mask_arr, ~clip_mask_arr))
        )
    if acc.nodata is not None and not np.isnan(acc.nodata):
        cropped_ds = cropped_ds.fillna(acc.nodata)
    return cropped_ds.astype(xds.dtype)
