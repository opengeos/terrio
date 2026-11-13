"""Freeze PROJ's WKT 1 of CRSs for ``SpatialRef::to_wkt1_strict`` (``tests/wkt1_strict_export.rs``).

PROJ 9.8.1 (pyproj 3.8.0) writes each CRS with ``CRS.to_wkt("WKT1_GDAL", output_axis_rule=True)``
(``proj_as_wkt`` with ``WKT1_GDAL`` and ``OUTPUT_AXIS=YES``, the AXIS nodes GDAL's
``exportToWkt`` writes, as ``projinfo -o WKT1_GDAL``), with the network off. The rows are every
20th EPSG and ESRI code, every registry code PROJ writes no WKT 1 for, the codes behind each
rule, the custom definitions of ``scripts/gen_proj_string_export.py`` with the cases of WKT 1's
extension (grids, axis orders, methods without a WKT 1 name, WKT 2 input), compound references
and every 20th IAU_2015 CRS. ``NONE`` marks a CRS PROJ has no WKT 1 for.

Usage: python scripts/gen_wkt1_strict_export.py (needs pyproj 3.8.0)

``--all PATH`` writes every EPSG, ESRI and IAU_2015 code instead, to PATH (not committed), for the
whole-table comparison: ``TERRIO_WKT1_ALL=PATH cargo test -p terrio-crs --test
wkt1_strict_export -- --ignored``.
"""

import argparse
import re
import sys
import warnings
from pathlib import Path

import pyproj
from pyproj import CRS
from pyproj.database import query_crs_info

# The custom definitions of the PROJ string export, beside this script (also under ``python -I``).
sys.path.insert(0, str(Path(__file__).resolve().parent))
from gen_proj_string_export import (  # noqa: E402
    PROJ_STRINGS,
    REFERENCES,
    SHIFTED,
    UNIDENTIFIED,
    WKTS,
    shifted_wkt,
    unidentified_wkt,
)

OUT = (
    Path(__file__).resolve().parents[1]
    / "crates/terrio-crs/tests/data/wkt1_strict_export.tsv"
)

# Web Mercator, the only registry CRS PROJ writes with its PROJ4 extension; geographic 3D,
# geocentric, Equal Earth, south-orientated and compound CRSs.
RULES = [("EPSG", c) for c in (3857, 4979, 4978, 8857, 2046, 5514, 7405, 9707)] + [
    ("ESRI", c) for c in (102100, 54030, 53035)
]

# Polar Stereographic (variant C): terrio's table WKT is GDAL's, without the AXIS nodes PROJ
# 9.8.1 writes for it.
LEFT_OUT = {("EPSG", 2985), ("EPSG", 2986)}

STRICT_PROJ_STRINGS = [
    # Geocentric CRSs and datum shifts: WKT 1 describes them.
    "+proj=geocent +ellps=GRS80 +units=m",
    "+proj=geocent +datum=WGS84 +units=m",
    "+proj=geocent +ellps=bessel +towgs84=598.1,73.7,418.2,0.202,0.045,-2.455,6.7 +units=m",
    "+proj=utm +zone=32 +ellps=intl +towgs84=-87,-98,-121 +units=m",
    "+proj=longlat +a=6378137 +rf=298.257223563 +towgs84=0,0,0",
    # A conformal sphere of a sphere (IAU_2015's Mercators) changes nothing.
    "+proj=merc +R_C +lon_0=0 +x_0=0 +y_0=0 +R=3396190 +units=m",
    # What WKT 1 keeps in the extension: a grid, an approximation, a sweep axis, a rotated pole.
    "+proj=longlat +ellps=clrk66 +nadgrids=conus +no_defs",
    "+proj=tmerc +approx +lon_0=9 +ellps=WGS84 +units=m",
    "+proj=geos +h=35785831 +sweep=x +ellps=WGS84 +units=m",
    "+proj=ob_tran +o_proj=longlat +o_lat_p=40 +o_lon_p=10 +ellps=WGS84",
    # Methods PROJ has no WKT 1 for: Equal Earth, LCC 2SP with a scale, the Colombian urban one.
    "+proj=eqearth +lon_0=150 +ellps=WGS84 +units=m",
    "+proj=eqearth +lon_0=150 +x_0=0 +y_0=0 +units=m +a=6378137.0 +rf=298.257223563",
    "+proj=lcc +lat_1=40 +lat_2=50 +k=0.99 +ellps=WGS84 +units=m",
    (
        "+proj=col_urban +lat_0=7.88893673611111 +lon_0=-72.50287095 +x_0=842805.406 +y_0=1364404.57 "
        "+h_0=308 +ellps=GRS80 +units=m"
    ),
    # Known differences: PROJ describes a north-east projected CRS and a cylindrical equal-area
    # projection by its scale in WKT 1, terrio keeps them in the extension; PROJ keeps UPS in its
    # extension, terrio writes it as Polar Stereographic.
    "+proj=tmerc +lon_0=9 +k=0.9996 +x_0=500000 +ellps=GRS80 +axis=neu +units=m",
    "+proj=cea +k_0=0.9 +ellps=WGS84 +units=m",
    "+proj=ups +ellps=WGS84 +units=m",
    # A word without a `+`, read as a parameter (`approx`, kept in the extension); an ellipsoid of
    # +b and +es, whose order of precedence differs (known difference).
    "+proj=tmerc approx +lon_0=9 +ellps=WGS84 +units=m",
    "+proj=longlat +a=6378137 +b=6356752.314245179 +es=0.006",
]

