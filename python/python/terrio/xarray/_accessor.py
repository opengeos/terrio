"""The `.terrio` accessors of xarray DataArrays and Datasets, with rioxarray's `.rio` API.

The methods, their arguments and their output follow rioxarray (Apache-2.0, Copyright (c)
2019-2023 rioxarray Contributors). CRSs are `terrio.rio.crs.CRS` objects; warping, masking and
writing go through terrio's rasterio-compatible API (`terrio.rio`).
"""

from __future__ import annotations

import copy
import json
import math
import os
import warnings
from collections.abc import Generator, Hashable, Iterable, Mapping
from contextlib import contextmanager
from pathlib import Path
from typing import TYPE_CHECKING, Any, Literal
from uuid import uuid4

import numpy as np
import xarray
from affine import Affine

from terrio.rio import mask as rio_mask
from terrio.rio import open as rio_open
from terrio.rio import warp, windows
from terrio.rio.crs import CRS
from terrio.rio.enums import Resampling
from terrio.rio.errors import WindowError
from terrio.rio.io import MemoryFile, driver_from_extension
from terrio.xarray._crs import crs_from_user_input, crs_to_cf
from terrio.xarray._options import EXPORT_GRID_MAPPING, SKIP_MISSING_SPATIAL_DIMS, get_option
from terrio.xarray._spatial import (
    DEFAULT_GRID_MAP,
    NODATA_DTYPE_MAP,
    add_attrs_proj,
    affine_has_rotation,
    clip_xarray,
    convert_str_to_resampling,
    generate_attrs,
    get_data_var_message,
    get_spatial_dims,
    has_spatial_dims,
    make_coords,
    make_dst_affine,
    matmul,
    order_bounds,
    resolution_of,
)
from terrio.xarray.exceptions import (
    DimensionError,
    DimensionMissingCoordinateError,
    InvalidDimensionOrder,
    MissingCRS,
    MissingSpatialDimensionError,
    NoDataInBounds,
    OneDimensionalRaster,
    RioXarrayError,
    TooManyDimensions,
)

if TYPE_CHECKING:
    from terrio.xarray._io import _Source

XarrayObject = xarray.DataArray | xarray.Dataset
_Resampling = Resampling | str
_ConstantValues = float | tuple[int, int] | Mapping[Any, tuple[int, int]] | None


# Reading georeferencing, as rioxarray's CF and Zarr conventions do.


def _declared(attrs: Mapping[Hashable, Any], name: str) -> bool:
    """Whether attributes declare a Zarr convention.

    Args:
        attrs: The attributes.
        name: The convention, such as `proj:`.

    Returns:
        bool: True when declared.
    """
    conventions = attrs.get("zarr_conventions", [])
    if not isinstance(conventions, list):
        return False
    return any(isinstance(c, dict) and c.get("name") == name for c in conventions)


def _read_crs(obj: XarrayObject) -> CRS | None:
    """The CRS of an object, from its grid mapping (CF), Zarr `proj:` or `crs` attributes.

    Args:
        obj: The object.

    Returns:
        CRS | None: The CRS.
    """
    try:
        gm = obj.coords[obj.terrio.grid_mapping]
    except KeyError:
        gm = None
    if gm is not None:
        for key in ("spatial_ref", "crs_wkt"):
            if key in gm.attrs:
                try:
                    return crs_from_user_input(gm.attrs[key])
                except Exception:  # noqa: BLE001 - try the next source
                    pass
    if _declared(obj.attrs, "proj:"):
        for key in ("proj:wkt2", "proj:code"):
            value = obj.attrs.get(key)
            if value is not None:
                try:
                    return crs_from_user_input(value)
                except Exception:  # noqa: BLE001 - try the next source
                    pass
    if "crs" in obj.attrs:
        return crs_from_user_input(obj.attrs["crs"])
    return None


def _read_transform(obj: XarrayObject) -> Affine | None:
    """The transform of an object, from its grid mapping's GeoTransform or Zarr attributes.

    Args:
        obj: The object.

    Returns:
        Affine | None: The transform.
    """
    try:
        gt = obj.coords[obj.terrio.grid_mapping].attrs["GeoTransform"]
        values = [float(v) for v in str(gt).split()]
        return Affine.from_gdal(*values)
    except KeyError:
        pass
    if _declared(obj.attrs, "spatial:"):
        t = obj.attrs.get("spatial:transform")
        if isinstance(t, (list, tuple)) and len(t) == 6:
            return Affine(*t)
    if "transform" in obj.attrs:
        return Affine(*obj.attrs["transform"][:6])
    return None


def _read_spatial_dims(obj: XarrayObject) -> tuple[Hashable, Hashable] | None:
    """The spatial dimensions of an object, by name or CF attributes.

    Args:
        obj: The object.

    Returns:
        tuple | None: `(y_dim, x_dim)`.
    """
    if "x" in obj.dims and "y" in obj.dims:
        return "y", "x"
    if "longitude" in obj.dims and "latitude" in obj.dims:
        return "latitude", "longitude"
    x_dim = y_dim = None
    for coord in obj.coords:
        if obj.coords[coord].dims != (coord,):
            continue
        attrs = obj.coords[coord].attrs
        axis = str(attrs.get("axis", "")).upper()
        std = str(attrs.get("standard_name", "")).lower()
        if axis == "X" or std in ("longitude", "projection_x_coordinate"):
            x_dim = coord
        elif axis == "Y" or std in ("latitude", "projection_y_coordinate"):
            y_dim = coord
    if x_dim is not None and y_dim is not None:
        return y_dim, x_dim
    if _declared(obj.attrs, "spatial:"):
        dims = obj.attrs.get("spatial:dimensions")
        if dims is not None and len(dims) >= 2:
            y, x = dims[-2:]
            if y in obj.dims and x in obj.dims:
                return y, x
    return None


def _ensure_nodata_dtype(original_nodata: Any, new_dtype: Any) -> Any:
    """A nodata value in a data type, checking it fits.

    Args:
        original_nodata: The value.
        new_dtype: The data type.

    Returns:
        Any: The value as a NumPy scalar of the type.

    Raises:
        OverflowError: If the value does not fit.
    """
    new_dtype = np.dtype(new_dtype)
    if np.issubdtype(new_dtype, np.complexfloating):
        return original_nodata
    original = (
        float(original_nodata)
        if not np.issubdtype(type(original_nodata), np.integer)
        else original_nodata
    )
    message = f"Unable to convert nodata value ({original}) to new dtype ({new_dtype})."
    try:
        with warnings.catch_warnings():
            warnings.simplefilter("error", RuntimeWarning)
            nodata = new_dtype.type(original)
    except (OverflowError, RuntimeWarning) as error:
        raise OverflowError(message) from error
    if not np.isnan(nodata) and original != nodata:
        raise OverflowError(message)
    return nodata


