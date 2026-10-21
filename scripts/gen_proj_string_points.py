"""Write PROJ's reference points for every PROJ projection, as a PROJ string and as WKT.

For each projection PROJ has (117 of them, with typical parameters), on a sphere
(``+R=6371000``) and on WGS 84 (``+ellps=WGS84``), writes to
``crates/terrio/tests/data/proj_strings.tsv`` (checked by ``crates/terrio/tests/proj_strings.rs``
and ``python/tests/test_proj_strings.py``):

- ``# case NAME EARTH``, then ``# proj STRING``: the PROJ string, ``+units=m +no_defs``;
- ``# wkt1 WKT`` and ``# wkt1_geog WKT``: pyproj's WKT1_GDAL of the CRS and of its geographic
  CRS (``# wkt1 -`` when PROJ cannot write it), and ``# wkt2`` and ``# wkt2_geog`` the same in
  WKT2_2019;
- the points, ``lon lat x y ilon ilat``: 20 points drawn within 30° of the projection's origin
  (5° for the New Zealand Map Grid; 80° of latitude at most), transformed by PROJ from the
  geographic CRS on the same figure, and PROJ's inverse of ``x y`` (``nan`` for projections PROJ
  has no inverse of); ``x y`` are ``inf`` where PROJ refuses the point.

A case PROJ rejects has ``# rejected`` instead of the WKT and points. The points are drawn from a
generator seeded by the projection's name, so the file is the same whatever runs first.

Usage: python scripts/gen_proj_string_points.py [OUT.tsv]
"""

import math
import sys
import warnings
import zlib
from pathlib import Path

import numpy as np
import pyproj

warnings.simplefilter("ignore")
OUT = Path(sys.argv[1]) if len(sys.argv) > 1 else Path(__file__).resolve().parents[1] / "crates/terrio/tests/data/proj_strings.tsv"

# Every projection of PROJ's `proj -l`, with parameters typical of its use (none where its
# defaults are the usual ones).
PARAMS = {
    "aea": "+lat_1=29.5 +lat_2=45.5 +lat_0=23 +lon_0=-96", "aeqd": "+lat_0=40 +lon_0=-100",
    "airy": "+lat_0=40 +lon_0=-100", "aitoff": "", "august": "", "bacon": "", "bertin1953": "",
    "bipc": "", "boggs": "", "bonne": "+lat_1=45", "cass": "+lat_0=48 +lon_0=10",
    "cc": "", "cea": "+lat_ts=30", "collg": "", "comill": "", "crast": "", "denoy": "",
    "eck1": "", "eck2": "", "eck3": "", "eck4": "", "eck5": "", "eck6": "", "eqc": "+lat_ts=30",
    "eqdc": "+lat_1=20 +lat_2=60 +lat_0=40", "eqearth": "", "euler": "+lat_1=20 +lat_2=60",
    "fahey": "", "fouc": "", "fouc_s": "", "gall": "", "geos": "+h=35786023 +lon_0=-75",
    "gins8": "", "gn_sinu": "+m=2 +n=3", "gnom": "+lat_0=40 +lon_0=-100", "goode": "",
    "hammer": "", "hatano": "", "igh": "", "imw_p": "+lat_1=30 +lat_2=60", "kav5": "", "kav7": "",
    "krovak": "+lat_0=49.5 +lon_0=24.83333333333333 +alpha=30.28813972222222 +k=0.9999", "laea": "+lat_0=60 +lon_0=-40", "lagrng": "+lat_1=0", "larr": "", "lask": "",
    "lcc": "+lat_1=33 +lat_2=45 +lat_0=39 +lon_0=-96", "lcca": "+lat_0=35", "loxim": "",
    "mbt_fps": "", "mbt_s": "", "mbtfpp": "", "mbtfpq": "", "mbtfps": "", "merc": "+lat_ts=33",
    "mill": "", "mil_os": "", "moll": "", "murd1": "+lat_1=20 +lat_2=60",
    "murd2": "+lat_1=20 +lat_2=60", "murd3": "+lat_1=20 +lat_2=60", "natearth": "",
    "natearth2": "", "nell": "", "nell_h": "", "nicol": "", "nsper": "+h=3000000 +lat_0=40 +lon_0=-100",
    "nzmg": "+lat_0=-41 +lon_0=173", "omerc": "+lat_0=4 +lonc=115 +alpha=53.3 +gamma=53.1 +k=0.99984", "ortel": "",
    "ortho": "+lat_0=40 +lon_0=-100", "patterson": "", "poly": "+lon_0=-54", "putp1": "",
    "putp2": "", "putp3": "", "putp3p": "", "putp4p": "", "putp5": "", "putp5p": "", "putp6": "",
    "putp6p": "", "qua_aut": "", "robin": "", "rouss": "+lat_0=40 +lon_0=-100", "sinu": "",
    "somerc": "+lat_0=46.95 +lon_0=7.44", "stere": "+lat_0=90 +lat_ts=70 +lon_0=-45",
    "sterea": "+lat_0=52 +lon_0=5 +k=0.9999", "tcc": "", "tcea": "", "times": "",
    "tissot": "+lat_1=20 +lat_2=60", "tmerc": "+lat_0=10 +lon_0=-103 +k=0.9996", "tobmerc": "",
    "tpeqd": "+lat_1=40 +lon_1=-100 +lat_2=45 +lon_2=-80",
    "tpers": "+h=3000000 +lat_0=40 +lon_0=-100 +tilt=10", "urm5": "+n=0.9 +alpha=2 +q=4",
    "urmfps": "+n=0.5", "utm": "+zone=13", "vandg": "", "vandg2": "", "vandg3": "", "vandg4": "",
    "vitk1": "+lat_1=20 +lat_2=60", "wag1": "", "wag2": "", "wag3": "", "wag4": "", "wag5": "",
    "wag6": "", "wag7": "", "weren": "", "wink1": "", "wink2": "", "wintri": "",
}
# How far from the origin points are drawn, in degrees, for projections of a small area.
SPREAD = {"nzmg": 5.0, "krovak": 5.0}
EARTHS = {"sphere": "+R=6371000", "wgs84": "+ellps=WGS84"}
N_POINTS = 20


