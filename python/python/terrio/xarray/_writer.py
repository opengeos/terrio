"""Writing DataArrays to raster files with terrio, as rioxarray's `to_raster` writes them.

Metadata handling follows rioxarray's `raster_writer` (Apache-2.0, Copyright (c) 2019-2023
rioxarray Contributors). terrio's writers hold the pixels in memory and write the file when it is
closed: dask arrays are computed chunk by chunk into that buffer.
"""

from __future__ import annotations

import threading
import warnings
from collections.abc import Hashable
from typing import Any

import numpy as np
import xarray
from xarray.conventions import encode_cf_variable

from terrio.rio import open as rio_open
from terrio.rio.io import driver_from_extension
from terrio.rio.windows import Window
from terrio.xarray._io import _get_unsigned_dtype
from terrio.xarray._spatial import FILL_VALUE_NAMES, UNWANTED_RIO_ATTRS
from terrio.xarray.exceptions import RioXarrayError


def is_dask_collection(x: Any) -> bool:
    """Whether an object is a dask collection.

    Args:
        x: The object.

    Returns:
        bool: True for dask arrays (False when dask is not installed).
    """
    try:
        from dask.base import is_dask_collection as is_collection
    except ImportError:  # pragma: no cover
        return False
    return bool(is_collection(x))


def _write_tags(*, raster_handle: Any, tags: dict[Hashable, Any]) -> None:
    """Writes attributes as metadata items, skipping the ones rioxarray skips.

    Args:
        raster_handle: The open writer.
        tags: The attributes (and tags).
    """
    skip_tags: tuple[str, ...] = (
        UNWANTED_RIO_ATTRS
        + FILL_VALUE_NAMES
        + ("crs", "transform", "scales", "scale_factor", "add_offset", "offsets", "grid_mapping")
    )
    if not isinstance(tags.get("long_name"), str):
        skip_tags += ("long_name",)
    band_tags = tags.pop("band_tags", [])
    items = {str(k): v for k, v in tags.items() if k not in skip_tags}
    raster_handle.update_tags(**items)
    if isinstance(band_tags, list):
        for i, band_tag in enumerate(band_tags):
            raster_handle.update_tags(i + 1, **{str(k): v for k, v in band_tag.items()})


def _write_band_description(*, raster_handle: Any, xarray_dataset: xarray.DataArray) -> None:
    """Writes `long_name` (or the array's name) as the band descriptions.

    Args:
        raster_handle: The open writer.
        xarray_dataset: The array.

    Raises:
        RioXarrayError: If `long_name` lists a different number of names than bands.
    """
    long_name = xarray_dataset.attrs.get("long_name")
    if isinstance(long_name, (tuple, list)):
        if len(long_name) != raster_handle.count:
            raise RioXarrayError(
                "Number of names in the 'long_name' attribute does not equal the number of bands."
            )
        for i, description in enumerate(long_name):
            raster_handle.set_band_description(i + 1, description)
    else:
        description = long_name or xarray_dataset.name
        if description:
            for i in range(raster_handle.count):
                raster_handle.set_band_description(i + 1, str(description))


