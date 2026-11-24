"""Generate crates/terrio-crs/tests/data/wkt2_bound.json: where PROJ 9.9.0 places points of WKT 2
`BOUNDCRS`s with a Helmert transformation, to WGS 84 and to other CRSs (ED50, OSGB36), whose own
transformations to WGS 84 PROJ then applies, and of PROJ strings with `+towgs84`, which PROJ
reads as bound to WGS 84.

Each case is a bound CRS, a point in it, and the point in WGS 84, or in the
target CRS's own system, by `Transformer.from_crs(wkt, dst, always_xy=True)`, and for the
latter, PROJ's way back. The scale difference is written as PROJ
writes it, a ratio in unity (`1.0000067` for 6.7 ppm).

Usage: PROJ_NETWORK=OFF python scripts/gen_wkt2_bound.py (needs pyproj 3.8.0)
"""

import json
import os
import re
from pathlib import Path

import pyproj
import pyproj.datadir
import pyproj.network
from pyproj import CRS, Transformer

OUT = (
    Path(__file__).resolve().parents[1] / "crates/terrio-crs/tests/data/wkt2_bound.json"
)
DEG = 'ANGLEUNIT["degree",0.0174532925199433]'
LATLON = f'CS[ellipsoidal,2],AXIS["lat",north,ORDER[1],{DEG}],AXIS["lon",east,ORDER[2],{DEG}]'


def geog(name, datum, ellipsoid, a, rf, code=None):
    ident = f',ID["EPSG",{code}]' if code else ""
    return (
        f'GEOGCRS["{name}",DATUM["{datum}",ELLIPSOID["{ellipsoid}",{a},{rf},LENGTHUNIT["metre",1]]],'
        f'PRIMEM["Greenwich",0,{DEG}],{LATLON}{ident}]'
    )


BESSEL = geog("Local", "Local datum", "Bessel 1841", 6377397.155, 299.1528128)
ED50 = geog("ED50", "European Datum 1950", "International 1924", 6378388, 297, 4230)
ED50_NO_ID = geog("ED50", "European Datum 1950", "International 1924", 6378388, 297)
OSGB36 = geog(
    "OSGB36",
    "Ordnance Survey of Great Britain 1936",
    "Airy 1830",
    6377563.396,
    299.3249646,
    4277,
)
OSGB36_NO_ID = geog(
    "OSGB36",
    "Ordnance Survey of Great Britain 1936",
    "Airy 1830",
    6377563.396,
    299.3249646,
)
WGS84 = geog(
    "WGS 84", "World Geodetic System 1984", "WGS 84", 6378137, 298.257223563, 4326
)
NAD83 = geog(
    "NAD83", "North American Datum 1983", "GRS 1980", 6378137, 298.257222101, 4269
)
LOCAL = geog(
    "Local target", "Local target datum", "Clarke 1866", 6378206.4, 294.9786982
)
UTM32 = (
    'PROJCRS["UTM 32 on Local",BASEGEOGCRS["Local",DATUM["Local datum",ELLIPSOID["Bessel 1841",'
    f'6377397.155,299.1528128,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,{DEG}]],'
    'CONVERSION["UTM zone 32N",METHOD["Transverse Mercator",ID["EPSG",9807]],'
    f'PARAMETER["Latitude of natural origin",0,{DEG},ID["EPSG",8801]],'
    f'PARAMETER["Longitude of natural origin",9,{DEG},ID["EPSG",8802]],'
    'PARAMETER["Scale factor at natural origin",0.9996,SCALEUNIT["unity",1],ID["EPSG",8805]],'
    'PARAMETER["False easting",500000,LENGTHUNIT["metre",1],ID["EPSG",8806]],'
    'PARAMETER["False northing",0,LENGTHUNIT["metre",1],ID["EPSG",8807]]],'
    'CS[Cartesian,2],AXIS["(E)",east,ORDER[1],LENGTHUNIT["metre",1]],'
    'AXIS["(N)",north,ORDER[2],LENGTHUNIT["metre",1]]]'
)
# British National Grid on OSGB36 known only by its datum's code.
BNG = (
    'PROJCRS["Local grid",BASEGEOGCRS["Local OSGB",DATUM["D_OSGB_1936",ELLIPSOID["Airy 1830",'
    f'6377563.396,299.3249646,LENGTHUNIT["metre",1]],ID["EPSG",6277]],PRIMEM["Greenwich",0,{DEG}]],'
    'CONVERSION["British National Grid",METHOD["Transverse Mercator",ID["EPSG",9807]],'
    f'PARAMETER["Latitude of natural origin",49,{DEG},ID["EPSG",8801]],'
    f'PARAMETER["Longitude of natural origin",-2,{DEG},ID["EPSG",8802]],'
    'PARAMETER["Scale factor at natural origin",0.9996012717,SCALEUNIT["unity",1],ID["EPSG",8805]],'
    'PARAMETER["False easting",400000,LENGTHUNIT["metre",1],ID["EPSG",8806]],'
    'PARAMETER["False northing",-100000,LENGTHUNIT["metre",1],ID["EPSG",8807]]],'
    'CS[Cartesian,2],AXIS["(E)",east,ORDER[1],LENGTHUNIT["metre",1]],'
    'AXIS["(N)",north,ORDER[2],LENGTHUNIT["metre",1]]]'
)
NAMES = ["X-axis translation", "Y-axis translation", "Z-axis translation"]
NAMES += ["X-axis rotation", "Y-axis rotation", "Z-axis rotation"]
SHIFT = [598.1, 73.7, 418.2, 0.202, 0.045, -2.455, 6.7]


