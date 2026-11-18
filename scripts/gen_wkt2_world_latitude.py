"""Generate crates/terrio-crs/tests/data/wkt2_world_latitude.json: WKT 2 world projections with a
latitude of natural origin their method has no use for, which PROJ 9.9.0 ignores
(opengeos/terrio#1012, item 20): its PROJ string and where it places a point from WGS 84
(`always_xy`).

Usage: PROJ_NETWORK=OFF python scripts/gen_wkt2_world_latitude.py (needs pyproj 3.8.0)
"""

import json
import os
from pathlib import Path

import pyproj
from pyproj import CRS, Transformer

OUT = (
    Path(__file__).resolve().parents[1]
    / "crates/terrio-crs/tests/data/wkt2_world_latitude.json"
)
DEG = 'ANGLEUNIT["degree",0.0174532925199433]'
METRE = 'LENGTHUNIT["metre",1]'
ELLIPSOIDS = {
    "WGS 84": f'ELLIPSOID["WGS 84",6378137,298.257223563,{METRE}]',
    "a sphere": f'ELLIPSOID["sphere",6371000,0,{METRE}]',
}
METHODS = ["Robinson", "Sinusoidal", "Mollweide", "Eckert IV", "Natural Earth"]


def wkt(method, ellipsoid):
    return (
        f'PROJCRS["x",BASEGEOGCRS["g",DATUM["d",{ellipsoid}],PRIMEM["Greenwich",0,{DEG}]],'
        f'CONVERSION["c",METHOD["{method}"],PARAMETER["Latitude of natural origin",55,{DEG},ID["EPSG",8801]],'
        f'PARAMETER["Longitude of natural origin",5,{DEG},ID["EPSG",8802]],'
        f'PARAMETER["False easting",0,{METRE},ID["EPSG",8806]],PARAMETER["False northing",0,{METRE},ID["EPSG",8807]]],'
        f'CS[Cartesian,2],AXIS["(E)",east,ORDER[1],{METRE}],AXIS["(N)",north,ORDER[2],{METRE}]]'
    )


def main():
    assert pyproj.__version__ == "3.8.0" and pyproj.proj_version_str == "9.9.0", (
        pyproj.proj_version_str
    )
    assert os.environ.get("PROJ_NETWORK") == "OFF"
    cases = []
    for method in METHODS:
        for name, ellipsoid in ELLIPSOIDS.items():
            w = wkt(method, ellipsoid)
            crs = CRS.from_wkt(w)
            point = (6.0, 54.0)
            x, y = Transformer.from_crs("EPSG:4326", crs, always_xy=True).transform(
                *point, errcheck=True
            )
            cases.append(
                {
                    "name": f"{method} on {name}",
                    "wkt": w,
                    "point": list(point),
                    "proj": [x, y],
                    "proj4": crs.to_proj4(),
                }
            )
    OUT.write_text(
        json.dumps({"proj": pyproj.proj_version_str, "cases": cases}, indent=1) + "\n"
    )


if __name__ == "__main__":
    main()
