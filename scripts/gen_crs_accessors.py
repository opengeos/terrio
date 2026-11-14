"""Freeze PROJ's axis orders, celestial bodies and conversion parameters of CRSs for
``SpatialRef::axis_order``, ``body`` and ``projection_params`` (``tests/crs_accessors.rs``).

PROJ 9.8.1 (pyproj 3.8.0, network off) reads each CRS:

- the axis order is the direction of its first two axes (``CRS.axis_info``): ``EN`` for east then
  north, ``NE`` for north then east, ``OTHER`` for any other;
- the body is ``proj_get_celestial_body_name`` of its ellipsoid, called through pyproj's own
  libproj, which pyproj does not expose: ``-`` for PROJ's ``Non-Earth body``, ``?`` for a CRS
  without an ellipsoid (a vertical CRS);
- the parameters are those of the PROJ string PROJ writes (``CRS.to_proj4()``), with PROJ's
  defaults, and UTM's and UPS's from the zone and the hemisphere; the prime meridian and the
  ellipsoid are those PROJ reports for that string (the registry's for a code), in degrees and
  metres; metres per unit is 1 for an angular CRS. ``NONE`` when PROJ has no PROJ string.

The rows are every 20th EPSG and ESRI code, the codes behind each rule, custom PROJ strings and
WKT, compound references, every 20th IAU_2015 CRS and a CRS of every IAU_2015 body.

Usage: python scripts/gen_crs_accessors.py (needs pyproj 3.8.0)

``--all PATH`` writes every EPSG, ESRI and IAU_2015 code instead, to PATH (not committed), for the
whole-table comparison: ``TERRIO_CRS_ACCESSORS_ALL=PATH cargo test -p terrio-crs --test
crs_accessors -- --ignored``.
"""

import argparse
import ctypes
import glob
import math
import os
import re
import warnings
from pathlib import Path

import pyproj
from pyproj import CRS
from pyproj.database import query_crs_info

OUT = (
    Path(__file__).resolve().parents[1]
    / "crates/terrio-crs/tests/data/crs_accessors.tsv"
)

# The codes behind each rule: axis orders (geographic, northing/easting, polar, geocentric,
# vertical, compound), prime meridians in grads, units, UTM and UPS, Web Mercator on WGS 84,
# ESRI's spherical orthographic CRSs, ESRI's spheres and Polar Stereographic (variant C).
RULES = [
    ("EPSG", c)
    for c in (
        4326,
        4258,
        4807,
        27572,
        2193,
        3346,
        32633,
        32761,
        32661,
        5041,
        3413,
        3031,
        4978,
        4979,
        5773,
        9707,
        3857,
        2985,
        2986,
        2056,
        5514,
        2154,
        26917,
        2263,
        3035,
        6933,
    )
] + [("ESRI", c) for c in (102035, 102037, 102100, 53002, 54009, 54030, 102003)]

