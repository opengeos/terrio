# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (adapted and modified):
#   rasterio 1.5.2:
#     rasterio/__init__.py

"""A rasterio-compatible API on top of terrio: `import terrio.rio as rasterio`.

Typical rasterio scripts switch with an import change::

    import terrio.rio as rasterio
    from terrio.rio.windows import Window

    with rasterio.open("dem.tif") as src:
        profile = src.profile
        dem = src.read(1, window=Window(0, 0, 512, 512), masked=True)

    with rasterio.open("out.tif", "w", **profile) as dst:
        dst.write(dem.filled(), 1)

Datasets, windows, transforms, CRSs, features, warping, merging, masking, warped VRTs and memory
files follow rasterio's API and semantics (band indexes from 1, `Affine` transforms, masked arrays
from nodata, GeoTIFF profiles). GeoTIFFs, local or remote, are read lazily by window; other
formats terrio reads are read whole. Writers hold their pixels in memory and write the file when
closed.

The API and several algorithms follow rasterio (Copyright (c) 2013-2021, Mapbox; BSD-3-Clause).
"""

from __future__ import annotations

import os
from typing import Any, NamedTuple

from affine import Affine

from terrio import __version__ as _terrio_version
from terrio.rio import (
    coords,
    crs,
    dtypes,
    enums,
    env,
    errors,
    features,
    io,
    mask,
    merge,
    profiles,
    sample,
    session,
    transform,
    vrt,
    warp,
    windows,
)
from terrio.rio.crs import CRS
from terrio.rio.dtypes import (
    bool_,
    check_dtype,
    complex64,
    complex128,
    complex_,
    complex_int16,
    float16,
    float32,
    float64,
    int8,
    int16,
    int32,
    int64,
    sbyte,
    ubyte,
    uint8,
    uint16,
    uint32,
    uint64,
)
from terrio.rio.env import Env
from terrio.rio.errors import DriverCapabilityError, RasterioIOError
from terrio.rio.io import (
    DatasetReader,
    DatasetWriter,
    MemoryFile,
    driver_from_extension,
    get_writer_for_driver,
)
from terrio.rio.profiles import default_gtiff_profile
from terrio.rio.transform import guard_transform

__version__ = _terrio_version


class Band(NamedTuple):
    """A band (or bands) of a dataset, for `warp.reproject`."""

    ds: Any
    bidx: Any
    dtype: str
    shape: tuple[int, int]


def band(ds: Any, bidx: Any) -> Band:
    """A band of a dataset, for `warp.reproject`.

    Args:
        ds: The dataset.
        bidx: The band (or a sequence of bands).

    Returns:
        Band: The band.
    """
    return Band(ds, bidx, set(ds.dtypes).pop(), ds.shape)


def pad(
    array: Any, transform: Affine, pad_width: int, mode: Any = None, **kwargs: Any
) -> tuple[Any, Affine]:
    """Pads an array and adjusts its transform.

    Args:
        array: The array.
        transform: Its transform.
        pad_width: Pixels to add on every side.
        mode: The `numpy.pad` mode.
        **kwargs: Keyword arguments of `numpy.pad`.

    Returns:
        tuple: The padded array and its transform.
    """
    import numpy as np

    transform = guard_transform(transform)
    padded = np.pad(array, pad_width, mode, **kwargs)
    t = list(transform)
    t[2] -= pad_width * t[0]
    t[5] -= pad_width * t[4]
    return padded, Affine(*t[:6])


