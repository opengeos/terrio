# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT

"""Generate crates/terrio-crs/tests/data/epsg_deprecated.json: where PROJ 9.9.0 places a point
in each deprecated EPSG geographic, geocentric and projected CRS, by the code's own definition.

For each such CRS (``pyproj.get_codes(..., allow_deprecated=True)`` less the current ones), the
fixture holds the code, the codes PROJ lists as its replacements
(``CRS.get_non_deprecated()``), a point (the centre of its area of use, in WGS 84 degrees) and
PROJ's coordinates of the point in the CRS by
``Transformer.from_crs("EPSG:4326", f"EPSG:{code}", always_xy=True).transform(lon, lat, 0)``,
or ``null`` where PROJ cannot transform it. PROJ uses the code's own definition, where GDAL
substitutes the replacement by default (``OSR_USE_NON_DEPRECATED``).

Usage: PROJ_NETWORK=OFF python scripts/gen_epsg_deprecated.py (needs pyproj 3.8.0, PROJ 9.9.0,
no grids installed)
"""

import json
import math
from pathlib import Path

import pyproj
from pyproj import CRS, Transformer
from pyproj.enums import PJType

OUT = Path(__file__).resolve().parents[1] / "crates/terrio-crs/tests/data/epsg_deprecated.json"
KINDS = (PJType.GEOGRAPHIC_2D_CRS, PJType.GEOGRAPHIC_3D_CRS, PJType.GEOCENTRIC_CRS, PJType.PROJECTED_CRS)


def centre(crs):
    """The centre of a CRS's area of use, in degrees (across the antimeridian when it spans it)."""
    a = crs.area_of_use
    east = a.east + 360.0 if a.east < a.west else a.east
    lon = (a.west + east) / 2.0
    return (lon - 360.0 if lon > 180.0 else lon, (a.south + a.north) / 2.0)


def main():
    pyproj.network.set_network_enabled(False)
    codes = set()
    for kind in KINDS:
        every = set(pyproj.get_codes("EPSG", kind, allow_deprecated=True))
        codes |= every - set(pyproj.get_codes("EPSG", kind, allow_deprecated=False))
    cases, skipped = [], []
    for code in sorted(codes, key=int):
        crs = CRS.from_epsg(int(code))
        # Without an area of use, or with PROJ's placeholder for none (-1000 degrees: EPSG:4296).
        if crs.area_of_use is None or crs.area_of_use.west < -180.0:
            skipped.append(int(code))
            continue
        lon, lat = centre(crs)
        try:
            out = Transformer.from_crs("EPSG:4326", crs, always_xy=True).transform(lon, lat, 0.0)
            placed = [v for v in out] if all(math.isfinite(v) for v in out) else None
        except pyproj.exceptions.ProjError:
            placed = None
        ids = (c.to_json_dict().get("id", {}) for c in crs.get_non_deprecated())
        replacements = sorted({int(i["code"]) for i in ids if i.get("authority") == "EPSG"})
        cases.append({"code": int(code), "name": crs.name, "replaced_by": replacements, "point": [lon, lat], "proj": placed})
    data = {"proj_version": pyproj.proj_version_str, "cases": cases}
    OUT.write_text(json.dumps(data, indent=1) + "\n")
    print(f"{len(cases)} deprecated CRSs, {sum(c['proj'] is None for c in cases)} PROJ cannot transform")
    print(f"{len(skipped)} without an area of use left out: {skipped}")


if __name__ == "__main__":
    main()
