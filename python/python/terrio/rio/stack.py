"""Stacking the bands of rasters into one, with rasterio's `stack` and its algorithm.

Each source's bands (or the bands chosen with `indexes`) become successive bands of the output,
read over the output's extent and grid; the parts of the output a source does not cover hold the
nodata value.
"""

from __future__ import annotations

import cmath
import math
import numbers
import os
import warnings
from collections.abc import Iterable, Iterator, Sequence
from contextlib import ExitStack, contextmanager
from typing import Any

import numpy as np
from affine import Affine

from terrio.rio import windows
from terrio.rio.coords import disjoint_bounds
from terrio.rio.enums import Resampling
from terrio.rio.errors import RasterioError, StackError, WindowError
from terrio.rio.merge import _res_size
from terrio.rio.transform import compose


@contextmanager
def _borrowed(dataset: Any) -> Iterator[Any]:
    """Yields an already open dataset without closing it.

    Args:
        dataset: An open dataset.

    Yields:
        The dataset.
    """
    yield dataset


def _intersect_bounds(
    bounds1: Sequence[float], bounds2: Sequence[float], transform: Affine
) -> tuple[float, float, float, float]:
    """The intersection of two bounding boxes, as `gdal_merge.py` computes it.

    Args:
        bounds1: `(left, bottom, right, top)`.
        bounds2: `(left, bottom, right, top)`.
        transform: The transform of the grid the boxes are on (for its orientation).

    Returns:
        tuple: The intersection `(left, bottom, right, top)`.

    Raises:
        ValueError: If the boxes do not intersect.
    """
    int_w = max(bounds1[0], bounds2[0])
    int_e = min(bounds1[2], bounds2[2])
    if int_w >= int_e:
        raise ValueError
    if transform.e < 0:
        int_s = max(bounds1[1], bounds2[1])
        int_n = min(bounds1[3], bounds2[3])
        if int_s >= int_n:
            raise ValueError
    else:
        int_s = min(bounds1[1], bounds2[1])
        int_n = max(bounds1[3], bounds2[3])
        if int_n >= int_s:
            raise ValueError
    return int_w, int_s, int_e, int_n


def _win_align(window: windows.Window) -> windows.Window:
    """Rounds a window's offsets and size, as rasterio's merge and stack tools do.

    Args:
        window: The window.

    Returns:
        Window: The window with integer offsets and size.
    """
    return windows.Window(
        math.floor(window.col_off + 0.1),
        math.floor(window.row_off + 0.1),
        math.floor(window.width + 0.5),
        math.floor(window.height + 0.5),
    )


