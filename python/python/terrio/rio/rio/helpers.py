# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Copyright © 2014, contributors of python-geojson
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (adapted and modified):
#   rasterio 1.5.0:
#     rasterio/rio/helpers.py
#   python-geojson (through rasterio) e82d2827110c9fccc5eb51d57076fd707c9a35be:
#     geojson/coords.py

"""Helpers of rasterio's command line (`rasterio.rio.helpers`)."""

from __future__ import annotations

from collections.abc import Iterator
from typing import Any


def coords(obj: Any) -> Iterator[tuple[Any, ...]]:
    """Yields every coordinate tuple of a GeoJSON geometry or feature, or of nested coordinates.

    Args:
        obj: A feature, geometry, or (nested) sequence of coordinates.

    Yields:
        tuple: Each position, such as `(x, y)`.
    """
    if isinstance(obj, (tuple, list)):
        coordinates = obj
    elif "geometry" in obj:
        coordinates = obj["geometry"]["coordinates"]
    else:
        coordinates = obj.get("coordinates", obj)
    for item in coordinates:
        if isinstance(item, (float, int)):
            yield tuple(coordinates)
            break
        yield from coords(item)
