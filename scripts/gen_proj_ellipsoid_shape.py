# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT

"""Generate crates/terrio-crs/tests/data/proj_ellipsoid_shape.json: the ellipsoid PROJ 9.9.0
gives a CRS defined by a PROJ string with `+R`, `+ellps`, `+datum`, `+a`, `+b`, `+rf`, `+f`,
`+es`, `+e` and the spherification flags (`+R_A`, `+R_V`, `+R_a`, `+R_g`, `+R_h`, `+R_lat_a`,
`+R_lat_g`, `+R_C`), alone and combined, and where it places a point with that ellipsoid.

Each method of METHODS gets each ellipsoid definition of SHAPES, and `ortho` and `mill` a few
more of their own (EXTRA). A case holds the string (without `+type=crs`), the point, and either
PROJ's coordinates or null where PROJ refuses the string, with the semi-major and semi-minor axes
of the CRS's ellipsoid (`CRS.ellipsoid`). Projected and geocentric CRSs are reached from
EPSG:4326 (`Transformer.from_crs("EPSG:4326", CRS(s + " +type=crs"), always_xy=True)`);
a geographic CRS, given a null `+towgs84` so that its ellipsoid matters, is transformed to
EPSG:4326. Points are at a height of 0; a geocentric CRS's coordinates have three values.
`extensions` holds WKT 1 CRSs whose `PROJ4` extension has such parameters, with PROJ's
coordinates (`Transformer.from_crs("EPSG:4326", CRS(wkt), always_xy=True)`), or null where PROJ
refuses to transform. `written` holds PROJ strings whose WKT terrio writes with a normalized
`PROJ4` extension, the extension, and PROJ's coordinates of that WKT, which the script checks
against PROJ's coordinates of the string.

Usage: PROJ_NETWORK=OFF python scripts/gen_proj_ellipsoid_shape.py (needs pyproj 3.8.0)
"""

import json
import math
import os
from pathlib import Path

import pyproj
import pyproj.datadir
import pyproj.network
from pyproj import CRS, Transformer