# Compound and OGC references: Web Mercator's extension in a compound CRS without its code; CRS84,
# EPSG:4326's datum with east-north axes.
STRICT_REFERENCES = ["EPSG:3857+3855", "OGC:CRS84"]


def renamed(wkt):
    """A WKT without its codes and names, which terrio reads rather than identifies."""
    wkt = re.sub(r',(AUTHORITY|ID)\["EPSG",("?\d+"?)\]', "", wkt)
    wkt = re.sub(r',USAGE\[.*?BBOX\[[^\]]*\]\]', "", wkt)
    return re.sub(r'(PROJCS|GEOGCS|DATUM|PROJCRS|BASEGEOGCRS|GEOGCRS|GEODCRS|COMPOUNDCRS|VERTCRS)\["[^"]*"', r'\1["custom"', wkt)


def with_axes(code, name, order):
    """The WKT 2 of a registry CRS without its code, named ``name``, with its axes in ``order``."""
    d = CRS.from_epsg(code).to_json_dict()
    for key in ("id", "usage", "usages", "scope", "area", "bbox"):
        d.pop(key, None)
    d["name"] = name
    axes = d["coordinate_system"]["axis"]
    d["coordinate_system"]["axis"] = [axes[i] for i in order]
    return CRS.from_json_dict(d).to_wkt("WKT2_2019")


def compound_with_axes(code, order):
    """The WKT 2 of a registry compound CRS, with its code, its horizontal axes in ``order``."""
    d = CRS.from_epsg(code).to_json_dict()
    cs = d["components"][0]["coordinate_system"]
    cs["axis"] = [cs["axis"][i] for i in order]
    return CRS.from_json_dict(d).to_wkt("WKT2_2019")


