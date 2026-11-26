"""Generate crates/terrio-crs/tests/data/geokeys_read_gdal.json: GeoKeys of user-defined
projected CRSs, as other writers than GDAL write them, and where GDAL 3.12.2 places a point in the
CRS it reads from them (opengeos/terrio#1012, item 35).

Each case's keys go into a one-pixel GeoTIFF (GeoKeyDirectoryTag and GeoDoubleParamsTag) that GDAL
reads (`GTIFGetOGISDefnAsOSR`); pyproj then places a point from WGS 84 (`always_xy`) in GDAL's WKT
2 of the CRS. The cases are those where GDAL's reading (libgeotiff's `GTIFFetchProjParms`) differs
from a reading by key names alone: ProjCenterEasting/NorthingGeoKey as the false easting and
northing, a Hotine oblique Mercator without ProjRectifiedGridAngleGeoKey (90°), and a polar
stereographic CRS whose ProjNatOriginLatGeoKey of 90° wins over ProjStdParallel1GeoKey.

`KEYED` cases give their own keys (#1012 items 32 to 35): a projected code alone, read as the
registry's CRS (a deprecated code's replacement) in the keys' linear unit; an ellipsoid, datum or
prime meridian by its code alone, looked up in the registry; Mercator's scale and latitude keys;
Laborde oblique Mercator. A point on a sphere, which pyproj does not place from
WGS 84, is placed from the CRS's own geographic CRS (`"from": "geodetic"`).

Usage: PROJ_NETWORK=OFF python scripts/gen_geokeys_read_gdal.py (needs GDAL 3.12.2's Python
bindings and pyproj)
"""

import json
import os
import struct
from pathlib import Path

import pyproj
from pyproj import CRS, Transformer

# GDAL reads the PROJ database of pyproj's PROJ, where its own has none.
os.environ.setdefault("PROJ_DATA", pyproj.datadir.get_data_dir())

from osgeo import gdal  # noqa: E402

gdal.UseExceptions()
OUT = (
    Path(__file__).resolve().parents[1]
    / "crates/terrio-crs/tests/data/geokeys_read_gdal.json"
)

# A projected CRS of user-defined parameters on WGS 84 in metres.
USER = [(1024, 1), (1025, 1), (2048, 4326), (3072, 32767), (3074, 32767), (3076, 9001)]
CASES = [
    (
        "Transverse Mercator with ProjCenterEasting/NorthingGeoKey",
        1,
        [(3080, 9.0), (3081, 0.0), (3090, 500000.0), (3091, 100.0), (3092, 0.9996)],
        (10.0, 45.0),
    ),
    (
        "Hotine oblique Mercator (variant A) without a rectified grid angle",
        3,
        [
            (3088, 10.0),
            (3089, 40.0),
            (3094, 30.0),
            (3093, 1.0),
            (3082, 0.0),
            (3083, 0.0),
        ],
        (11.0, 41.0),
    ),
    (
        "Hotine oblique Mercator (variant B) without a rectified grid angle",
        9815,
        [
            (3088, 10.0),
            (3089, 40.0),
            (3094, 30.0),
            (3093, 0.9999),
            (3082, 1000.0),
            (3083, 2000.0),
        ],
        (11.0, 41.0),
    ),
    (
        "polar stereographic at 90° with a standard parallel of 71°",
        15,
        [(3095, 0.0), (3081, 90.0), (3078, 71.0), (3082, 0.0), (3083, 0.0)],
        (10.0, 80.0),
    ),
    (
        "polar stereographic at 90° with a standard parallel of 71° and a scale",
        15,
        [
            (3095, -45.0),
            (3081, 90.0),
            (3078, 71.0),
            (3092, 0.994),
            (3082, 2000000.0),
            (3083, 2000000.0),
        ],
        (10.0, 80.0),
    ),
    # The default grid angle is 90 in the GeoKeys' angular unit: 81° in grads.
    (
        "Hotine oblique Mercator (variant A) in grads without a rectified grid angle",
        3,
        [
            (3088, 11.0),
            (3089, 44.0),
            (3094, 30.0),
            (3093, 1.0),
            (3082, 0.0),
            (3083, 0.0),
        ],
        (11.0, 41.0),
        [(2054, 9105)],
    ),
    # Swiss oblique Mercator (variant B of azimuth 90) in grads, without a grid angle.
    (
        "Hotine oblique Mercator (variant B) of azimuth 90 in grads without a rectified grid angle",
        9815,
        [
            (3088, 8.2),
            (3089, 51.9),
            (3094, 90.0),
            (3093, 1.0),
            (3082, 600000.0),
            (3083, 200000.0),
        ],
        (7.4, 46.9),
        [(2054, 9105)],
    ),
    # A natural origin latitude of 90 wins over a southern standard parallel.
    (
        "polar stereographic at 90° with a standard parallel of -71°",
        15,
        [(3095, 0.0), (3081, 90.0), (3078, -71.0), (3082, 0.0), (3083, 0.0)],
        (10.0, 80.0),
    ),
    # A centre or false origin latitude is an origin latitude too, whatever the standard parallel.
    (
        "polar stereographic with a centre latitude of -90° and a standard parallel of -71°",
        15,
        [(3095, 0.0), (3089, -90.0), (3078, -71.0), (3082, 0.0), (3083, 0.0)],
        (10.0, -80.0),
    ),
    (
        "polar stereographic with a false origin latitude of -71°",
        15,
        [(3095, 0.0), (3085, -71.0), (3078, -60.0), (3082, 0.0), (3083, 0.0)],
        (10.0, -80.0),
    ),
]