OUT = (
    Path(__file__).resolve().parents[1]
    / "crates/terrio-crs/tests/data/proj_ellipsoid_shape.json"
)
POINT = [-80.0, 30.0]
METHODS = [
    "+proj=laea +lat_0=45 +lon_0=-100",
    "+proj=tmerc +lon_0=-75",
    "+proj=merc",
    "+proj=ortho +lat_0=40 +lon_0=-90",
    "+proj=geocent",
    "+proj=longlat +towgs84=0,0,0",
]
SHAPES = [
    # A named ellipsoid and a shape parameter.
    "+ellps=WGS84",
    "+ellps=WGS84 +f=0",
    "+ellps=WGS84 +f=0.01",
    "+ellps=WGS84 +es=0.01",
    "+ellps=WGS84 +e=0.1",
    "+ellps=WGS84 +rf=300",
    "+ellps=WGS84 +b=6350000",
    "+ellps=WGS84 +a=6400000",
    "+ellps=WGS84 +a=6400000 +f=0",
    "+ellps=WGS84 +a=6400000 +rf=300",
    "+ellps=WGS84 +a=6400000 +es=0.01 +b=6300000",
    # Ellipsoids PROJ defines by their semi-minor axis.
    "+ellps=clrk66 +a=6400000",
    "+ellps=clrk66 +rf=300",
    "+ellps=clrk66 +f=0",
    "+ellps=clrk66 +b=6350000",
    "+ellps=sphere +a=6400000",
    "+ellps=sphere +rf=300",
    # A datum.
    "+datum=WGS84 +f=0",
    "+datum=WGS84 +es=0.01",
    "+datum=WGS84 +a=6400000",
    "+datum=WGS84 +rf=300",
    "+datum=WGS84 +b=6350000",
    "+datum=WGS84 +ellps=clrk66",
    "+datum=NAD27 +a=6400000",
    "+datum=NAD27 +rf=300",
    # A semi-major axis and shape parameters.
    "+a=6400000",
    "+a=6400000 +f=0",
    "+a=6400000 +f=0.01",
    "+a=6400000 +es=0.01",
    "+a=6400000 +e=0.1",
    "+a=6400000 +rf=300",
    "+a=6400000 +b=6350000",
    "+a=6400000 +rf=300 +b=6350000",
    "+a=6400000 +f=0.01 +b=6350000",
    "+a=6400000 +es=0.01 +b=6350000",
    "+a=6400000 +e=0.1 +b=6350000",
    "+a=6400000 +es=0.01 +e=0.2",
    "+a=6400000 +f=0.01 +rf=300",
    "+a=6400000 +f=0.01 +es=0.02",
    # A radius.
    "+R=6400000",
    "+R=6400000 +ellps=WGS84",
    "+R=6400000 +rf=300",
    "+R=6400000 +a=6300000",
    "+R=6400000 +R_A",
    # Spherification.
    "+ellps=WGS84 +R_A",
    "+ellps=WGS84 +f=0 +R_A",
    "+ellps=WGS84 +rf=300 +R_A",
    "+ellps=clrk66 +a=6400000 +R_A",
    "+datum=WGS84 +a=6400000 +R_A",
    "+a=6400000 +rf=300 +R_A",
    "+ellps=WGS84 +R_V",
    "+ellps=WGS84 +R_a",
    "+ellps=WGS84 +R_g",
    "+ellps=WGS84 +R_h",
    "+ellps=WGS84 +R_lat_a=40",
    "+ellps=WGS84 +R_lat_g=40",
    "+ellps=WGS84 +R_V +R_A",
    "+ellps=WGS84 +R_g +R_V",
    "+ellps=WGS84 +f=0 +R_V",
    "+ellps=WGS84 +es=0.01 +R_V",
    "+ellps=clrk66 +a=6400000 +R_V",
    "+ellps=clrk66 +rf=300 +R_h",
    "+datum=WGS84 +ellps=clrk66 +R_V",
    "+datum=WGS84 +a=6400000 +R_g",
    "+a=6400000 +es=0.01 +b=6300000 +R_V",
    "+R_V",
    "+ellps=WGS84 +R_C",
    "+datum=WGS84 +R_V",
    "+datum=NAD83 +R_g",
    # A shape without a size, which PROJ refuses.
    "+rf=300",
    "+f=0.01",
    "+es=0.01",
    "+e=0.1",
    "+b=6350000",
    "+rf=300 +R_V",
]
EXTRA = [
    # Orthographic on a sphere: `+f=0`, written so, not another zero.
    "+proj=ortho +lat_0=40 +lon_0=-90 +ellps=WGS84 +f=0.0",
    "+proj=ortho +lat_0=40 +lon_0=-90 +ellps=clrk66 +f=0",
    "+proj=ortho +lat_0=40 +lon_0=-90 +a=6400000 +rf=300 +f=0",
    "+proj=ortho +lat_0=40 +lon_0=-90 +a=6400000 +b=6350000 +f=0",
    "+proj=ortho +lat_0=40 +lon_0=-90 +a=6378137 +rf=298.257223563 +f=0",
    "+proj=ortho +lat_0=40 +lon_0=-90 +datum=NAD27 +a=6400000 +f=0",
    "+proj=ortho +lat_0=40 +lon_0=-90 +ellps=WGS84 +f=0 +R_V",
    # Miller is on the authalic sphere.
    "+proj=mill +ellps=WGS84",
    "+proj=mill +ellps=WGS84 +f=0",
    "+proj=mill +ellps=clrk66 +a=6400000",
    "+proj=cea +ellps=WGS84 +f=0 +R_A",
    "+proj=cea +ellps=WGS84 +f=0",
    # `+R_A` given for Miller or van der Grinten, which PROJ's parser does not take: a step.
    "+proj=mill +datum=WGS84 +R_A",
    "+proj=mill +ellps=WGS84 +f=0 +R_A",
    "+proj=vandg +datum=WGS84 +R_A",
    "+proj=vandg +ellps=WGS84 +f=0 +R_A",
    "+proj=cea +datum=WGS84 +R_A",
    "+proj=laea +lat_0=45 +lon_0=-100 +datum=WGS84 +R_A",
    # The latitude of `+R_lat_a` and `+R_lat_g` as PROJ's `dmstor` reads it: an unreadable one
    # is the equator, beyond 90 degrees it is refused.
    "+proj=tmerc +lon_0=-75 +ellps=WGS84 +R_lat_a=foo",
    "+proj=tmerc +lon_0=-75 +ellps=WGS84 +R_lat_g=foo",
    "+proj=tmerc +lon_0=-75 +ellps=WGS84 +R_lat_a=0.5r",
    "+proj=tmerc +lon_0=-75 +ellps=WGS84 +R_lat_g=40d30",
    "+proj=tmerc +lon_0=-75 +ellps=WGS84 +R_lat_a=91",
    # A datum beside `+a`, even of its own semi-major axis, is dropped: no NAD27 shift.
    "+proj=longlat +datum=NAD27 +a=6378206.4",
    "+proj=longlat +datum=NAD27 +rf=294.978698213898",
    # `+nadgrids=@null` wins over `+towgs84`: no shift.
    "+proj=longlat +ellps=WGS84 +nadgrids=@null +towgs84=1000,0,0",
    "+proj=tmerc +lon_0=-75 +ellps=WGS84 +nadgrids=@null +towgs84=1000,0,0",
    "+proj=eqc +ellps=sphere +a=6370000",
    # A step's `+datum=` is shifted as PROJ's `pj_datum_set` gives it, but under `+R_C`.
    "+proj=merc +datum=carthage +R_V",
    "+proj=merc +datum=NAD27 +R_V",
    "+proj=merc +datum=potsdam +R_V",
    "+proj=merc +datum=OSGB36 +R_g",
    "+proj=merc +datum=WGS84 +a=6400000 +R_C",
    "+proj=merc +datum=NAD83 +R_C",
]