def bound(
    source,
    target,
    shift,
    method="Position Vector transformation (geog2D domain)",
    code=9606,
):
    params = [
        f'PARAMETER["{n}",{v},ID["EPSG",{c}]]'
        for (v, n), c in zip(zip(shift, NAMES), range(8605, 8611))
    ]
    if len(shift) > 6:
        params.append(
            f'PARAMETER["Scale difference",{1 + shift[6] * 1e-6!r},SCALEUNIT["unity",1],ID["EPSG",8611]]'
        )
    ident = f',ID["EPSG",{code}]' if code else ""
    return (
        f'BOUNDCRS[SOURCECRS[{source}],TARGETCRS[{target}],ABRIDGEDTRANSFORMATION["t",'
        f'METHOD["{method}"{ident}],{",".join(params)}]]'
    )


def wkt1(code, fmt="WKT1_GDAL", towgs84=None):
    """GDAL's (or Esri's) WKT 1 of an EPSG code, with a TOWGS84 node in its datum if given, as
    GDAL writes a bound CRS's source CRS when it has only WKT 1 for it."""
    wkt = CRS.from_epsg(code).to_wkt(fmt)
    if towgs84:
        wkt, n = re.subn(
            r"(SPHEROID\[[^\]]*\]\])", rf"\1,TOWGS84[{towgs84}]", wkt, count=1
        )
        assert n == 1, code
    return wkt


# British National Grid and OSGB36 as WKT 1 source CRSs: with a TOWGS84 of their own, which the
# bound transformation replaces, as PROJ transforms them; without one; and Esri's.
OSGB_SHIFT = "446.448,-125.157,542.06,0.15,0.247,0.842,-20.489"
BNG_WKT1_SHIFTED = wkt1(27700, towgs84=OSGB_SHIFT)
OSGB36_WKT1_SHIFTED = wkt1(4277, towgs84=OSGB_SHIFT)