# Model types, and a user-defined projection of a Transverse Mercator of these doubles.
PROJECTED, GEOGRAPHIC = [(1024, 1), (1025, 1)], [(1024, 2), (1025, 1)]
USER_TM = [(3072, 32767), (3074, 32767), (3076, 9001), (2048, 32767), (3075, 1)]
# Off the UTM zones (which PROJ refuses on a sphere), as GDAL would read 9°E as zone 32.
TM = [(3080, 10.0), (3081, 0.0), (3092, 0.9996), (3082, 500000.0), (3083, 0.0)]
KEYED = [
    (
        "deprecated EPSG:4855 beside GeographicTypeGeoKey 4258, by its own definition",
        PROJECTED + [(3072, 4855), (2048, 4258)],
        [],
        (10.0, 63.0),
        False,
    ),
    (
        "deprecated EPSG:26814 alone, as EPSG:26847 in metres",
        PROJECTED + [(3072, 26814)],
        [],
        (-69.0, 45.0),
        False,
    ),
    (
        "EPSG:32633 with ProjLinearUnitsGeoKey 9002, in feet",
        PROJECTED + [(3072, 32633), (3076, 9002)],
        [],
        (15.0, 50.0),
        False,
    ),
    (
        "EPSG:27700 with ProjLinearUnitsGeoKey 9002, in feet on OSGB36",
        PROJECTED + [(3072, 27700), (3076, 9002)],
        [],
        (-1.5, 52.5),
        False,
    ),
    (
        "geographic EPSG:4326 with ProjLinearUnitsGeoKey 9002, kept as EPSG:4326",
        PROJECTED + [(3072, 4326), (3076, 9002)],
        [],
        (10.0, 50.0),
        False,
    ),
    (
        "ellipsoid 7030 with a sphere's axes, which win",
        PROJECTED + USER_TM + [(2050, 32767), (2056, 7030)],
        TM + [(2057, 6371007.0), (2059, 0.0)],
        (10.0, 50.0),
        True,
    ),
    (
        "ellipsoid 7001 without axes",
        PROJECTED + USER_TM + [(2050, 32767), (2056, 7001)],
        TM,
        (10.0, 50.0),
        False,
    ),
    (
        "ellipsoid 7048 without axes",
        PROJECTED + USER_TM + [(2050, 32767), (2056, 7048)],
        TM,
        (10.0, 50.0),
        True,
    ),
    (
        "ellipsoid 7035 without axes",
        PROJECTED + USER_TM + [(2050, 32767), (2056, 7035)],
        TM,
        (10.0, 50.0),
        True,
    ),
    (
        "ellipsoid 7047 without axes",
        PROJECTED + USER_TM + [(2050, 32767), (2056, 7047)],
        TM,
        (10.0, 50.0),
        True,
    ),
    (
        "datum 6277 without ellipsoid, projected",
        PROJECTED + USER_TM + [(2050, 6277)],
        [
            (3080, -2.0),
            (3081, 49.0),
            (3092, 0.9996012717),
            (3082, 400000.0),
            (3083, -100000.0),
        ],
        (-2.0, 52.0),
        False,
    ),
    (
        "datum 6277 without ellipsoid, geographic",
        GEOGRAPHIC + [(2048, 32767), (2050, 6277)],
        [],
        (-2.0, 52.0),
        False,
    ),
    (
        "prime meridian 8903 without its longitude",
        PROJECTED + USER_TM + [(2050, 32767), (2056, 7022), (2051, 8903)],
        [(3080, 0.0), (3081, 0.0), (3092, 1.0), (3082, 0.0), (3083, 0.0)],
        (5.0, 46.0),
        False,
    ),
    (
        "Mercator with a standard parallel of 0 and a scale",
        PROJECTED + USER[2:] + [(3075, 7)],
        [(3078, 0.0), (3092, 0.9996), (3088, 10.0)],
        (12.0, 40.0),
        False,
    ),
    (
        "Mercator 1SP with a natural origin latitude",
        PROJECTED + USER[2:] + [(3075, 7)],
        [(3081, 10.0), (3080, 10.0)],
        (12.0, 40.0),
        False,
    ),
    (
        "Mercator with ProjScaleAtCenterGeoKey only",
        PROJECTED + USER[2:] + [(3075, 7)],
        [(3093, 0.9), (3088, 10.0)],
        (12.0, 40.0),
        False,
    ),
    (
        "Laborde oblique Mercator",
        PROJECTED + USER[2:] + [(3075, 4)],
        [
            (3088, 46.4372291),
            (3089, -18.9),
            (3094, 18.9),
            (3093, 0.9995),
            (3082, 400000.0),
            (3083, 800000.0),
        ],
        (47.0, -19.0),
        False,
    ),
    (
        "Laborde oblique Mercator without its azimuth, of 0",
        PROJECTED + USER[2:] + [(3075, 4)],
        [
            (3088, 46.4372291),
            (3089, -18.9),
            (3093, 0.9995),
            (3082, 400000.0),
            (3083, 800000.0),
        ],
        (47.0, -19.0),
        False,
    ),
]