# WKT 1 CRSs with a `PROJ4` extension, which PROJ computes on as a pipeline step reads it, whatever
# the nodes say: a shape parameter changes a named ellipsoid, `+a` keeps its eccentricity, GRS
# 1980 without an ellipsoid, `+R_a` is not `+R_A`, `+datum=` is `pj_datum_set`'s shift. The extension's
# ellipsoid is refused where the step refuses it (PROJ creates the CRS, but not the
# transformation).
EXTENSION_WKT = (
    'PROJCS["custom",GEOGCS["custom",DATUM["custom",SPHEROID["WGS 84",6378137,298.257223563]],'
    'PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],'
    'PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",0],'
    'PARAMETER["central_meridian",-75],PARAMETER["scale_factor",1],'
    'PARAMETER["false_easting",0],PARAMETER["false_northing",0],UNIT["metre",1],'
    'EXTENSION["PROJ4","{}"]]'
)
EXTENSIONS = [
    "+proj=tmerc +lon_0=-75 +ellps=WGS84",
    "+proj=tmerc +lon_0=-75 +ellps=WGS84 +f=0",
    "+proj=tmerc +lon_0=-75 +ellps=WGS84 +f=abc",
    "+proj=tmerc +lon_0=-75 +ellps=WGS84 +es=0.01",
    "+proj=tmerc +lon_0=-75 +ellps=WGS84 +rf=300",
    "+proj=tmerc +lon_0=-75 +ellps=clrk66 +a=6400000",
    "+proj=tmerc +lon_0=-75 +ellps=WGS84 +R_V",
    "+proj=tmerc +lon_0=-75 +datum=WGS84 +a=6400000",
    "+proj=tmerc +lon_0=-75 +datum=NAD27 +rf=300",
    "+proj=tmerc +lon_0=-75 +a=6400000 +es=0.01 +b=6300000",
    "+proj=tmerc +lon_0=-75 +ellps=WGS84 +rf=abc",
    "+proj=tmerc +lon_0=-75 +ellps=WGS84 +rf=0",
    "+proj=tmerc +lon_0=-75 +ellps=WGS84 +es=1",
    "+proj=tmerc +lon_0=-75 +a=6400000 +rf=abc",
    "+proj=tmerc +lon_0=-75 +rf=300",
    "+proj=tmerc +lon_0=-75",
    "+proj=tmerc +lon_0=-75 +datum=carthage",
    "+proj=tmerc +lon_0=-75 +datum=WGS84 +a=6400000 +R_C",
    "+proj=laea +lat_0=45 +lon_0=-100 +datum=WGS84 +R_A",
    "+proj=laea +lat_0=45 +lon_0=-100 +ellps=WGS84 +R_a",
    # `+R_C` beside a latitude of origin, and the datum's grids over the string's `+towgs84`.
    "+proj=tmerc +lat_0=45 +lon_0=-75 +datum=WGS84 +a=6400000 +R_C",
    "+proj=lcc +lat_1=30 +lat_2=60 +lat_0=45 +lon_0=-75 +datum=WGS84 +a=6400000 +R_C",
    "+proj=merc +lat_ts=45 +lon_0=-75 +datum=WGS84 +a=6400000 +R_C",
    "+proj=tmerc +lon_0=-75 +datum=NAD27 +towgs84=-8,160,176",
    "+proj=tmerc +lon_0=-75 +datum=carthage +towgs84=-263,6,431",
    # `+a` on a named sphere, which the CRS parser refuses (its semi-minor axis above `+a`) and
    # the step reads as a sphere of that radius.
    "+proj=eqc +ellps=sphere +a=6370000",
    # `+nadgrids=@null` wins over `+towgs84`: no shift.
    "+proj=tmerc +lon_0=-75 +ellps=WGS84 +nadgrids=@null +towgs84=1000,0,0",
]

