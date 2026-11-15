"""Generate crates/terrio-crs/tests/data/wkt1_proj_reads.json: WKT 1 that PROJ 9.8.1 reads and
where it places points in them.

- ESRI's `Mercator_Variant_A` (EPSG Guidance Note 7-2's example of Mercator (variant A),
  EPSG:3002 Makassar / NEIEZ, as an ESRI `.prj` names it), and the same with a latitude of origin
  and a standard parallel, which PROJ leaves out of this method;
- WKT 1 with a node of an unknown keyword (`rustgis:unapplied`, as RustGIS marks a WKT 2
  `BOUNDCRS` it converts to WKT 1, `a-b`, `a.b`, `1x`, an empty one), on a datum and at the root
  of a `GEOGCS` and a `PROJCS`, which PROJ skips whatever its keyword;
- WKT 1 whose keyword PROJ refuses: one with a space or a quote in it.

Makassar / NEIEZ is also taken to WGS 84 by `Transformer.from_crs(crs, "EPSG:4326",
always_xy=True)`, with the code `CRS.to_epsg()` identifies.

Each read case is a WKT, a point in its geographic CRS (`CRS.geodetic_crs`) and the point in the
WKT's CRS by `Transformer.from_crs(crs.geodetic_crs, crs, always_xy=True)`, and its
`CRS.to_proj4()`.

Usage: PROJ_NETWORK=OFF python scripts/gen_wkt1_proj_reads.py (needs pyproj 3.8.0)
"""

import json
import os
import warnings
from pathlib import Path

import pyproj
import pyproj.datadir
import pyproj.network
from pyproj import CRS, Transformer
from pyproj.exceptions import CRSError

OUT = (
    Path(__file__).resolve().parents[1]
    / "crates/terrio-crs/tests/data/wkt1_proj_reads.json"
)
MAKASSAR_GEOG = (
    'GEOGCS["GCS_Makassar",DATUM["D_Makassar",SPHEROID["Bessel_1841",6377397.155,299.1528128]],'
    'PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]'
)
VARIANT_A = (
    'PARAMETER["False_Easting",3900000.0],PARAMETER["False_Northing",900000.0],'
    'PARAMETER["Central_Meridian",110.0],PARAMETER["Scale_Factor",0.997]'
)


def makassar(method="Mercator_Variant_A", params=VARIANT_A, unit='UNIT["Meter",1.0]'):
    return f'PROJCS["Makassar_NEIEZ",{MAKASSAR_GEOG},PROJECTION["{method}"],{params},{unit}]'


def nad27(mark=""):
    return (
        'GEOGCS["unknown",DATUM["North American Datum 1927",SPHEROID["Clarke 1866",6378206.4,'
        f'294.978698213898]{mark}],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]'
    )


def utm17(mark="", root_mark=""):
    return (
        f'PROJCS["unknown",{nad27(mark)},PROJECTION["Transverse_Mercator"],'
        'PARAMETER["Latitude_Of_Origin",0],PARAMETER["Central_Meridian",-81],'
        'PARAMETER["Scale_Factor",0.9996],PARAMETER["False_Easting",500000],'
        f'PARAMETER["False_Northing",0],UNIT["metre",1]{root_mark}]'
    )


UNAPPLIED = ',rustgis:unapplied["NTv2 transformation to WGS 84 (ntv2_0.gsb)"]'
UNTRANSFORMED = ',rustgis:untransformed["WGS 84"]'

READ = [
    ("Mercator_Variant_A", makassar(), (120.0, -3.0)),
    (
        "Mercator_Variant_A in US survey feet",
        makassar(unit='UNIT["Foot_US",0.3048006096012192]'),
        (120.0, -3.0),
    ),
    (
        "Mercator_Variant_A with a latitude of origin and a standard parallel",
        makassar(
            params=VARIANT_A
            + ',PARAMETER["Latitude_Of_Origin",5.0],PARAMETER["Standard_Parallel_1",10.0]'
        ),
        (120.0, -3.0),
    ),
    (
        "Mercator_Variant_A without a scale factor",
        makassar(params=VARIANT_A.replace(',PARAMETER["Scale_Factor",0.997]', "")),
        (120.0, -3.0),
    ),
    ("a marked datum", nad27(UNAPPLIED), (-79.0, 43.0)),
    ("a mark at the root", nad27()[:-1] + UNTRANSFORMED + "]", (-79.0, 43.0)),
    ("a projected CRS with a marked datum", utm17(UNAPPLIED), (-79.0, 43.0)),
    (
        "a projected CRS with a mark at its root",
        utm17(root_mark=UNTRANSFORMED),
        (-79.0, 43.0),
    ),
    ("a keyword with a hyphen", nad27(',a-b["x"]'), (-79.0, 43.0)),
    ("a keyword with a dot", nad27(',a.b["x",1]'), (-79.0, 43.0)),
    ("a keyword starting with a digit", nad27(',1x["x"]'), (-79.0, 43.0)),
    ("an empty keyword", nad27(',["x"]'), (-79.0, 43.0)),
    ("a keyword of other letters", nad27(',été["x"]'), (-79.0, 43.0)),
]

REFUSED = [
    ("a keyword with a space", nad27(',a b["x"]')),
    ("a keyword with a quote", nad27(',a"b["x"]')),
]


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
    warnings.simplefilter("ignore")
    read = []
    for name, wkt, point in READ:
        crs = CRS.from_wkt(wkt)
        t = Transformer.from_crs(crs.geodetic_crs, crs, always_xy=True)
        x, y = t.transform(*point, errcheck=True)
        read.append(
            {
                "name": name,
                "wkt": wkt,
                "point": point,
                "proj": [x, y],
                "proj4": crs.to_proj4(),
            }
        )
    # Makassar / NEIEZ to WGS 84: PROJ knows the CRS as EPSG:3002 and takes Makassar's
    # transformation to WGS 84.
    wkt = makassar()
    t = Transformer.from_crs(CRS.from_wkt(wkt), "EPSG:4326", always_xy=True)
    point = (5009726.58, 569150.82)
    to_wgs84 = {
        "wkt": wkt,
        "point": point,
        "proj": list(t.transform(*point, errcheck=True)),
        "epsg": CRS.from_wkt(wkt).to_epsg(),
    }
    refused = []
    for name, wkt in REFUSED:
        try:
            CRS.from_wkt(wkt)
        except CRSError:
            refused.append({"name": name, "wkt": wkt})
        else:
            raise AssertionError(f"PROJ reads {name}")
    OUT.write_text(
        json.dumps(
            {
                "proj_version": pyproj.proj_version_str,
                "read": read,
                "to_wgs84": to_wgs84,
                "refused": refused,
            },
            indent=1,
            ensure_ascii=False,
        )
        + "\n"
    )


if __name__ == "__main__":
    main()
