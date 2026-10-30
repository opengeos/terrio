# SPDX-FileCopyrightText: Copyright (c) 1998, Frank Warmerdam
# SPDX-FileCopyrightText: Copyright (c) 2007-2014, Even Rouault <even dot rouault at spatialys.com>
# SPDX-FileCopyrightText: Terrio contributors
# SPDX-License-Identifier: MIT
#
# Derived from (translated to Python and modified):
#   GDAL v3.12.2:
#     gcore/rasterio.cpp

"""Reading pixels as GDAL's `RasterIO` does.

Windows may be fractional, buffers of another size (decimation through overviews, then
resampling), and pixels are converted to the requested data type as GDAL converts them.
"""

from __future__ import annotations

import math
from collections.abc import Callable, Sequence
from typing import Any

import numpy as np

from terrio import _terrio

# Resampling method names terrio's operations take, by `Resampling` value.
RESAMPLING_NAMES = {
    0: "nearest",
    1: "bilinear",
    2: "cubic",
    3: "cubicspline",
    4: "lanczos",
    5: "average",
    6: "mode",
    7: "gauss",
    8: "max",
    9: "min",
    10: "med",
    11: "q1",
    12: "q3",
    13: "sum",
    14: "rms",
}

# A function reading `bands` (1-based) of a window `(x, y, w, h)` of a level (None for full
# resolution, else an overview index): an array `(bands, h, w)`.
LevelReader = Callable[[Sequence[int], tuple[int, int, int, int], "int | None"], np.ndarray]


def gdal_cast(arr: np.ndarray, dtype: Any) -> np.ndarray:
    """Converts pixel values as GDAL does.

    Floats become integers rounded half away from zero and saturated (NaN becomes 0); integers
    are saturated.

    Args:
        arr: The values.
        dtype: The target data type.

    Returns:
        numpy.ndarray: The converted values.
    """
    dt = np.dtype(dtype)
    if arr.dtype == dt:
        return arr
    if dt.kind in "fc" or dt == np.bool_:
        return arr.astype(dt)
    info = np.iinfo(dt)
    if arr.dtype.kind == "f":
        x = arr
        half = x.dtype.type(0.5)
        with np.errstate(invalid="ignore"):
            y = np.where(x >= 0, x + half, x - half)
            y = np.clip(y, info.min, info.max)
            y = np.where(np.isnan(y), 0, y)
        out: np.ndarray = np.trunc(y).astype(dt)
        return out
    if arr.dtype.kind in "iu":
        src = np.iinfo(arr.dtype)
        if src.min < info.min or src.max > info.max:
            clipped: np.ndarray = np.clip(
                arr, max(info.min, src.min), min(info.max, src.max)
            ).astype(dt)
            return clipped
        return arr.astype(dt)
    return arr.astype(dt)


def best_overview(
    overviews: Sequence[tuple[int, int]],
    width: int,
    height: int,
    x_size: int,
    y_size: int,
    buf_w: int,
    buf_h: int,
    nearest: bool,
) -> int | None:
    """The overview GDAL reads a decimated request from (`GDALBandGetBestOverviewLevel2`).

    Args:
        overviews: Overview sizes `(width, height)`, largest first.
        width: The full-resolution width.
        height: The full-resolution height.
        x_size: The request's width in full-resolution pixels.
        y_size: The request's height.
        buf_w: The output width.
        buf_h: The output height.
        nearest: Whether the resampling is nearest neighbour.

    Returns:
        int | None: The overview index, or None for the full resolution.
    """
    xr, yr = x_size / buf_w, y_size / buf_h
    desired = xr if (xr < yr or buf_h == 1) else yr
    threshold = 1.2 if nearest else 1.0
    eps = 0.1
    best, best_ratio = None, 0.0
    for i, (ow, oh) in enumerate(overviews):
        if ow > width or oh > height:
            continue
        ratio = min(width / ow, height / oh)
        if ratio >= desired * threshold + eps or ratio <= best_ratio:
            continue
        best, best_ratio = i, ratio
        if abs(desired - ratio) < eps:
            break
    return best


