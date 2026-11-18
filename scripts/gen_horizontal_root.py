# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT

"""Generate crates/terrio-crs/tests/data/horizontal_root.json: the horizontal axes and the
geographic CRS PROJ 9.9.0 reads in WKTs whose horizontal CRS is not their root, or whose base
`GEOGCS` a text scan misses (opengeos/terrio#1012, item 60, and #1058 step 1b).

Axes: PROJ's WKT 2 (2019) of a WKT 1 with `TOWGS84`, a `BOUNDCRS` whose source CRS has the axes
(OSGB36, north then east; British National Grid, east then north), alone and in a
`COMPOUNDCRS`. Each case holds PROJ's axis directions. Geographic CRS: a WKT 1 in `(`/`)`
brackets, and one whose name holds `GEOGCS[`, both of no code (British National Grid with another
false easting, without its `AUTHORITY`), with the code of PROJ's `geodetic_crs`.

Usage: PROJ_NETWORK=OFF python scripts/gen_horizontal_root.py (needs pyproj 3.8.0)
"""

import json
from pathlib import Path

import pyproj
from pyproj import CRS

OUT = (
    Path(__file__).resolve().parents[1]
    / "crates/terrio-crs/tests/data/horizontal_root.json"
)
TOWGS84 = "TOWGS84[446.448,-125.157,542.06,0.15,0.247,0.842,-20.489],"


def sub(wkt, old, new):
    """`wkt` with `old`, which must occur once, replaced by `new`."""
    assert wkt.count(old) == 1, old
    return wkt.replace(old, new)


def bound(code):
    """PROJ's WKT 2 of the EPSG CRS's WKT 1 with a `TOWGS84`: a `BOUNDCRS`."""
    wkt = CRS.from_epsg(code).to_wkt("WKT1_GDAL")
    old = 'AUTHORITY["EPSG","6277"]]'
    assert wkt.count(old) == 1
    return CRS.from_wkt(wkt.replace(old, TOWGS84 + old)).to_wkt("WKT2_2019")


def main():
    height = CRS.from_epsg(5701).to_wkt("WKT2_2019")
    axes = [
        ("BOUNDCRS of OSGB36", bound(4277)),
        ("BOUNDCRS of British National Grid", bound(27700)),
        (
            "COMPOUNDCRS of a BOUNDCRS of OSGB36",
            f'COMPOUNDCRS["OSGB36 + ODN height",{bound(4277)},{height}]',
        ),
    ]
    # British National Grid with another false easting and without its code: a CRS of no code,
    # whose geographic CRS is read from its `GEOGCS`, not from the registry.
    bng = sub(
        CRS.from_epsg(27700).to_wkt("WKT1_GDAL"), ',AUTHORITY["EPSG","27700"]]', "]"
    )
    bng = sub(
        bng, 'PARAMETER["false_easting",400000]', 'PARAMETER["false_easting",400001]'
    )
    geographic = [
        ("WKT 1 in brackets", bng.replace("[", "(").replace("]", ")")),
        (
            "WKT 1 named with GEOGCS[",
            bng.replace(
                'PROJCS["OSGB36 / British National Grid"', 'PROJCS["GEOGCS[x] grid"'
            ),
        ),
    ]
    cases = []
    for name, wkt in axes:
        crs = CRS.from_wkt(wkt)
        directions = [a.direction for a in crs.axis_info][:2]
        cases.append({"name": name, "wkt": wkt, "axes": directions})
    for name, wkt in geographic:
        if "(" not in wkt:
            crs = CRS.from_wkt(wkt)
            assert crs.to_epsg() is None, name
            base = crs.geodetic_crs.to_epsg()
        else:
            # pyproj does not read `(`/`)` brackets; PROJ's projinfo does, with the same base.
            base = CRS.from_wkt(wkt.replace("(", "[").replace(")", "]")).geodetic_crs
            base = base.to_epsg()
        cases.append({"name": name, "wkt": wkt, "geographic": base})
    data = {"proj": pyproj.proj_version_str, "cases": cases}
    OUT.write_text(json.dumps(data, indent=1) + "\n")
    print(f"{len(cases)} cases to {OUT}")


if __name__ == "__main__":
    main()