CASES = [
    ("to ED50", bound(BESSEL, ED50, SHIFT), (10.0, 50.0)),
    ("to ED50 without its code", bound(BESSEL, ED50_NO_ID, SHIFT), (10.0, 50.0)),
    ("to ED50, three parameters", bound(BESSEL, ED50, SHIFT[:3]), (10.0, 50.0)),
    (
        "to ED50, coordinate frame",
        bound(BESSEL, ED50, SHIFT, "Coordinate Frame rotation (geog2D domain)", 9607),
        (10.0, 50.0),
    ),
    ("to ED50 in Spain", bound(BESSEL, ED50, SHIFT), (-3.7, 40.4)),
    ("to OSGB36", bound(BESSEL, OSGB36, SHIFT), (-1.0, 52.0)),
    ("to OSGB36 outside its area", bound(BESSEL, OSGB36, SHIFT), (10.0, 50.0)),
    ("to WGS 84", bound(BESSEL, WGS84, SHIFT), (10.0, 50.0)),
    (
        "over EPSG:4277 to WGS 84",
        bound(OSGB36, WGS84, [375.0, -111.0, 431.0]),
        (-0.1246, 51.5007),
    ),
    ("projected, to ED50", bound(UTM32, ED50, SHIFT), (500000.0, 5540000.0)),
    (
        "over EPSG:4277 to ED50",
        bound(OSGB36, ED50, SHIFT),
        (-1.0, 52.0),
    ),
    (
        "over EPSG:4277 to a datum without transformations",
        bound(OSGB36, LOCAL, SHIFT),
        (-1.0, 52.0),
    ),
    ("to a datum without transformations", bound(BESSEL, LOCAL, SHIFT), (10.0, 50.0)),
    # To the target CRS itself, or one on its datum: the shift alone.
    ("to ED50, into ED50", bound(BESSEL, ED50, SHIFT), (10.0, 50.0), "EPSG:4230"),
    (
        "to ED50 without its code, into ED50",
        bound(BESSEL, ED50_NO_ID, SHIFT),
        (10.0, 50.0),
        "EPSG:4230",
    ),
    (
        "to ED50, into UTM 32 on ED50",
        bound(BESSEL, ED50, SHIFT),
        (10.0, 50.0),
        "EPSG:23032",
    ),
    (
        "over EPSG:4277 to ED50, into ED50",
        bound(OSGB36, ED50, SHIFT),
        (-1.0, 52.0),
        "EPSG:4230",
    ),
    (
        "projected, to ED50, into UTM 32 on ED50",
        bound(UTM32, ED50, SHIFT),
        (500000.0, 5540000.0),
        "EPSG:23032",
    ),
    ("to OSGB36, into OSGB36", bound(BESSEL, OSGB36, SHIFT), (-1.0, 52.0), "EPSG:4277"),
    # Into a CRS on the source CRS's datum: PROJ's null transformation, no shift.
    ("on WGS 84, to NAD83", bound(WGS84, NAD83, [100.0, 0.0, 0.0]), (-103.0, 10.0)),
    ("on ED50, to OSGB36", bound(ED50, OSGB36, SHIFT), (-1.0, 52.0)),
    (
        "on ED50, to OSGB36, into ED50",
        bound(ED50, OSGB36, SHIFT),
        (-1.0, 52.0),
        "EPSG:4230",
    ),
    (
        "on ED50, to OSGB36, into OSGB36",
        bound(ED50, OSGB36, SHIFT),
        (-1.0, 52.0),
        "EPSG:4277",
    ),
    (
        "over EPSG:4277 to WGS 84, into British National Grid",
        bound(OSGB36, WGS84, [375.0, -111.0, 431.0]),
        (-0.1246, 51.5007),
        "EPSG:27700",
    ),
    (
        "on OSGB36 without its code, to WGS 84, into OSGB36",
        bound(OSGB36_NO_ID, WGS84, [375.0, -111.0, 431.0]),
        (-0.1246, 51.5007),
        "EPSG:4277",
    ),
    (
        "projected on OSGB36 by its datum's code, to WGS 84",
        bound(BNG, WGS84, [375.0, -111.0, 431.0]),
        (530000.0, 180000.0),
    ),
    (
        "projected on OSGB36 by its datum's code, to WGS 84, into OSGB36",
        bound(BNG, WGS84, [375.0, -111.0, 431.0]),
        (530000.0, 180000.0),
        "EPSG:4277",
    ),
    # To a CRS without transformations: the source CRS's own.
    (
        "on OSGB36 without its code, to a datum without transformations",
        bound(OSGB36_NO_ID, LOCAL, SHIFT),
        (-1.0, 52.0),
    ),
    # Into a CRS of the target's ellipsoid on another datum, or the source CRS: PROJ's ballpark.
    (
        "to a datum without transformations, into the source CRS",
        bound(BESSEL, LOCAL, SHIFT),
        (10.0, 50.0),
        BESSEL,
    ),
    (
        "to a datum without transformations, into Clarke 1866",
        bound(BESSEL, LOCAL, SHIFT),
        (10.0, 50.0),
        "+proj=longlat +ellps=clrk66 +type=crs",
    ),
    (
        "to OSGB36, into British National Grid",
        bound(BESSEL, OSGB36, SHIFT),
        (-1.0, 52.0),
        "EPSG:27700",
    ),
    (
        "over EPSG:4277 to WGS 84, into OSGB36",
        bound(OSGB36, WGS84, [375.0, -111.0, 431.0]),
        (-0.1246, 51.5007),
        "EPSG:4277",
    ),
    # WKT 1 source CRSs (#1012 item 11): the bound transformation, not the source's TOWGS84.
    (
        "WKT 1 projected source with its own TOWGS84, to WGS 84",
        bound(BNG_WKT1_SHIFTED, WGS84, [375.0, -111.0, 431.0]),
        (530000.0, 180000.0),
    ),
    (
        "WKT 1 projected source with its own TOWGS84, to WGS 84, into OSGB36",
        bound(BNG_WKT1_SHIFTED, WGS84, [375.0, -111.0, 431.0]),
        (530000.0, 180000.0),
        "EPSG:4277",
    ),
    (
        "WKT 1 projected source with its own TOWGS84, seven parameters, to WGS 84",
        bound(BNG_WKT1_SHIFTED, WGS84, SHIFT),
        (530000.0, 180000.0),
    ),
    (
        "WKT 1 projected source with its own TOWGS84, to ED50",
        bound(BNG_WKT1_SHIFTED, ED50, SHIFT),
        (530000.0, 180000.0),
    ),
    (
        "WKT 1 projected source, to WGS 84",
        bound(wkt1(27700), WGS84, [375.0, -111.0, 431.0]),
        (530000.0, 180000.0),
    ),
    (
        "Esri WKT 1 projected source, to WGS 84",
        bound(wkt1(27700, "WKT1_ESRI"), WGS84, [375.0, -111.0, 431.0]),
        (530000.0, 180000.0),
    ),
    (
        "WKT 1 geographic source with its own TOWGS84, to WGS 84",
        bound(OSGB36_WKT1_SHIFTED, WGS84, [375.0, -111.0, 431.0]),
        (-0.1246, 51.5007),
    ),
]
# To a CRS whose transformations to WGS 84 PROJ lists without its ballpark (#1012 item 22): its
# nested search keeps the shift and the ballpark, which places points outside their areas; into
# a CRS whose area none of them reaches, only the ballpark is left, which PROJ excludes: no shift.
DHDN = geog(
    "DHDN", "Deutsches Hauptdreiecksnetz", "Bessel 1841", 6377397.155, 299.1528128, 4314
)
CH1903 = geog("CH1903", "CH1903", "Bessel 1841", 6377397.155, 299.1528128, 4149)
TOKYO = geog("Tokyo", "Tokyo", "Bessel 1841", 6377397.155, 299.1528128, 4301)
NTF = geog(
    "NTF",
    "Nouvelle Triangulation Francaise",
    "Clarke 1880 (IGN)",
    6378249.2,
    293.466021293627,
    4275,
)
CASES += [
    ("to DHDN outside its transformations", bound(BESSEL, DHDN, SHIFT), (14.1, 47.8)),
    (
        "to DHDN far outside its transformations",
        bound(BESSEL, DHDN, SHIFT),
        (-100.0, 40.0),
    ),
    ("to DHDN in Germany", bound(BESSEL, DHDN, SHIFT), (10.0, 51.0)),
    (
        "to CH1903 outside its transformation",
        bound(BESSEL, CH1903, SHIFT),
        (14.1, 47.8),
    ),
    (
        "to Tokyo outside its transformations",
        bound(BESSEL, TOKYO, SHIFT),
        (-100.0, 40.0),
    ),
    ("to NTF outside its transformation", bound(BESSEL, NTF, SHIFT), (14.1, 47.8)),
    (
        "to DHDN outside its transformations, into Pseudo-Mercator",
        bound(BESSEL, DHDN, SHIFT),
        (14.1, 47.8),
        "EPSG:3857",
    ),
    (
        "to DHDN, into UTM 33N",
        bound(BESSEL, DHDN, SHIFT),
        (14.1, 47.8),
        "EPSG:32633",
    ),
    (
        "to CH1903, into UTM 33N",
        bound(BESSEL, CH1903, SHIFT),
        (14.1, 47.8),
        "EPSG:32633",
    ),
    (
        "to OSGB36, into UTM 33N",
        bound(BESSEL, OSGB36, SHIFT),
        (14.1, 47.8),
        "EPSG:32633",
    ),
]
# Into a CRS with transformations of its own (#1012 item 21): PROJ concatenates the shift with
# each of its transformations from the hub CRS (WGS 84 for a `+towgs84` shift, which PROJ reads
# as a bound CRS to WGS 84), each of unknown accuracy, the shift and the ballpark last; inside
# the target's area, a direct transformation from the hub (OSGB36 to ED50 (1)).
TOWGS84 = "+proj=longlat +ellps=bessel +towgs84=598.1,73.7,418.2,0.202,0.045,-2.455,6.7 +type=crs"
CASES += [
    ("+towgs84, into OSGB36 outside its area", TOWGS84, (10.0, 50.0), "EPSG:4277"),
    ("+towgs84, into OSGB36", TOWGS84, (-1.0, 52.0), "EPSG:4277"),
    ("+towgs84, into British National Grid", TOWGS84, (-1.0, 52.0), "EPSG:27700"),
    ("+towgs84, into NAD83", TOWGS84, (-100.0, 40.0), "EPSG:4269"),
    ("to WGS 84, into OSGB36", bound(BESSEL, WGS84, SHIFT), (10.0, 50.0), "EPSG:4277"),
    ("to ED50, into OSGB36", bound(BESSEL, ED50, SHIFT), (-1.0, 52.0), "EPSG:4277"),
    (
        "to ED50, into British National Grid",
        bound(BESSEL, ED50, SHIFT),
        (-1.0, 52.0),
        "EPSG:27700",
    ),
    ("to DHDN, into ETRS89", bound(BESSEL, DHDN, SHIFT), (10.0, 51.0), "EPSG:4258"),
    (
        "to DHDN, into ETRS89 outside its transformations",
        bound(BESSEL, DHDN, SHIFT),
        (7.5, 46.9),
        "EPSG:4258",
    ),
    (
        "to DHDN, into UTM 32N on ETRS89",
        bound(BESSEL, DHDN, SHIFT),
        (10.0, 51.0),
        "EPSG:25832",
    ),
    ("to OSGB36, into ED50", bound(BESSEL, OSGB36, SHIFT), (10.0, 50.0), "EPSG:4230"),
]
# Each Helmert method PROJ applies (proj_constants.h), given by its EPSG code alone: geocentric
# translations (which PROJ applies without the rotations and scale given), position vector and
# coordinate frame in each domain, the time-dependent methods without their rates, coordinate
# frame's full matrix and geog3D to compound variants, and Molodensky-Badekas without an
# evaluation point.
METHOD_CODES = [
    9603,
    1031,
    1035,
    9606,
    1033,
    1037,
    9607,
    1032,
    1038,
    1149,
    1132,
    1133,
    1140,
]
METHOD_CODES += [1053, 1054, 1055, 1056, 1057, 1058, 1034, 1039, 9636, 1061, 1062, 1063]
CASES += [
    (
        f"method EPSG:{code}, to WGS 84",
        bound(BNG, WGS84, SHIFT, "m", code),
        (530000.0, 180000.0),
    )
    for code in METHOD_CODES
]
# Coordinate frame's full matrix, whose rotation PROJ applies exactly: Saba to WGS 84 (1)
# (EPSG:10676), rotations of hundreds of arc-seconds, where the small-angle approximation is 9 m
# away; and coordinate frame itself.
SABA = geog("Saba", "Saba", "International 1924", 6378388, 297)
SABA_SHIFT = [1138.7432, -2064.4761, 110.7016, -214.615206, 479.360036, -164.703951]
SABA_SHIFT += [-402.32073]
CASES += [
    (
        f"Saba, method EPSG:{code}, to WGS 84",
        bound(SABA, WGS84, SABA_SHIFT, "m", code),
        (-63.23, 17.63),
    )
    for code in [1132, 1133, 1140, 9607]
]
# Molodensky-Badekas about an evaluation point, which PROJ applies as its pivot.
EVALUATION_POINT = (
    'PARAMETER["Ordinate 1 of evaluation point",3900000,LENGTHUNIT["metre",1],ID["EPSG",8617]],'
    'PARAMETER["Ordinate 2 of evaluation point",-100000,LENGTHUNIT["metre",1],ID["EPSG",8618]],'
    'PARAMETER["Ordinate 3 of evaluation point",5000000,LENGTHUNIT["metre",1],ID["EPSG",8667]]'
)
CASES += [
    (
        f"method EPSG:{code} about an evaluation point, to WGS 84",
        bound(BNG, WGS84, SHIFT, "m", code)[:-2] + "," + EVALUATION_POINT + "]]",
        (530000.0, 180000.0),
    )
    for code in [9636, 1034, 1063, 1061]
]
# The full matrix and an evaluation point to another CRS (ED50), then its transformations, and
# into ED50 and back.
CASES += [
    (
        f"{what}, to ED50{', into ED50' if dst else ''}",
        wkt,
        point,
        *dst,
    )
    for what, wkt, point in [
        (
            "Saba, method EPSG:1132",
            bound(SABA, ED50, SABA_SHIFT, "m", 1132),
            (-63.23, 17.63),
        ),
        (
            "method EPSG:9636 about an evaluation point",
            bound(BNG, ED50, SHIFT, "m", 9636)[:-2] + "," + EVALUATION_POINT + "]]",
            (530000.0, 180000.0),
        ),
    ]
    for dst in ([], ["EPSG:4230"])
]
# Methods by name alone: an EPSG name, whatever the number of parameters, as its code; another,
# by its convention's words and the number of parameters of its kind.
CASES += [
    (
        f"method by name {name!r}, {len(shift)} parameters, to WGS 84",
        bound(BNG, WGS84, shift, name, None),
        (530000.0, 180000.0),
    )
    for name, shift in [
        ("Position Vector transformation (geog2D domain)", SHIFT[:3]),
        ("Geocentric translations (geog2D domain)", SHIFT),
        ("Molodensky-Badekas (CF geog2D domain)", SHIFT),
        ("Coordinate Frame rotation", SHIFT),
        ("Time-specific Coordinate Frame rotation (geocen)", SHIFT),
        ("Coordinate Frame rotation full matrix (geog2D) (3D)", SHIFT),
        ("Position Vector transformation (geog2D domain) (3D)", SHIFT[:3]),
    ]
]


