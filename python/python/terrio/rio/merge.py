# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (adapted and modified):
#   rasterio 1.5.2:
#     rasterio/merge.py

"""Merging rasters into a mosaic, with rasterio's `merge` and its algorithm."""

from __future__ import annotations

import cmath
import math
import numbers
import os
import warnings
from collections.abc import Callable, Iterator, Sequence
from contextlib import ExitStack, contextmanager
from typing import Any

import numpy as np
from affine import Affine

from terrio.rio import windows
from terrio.rio.enums import Resampling
from terrio.rio.errors import MergeError, RasterioDeprecationWarning, RasterioError, WindowError
from terrio.rio.transform import compose


def copy_first(
    merged_data: Any, new_data: Any, merged_mask: Any, new_mask: Any, **kwargs: Any
) -> None:
    """Fills invalid merged pixels with valid new ones.

    Args:
        merged_data: The merged pixels, updated.
        new_data: A source's pixels.
        merged_mask: Where merged pixels are invalid.
        new_mask: Where new pixels are invalid.
        **kwargs: Unused.
    """
    mask = np.logical_and(merged_mask, np.logical_not(new_mask))
    np.copyto(merged_data, new_data, where=mask, casting="unsafe")


def copy_last(
    merged_data: Any, new_data: Any, merged_mask: Any, new_mask: Any, **kwargs: Any
) -> None:
    """Copies every valid new pixel.

    Args:
        merged_data: The merged pixels, updated.
        new_data: A source's pixels.
        merged_mask: Where merged pixels are invalid.
        new_mask: Where new pixels are invalid.
        **kwargs: Unused.
    """
    np.copyto(merged_data, new_data, where=np.logical_not(new_mask), casting="unsafe")


def _combine(op: Callable[..., Any]) -> Callable[..., None]:
    """A merge method combining valid pixels with `op`.

    Args:
        op: A NumPy binary ufunc.

    Returns:
        callable: The merge method.
    """

    def method(
        merged_data: Any, new_data: Any, merged_mask: Any, new_mask: Any, **kwargs: Any
    ) -> None:
        both = np.logical_not(np.logical_or(merged_mask, new_mask))
        op(merged_data, new_data, out=merged_data, where=both, casting="unsafe")
        copy_first(merged_data, new_data, merged_mask, new_mask)

    return method


copy_min = _combine(np.minimum)
copy_max = _combine(np.maximum)
copy_sum = _combine(np.add)


def copy_count(
    merged_data: Any, new_data: Any, merged_mask: Any, new_mask: Any, **kwargs: Any
) -> None:
    """Counts valid pixels.

    Args:
        merged_data: The counts, updated.
        new_data: A source's pixels.
        merged_mask: Where merged pixels are invalid.
        new_mask: Where new pixels are invalid.
        **kwargs: Unused.
    """
    mask = np.logical_not(np.logical_or(merged_mask, new_mask))
    np.add(merged_data, mask, out=merged_data, where=mask, casting="unsafe")
    mask = np.logical_and(merged_mask, np.logical_not(new_mask))
    np.copyto(merged_data, mask, where=mask, casting="unsafe")


MERGE_METHODS: dict[str, Callable[..., None]] = {
    "first": copy_first,
    "last": copy_last,
    "min": copy_min,
    "max": copy_max,
    "sum": copy_sum,
    "count": copy_count,
}


def _res_size(res: Any) -> float:
    """The size of a resolution, to pick the finest.

    Args:
        res: A number or `(x, y)`.

    Returns:
        float: The size.
    """
    if isinstance(res, numbers.Number):
        return float(res)  # type: ignore[arg-type]
    return math.sqrt(res[0] ** 2 + res[1] ** 2)


@contextmanager
def _as_is(obj: Any) -> Iterator[Any]:
    """A context that yields an already open dataset.

    Args:
        obj: The dataset.

    Yields:
        The dataset.
    """
    yield obj