PROJ_STRINGS = [
    "+proj=longlat +datum=WGS84",
    "+proj=longlat +ellps=GRS80 +axis=neu",
    "+proj=latlong +datum=NAD27",
    "+proj=lonlat +datum=WGS84",
    "+proj=longlat +a=6378000 +b=6370000",
    "+proj=longlat +R=3396190",
    "+proj=longlat +ellps=bessel +pm=paris",
    "+proj=longlat +datum=WGS84 +pm=2d20'14.025\"E",
    "+proj=utm +zone=33 +datum=WGS84",
    "+proj=utm +zone=56 +south +ellps=GRS80",
    "+proj=ups +datum=WGS84",
    "+proj=ups +south +datum=WGS84",
    "+proj=tmerc +lat_0=49 +lon_0=-2 +k=0.9996012717 +x_0=400000 +y_0=-100000 +ellps=airy +units=m",
    "+proj=tmerc +lon_0=9 +x_0=500000 +datum=WGS84 +axis=neu",
    "+proj=tmerc +lat_0=40d30'N +lon_0=3d15'W +datum=WGS84",
    "+proj=lcc +lat_1=33 +lat_2=45 +lat_0=39 +lon_0=-96 +x_0=0 +y_0=0 +datum=NAD83 +units=us-ft",
    "+proj=lcc +lat_1=46.8 +lat_0=46.8 +lon_0=0 +k_0=0.99987742 +x_0=600000 +y_0=2200000 +ellps=clrk80ign +pm=paris +units=m",
    "+proj=merc +lat_ts=30 +lon_0=100 +datum=WGS84",
    "+proj=merc +a=6378137 +b=6378137 +nadgrids=@null +wktext",
    "+proj=somerc +lat_0=46.95240555555556 +lon_0=7.439583333333333 +k_0=1 +x_0=2600000 +y_0=1200000 +ellps=bessel +towgs84=674.374,15.056,405.346,0,0,0,0",
    "+proj=laea +lat_0=52 +lon_0=10 +x_0=4321000 +y_0=3210000 +ellps=GRS80",
    "+proj=laea +R_A +ellps=GRS80 +lat_0=45 +lon_0=-100",
    "+proj=merc +R_C +ellps=GRS80 +lat_0=10",
    "+proj=ortho +f=0 +datum=WGS84 +lat_0=90",
    "+proj=tmerc +rf=300 +datum=WGS84",
    "+proj=tmerc +es=0.006 +ellps=GRS80",
    "+proj=tmerc +b=6356000 +ellps=GRS80",
    "+proj=stere +lat_0=90 +lat_ts=70 +lon_0=-45 +x_0=0 +y_0=0 +datum=WGS84",
    "+proj=stere +lat_0=-90 +lat_ts=-71 +datum=WGS84",
    "+proj=stere +lat_0=90 +lat_ts=70 +datum=WGS84 +axis=neu",
    "+proj=sterea +lat_0=90 +datum=WGS84",
    "+proj=robin +lon_0=150 +R=6371000",
    "+proj=natearth +datum=WGS84",
    "+proj=eck4 +lon_0=-100 +x_0=1000 +y_0=-2000 +R=6400000",
    "+proj=moll +lon_0=10 +ellps=WGS84 +units=km",
    "+proj=omerc +lat_0=4 +lonc=102.25 +alpha=323.0257905 +k=0.99984 +x_0=804671 +y_0=0 +ellps=evrstSS +no_uoff +gamma=323.1301023611111",
    "+proj=krovak +lat_0=49.5 +lon_0=24.83333333333333 +alpha=30.28813972222222 +k=0.9999 +x_0=0 +y_0=0 +ellps=bessel",
    "+proj=geocent +datum=WGS84",
    "+proj=ob_tran +o_proj=longlat +o_lat_p=40 +o_lon_p=-170 +lon_0=0 +R=6371229",
    "+proj=aea +lat_1=29.5 +lat_2=45.5 +lat_0=23 +lon_0=-96 +x_0=0 +y_0=0 +datum=NAD83 +units=ft",
    "+proj=cass +lat_0=10.44166666666667 +lon_0=-61.33333333333334 +x_0=86501.46392052001 +y_0=65379.0134283 +a=6378293.645208759 +b=6356617.987679838 +to_meter=0.201166195164",
    "+proj=eqc +lat_ts=0 +lat_0=0 +lon_0=0 +x_0=0 +y_0=0 +R=3396190",
    "+proj=sinu +lon_0=0 +x_0=0 +y_0=0 +R=1737400",
    "+proj=eqearth +datum=WGS84",
    # Aliases PROJ writes under another method's name.
    "+proj=etmerc +lon_0=9 +datum=WGS84",
    "+proj=webmerc +datum=WGS84",
]

# GDAL's WKT 1, WKT 2 and ESRI's WKT of these codes (as far as PROJ writes them).
WKT_CODES = [
    "EPSG:2056",
    "EPSG:4326",
    "EPSG:27572",
    "EPSG:4807",
    "EPSG:3035",
    "EPSG:4978",
    "EPSG:32761",
    "EPSG:2193",
    "EPSG:3346",
    "EPSG:3413",
    "EPSG:4258",
    "EPSG:3857",
    "EPSG:2154",
    "ESRI:54009",
    "ESRI:102035",
]