def main():
    assert pyproj.proj_version_str == "9.9.0", pyproj.proj_version_str
    pyproj.network.set_network_enabled(False)
    # No grids, so the values don't depend on what is installed.
    for d in pyproj.datadir.get_data_dir().split(os.pathsep) + [
        pyproj.datadir.get_user_data_dir()
    ]:
        grids = [
            f
            for f in (os.listdir(d) if os.path.isdir(d) else [])
            if f.endswith((".tif", ".gsb", ".gtx"))
        ]
        assert not grids, (d, grids)
    cases = []
    for name, wkt, point, *dst in CASES:
        dst = dst[0] if dst else "EPSG:4326"
        t = Transformer.from_crs(CRS(wkt), dst, always_xy=True)
        x, y = t.transform(*point, errcheck=True)
        case = {"name": name, "wkt": wkt, "dst": dst, "point": point, "proj": [x, y]}
        if dst != "EPSG:4326":
            # And back, by PROJ's inverse of the Helmert transformation (approximate, as PROJ's
            # is).
            back = Transformer.from_crs(dst, CRS(wkt), always_xy=True)
            case["proj_back"] = list(back.transform(x, y, errcheck=True))
        cases.append(case)
    OUT.write_text(
        json.dumps({"proj_version": pyproj.proj_version_str, "cases": cases}, indent=1)
        + "\n"
    )


if __name__ == "__main__":
    main()
