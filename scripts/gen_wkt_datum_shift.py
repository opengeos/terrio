"""Generate crates/terrio-crs/tests/data/wkt_datum_shift.json: where PROJ 9.9.0 places points of
WKTs whose geographic CRS is not given by an EPSG code of its own, but whose datum is known by its
EPSG ID or by its name (ESRI's `D_Monte_Mario`, `D_OSGB_1936`, ...), in WGS 84.

The WKTs are pyproj's of EPSG CRSs, rewritten:

- WKT 2 (2019) without any ID, with its names, or with only the datum's ID, also under other CRS
  and datum names (`Local`, `Local datum`);
- OGC WKT 1 (GDAL's) with only the datum's `AUTHORITY`, also under other names;
- ESRI WKT 1 (`to_wkt("WKT1_ESRI")`) as ESRI writes it, without codes, with the geographic CRS
  (and projected CRS) renamed, with the datum's `AUTHORITY`, and with the CRS's `AUTHORITY`;
- the two cases of opengeos/terrio#1012 from RustGIS's corpora: EPSG:3003 as ESRI WKT 1 and
  EPSG:4277 as WKT 2 without its CRS and datum IDs, at their points.

Each case holds a point in the CRS (from the centre of its area of use in WGS 84, or RustGIS's),
PROJ's coordinates of it in WGS 84 by `Transformer.from_crs(CRS(wkt), "EPSG:4326",
always_xy=True)`, the transformation PROJ used (`get_last_used_operation().description`), and
where the CRS's EPSG code places the point.

No grids are installed, so PROJ uses the best transformation it has without one, a datum
transformation for every case (none is PROJ's no-shift ballpark, which the script asserts). Where a
better one needs a grid (OSTN15 for OSGB36, BETA2007 for DHDN, the NZGD2000 grids for NZGD49,
CHENyx06 for some CH1903 cases), the case records it (`best_grid_missing`, from
`TransformerGroup(...).best_available`): with those grids installed, PROJ places such points
otherwise.

`differences` holds the WKTs PROJ places otherwise than their CRS's EPSG code: the ESRI WKT 1 of
EPSG:4806, 5132 and 7373, and of EPSG:4314 and 31467 renamed. PROJ does not resolve their
geographic CRS to the code (ESRI's WKT 1 has no `AXIS`, so longitude first, unlike the EPSG CRS;
or its name is another), and then also searches transformations through other datums and other
CRSs of the datum. They are split from `cases` at 1 mm and don't count among its 180. terrio
follows PROJ's search, but its datum pivot through the datum's geocentric CRS (ONGD14, EPSG:7373):
that case is marked `pending`.

Usage: PROJ_NETWORK=OFF python scripts/gen_wkt_datum_shift.py (needs pyproj 3.8.0)
"""

import json
import math
import os
import re
import sqlite3
import warnings
from pathlib import Path

import pyproj
import pyproj.datadir
import pyproj.exceptions
import pyproj.network
from pyproj import CRS, Transformer
from pyproj.transformer import TransformerGroup

