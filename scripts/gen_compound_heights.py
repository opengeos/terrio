# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT

"""Generate crates/terrio-crs/tests/data/compound_heights.tsv: where PROJ 9.9.0 places points
between EPSG compound CRSs and ellipsoidal heights, without grids.

Without a geoid model's grid, PROJ goes between ellipsoidal heights and a vertical CRS by its
ballpark vertical transformation, which keeps the height. Chained with it, every horizontal
transformation is of unknown accuracy, so ``proj_trans`` uses the one of the smallest area that
holds the point: from WGS 84 (3D) into OSGB36 / British National Grid + ODN height (EPSG:7405), it
places (-2°, 54°) by OSGB36 to WGS 84 (3), 5.2 m from (6), the most accurate.

Cases, each a source CRS, a target CRS, a point and the point in the target by
``Transformer.from_crs(src, dst, always_xy=True).transform``:

- every EPSG compound CRS that is not deprecated, from WGS 84 (3D) (EPSG:4979) at the centre of its
  area at 100 m, and back from where PROJ places that point;
- for every tenth, the same point from WGS 84 (geocentric) (EPSG:4978) and from WGS 84 (2D)
  (EPSG:4326, which has no heights, so no vertical transformation), and in 2D (no height column:
  ``transform(x, y)``);
- between compound CRSs of one horizontal CRS and another vertical CRS.

Usage: PROJ_NETWORK=OFF python scripts/gen_compound_heights.py (pyproj with PROJ 9.9.0, no grids)
"""

import math
import sys
import warnings
from collections import defaultdict
from pathlib import Path

import pyproj
import pyproj.network
from pyproj import CRS, Transformer
from pyproj.database import query_crs_info
from pyproj.enums import PJType

# The sibling script's grid check, importable under `python -I` too.
sys.path.insert(0, str(Path(__file__).resolve().parent))
from gen_wkt_datum_shift import installed_grids

OUT = (
    Path(__file__).resolve().parents[1]
    / "crates/terrio-crs/tests/data/compound_heights.tsv"
)
SAMPLE_EVERY = 10
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
        point: Input coordinates, with an optional height.

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


def row(src, dst, point):
    """Format a reference case as one tab-separated line.

    Args:
        src: Source CRS definition.
        dst: Target CRS definition.
        point: Input coordinates, with an optional height.

    Returns:
        str or None: Reference case, or None if PROJ cannot place the point.
    """
    out = place(src, dst, point)
    if out is None:
        return None
    fields = [src, dst, *(repr(float(v)) for v in point)]
    if len(point) == 2:
        fields.append("")
    fields += [repr(float(v)) for v in out]
    if len(out) == 2:
        fields.append("")
    # A 2D result has no final height field; avoid trailing tabs in the frozen fixture.
    return "\t".join(fields).rstrip("\t")


def main():
    """Write the offline PROJ 9.9.0 compound-height reference corpus."""
    assert pyproj.__proj_version__ == "9.9.0", pyproj.__proj_version__
    assert not pyproj.network.is_network_enabled(), "set PROJ_NETWORK=OFF"
    # No grids, so the values don't depend on what is installed: no file PROJ's database names
    # as a grid, nor of a grid format (`gen_wkt_datum_shift.installed_grids`).
    found = installed_grids()
    assert not found, found
    warnings.filterwarnings("ignore")
    infos = [
        i
        for i in query_crs_info("EPSG", PJType.COMPOUND_CRS, allow_deprecated=False)
        if i.area_of_use is not None
    ]
    infos.sort(key=lambda i: int(i.code))
    lines = []
    by_horizontal = defaultdict(list)
    for n, info in enumerate(infos):
        code = f"EPSG:{info.code}"
        lon, lat = centre(info.area_of_use)
        start = (lon, lat, HEIGHT)
        placed = place("EPSG:4979", code, start)
        if placed is None:
            continue
        lines.append(row("EPSG:4979", code, start))
        lines.append(row(code, "EPSG:4979", placed))
        if n % SAMPLE_EVERY == 0:
            geocentric = place("EPSG:4979", "EPSG:4978", start)
            lines.append(row("EPSG:4978", code, geocentric))
            lines.append(row("EPSG:4326", code, start))
            lines.append(row("EPSG:4979", code, start[:2]))
            lines.append(row(code, "EPSG:4979", placed[:2]))
        parts = CRS(code).sub_crs_list
        horizontal = parts[0].to_epsg() if parts else None
        if horizontal is not None:
            by_horizontal[horizontal].append((code, placed))
    # Between compound CRSs of one horizontal CRS: the first with each of the others.
    for same in by_horizontal.values():
        (first, placed), others = same[0], same[1:]
        for other, _ in others:
            lines.append(row(first, other, placed))
            back = place(first, other, placed)
            if back is not None:
                lines.append(row(other, first, back))
    lines = [line for line in lines if line is not None]
    OUT.write_text("\n".join(lines) + "\n")
    print(f"{len(lines)} cases -> {OUT}")


if __name__ == "__main__":
    main()
