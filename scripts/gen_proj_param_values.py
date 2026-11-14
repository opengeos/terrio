"""Generate crates/terrio-crs/tests/data/proj_param_values.tsv: whether PROJ 9.8.1 accepts a CRS
given by a PROJ string with a parameter value it may not read.

Each method in the frozen METHODS list gets, in turn, each common parameter
set to `abc` (no number) and to `0.5r` (radians), and the scale factor set to values PROJ may
refuse (0, -1, `abc` and 2). A few strings test `+to_meter`, `+units`, `+vto_meter`, `+towgs84`,
`+pm`, the ellipsoid with `+a`, and geocentric units. Each row is the PROJ string and `OK` or
`ERR`: PROJ's `proj_create` of it with `+type=crs`, on WGS 84 (`+ellps=WGS84`) unless it gives
`+a`, `+rf` or `+R`.

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
    "h_0",
    "t_epoch",
    "dh",
    "zone",
    "azi",
    "b",
    "f",
    "es",
    "e",
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
    "+proj=tmerc +units=ft +to_meter=abc",
    "+proj=eqc +to_meter=nan",
    "+proj=tmerc +k_0=nan",
    "+proj=tmerc +k_0=0.5r +k=0.9996",
    "+proj=tmerc +k_0=0.5r +k=0",
    "+proj=tmerc +k_0=0.5r +k=-1",
    "+proj=tmerc +k_0=0.5r +k=nan",
    "+proj=ob_tran +o_proj=longlat +o_lat_p=abc +o_lon_p=0",
    "+proj=ob_tran +o_proj=longlat +o_lat_p=0.5r +o_lon_p=0",
    "+proj=geocent +units=m +to_meter=abc",
    "+proj=geocent +to_meter=abc",
    "+proj=eqc +a=6378137 +b=abc",
    "+proj=eqc +a=6378137 +f=abc",
    "+proj=eqc +a=6378137 +es=abc",
    "+proj=eqc +a=6378137 +e=abc",
    "+proj=eqc +a=6378137 +rf=abc",
    "+proj=ortho +alpha=abc +towgs84=1,abc,3",
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
    """Add WGS 84 unless the definition supplies its own ellipsoid.

    Args:
        definition: PROJ string to complete.

    Returns:
        A PROJ string with an explicit ellipsoid.
    """
    own = any(f" +{k}=" in definition for k in ("a", "rf", "R"))
    return definition if own else definition + " +ellps=WGS84"


def accepted(definition):
    """Check whether the reference PROJ accepts a definition.

    Args:
        definition: Complete PROJ string to check as a CRS.

    Returns:
        ``OK`` if PROJ accepts the string, otherwise ``ERR``.
    """
    try:
        CRS(definition + " +type=crs")
    except pyproj.exceptions.CRSError:
        return "ERR"
    return "OK"


def main():
    """Regenerate the frozen acceptance fixture using PROJ 9.8.1."""
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