class XRasterBase:
    """What the DataArray and Dataset accessors share: dimensions, CRS, transform, bounds."""

    def __init__(self, xarray_obj: XarrayObject) -> None:
        """Creates the accessor of an object.

        Args:
            xarray_obj: The DataArray or Dataset.
        """
        self._obj = xarray_obj
        self._x_dim: Hashable | None = None
        self._y_dim: Hashable | None = None
        dims = _read_spatial_dims(xarray_obj)
        if dims is not None:
            self._y_dim, self._x_dim = dims
        self._count: int | None = None
        self._height: int | None = None
        self._width: int | None = None
        self._crs: CRS | None | Literal[False] = None
        self._source: _Source | None = None
        # GCPs and RPCs written with `write_gcps` and `write_rpcs`, as rioxarray keeps them.
        self._gcps: list[Any] | None = None
        self._rpcs: Any = None

    # CRS.

    @property
    def crs(self) -> CRS | None:
        """The CRS of the object.

        Returns:
            CRS | None: The CRS, read from the grid mapping coordinate's `spatial_ref` or
            `crs_wkt`, Zarr `proj:` attributes or a `crs` attribute.
        """
        if self._crs is not None:
            return None if self._crs is False else self._crs
        parsed = _read_crs(self._obj)
        if parsed is not None:
            self._set_crs(parsed, inplace=True)
            return self._crs if self._crs is not False else None
        self._crs = False
        return None

    def _get_obj(self, inplace: bool) -> Any:
        """The object to change: this one, or a deep copy keeping the accessor's state.

        Args:
            inplace: Change this object.

        Returns:
            xarray.DataArray | xarray.Dataset: The object.
        """
        if inplace:
            return self._obj
        obj_copy = self._obj.copy(deep=True)
        acc = obj_copy.terrio
        acc._x_dim = self._x_dim
        acc._y_dim = self._y_dim
        acc._width = self._width
        acc._height = self._height
        acc._crs = self._crs
        return obj_copy

    def set_crs(self, input_crs: Any, inplace: bool = True) -> Any:
        """Sets the CRS without writing it (deprecated; use `write_crs`).

        Args:
            input_crs: Anything `CRS.from_user_input` takes.
            inplace: Change this object.

        Returns:
            xarray.DataArray | xarray.Dataset: The object.
        """
        warnings.warn(
            "It is recommended to use 'write_crs()' instead. 'set_crs()' will likely be removed "
            "in a future release.",
            FutureWarning,
            stacklevel=2,
        )
        return self._set_crs(input_crs, inplace=inplace)

    def _set_crs(self, input_crs: Any, inplace: bool = True) -> Any:
        """Sets the CRS without writing it.

        Args:
            input_crs: Anything `CRS.from_user_input` takes.
            inplace: Change this object.

        Returns:
            xarray.DataArray | xarray.Dataset: The object.
        """
        crs = crs_from_user_input(input_crs)
        obj = self._get_obj(inplace=inplace)
        obj.terrio._crs = crs
        return obj

    @property
    def grid_mapping(self) -> str:
        """The name of the grid mapping coordinate (`spatial_ref` by default).

        Returns:
            str: The name.

        Raises:
            RioXarrayError: If the variables of a Dataset have different grid mappings.
        """
        gm = self._obj.encoding.get("grid_mapping", self._obj.attrs.get("grid_mapping"))
        if gm is not None:
            return str(gm)
        gm = DEFAULT_GRID_MAP
        if isinstance(self._obj, xarray.Dataset):
            found = set()
            for var in self._obj.data_vars:
                if not has_spatial_dims(self._obj, var=var):
                    continue
                v = self._obj[var]
                var_gm = v.encoding.get("grid_mapping", v.attrs.get("grid_mapping"))
                if var_gm is not None:
                    gm = var_gm
                    found.add(var_gm)
            if len(found) > 1:
                raise RioXarrayError("Multiple grid mappings exist.")
        return str(gm)

    def write_grid_mapping(
        self, grid_mapping_name: str = DEFAULT_GRID_MAP, inplace: bool = False
    ) -> Any:
        """Writes the grid mapping name to the encoding (of every spatial variable).

        Args:
            grid_mapping_name: The grid mapping coordinate's name.
            inplace: Change this object.

        Returns:
            xarray.DataArray | xarray.Dataset: The object.
        """
        data_obj = self._get_obj(inplace=inplace)
        if isinstance(data_obj, xarray.Dataset):
            for var in data_obj.data_vars:
                try:
                    x_dim, y_dim = get_spatial_dims(data_obj, var=var)
                except MissingSpatialDimensionError:
                    continue
                new_attrs = dict(data_obj[var].attrs)
                new_attrs.pop("grid_mapping", None)
                data_obj[var].terrio.update_encoding(
                    {"grid_mapping": grid_mapping_name}, inplace=True
                ).terrio.set_attrs(new_attrs, inplace=True).terrio.set_spatial_dims(
                    x_dim=x_dim, y_dim=y_dim, inplace=True
                )
        new_attrs = dict(data_obj.attrs)
        new_attrs.pop("grid_mapping", None)
        return data_obj.terrio.update_encoding(
            {"grid_mapping": grid_mapping_name}, inplace=True
        ).terrio.set_attrs(new_attrs, inplace=True)

    def write_crs(
        self,
        input_crs: Any = None,
        grid_mapping_name: str | None = None,
        convention: Any = None,
        inplace: bool = False,
    ) -> Any:
        """Writes the CRS to the grid mapping coordinate, as CF conventions describe it.

        The coordinate (`spatial_ref` by default) gets `crs_wkt` and `spatial_ref` (WKT), the CF
        grid mapping attributes of the CRS and the `GeoTransform`; the variables get a
        `grid_mapping` encoding.

        Args:
            input_crs: The CRS (anything `CRS.from_user_input` takes); by default, the current.
            grid_mapping_name: The grid mapping coordinate's name.
            convention: Only None or `"CF"` (rioxarray's `Convention.CF`) are supported.
            inplace: Change this object.

        Returns:
            xarray.DataArray | xarray.Dataset: The object.

        Raises:
            MissingCRS: If there is no CRS to write.
            NotImplementedError: For other conventions.
        """
        _check_convention(convention)
        if input_crs is not None:
            data_obj = self._set_crs(input_crs, inplace=inplace)
        else:
            data_obj = self._get_obj(inplace=inplace)
        crs = data_obj.terrio.crs
        if crs is None:
            raise MissingCRS("CRS not found. Please set the CRS with 'rio.write_crs()'.")
        data_obj.attrs.pop("crs", None)
        acc = data_obj.terrio
        transform = _read_transform(data_obj)
        grid_mapping_name = acc.grid_mapping if grid_mapping_name is None else grid_mapping_name
        try:
            del data_obj.coords[grid_mapping_name]
        except KeyError:
            pass
        data_obj.coords[grid_mapping_name] = xarray.Variable((), 0)
        grid_map_attrs: dict[str, Any] = {}
        if get_option(EXPORT_GRID_MAPPING):
            grid_map_attrs = crs_to_cf(crs)
        crs_wkt = crs.to_wkt()
        grid_map_attrs["spatial_ref"] = crs_wkt
        grid_map_attrs["crs_wkt"] = crs_wkt
        if transform is not None:
            grid_map_attrs["GeoTransform"] = " ".join(str(v) for v in transform.to_gdal())
        data_obj.coords[grid_mapping_name].terrio.set_attrs(grid_map_attrs, inplace=True)
        return data_obj.terrio.write_grid_mapping(grid_mapping_name=grid_mapping_name, inplace=True)

    def estimate_utm_crs(self, datum_name: str = "WGS 84") -> CRS:
        """The UTM CRS of the centre of the object's bounds.

        Args:
            datum_name: `WGS 84`, `NAD83` or `NAD27`.

        Returns:
            CRS: The UTM zone's CRS.

        Raises:
            RuntimeError: If there is no CRS, or no UTM zone of the datum covers the centre.
        """
        if self.crs is None:
            raise RuntimeError("crs must be set to estimate UTM CRS.")
        if self.crs.is_geographic:
            minx, miny, maxx, maxy = self.bounds(recalc=True)
        else:
            minx, miny, maxx, maxy = self.transform_bounds("EPSG:4326", recalc=True)
        x_center = float(np.mean([minx, maxx]))
        y_center = float(np.mean([miny, maxy]))
        return CRS.from_epsg(_utm_epsg(x_center, y_center, datum_name))

    def _cached_transform(self) -> Affine | None:
        """The transform written to the object.

        Returns:
            Affine | None: The transform.
        """
        return _read_transform(self._obj)

    def write_transform(
        self,
        transform: Affine | None = None,
        grid_mapping_name: str | None = None,
        convention: Any = None,
        inplace: bool = False,
    ) -> Any:
        """Writes the transform to the grid mapping coordinate's `GeoTransform`.

        Args:
            transform: The transform; by default, computed from the coordinates.
            grid_mapping_name: The grid mapping coordinate's name.
            convention: Only None or `"CF"` are supported.
            inplace: Change this object.

        Returns:
            xarray.DataArray | xarray.Dataset: The object.
        """
        _check_convention(convention)
        transform = transform or self.transform(recalc=True)
        data_obj = self._get_obj(inplace=inplace)
        data_obj.attrs.pop("transform", None)
        acc = data_obj.terrio
        grid_mapping_name = acc.grid_mapping if grid_mapping_name is None else grid_mapping_name
        try:
            grid_map_attrs = data_obj.coords[grid_mapping_name].attrs.copy()
        except KeyError:
            data_obj.coords[grid_mapping_name] = xarray.Variable((), 0)
            grid_map_attrs = data_obj.coords[grid_mapping_name].attrs.copy()
        grid_map_attrs["GeoTransform"] = " ".join(str(v) for v in transform.to_gdal())
        data_obj.coords[grid_mapping_name].terrio.set_attrs(grid_map_attrs, inplace=True)
        return data_obj.terrio.write_grid_mapping(grid_mapping_name=grid_mapping_name, inplace=True)

    def transform(self, recalc: bool = False) -> Affine:
        """The affine transform of the grid.

        Args:
            recalc: Compute it from the coordinates instead of reading the written one.

        Returns:
            Affine: The transform.
        """
        transform = self._cached_transform()
        if transform and (not transform.is_rectilinear or affine_has_rotation(transform)):
            if recalc:
                warnings.warn(
                    "Transform that is non-rectilinear or with rotation found. "
                    "Unable to recalculate.",
                    stacklevel=2,
                )
            return transform
        try:
            src_left, _, _, src_top = self._unordered_bounds(recalc=recalc)
            src_resolution_x, src_resolution_y = self.resolution(recalc=recalc)
        except (DimensionMissingCoordinateError, DimensionError):
            return Affine.identity() if transform is None else transform
        out: Affine = matmul(
            Affine.translation(src_left, src_top), Affine.scale(src_resolution_x, src_resolution_y)
        )
        return out

    def write_coordinate_system(self, inplace: bool = False) -> Any:
        """Writes CF attributes (axis, names, units) to the x and y coordinates.

        Args:
            inplace: Change this object.

        Returns:
            xarray.DataArray | xarray.Dataset: The object.
        """
        data_obj = self._get_obj(inplace=inplace)
        crs = data_obj.terrio.crs
        is_projected = crs and crs.is_projected
        is_geographic = crs and crs.is_geographic
        x_attrs = dict(data_obj.coords[self.x_dim].attrs)
        x_attrs["axis"] = "X"
        y_attrs = dict(data_obj.coords[self.y_dim].attrs)
        y_attrs["axis"] = "Y"
        if is_projected:
            assert crs is not None
            factor = crs.linear_units_factor[-1]
            units = f"{factor} metre" if factor != 1 else "metre"
            x_attrs["long_name"] = "x coordinate of projection"
            x_attrs["standard_name"] = "projection_x_coordinate"
            x_attrs["units"] = units
            y_attrs["long_name"] = "y coordinate of projection"
            y_attrs["standard_name"] = "projection_y_coordinate"
            y_attrs["units"] = units
        elif is_geographic:
            x_attrs["long_name"] = "longitude"
            x_attrs["standard_name"] = "longitude"
            x_attrs["units"] = "degrees_east"
            y_attrs["long_name"] = "latitude"
            y_attrs["standard_name"] = "latitude"
            y_attrs["units"] = "degrees_north"
        data_obj.coords[self.y_dim].attrs = y_attrs
        data_obj.coords[self.x_dim].attrs = x_attrs
        return data_obj

    def set_attrs(self, new_attrs: Mapping[Hashable, Any], inplace: bool = False) -> Any:
        """Sets the attributes, resetting what the accessor read from them.

        Args:
            new_attrs: The attributes.
            inplace: Change this object.

        Returns:
            xarray.DataArray | xarray.Dataset: The object.
        """
        data_obj = self._get_obj(inplace=inplace)
        data_obj.attrs = dict(new_attrs)
        data_obj.terrio._nodata = None
        data_obj.terrio._crs = None
        return data_obj

    def update_attrs(self, new_attrs: Mapping[Hashable, Any], inplace: bool = False) -> Any:
        """Updates the attributes, resetting what the accessor read from them.

        Args:
            new_attrs: The attributes to add or replace.
            inplace: Change this object.

        Returns:
            xarray.DataArray | xarray.Dataset: The object.
        """
        data_attrs = dict(self._obj.attrs)
        data_attrs.update(new_attrs)
        return self.set_attrs(data_attrs, inplace=inplace)

    def set_encoding(self, new_encoding: Mapping[Any, Any], inplace: bool = False) -> Any:
        """Sets the encoding, resetting what the accessor read from it.

        Args:
            new_encoding: The encoding.
            inplace: Change this object.

        Returns:
            xarray.DataArray | xarray.Dataset: The object.
        """
        data_obj = self._get_obj(inplace=inplace)
        data_obj.encoding = dict(new_encoding)
        data_obj.terrio._nodata = None
        data_obj.terrio._crs = None
        return data_obj

    def update_encoding(self, new_encoding: Mapping[Any, Any], inplace: bool = False) -> Any:
        """Updates the encoding, resetting what the accessor read from it.

        Args:
            new_encoding: The encoding items to add or replace.
            inplace: Change this object.

        Returns:
            xarray.DataArray | xarray.Dataset: The object.
        """
        data_encoding = dict(self._obj.encoding)
        data_encoding.update(new_encoding)
        return self.set_encoding(data_encoding, inplace=inplace)

    def set_spatial_dims(self, x_dim: Hashable, y_dim: Hashable, inplace: bool = True) -> Any:
        """Sets the spatial dimensions.

        Args:
            x_dim: The x dimension.
            y_dim: The y dimension.
            inplace: Change this object.

        Returns:
            xarray.DataArray | xarray.Dataset: The object.

        Raises:
            MissingSpatialDimensionError: If a dimension does not exist.
        """
        data_obj = self._get_obj(inplace=inplace)
        if x_dim in data_obj.dims:
            data_obj.terrio._x_dim = x_dim
        else:
            raise MissingSpatialDimensionError(
                f"x dimension ({x_dim}) not found.{get_data_var_message(data_obj)}"
            )
        if y_dim in data_obj.dims:
            data_obj.terrio._y_dim = y_dim
        else:
            raise MissingSpatialDimensionError(
                f"y dimension ({y_dim}) not found.{get_data_var_message(data_obj)}"
            )
        return data_obj

    @property
    def x_dim(self) -> Hashable:
        """The x dimension.

        Returns:
            Hashable: Its name.

        Raises:
            MissingSpatialDimensionError: If it cannot be found.
        """
        if self._x_dim is not None:
            return self._x_dim
        raise MissingSpatialDimensionError(
            "x dimension not found. 'set_spatial_dims()' or using 'rename()' to change the "
            f"dimension name to 'x' can address this.{get_data_var_message(self._obj)}"
        )

    @property
    def y_dim(self) -> Hashable:
        """The y dimension.

        Returns:
            Hashable: Its name.

        Raises:
            MissingSpatialDimensionError: If it cannot be found.
        """
        if self._y_dim is not None:
            return self._y_dim
        raise MissingSpatialDimensionError(
            "y dimension not found. 'set_spatial_dims()' or using 'rename()' to change the "
            f"dimension name to 'y' can address this.{get_data_var_message(self._obj)}"
        )

    @property
    def width(self) -> int:
        """The size of the x dimension.

        Returns:
            int: The width.
        """
        if self._width is None:
            self._width = int(self._obj[self.x_dim].size)
        return self._width

    @property
    def height(self) -> int:
        """The size of the y dimension.

        Returns:
            int: The height.
        """
        if self._height is None:
            self._height = int(self._obj[self.y_dim].size)
        return self._height

    @property
    def shape(self) -> tuple[int, int]:
        """The grid's shape.

        Returns:
            tuple: `(height, width)`.
        """
        return self.height, self.width

    def _check_dimensions(self) -> str | None:
        """Checks the object is 2-D or 3-D, ordered `(extra, y, x)`.

        Returns:
            str | None: The extra dimension.

        Raises:
            TooManyDimensions: If there are more than three dimensions.
            InvalidDimensionOrder: If they are not in that order.
        """
        extra_dims = tuple(set(self._obj.dims) - {self.x_dim, self.y_dim})
        if len(extra_dims) > 1:
            raise TooManyDimensions(
                f"Only 2D and 3D data arrays supported.{get_data_var_message(self._obj)}"
            )
        if extra_dims and tuple(self._obj.dims) != (extra_dims[0], self.y_dim, self.x_dim):
            info: tuple[Hashable, ...] = (extra_dims[0], self.y_dim, self.x_dim)
            raise InvalidDimensionOrder(
                f"Invalid dimension order. Expected order: {info}. You can use "
                f"`DataArray.transpose{info}` to reorder your dimensions."
                f"{get_data_var_message(self._obj)}"
            )
        if not extra_dims and tuple(self._obj.dims) != (self.y_dim, self.x_dim):
            info = (self.y_dim, self.x_dim)
            raise InvalidDimensionOrder(
                f"Invalid dimension order. Expected order: {info}. You can use "
                f"`DataArray.transpose{info}` to reorder your dimensions."
                f"{get_data_var_message(self._obj)}"
            )
        return str(extra_dims[0]) if extra_dims else None

    @property
    def count(self) -> int:
        """The number of bands (the size of the extra dimension, or 1).

        Returns:
            int: The count.
        """
        if self._count is None:
            extra_dim = self._check_dimensions()
            self._count = 1 if extra_dim is None else int(self._obj[extra_dim].size)
        return self._count

    def _internal_bounds(self) -> tuple[float, float, float, float]:
        """The coordinates of the first and last pixel centres.

        Returns:
            tuple: `(left, bottom, right, top)` pixel centres.

        Raises:
            DimensionMissingCoordinateError: If a spatial dimension has no coordinate.
            NoDataInBounds: If a dimension is empty.
        """
        if self.x_dim not in self._obj.coords:
            raise DimensionMissingCoordinateError(f"{self.x_dim} missing coordinates.")
        if self.y_dim not in self._obj.coords:
            raise DimensionMissingCoordinateError(f"{self.y_dim} missing coordinates.")
        try:
            left = float(self._obj[self.x_dim][0])
            right = float(self._obj[self.x_dim][-1])
            top = float(self._obj[self.y_dim][0])
            bottom = float(self._obj[self.y_dim][-1])
        except IndexError:
            raise NoDataInBounds(
                f"Unable to determine bounds from coordinates.{get_data_var_message(self._obj)}"
            ) from None
        return left, bottom, right, top

    def resolution(self, recalc: bool = False) -> tuple[float, float]:
        """The grid's resolution (signs lost when it is rotated).

        Args:
            recalc: Compute it from the coordinates instead of the written transform.

        Returns:
            tuple: `(x_resolution, y_resolution)`.

        Raises:
            OneDimensionalRaster: If it cannot be computed from a single row or column.
        """
        transform = self._cached_transform()
        if (not recalc or self.width == 1 or self.height == 1) and transform is not None:
            return resolution_of(transform)
        try:
            left, bottom, right, top = self._internal_bounds()
        except DimensionMissingCoordinateError:
            if transform is None:
                raise
            return resolution_of(transform)
        if self.width == 1 or self.height == 1:
            raise OneDimensionalRaster(
                "Only 1 dimenional array found. Cannot calculate the resolution."
                f"{get_data_var_message(self._obj)}"
            )
        return (right - left) / (self.width - 1), (bottom - top) / (self.height - 1)

    def _unordered_bounds(self, recalc: bool = False) -> tuple[float, float, float, float]:
        """The outer bounds, in the order of the axes.

        Args:
            recalc: Compute them from the coordinates.

        Returns:
            tuple: `(left, bottom, right, top)`.

        Raises:
            RioXarrayError: If neither coordinates nor a transform exist.
        """
        resolution_x, resolution_y = self.resolution(recalc=recalc)
        try:
            left, bottom, right, top = self._internal_bounds()
            left -= resolution_x / 2.0
            right += resolution_x / 2.0
            top -= resolution_y / 2.0
            bottom += resolution_y / 2.0
        except DimensionMissingCoordinateError as error:
            transform = self._cached_transform()
            if not transform:
                raise RioXarrayError("Transform not able to be determined.") from error
            left = transform.c
            top = transform.f
            right = left + resolution_x * self.width
            bottom = top + resolution_y * self.height
        return left, bottom, right, top

    def bounds(self, *, recalc: bool = False) -> tuple[float, float, float, float]:
        """The outer bounds of the grid.

        Args:
            recalc: Compute them from the coordinates.

        Returns:
            tuple: `(left, bottom, right, top)`.
        """
        minx, miny, maxx, maxy = self._unordered_bounds(recalc=recalc)
        resolution_x, resolution_y = self.resolution(recalc=recalc)
        return order_bounds(
            minx=minx,
            miny=miny,
            maxx=maxx,
            maxy=maxy,
            resolution_x=resolution_x,
            resolution_y=resolution_y,
        )

    def isel_window(self, window: windows.Window, *, pad: bool = False) -> Any:
        """The pixels of a window (lazily, for arrays opened with `open_rasterio`).

        Args:
            window: A `terrio.rio.windows.Window` (or rasterio's); offsets are floored and
                stops ceiled.
            pad: Extend the result to the whole window (with nodata).

        Returns:
            xarray.DataArray | xarray.Dataset: The window.
        """
        (row_start, row_stop), (col_start, col_stop) = window.toranges()
        row_start = 0 if row_start < 0 else math.floor(row_start)
        row_stop = 0 if row_stop < 0 else math.ceil(row_stop)
        col_start = 0 if col_start < 0 else math.floor(col_start)
        col_stop = 0 if col_stop < 0 else math.ceil(col_stop)
        row_slice = slice(int(row_start), int(row_stop))
        col_slice = slice(int(col_start), int(col_stop))
        win_t = windows.transform(
            windows.Window.from_slices(
                rows=row_slice, cols=col_slice, width=self.width, height=self.height
            ),
            self.transform(recalc=True),
        )
        subset = self._obj.isel({self.y_dim: row_slice, self.x_dim: col_slice}).copy()
        subset.terrio.set_spatial_dims(x_dim=self.x_dim, y_dim=self.y_dim, inplace=True)
        subset.terrio.write_transform(transform=win_t, inplace=True)
        if pad:
            return subset.terrio.pad_box(
                *windows.bounds(
                    windows.Window(window.col_off, window.row_off, window.width, window.height),
                    self.transform(recalc=True),
                )
            )
        return subset

    def slice_xy(self, minx: float, miny: float, maxx: float, maxy: float) -> Any:
        """The pixels whose centres are within bounds.

        Args:
            minx: The minimum x.
            miny: The minimum y.
            maxx: The maximum x.
            maxy: The maximum y.

        Returns:
            xarray.DataArray | xarray.Dataset: The slice.
        """
        left, bottom, right, top = self._internal_bounds()
        y_slice = slice(maxy, miny) if top > bottom else slice(miny, maxy)
        x_slice = slice(maxx, minx) if left > right else slice(minx, maxx)
        subset = self._obj.sel({self.x_dim: x_slice, self.y_dim: y_slice}).copy()
        subset.terrio.set_spatial_dims(x_dim=self.x_dim, y_dim=self.y_dim, inplace=True)
        subset.terrio.write_transform(inplace=True)
        return subset

    def transform_bounds(
        self, dst_crs: Any, *, densify_pts: int = 21, recalc: bool = False
    ) -> tuple[float, float, float, float]:
        """The bounds in another CRS, densifying the edges.

        Args:
            dst_crs: The target CRS.
            densify_pts: Points added along each edge.
            recalc: Compute the bounds from the coordinates.

        Returns:
            tuple: `(left, bottom, right, top)` in the target CRS.
        """
        left, bottom, right, top = warp.transform_bounds(
            self.crs, dst_crs, *self.bounds(recalc=recalc), densify_pts=densify_pts
        )
        return left, bottom, right, top

    def write_gcps(
        self,
        gcps: Iterable[Any],
        gcp_crs: Any,
        *,
        grid_mapping_name: str | None = None,
        inplace: bool = False,
    ) -> Any:
        """Writes ground control points to the grid mapping variable, as rioxarray does.

        The points go to its `gcps` attribute as a GeoJSON FeatureCollection, and their CRS is
        written as the array's.

        Args:
            gcps: `terrio.rio.control.GroundControlPoint` objects (or rasterio's).
            gcp_crs: The CRS of the points' coordinates.
            grid_mapping_name: The grid mapping variable (by default the array's).
            inplace: Modify the object rather than a copy.

        Returns:
            xarray.DataArray | xarray.Dataset: The object with its GCPs.
        """
        grid_mapping_name = self.grid_mapping if grid_mapping_name is None else grid_mapping_name
        data_obj = self._get_obj(inplace=True)
        if gcp_crs:
            data_obj = data_obj.terrio.write_crs(
                gcp_crs, grid_mapping_name=grid_mapping_name, inplace=inplace
            )
        if grid_mapping_name not in data_obj.coords:
            data_obj.coords[grid_mapping_name] = xarray.Variable((), 0)
        attrs = data_obj.coords[grid_mapping_name].attrs.copy()
        gcps = list(gcps)
        attrs["gcps"] = json.dumps(_gcps_to_geojson(gcps))
        data_obj.coords[grid_mapping_name].terrio.set_attrs(attrs, inplace=True)
        # Kept by the accessor too, as rioxarray keeps them: writing the CRS again (as
        # `write_rpcs` does) rebuilds the grid mapping variable.
        data_obj.terrio._gcps = gcps
        return data_obj

    def get_gcps(self) -> list[Any] | None:
        """The ground control points written with `write_gcps` (or read from the file).

        Returns:
            list | None: `terrio.rio.control.GroundControlPoint` objects, or None.
        """
        from terrio.rio.control import GroundControlPoint

        if self._gcps is not None:
            return list(self._gcps)
        try:
            geojson = json.loads(self._obj.coords[self.grid_mapping].attrs["gcps"])
        except (KeyError, AttributeError):
            return None
        out = []
        for f in geojson["features"]:
            x, y, *z = f["geometry"]["coordinates"]
            p = f["properties"]
            gcp = GroundControlPoint(row=p["row"], col=p["col"], x=x, y=y, z=z[0] if z else None)
            gcp.id, gcp.info = p["id"], p["info"]
            out.append(gcp)
        return out

    def write_rpcs(
        self, rpcs: Any, *, grid_mapping_name: str | None = None, inplace: bool = False
    ) -> Any:
        """Writes rational polynomial coefficients to the grid mapping variable, as rioxarray does.

        The coefficients go to its `rpcs` attribute as JSON, and the CRS written is EPSG:4326, the
        CRS of RPCs.

        Args:
            rpcs: A `terrio.rio.rpc.RPC` (or rasterio's).
            grid_mapping_name: The grid mapping variable (by default the array's).
            inplace: Modify the object rather than a copy.

        Returns:
            xarray.DataArray | xarray.Dataset: The object with its RPCs.
        """
        grid_mapping_name = self.grid_mapping if grid_mapping_name is None else grid_mapping_name
        data_obj = self._get_obj(inplace=True)
        data_obj = data_obj.terrio.write_crs(
            "epsg:4326", grid_mapping_name=grid_mapping_name, inplace=inplace
        )
        if grid_mapping_name not in data_obj.coords:
            data_obj.coords[grid_mapping_name] = xarray.Variable((), 0)
        attrs = data_obj.coords[grid_mapping_name].attrs.copy()
        attrs["rpcs"] = json.dumps(rpcs.to_dict())
        data_obj.coords[grid_mapping_name].terrio.set_attrs(attrs, inplace=True)
        data_obj.terrio._rpcs = rpcs
        return data_obj

    def get_rpcs(self) -> Any:
        """The rational polynomial coefficients written with `write_rpcs` (or read from the file).

        Returns:
            terrio.rio.rpc.RPC | None: The coefficients, or None.
        """
        from terrio.rio.rpc import RPC

        if self._rpcs is not None:
            return self._rpcs
        try:
            return RPC(**json.loads(self._obj.coords[self.grid_mapping].attrs["rpcs"]))
        except (KeyError, AttributeError):
            return None


