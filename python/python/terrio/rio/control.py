# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (adapted and modified):
#   rasterio 1.5.2:
#     rasterio/control.py

"""Ground control points, as `rasterio.control`."""

from __future__ import annotations

import uuid
from typing import Any


class GroundControlPoint:
    """A mapping of row, col image coordinates to x, y, z."""

    def __init__(
        self,
        row: float | None = None,
        col: float | None = None,
        x: float | None = None,
        y: float | None = None,
        z: float | None = None,
        id: str | None = None,
        info: str | None = None,
    ) -> None:
        """Creates a ground control point.

        Args:
            row: The row (line) coordinate, from the image's top edge.
            col: The column (pixel) coordinate, from the image's left edge.
            x: The spatial X coordinate.
            y: The spatial Y coordinate.
            z: The elevation, optional.
            id: A unique identifier (a UUID by default).
            info: A short description.

        Raises:
            ValueError: If `row`, `col`, `x` or `y` is missing.
        """
        if any(v is None for v in (row, col, x, y)):
            raise ValueError("row, col, x, and y are required parameters.")
        self.id = str(uuid.uuid4()) if id is None else id
        self.info = info
        self.row = row
        self.col = col
        self.x = x
        self.y = y
        self.z = z

    def __repr__(self) -> str:
        """The representation.

        Returns:
            str: `GroundControlPoint(row=..., col=..., ...)`.
        """
        args = ", ".join(
            f"{att}={getattr(self, att)!r}"
            for att in ("row", "col", "x", "y", "z", "id", "info")
            if getattr(self, att) is not None
        )
        return f"GroundControlPoint({args})"

    def asdict(self) -> dict[str, Any]:
        """A dictionary of the point.

        Returns:
            dict: Its id, info, row, col, x, y and z.
        """
        return {
            "id": self.id,
            "info": self.info,
            "row": self.row,
            "col": self.col,
            "x": self.x,
            "y": self.y,
            "z": self.z,
        }

    @property
    def __geo_interface__(self) -> dict[str, Any]:
        """The point as a GeoJSON feature.

        Returns:
            dict: The feature.
        """
        coords = [self.x, self.y] + ([self.z] if self.z is not None else [])
        return {
            "id": self.id,
            "type": "Feature",
            "geometry": {"type": "Point", "coordinates": tuple(coords)},
            "properties": self.asdict(),
        }


__all__ = ["GroundControlPoint"]
