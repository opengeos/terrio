# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (adapted and modified):
#   rasterio 1.5.2:
#     rasterio/sample.py

"""Sampling datasets at points."""

from __future__ import annotations

from collections.abc import Iterable, Iterator
from itertools import islice
from typing import Any

import numpy as np

from terrio.rio.enums import MaskFlags
from terrio.rio.transform import rowcol
from terrio.rio.windows import Window


def _transform_xy(dataset: Any, xy: Iterable[tuple[float, float]]) -> Iterator[tuple[int, int]]:
    """The rows and columns of points, in batches.

    Args:
        dataset: The dataset.
        xy: Points `(x, y)`.

    Yields:
        tuple: `(row, col)`.
    """
    it = iter(xy)
    while True:
        buf = tuple(islice(it, 0, 256))
        if not buf:
            break
        rows, cols = rowcol(dataset.transform, *zip(*buf, strict=False))
        yield from zip(rows, cols, strict=False)


def sort_xy(xy: Iterable[tuple[float, float]]) -> list[tuple[float, float]]:
    """Sorts points by y, then x.

    Args:
        xy: Points.

    Returns:
        list: The sorted points.
    """
    x, y = tuple(zip(*xy, strict=False))
    return [(x[i], y[i]) for i in np.lexsort([y, x])]


def sample_gen(
    dataset: Any, xy: Iterable[tuple[float, float]], indexes: Any = None, masked: bool = False
) -> Iterator[Any]:
    """Samples a dataset's bands at points.

    Args:
        dataset: The dataset.
        xy: Points `(x, y)` in the dataset's CRS.
        indexes: A band, a sequence of bands, or None for all.
        masked: Yield masked arrays.

    Yields:
        numpy.ndarray: The band values at each point (nodata, or 0, outside the dataset).
    """
    if indexes is None:
        indexes = dataset.indexes
    elif isinstance(indexes, int):
        indexes = [indexes]
    nodata: Any = np.full(len(indexes), (dataset.nodata or 0), dtype=dataset.dtypes[0])
    if masked:
        flags = [set(dataset.mask_flag_enums[i - 1]) for i in indexes]
        is_masked = any(
            {MaskFlags.alpha, MaskFlags.per_dataset, MaskFlags.nodata} & f for f in flags
        )
        mask = [not (is_masked and f == {MaskFlags.all_valid}) for f in flags]
        nodata = np.ma.array(nodata, mask=mask)
    for row, col in _transform_xy(dataset, xy):
        if 0 <= row < dataset.height and 0 <= col < dataset.width:
            data = dataset.read(indexes, window=Window(col, row, 1, 1), masked=masked)
            yield data[:, 0, 0]
        else:
            yield nodata


__all__ = ["sample_gen", "sort_xy"]
