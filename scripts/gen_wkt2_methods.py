"""Generate crates/terrio-crs/tests/data/wkt2_methods.json: EPSG CRSs of five methods PROJ 9.9.0
applies in WKT 2, and where PROJ places a point in them.

Each case is the WKT 2 of an EPSG CRS of Hyperbolic Cassini-Soldner (9833), Guam Projection
(9831), Krovak Modified (1042) and its north-orientated form (1043), Lambert Conic Conformal (2SP
Michigan) (1051) or Colombia Urban (1052), as PROJ writes it, without its `ID` and `USAGE` nodes
(so it is read by its method, not by its code). One more is Krovak Modified with the evaluation
point and coefficients EPSG's method defines (illustrative values), which PROJ's `mod_krovak`
fixes and ignores. For each: the PROJ string of
its geographic CRS, a point in it, the point's coordinates by
`Transformer.from_crs(crs.geodetic_crs, crs, always_xy=True)`, `CRS.to_proj4()`, its method's
EPSG code, and the method and parameters (lengths in metres) of the PROJJSON conversion PROJ
writes for that PROJ string.

Usage: PROJ_NETWORK=OFF python scripts/gen_wkt2_methods.py (needs pyproj 3.8.0)
"""

import json
import re
from pathlib import Path

import pyproj
from pyproj import CRS, Transformer

OUT = (
    Path(__file__).resolve().parents[1]
    / "crates/terrio-crs/tests/data/wkt2_methods.json"
)
# A point in each CRS's area of use.
CODES = {
    3139: (179.3, -16.6),
    3993: (144.795, 13.44),
    5224: (15.475, 49.82),
    5225: (15.475, 49.82),
    5515: (15.475, 49.82),
    5516: (15.475, 49.82),
    6201: (-84.665, 44.86),
    6247: (-74.125, 4.65),
}
# Krovak Modified's evaluation point and coefficients C1 to C10 (EPSG parameters 8617, 8618 and 1026
# to 1035), with illustrative values: PROJ ignores them.
KROVAK_MODIFIED = (
    'PARAMETER["Ordinate 1 of evaluation point",1089000,LENGTHUNIT["metre",1],ID["EPSG",8617]],'
    'PARAMETER["Ordinate 2 of evaluation point",654000,LENGTHUNIT["metre",1],ID["EPSG",8618]],'
    + ",".join(
        f'PARAMETER["C{i + 1}",{c},SCALEUNIT["coefficient",1],ID["EPSG",{1026 + i}]]'
        for i, c in enumerate(
            [
                2.946529277e-02,
                2.515965696e-02,
                1.193845912e-07,
                -4.668270147e-07,
                9.233980362e-12,
                1.523735715e-12,
                1.696780024e-18,
                4.408314235e-18,
                -8.331083518e-24,
                -3.689471323e-24,
            ]
        )
    )
)


def without_ids(wkt):
    """The WKT without its EPSG `ID` and `USAGE` nodes."""
    wkt = re.sub(r",USAGE\[.*?BBOX\[[^\]]*\]\]", "", wkt)
    return re.sub(r',ID\["EPSG",\d+\]', "", wkt)


def conversion(crs):
    """The method code and `[parameter code, value]`s of a CRS's PROJJSON conversion, lengths in
    metres."""
    conv = json.loads(crs.to_json())["conversion"]
    params = []
    for p in conv["parameters"]:
        unit = p.get("unit")
        linear = isinstance(unit, dict) and unit.get("type") == "LinearUnit"
        factor = unit["conversion_factor"] if linear else 1.0
        params.append([p["id"]["code"], p["value"] * factor])
    return {"method": conv["method"]["id"]["code"], "parameters": params}


def case(name, wkt, point, method_code):
    crs = CRS(wkt)
    x, y = Transformer.from_crs(crs.geodetic_crs, crs, always_xy=True).transform(
        *point, errcheck=True
    )
    return {
        "name": name,
        "wkt": wkt,
        "geographic": crs.geodetic_crs.to_proj4(),
        "point": point,
        "proj": [x, y],
        "proj4": crs.to_proj4(),
        "method_code": int(method_code),
        "projjson": conversion(CRS(crs.to_proj4())),
    }


def main():
    """Write the cases from the pinned PROJ version."""
    assert pyproj.proj_version_str == "9.9.0", pyproj.proj_version_str
    cases = []
    for code, point in CODES.items():
        crs = CRS.from_epsg(code)
        cases.append(
            case(
                f"EPSG:{code} {crs.name}",
                without_ids(crs.to_wkt("WKT2_2019")),
                point,
                crs.coordinate_operation.method_code,
            )
        )
    wkt = without_ids(CRS.from_epsg(5515).to_wkt("WKT2_2019"))
    wkt = wkt.replace(
        'PARAMETER["False easting"', KROVAK_MODIFIED + ',PARAMETER["False easting"', 1
    )
    cases.append(
        case(
            "EPSG:5515 with its evaluation point and coefficients",
            wkt,
            CODES[5515],
            CRS.from_epsg(5515).coordinate_operation.method_code,
        )
    )
    OUT.write_text(
        json.dumps({"proj_version": pyproj.proj_version_str, "cases": cases}, indent=1)
        + "\n"
    )


if __name__ == "__main__":
    main()
