# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT

"""Generate crates/terrio-crs/tests/data/vertical_esri.json: ESRI's vertical CRS (`VERTCS`, with
`PARAMETER["Direction",-1]` for an axis down) alone, in a `COMPD_CS`, and in ESRI's compound CRS
(a `GEOGCS` or `PROJCS` followed by a `VERTCS`, as ESRI writes EPSG:9707), as PROJ 9.9.0 reads
them (opengeos/terrio#1012, item 59). Controls: a GDAL `VERT_CS` after a `GEOGCS`, and a `VERTCS`
after a WKT 2 `GEOGCRS`, which PROJ ignores.

Each case is a WKT, the type and code PROJ reads (`to_epsg()`), its axis directions, and, for a
compound CRS, a point from EPSG:9707 (WGS 84 + EGM96 height) by `Transformer.from_crs(
"EPSG:9707", wkt, always_xy=True)`. Then every EPSG vertical CRS ESRI writes, in ESRI's WKT and in
GDAL's WKT 1 without its `AUTHORITY`, with the code PROJ identifies it by (`to_epsg()`: by its
name, datum, unit and direction).

Usage: PROJ_NETWORK=OFF python scripts/gen_vertical_esri.py (needs pyproj 3.8.0)
"""

import json
from pathlib import Path

import pyproj
from pyproj import CRS, Transformer
from pyproj.database import query_crs_info
from pyproj.enums import PJType

OUT = (
    Path(__file__).resolve().parents[1]
    / "crates/terrio-crs/tests/data/vertical_esri.json"
)
POINT = [8.5, 47.3, 10.0]


def esri(code):
    return CRS.from_epsg(code).to_wkt("WKT1_ESRI")


def declared(wkt, code):
    """The WKT with an `AUTHORITY` of the code as its root's last node."""
    return wkt[:-1] + f',AUTHORITY["EPSG","{code}"]]'


def down(wkt):
    assert wkt.count('"Direction",1.0') == 1
    return wkt.replace('"Direction",1.0', '"Direction",-1.0')


def identified():
    """Every EPSG vertical CRS ESRI writes as a `VERTCS`, in ESRI's WKT and in GDAL's WKT 1
    without its `AUTHORITY`, with the code PROJ identifies each by (`to_epsg()`)."""
    out = []
    for info in query_crs_info(auth_name="EPSG", pj_types=[PJType.VERTICAL_CRS]):
        crs = CRS.from_epsg(int(info.code))
        try:
            wkt = crs.to_wkt("WKT1_ESRI")
        except pyproj.exceptions.CRSError:
            continue
        if not wkt or not wkt.startswith("VERTCS"):
            continue
        gdal = crs.to_wkt("WKT1_GDAL")
        old = f',AUTHORITY["EPSG","{info.code}"]]'
        assert gdal.endswith(old), info.code
        gdal = gdal[: -len(old)] + "]"
        out.append(
            {
                "code": int(info.code),
                "esri": wkt,
                "esri_epsg": CRS.from_wkt(wkt).to_epsg(),
                "gdal": gdal,
                "gdal_epsg": CRS.from_wkt(gdal).to_epsg(),
            }
        )
    return out


def main():
    g, egm = esri(4326), esri(5773)
    cases = [
        ("VERTCS of EGM96 height declaring it", declared(egm, 5773)),
        ("VERTCS of MSL depth (Direction -1) declaring it", declared(esri(5715), 5715)),
        ("VERTCS of NAVD88 height (ftUS) declaring it", declared(esri(6360), 6360)),
        (
            "VERTCS of EGM96 height, Direction -1, declaring it",
            declared(down(egm), 5773),
        ),
        ("ESRI's WKT of EPSG:9707", esri(9707)),
        ("GEOGCS and VERTCS, Direction -1", f"{g},{down(egm)}"),
        ("GEOGCS and VERTCS with spaces between", f"{g} ,\n {egm}"),
        ("GEOGCS and VERTCS declaring EPSG:5773", f"{g},{declared(egm, 5773)}"),
        ("GEOGCS, VERTCS and another VERTCS", f"{g},{egm},{down(egm)}"),
        ("PROJCS and VERTCS", f"{esri(32631)},{egm}"),
        ("COMPD_CS with a VERTCS", f'COMPD_CS["WGS 84 + EGM96 height",{g},{egm}]'),
        ("COMPD_CS with a VERTCS, Direction -1", f'COMPD_CS["x",{g},{down(egm)}]'),
        (
            "GDAL GEOGCS and VERT_CS (ignored)",
            f"{CRS.from_epsg(4326).to_wkt('WKT1_GDAL')},"
            f"{CRS.from_epsg(5773).to_wkt('WKT1_GDAL')}",
        ),
        (
            "WKT 2 GEOGCRS and VERTCS (ignored)",
            f"{CRS.from_epsg(4326).to_wkt('WKT2_2019')},{egm}",
        ),
    ]
    out = []
    for name, wkt in cases:
        crs = CRS.from_wkt(wkt)
        case = {
            "name": name,
            "wkt": wkt,
            "type": crs.type_name,
            "epsg": crs.to_epsg(),
            "axes": [a.direction for a in crs.axis_info],
        }
        if crs.is_compound:
            t = Transformer.from_crs("EPSG:9707", wkt, always_xy=True)
            case["proj"] = list(t.transform(*POINT))
        out.append(case)
    data = {
        "proj": pyproj.proj_version_str,
        "point": POINT,
        "cases": out,
        "identified": identified(),
    }
    OUT.write_text(json.dumps(data, indent=1) + "\n")
    print(f"{len(out)} cases to {OUT}")


if __name__ == "__main__":
    main()
