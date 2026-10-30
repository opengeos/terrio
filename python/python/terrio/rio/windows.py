# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (adapted and modified):
#   rasterio 1.5.2:
#     rasterio/windows.py

"""Windows: rectangles of pixels, as rasterio defines them.

A window has a column and row offset and a width and height, all of which may be fractional.
"""

from __future__ import annotations

import functools
import math
import warnings
from collections import OrderedDict
from collections.abc import Callable, Iterable
from dataclasses import dataclass
from typing import Any

import numpy as np
from affine import Affine

from terrio.rio.errors import RasterioDeprecationWarning, WindowError
from terrio.rio.transform import apply, compose, guard_transform, rowcol


@dataclass(frozen=True)
class Window:
    """A window: `width` x `height` pixels from column `col_off`, row `row_off`."""

    col_off: float
    row_off: float
    width: float
    height: float

    def __post_init__(self) -> None:
        """Checks that the size is not negative.

        Raises:
            ValueError: If the width or height is negative.
        """
        for v in (self.width, self.height):
            if v and v < 0:
                raise ValueError("Number of columns or rows must be non-negative")

    def __repr__(self) -> str:
        """The representation.

        Returns:
            str: `Window(col_off=..., row_off=..., width=..., height=...)`.
        """
        return (
            f"Window(col_off={self.col_off!r}, row_off={self.row_off!r}, width={self.width!r}, "
            f"height={self.height!r})"
        )

    def flatten(self) -> tuple[float, float, float, float]:
        """The window as a tuple.

        Returns:
            tuple: `(col_off, row_off, width, height)`.
        """
        return (self.col_off, self.row_off, self.width, self.height)

    def todict(self) -> OrderedDict[str, float]:
        """The window as a dictionary.

        Returns:
            OrderedDict: `col_off`, `row_off`, `width` and `height`.
        """
        return OrderedDict(
            col_off=self.col_off, row_off=self.row_off, width=self.width, height=self.height
        )

    def toranges(self) -> tuple[tuple[float, float], tuple[float, float]]:
        """The window as row and column ranges.

        Returns:
            tuple: `((row_start, row_stop), (col_start, col_stop))`.
        """
        return (
            (self.row_off, self.row_off + self.height),
            (self.col_off, self.col_off + self.width),
        )

    def toslices(self) -> tuple[slice, slice]:
        """The window as array slices, clamped at 0 and rounded outwards.

        Returns:
            tuple: `(row_slice, col_slice)`.
        """
        (r0, r1), (c0, c1) = self.toranges()
        r0, r1, c0, c1 = (max(v, 0) for v in (r0, r1, c0, c1))
        return (
            slice(int(math.floor(r0)), int(math.ceil(r1))),
            slice(int(math.floor(c0)), int(math.ceil(c1))),
        )

    @classmethod
    def from_slices(
        cls,
        rows: Any,
        cols: Any,
        height: float = -1,
        width: float = -1,
        boundless: bool = False,
    ) -> Window:
        """A window from row and column slices or `(start, stop)` pairs.

        Args:
            rows: The rows: a slice or a `(start, stop)` pair.
            cols: The columns.
            height: The raster height, needed for open-ended or negative indexes.
            width: The raster width, needed for open-ended or negative indexes.
            boundless: Keep negative indexes as offsets outside the raster.

        Returns:
            Window: The window.

        Raises:
            WindowError: If the slices are not valid.
        """
        rows = _to_slice(rows, "rows")
        cols = _to_slice(cols, "cols")
        if rows.stop is None and height < 0:
            raise WindowError("height is required if row stop index is None")
        if cols.stop is None and width < 0:
            raise WindowError("width is required if col stop index is None")
        row_off = 0.0 if rows.start is None else rows.start
        row_stop = height if rows.stop is None else rows.stop
        col_off = 0.0 if cols.start is None else cols.start
        col_stop = width if cols.stop is None else cols.stop
        if not boundless:
            if row_off < 0 or row_stop < 0:
                if height < 0:
                    raise WindowError("height is required when providing negative indexes")
                if row_off < 0:
                    row_off += height
                if row_stop < 0:
                    row_stop += height
            if col_off < 0 or col_stop < 0:
                if width < 0:
                    raise WindowError("width is required when providing negative indexes")
                if col_off < 0:
                    col_off += width
                if col_stop < 0:
                    col_stop += width
        return cls(
            col_off=col_off,
            row_off=row_off,
            width=max(col_stop - col_off, 0.0),
            height=max(row_stop - row_off, 0.0),
        )

    def round_lengths(self, **kwds: Any) -> Window:
        """The window with its width and height rounded to the nearest integer.

        Args:
            **kwds: Unused, for compatibility.

        Returns:
            Window: The window.
        """
        return Window(
            self.col_off,
            self.row_off,
            math.floor(self.width + 0.5),
            math.floor(self.height + 0.5),
        )

    def round_shape(self, **kwds: Any) -> Window:
        """Deprecated alias of `round_lengths`.

        Args:
            **kwds: Unused.

        Returns:
            Window: The window.
        """
        warnings.warn(
            "round_shape is deprecated and will be removed in Rasterio 2.0.0.",
            RasterioDeprecationWarning,
            stacklevel=2,
        )
        return self.round_lengths(**kwds)

    def round_offsets(self, **kwds: Any) -> Window:
        """The window with its offsets rounded down (after adding 0.1).

        Args:
            **kwds: Unused, for compatibility.

        Returns:
            Window: The window.
        """
        return Window(
            math.floor(self.col_off + 0.1),
            math.floor(self.row_off + 0.1),
            self.width,
            self.height,
        )

    def round(self, ndigits: int | None = None) -> Window:
        """The window with every value rounded.

        Args:
            ndigits: Digits to keep, as for `round()`.

        Returns:
            Window: The window.
        """
        return Window(
            round(self.col_off, ndigits),
            round(self.row_off, ndigits),
            round(self.width, ndigits),
            round(self.height, ndigits),
        )

    def crop(self, height: float, width: float) -> Window:
        """The window cropped to a raster of `height` x `width`.

        Args:
            height: The raster height.
            width: The raster width.

        Returns:
            Window: The cropped window.
        """
        return crop(self, height, width)

    def intersection(self, other: Window) -> Window:
        """The intersection with another window.

        Args:
            other: The other window.

        Returns:
            Window: The intersection.
        """
        out: Window = intersection([self, other])
        return out


