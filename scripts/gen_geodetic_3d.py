# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT

"""Generate crates/terrio-crs/tests/data/geodetic_3d.tsv: where PROJ 9.9.0 places points
between EPSG geocentric and geographic 3D CRSs and WGS 84, without grids.

From or into WGS 84 (2D) (EPSG:4326), which has no heights, PROJ keeps the height through the
datum shift (``push v_3`` before it, ``pop v_3`` after) and converts it to or from geocentric
coordinates on the geocentric CRS's own ellipsoid; it runs the legs between geocentric CRSs
before it on geocentric coordinates. From WGS 84 (3D) (EPSG:4979) the whole shift runs on
geocentric coordinates and moves the height.

Cases, each a source CRS, a target CRS, a point and the point in the target by
``Transformer.from_crs(src, dst, always_xy=True).transform``: every EPSG geocentric and geographic
3D CRS that is not deprecated and has an area of use, from WGS 84 (2D) and (3D) at the centre of
its area at 100 m, and back into both from where PROJ places that point from WGS 84 (3D) (the
codes left out, without an area or a placement, are printed).

Usage: PROJ_NETWORK=OFF python scripts/gen_geodetic_3d.py (pyproj with PROJ 9.9.0, no grids)
"""

import math
import sys
import warnings
from pathlib import Path

import pyproj
import pyproj.network
from pyproj import Transformer
from pyproj.database import query_crs_info
from pyproj.enums import PJType

# The sibling script's grid check, importable under `python -I` too.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from gen_wkt_datum_shift import installed_grids

OUT = (
    Path(__file__).resolve().parents[1] / "crates/terrio-crs/tests/data/geodetic_3d.tsv"
)
HEIGHT = 100.0


def centre(area):
    """Return the centre of an area of use, across the antimeridian too.

    Args:
        area: CRS area of use with west, east, south and north bounds.

    Returns:
        tuple: Centre longitude and latitude in degrees.
    """
    west, east = area.west, area.east
    lon = (
        (west + east) / 2
        if west <= east
        else ((west + east + 360) / 2 + 180) % 360 - 180
    )
    return lon, (area.south + area.north) / 2


def place(src, dst, point):
    """Return where PROJ places a point, or None when it cannot.

    Args:
        src: Source CRS definition.
        dst: Target CRS definition.
        point: Input coordinates with a height.

    Returns:
        tuple or None: Finite transformed coordinates, or None on failure.
    """
    try:
        values = Transformer.from_crs(src, dst, always_xy=True).transform(*point)
    except pyproj.exceptions.ProjError:
        return None
    if not all(math.isfinite(v) for v in values):
        return None
    return values


def row(src, dst, point, out):
    """Format a reference case as one tab-separated line.

    Args:
        src: Source CRS definition.
        dst: Target CRS definition.
        point: Input coordinates with a height.
        out: Where PROJ places the point.

    Returns:
        str: Reference case.
    """
    return "\t".join([src, dst, *(repr(float(v)) for v in (*point, *out))])


def main():
    """Write the offline PROJ 9.9.0 geocentric and geographic 3D reference corpus."""
    assert pyproj.__proj_version__ == "9.9.0", pyproj.__proj_version__
    assert not pyproj.network.is_network_enabled(), "set PROJ_NETWORK=OFF"
    found = installed_grids()
    assert not found, found
    warnings.filterwarnings("ignore")
    every = [
        info
        for kind in (PJType.GEOCENTRIC_CRS, PJType.GEOGRAPHIC_3D_CRS)
        for info in query_crs_info("EPSG", kind, allow_deprecated=False)
    ]
    skipped = sorted(int(i.code) for i in every if i.area_of_use is None)
    lines = []
    for info in sorted(
        (i for i in every if i.area_of_use is not None), key=lambda i: int(i.code)
    ):
        code = f"EPSG:{info.code}"
        lon, lat = centre(info.area_of_use)
        start = (lon, lat, HEIGHT)
        placed = place("EPSG:4979", code, start)
        if placed is None:
            skipped.append(int(info.code))
            continue
        lines.append(row("EPSG:4979", code, start, placed))
        cases = [
            ("EPSG:4326", code, start),
            (code, "EPSG:4979", placed),
            (code, "EPSG:4326", placed),
        ]
        for src, dst, point in cases:
            out = place(src, dst, point)
            if out is not None:
                lines.append(row(src, dst, point, out))
    OUT.write_text("\n".join(lines) + "\n")
    print(f"{len(lines)} cases -> {OUT}")
    print(f"{len(skipped)} codes left out: {', '.join(map(str, sorted(skipped)))}")


if __name__ == "__main__":
    main()
