# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT

"""Generate crates/terrio-crs/tests/data/proj_ellipsoid_shape.json: the ellipsoid PROJ 9.8.1
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

Usage: PROJ_NETWORK=OFF python scripts/gen_proj_ellipsoid_shape.py (needs pyproj 3.8.0)
"""

import json
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
    # Miller is on the authalic sphere, with or without `+R_A`.
    "+proj=mill +ellps=WGS84",
    "+proj=mill +ellps=WGS84 +f=0",
    "+proj=mill +ellps=clrk66 +a=6400000",
    "+proj=cea +ellps=WGS84 +f=0 +R_A",
    "+proj=cea +ellps=WGS84 +f=0",
]


def case(definition):
    crs = None
    try:
        crs = CRS(definition + " +type=crs")
        if crs.is_geographic:
            t = Transformer.from_crs(crs, "EPSG:4326", always_xy=True)
        else:
            t = Transformer.from_crs("EPSG:4326", crs, always_xy=True)
        x, y, z = t.transform(*POINT, 0.0, errcheck=True)
    except pyproj.exceptions.CRSError:
        return {"proj_string": definition, "point": POINT, "proj": None}
    e = crs.ellipsoid
    return {
        "proj_string": definition,
        "point": POINT,
        "proj": [x, y, z] if crs.is_geocentric else [x, y],
        "ellipsoid": [e.semi_major_metre, e.semi_minor_metre],
    }


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
    definitions = [f"{m} {s}" for m in METHODS for s in SHAPES] + EXTRA
    cases = [case(d) for d in definitions]
    OUT.write_text(
        json.dumps({"proj_version": pyproj.proj_version_str, "cases": cases}, indent=1)
        + "\n"
    )


if __name__ == "__main__":
    main()
