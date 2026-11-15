"""Generate crates/terrio-crs/tests/data/wkt2_bound.json: where PROJ 9.8.1 places points of WKT 2
`BOUNDCRS`s with a Helmert transformation, to WGS 84 and to other CRSs (ED50, OSGB36), whose own
transformations to WGS 84 PROJ then applies.

Each case is a bound CRS, a point in it, and the point in WGS 84, or in the
target CRS's own system, by `Transformer.from_crs(wkt, dst, always_xy=True)`, and for the
latter, PROJ's way back. The scale difference is written as PROJ
writes it, a ratio in unity (`1.0000067` for 6.7 ppm).

Usage: PROJ_NETWORK=OFF python scripts/gen_wkt2_bound.py (needs pyproj 3.8.0)
"""

import json
import os
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
    return (
        f'BOUNDCRS[SOURCECRS[{source}],TARGETCRS[{target}],ABRIDGEDTRANSFORMATION["t",'
        f'METHOD["{method}",ID["EPSG",{code}]],{",".join(params)}]]'
    )


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
]


def main():
    assert pyproj.proj_version_str == "9.8.1", pyproj.proj_version_str
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