OUT = (
    Path(__file__).resolve().parents[1]
    / "crates/terrio-crs/tests/data/wkt_datum_shift.json"
)
RUSTGIS = {
    # EPSG:3003 as GDAL writes it for ESRI, at RustGIS's point in Milan.
    "EPSG:3003 as ESRI WKT 1 (opengeos/terrio#1012)": (
        3003,
        (
            'PROJCS["Monte_Mario_Italy_1",GEOGCS["GCS_Monte_Mario",DATUM["D_Monte_Mario",'
            'SPHEROID["International_1924",6378388.0,297.0]],PRIMEM["Greenwich",0.0],'
            'UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],'
            'PARAMETER["False_Easting",1500000.0],PARAMETER["False_Northing",0.0],'
            'PARAMETER["Central_Meridian",9.0],PARAMETER["Scale_Factor",0.9996],'
            'PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]'
        ),
        (1514881.490594152, 5034556.812441661),
    ),
    # EPSG:4277 in WKT 2 without its CRS and datum IDs (its units and prime meridian keep theirs),
    # from RustGIS's `wkt2_corpus.tsv`, at RustGIS's point.
    "EPSG:4277 as WKT 2 without its CRS and datum IDs (opengeos/terrio#1012)": (
        4277,
        (
            'GEOGCRS["OSGB36",DATUM["Ordnance Survey of Great Britain 1936",ELLIPSOID["Airy 1830",'
            '6377563.396,299.3249646,LENGTHUNIT["metre",1,ID["EPSG",9001]]]],PRIMEM["Greenwich",0,'
            'ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8901]],CS[ellipsoidal,2],'
            'AXIS["geodetic latitude (Lat)",north,ORDER[1],ANGLEUNIT["degree",0.0174532925199433,'
            'ID["EPSG",9122]]],AXIS["geodetic longitude (Lon)",east,ORDER[2],'
            'ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]]]'
        ),
        (-3.4986390218735695, 55.37999381533582),
    ),
}
# EPSG CRSs, geographic and projected, on datums with Helmert transformations to WGS 84.
CODES = [
    4277,  # OSGB36
    27700,  # OSGB36 / British National Grid
    4265,  # Monte Mario
    3003,  # Monte Mario / Italy zone 1
    3004,  # Monte Mario / Italy zone 2
    4230,  # ED50
    23032,  # ED50 / UTM zone 32N
    4314,  # DHDN
    31467,  # DHDN / 3-degree Gauss-Kruger zone 3
    4289,  # Amersfoort
    28992,  # Amersfoort / RD New
    4121,  # GGRS87
    2100,  # GGRS87 / Greek Grid
    4149,  # CH1903
    21781,  # CH1903 / LV03
    4301,  # Tokyo
    4237,  # HD72
    23700,  # HD72 / EOV
    4202,  # AGD66
    4272,  # NZGD49
]
# EPSG geographic CRSs whose ESRI WKT 1, as written, PROJ places otherwise than their EPSG code.
# With the renamed ESRI WKT 1 of four CRSs of `CODES`, the geographic EPSG:4314 and 4289 and the
# projected EPSG:31467 and 28992, they are `DIFFERENT`. (Amersfoort since EPSG v13: by its code,
# PROJ 9.9.0 goes through Amersfoort to ETRS89-NLD [AGRS2010] (8); its renamed ESRI WKT 1 through
# Amersfoort to WGS 84 (4).)
DIFFERENCES = [4806, 5132, 7373]
# Every case PROJ places otherwise than by its EPSG code: a PROJ change that moves a case in or out
# stops the script.
DIFFERENT = [
    "EPSG:4314, ESRI WKT 1, renamed",
    "EPSG:31467, ESRI WKT 1, renamed",
    "EPSG:4289, ESRI WKT 1, renamed",
    "EPSG:28992, ESRI WKT 1, renamed",
    *(f"EPSG:{code}, ESRI WKT 1" for code in DIFFERENCES),
]
# Those PROJ places by its datum pivot, which terrio does not follow yet.
PENDING = ["EPSG:7373, ESRI WKT 1"]
IDS = re.compile(r',(AUTHORITY|ID)\["(EPSG|ESRI)","?\d+"?\]')
USAGE = re.compile(r",USAGE\[SCOPE\[[^\]]*\],AREA\[[^\]]*\],BBOX\[[^\]]*\]\]")


def wkt2(crs, datum_id=False, local=False):
    """The CRS's WKT 2 without IDs: with the datum's alone, and under other names."""
    w, n = USAGE.subn("", crs.to_wkt("WKT2_2019"))
    check(n == 1, crs)
    w, n = IDS.subn("", w)
    check(n, crs)
    if datum_id:
        code = crs.datum.to_json_dict()["id"]["code"]
        # The ellipsoid with its LENGTHUNIT, then the DATUM's closing bracket.
        pattern = r'(ELLIPSOID\["[^"]*",[^\[\]]*,LENGTHUNIT\[[^\]]*\]\])\]'
        w, n = re.subn(
            pattern, lambda m: f'{m.group(1)},ID["EPSG",{code}]]', w, count=1
        )
        check(
            n == 1
            and f'LENGTHUNIT["metre",1]],ID["EPSG",{code}]]' in w.replace(" ", ""),
            crs,
        )
    if local:
        w, n = re.subn(r'(GEOGCRS|BASEGEOGCRS|PROJCRS)\["[^"]*"', r'\1["Local"', w)
        w, m = re.subn(r'DATUM\["[^"]*"', 'DATUM["Local datum"', w)
        check(n and m, crs)
    return w


