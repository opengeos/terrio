# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (adapted and modified):
#   rasterio 1.5.2:
#     rasterio/plot.py

"""Plotting rasters with matplotlib (optional)."""

from __future__ import annotations

from typing import Any

import numpy as np

from terrio.rio.io import DatasetReader


def get_plt() -> Any:
    """Imports matplotlib's pyplot.

    Returns:
        module: `matplotlib.pyplot`.

    Raises:
        ImportError: If matplotlib is not installed.
    """
    try:
        import matplotlib.pyplot as plt  # type: ignore[import-not-found,unused-ignore]
    except (ImportError, RuntimeError) as err:
        raise ImportError(
            "Could not import matplotlib\nmatplotlib required for plotting functions"
        ) from err
    return plt


def plotting_extent(source: Any, transform: Any = None) -> tuple[float, float, float, float]:
    """The extent `(left, right, bottom, top)` matplotlib's `imshow` takes.

    Args:
        source: A dataset, or an array with `transform`.
        transform: The array's transform.

    Returns:
        tuple: The extent.
    """
    if hasattr(source, "bounds"):
        b = source.bounds
        return (b.left, b.right, b.bottom, b.top)
    if transform is None:
        raise ValueError("transform is required if source is an array")
    rows, cols = source.shape[-2:]
    left, top = transform.c, transform.f
    right = left + transform.a * cols
    bottom = top + transform.e * rows
    return (left, right, bottom, top)


def reshape_as_image(arr: np.ndarray) -> np.ndarray:
    """Reorders `(bands, rows, cols)` as `(rows, cols, bands)`.

    Args:
        arr: The array.

    Returns:
        numpy.ndarray: The image.
    """
    return np.ma.transpose(arr, [1, 2, 0])  # type: ignore[no-any-return]


def reshape_as_raster(arr: np.ndarray) -> np.ndarray:
    """Reorders `(rows, cols, bands)` as `(bands, rows, cols)`.

    Args:
        arr: The image.

    Returns:
        numpy.ndarray: The raster array.
    """
    return np.transpose(arr, [2, 0, 1])


def adjust_band(band: np.ndarray, kind: str | None = None) -> np.ndarray:
    """Stretches a band's values to 0..1.

    Args:
        band: The band.
        kind: Unused.

    Returns:
        numpy.ndarray: The stretched band.
    """
    lo, hi = np.nanmin(band), np.nanmax(band)
    return (band - lo) / ((hi - lo) or 1)  # type: ignore[no-any-return]


def show(
    source: Any,
    with_bounds: bool = True,
    contour: bool = False,
    contour_label_kws: dict[str, Any] | None = None,
    ax: Any = None,
    title: str | None = None,
    transform: Any = None,
    adjust: bool = False,
    **kwargs: Any,
) -> Any:
    """Displays a raster or array with matplotlib.

    Args:
        source: A dataset, a `(dataset, band)` tuple, or an array (2-D, or 3-D with 3 or 4
            bands shown as RGB).
        with_bounds: Use the dataset's (or `transform`'s) coordinates for the axes.
        contour: Draw contours instead of an image.
        contour_label_kws: Keyword arguments of `clabel`.
        ax: The axes to draw on; by default, a new figure's.
        title: The title.
        transform: The array's transform.
        adjust: Stretch RGB bands to 0..1.
        **kwargs: Keyword arguments of `imshow` (or `contour`).

    Returns:
        matplotlib.axes.Axes: The axes.
    """
    plt = get_plt()
    if isinstance(source, tuple):
        arr = source[0].read(source[1], masked=True)
        if with_bounds:
            kwargs["extent"] = plotting_extent(source[0])
    elif isinstance(source, DatasetReader):
        transform = source.transform
        if source.count == 1:
            arr = source.read(1, masked=True)
        else:
            from terrio.rio.enums import ColorInterp

            ci = {c: i for i, c in enumerate(source.colorinterp)}
            try:
                idx = [1 + ci[c] for c in (ColorInterp.red, ColorInterp.green, ColorInterp.blue)]
                arr = source.read(idx, masked=True)
            except KeyError:
                arr = source.read(1, masked=True)
        if with_bounds:
            kwargs["extent"] = plotting_extent(source)
    else:
        arr = source
        if transform and with_bounds:
            kwargs["extent"] = plotting_extent(arr, transform)
    show_ax = ax is None
    if ax is None:
        ax = plt.gca()
    if arr.ndim >= 3 and arr.shape[0] in (3, 4):
        arr = reshape_as_image(arr)
        if adjust:
            arr = np.stack([adjust_band(arr[..., i]) for i in range(arr.shape[-1])], axis=-1)
    elif arr.ndim >= 3:
        arr = arr[0]
    if contour:
        if "cmap" not in kwargs:
            kwargs["colors"] = kwargs.get("colors", "red")
        cs = ax.contour(np.flipud(arr) if "extent" in kwargs else arr, origin="lower", **kwargs)
        if contour_label_kws is not None:
            ax.clabel(cs, **contour_label_kws)
    else:
        ax.imshow(arr, **kwargs)
    if title:
        ax.set_title(title, fontweight="bold")
    if show_ax:
        plt.show()
    return ax


def show_hist(
    source: Any,
    bins: int = 10,
    masked: bool = True,
    title: str = "Histogram",
    ax: Any = None,
    label: Any = None,
    range: Any = None,
    **kwargs: Any,
) -> None:
    """Displays a histogram of a raster's values.

    Args:
        source: A dataset, a `(dataset, band)` tuple, or an array.
        bins: The number of bins.
        masked: Leave out masked values.
        title: The title.
        ax: The axes to draw on.
        label: Band labels.
        range: The value range.
        **kwargs: Keyword arguments of `hist`.
    """
    plt = get_plt()
    if isinstance(source, DatasetReader):
        arr = source.read(masked=masked)
    elif isinstance(source, tuple):
        arr = source[0].read(source[1], masked=masked)
    else:
        arr = source
    arr = np.asarray(arr.compressed() if np.ma.isMaskedArray(arr) and masked else arr).ravel()
    show_fig = ax is None
    if ax is None:
        ax = plt.gca()
    ax.hist(arr, bins=bins, range=range, label=label, **kwargs)
    ax.set_title(title, fontweight="bold")
    ax.set_xlabel("DN")
    ax.set_ylabel("Frequency")
    if show_fig:
        plt.show()


__all__ = [
    "adjust_band",
    "plotting_extent",
    "reshape_as_image",
    "reshape_as_raster",
    "show",
    "show_hist",
]