def _to_slice(s: Any, name: str) -> slice:
    """Converts a `(start, stop)` pair to a slice.

    Args:
        s: A slice, tuple or list.
        name: `rows` or `cols`, for error messages.

    Returns:
        slice: The slice.

    Raises:
        WindowError: If `s` is not a slice or a pair.
    """
    if isinstance(s, (tuple, list)):
        if len(s) != 2:
            raise WindowError(f"{name} must have a start and stop index")
        return slice(*s)
    if not isinstance(s, slice):
        raise WindowError(f"{name} must be a slice, tuple, or list")
    return s


def iter_args(function: Callable[..., Any]) -> Callable[..., Any]:
    """Lets a function of windows take them as arguments or as one iterable.

    Args:
        function: The function.

    Returns:
        callable: The wrapped function.
    """

    @functools.wraps(function)
    def wrapper(*args: Any, **kwargs: Any) -> Any:
        if len(args) == 1 and isinstance(args[0], Iterable):
            return function(*args[0])
        return function(*args)

    return wrapper


def toranges(window: Any) -> Any:
    """A window's row and column ranges.

    Args:
        window: A `Window` or ranges.

    Returns:
        tuple: `((row_start, row_stop), (col_start, col_stop))`.
    """
    return window.toranges() if isinstance(window, Window) else window


def get_data_window(arr: Any, nodata: float | None = None) -> Window:
    """The window of an array's valid data.

    Args:
        arr: A 1-, 2- or 3-dimensional array (bands first).
        nodata: The nodata value; by default, the array's mask.

    Returns:
        Window: The smallest window holding every valid value.

    Raises:
        WindowError: If the array has another number of dimensions.
    """
    if not 0 < arr.ndim <= 3:
        raise WindowError("get_data_window input array must have 1, 2, or 3 dimensions")
    if nodata is not None:
        valid = ~np.isnan(arr) if np.isnan(nodata) else arr != nodata
    elif np.ma.is_masked(arr):
        valid = ~np.ma.getmask(arr)
    else:
        full = (
            ((0, arr.size), (0, 0)) if arr.ndim == 1 else ((0, arr.shape[-2]), (0, arr.shape[-1]))
        )
        return Window.from_slices(full[0], full[1])
    if arr.ndim == 3:
        valid = np.any(valid, axis=0)
    ranges: list[tuple[int, int]] = []
    for nz in valid.nonzero():
        ranges.append((int(nz.min()), int(nz.max()) + 1) if nz.size else (0, 0))
    if valid.ndim == 1:
        ranges.append((0, 0))
    return Window.from_slices(ranges[0], ranges[1])


def _union2(w1: Window, w2: Window) -> Window:
    """The union of two windows.

    Args:
        w1: A window.
        w2: Another window.

    Returns:
        Window: The smallest window containing both.
    """
    col_off = min(w1.col_off, w2.col_off)
    row_off = min(w1.row_off, w2.row_off)
    width = max(w1.col_off + w1.width, w2.col_off + w2.width) - col_off
    height = max(w1.row_off + w1.height, w2.row_off + w2.height) - row_off
    return Window(col_off, row_off, width, height)