def directory(shorts, doubles):
    """GeoKeyDirectoryTag and GeoDoubleParamsTag of the keys, in key order."""
    entries = [[key, 0, 1, value] for key, value in shorts]
    entries += [[key, 34736, 1, i] for i, (key, _) in enumerate(doubles)]
    entries.sort(key=lambda e: e[0])
    return [1, 1, 0, len(entries)] + [x for e in entries for x in e], [
        v for _, v in doubles
    ]


def tiff(keys, doubles):
    """A little-endian one-pixel GeoTIFF with the GeoKeys."""
    tags = [
        (256, 3, 1, struct.pack("<H", 1)),
        (257, 3, 1, struct.pack("<H", 1)),
        (258, 3, 1, struct.pack("<H", 8)),
        (259, 3, 1, struct.pack("<H", 1)),
        (262, 3, 1, struct.pack("<H", 1)),
        (273, 4, 1, None),
        (277, 3, 1, struct.pack("<H", 1)),
        (278, 3, 1, struct.pack("<H", 1)),
        (279, 4, 1, struct.pack("<I", 1)),
        (33550, 12, 3, struct.pack("<3d", 1, 1, 0)),
        (33922, 12, 6, struct.pack("<6d", 0, 0, 0, 0, 0, 0)),
        (34735, 3, len(keys), struct.pack(f"<{len(keys)}H", *keys)),
        (34736, 12, len(doubles), struct.pack(f"<{len(doubles)}d", *doubles)),
    ]
    data_offset = 8 + 2 + 12 * len(tags) + 4
    blob, entries = b"", []
    for tag, kind, count, data in tags:
        if data is None:
            entries.append((tag, kind, count, None))
        elif len(data) <= 4:
            entries.append((tag, kind, count, data.ljust(4, b"\0")))
        else:
            blob += b"\0" * ((data_offset + len(blob)) % 2)
            entries.append(
                (tag, kind, count, struct.pack("<I", data_offset + len(blob)))
            )
            blob += data
    strip = struct.pack("<I", data_offset + len(blob))
    ifd = struct.pack("<H", len(entries))
    for tag, kind, count, value in entries:
        ifd += struct.pack("<HHI", tag, kind, count) + (
            strip if value is None else value
        )
    return b"II*\0" + struct.pack("<I", 8) + ifd + struct.pack("<I", 0) + blob + b"\0"


def main():
    assert gdal.__version__ == "3.12.2", gdal.__version__
    assert os.environ.get("PROJ_NETWORK") == "OFF"
    cases = []
    listed = [
        (
            name,
            USER + (extra[0] if extra else []) + [(3075, method)],
            doubles,
            point,
            False,
        )
        for name, method, doubles, point, *extra in CASES
    ]
    for name, shorts, doubles, point, geodetic in listed + KEYED:
        keys, values = directory(shorts, doubles)
        path = "/vsimem/case.tif"
        gdal.FileFromMemBuffer(path, tiff(keys, values))
        ds = gdal.Open(path)
        srs = ds.GetSpatialRef()
        wkt = srs.ExportToWkt(["FORMAT=WKT2_2019"])
        ds = None
        gdal.Unlink(path)
        crs = CRS.from_wkt(wkt)
        source = crs.geodetic_crs if geodetic else "EPSG:4326"
        x, y = Transformer.from_crs(source, crs, always_xy=True).transform(
            *point, errcheck=True
        )
        cases.append(
            {
                "name": name,
                "directory": keys,
                "doubles": values,
                "gdal_proj4": srs.ExportToProj4(),
                "gdal_code": srs.GetAuthorityCode(None),
                "point": list(point),
                "from": "geodetic" if geodetic else "wgs84",
                "gdal": [x, y],
            }
        )
    OUT.write_text(
        json.dumps({"gdal": gdal.__version__, "cases": cases}, indent=1) + "\n"
    )


if __name__ == "__main__":
    main()
