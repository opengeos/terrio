# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (adapted and modified):
#   rasterio 1.5.2:
#     rasterio/mask.py

"""Masking rasters by geometries."""

from __future__ import annotations

import warnings
from collections.abc import Iterable
from typing import Any

import numpy as np
from affine import Affine

from terrio.rio.errors import WindowError
from terrio.rio.features import geometry_mask, geometry_window
from terrio.rio.windows import Window


def raster_geometry_mask(
    dataset: Any,
    shapes: Iterable[Any],
    all_touched: bool = False,
    invert: bool = False,
    crop: bool = False,
    pad: bool = False,
    pad_width: float = 0.5,
) -> tuple[np.ndarray, Affine, Window | None]:
    """A mask of a dataset's pixels outside (or inside) geometries.

    Args:
        dataset: The dataset.
        shapes: GeoJSON-like geometries in the dataset's CRS.
        all_touched: Include every pixel a geometry touches.
        invert: True inside the geometries (by default, True outside).
        crop: Crop the mask to the geometries' window.
        pad: Pad the cropped window by `pad_width` pixels.
        pad_width: The padding.

    Returns:
        tuple: `(mask, transform, window)`; the window is None without `crop`.

    Raises:
        ValueError: If `crop` and the shapes do not overlap the dataset.
    """
    shapes = list(shapes)
    pad_x = pad_y = pad_width if crop and pad else 0
    try:
        window = geometry_window(dataset, shapes, pad_x=pad_x, pad_y=pad_y)
    except WindowError:
        if crop:
            raise ValueError("Input shapes do not overlap raster.") from None
        warnings.warn(
            "shapes are outside bounds of raster. Are they in different coordinate reference "
            "systems?",
            stacklevel=2,
        )
        mask = np.ones(shape=dataset.shape[-2:], dtype=bool)
        return (~mask if invert else mask), dataset.transform, None
    win: Window | None
    if crop:
        transform = dataset.window_transform(window)
        out_shape = (int(window.height), int(window.width))
        win = window
    else:
        win = None
        transform = dataset.transform
        out_shape = (int(dataset.height), int(dataset.width))
    mask = geometry_mask(
        shapes, transform=transform, invert=invert, out_shape=out_shape, all_touched=all_touched
    )
    return mask, transform, win


def mask(
    dataset: Any,
    shapes: Iterable[Any],
    all_touched: bool = False,
    invert: bool = False,
    nodata: Any = None,
    filled: bool = True,
    crop: bool = False,
    pad: bool = False,
    pad_width: float = 0.5,
    indexes: Any = None,
) -> tuple[Any, Affine]:
    """Masks a dataset's pixels outside (or inside) geometries.

    Args:
        dataset: The dataset.
        shapes: GeoJSON-like geometries in the dataset's CRS.
        all_touched: Keep every pixel a geometry touches.
        invert: Mask the pixels inside the geometries instead.
        nodata: The value of masked pixels (by default the dataset's nodata, or 0).
        filled: Return a filled array (else a masked array).
        crop: Crop to the geometries.
        pad: Pad the cropped window by `pad_width` pixels.
        pad_width: The padding.
        indexes: A band, a sequence of bands, or None for all.

    Returns:
        tuple: `(array, transform)`.
    """
    if nodata is None:
        nodata = dataset.nodata if dataset.nodata is not None else 0
    shapes = list(shapes)
    shape_mask, transform, window = raster_geometry_mask(
        dataset,
        shapes,
        all_touched=all_touched,
        invert=invert,
        crop=crop,
        pad=pad,
        pad_width=pad_width,
    )
    if indexes is None:
        out_shape: tuple[int, ...] = (dataset.count,) + shape_mask.shape
    elif isinstance(indexes, int):
        out_shape = shape_mask.shape
    else:
        out_shape = (len(indexes),) + shape_mask.shape
    out = dataset.read(window=window, out_shape=out_shape, masked=True, indexes=indexes)
    out.mask = out.mask | shape_mask
    if filled:
        out = out.filled(nodata)
    return out, transform


__all__ = ["mask", "raster_geometry_mask"]