def open(
    fp: Any,
    mode: str = "r",
    driver: str | None = None,
    width: int | None = None,
    height: int | None = None,
    count: int | None = None,
    crs: Any = None,
    transform: Any = None,
    dtype: Any = None,
    nodata: Any = None,
    sharing: bool = False,
    thread_safe: bool = False,
    opener: Any = None,
    **kwargs: Any,
) -> Any:
    """Opens a dataset for reading (`r`), writing (`w`, `w+`) or updating (`r+`).

    Args:
        fp: A path, URL (`https://`, `s3://`, `gs://`, `az://`), GDAL `/vsicurl/` path,
            `pathlib.Path`, `MemoryFile`, or file object.
        mode: `r`, `r+`, `w` or `w+`.
        driver: The format's short name, for writing (by default, from the extension).
        width: The number of columns, for writing.
        height: The number of rows, for writing.
        count: The number of bands, for writing.
        crs: The CRS, for writing.
        transform: The affine transform, for writing.
        dtype: The data type, for writing.
        nodata: The nodata value, for writing.
        sharing: Unused.
        thread_safe: Unused.
        opener: Unsupported: terrio reads paths and URLs itself.
        **kwargs: Creation options for writing (`tiled`, `blockxsize`, `compress`...).

    Returns:
        DatasetReader | DatasetWriter: The dataset, usable as a context manager.

    Raises:
        TypeError: If an argument has the wrong type.
        ValueError: If the mode is invalid.
        RasterioIOError: If the dataset cannot be opened.
    """
    if not isinstance(fp, str):
        if not (
            hasattr(fp, "open")
            or hasattr(fp, "read")
            or hasattr(fp, "write")
            or isinstance(fp, (os.PathLike, MemoryFile))
        ) or isinstance(fp, io.DatasetBase):
            raise TypeError(f"invalid path or file: {fp!r}")
    if not isinstance(mode, str):
        raise TypeError(f"invalid mode: {mode!r}")
    if not mode or mode[0] not in ("r", "w"):
        raise ValueError(f"invalid mode: {mode!r}")
    if driver and not isinstance(driver, str):
        raise TypeError(f"invalid driver: {driver!r}")
    if dtype and not check_dtype(dtype):
        raise TypeError(f"invalid dtype: {dtype!r}")
    if opener is not None:
        raise RasterioIOError("terrio.rio does not support custom openers")
    if nodata is not None:
        nodata = float(nodata)
    if transform:
        transform = guard_transform(transform)
    write_args = dict(
        driver=driver,
        width=width,
        height=height,
        count=count,
        crs=crs,
        transform=transform,
        dtype=dtype,
        nodata=nodata,
    )
    if isinstance(fp, MemoryFile):
        if mode.startswith("r"):
            return fp.open(driver=driver, **kwargs)
        return fp.open(**write_args, **kwargs)
    if mode == "r" and hasattr(fp, "read"):
        memfile = MemoryFile(fp.read())
        if hasattr(fp, "seek"):
            fp.seek(0)
        ds = memfile.open(driver=driver, **kwargs)
        ds._env.enter_context(memfile)
        return ds
    if mode in ("w", "w+") and hasattr(fp, "write"):
        memfile = MemoryFile(ext=os.path.splitext(getattr(fp, "name", "") or ".tif")[1] or ".tif")
        ds = memfile.open(**write_args, **kwargs)

        def flush() -> None:
            memfile.seek(0)
            fp.write(memfile.read())

        ds._env.callback(memfile.close)
        ds._env.callback(flush)
        return ds
    path = os.fspath(fp) if not isinstance(fp, str) else fp
    if mode == "r":
        return DatasetReader(path, driver=driver, **kwargs)
    if mode == "r+":
        return DatasetWriter(path, "r+", driver=driver, **kwargs)
    if mode in ("w", "w+"):
        if not driver:
            driver = driver_from_extension(path)
        get_writer_for_driver(driver)
        write_args["driver"] = driver
        return DatasetWriter(path, mode, **write_args, **kwargs)
    raise DriverCapabilityError(f"mode must be one of 'r', 'r+', or 'w', not {mode}")


__all__ = [
    "CRS",
    "Affine",
    "Band",
    "Env",
    "MemoryFile",
    "band",
    "bool_",
    "check_dtype",
    "complex128",
    "complex64",
    "complex_",
    "complex_int16",
    "coords",
    "crs",
    "default_gtiff_profile",
    "dtypes",
    "enums",
    "env",
    "errors",
    "features",
    "float16",
    "float32",
    "float64",
    "int16",
    "int32",
    "int64",
    "int8",
    "io",
    "mask",
    "merge",
    "open",
    "pad",
    "profiles",
    "sample",
    "sbyte",
    "session",
    "transform",
    "ubyte",
    "uint16",
    "uint32",
    "uint64",
    "uint8",
    "vrt",
    "warp",
    "windows",
]