def wkt(crs, version):
    """Write a CRS as WKT on one line.

    Args:
        crs: The pyproj CRS.
        version: ``"WKT1_GDAL"`` or ``"WKT2_2019"``.

    Returns:
        The WKT, or ``-`` when PROJ cannot write it in that version.
    """
    try:
        text = crs.to_wkt(version)
    except pyproj.exceptions.CRSError:
        return "-"
    return text if text else "-"


def case(name, extra, earth):
    """Compute the lines of one projection on one figure.

    Args:
        name: The projection.
        extra: Its parameters.
        earth: The figure's PROJ parameters.

    Returns:
        The lines.
    """
    proj = " ".join(p for p in (f"+proj={name}", extra, earth, "+units=m +no_defs") if p)
    geog = f"+proj=longlat {earth} +no_defs"
    lines = [f"# proj {proj}\n"]
    try:
        crs = pyproj.CRS(proj)
        fwd = pyproj.Transformer.from_crs(geog, proj, always_xy=True)
    except (pyproj.exceptions.CRSError, pyproj.exceptions.ProjError):
        return lines + ["# rejected\n"]
    try:
        inv = pyproj.Transformer.from_crs(proj, geog, always_xy=True)
    except pyproj.exceptions.ProjError:  # a projection without inverse (airy, bacon, …)
        inv = None
    base = crs.geodetic_crs
    for key, version in (("wkt1", "WKT1_GDAL"), ("wkt2", "WKT2_2019")):
        lines.append(f"# {key} {wkt(crs, version)}\n")
        lines.append(f"# {key}_geog {wkt(base, version)}\n")
    d = crs.to_dict()
    lon0 = float(d.get("lon_0", d.get("lonc", 0.0)))
    lat0 = 70.0 if name == "stere" else float(d.get("lat_0", 0.0))
    if "lon_1" in d and "lon_2" in d:  # two-point equidistant: around the two points
        lon0, lat0 = (d["lon_1"] + d["lon_2"]) / 2, (d["lat_1"] + d["lat_2"]) / 2
    rng = np.random.default_rng(zlib.crc32(name.encode()))
    spread = SPREAD.get(name, 30.0)
    lon = lon0 + rng.uniform(-spread, spread, N_POINTS)
    lat = np.clip(lat0 + rng.uniform(-spread, spread, N_POINTS), -80, 80)
    xs, ys = fwd.transform(lon, lat)
    for lo, la, x, y in zip(lon.tolist(), lat.tolist(), xs.tolist(), ys.tolist()):
        if math.isfinite(x) and math.isfinite(y):
            ilon, ilat = inv.transform(x, y) if inv else (math.nan, math.nan)
            lines.append(f"{lo!r}\t{la!r}\t{x!r}\t{y!r}\t{ilon!r}\t{ilat!r}\n")
        else:
            lines.append(f"{lo!r}\t{la!r}\tinf\tinf\n")
    return lines


def main():
    """Write the reference points."""
    with open(OUT, "w") as out:
        out.write(f"# PROJ projections against pyproj {pyproj.__version__} (PROJ {pyproj.proj_version_str}),\n")
        out.write("# from scripts/gen_proj_string_points.py.\n")
        for name, extra in PARAMS.items():
            for ename, earth in EARTHS.items():
                out.write(f"# case {name} {ename}\n")
                out.writelines(case(name, extra, earth))


if __name__ == "__main__":
    main()
