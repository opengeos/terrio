"""Generate crates/terrio-crs/tests/data/wkt_root_code.json: where PROJ 9.8.1 places points of
WKTs whose root `AUTHORITY` or `ID` is not the CRS their nodes describe (opengeos/terrio#1012,
item 16), or whose name is that of an Esri CRS on another datum (item 14), and the code PROJ
identifies (`to_epsg()`, and an ESRI one by `to_authority()`). Controls: WKTs of the right code,
an Esri `.prj` identified by its name and datum, and PROJ's WKT 2 of a method terrio identifies by
its code (Belgian Lambert 72).

PROJ keeps a WKT's content and identifies a code only when the content is that CRS. Each case is a
WKT, a point in WGS 84 and that point in the CRS by `Transformer.from_crs("EPSG:4326", wkt,
always_xy=True)`, or `null` where PROJ refuses the transformation.

Usage: PROJ_NETWORK=OFF python scripts/gen_wkt_root_code.py (needs pyproj 3.8.0)
"""

import json
import os
from pathlib import Path

import pyproj
from pyproj import CRS, Transformer

OUT = Path(__file__).resolve().parents[1] / "crates/terrio-crs/tests/data/wkt_root_code.json"

BNG = (
    'PROJCS["OSGB 1936 / British National Grid",GEOGCS["OSGB 1936",DATUM["OSGB_1936",'
    'SPHEROID["Airy 1830",6377563.396,299.3249646,AUTHORITY["EPSG","7001"]],'
    "TOWGS84[{towgs84}],AUTHORITY[\"EPSG\",\"6277\"]],"
    'PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],'
    'AUTHORITY["EPSG","4277"]],PROJECTION["{method}"],PARAMETER["latitude_of_origin",49],'
    'PARAMETER["central_meridian",-2],PARAMETER["scale_factor",0.9996012717],PARAMETER["false_easting",400000],'
    'PARAMETER["false_northing",-100000],UNIT["metre",1,AUTHORITY["EPSG","9001"]],AXIS["Easting",EAST],'
    'AXIS["Northing",NORTH],AUTHORITY["EPSG","{code}"]]'
)
BNG_SHIFT = "446.448,-125.157,542.06,0.15,0.247,0.842,-20.489"
NTF_2D = (
    'GEOGCS["unnamed ellipsoid",DATUM["unknown",SPHEROID["unnamed",6378249.2,293.466021294],'
    'TOWGS84[-168,-60,320,0,0,0,0]],PRIMEM["Paris",2.337229166666667],UNIT["degree",0.0174532925199433],'
    'AUTHORITY["EPSG","{code}"]]'
)


def lo19(code):
    wkt = CRS.from_epsg(2048).to_wkt()
    i = wkt.rindex('ID["EPSG",2048]')
    return wkt[:i] + f'ID["EPSG",{code}]' + wkt[i + len('ID["EPSG",2048]') :]


def main():
    assert pyproj.__version__ == "3.8.0" and pyproj.proj_version_str == "9.8.1", pyproj.proj_version_str
    assert os.environ.get("PROJ_NETWORK") == "OFF"
    global MONTE_MARIO
    MONTE_MARIO = CRS.from_epsg(3003).to_wkt("WKT1_ESRI")
    cases = [
        ("British National Grid with its own code", BNG.format(towgs84=BNG_SHIFT, method="Transverse_Mercator", code=27700), (-2.0, 54.0)),
        ("British National Grid declaring EPSG:4277", BNG.format(towgs84=BNG_SHIFT, method="Transverse_Mercator", code=4277), (-2.0, 54.0)),
        ("an unknown method declaring EPSG:27700", BNG.format(towgs84="375,-111,431,0,0,0,0", method="Unheard_Of", code=27700), (-2.0, 54.0)),
        ("Hartebeesthoek94 / Lo19 declaring EPSG:1", lo19(1), (18.5, -33.9)),
        ("Hartebeesthoek94 / Lo19 with its own code", lo19(2048), (18.5, -33.9)),
        ("a 2D NTF (Paris) in degrees declaring EPSG:7400", NTF_2D.format(code=7400), (2.35, 48.86)),
        ("Esri Monte_Mario_Italy_1", MONTE_MARIO, (12.5, 41.9)),
        ("Esri Monte_Mario_Italy_1 on a datum renamed D_Local", MONTE_MARIO.replace("D_Monte_Mario", "D_Local"), (12.5, 41.9)),
        ("Esri USA_Contiguous_Albers_Equal_Area_Conic", CRS.from_user_input("ESRI:102003").to_wkt("WKT1_ESRI"), (-96.0, 40.0)),
        ("Belge 1972 / Belgian Lambert 72 with its own code", CRS.from_epsg(31300).to_wkt(), (4.4, 50.8)),
    ]
    out = []
    for name, wkt, point in cases:
        crs = CRS.from_wkt(wkt)
        try:
            x, y = Transformer.from_crs("EPSG:4326", crs, always_xy=True).transform(*point, errcheck=True)
            proj = [x, y]
        except pyproj.exceptions.ProjError:
            proj = None
        authority = crs.to_authority()
        esri = int(authority[1]) if authority and authority[0] == "ESRI" else None
        out.append({"name": name, "wkt": wkt, "point": list(point), "proj": proj, "epsg": crs.to_epsg(), "esri": esri})
    OUT.write_text(json.dumps({"proj": pyproj.proj_version_str, "cases": out}, indent=1) + "\n")


if __name__ == "__main__":
    main()