def _write_metadata(
    *, raster_handle: Any, xarray_dataset: xarray.DataArray, tags: dict[str, Any] | None
) -> None:
    """Writes scales, offsets, metadata items and band descriptions.

    Args:
        raster_handle: The open writer.
        xarray_dataset: The array.
        tags: Extra metadata items.
    """
    all_tags: dict[Hashable, Any] = dict(xarray_dataset.attrs)
    if tags is not None:
        all_tags.update(tags)
    scales = all_tags.get("scales", xarray_dataset.encoding.get("scales"))
    if scales is None:
        scale_factor = all_tags.get("scale_factor", xarray_dataset.encoding.get("scale_factor"))
        if scale_factor is not None:
            scales = (scale_factor,) * raster_handle.count
    if scales is not None:
        raster_handle.scales = [float(s) for s in scales]
    offsets = all_tags.get("offsets", xarray_dataset.encoding.get("offsets"))
    if offsets is None:
        add_offset = all_tags.get("add_offset", xarray_dataset.encoding.get("add_offset"))
        if add_offset is not None:
            offsets = (add_offset,) * raster_handle.count
    if offsets is not None:
        raster_handle.offsets = [float(o) for o in offsets]
    lossy = any(float(v) != 1.0 for v in scales or ()) or any(float(v) for v in offsets or ())
    if lossy and str(raster_handle.driver).upper() != "GTIFF":
        warnings.warn(
            f"terrio's {raster_handle.driver} writer does not store band scales and offsets: "
            "the file holds the encoded values without them. Write a GeoTIFF, write decoded "
            "values (dtype='float32'), or use xarray's to_zarr / to_netcdf.",
            UserWarning,
            stacklevel=4,
        )
    _write_tags(raster_handle=raster_handle, tags=all_tags)
    _write_band_description(raster_handle=raster_handle, xarray_dataset=xarray_dataset)


def _ensure_nodata_dtype(original_nodata: Any, new_dtype: Any) -> Any:
    """A nodata value in a data type.

    Args:
        original_nodata: The value.
        new_dtype: The data type.

    Returns:
        Any: The value.
    """
    from terrio.xarray._accessor import _ensure_nodata_dtype as ensure

    return ensure(original_nodata, new_dtype)


def _to_raster_dtypes(
    xarray_dataarray: xarray.DataArray, dtype: Any
) -> tuple[xarray.DataArray, np.dtype[Any], Any]:
    """The data type to write, from the argument, the encoding or the array.

    Args:
        xarray_dataarray: The array (its encoding may be updated).
        dtype: The requested data type, or None.

    Returns:
        tuple: The array, the NumPy data type and the raster data type.
    """
    if dtype is None:
        dtype = xarray_dataarray.encoding.get("dtype", str(xarray_dataarray.dtype))
    numpy_dtype = np.dtype(dtype)
    if "_Unsigned" in xarray_dataarray.encoding:
        unsigned = _get_unsigned_dtype(xarray_dataarray.encoding["_Unsigned"] == "true", dtype)
        if unsigned is not None:
            numpy_dtype = unsigned
            dtype = unsigned
            xarray_dataarray.encoding["rasterio_dtype"] = str(unsigned)
            xarray_dataarray.encoding["dtype"] = str(unsigned)
    return xarray_dataarray, numpy_dtype, str(np.dtype(dtype))


class _StoreTarget:
    """A `dask.array.store` target writing chunks into an open terrio writer."""

    def __init__(self, raster_handle: Any, ndim: int) -> None:
        """Creates the target.

        Args:
            raster_handle: The open writer.
            ndim: The array's number of dimensions (2 or 3).
        """
        self.rds = raster_handle
        self.ndim = ndim

    def __setitem__(self, key: tuple[slice, ...], item: np.ndarray) -> None:
        """Writes a chunk.

        Args:
            key: The chunk's slices.
            item: Its pixels.
        """
        if self.ndim == 3:
            bands, yy, xx = key
            start = bands.start or 0
            stop = bands.stop if bands.stop is not None else self.rds.count
            indexes: Any = list(range(start + 1, stop + 1, bands.step or 1))
        else:
            indexes = 1
            yy, xx = key
        y0 = yy.start or 0
        x0 = xx.start or 0
        h = item.shape[-2]
        w = item.shape[-1]
        self.rds.write(np.asarray(item), indexes, window=Window(x0, y0, w, h))


def _flip_index(start: int, stop: int, size: int, flipped: bool) -> slice:
    """The source rows (or columns) of an output range.

    Args:
        start: The first output row.
        stop: The output row after the last.
        size: The number of rows.
        flipped: Whether the source runs the other way.

    Returns:
        slice: The source slice, in output order.
    """
    if not flipped:
        return slice(start, stop)
    hi = size - 1 - start
    lo = size - stop
    return slice(hi, lo - 1 if lo > 0 else None, -1)


