# Written to match the behavior of:
#   rasterio 1.5.2:
#     rasterio/
# No code is translated from these sources.

"""Bounding boxes."""

from __future__ import annotations

from collections.abc import Sequence
from typing import NamedTuple


class BoundingBox(NamedTuple):
    """A bounding box: `(left, bottom, right, top)`."""

    left: float
    bottom: float
    right: float
    top: float


def disjoint_bounds(bounds1: Sequence[float], bounds2: Sequence[float]) -> bool:
    """Whether two bounding boxes do not intersect.

    Args:
        bounds1: A bounding box `(left, bottom, right, top)`.
        bounds2: Another bounding box.

    Returns:
        bool: True when the boxes are disjoint (boxes touching at an edge intersect).

    Raises:
        ValueError: If two nondegenerate boxes have opposite orientations.
            A zero-height box is treated as a boundary line in either orientation.
    """
    dy1 = bounds1[3] - bounds1[1]
    dy2 = bounds2[3] - bounds2[1]
    if (dy1 > 0 and dy2 < 0) or (dy1 < 0 and dy2 > 0):
        raise ValueError("Bounds must both have the same orientation")
    if dy1 > 0 or dy2 > 0:
        return (
            bounds1[0] > bounds2[2]
            or bounds2[0] > bounds1[2]
            or bounds1[1] > bounds2[3]
            or bounds2[1] > bounds1[3]
        )
    return (
        bounds1[0] > bounds2[2]
        or bounds2[0] > bounds1[2]
        or bounds1[3] > bounds2[1]
        or bounds2[3] > bounds1[1]
    )


__all__ = ["BoundingBox", "disjoint_bounds"]
