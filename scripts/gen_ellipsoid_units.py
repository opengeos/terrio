# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT

"""Generate crates/terrio-crs/tests/data/ellipsoid_units.json: the EPSG CRSs whose ellipsoid PROJ
9.9.0's WKT 2 gives in a unit other than the metre (`ELLIPSOID["Clarke 1858",20926348,
294.260676369261,LENGTHUNIT["Clarke's foot",0.3047972654]]`), with the ellipsoid PROJ reads in
metres, the code it identifies, and a point from WGS 84 (opengeos/terrio#1012, item 58).

Each CRS is written twice: PROJ's WKT 2 (2019), and the same without its root `ID` (identified by
its content only; the `ID` can stand before a `REMARK`). The point is the centre of the CRS's area
of use, transformed by `Transformer.from_crs("EPSG:4326", wkt, always_xy=True)` without the root
`ID`.

Usage: PROJ_NETWORK=OFF python scripts/gen_ellipsoid_units.py (needs pyproj 3.8.0)
"""

import json
import re
from pathlib import Path

import pyproj
from pyproj import CRS, Transformer
from pyproj.database import query_crs_info

OUT = (
    Path(__file__).resolve().parents[1]
    / "crates/terrio-crs/tests/data/ellipsoid_units.json"
)
UNIT = re.compile(r'ELLIPSOID\["[^"]*",[^,]+,[^,]+,LENGTHUNIT\["([^"]+)"')


def without_root_id(wkt, code):
    """The WKT 2 without its root `ID` (its last `ID` of the code, before any `REMARK`)."""
    old = f',ID["EPSG",{code}]'
    i = wkt.rindex(old)
    return wkt[:i] + wkt[i + len(old) :]


def main():
    cases = []
    for info in query_crs_info(auth_name="EPSG"):
        crs = CRS.from_epsg(int(info.code))
        wkt = crs.to_wkt("WKT2_2019")
        unit = UNIT.search(wkt)
        if crs.ellipsoid is None or not unit or unit.group(1) == "metre":
            continue
        code = int(info.code)
        bare = without_root_id(wkt, code)
        area = crs.area_of_use
        lon = (area.west + area.east) / 2
        lat = (area.south + area.north) / 2
        x, y = Transformer.from_crs("EPSG:4326", bare, always_xy=True).transform(
            lon, lat
        )
        cases.append(
            {
                "code": code,
                "type": info.type.name,
                "unit": unit.group(1),
                "a": crs.ellipsoid.semi_major_metre,
                "b": crs.ellipsoid.semi_minor_metre,
                "epsg": crs.to_epsg(),
                "bare_epsg": CRS.from_wkt(bare).to_epsg(),
                "wkt": wkt,
                "bare": bare,
                "point": [lon, lat],
                "proj": [x, y],
            }
        )
    data = {"proj": pyproj.proj_version_str, "cases": cases}
    OUT.write_text(json.dumps(data, indent=1) + "\n")
    print(f"{len(cases)} cases to {OUT}")


if __name__ == "__main__":
    main()
