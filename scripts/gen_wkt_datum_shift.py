"""Generate crates/terrio-crs/tests/data/wkt_datum_shift.json: where PROJ 9.8.1 places points of
WKTs whose geographic CRS is not given by an EPSG code of its own, but whose datum is known by its
EPSG ID or by its name (ESRI's `D_Monte_Mario`, `D_OSGB_1936`, ...), in WGS 84.

The WKTs are pyproj's of EPSG CRSs, rewritten:

- WKT 2 (2019) without any ID, with its names, or with only the datum's ID, also under other CRS
  and datum names (`Local`, `Local datum`);
- OGC WKT 1 (GDAL's) with only the datum's `AUTHORITY`, also under other names;
- ESRI WKT 1 (`to_wkt("WKT1_ESRI")`) as ESRI writes it, without codes, with the geographic CRS
  (and projected CRS) renamed, with the datum's `AUTHORITY`, and with the CRS's `AUTHORITY`;
- the two cases of opengeos/terrio#1012 from RustGIS's corpora: EPSG:3003 as ESRI WKT 1 and
  EPSG:4277 as WKT 2 without IDs, at their points.

Each case holds a point in the CRS (from the centre of its area of use in WGS 84, or RustGIS's),
PROJ's coordinates of it in WGS 84 by `Transformer.from_crs(CRS(wkt), "EPSG:4326",
always_xy=True)`, the transformation PROJ used (`get_last_used_operation().description`), and
where the CRS's EPSG code places the point.

`differences` holds the WKTs PROJ places otherwise than their CRS's EPSG code: the ESRI WKT 1 of
EPSG:4806, 5132 and 7373, and of EPSG:4314 and 31467 renamed. PROJ does not resolve their geographic CRS
to the code (ESRI's WKT 1 has no `AXIS`, so longitude first, unlike the EPSG CRS; or its name is
another), and then also searches transformations through other datums and other CRSs of the
datum.

Usage: PROJ_NETWORK=OFF python scripts/gen_wkt_datum_shift.py (needs pyproj 3.8.0)
"""

import json
import math
import os
import re
import warnings
from pathlib import Path

import pyproj
import pyproj.datadir
import pyproj.exceptions
import pyproj.network
from pyproj import CRS, Transformer

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
    # EPSG:4277 as PROJ 9.7.1 writes it in WKT 2 without its ID, at RustGIS's point.
    "EPSG:4277 as WKT 2 without IDs (opengeos/terrio#1012)": (
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
# EPSG CRSs on datums with transformations to WGS 84 (Helmert, and a grid for NAD27), geographic
# and projected.
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
    4267,  # NAD27
    26717,  # NAD27 / UTM zone 17N
    4202,  # AGD66
    4272,  # NZGD49
]
# Geographic CRSs whose ESRI WKT 1 PROJ places otherwise than their EPSG code.
DIFFERENCES = [4806, 5132, 7373]
IDS = re.compile(r',(AUTHORITY|ID)\["(EPSG|ESRI)","?\d+"?\]')
USAGE = re.compile(r",USAGE\[SCOPE\[[^\]]*\],AREA\[[^\]]*\],BBOX\[[^\]]*\]\]")


def wkt2(crs, datum_id=False, local=False):
    """The CRS's WKT 2 without IDs: with the datum's alone, and under other names."""
    w = IDS.sub("", USAGE.sub("", crs.to_wkt("WKT2_2019")))
    if datum_id:
        code = crs.datum.to_json_dict()["id"]["code"]
        w, n = re.subn(
            r"(ELLIPSOID\[[^\[]*\[[^\]]*\]\])\]",
            lambda m: f'{m.group(1)},ID["EPSG",{code}]]',
            w,
            count=1,
        )
        assert n == 1, crs
    if local:
        w, n = re.subn(r'(GEOGCRS|BASEGEOGCRS|PROJCRS)\["[^"]*"', r'\1["Local"', w)
        w, m = re.subn(r'DATUM\["[^"]*"', 'DATUM["Local datum"', w)
        assert n and m, crs
    return w


def wkt1(crs, local=False):
    """GDAL's WKT 1 with only the datum's `AUTHORITY`, and under other names."""
    w = crs.to_wkt("WKT1_GDAL")
    code = re.search(r'AUTHORITY\["EPSG","(\d+)"\]\],PRIMEM', w).group(1)
    w, n = re.subn(
        r"\]\],PRIMEM", f'],AUTHORITY["EPSG","{code}"]],PRIMEM', IDS.sub("", w)
    )
    assert n == 1, crs
    if local:
        w, n = re.subn(r'(GEOGCS|PROJCS)\["[^"]*"', r'\1["Local"', w)
        w, m = re.subn(r'DATUM\["[^"]*"', 'DATUM["Local datum"', w)
        assert n and m, crs
    return w


def esri(crs):
    """ESRI's WKT 1: as written, renamed, with the datum's and with the CRS's code."""
    w = crs.to_wkt("WKT1_ESRI")
    datum = crs.datum.to_json_dict()["id"]["code"]
    root = "PROJCS" if w.startswith("PROJCS") else "GEOGCS"
    renamed = re.sub(r'GEOGCS\["[^"]*"', 'GEOGCS["GCS_Local"', w)
    if root == "PROJCS":
        renamed = re.sub(r'^PROJCS\["[^"]*"', 'PROJCS["Local grid"', renamed)
    return {
        "ESRI WKT 1": w,
        "ESRI WKT 1, renamed": renamed,
        "ESRI WKT 1 with the datum's code": re.sub(
            r"\]\],PRIMEM", f'],AUTHORITY["EPSG","{datum}"]],PRIMEM', w, count=1
        ),
        "ESRI WKT 1 with the CRS's code": w[:-1]
        + f',AUTHORITY["EPSG","{crs.to_epsg()}"]]',
    }


def centre(crs):
    """A point in the CRS: the centre of its area of use."""
    w, s, e, n = crs.area_of_use.bounds
    t = Transformer.from_crs("EPSG:4326", crs, always_xy=True)
    return list(t.transform((w + e) / 2, (s + n) / 2, errcheck=True))


def place(crs, point):
    """PROJ's WGS 84 coordinates of a point and the transformation it used."""
    t = Transformer.from_crs(crs, "EPSG:4326", always_xy=True)
    lon, lat = t.transform(*point, errcheck=True)
    try:
        operation = t.get_last_used_operation().description
    except pyproj.exceptions.ProjError:
        # A transformer of a single operation keeps no last used one.
        operation = t.description
    return [lon, lat], operation


def metres(a, b):
    """Metres between two longitudes/latitudes, near enough at a millimetre."""
    dlon = (a[0] - b[0]) * math.cos(math.radians(b[1]))
    return math.hypot(dlon, a[1] - b[1]) * 111_195.0


def case(name, code, wkt, point):
    proj, operation = place(CRS.from_wkt(wkt), point)
    by_code, _ = place(CRS.from_epsg(code), point)
    return {
        "name": name,
        "code": f"EPSG:{code}",
        "wkt": wkt,
        "point": point,
        "proj": proj,
        "operation": operation,
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
    # PROJ warns that the best transformation's grid is missing.
    warnings.filterwarnings("ignore", message="Best transformation is not available")
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
    differences = [c for c in cases if metres(c["proj"], c["by_code"]) >= 1e-3]
    cases = [c for c in cases if c not in differences]
    for code in DIFFERENCES:
        assert any(c["name"] == f"EPSG:{code}, ESRI WKT 1" for c in differences), code
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