def merge(
    sources: Sequence[Any],
    bounds: Any = None,
    res: Any = None,
    nodata: Any = None,
    dtype: Any = None,
    precision: Any = None,
    indexes: Any = None,
    output_count: int | None = None,
    resampling: Resampling = Resampling.nearest,
    method: Any = "first",
    target_aligned_pixels: bool = False,
    mem_limit: int = 64,
    use_highest_res: bool = False,
    masked: bool = False,
    dst_path: Any = None,
    dst_kwds: dict[str, Any] | None = None,
) -> Any:
    """Merges datasets into one array (or file).

    Args:
        sources: Datasets or paths, in the same CRS.
        bounds: The output bounds `(left, bottom, right, top)`; by default, the union.
        res: The output resolution; by default, the first dataset's.
        nodata: The output nodata value; by default, the first dataset's.
        dtype: The output data type; by default, the first dataset's.
        precision: Unused, deprecated.
        indexes: The bands to merge.
        output_count: The number of output bands.
        resampling: The resampling method.
        method: `first`, `last`, `min`, `max`, `sum`, `count`, or a function.
        target_aligned_pixels: Align the bounds on multiples of the resolution.
        mem_limit: The memory limit in MB, when writing to `dst_path`.
        use_highest_res: Use the finest resolution of the sources.
        masked: Return a masked array.
        dst_path: Write the result to this path (or writer) instead of returning it.
        dst_kwds: Creation options for `dst_path`.

    Returns:
        tuple: `(array, transform)`, or None when written to `dst_path`.

    Raises:
        ValueError: If the method is unknown.
        MergeError: If a source is rotated or flipped.
        RasterioError: If the CRSs differ.
    """
    from terrio.rio import open as rio_open
    from terrio.rio.io import DatasetWriter

    if precision is not None:
        warnings.warn(
            "The precision parameter is unused, deprecated, and will be removed in 2.0.0.",
            RasterioDeprecationWarning,
            stacklevel=2,
        )
    if method in MERGE_METHODS:
        copyto = MERGE_METHODS[method]
    elif callable(method):
        copyto = method
    else:
        raise ValueError(
            f"Unknown method {method}, must be one of {list(MERGE_METHODS)} or callable"
        )
    opener: Callable[[Any], Any] = (
        rio_open if isinstance(sources[0], (str, os.PathLike)) else _as_is
    )
    dst = None
    with ExitStack() as stack:
        with opener(sources[0]) as first:
            first_profile = first.profile
            first_crs = first.crs
            best_res = first.res
            first_nodata = first.nodatavals[0]
            nodataval = first_nodata
            dt = first.dtypes[0]
            src_count = (
                first.count
                if indexes is None
                else (indexes if isinstance(indexes, int) else len(indexes))
            )
            try:
                first_colormap = first.colormap(1)
            except ValueError:
                first_colormap = None
        if not output_count:
            output_count = src_count
        if bounds:
            dst_w, dst_s, dst_e, dst_n = bounds
        else:
            xs: list[float] = []
            ys: list[float] = []
            for ds in sources:
                with opener(ds) as src:
                    t = src.transform
                    if use_highest_res:
                        best_res = min(best_res, src.res, key=_res_size)
                    if not t.is_rectilinear:
                        raise MergeError("Rotated, non-rectilinear rasters cannot be merged.")
                    if t.a < 0:
                        raise MergeError(
                            'Rasters with negative pixel width ("flipped" rasters) cannot be '
                            "merged."
                        )
                    if t.e > 0:
                        raise MergeError(
                            'Rasters with negative pixel height ("upside down" rasters) cannot be '
                            "merged."
                        )
                    left, bottom, right, top = src.bounds
                xs.extend([left, right])
                ys.extend([bottom, top])
            dst_w, dst_s, dst_e, dst_n = min(xs), min(ys), max(xs), max(ys)
        if not res:
            res = best_res
        elif isinstance(res, numbers.Number):
            res = (res, res)
        elif len(res) == 1:
            res = (res[0], res[0])
        if target_aligned_pixels:
            dst_w = math.floor(dst_w / res[0]) * res[0]
            dst_e = math.ceil(dst_e / res[0]) * res[0]
            dst_s = math.floor(dst_s / res[1]) * res[1]
            dst_n = math.ceil(dst_n / res[1]) * res[1]
        out_w = int(round((dst_e - dst_w) / res[0]))
        out_h = int(round((dst_n - dst_s) / res[1]))
        out_t = compose(Affine.translation(dst_w, dst_n), Affine.scale(res[0], -res[1]))
        if dtype is not None:
            dt = dtype
        if nodata is not None:
            nodataval = nodata
        inrange = False
        if nodataval is not None:
            if np.issubdtype(dt, np.integer):
                info = np.iinfo(dt)
                inrange = bool(info.min <= nodataval <= info.max)
            elif cmath.isfinite(nodataval):
                finfo = np.finfo(dt)
                inrange = bool(finfo.min <= nodataval <= finfo.max) and bool(
                    np.can_cast(np.min_scalar_type(nodataval), dt)
                )
            else:
                inrange = True
            if not inrange:
                warnings.warn(
                    f"Ignoring nodata value. The nodata value, {nodataval}, cannot safely be "
                    f"represented in the chosen data type, {dt}. Consider overriding it using the "
                    "--nodata option for better results. Falling back to first source's nodata "
                    "value.",
                    stacklevel=2,
                )
                nodataval = first_nodata
        else:
            nodataval = 0
        out_window = windows.Window(0, 0, out_w, out_h)
        chunks = [out_window]
        if dst_path is not None:
            if isinstance(dst_path, DatasetWriter):
                dst = dst_path
            else:
                profile = first_profile
                profile.update(**(dst_kwds or {}))
                profile.update(
                    transform=out_t, height=out_h, width=out_w, count=output_count, dtype=dt
                )
                if nodata is not None:
                    profile["nodata"] = nodata
                dst = rio_open(dst_path, "w", **profile)
                stack.enter_context(dst)
            max_pixels = mem_limit * 1.0e6 / (np.dtype(dt).itemsize * output_count)
            if out_w * out_h >= max_pixels:
                n = math.floor(math.sqrt(max_pixels))
                chunks = windows.subdivide(out_window, n, n)

        def intersect(b1: Any, b2: Any, t: Affine) -> tuple[float, float, float, float]:
            w, e = max(b1[0], b2[0]), min(b1[2], b2[2])
            if w >= e:
                raise ValueError
            if t.e < 0:
                s, n = max(b1[1], b2[1]), min(b1[3], b2[3])
                if s >= n:
                    raise ValueError
            else:
                s, n = min(b1[1], b2[1]), max(b1[3], b2[3])
                if n >= s:
                    raise ValueError
            return w, s, e, n

        def align(w: windows.Window) -> windows.Window:
            return windows.Window(
                math.floor(w.col_off + 0.1),
                math.floor(w.row_off + 0.1),
                math.floor(w.width + 0.5),
                math.floor(w.height + 0.5),
            )

        dest: Any = None
        for chunk in chunks:
            dest = np.zeros((output_count, int(chunk.height), int(chunk.width)), dtype=dt)
            if inrange:
                dest.fill(nodataval)
            chunk_bounds = windows.bounds(chunk, out_t)
            chunk_t = windows.transform(chunk, out_t)
            for idx, ds in enumerate(sources):
                with opener(ds) as src:
                    if first_crs != src.crs:
                        raise RasterioError(f"CRS mismatch with source: {ds}")
                    try:
                        ib = intersect(src.bounds, chunk_bounds, chunk_t)
                        sw = windows.from_bounds(*ib, src.transform)
                        cw = windows.from_bounds(*ib, chunk_t)
                    except (ValueError, WindowError):
                        continue
                    cw = align(cw)
                    rows, cols = cw.toslices()
                    region = dest[:, rows, cols]
                    if cmath.isnan(nodataval):
                        region_mask = np.isnan(region)
                    elif not np.issubdtype(region.dtype, np.integer):
                        region_mask = np.isclose(region, nodataval)
                    else:
                        region_mask = region == nodataval
                    data = src.read(
                        out_shape=(src_count, cw.height, cw.width),
                        indexes=indexes,
                        masked=True,
                        window=sw,
                        resampling=resampling,
                    )
                    copyto(
                        region,
                        data,
                        region_mask,
                        data.mask,
                        index=idx,
                        roff=cw.row_off,
                        coff=cw.col_off,
                    )
            if dst is not None:
                dw = align(windows.from_bounds(*chunk_bounds, out_t))
                dst.write(dest, window=dw)
        if dst is None:
            if masked:
                dest = np.ma.masked_equal(dest, nodataval, copy=False)
            return dest, out_t
        if first_colormap:
            dst.write_colormap(1, first_colormap)
        return None


__all__ = ["MERGE_METHODS", "merge"]
