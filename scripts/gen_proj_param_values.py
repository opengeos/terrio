"""Generate crates/terrio-crs/tests/data/proj_param_values.tsv: whether PROJ 9.8.1 accepts a CRS
given by a PROJ string with a parameter value it may not read.

Each method terrio-crs has without the wbprojection fallback gets, in turn, each common
parameter set to `abc` (no number) and to `0.5r` (radians), and the scale factor set to values
PROJ refuses (0, -1, `abc`, and 2 for `cea`). A few strings test `+to_meter`, `+units`,
`+vto_meter`, `+towgs84` and `+pm`. Each row is the PROJ string and `OK` or `ERR`: PROJ's
`proj_create` of it with `+type=crs`, on WGS 84 (`+ellps=WGS84`) unless it gives `+a`, `+rf` or
`+R`.

Usage: PROJ_NETWORK=OFF python scripts/gen_proj_param_values.py (needs pyproj 3.8.0)
"""

import itertools
from pathlib import Path

import pyproj
from pyproj import CRS

OUT = (
    Path(__file__).resolve().parents[1]
    / "crates/terrio-crs/tests/data/proj_param_values.tsv"
)
METHODS = [
    "aea",
    "aeqd",
    "bonne",
    "cass",
    "cea",
    "col_urban",
    "eck4",
    "eqc",
    "eqdc",
    "eqearth",
    "etmerc",
    "fouc_s",
    "gall",
    "geos",
    "gnom",
    "gstmerc",
    "healpix",
    "igh",
    "isea",
    "krovak",
    "labrd",
    "laea",
    "lcc",
    "mbtfps",
    "merc",
    "mill",
    "mod_krovak",
    "moll",
    "murd2",
    "natearth",
    "nsper",
    "nzmg",
    "omerc",
    "ortho",
    "poly",
    "putp4p",
    "qsc",
    "rhealpix",
    "robin",
    "sinu",
    "somerc",
    "stere",
    "sterea",
    "tissot",
    "tmerc",
    "tpers",
    "ups",
    "utm",
    "vandg",
    "wag4",
    "wag5",
    "webmerc",
    "weren",
    "wink1",
    "wink2",
    "wintri",
    "longlat",
    "geocent",
]
# The parameters a method needs, with valid values.
BASE = {
    "aea": "+lat_1=30 +lat_2=50",
    "eqdc": "+lat_1=30 +lat_2=50",
    "lcc": "+lat_1=30 +lat_2=50",
    "tissot": "+lat_1=30 +lat_2=50",
    "murd2": "+lat_1=30 +lat_2=50",
    "bonne": "+lat_1=40",
    "geos": "+h=35785831",
    "nsper": "+h=3000000",
    "tpers": "+h=3000000",
    "omerc": "+lat_0=40 +lonc=10 +alpha=30",
    "utm": "+zone=32",
    "col_urban": "+h=200 +lat_0=4.68 +lon_0=-74.14",
    "labrd": "+lat_0=-18.9 +lon_0=46.4 +azi=18.9",
}
PARAMS = [
    "lat_0",
    "lon_0",
    "lat_ts",
    "lat_1",
    "lat_2",
    "k_0",
    "x_0",
    "y_0",
    "alpha",
    "gamma",
    "lonc",
    "h",
    "zone",
    "azi",
    "pm",
    "a",
    "rf",
    "R",
]
EXTRA = [
    "+proj=eqc +to_meter=0",
    "+proj=eqc +to_meter=-1",
    "+proj=eqc +to_meter=abc",
    "+proj=eqc +to_meter=0.5",
    "+proj=eqc +units=m +to_meter=0",
    "+proj=eqc +units=ft +to_meter=abc",
    "+proj=longlat +to_meter=0",
    "+proj=eqc +geoidgrids=x.gtx +vto_meter=0",
    "+proj=eqc +vunits=m +vto_meter=0",
    "+proj=longlat +towgs84=1,abc,3",
    "+proj=longlat +pm=abc",
    "+proj=longlat +pm=paris",
    "+proj=eqc +lat_ts=0.5r",
    "+proj=eqc +lat_ts=0.5R",
    "+proj=eqc +lat_ts=30d30r",
    "+proj=tmerc +lon_0=0.2R",
    "+proj=eqc +lat_ts=10.5.5",
    "+proj=merc +lat_ts=",
    "+proj=eqc +lat_ts",
    "+proj=stere +lat_0=90 +lat_ts=abc",
    "+proj=stere +lat_0=-90 +lat_ts=abc",
    "+proj=stere +lat_0=90 +lat_ts=0.5r",
    "+proj=cea +lat_ts=10 +k_0=2",
]


def with_ellipsoid(definition):
    """The definition on WGS 84, unless it gives its own ellipsoid (`+a`, `+rf`, `+R`)."""
    own = any(f" +{k}=" in definition for k in ("a", "rf", "R"))
    return definition if own else definition + " +ellps=WGS84"


def accepted(definition):
    try:
        CRS(definition + " +type=crs")
    except pyproj.exceptions.CRSError:
        return "ERR"
    return "OK"


def main():
    assert pyproj.proj_version_str == "9.8.1", pyproj.proj_version_str
    rows = []
    for method, key, value in itertools.product(METHODS, PARAMS, ["abc", "0.5r"]):
        given = [
            t for t in BASE.get(method, "").split() if not t.startswith(f"+{key}=")
        ]
        rows.append(" ".join([f"+proj={method}", *given, f"+{key}={value}"]))
    for method, key, value in itertools.product(
        METHODS, ["k_0", "k"], ["0", "-1", "abc", "2"]
    ):
        rows.append(
            " ".join(
                [
                    f"+proj={method}",
                    *BASE.get(method, "").split(),
                    f"+{key}={value}",
                ]
            )
        )
    rows += EXTRA
    OUT.write_text("".join(f"{d}\t{accepted(with_ellipsoid(d))}\n" for d in rows))


if __name__ == "__main__":
    main()