WKTS = [
    # Without axes: a geographic CRS of no code, of an EPSG code, and a projected CRS.
    'GEOGCS["x",DATUM["d",SPHEROID["s",6378137,298.257223563]],PRIMEM["G",0],UNIT["degree",0.0174532925199433]]',
    'GEOGCS["x",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],'
    'UNIT["degree",0.0174532925199433],AUTHORITY["EPSG","4326"]]',
    'PROJCS["p",GEOGCS["x",DATUM["d",SPHEROID["s",6378137,298.257223563]],PRIMEM["G",0],'
    'UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["central_meridian",9],'
    'PARAMETER["scale_factor",0.9996],PARAMETER["false_easting",500000],UNIT["metre",1]]',
    # Northing then easting.
    'PROJCS["p",GEOGCS["x",DATUM["d",SPHEROID["s",6378137,298.257223563]],PRIMEM["G",0],'
    'UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["central_meridian",9],'
    'UNIT["metre",1],AXIS["Northing",NORTH],AXIS["Easting",EAST]]',
    # ESRI's vertical CRS.
    'VERTCS["NAVD_1988",VDATUM["North_American_Vertical_Datum_1988"],PARAMETER["Vertical_Shift",0.0],'
    'PARAMETER["Direction",1.0],UNIT["Meter",1.0]]',
]

REFERENCES = ["EPSG:4326+5773", "EPSG:2056+5728", "EPSG:32632+5773"]


class Proj:
    """``proj_get_celestial_body_name`` through pyproj's own libproj."""

    def __init__(self):
        libs = os.path.join(
            os.path.dirname(pyproj.__file__), "..", "pyproj.libs", "libproj-*.so*"
        )
        found = glob.glob(libs)
        if not found:
            raise SystemExit(
                "needs a Linux pyproj 3.8.0 wheel, which bundles libproj in pyproj.libs"
            )
        lib = ctypes.CDLL(found[0])
        p, s = ctypes.c_void_p, ctypes.c_char_p
        for name, res, args in [
            ("proj_context_create", p, []),
            ("proj_context_set_enable_network", ctypes.c_int, [p, ctypes.c_int]),
            (
                "proj_context_set_search_paths",
                None,
                [p, ctypes.c_int, ctypes.POINTER(s)],
            ),
            ("proj_log_level", ctypes.c_int, [p, ctypes.c_int]),
            ("proj_create", p, [p, s]),
            ("proj_get_ellipsoid", p, [p, p]),
            ("proj_get_celestial_body_name", s, [p, p]),
            ("proj_destroy", p, [p]),
        ]:
            getattr(lib, name).restype = res
            getattr(lib, name).argtypes = args
        self.lib, self.ctx = lib, lib.proj_context_create()
        lib.proj_context_set_enable_network(self.ctx, 0)
        lib.proj_log_level(self.ctx, 0)
        lib.proj_context_set_search_paths(
            self.ctx, 1, (s * 1)(pyproj.datadir.get_data_dir().encode())
        )

    def body(self, text):
        lib, ctx = self.lib, self.ctx
        crs = lib.proj_create(ctx, text.encode())
        assert crs, text
        ellipsoid = lib.proj_get_ellipsoid(ctx, crs)
        name = lib.proj_get_celestial_body_name(ctx, ellipsoid) if ellipsoid else None
        if ellipsoid:
            lib.proj_destroy(ellipsoid)
        lib.proj_destroy(crs)
        if name is None:
            return "?"
        return "-" if name == b"Non-Earth body" else name.decode()


def axis(crs):
    directions = tuple(a.direction.lower() for a in crs.axis_info[:2])
    return {("east", "north"): "EN", ("north", "east"): "NE"}.get(directions, "OTHER")


def params(crs, given_as_proj_string):
    """Method, lon_0, lat_0, k_0, x_0, y_0, prime meridian, a, b and metres per unit."""
    try:
        text = crs.to_proj4()
    except pyproj.exceptions.CRSError:
        text = None
    values = {}
    for key, value in re.findall(r"\+(\w+)(?:=(\S+))?", text or ""):
        values.setdefault(key, value)
    if "proj" not in values:
        return ["NONE"]
    method = values["proj"]

    def number(key, default=0.0):
        return float(values[key]) if values.get(key) else default

    lon, lat, k, x, y = (
        number("lon_0"),
        number("lat_0"),
        number("k_0", number("k", 1.0)),
        number("x_0"),
        number("y_0"),
    )
    if method == "utm":
        lon, lat, k, x, y = (
            int(values["zone"]) * 6.0 - 183.0,
            0.0,
            0.9996,
            500000.0,
            10000000.0 if "south" in values else 0.0,
        )
    elif method == "ups":
        lon, lat, k, x, y = (
            0.0,
            -90.0 if "south" in values else 90.0,
            0.994,
            2000000.0,
            2000000.0,
        )
    # The prime meridian and the ellipsoid PROJ reports for its string (the registry's for a code).
    try:
        written = CRS(text)
    except pyproj.exceptions.CRSError:
        written = crs
    pm = written.prime_meridian
    pm_degrees = (
        pm.longitude
        if pm.unit_name == "degree"
        else math.degrees(pm.longitude * pm.unit_conversion_factor)
    )
    # The registry's ellipsoid for a code, a WKT PROJ identifies as one or a reference; terrio
    # identifies no CRS given as a PROJ string.
    registry = not given_as_proj_string and crs.to_authority()
    ellipsoid = (crs if registry else written).ellipsoid
    angular = method == "longlat" or (
        method == "ob_tran" and values.get("o_proj") == "longlat"
    )
    to_meter = (
        1.0
        if angular
        else number("to_meter")
        if "to_meter" in values
        else written.axis_info[0].unit_conversion_factor
    )
    return [method] + [
        repr(float(v))
        for v in (
            lon,
            lat,
            k,
            x,
            y,
            pm_degrees,
            ellipsoid.semi_major_metre,
            ellipsoid.semi_minor_metre,
            to_meter,
        )
    ]


