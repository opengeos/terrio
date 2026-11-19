"""Generate crates/terrio-crs/tests/data/wkt1_proj_reads.json: WKT 1 that PROJ 9.9.0 reads and
where it places points in them.

- ESRI's `Mercator_Variant_A` (EPSG Guidance Note 7-2's example of Mercator (variant A),
  EPSG:3002 Makassar / NEIEZ, as an ESRI `.prj` names it), and the same with a latitude of origin
  and a standard parallel, which PROJ leaves out of this method;
- ESRI's `Lambert_Conformal_Conic` with one `Standard_Parallel_1` other than its
  `Latitude_Of_Origin`, which PROJ reads as 1SP at the latitude of origin;
- GDAL's `Mercator_1SP` with a latitude of origin other than 0 and a scale of 1, which PROJ reads
  as Mercator (variant B) with that latitude as its standard parallel;
- GDAL's `Lambert_Conformal_Conic_2SP` (and `_Belgium`) with a `scale_factor`, which PROJ
  ignores, and ESRI's `Lambert_Conformal_Conic` with a `Scale_Factor`, which PROJ applies;
- WKT 1 with a node of an unknown keyword (`rustgis:unapplied`, as RustGIS marks a WKT 2
  `BOUNDCRS` it converts to WKT 1, `a-b`, `a.b`, `1x`, an empty one), on a datum and at the root
  of a `GEOGCS` and a `PROJCS`, which PROJ skips whatever its keyword;
- WKT 1 whose keyword PROJ refuses: one with a space, a vertical tab, a NUL or an unclosed quote
  in it.

EPSG:32631's PROJJSON (`CRS.to_json()`) with a NUL (`\\u0000`) in a parameter's name is read
by `CRS.from_json`, with its `to_epsg()` and a point by `Transformer.from_crs("EPSG:4326", crs,
always_xy=True)`. So is a bound CRS's PROJJSON (UTM 32N with `+towgs84`) with a NUL in its
name, with `to_proj4()` and a point by `Transformer.from_crs(crs, "EPSG:4326", always_xy=True)`.
Mollweide's PROJJSON with a NUL in its name is read, with `to_proj4()`; PROJ's `to_wkt()` of it
ends at the NUL. A WKT 1 with `TOWGS84` is written by `CRS.to_json_dict()`: its type and its transformation's
parameters.

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


LCC = (
    'PARAMETER["standard_parallel_1",33],PARAMETER["standard_parallel_2",45],'
    'PARAMETER["latitude_of_origin",23],PARAMETER["central_meridian",-96],'
    'PARAMETER["false_easting",1000000],PARAMETER["false_northing",500000]'
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
    ("a keyword with a quoted run", nad27(',a"b"["x"]'), (-79.0, 43.0)),
    ("a keyword with a quoted space", nad27(',a"b c"["x"]'), (-79.0, 43.0)),
    ("a keyword with a quoted comma", nad27(',a"b,c"["x"]'), (-79.0, 43.0)),
    ("a quoted keyword", nad27(',"q"["x"]'), (-79.0, 43.0)),
    ("a value with a quoted comma", nad27(',x["a",b"c,d"e]'), (-79.0, 43.0)),
    (
        "a value whose quote runs over a sibling node",
        nad27(',x["a",b"],y["c"],z[d"]'),
        (-79.0, 43.0),
    ),
    ("a vertical tab before a keyword", nad27(',\vq["x"]'), (-79.0, 43.0)),
    ("text after a NUL", nad27() + "\0garbage", (-79.0, 43.0)),
    (
        "NAD27 / UTM zone 17N with a keyword with a hyphen",
        utm17(',a-b["x"]').replace('PROJCS["unknown"', 'PROJCS["NAD27 / UTM zone 17N"'),
        (-79.0, 43.0),
    ),
    (
        "a WKT 2 CRS with a node of an unknown keyword",
        # On the datum.
        CRS("EPSG:26717")
        .to_wkt()
        .replace(
            'LENGTHUNIT["metre",1]]]', 'LENGTHUNIT["metre",1]],vendor:note["x"]]', 1
        ),
        (-79.0, 43.0),
    ),
    (
        "Mercator (variant A), EPSG's name, with a latitude of origin of 0",
        makassar(
            "Mercator (variant A)", VARIANT_A + ',PARAMETER["Latitude_Of_Origin",0]'
        ),
        (120.0, -3.0),
    ),
    (
        "Mercator (variant A) with a standard parallel and a latitude of center",
        makassar(
            "Mercator (variant A)",
            VARIANT_A
            + ',PARAMETER["Standard_Parallel_1",10],PARAMETER["Latitude_Of_Center",5]',
        ),
        (120.0, -3.0),
    ),
    (
        "MERCATOR_VARIANT_A, ESRI's name in capitals, with a latitude of origin",
        makassar(
            "MERCATOR_VARIANT_A", VARIANT_A + ',PARAMETER["Latitude_Of_Origin",5]'
        ),
        (120.0, -3.0),
    ),
    # ESRI's Lambert_Conformal_Conic with one standard parallel: 1SP at the latitude of origin,
    # Standard_Parallel_1 left out (opengeos/terrio#1012 item 30).
    (
        "Lambert_Conformal_Conic, ESRI's name, with one parallel other than the origin's",
        f'PROJCS["l",{nad27()},PROJECTION["Lambert_Conformal_Conic"],'
        'PARAMETER["False_Easting",1000000],PARAMETER["False_Northing",500000],'
        'PARAMETER["Central_Meridian",-96],PARAMETER["Standard_Parallel_1",42],'
        'PARAMETER["Scale_Factor",0.99],PARAMETER["Latitude_Of_Origin",40],UNIT["Meter",1.0]]',
        (-90.0, 45.0),
    ),
    # GDAL's LCC 2SP methods have no scale: PROJ ignores a scale_factor given with them; ESRI's
    # Lambert_Conformal_Conic applies its Scale_Factor (opengeos/terrio#1012 item 29).
    (
        "Lambert_Conformal_Conic_2SP with a scale factor",
        f'PROJCS["l",{nad27()},PROJECTION["Lambert_Conformal_Conic_2SP"],{LCC},'
        'PARAMETER["scale_factor",0.99],UNIT["metre",1]]',
        (-96.0, 40.0),
    ),
    (
        "Lambert_Conformal_Conic_2SP_Belgium with a scale factor",
        f'PROJCS["l",{nad27()},PROJECTION["Lambert_Conformal_Conic_2SP_Belgium"],{LCC},'
        'PARAMETER["scale_factor",0.99],UNIT["metre",1]]',
        (-96.0, 40.0),
    ),
    (
        "Lambert_Conformal_Conic, ESRI's name, with a scale factor",
        f'PROJCS["l",{nad27()},PROJECTION["Lambert_Conformal_Conic"],{LCC},'
        'PARAMETER["Scale_Factor",0.99],UNIT["Meter",1.0]]',
        (-96.0, 40.0),
    ),
    # GDAL's Mercator_1SP with a latitude of origin and a scale of 1: Mercator (variant B) with
    # that latitude as its standard parallel (GDAL #3026, opengeos/terrio#1012 item 24).
    (
        "Mercator_1SP, GDAL's name, with a latitude of origin",
        makassar(
            "Mercator_1SP",
            VARIANT_A.replace("0.997", "1") + ',PARAMETER["latitude_of_origin",30]',
        ),
        (120.0, -3.0),
    ),
    (
        "Mercator_1SP with a southern latitude of origin and no scale",
        makassar(
            "Mercator_1SP",
            VARIANT_A.replace(',PARAMETER["Scale_Factor",0.997]', "")
            + ',PARAMETER["latitude_of_origin",-30]',
        ),
        (120.0, -3.0),
    ),
]

# Refused by `CRS.from_wkt`.
REFUSED = [
    ("a keyword with a space", nad27(',a b["x"]')),
    ("a keyword with an unclosed quote", nad27(',a"b["x"]')),
    ("a keyword with a vertical tab", nad27(',a\vb["x"]')),
    ("a keyword with a NUL", nad27(',a\0b["x"]')),
    ("a value with an unclosed quote", nad27(',x["a",b"c]')),
    ("a value whose quote runs to a later quote", nad27(',x["a",b"],y["c"]')),
]

# Read by `CRS.from_wkt`, but neither transformed nor written as a PROJ string.
NOT_TRANSFORMED = [
    (
        "Mercator (variant A), EPSG's name, with a latitude of origin",
        makassar(
            "Mercator (variant A)", VARIANT_A + ',PARAMETER["Latitude_Of_Origin",5]'
        ),
    ),
    (
        "Mercator (variant A) with a latitude of natural origin",
        makassar(
            "Mercator (variant A)",
            VARIANT_A + ',PARAMETER["Latitude_of_natural_origin",-5]',
        ),
    ),
    (
        "Mercator_(variant_A) with a latitude of origin",
        makassar(
            "Mercator_(variant_A)", VARIANT_A + ',PARAMETER["Latitude_Of_Origin",1e-10]'
        ),
    ),
]


def main():
    assert pyproj.proj_version_str == "9.9.0", pyproj.proj_version_str
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
        if "WKT 2" in name:
            # The node went into PROJ's WKT 2.
            assert "vendor:note" in wkt, name
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
                "epsg": crs.to_epsg(),
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
    # EPSG:32631's PROJJSON with a NUL (`\u0000`) in a parameter's name, which PROJ keeps in the
    # name and reads as EPSG:32631.
    plain = CRS("EPSG:32631").to_json()
    text = plain.replace(
        '"Latitude of natural origin"', '"Latitude of\\u0000 natural origin"', 1
    )
    assert text != plain
    crs = CRS.from_json(text)
    t = Transformer.from_crs("EPSG:4326", crs, always_xy=True)
    projjson = {
        "projjson": text,
        "point": (3.0, 45.0),
        "proj": list(t.transform(3.0, 45.0, errcheck=True)),
        "epsg": crs.to_epsg(),
    }
    # A bound CRS (UTM 32N on International 1924 with `+towgs84`) in PROJJSON with a NUL in its
    # name, which PROJ keeps, still bound by its shift.
    plain = CRS(
        "+proj=utm +zone=32 +ellps=intl +towgs84=-87,-98,-121 +units=m +type=crs"
    ).to_json()
    text = plain.replace('"name":"unknown"', '"name":"un\\u0000known"', 1)
    assert text != plain
    crs = CRS.from_json(text)
    assert json.loads(text)["source_crs"]["name"] == "un\0known"
    assert crs.to_json_dict()["type"] == "BoundCRS"
    t = Transformer.from_crs(crs, "EPSG:4326", always_xy=True)
    bound = {
        "projjson": text,
        "point": (500000.0, 5000000.0),
        "proj": list(t.transform(500000.0, 5000000.0, errcheck=True)),
        "proj4": crs.to_proj4(),
    }
    # Mollweide, which GeoTIFF keys can't describe, in PROJJSON with a NUL in its name: PROJ's
    # WKT of it, a C string, ends at the NUL.
    plain = CRS("+proj=moll +lon_0=0 +datum=WGS84 +units=m +type=crs").to_json()
    text = plain.replace('"name":"unknown"', '"name":"Moll\\u0000weide"', 1)
    assert text != plain
    crs = CRS.from_json(text)
    assert json.loads(text)["name"] == "Moll\0weide"
    assert crs.to_wkt().startswith('PROJCRS["Moll') and len(crs.to_wkt()) < 20
    mollweide = {"projjson": text, "proj4": crs.to_proj4()}
    # A WKT 1 with TOWGS84, which PROJ writes as a PROJJSON `BoundCRS`; terrio stores it with a
    # NUL in its quoted name, as a PROJJSON name can give one.
    wkt = (
        'GEOGCS["unknown",DATUM["Local",SPHEROID["International 1924",6378388,297],'
        'TOWGS84[-87,-98,-121,0,0,0,0]],PRIMEM["Greenwich",0],'
        'UNIT["degree",0.0174532925199433]]'
    )
    exported = CRS.from_wkt(wkt).to_json_dict()
    towgs84 = {
        "wkt": wkt,
        "type": exported["type"],
        "parameters": [p["value"] for p in exported["transformation"]["parameters"]],
    }
    untransformed = []
    for name, wkt in NOT_TRANSFORMED:
        crs = CRS.from_wkt(wkt)
        try:
            Transformer.from_crs(crs.geodetic_crs, crs, always_xy=True)
        except pyproj.exceptions.ProjError:
            pass
        else:
            raise AssertionError(f"PROJ transforms {name}")
        try:
            crs.to_proj4()
        except CRSError:
            pass
        else:
            raise AssertionError(f"PROJ writes {name} as a PROJ string")
        untransformed.append({"name": name, "wkt": wkt})
    OUT.write_text(
        json.dumps(
            {
                "proj_version": pyproj.proj_version_str,
                "read": read,
                "to_wgs84": to_wgs84,
                "projjson": projjson,
                "bound_projjson": bound,
                "towgs84_projjson": towgs84,
                "mollweide_projjson": mollweide,
                "refused": refused,
                "untransformed": untransformed,
            },
            indent=1,
            ensure_ascii=False,
        )
        + "\n",
        encoding="utf-8",
    )


if __name__ == "__main__":
    main()