def _intersection2(w1: Window, w2: Window) -> Window:
    """The intersection of two windows.

    Args:
        w1: A window.
        w2: Another window.

    Returns:
        Window: The intersection.

    Raises:
        WindowError: If the windows do not intersect.
    """
    col_off = max(w1.col_off, w2.col_off)
    row_off = max(w1.row_off, w2.row_off)
    width = min(w1.col_off + w1.width, w2.col_off + w2.width) - col_off
    height = min(w1.row_off + w1.height, w2.row_off + w2.height) - row_off
    if width > 0 and height > 0:
        return Window(col_off, row_off, width, height)
    raise WindowError(f"Intersection is empty {w1} {w2}")


@iter_args
def union(*windows: Window) -> Window:
    """The union of windows.

    Args:
        *windows: Windows, or one iterable of windows.

    Returns:
        Window: The smallest window containing all of them.
    """
    return functools.reduce(_union2, windows)


@iter_args
def intersection(*windows: Window) -> Window:
    """The intersection of windows.

    Args:
        *windows: Windows, or one iterable of windows.

    Returns:
        Window: The intersection.

    Raises:
        WindowError: If the windows do not intersect.
    """
    return functools.reduce(_intersection2, windows)


@iter_args
def intersect(*windows: Window) -> bool:
    """Whether windows intersect.

    Args:
        *windows: Windows, or one iterable of windows.

    Returns:
        bool: True when they share pixels.
    """
    try:
        intersection(*windows)
        return True
    except WindowError:
        return False


def from_bounds(
    left: float,
    bottom: float,
    right: float,
    top: float,
    transform: Affine | None = None,
    height: Any = None,
    width: Any = None,
    precision: Any = None,
) -> Window:
    """The (fractional) window of bounds in a grid.

    Args:
        left: The left bound.
        bottom: The bottom bound.
        right: The right bound.
        top: The top bound.
        transform: The grid's transform.
        height: Unused, deprecated.
        width: Unused, deprecated.
        precision: Unused, deprecated.

    Returns:
        Window: The window.

    Raises:
        WindowError: If the transform is missing or inconsistent with the bounds.
    """
    if height is not None or width is not None or precision is not None:
        warnings.warn(
            "The height, width, and precision parameters are unused, deprecated, and will be "
            "removed in 2.0.0.",
            RasterioDeprecationWarning,
            stacklevel=2,
        )
    if not isinstance(transform, Affine):
        raise WindowError("A transform object is required to calculate the window")
    if (right - left) / transform.a < 0:
        raise WindowError("Bounds and transform are inconsistent")
    if (bottom - top) / transform.e < 0:
        raise WindowError("Bounds and transform are inconsistent")
    rows, cols = rowcol(transform, [left, right, right, left], [top, top, bottom, bottom], op=float)
    row_start, row_stop = min(rows), max(rows)
    col_start, col_stop = min(cols), max(cols)
    return Window(
        col_off=col_start,
        row_off=row_start,
        width=max(col_stop - col_start, 0.0),
        height=max(row_stop - row_start, 0.0),
    )


def transform(window: Any, transform: Affine) -> Affine:
    """The transform of a window's grid.

    Args:
        window: A `Window` or ranges.
        transform: The dataset's transform.

    Returns:
        Affine: The transform whose origin is the window's upper left corner.
    """
    window = evaluate(window, height=0, width=0)
    x, y = apply(transform, window.col_off or 0.0, window.row_off or 0.0)
    return compose(Affine.translation(x - transform.c, y - transform.f), transform)


def bounds(
    window: Any, transform: Affine, height: float = 0, width: float = 0
) -> tuple[float, float, float, float]:
    """The bounds of a window.

    Args:
        window: A `Window` or ranges.
        transform: The dataset's transform.
        height: The raster height, for open-ended ranges.
        width: The raster width, for open-ended ranges.

    Returns:
        tuple: `(left, bottom, right, top)`.
    """
    window = evaluate(window, height=height, width=width)
    row_min = window.row_off
    row_max = row_min + window.height
    col_min = window.col_off
    col_max = col_min + window.width
    left, bottom = apply(transform, col_min, row_max)
    right, top = apply(transform, col_max, row_min)
    return left, bottom, right, top


def crop(window: Any, height: float, width: float) -> Window:
    """A window cropped to a raster.

    Args:
        window: A `Window` or ranges.
        height: The raster height.
        width: The raster width.

    Returns:
        Window: The cropped window.
    """
    window = evaluate(window, height=height, width=width)
    row_start = min(max(window.row_off, 0), height)
    col_start = min(max(window.col_off, 0), width)
    row_stop = max(0, min(window.row_off + window.height, height))
    col_stop = max(0, min(window.col_off + window.width, width))
    return Window(col_start, row_start, col_stop - col_start, row_stop - row_start)