def row(proj, kind, text, crs):
    return "\t".join(
        [kind, text, axis(crs), proj.body(crs.srs)] + params(crs, kind == "proj")
    )


def write_all(proj, path):
    """Every EPSG, ESRI and IAU_2015 code, in the frozen data's format."""
    lines = [
        f"# Every code as PROJ {pyproj.proj_version_str} reads it (pyproj {pyproj.__version__})."
    ]
    for auth in ("EPSG", "ESRI"):
        for info in query_crs_info(auth_name=auth):
            lines.append(
                row(proj, auth, info.code, CRS.from_authority(auth, info.code))
            )
    for info in query_crs_info(auth_name="IAU_2015"):
        lines.append(
            row(proj, "ref", f"IAU_2015:{info.code}", CRS(f"IAU_2015:{info.code}"))
        )
    Path(path).write_text("\n".join(lines) + "\n")
    print(f"{len(lines) - 1} rows written to {path}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--all", metavar="PATH", help="write every code to PATH instead"
    )
    args = parser.parse_args()
    warnings.simplefilter("ignore")
    pyproj.network.set_network_enabled(False)
    assert pyproj.proj_version_str == "9.8.1", pyproj.proj_version_str
    proj = Proj()
    if args.all:
        write_all(proj, args.all)
        return
    codes = set(RULES)
    for auth in ("EPSG", "ESRI"):
        codes |= {
            (auth, int(info.code)) for info in query_crs_info(auth_name=auth)[::20]
        }
    lines = [
        f"# Axis orders, bodies and conversion parameters of CRSs as PROJ {pyproj.proj_version_str} reads them"
        f" (pyproj {pyproj.__version__}, network off),",
        "# from scripts/gen_crs_accessors.py. Columns: kind (EPSG, ESRI, proj, wkt, ref), code or definition,",
        "# axis order (EN, NE, OTHER), body (-: PROJ's Non-Earth body, ?: no ellipsoid), then NONE or method, lon_0,",
        "# lat_0, k_0, x_0, y_0, prime meridian (degrees), a, b, metres per unit.",
    ]
    for auth, code in sorted(codes):
        lines.append(row(proj, auth, str(code), CRS.from_authority(auth, code)))
    for text in PROJ_STRINGS:
        lines.append(row(proj, "proj", text, CRS(text)))
    wkts = list(WKTS)
    for code in WKT_CODES:
        for version in ("WKT1_GDAL", "WKT2_2019", "WKT1_ESRI"):
            try:
                wkts.append(CRS(code).to_wkt(version))
            except pyproj.exceptions.CRSError:
                pass  # PROJ writes no ESRI WKT for some CRSs.
    for text in wkts:
        lines.append(row(proj, "wkt", text, CRS(text)))
    iau = [int(i.code) for i in query_crs_info(auth_name="IAU_2015")]
    # Every 20th IAU_2015 CRS, and the first of each body.
    bodies = {}
    for code in iau:
        bodies.setdefault(code // 100, code)
    for code in sorted(set(iau[::20]) | set(bodies.values())):
        lines.append(row(proj, "ref", f"IAU_2015:{code}", CRS(f"IAU_2015:{code}")))
    for text in REFERENCES:
        lines.append(row(proj, "ref", text, CRS(text)))
    OUT.write_text("\n".join(lines) + "\n")
    print(f"{len(lines) - 4} rows written to {OUT}")


if __name__ == "__main__":
    main()
