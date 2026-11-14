"""Generate crates/terrio-crs/tests/data/projjson_key_order.txt: the key order of every kind of
object PROJ 9.8.1 writes in PROJJSON, one comma-separated list of keys per line.

The objects are those of the PROJJSON of every EPSG, ESRI and IAU_2015 CRS in PROJ's database,
and of a few CRSs given by PROJ strings (bound and planetary). terrio-crs writes PROJJSON's keys
in one fixed order (``src/projjson_order.rs``); its tests check that order against these lists.

Usage: PROJ_NETWORK=OFF python scripts/gen_projjson_key_order.py (needs pyproj 3.8.0)
"""

import json
from pathlib import Path

import pyproj
from pyproj import CRS, database

OUT = (
    Path(__file__).resolve().parents[1]
    / "crates/terrio-crs/tests/data/projjson_key_order.txt"
)
PROJ_STRINGS = [
    "+proj=longlat +ellps=bessel +towgs84=598.1,73.7,418.2,0.202,0.045,-2.455,6.7 +type=crs",
    "+proj=tmerc +ellps=GRS80 +towgs84=1,2,3 +type=crs",
    "+proj=longlat +a=2440530 +b=2438260 +type=crs",
]


def main():
    assert pyproj.proj_version_str == "9.8.1", pyproj.proj_version_str
    seen = set()

    def walk(value):
        if isinstance(value, dict):
            seen.add(",".join(value))
            for v in value.values():
                walk(v)
        elif isinstance(value, list):
            for v in value:
                walk(v)

    for authority in ("EPSG", "ESRI", "IAU_2015"):
        for code in database.get_codes(authority, "CRS"):
            try:
                crs = CRS.from_authority(authority, code)
            except pyproj.exceptions.CRSError:
                continue
            walk(json.loads(crs.to_json()))
    for definition in PROJ_STRINGS:
        walk(json.loads(CRS(definition).to_json()))
    OUT.write_text("".join(f"{keys}\n" for keys in sorted(seen)))


if __name__ == "__main__":
    main()