def _write(
    raster_path: str,
    da: xarray.DataArray,
    numpy_dtype: np.dtype[Any],
    tags: dict[str, Any] | None,
    windowed: bool,
    kwargs: dict[str, Any],
) -> None:
    """Writes the raster.

    Args:
        raster_path: The output path.
        da: The array.
        numpy_dtype: The data type of the pixels written.
        tags: Extra metadata items.
        windowed: Write block by block.
        kwargs: Options of `terrio.rio.open` in `w` mode.
    """
    transform = kwargs["transform"]
    flip_y = transform.e > 0
    flip_x = transform.a < 0
    if flip_y or flip_x:
        h, w = kwargs["height"], kwargs["width"]
        a, b, c, d, e, f = transform[:6]
        if flip_y:
            f, e = f + e * h, -e
        if flip_x:
            c, a = c + a * w, -a
        kwargs["transform"] = type(transform)(a, b, c, d, e, f)
    yd, xd = da.terrio.y_dim, da.terrio.x_dim
    height, width = kwargs["height"], kwargs["width"]
    with rio_open(raster_path, "w", **kwargs) as rds:
        _write_metadata(raster_handle=rds, xarray_dataset=da, tags=tags)
        if is_dask_collection(da.data):
            import dask.array

            data = encode_cf_variable(da.variable).data.astype(numpy_dtype)
            if flip_y:
                data = data[..., ::-1, :]
            if flip_x:
                data = data[..., :, ::-1]
            target: Any = _StoreTarget(rds, data.ndim)
            dask.array.store(data, target, lock=threading.Lock())
            return
        if windowed:
            blocks = [w for _, w in rds.block_windows(1)]
        else:
            blocks = [Window(0, 0, width, height)]
        for win in blocks:
            r0, c0 = int(win.row_off), int(win.col_off)
            r1, c1 = r0 + int(win.height), c0 + int(win.width)
            part = da.isel(
                {
                    yd: _flip_index(r0, r1, height, flip_y),
                    xd: _flip_index(c0, c1, width, flip_x),
                }
            )
            values = np.asarray(encode_cf_variable(part.variable).values).astype(numpy_dtype)
            if values.ndim == 2:
                rds.write(values, 1, window=win)
            else:
                rds.write(values, window=win)


def write_raster(
    raster_path: str,
    *,
    xarray_dataarray: xarray.DataArray,
    tags: dict[str, Any] | None,
    windowed: bool,
    compute: bool,
    **kwargs: Any,
) -> Any:
    """Writes a DataArray to a raster file.

    Args:
        raster_path: The output path.
        xarray_dataarray: The array, with its spatial dimensions set.
        tags: Extra metadata items.
        windowed: Write block by block.
        compute: For dask arrays, write now (else return a delayed).
        **kwargs: `terrio.rio.open` options: driver, height, width, count, dtype, crs, transform,
            nodata and creation options.

    Returns:
        None, or a `dask.delayed.Delayed` writing the file.
    """
    acc = xarray_dataarray.terrio
    x_dim, y_dim = acc.x_dim, acc.y_dim
    da = xarray_dataarray.copy()
    da.terrio.set_spatial_dims(x_dim=x_dim, y_dim=y_dim, inplace=True)
    da, numpy_dtype, kwargs["dtype"] = _to_raster_dtypes(da, kwargs.get("dtype"))
    if kwargs.get("nodata") is not None:
        kwargs["nodata"] = _ensure_nodata_dtype(kwargs["nodata"], numpy_dtype)
    if kwargs.get("crs") is None:
        kwargs.pop("crs", None)
    if not kwargs.get("driver"):
        kwargs["driver"] = driver_from_extension(raster_path)
    if not compute and is_dask_collection(da.data):
        from dask.delayed import delayed

        return delayed(_write)(raster_path, da, numpy_dtype, tags, windowed, kwargs)
    _write(raster_path, da, numpy_dtype, tags, windowed, kwargs)
    return None