def stack(
    sources: Sequence[Any],
    bounds: Any = None,
    res: Any = None,
    nodata: Any = None,
    dtype: Any = None,
    indexes: Any = None,
    output_count: int | None = None,
    resampling: Resampling = Resampling.nearest,
    target_aligned_pixels: bool = False,
    mem_limit: int = 64,
    use_highest_res: bool = False,
    masked: bool = False,
    dst_path: Any = None,
    dst_kwds: dict[str, Any] | None = None,
) -> Any:
    """Stacks the bands of datasets into one array (or file), as `rio stack` does.

    Args:
        sources: Datasets opened for reading, or paths, in the same CRS.
        bounds: The output bounds `(left, bottom, right, top)`; by default, the union.
        res: The output resolution (a number or a pair); by default, the first dataset's.
        nodata: The output nodata value; by default, the first dataset's (else 0).
        dtype: The output data type; by default, the first dataset's.
        indexes: One entry per source: None (all bands), a band, or a list of bands.
        output_count: Unused: the count is the sum of the bands read.
        resampling: The resampling of the reads.
        target_aligned_pixels: Align the bounds to the resolution (GDAL's `-tap`).
        mem_limit: Write a file in chunks of about this many megabytes.
        use_highest_res: Use the finest resolution of the sources.
        masked: Return a masked array (nodata masked).
        dst_path: A path (or a dataset open for writing) to write to instead of returning the
            array.
        dst_kwds: Creation options and profile items for the output file.

    Returns:
        tuple | None: `(array, transform)` when `dst_path` is None, else None.

    Raises:
        StackError: If a source is rotated, flipped or upside down.
        RasterioError: If the sources' CRSs differ.
    """
    from terrio.rio import open as rio_open
    from terrio.rio.io import DatasetWriter

    opener: Any = rio_open if isinstance(sources[0], (str, os.PathLike)) else _borrowed
    dst = None
    with ExitStack() as exit_stack:
        with opener(sources[0]) as first:
            first_profile = first.profile
            first_crs = first.crs
            best_res = first.res
            first_nodataval = first.nodatavals[0]
            nodataval = first_nodataval
            dt = first.dtypes[0]
            if indexes is None:
                indexes = [None for _ in sources]
            try:
                first_colormap = first.colormap(1)
            except ValueError:
                first_colormap = None

        xs: list[float] = []
        ys: list[float] = []
        output_count = 0
        for dataset, src_indexes in zip(sources, indexes, strict=False):
            with opener(dataset) as src:
                src_transform = src.transform
                if src_indexes is None:
                    output_count += src.count
                elif isinstance(src_indexes, int):
                    output_count += 1
                else:
                    output_count += len(src_indexes)
                if use_highest_res:
                    best_res = min(best_res, src.res, key=_res_size)
                if not src_transform.is_rectilinear:
                    raise StackError("Rotated, non-rectilinear rasters cannot be stacked.")
                if src_transform.a < 0:
                    raise StackError(
                        'Rasters with negative pixel width ("flipped" rasters) cannot be stacked.'
                    )
                if src_transform.e > 0:
                    raise StackError(
                        'Rasters with negative pixel height ("upside down" rasters) cannot be '
                        "stacked."
                    )
                left, bottom, right, top = src.bounds
            xs.extend([left, right])
            ys.extend([bottom, top])

        if bounds:
            dst_w, dst_s, dst_e, dst_n = bounds
        else:
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
        output_width = int(round((dst_e - dst_w) / res[0]))
        output_height = int(round((dst_n - dst_s) / res[1]))
        output_transform = compose(Affine.translation(dst_w, dst_n), Affine.scale(res[0], -res[1]))

        if dtype is not None:
            dt = dtype
        if nodata is not None:
            nodataval = nodata
        inrange = False
        if nodataval is not None:
            if np.issubdtype(dt, np.integer):
                info: Any = np.iinfo(dt)
                inrange = info.min <= nodataval <= info.max
            elif cmath.isfinite(nodataval):
                info = np.finfo(dt)
                inrange = bool(info.min <= nodataval <= info.max)
                inrange = inrange and np.can_cast(np.min_scalar_type(nodataval), dt)
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
                nodataval = first_nodataval
        else:
            nodataval = 0

        dout_window = windows.Window(0, 0, output_width, output_height)
        chunks: Iterable[windows.Window]
        if dst_path is not None:
            if isinstance(dst_path, DatasetWriter):
                dst = dst_path
            else:
                out_profile = dict(first_profile)
                out_profile.update(**(dst_kwds or {}))
                out_profile["transform"] = output_transform
                out_profile["height"] = output_height
                out_profile["width"] = output_width
                out_profile["count"] = output_count
                out_profile["dtype"] = dt
                if nodata is not None:
                    out_profile["nodata"] = nodata
                dst = rio_open(dst_path, "w", **out_profile)
                exit_stack.enter_context(dst)
            max_pixels = mem_limit * 1.0e6 / (np.dtype(dt).itemsize * output_count)
            if output_width * output_height < max_pixels:
                chunks = [dout_window]
            else:
                n = math.floor(math.sqrt(max_pixels))
                chunks = windows.subdivide(dout_window, n, n)
        else:
            chunks = [dout_window]

        dest: Any = None
        for chunk in chunks:
            chunk_bounds = windows.bounds(chunk, output_transform)
            chunk_transform = windows.transform(chunk, output_transform)
            dest = np.zeros((output_count, int(chunk.height), int(chunk.width)), dtype=dt)
            if inrange:
                dest.fill(nodataval)
            dst_idx = 0
            for dataset, src_indexes in zip(sources, indexes, strict=False):
                with opener(dataset) as src:
                    # As in rasterio (1.5), a source that doesn't reach this chunk doesn't advance
                    # the output band offset: later sources move up into its bands. That is kept
                    # so that scripts give rasterio's results.
                    if disjoint_bounds(chunk_bounds, src.bounds):
                        continue
                    if first_crs != src.crs:
                        raise RasterioError(f"CRS mismatch with source: {dataset}")
                    try:
                        ibounds = _intersect_bounds(src.bounds, chunk_bounds, chunk_transform)
                        sw = windows.from_bounds(*ibounds, src.transform)
                        cw = windows.from_bounds(*ibounds, chunk_transform)
                    except (ValueError, WindowError):
                        continue
                    if src_indexes is None:
                        src_indexes = src.indexes
                    elif isinstance(src_indexes, int):
                        src_indexes = [src_indexes]
                    cw = _win_align(cw)
                    data = src.read(
                        out_shape=(len(src_indexes), cw.height, cw.width),
                        window=sw,
                        masked=True,
                        indexes=src_indexes,
                        resampling=resampling,
                    )
                rows, cols = cw.toslices()
                region = dest[dst_idx : dst_idx + len(src_indexes), rows, cols]
                dst_idx += len(src_indexes)
                np.copyto(region, data, casting="unsafe")
            if dst is not None:
                dw = _win_align(windows.from_bounds(*chunk_bounds, output_transform))
                dst.write(dest, window=dw)

        if dst is None:
            if masked:
                dest = np.ma.masked_equal(dest, nodataval, copy=False)
            return dest, output_transform
        if first_colormap:
            dst.write_colormap(1, first_colormap)
        dst.close()
        return None


__all__ = ["stack"]