GEOGRAPHIC_EXTENSION_WKT = (
    'GEOGCS["custom",DATUM["custom",SPHEROID["WGS 84",6378137,298.257223563]],'
    'PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433],EXTENSION["PROJ4","{}"]]'
)
# PROJ strings whose WKT 1 terrio writes with a `PROJ4` extension that a pipeline step reads
# otherwise than PROJ's CRS parser reads the string, and the extension terrio writes in its place:
# the CRS's figure and datum, so that PROJ (as terrio) reads the WKT as the string.
WRITTEN = [
    (
        "+proj=laea +lat_0=45 +lon_0=-100 +ellps=WGS84 +f=0 +R_A",
        "+proj=laea +lat_0=45 +lon_0=-100 +R=6371007.181082429",
    ),
    (
        "+proj=laea +lat_0=45 +lon_0=-100 +datum=WGS84 +a=6400000 +R_A",
        "+proj=laea +lat_0=45 +lon_0=-100 +R=6392845.741464561",
    ),
    (
        "+proj=laea +lat_0=45 +lon_0=-100 +ellps=WGS84 +R_a",
        "+proj=laea +lat_0=45 +lon_0=-100 +R=6371007.181082429",
    ),
    ("+proj=cea +ellps=WGS84 +f=0 +R_A", "+proj=cea +R=6371007.181082429"),
    (
        "+proj=ortho +lat_0=40 +lon_0=-90 +datum=WGS84 +f=0",
        "+proj=ortho +lat_0=40 +lon_0=-90 +R=6378137",
    ),
    (
        "+proj=longlat +towgs84=0,0,0 +ellps=clrk66 +a=6400000",
        "+proj=longlat +towgs84=0,0,0 +a=6400000 +rf=147.41041362440728",
    ),
    (
        "+proj=longlat +towgs84=0,0,0 +datum=WGS84 +ellps=clrk66",
        "+proj=longlat +towgs84=0,0,0 +ellps=WGS84",
    ),
    (
        "+proj=merc +datum=NAD83 +R_C",
        "+proj=merc +ellps=GRS80 +R_C",
    ),
    (
        "+proj=merc +datum=WGS84 +a=6400000 +R_C",
        "+proj=merc +a=6400000 +rf=298.25722356300156 +R_C",
    ),
]


def written_case(definition, extension):
    template = (
        GEOGRAPHIC_EXTENSION_WKT if "+proj=longlat" in definition else EXTENSION_WKT
    )
    wkt = template.format(extension)
    original = case(definition)["proj"]
    crs = CRS(wkt)
    if crs.is_geographic:
        t = Transformer.from_crs(crs, "EPSG:4326", always_xy=True)
    else:
        t = Transformer.from_crs("EPSG:4326", crs, always_xy=True)
    proj = list(t.transform(*POINT, errcheck=True))
    # PROJ reads the WKT as it reads the string, within 1 mm (degrees in metres for a
    # geographic CRS, as tests/proj_ellipsoid_shape.rs measures them).
    if crs.is_geographic:
        dx = (proj[0] - original[0]) * math.cos(math.radians(original[1])) * 111_195.0
        dy = (proj[1] - original[1]) * 111_195.0
    else:
        dx, dy = proj[0] - original[0], proj[1] - original[1]
    assert math.hypot(dx, dy) < 1e-3, (definition, proj, original)
    return {"proj_string": definition, "extension": extension, "wkt": wkt, "proj": proj}


def extension_case(definition):
    wkt = EXTENSION_WKT.format(definition)
    try:
        t = Transformer.from_crs("EPSG:4326", CRS(wkt), always_xy=True)
        x, y = t.transform(*POINT, errcheck=True)
    except pyproj.exceptions.ProjError:
        return {"wkt": wkt, "point": POINT, "proj": None}
    return {"wkt": wkt, "point": POINT, "proj": [x, y]}


def case(definition):
    crs = None
    try:
        crs = CRS(definition + " +type=crs")
        if crs.is_geographic:
            t = Transformer.from_crs(crs, "EPSG:4326", always_xy=True)
        else:
            t = Transformer.from_crs("EPSG:4326", crs, always_xy=True)
        x, y, z = t.transform(*POINT, 0.0, errcheck=True)
    except pyproj.exceptions.ProjError:
        return {"proj_string": definition, "point": POINT, "proj": None}
    e = crs.ellipsoid
    return {
        "proj_string": definition,
        "point": POINT,
        "proj": [x, y, z] if crs.is_geocentric else [x, y],
        "ellipsoid": [e.semi_major_metre, e.semi_minor_metre],
    }


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
    definitions = [f"{m} {s}" for m in METHODS for s in SHAPES] + EXTRA
    cases = [case(d) for d in definitions]
    extensions = [extension_case(s) for s in EXTENSIONS]
    written = [written_case(d, e) for d, e in WRITTEN]
    OUT.write_text(
        json.dumps(
            {
                "proj_version": pyproj.proj_version_str,
                "cases": cases,
                "extensions": extensions,
                "written": written,
            },
            indent=1,
        )
        + "\n"
    )


if __name__ == "__main__":
    main()