def raster_io(
    read: LevelReader,
    bands: Sequence[int],
    size: tuple[int, int],
    overviews: Sequence[tuple[int, int]],
    window: tuple[float, float, float, float],
    buf: tuple[int, int],
    resampling: int,
    nodata: float | None,
) -> np.ndarray:
    """Reads a window of bands into a buffer, as GDAL's `RasterIO` with a floating-point window.

    Args:
        read: Reads integer windows of a level.
        bands: The bands, numbered from 1.
        size: The full-resolution `(width, height)`.
        overviews: The overview sizes, largest first.
        window: `(x_off, y_off, x_size, y_size)` in full-resolution pixels, inside the raster.
        buf: The output `(height, width)`.
        resampling: The `Resampling` value.
        nodata: The nodata value, which resampling other than nearest ignores.

    Returns:
        numpy.ndarray: The pixels, `(bands, buf_h, buf_w)`, in the raster's data type.
    """
    width, height = size
    fx, fy, fxs, fys = window
    bh, bw = buf
    nx, ny = int(fx), int(fy)
    nxs, nys = int(max(1, fxs)), int(max(1, fys))
    integer = nx == fx and ny == fy and nxs == fxs and nys == fys
    if (bw, bh) == (nxs, nys) and integer:
        return read(bands, (nx, ny, nxs, nys), None)

    level: int | None = None
    lw, lh = width, height
    if (bw < nxs or bh < nys) and overviews:
        level = best_overview(overviews, width, height, nxs, nys, bw, bh, resampling == 0)
        if level is not None:
            lw, lh = overviews[level]
            xf, yf = width / lw, height / lh
            ox = min(lw - 1, int(nx / xf + 0.5))
            oy = min(lh - 1, int(ny / yf + 0.5))
            oxs = max(1, int(nxs / xf + 0.5))
            oys = max(1, int(nys / yf + 0.5))
            oxs = min(oxs, lw - ox)
            oys = min(oys, lh - oy)
            fx, fy, fxs, fys = fx / xf, fy / yf, fxs / xf, fys / yf
            nx, ny, nxs, nys = ox, oy, oxs, oys
            integer = nx == fx and ny == fy and nxs == fxs and nys == fys
            if (bw, bh) == (nxs, nys) and integer:
                return read(bands, (nx, ny, nxs, nys), level)

    if resampling == 0:
        return _nearest(read, bands, (lw, lh), level, (fx, fy, fxs, fys), (bh, bw))
    return _resampled(
        read, bands, (lw, lh), level, (fx, fy, fxs, fys), (bh, bw), resampling, nodata
    )


def _nearest(
    read: LevelReader,
    bands: Sequence[int],
    size: tuple[int, int],
    level: int | None,
    window: tuple[float, float, float, float],
    buf: tuple[int, int],
) -> np.ndarray:
    """Nearest-neighbour decimation or replication, with GDAL's pixel centre formula.

    Args:
        read: Reads integer windows of a level.
        bands: The bands.
        size: The level's `(width, height)`.
        level: The level.
        window: The floating-point window in the level's pixels.
        buf: The output `(height, width)`.

    Returns:
        numpy.ndarray: The pixels.
    """
    lw, lh = size
    fx, fy, fxs, fys = window
    bh, bw = buf
    eps = 1e-10
    xs = ((np.arange(bw) + 0.5) * (fxs / bw) + fx + eps).clip(0, lw - 1).astype(np.int64)
    ys = ((np.arange(bh) + 0.5) * (fys / bh) + fy + eps).clip(0, lh - 1).astype(np.int64)
    x0, x1 = int(xs.min()), int(xs.max()) + 1
    y0, y1 = int(ys.min()), int(ys.max()) + 1
    block = read(bands, (x0, y0, x1 - x0, y1 - y0), level)
    return np.ascontiguousarray(block[:, (ys - y0)[:, None], (xs - x0)[None, :]])


def _resampled(
    read: LevelReader,
    bands: Sequence[int],
    size: tuple[int, int],
    level: int | None,
    window: tuple[float, float, float, float],
    buf: tuple[int, int],
    resampling: int,
    nodata: float | None,
) -> np.ndarray:
    """Resamples with another method than nearest neighbour.

    The kernels are terrio's implementation of GDAL's (the arithmetic of
    `gdal_translate -outsize -r`).

    Args:
        read: Reads integer windows of a level.
        bands: The bands.
        size: The level's `(width, height)`.
        level: The level.
        window: The floating-point window in the level's pixels.
        buf: The output `(height, width)`.
        resampling: The `Resampling` value.
        nodata: The nodata value.

    Returns:
        numpy.ndarray: The pixels.
    """
    lw, lh = size
    fx, fy, fxs, fys = window
    bh, bw = buf
    # Source pixels around the window that the kernels reach.
    margin_x = int(math.ceil(4 * max(1.0, fxs / bw))) + 2
    margin_y = int(math.ceil(4 * max(1.0, fys / bh))) + 2
    x0 = max(0, int(math.floor(fx)) - margin_x)
    y0 = max(0, int(math.floor(fy)) - margin_y)
    x1 = min(lw, int(math.ceil(fx + fxs)) + margin_x)
    y1 = min(lh, int(math.ceil(fy + fys)) + margin_y)
    block = read(bands, (x0, y0, x1 - x0, y1 - y0), level)
    name = RESAMPLING_NAMES.get(resampling, "nearest")
    srcwin = (fx - x0, fy - y0, fxs, fys)
    src = _terrio.Raster.from_array(block, (0.0, 1.0, 0.0, 0.0, 0.0, -1.0), None, nodata)
    out = _terrio.translate(src, srcwin=srcwin, outsize=(bw, bh), resampling=name)
    res: np.ndarray = np.asarray(out.read()).astype(block.dtype, copy=False)
    return res