def strict_wkts():
    web = CRS.from_epsg(3857).to_wkt("WKT1_GDAL", output_axis_rule=True)
    return [
        # GDAL's Web Mercator: its extension, under its code; without it, a custom extension.
        web,
        renamed(web),
        # WKT 2 of a registry CRS, identified and not; of a datum shift; of a 3D CRS.
        CRS.from_epsg(32632).to_wkt("WKT2_2019"),
        renamed(CRS.from_epsg(32632).to_wkt("WKT2_2019")),
        CRS("+proj=utm +zone=32 +ellps=intl +towgs84=-87,-98,-121 +units=m").to_wkt("WKT2_2019"),
        CRS.from_epsg(4979).to_wkt("WKT2_2019"),
        renamed(CRS.from_epsg(4979).to_wkt("WKT2_2019")),
        # An ESRI .prj of Equal Earth, which PROJ cannot write in WKT 1.
        CRS("+proj=eqearth +datum=WGS84 +units=m").to_wkt("WKT1_ESRI"),
        # WKT 2 with other axes than their code's: EPSG:4326 east-north, EPSG:2193 east-north,
        # EPSG:32632 north-east.
        with_axes(4326, "custom_en", (1, 0)),
        with_axes(2193, "custom_en", (1, 0)),
        with_axes(32632, "custom_ne", (1, 0)),
        # Swapped, on a datum a PROJ string does not name: the registry's WKT 1, axes swapped.
        with_axes(4258, "custom_en", (1, 0)),
        # Web Mercator swapped: its extension no longer under its code, so none.
        with_axes(3857, "custom_ne", (1, 0)),
        compound_with_axes(7405, (1, 0)),
        compound_with_axes(9707, (1, 0)),
        # Polar CRSs' own WKT 2 (axes along meridians): their registry WKT 1.
        CRS.from_epsg(5041).to_wkt("WKT2_2019"),
        CRS.from_epsg(3031).to_wkt("WKT2_2019"),
        CRS.from_epsg(6931).to_wkt("WKT2_2019"),
        # A WKT 2 geocentric CRS without a code.
        renamed(CRS("+proj=geocent +ellps=bessel +units=m").to_wkt("WKT2_2019")),
        # Known difference: an unidentified WKT 2 compound CRS, which terrio does not write in
        # WKT 1.
        renamed(CRS("EPSG:32632+5773").to_wkt("WKT2_2019")),
    ]


def wkt1(crs):
    try:
        return crs.to_wkt("WKT1_GDAL", output_axis_rule=True) or "NONE"
    except pyproj.exceptions.CRSError:
        return "NONE"


def write_all(path):
    lines = [f"# Every code's WKT 1 as PROJ {pyproj.proj_version_str} writes it (pyproj {pyproj.__version__})."]
    for auth in ("EPSG", "ESRI"):
        for info in query_crs_info(auth_name=auth):
            if (auth, int(info.code)) not in LEFT_OUT:
                lines.append(f"{auth}\t{info.code}\t{wkt1(CRS.from_authority(auth, info.code))}")
    for info in query_crs_info(auth_name="IAU_2015"):
        lines.append(f"ref\tIAU_2015:{info.code}\t{wkt1(CRS(f'IAU_2015:{info.code}'))}")
    Path(path).write_text("\n".join(lines) + "\n")
    print(f"{len(lines) - 1} rows written to {path}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--all", metavar="PATH", help="write every code to PATH instead")
    args = parser.parse_args()
    warnings.simplefilter("ignore")
    pyproj.network.set_network_enabled(False)
    assert pyproj.proj_version_str == "9.8.1", pyproj.proj_version_str
    if args.all:
        write_all(args.all)
        return
    rows = set(RULES)
    for auth in ("EPSG", "ESRI"):
        for i, info in enumerate(query_crs_info(auth_name=auth)):
            code = (auth, int(info.code))
            if i % 20 == 0 or wkt1(CRS.from_authority(auth, info.code)) == "NONE":
                rows.add(code)
    lines = [
        (
            f"# WKT 1 of CRSs as PROJ {pyproj.proj_version_str} writes it (pyproj {pyproj.__version__}"
            ' CRS.to_wkt("WKT1_GDAL", output_axis_rule=True), network off),'
        ),
        "# by scripts/gen_wkt1_strict_export.py: kind, input, WKT 1 (NONE: PROJ has none).",
    ]
    for auth, code in sorted(rows - LEFT_OUT):
        lines.append(f"{auth}\t{code}\t{wkt1(CRS.from_authority(auth, code))}")
    for p in PROJ_STRINGS + STRICT_PROJ_STRINGS:
        lines.append(f"proj\t{p}\t{wkt1(CRS(p))}")
    wkts = WKTS + [shifted_wkt(c, t) for c, t in SHIFTED] + [unidentified_wkt(c) for c in UNIDENTIFIED]
    for w in wkts + strict_wkts():
        lines.append(f"wkt\t{w}\t{wkt1(CRS(w))}")
    iau = [f"IAU_2015:{i.code}" for i in query_crs_info(auth_name="IAU_2015")][::20]
    for r in REFERENCES + STRICT_REFERENCES + iau:
        lines.append(f"ref\t{r}\t{wkt1(CRS(r))}")
    OUT.write_text("\n".join(lines) + "\n")
    print(f"{len(lines) - 2} rows written to {OUT}")


if __name__ == "__main__":
    main()
