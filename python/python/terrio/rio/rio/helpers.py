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