def wkt1(crs, local=False):
    """GDAL's WKT 1 with only the datum's `AUTHORITY`, and under other names."""
    w = crs.to_wkt("WKT1_GDAL")
    found = re.search(r'AUTHORITY\["EPSG","(\d+)"\]\],PRIMEM', w)
    check(found, crs)
    w, n = IDS.subn("", w)
    check(n, crs)
    w, n = re.subn(r"\]\],PRIMEM", f'],AUTHORITY["EPSG","{found.group(1)}"]],PRIMEM', w)
    check(n == 1, crs)
    if local:
        w, n = re.subn(r'(GEOGCS|PROJCS)\["[^"]*"', r'\1["Local"', w)
        w, m = re.subn(r'DATUM\["[^"]*"', 'DATUM["Local datum"', w)
        check(n and m, crs)
    return w


def esri(crs):
    """ESRI's WKT 1: as written, renamed, with the datum's and with the CRS's code."""
    w = crs.to_wkt("WKT1_ESRI")
    # The CRS's code goes before the closing bracket of its root node.
    check(w.endswith("]") and not w.endswith("]]]"), w[-20:])
    datum = crs.datum.to_json_dict()["id"]["code"]
    root = "PROJCS" if w.startswith("PROJCS") else "GEOGCS"
    renamed, n = re.subn(r'GEOGCS\["[^"]*"', 'GEOGCS["GCS_Local"', w)
    check(n == 1, crs)
    if root == "PROJCS":
        renamed, n = re.subn(r'^PROJCS\["[^"]*"', 'PROJCS["Local grid"', renamed)
        check(n == 1, crs)
    with_datum, n = re.subn(
        r"\]\],PRIMEM", f'],AUTHORITY["EPSG","{datum}"]],PRIMEM', w, count=1
    )
    check(n == 1, crs)
    return {
        "ESRI WKT 1": w,
        "ESRI WKT 1, renamed": renamed,
        "ESRI WKT 1 with the datum's code": with_datum,
        "ESRI WKT 1 with the CRS's code": w[:-1]
        + f',AUTHORITY["EPSG","{crs.to_epsg()}"]]',
    }


def centre(crs):
    """A point in the CRS: the centre of its area of use."""
    w, s, e, n = crs.area_of_use.bounds
    t = Transformer.from_crs("EPSG:4326", crs, always_xy=True)
    return list(t.transform((w + e) / 2, (s + n) / 2, errcheck=True))


def place(crs, point):
    """PROJ's WGS 84 coordinates of a point, the transformation it used, and whether the best
    transformation PROJ knows needs a grid that is missing (`TransformerGroup.best_available`)."""
    t = Transformer.from_crs(crs, "EPSG:4326", always_xy=True)
    lon, lat = t.transform(*point, errcheck=True)
    with warnings.catch_warnings():
        # TransformerGroup warns when the best is missing, which is what is recorded.
        warnings.simplefilter("ignore")
        group = TransformerGroup(crs, "EPSG:4326", always_xy=True)
    missing = not group.best_available
    try:
        operation = t.get_last_used_operation().description
    except pyproj.exceptions.ProjError:
        # A transformer of a single operation keeps no last used one.
        operation = t.description
    return [lon, lat], operation, missing


def metres(a, b):
    """Metres between two longitudes/latitudes, near enough at a millimetre."""
    dlon = (a[0] - b[0]) * math.cos(math.radians(b[1]))
    return math.hypot(dlon, a[1] - b[1]) * 111_195.0