def _gcps_to_geojson(gcps: Iterable[Any]) -> dict[str, Any]:
    """Ground control points as rioxarray stores them: a GeoJSON FeatureCollection.

    Args:
        gcps: The points.

    Returns:
        dict: The FeatureCollection, each point's id, info, row and col as properties.
    """
    features = []
    for gcp in gcps:
        coords = [gcp.x, gcp.y] if gcp.z is None else [gcp.x, gcp.y, gcp.z]
        features.append(
            {
                "type": "Feature",
                "properties": {"id": gcp.id, "info": gcp.info, "row": gcp.row, "col": gcp.col},
                "geometry": {"type": "Point", "coordinates": coords},
            }
        )
    return {"type": "FeatureCollection", "features": features}


def _check_convention(convention: Any) -> None:
    """Checks a georeferencing convention is supported.

    Args:
        convention: None, `"CF"` or rioxarray's `Convention.CF`.

    Raises:
        NotImplementedError: For other conventions.
    """
    if convention is None:
        return
    value = getattr(convention, "value", convention)
    if str(value).upper() != "CF":
        raise NotImplementedError("terrio.xarray writes the CF convention only")


def _utm_epsg(lon: float, lat: float, datum_name: str) -> int:
    """The EPSG code of the UTM zone of a point.

    Args:
        lon: The longitude.
        lat: The latitude.
        datum_name: `WGS 84`, `NAD83` or `NAD27`.

    Returns:
        int: The EPSG code.

    Raises:
        RuntimeError: If no UTM zone of the datum covers the point.
    """
    if not (-180 <= lon <= 180 and -80 <= lat <= 84):
        raise RuntimeError("Unable to determine UTM CRS")
    shifted = lon + 180.0
    raw_zone = int(shifted // 6) + 1
    zone = min(raw_zone, 60)
    if shifted % 6 == 0 and 1 < raw_zone <= 60:
        # On a zone boundary, PROJ's database query lists the lower zone first (180°E stays in
        # zone 60).
        zone -= 1
    name = datum_name.upper().replace(" ", "")
    if name in ("WGS84", "WGS1984"):
        return (32700 if lat < 0 else 32600) + zone
    if name in ("NAD83", "NORTHAMERICANDATUM1983") and lat >= 0 and 1 <= zone <= 23:
        return 26900 + zone
    if name in ("NAD27", "NORTHAMERICANDATUM1927") and lat >= 0 and 1 <= zone <= 22:
        return 26700 + zone
    raise RuntimeError("Unable to determine UTM CRS")


def _flip_north_up(data: np.ndarray, t: Affine) -> tuple[np.ndarray, Affine]:
    """Flips an array (and its transform) so that it is north-up with positive pixel width.

    Args:
        data: The array; its last two axes are y and x.
        t: Its transform (not rotated).

    Returns:
        tuple: The array and transform.
    """
    h, w = data.shape[-2:]
    if t.e > 0:
        data = data[..., ::-1, :]
        t = Affine(t.a, t.b, t.c, t.d, -t.e, t.f + t.e * h)
    if t.a < 0:
        data = data[..., :, ::-1]
        t = Affine(-t.a, t.b, t.c + t.a * w, t.d, t.e, t.f)
    return data, t


def _north_up(t: Affine, width: int, height: int) -> Affine:
    """The north-up transform of the same grid.

    Args:
        t: A transform (not rotated).
        width: The grid's width.
        height: Its height.

    Returns:
        Affine: The transform with positive pixel width and negative pixel height.
    """
    if t.e > 0:
        t = Affine(t.a, t.b, t.c, t.d, -t.e, t.f + t.e * height)
    if t.a < 0:
        t = Affine(-t.a, t.b, t.c + t.a * width, t.d, t.e, t.f)
    return t


class TerrioArray(XRasterBase):
    """The `.terrio` accessor of DataArrays: rioxarray's `.rio` API on terrio."""

    def __init__(self, xarray_obj: xarray.DataArray) -> None:
        """Creates the accessor of a DataArray.

        Args:
            xarray_obj: The DataArray.
        """
        super().__init__(xarray_obj)
        self._obj: xarray.DataArray = xarray_obj
        self._nodata: Any = None

    def set_nodata(self, input_nodata: float | None, *, inplace: bool = True) -> xarray.DataArray:
        """Sets the nodata value without writing it.

        Args:
            input_nodata: The value.
            inplace: Change this object.

        Returns:
            xarray.DataArray: The array.
        """
        obj: xarray.DataArray = self._get_obj(inplace=inplace)
        obj.terrio._nodata = input_nodata
        return obj

    def write_nodata(
        self, input_nodata: float | None, *, encoded: bool = False, inplace: bool = False
    ) -> xarray.DataArray:
        """Writes the nodata value to the `_FillValue` attribute (or encoding).

        Args:
            input_nodata: The value, or None to remove it.
            encoded: Write it to the encoding (the data is masked with NaN).
            inplace: Change this object.

        Returns:
            xarray.DataArray: The array.
        """
        data_obj: xarray.DataArray = self._get_obj(inplace=inplace)
        value: Any = False if input_nodata is None else input_nodata
        if value is not False:
            value = _ensure_nodata_dtype(value, self._obj.dtype)
            if encoded:
                data_obj.terrio.update_encoding({"_FillValue": value}, inplace=True)
            else:
                data_obj.terrio.update_attrs({"_FillValue": value}, inplace=True)
        if value is False or encoded:
            new_attrs = dict(data_obj.attrs)
            new_attrs.pop("_FillValue", None)
            data_obj.terrio.set_attrs(new_attrs, inplace=True)
        if value is False and encoded:
            new_encoding = dict(data_obj.encoding)
            new_encoding.pop("_FillValue", None)
            data_obj.terrio.set_encoding(new_encoding, inplace=True)
        if not encoded:
            data_obj.terrio.set_nodata(value, inplace=True)
        return data_obj

    @property
    def encoded_nodata(self) -> Any:
        """The nodata value in the encoding (data masked with NaN).

        Returns:
            Any: The value, or None.
        """
        encoded = self._obj.encoding.get("_FillValue")
        if encoded is None:
            return None
        return _ensure_nodata_dtype(encoded, self._obj.dtype)

    @property
    def nodata(self) -> Any:
        """The nodata value: NaN when it is encoded, else the `_FillValue` attribute.

        Returns:
            Any: The value (of the array's data type), or None.
        """
        if self._nodata is not None:
            return None if self._nodata is False else self._nodata
        if self.encoded_nodata is not None:
            self._nodata = xarray.core.dtypes.get_fill_value(  # type: ignore[no-untyped-call]
                self._obj.dtype
            )
        else:
            attrs = self._obj.attrs
            self._nodata = attrs.get(
                "_FillValue",
                attrs.get("missing_value", attrs.get("fill_value", attrs.get("nodata"))),
            )
        if self._nodata is None and self._source is not None:
            self._nodata = self._source.nodata
        if self._nodata is None:
            try:
                self._nodata = self._obj.attrs["nodatavals"][0]
            except (KeyError, IndexError):
                pass
        if self._nodata is None:
            self._nodata = False
            return None
        self._nodata = _ensure_nodata_dtype(self._nodata, self._obj.dtype)
        return self._nodata

    # Warping.

    def reproject(
        self,
        dst_crs: Any,
        *,
        resolution: float | tuple[float, float] | None = None,
        shape: tuple[int, int] | None = None,
        transform: Affine | None = None,
        resampling: _Resampling = Resampling.nearest,
        nodata: float | None = None,
        **kwargs: Any,
    ) -> xarray.DataArray:
        """Reprojects the array with terrio's warper, as `rio.reproject` does with GDAL's.

        Args:
            dst_crs: The target CRS.
            resolution: The target resolution (not with `shape` or `transform`).
            shape: The target `(height, width)`.
            transform: The target transform.
            resampling: The resampling method (a `Resampling` or its name).
            nodata: The target nodata value (default: the array's, else the data type's).
            **kwargs: Options of `terrio.rio.warp.reproject` (`tolerance`...).

        Returns:
            xarray.DataArray: The reprojected array.

        Raises:
            RioXarrayError: If resolution is given with shape or transform.
            MissingCRS: If the array has no CRS.
        """
        if resolution is not None and (shape is not None or transform is not None):
            raise RioXarrayError("resolution cannot be used with shape or transform.")
        if self.crs is None:
            raise MissingCRS(
                "CRS not found. Please set the CRS with 'rio.write_crs()'."
                f"{get_data_var_message(self._obj)}"
            )
        if isinstance(resampling, str):
            resampling = convert_str_to_resampling(resampling)
        # An image georeferenced by GCPs is warped through them, as rioxarray does.
        gcps = self.get_gcps()
        if gcps:
            kwargs.setdefault("gcps", gcps)
        elif self.get_rpcs() is not None:
            raise NotImplementedError(
                "terrio does not warp with RPCs yet: reproject an image georeferenced by GCPs "
                "or a transform"
            )
        src_affine = self.transform(recalc=True)
        if transform is None:
            dst_affine, dst_width, dst_height = make_dst_affine(
                src_data_array=self._obj,
                src_crs=self.crs,
                dst_crs=dst_crs,
                dst_resolution=resolution,
                dst_shape=shape,
                **kwargs,
            )
        else:
            dst_affine = transform
            dst_height, dst_width = shape if shape is not None else self.shape
        extra_dim = self._check_dimensions()
        if extra_dim:
            dst_shape: tuple[int, ...] = (self._obj[extra_dim].size, dst_height, dst_width)
        else:
            dst_shape = (dst_height, dst_width)
        dst_data = np.zeros(dst_shape, dtype=self._obj.dtype.type)
        default_nodata = (
            NODATA_DTYPE_MAP.get(self._obj.dtype.name) if self.nodata is None else self.nodata
        )
        dst_nodata = default_nodata if nodata is None else nodata
        if gcps:
            src_values, src_t = np.asarray(self._obj.values), None
        else:
            src_values, src_t = _flip_north_up(np.asarray(self._obj.values), src_affine)
        dst_t = _north_up(dst_affine, dst_width, dst_height)
        warp.reproject(
            source=np.ascontiguousarray(src_values),
            destination=dst_data,
            src_transform=src_t,
            src_crs=self.crs,
            src_nodata=self.nodata,
            dst_transform=dst_t,
            dst_crs=dst_crs,
            dst_nodata=dst_nodata,
            resampling=resampling,
            **{k: v for k, v in kwargs.items() if k not in ("dst_width", "dst_height")},
        )
        if dst_t != dst_affine:
            if dst_affine.e > 0:
                dst_data = dst_data[..., ::-1, :]
            if dst_affine.a < 0:
                dst_data = dst_data[..., :, ::-1]
            dst_data = np.ascontiguousarray(dst_data)
        new_attrs = generate_attrs(src_data_array=self._obj, dst_nodata=dst_nodata)
        dst_dims: list[Hashable] = []
        for dim in self._obj.dims:
            if dim == self.x_dim:
                dst_dims.append("x")
            elif dim == self.y_dim:
                dst_dims.append("y")
            else:
                dst_dims.append(dim)
        xda = xarray.DataArray(
            name=self._obj.name,
            data=dst_data,
            coords=make_coords(
                src_data_array=self._obj,
                dst_affine=dst_affine,
                dst_width=dst_width,
                dst_height=dst_height,
                # An image without coordinates (GCPs) gets the target grid's.
                force_generate=bool(gcps),
            ),
            dims=tuple(dst_dims),
            attrs=new_attrs,
        )
        xda.encoding = self._obj.encoding
        acc = xda.terrio
        acc.write_transform(dst_affine, inplace=True)
        acc.write_crs(dst_crs, inplace=True)
        acc.write_coordinate_system(inplace=True)
        return xda

    def reproject_match(
        self,
        match_data_array: XarrayObject,
        *,
        resampling: _Resampling = Resampling.nearest,
        **reproject_kwargs: Any,
    ) -> xarray.DataArray:
        """Reprojects the array onto another's grid (CRS, transform and shape).

        Args:
            match_data_array: The array (or Dataset) whose grid to match.
            resampling: The resampling method.
            **reproject_kwargs: Other options of `reproject`.

        Returns:
            xarray.DataArray: The array on the other's grid, with its coordinates.
        """
        match = match_data_array.terrio
        reprojected = self.reproject(
            match.crs,
            transform=match.transform(recalc=True),
            shape=match.shape,
            resampling=resampling,
            **reproject_kwargs,
        )
        racc = reprojected.terrio
        x_attrs = reprojected[racc.x_dim].attrs.copy()
        y_attrs = reprojected[racc.y_dim].attrs.copy()
        reprojected = reprojected.assign_coords(
            {
                racc.x_dim: copy.copy(match_data_array[match.x_dim].values),
                racc.y_dim: copy.copy(match_data_array[match.y_dim].values),
            }
        )
        racc = reprojected.terrio
        reprojected[racc.x_dim].attrs = x_attrs
        reprojected[racc.y_dim].attrs = y_attrs
        return reprojected

    # Padding and clipping.

    def pad_xy(
        self,
        minx: float,
        miny: float,
        maxx: float,
        maxy: float,
        *,
        constant_values: _ConstantValues = None,
    ) -> xarray.DataArray:
        """Pads the array so that its pixel centres cover bounds.

        Args:
            minx: The minimum x.
            miny: The minimum y.
            maxx: The maximum x.
            maxy: The maximum y.
            constant_values: The padding value (default: nodata, else NaN).

        Returns:
            xarray.DataArray: The padded array.
        """
        left, bottom, right, top = self._internal_bounds()
        resolution_x, resolution_y = self.resolution()
        y_before = y_after = 0
        x_before = x_after = 0
        y_coord: Any = self._obj[self.y_dim]
        x_coord: Any = self._obj[self.x_dim]
        if top - resolution_y < maxy:
            new_y_coord = np.arange(bottom, maxy, -resolution_y)[::-1]
            y_before = len(new_y_coord) - len(y_coord)
            y_coord = new_y_coord
            top = y_coord[0]
        if bottom + resolution_y > miny:
            new_y_coord = np.arange(top, miny, resolution_y)
            y_after = len(new_y_coord) - len(y_coord)
            y_coord = new_y_coord
            bottom = y_coord[-1]
        if left - resolution_x > minx:
            new_x_coord = np.arange(right, minx, -resolution_x)[::-1]
            x_before = len(new_x_coord) - len(x_coord)
            x_coord = new_x_coord
            left = x_coord[0]
        if right + resolution_x < maxx:
            new_x_coord = np.arange(left, maxx, resolution_x)
            x_after = len(new_x_coord) - len(x_coord)
            x_coord = new_x_coord
            right = x_coord[-1]
        if constant_values is None:
            constant_values = np.nan if self.nodata is None else self.nodata
        superset = self._obj.pad(
            pad_width={self.x_dim: (x_before, x_after), self.y_dim: (y_before, y_after)},
            constant_values=constant_values,
        )
        superset.terrio.set_spatial_dims(x_dim=self.x_dim, y_dim=self.y_dim, inplace=True)
        superset[self.x_dim] = x_coord
        superset[self.y_dim] = y_coord
        superset.terrio.write_transform(inplace=True)
        return superset

    def pad_box(
        self,
        minx: float,
        miny: float,
        maxx: float,
        maxy: float,
        *,
        constant_values: _ConstantValues = None,
    ) -> xarray.DataArray:
        """Pads the array to cover bounds.

        Args:
            minx: The minimum x.
            miny: The minimum y.
            maxx: The maximum x.
            maxy: The maximum y.
            constant_values: The padding value (default: nodata, else NaN).

        Returns:
            xarray.DataArray: The padded array.
        """
        resolution_x, resolution_y = self.resolution()
        padded = self.pad_xy(
            minx=minx - abs(resolution_x) / 2.0,
            miny=miny - abs(resolution_y) / 2.0,
            maxx=maxx + abs(resolution_x) / 2.0,
            maxy=maxy + abs(resolution_y) / 2.0,
            constant_values=constant_values,
        )
        add_attrs_proj(new_data_array=padded, src_data_array=self._obj)
        return padded

    def clip_box(
        self,
        minx: float,
        miny: float,
        maxx: float,
        maxy: float,
        *,
        auto_expand: bool | int = False,
        auto_expand_limit: int = 3,
        crs: Any = None,
        allow_one_dimensional_raster: bool = False,
    ) -> xarray.DataArray:
        """Clips the array to bounds (lazily, for arrays opened with `open_rasterio`).

        Args:
            minx: The minimum x.
            miny: The minimum y.
            maxx: The maximum x.
            maxy: The maximum y.
            auto_expand: Expand the bounds when the result would be a single row or column.
            auto_expand_limit: The number of expansions.
            crs: The CRS of the bounds (default: the array's).
            allow_one_dimensional_raster: Allow a single row or column.

        Returns:
            xarray.DataArray: The clipped array.

        Raises:
            OneDimensionalRaster: If the array or the result is a single row or column.
            MissingCRS: If `crs` is given and the array has no CRS.
            RioXarrayError: If the transformed bounds cross the antimeridian.
            NoDataInBounds: If no pixel is in the bounds.
        """
        if not allow_one_dimensional_raster and (self.width == 1 or self.height == 1):
            raise OneDimensionalRaster(
                "At least one of the raster x,y coordinates has only one point."
                f"{get_data_var_message(self._obj)}. "
                "Set allow_one_dimensional_raster=True to disable this error."
            )
        if crs is not None and self.crs is None:
            raise MissingCRS(
                "CRS not found. Please set the CRS with 'rio.write_crs()'."
                f"{get_data_var_message(self._obj)}"
            )
        crs = crs_from_user_input(crs) if crs is not None else self.crs
        if self.crs != crs:
            minx, miny, maxx, maxy = warp.transform_bounds(
                src_crs=crs, dst_crs=self.crs, left=minx, bottom=miny, right=maxx, top=maxy
            )
            if self.crs is not None and self.crs.is_geographic and minx > maxx:
                raise RioXarrayError(
                    "Transformed bounds crossed the antimeridian. Please transform your bounds "
                    "manually using terrio.rio.warp.transform_bounds and clip using the bounding "
                    "box(es) desired."
                )
        resolution_x, resolution_y = self.resolution()
        left, bottom, right, top = order_bounds(
            minx=minx,
            miny=miny,
            maxx=maxx,
            maxy=maxy,
            resolution_x=resolution_x,
            resolution_y=resolution_y,
        )
        window_error: WindowError | None = None
        cl_array: Any = None
        try:
            window = windows.from_bounds(
                left=np.array(left).item(),
                bottom=np.array(bottom).item(),
                right=np.array(right).item(),
                top=np.array(top).item(),
                transform=self.transform(recalc=True),
            )
            cl_array = self.isel_window(window)
        except WindowError as err:
            window_error = err
        if window_error or cl_array.terrio.width <= 1 or cl_array.terrio.height <= 1:
            if auto_expand and auto_expand < auto_expand_limit:
                return self.clip_box(
                    minx=minx - abs(resolution_x) / 2.0,
                    miny=miny - abs(resolution_y) / 2.0,
                    maxx=maxx + abs(resolution_x) / 2.0,
                    maxy=maxy + abs(resolution_y) / 2.0,
                    auto_expand=int(auto_expand) + 1,
                    auto_expand_limit=auto_expand_limit,
                )
            if window_error:
                raise window_error
            if cl_array.terrio.width < 1 or cl_array.terrio.height < 1:
                raise NoDataInBounds(f"No data found in bounds.{get_data_var_message(self._obj)}")
            if not allow_one_dimensional_raster and (
                cl_array.terrio.width == 1 or cl_array.terrio.height == 1
            ):
                raise OneDimensionalRaster(
                    "At least one of the clipped raster x,y coordinates has only one point."
                    f"{get_data_var_message(self._obj)}. "
                    "Set allow_one_dimensional_raster=True to disable this error."
                )
        add_attrs_proj(new_data_array=cl_array, src_data_array=self._obj)
        return cl_array  # type: ignore[no-any-return]

    def clip(
        self,
        geometries: Iterable[Any],
        crs: Any = None,
        *,
        all_touched: bool = False,
        drop: bool = True,
        invert: bool = False,
        from_disk: bool = False,
    ) -> xarray.DataArray:
        """Clips the array to geometries: pixels outside them become nodata.

        Args:
            geometries: GeoJSON-like geometries (dicts, or objects with `__geo_interface__`).
            crs: Their CRS (default: the array's).
            all_touched: Keep every pixel a geometry touches (else those whose centre is inside).
            drop: Crop to the geometries' extent.
            invert: Keep the pixels outside the geometries instead.
            from_disk: Read only the geometries' window from the file (arrays opened with
                `open_rasterio`).

        Returns:
            xarray.DataArray: The clipped array.

        Raises:
            MissingCRS: If the array has no CRS.
            NoDataInBounds: If no pixel is left.
        """
        if self.crs is None:
            raise MissingCRS(
                "CRS not found. Please set the CRS with 'rio.write_crs()'."
                f"{get_data_var_message(self._obj)}"
            )
        crs = crs_from_user_input(crs) if crs is not None else self.crs
        geoms = [getattr(g, "__geo_interface__", g) for g in geometries]
        if self.crs != crs:
            geoms = list(warp.transform_geom(crs, self.crs, geoms))
        cropped: xarray.DataArray | None = None
        if from_disk:
            cropped = self._clip_from_disk(geoms, all_touched=all_touched, drop=drop, invert=invert)
        if cropped is None:
            cropped = clip_xarray(
                self._obj, geometries=geoms, all_touched=all_touched, drop=drop, invert=invert
            )
        if cropped.coords[self.x_dim].size < 1 or cropped.coords[self.y_dim].size < 1:
            raise NoDataInBounds(f"No data found in bounds.{get_data_var_message(self._obj)}")
        add_attrs_proj(new_data_array=cropped, src_data_array=self._obj)
        return cropped

    def _clip_from_disk(
        self, geometries: list[Any], *, all_touched: bool, drop: bool, invert: bool
    ) -> xarray.DataArray | None:
        """Clips by reading the geometries' window from the file.

        Args:
            geometries: GeoJSON-like geometries in the array's CRS.
            all_touched: Keep every pixel a geometry touches.
            drop: Crop to the geometries' extent.
            invert: Keep the pixels outside the geometries.

        Returns:
            xarray.DataArray | None: The clipped array, or None when the file is not known.
        """
        if self._source is None or self._source.overview is not None:
            warnings.warn("File object not available, clipping in-memory.", stacklevel=3)
            return None
        with rio_open(self._source.name) as rio_dataset:
            out_image, out_transform = rio_mask.mask(
                rio_dataset, geometries, all_touched=all_touched, invert=invert, crop=drop
            )
        encoded = self.encoded_nodata
        if encoded is not None and not np.isnan(encoded):
            out_image = out_image.astype(np.float64)
            out_image[out_image == encoded] = np.nan
        height, width = out_image.shape[-2:]
        cropped = xarray.DataArray(
            name=self._obj.name,
            data=out_image,
            coords=make_coords(
                src_data_array=self._obj,
                dst_affine=out_transform,
                dst_width=width,
                dst_height=height,
            ),
            dims=self._obj.dims,
            attrs=self._obj.attrs,
        )
        cropped.encoding = self._obj.encoding
        return cropped

    # Filling.

    def _interpolate_na(
        self, src_data: Any, *, method: Literal["linear", "nearest", "cubic"] = "nearest"
    ) -> np.ndarray:
        """Interpolates the nodata pixels of a 2-D array.

        Args:
            src_data: The array.
            method: The interpolation method of `scipy.interpolate.griddata`.

        Returns:
            numpy.ndarray: The filled array.

        Raises:
            ModuleNotFoundError: If SciPy is not installed.
        """
        try:
            from scipy.interpolate import (  # type: ignore[import-untyped, import-not-found, unused-ignore]
                griddata,
            )
        except ModuleNotFoundError as err:
            raise ModuleNotFoundError("interpolate_na needs scipy: pip install scipy") from err
        flat = src_data.flatten()
        try:
            isnan = bool(np.isnan(self.nodata))
        except TypeError:
            isnan = False
        valid = ~np.isnan(flat) if isnan else flat != self.nodata
        if not valid.any():
            return np.asarray(src_data)
        x_coords, y_coords = np.meshgrid(
            self._obj.coords[self.x_dim].values, self._obj.coords[self.y_dim].values
        )
        out: np.ndarray = griddata(
            points=(x_coords.flatten()[valid], y_coords.flatten()[valid]),
            values=flat[valid],
            xi=(x_coords, y_coords),
            method=method,
            fill_value=self.nodata,
        )
        return out

    def interpolate_na(
        self, method: Literal["linear", "nearest", "cubic"] = "nearest"
    ) -> xarray.DataArray:
        """Fills nodata pixels by interpolating the others (needs SciPy).

        Args:
            method: `nearest`, `linear` or `cubic`.

        Returns:
            xarray.DataArray: The filled array.

        Raises:
            RioXarrayError: If the array has no nodata value.
        """
        if self.nodata is None:
            raise RioXarrayError(
                "nodata not found. Please set the nodata with 'rio.write_nodata()'."
                f"{get_data_var_message(self._obj)}"
            )
        extra_dim = self._check_dimensions()
        if extra_dim:
            interp: Any = np.array(
                [
                    self._interpolate_na(sub.squeeze(dim=extra_dim).values, method=method)
                    for _, sub in self._obj.groupby(extra_dim)
                ]
            )
        else:
            interp = self._interpolate_na(self._obj.values, method=method)
        out = xarray.DataArray(
            name=self._obj.name,
            data=interp,
            coords=self._obj.coords,
            dims=self._obj.dims,
            attrs=self._obj.attrs,
        )
        out.encoding = self._obj.encoding
        add_attrs_proj(new_data_array=out, src_data_array=self._obj)
        return out

    # Writing.

    def to_raster(
        self,
        raster_path: str | os.PathLike[str],
        *,
        driver: str | None = None,
        dtype: str | np.dtype[Any] | None = None,
        tags: dict[str, Any] | None = None,
        windowed: bool = False,
        recalc_transform: bool = True,
        lock: Any = None,
        compute: bool = True,
        **profile_kwargs: Any,
    ) -> Any:
        """Writes the array to a raster file with terrio's writers.

        GeoTIFFs (`driver="COG"` for cloud-optimized ones), Zarr (GeoZarr, with `.zarr`),
        netCDF, PNG and the other formats terrio writes. Dask arrays are computed chunk by chunk
        into the file's buffer, so they are never held twice in memory.

        Args:
            raster_path: The output path.
            driver: The driver (default: from the extension).
            dtype: The output data type (default: the encoded or the array's).
            tags: Metadata items to write (with the array's attributes).
            windowed: Write block by block (else the whole array at once).
            recalc_transform: Compute the transform from the coordinates.
            lock: Unused (writes are serialized by terrio).
            compute: For dask arrays, write now (else return a `dask.delayed` writing it).
            **profile_kwargs: Creation options (`compress`, `tiled`, `blockxsize`...).

        Returns:
            None, or a `dask.delayed.Delayed` when `compute=False` for a dask array.
        """
        if driver is None:
            if Path(raster_path).suffix.lower() in (".tif", ".tiff"):
                driver = "GTiff"
            else:
                try:
                    driver = driver_from_extension(os.fspath(raster_path))
                except ValueError:
                    driver = None  # terrio.rio.open reports the unknown extension
        out_profile = dict(self._source.profile) if self._source is not None else {}
        out_profile.update(profile_kwargs)
        out_profile = {
            k: v
            for k, v in out_profile.items()
            if k
            not in ("driver", "height", "width", "crs", "transform", "nodata", "count", "dtype")
        }
        if driver is not None and driver.upper() not in ("GTIFF", "COG"):
            # GeoTIFF layout options of the source are not options of other formats.
            for key in ("blockxsize", "blockysize", "tiled", "interleave", "photometric"):
                if key not in profile_kwargs:
                    out_profile.pop(key, None)
        rio_nodata = self.encoded_nodata if self.encoded_nodata is not None else self.nodata
        from terrio.xarray._writer import write_raster

        return write_raster(
            str(os.fspath(raster_path)),
            xarray_dataarray=self._obj,
            tags=tags,
            driver=driver,
            height=int(self.height),
            width=int(self.width),
            count=int(self.count),
            dtype=dtype,
            crs=self.crs,
            transform=self.transform(recalc=recalc_transform),
            gcps=self.get_gcps(),
            rpcs=self.get_rpcs(),
            nodata=rio_nodata,
            windowed=windowed,
            compute=compute,
            **out_profile,
        )

    @contextmanager
    def to_rasterio_dataset(self) -> Generator[Any, None, None]:
        """The array as an open `terrio.rio` dataset, in memory.

        Yields:
            terrio.rio.io.DatasetReader: The dataset.
        """
        with MemoryFile() as memfile:
            self.to_raster(memfile.name)
            with memfile.open() as src:
                yield src


class TerrioDataset(XRasterBase):
    """The `.terrio` accessor of Datasets: rioxarray's `.rio` API on terrio."""

    def __init__(self, xarray_obj: xarray.Dataset) -> None:
        """Creates the accessor of a Dataset.

        Args:
            xarray_obj: The Dataset.
        """
        super().__init__(xarray_obj)
        self._obj: xarray.Dataset = xarray_obj

    @property
    def vars(self) -> list[Hashable]:
        """The data variables.

        Returns:
            list: Their names.
        """
        return list(self._obj.data_vars)

    @property
    def crs(self) -> CRS | None:
        """The CRS of the Dataset (the same for all its variables).

        Returns:
            CRS | None: The CRS.

        Raises:
            RioXarrayError: If the variables' CRSs differ.
        """
        if self._crs is not None:
            return None if self._crs is False else self._crs
        crs = super().crs
        if crs is not None:
            return crs
        crs_list = [self._obj[v].terrio.crs for v in self.vars]
        crs_list = [c for c in crs_list if c is not None]
        if not crs_list:
            self._crs = False
            return None
        first: CRS = crs_list[0]
        if not all(c == first for c in crs_list):
            raise RioXarrayError(f"CRS in DataArrays differ in the Dataset: {crs_list}")
        self._crs = first
        return first

    def _per_var(self, func: Any) -> xarray.Dataset:
        """Applies a DataArray method to each spatial variable.

        Args:
            func: A function of a DataArray (with its spatial dimensions set).

        Returns:
            xarray.Dataset: The result.

        Raises:
            MissingSpatialDimensionError: If a variable of two or more dimensions has no
                spatial dimensions (unless `skip_missing_spatial_dims` is set).
        """
        out = xarray.Dataset(attrs=self._obj.attrs)
        for var in self.vars:
            try:
                x_dim, y_dim = get_spatial_dims(self._obj, var=var)
                da = self._obj[var]
                da.terrio.set_spatial_dims(x_dim=x_dim, y_dim=y_dim, inplace=True)
                out[var] = func(da)
            except MissingSpatialDimensionError:
                if len(self._obj[var].dims) >= 2 and not get_option(SKIP_MISSING_SPATIAL_DIMS):
                    raise
                out[var] = self._obj[var].copy()
        return out

    def reproject(
        self,
        dst_crs: Any,
        *,
        resolution: float | tuple[float, float] | None = None,
        shape: tuple[int, int] | None = None,
        transform: Affine | None = None,
        resampling: _Resampling = Resampling.nearest,
        nodata: float | None = None,
        **kwargs: Any,
    ) -> xarray.Dataset:
        """Reprojects every variable (see the DataArray method).

        Args:
            dst_crs: The target CRS.
            resolution: The target resolution.
            shape: The target `(height, width)`.
            transform: The target transform.
            resampling: The resampling method.
            nodata: The target nodata value.
            **kwargs: Other options.

        Returns:
            xarray.Dataset: The reprojected Dataset.
        """
        return self._per_var(
            lambda da: da.terrio.reproject(
                dst_crs,
                resolution=resolution,
                shape=shape,
                transform=transform,
                resampling=resampling,
                nodata=nodata,
                **kwargs,
            )
        )

    def reproject_match(
        self,
        match_data_array: XarrayObject,
        *,
        resampling: _Resampling = Resampling.nearest,
        **reproject_kwargs: Any,
    ) -> xarray.Dataset:
        """Reprojects every variable onto another's grid.

        Args:
            match_data_array: The array (or Dataset) whose grid to match.
            resampling: The resampling method.
            **reproject_kwargs: Other options.

        Returns:
            xarray.Dataset: The reprojected Dataset.
        """
        return self._per_var(
            lambda da: da.terrio.reproject_match(
                match_data_array, resampling=resampling, **reproject_kwargs
            )
        )

    def pad_box(
        self,
        minx: float,
        miny: float,
        maxx: float,
        maxy: float,
        *,
        constant_values: _ConstantValues = None,
    ) -> xarray.Dataset:
        """Pads every variable to cover bounds.

        Args:
            minx: The minimum x.
            miny: The minimum y.
            maxx: The maximum x.
            maxy: The maximum y.
            constant_values: The padding value.

        Returns:
            xarray.Dataset: The padded Dataset.
        """
        out = self._per_var(
            lambda da: da.terrio.pad_box(minx, miny, maxx, maxy, constant_values=constant_values)
        )
        return out.terrio.set_spatial_dims(x_dim=self.x_dim, y_dim=self.y_dim, inplace=True)  # type: ignore[no-any-return]

    def clip_box(
        self,
        minx: float,
        miny: float,
        maxx: float,
        maxy: float,
        *,
        auto_expand: bool | int = False,
        auto_expand_limit: int = 3,
        crs: Any = None,
        allow_one_dimensional_raster: bool = False,
    ) -> xarray.Dataset:
        """Clips every variable to bounds.

        Args:
            minx: The minimum x.
            miny: The minimum y.
            maxx: The maximum x.
            maxy: The maximum y.
            auto_expand: Expand the bounds when the result would be a single row or column.
            auto_expand_limit: The number of expansions.
            crs: The CRS of the bounds.
            allow_one_dimensional_raster: Allow a single row or column.

        Returns:
            xarray.Dataset: The clipped Dataset.
        """
        out = self._per_var(
            lambda da: da.terrio.clip_box(
                minx,
                miny,
                maxx,
                maxy,
                auto_expand=auto_expand,
                auto_expand_limit=auto_expand_limit,
                crs=crs,
                allow_one_dimensional_raster=allow_one_dimensional_raster,
            )
        )
        return out.terrio.set_spatial_dims(x_dim=self.x_dim, y_dim=self.y_dim, inplace=True)  # type: ignore[no-any-return]

    def clip(
        self,
        geometries: Iterable[Any],
        crs: Any = None,
        *,
        all_touched: bool = False,
        drop: bool = True,
        invert: bool = False,
        from_disk: bool = False,
    ) -> xarray.Dataset:
        """Clips every variable to geometries.

        Args:
            geometries: GeoJSON-like geometries.
            crs: Their CRS.
            all_touched: Keep every pixel a geometry touches.
            drop: Crop to the geometries' extent.
            invert: Keep the pixels outside the geometries instead.
            from_disk: Read only the geometries' window from the file.

        Returns:
            xarray.Dataset: The clipped Dataset.
        """
        geometries = list(geometries)
        out = self._per_var(
            lambda da: da.terrio.clip(
                geometries,
                crs=crs,
                all_touched=all_touched,
                drop=drop,
                invert=invert,
                from_disk=from_disk,
            )
        )
        return out.terrio.set_spatial_dims(x_dim=self.x_dim, y_dim=self.y_dim, inplace=True)  # type: ignore[no-any-return]

    def interpolate_na(
        self, method: Literal["linear", "nearest", "cubic"] = "nearest"
    ) -> xarray.Dataset:
        """Fills the nodata pixels of every variable by interpolation.

        Args:
            method: `nearest`, `linear` or `cubic`.

        Returns:
            xarray.Dataset: The filled Dataset.
        """
        out = self._per_var(lambda da: da.terrio.interpolate_na(method=method))
        return out.terrio.set_spatial_dims(x_dim=self.x_dim, y_dim=self.y_dim, inplace=True)  # type: ignore[no-any-return]

    def to_raster(
        self,
        raster_path: str | os.PathLike[str],
        *,
        driver: str | None = None,
        dtype: str | np.dtype[Any] | None = None,
        tags: dict[str, Any] | None = None,
        windowed: bool = False,
        recalc_transform: bool = True,
        lock: Any = None,
        compute: bool = True,
        **profile_kwargs: Any,
    ) -> Any:
        """Writes the Dataset's variables as the bands of a raster file.

        Args:
            raster_path: The output path.
            driver: The driver (default: from the extension).
            dtype: The output data type.
            tags: Metadata items to write.
            windowed: Write block by block.
            recalc_transform: Compute the transform from the coordinates.
            lock: Unused.
            compute: For dask arrays, write now (else return a `dask.delayed`).
            **profile_kwargs: Creation options.

        Returns:
            None, or a `dask.delayed.Delayed`.

        Raises:
            RioXarrayError: If the variables' nodata values differ.
        """
        variable_dim = f"band_{uuid4()}"
        data_array = self._obj.to_array(dim=variable_dim)
        attr_scales: list[Any] = []
        attr_offsets: list[Any] = []
        attr_nodatavals: list[Any] = []
        encoded_scales: list[Any] = []
        encoded_offsets: list[Any] = []
        encoded_nodatavals: list[Any] = []
        band_tags: list[dict[Hashable, Any]] = []
        long_name: list[Any] = []
        for data_var in data_array[variable_dim].values:
            var = self._obj[data_var]
            if "scale_factor" in var.encoding:
                encoded_scales.append(var.encoding["scale_factor"])
            else:
                attr_scales.append(var.attrs.get("scale_factor", 1.0))
            if "add_offset" in var.encoding:
                encoded_offsets.append(var.encoding["add_offset"])
            else:
                attr_offsets.append(var.attrs.get("add_offset", 0.0))
            long_name.append(var.attrs.get("long_name", data_var))
            if var.terrio.encoded_nodata is not None:
                encoded_nodatavals.append(var.terrio.encoded_nodata)
            else:
                attr_nodatavals.append(var.terrio.nodata)
            band_tags.append(var.attrs.copy())
        if encoded_scales:
            data_array.encoding["scales"] = encoded_scales
        else:
            data_array.attrs["scales"] = attr_scales
        if encoded_offsets:
            data_array.encoding["offsets"] = encoded_offsets
        else:
            data_array.attrs["offsets"] = attr_offsets
        data_array.attrs["band_tags"] = band_tags
        data_array.attrs["long_name"] = long_name
        use_encoded = bool(encoded_nodatavals)
        nodatavals = encoded_nodatavals if use_encoded else attr_nodatavals
        nodata = nodatavals[0]
        if (
            all(v == nodata for v in nodatavals)
            or np.isnan(np.array(nodatavals, dtype=float)).all()
        ):
            data_array.terrio.write_nodata(nodata, inplace=True, encoded=use_encoded)
        else:
            raise RioXarrayError(
                "All nodata values must be the same when exporting to raster. "
                f"Current values: {attr_nodatavals}"
            )
        if self.crs is not None:
            data_array.terrio.write_crs(self.crs, inplace=True)
        data_array.terrio.set_spatial_dims(x_dim=self.x_dim, y_dim=self.y_dim, inplace=True)
        return data_array.terrio.to_raster(
            raster_path=raster_path,
            driver=driver,
            dtype=dtype,
            tags=tags,
            windowed=windowed,
            recalc_transform=recalc_transform,
            lock=lock,
            compute=compute,
            **profile_kwargs,
        )


__all__ = ["TerrioArray", "TerrioDataset", "XRasterBase"]