def evaluate(window: Any, height: float, width: float, boundless: bool = False) -> Window:
    """A window from a `Window` or row and column ranges.

    Args:
        window: A `Window` or `(rows, cols)` ranges.
        height: The raster height.
        width: The raster width.
        boundless: Keep negative indexes as offsets.

    Returns:
        Window: The window.
    """
    if isinstance(window, Window):
        return window
    rows, cols = window
    return Window.from_slices(rows=rows, cols=cols, height=height, width=width, boundless=boundless)


def shape(window: Any, height: float = -1, width: float = -1) -> tuple[float, float]:
    """A window's shape.

    Args:
        window: A `Window` or ranges.
        height: The raster height.
        width: The raster width.

    Returns:
        tuple: `(height, width)`.
    """
    w = evaluate(window, height, width)
    return w.height, w.width


def window_index(window: Any, height: float = 0, width: float = 0) -> tuple[slice, slice]:
    """A window's array slices.

    Args:
        window: A `Window` or ranges.
        height: The raster height.
        width: The raster width.

    Returns:
        tuple: `(row_slice, col_slice)`.
    """
    return evaluate(window, height=height, width=width).toslices()


def round_window_to_full_blocks(
    window: Any, block_shapes: list[tuple[int, int]], height: float = 0, width: float = 0
) -> Window:
    """A window grown to whole blocks.

    Args:
        window: A `Window` or ranges.
        block_shapes: The bands' block shapes `(rows, cols)`.
        height: The raster height.
        width: The raster width.

    Returns:
        Window: The window aligned on blocks.

    Raises:
        WindowError: If the bands' blocks differ.
    """
    if len(set(block_shapes)) != 1:
        raise WindowError("All bands must have the same block/stripe structure")
    window = evaluate(window, height=height, width=width)
    bh, bw = block_shapes[0]
    (r0, r1), (c0, c1) = window.toranges()
    row_min = int(r0 // bh) * bh
    row_max = int(r1 // bh) * bh + (bh if r1 % bh != 0 else 0)
    col_min = int(c0 // bw) * bw
    col_max = int(c1 // bw) * bw + (bw if c1 % bw != 0 else 0)
    return Window(col_min, row_min, col_max - col_min, row_max - row_min)


def subdivide(window: Window, height: int, width: int) -> list[Window]:
    """Splits a window into windows of at most `height` x `width`.

    Args:
        window: The window.
        height: The largest sub-window height.
        width: The largest sub-window width.

    Returns:
        list: The windows, row by row.
    """
    out = []
    row_end = window.row_off + window.height
    col_end = window.col_off + window.width
    row_off = window.row_off
    while row_off < row_end:
        h = min(height, row_end - row_off)
        col_off = window.col_off
        while col_off < col_end:
            out.append(Window(col_off, row_off, min(width, col_end - col_off), h))
            col_off += width
        row_off += height
    return out


class WindowMethodsMixin:
    """`window`, `window_transform` and `window_bounds` methods for datasets."""

    transform: Affine

    def window(
        self, left: float, bottom: float, right: float, top: float, precision: Any = None
    ) -> Window:
        """The window of bounds in the dataset.

        Args:
            left: The left bound.
            bottom: The bottom bound.
            right: The right bound.
            top: The top bound.
            precision: Unused, deprecated.

        Returns:
            Window: The window.
        """
        if precision is not None:
            warnings.warn(
                "The precision parameter is unused, deprecated, and will be removed in 2.0.0.",
                RasterioDeprecationWarning,
                stacklevel=2,
            )
        return from_bounds(left, bottom, right, top, transform=guard_transform(self.transform))

    def window_transform(self, window: Any) -> Affine:
        """The transform of a window.

        Args:
            window: A `Window` or ranges.

        Returns:
            Affine: The window's transform.
        """
        return transform(window, guard_transform(self.transform))

    def window_bounds(self, window: Any) -> tuple[float, float, float, float]:
        """The bounds of a window.

        Args:
            window: A `Window` or ranges.

        Returns:
            tuple: `(left, bottom, right, top)`.
        """
        return bounds(window, guard_transform(self.transform))


__all__ = [
    "Window",
    "WindowMethodsMixin",
    "bounds",
    "crop",
    "evaluate",
    "from_bounds",
    "get_data_window",
    "intersect",
    "intersection",
    "round_window_to_full_blocks",
    "shape",
    "subdivide",
    "toranges",
    "transform",
    "union",
    "window_index",
]
