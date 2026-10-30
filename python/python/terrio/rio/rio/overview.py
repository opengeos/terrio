# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (adapted and modified):
#   rasterio 1.5.0:
#     rasterio/rio/overview.py

"""Overviews in rasterio's command line (`rasterio.rio.overview`)."""

from __future__ import annotations


def get_maximum_overview_level(width: int, height: int, minsize: int = 256) -> int:
    """The number of halvings after which the smaller side of a dataset is `minsize` or less.

    Args:
        width: The dataset's width.
        height: The dataset's height.
        minsize: The size the smallest overview reaches.

    Returns:
        int: The overview level.
    """
    level, factor = 0, 1
    while min(width // factor, height // factor) > minsize:
        factor *= 2
        level += 1
    return level