def case(name, code, wkt, point):
    proj, operation, missing = place(CRS.from_wkt(wkt), point)
    by_code, _, _ = place(CRS.from_epsg(code), point)
    # A datum shift, not PROJ's no-shift fallback.
    check("Ballpark" not in operation, (name, operation))
    return {
        "name": name,
        "code": f"EPSG:{code}",
        "wkt": wkt,
        "point": point,
        "proj": proj,
        "operation": operation,
        "best_grid_missing": missing,
        "by_code": by_code,
    }


def variants(code):
    crs = CRS.from_epsg(code)
    out = {
        "WKT 2 without IDs": wkt2(crs),
        "WKT 2 with the datum's ID alone": wkt2(crs, datum_id=True),
        "WKT 2 with the datum's ID alone, renamed": wkt2(
            crs, datum_id=True, local=True
        ),
        "WKT 1 with the datum's AUTHORITY alone": wkt1(crs),
        "WKT 1 with the datum's AUTHORITY alone, renamed": wkt1(crs, local=True),
    }
    out.update(esri(crs))
    return crs, out


def check(ok, what):
    """Stops the script when `ok` is false (an `assert` that `python -O` keeps)."""
    if not ok:
        raise RuntimeError(f"check failed: {what!r}")


# The files of the grid formats PROJ reads, by extension.
GRID_EXTENSIONS = (
    ".tif",
    ".tiff",
    ".gsb",
    ".gtx",
    ".byn",
    ".lvz",
    ".kgd",
    ".gvb",
    ".ntv1",
    ".ntv2",
    ".ctable",
    ".ctable2",
    ".las",
    ".los",
    ".grd",
)


def installed_grids():
    """The grid files in PROJ's search paths: every file PROJ's database names as a grid
    (`grid_alternatives`, with its pre-PROJ 7 name), and every file of a grid format."""
    # Absolute, without empty entries (which would be the working directory).
    paths = pyproj.datadir.get_data_dir().split(os.pathsep)
    paths.append(pyproj.datadir.get_user_data_dir())
    dirs = [os.path.abspath(d) for d in paths if d]
    # PROJ's database, in whichever search path holds it, opened read-only (connecting to a
    # missing path would create an empty one).
    found = [Path(d, "proj.db") for d in dirs if Path(d, "proj.db").is_file()]
    check(found, f"no proj.db in {dirs}")
    db = sqlite3.connect(found[0].as_uri() + "?mode=ro", uri=True)
    rows = db.execute(
        "SELECT proj_grid_name, old_proj_grid_name FROM grid_alternatives"
    )
    names = {n for row in rows for n in row if n}
    db.close()
    check(names, "no grid names in proj.db")
    return [
        os.path.join(root, f)
        for d in dirs
        if os.path.isdir(d)
        for root, _, files in os.walk(d)
        for f in files
        if f in names or f.lower().endswith(GRID_EXTENSIONS)
    ]


def main():
    check(pyproj.proj_version_str == "9.9.0", pyproj.proj_version_str)
    pyproj.network.set_network_enabled(False)
    # No grids, so the values don't depend on what is installed.
    found = installed_grids()
    check(not found, found)
    cases = [case(n, c, w, list(p)) for n, (c, w, p) in RUSTGIS.items()]
    for code in CODES:
        crs, wkts = variants(code)
        point = centre(crs)
        cases += [case(f"EPSG:{code}, {k}", code, w, point) for k, w in wkts.items()]
    for code in DIFFERENCES:
        crs = CRS.from_epsg(code)
        w = crs.to_wkt("WKT1_ESRI")
        cases.append(case(f"EPSG:{code}, ESRI WKT 1", code, w, centre(crs)))
    # Those PROJ places otherwise than by their EPSG code, apart.
    placed = {False: [], True: []}
    for c in cases:
        placed[metres(c["proj"], c["by_code"]) >= 1e-3].append(c)
    cases, differences = placed[False], placed[True]
    for c in differences:
        c["pending"] = c["name"] in PENDING
    names = sorted(c["name"] for c in differences)
    check(names == sorted(DIFFERENT), names)
    OUT.write_text(
        json.dumps(
            {
                "proj_version": pyproj.proj_version_str,
                "cases": cases,
                "differences": differences,
            },
            indent=1,
        )
        + "\n"
    )


if __name__ == "__main__":
    main()
