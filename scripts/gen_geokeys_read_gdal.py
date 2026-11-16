"""Generate crates/terrio-crs/tests/data/geokeys_read_gdal.json: GeoKeys of user-defined
projected CRSs, as other writers than GDAL write them, and where GDAL 3.12.2 places a point in the
CRS it reads from them (opengeos/terrio#1012, item 35).

Each case's keys go into a one-pixel GeoTIFF (GeoKeyDirectoryTag and GeoDoubleParamsTag) that GDAL
reads (`GTIFGetOGISDefnAsOSR`); pyproj then places a point from WGS 84 (`always_xy`) in GDAL's WKT
2 of the CRS. The cases are those where GDAL's reading (libgeotiff's `GTIFFetchProjParms`) differs
from a reading by key names alone: ProjCenterEasting/NorthingGeoKey as the false easting and
northing, a Hotine oblique Mercator without ProjRectifiedGridAngleGeoKey (90°), and a polar
stereographic CRS whose ProjNatOriginLatGeoKey of 90° wins over ProjStdParallel1GeoKey.

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
    for name, method, doubles, point, *extra in CASES:
        shorts = USER + (extra[0] if extra else []) + [(3075, method)]
        keys, values = directory(shorts, doubles)
        path = "/vsimem/case.tif"
        gdal.FileFromMemBuffer(path, tiff(keys, values))
        ds = gdal.Open(path)
        srs = ds.GetSpatialRef()
        wkt = srs.ExportToWkt(["FORMAT=WKT2_2019"])
        ds = None
        gdal.Unlink(path)
        x, y = Transformer.from_crs(
            "EPSG:4326", CRS.from_wkt(wkt), always_xy=True
        ).transform(*point, errcheck=True)
        cases.append(
            {
                "name": name,
                "directory": keys,
                "doubles": values,
                "gdal_proj4": srs.ExportToProj4(),
                "point": list(point),
                "gdal": [x, y],
            }
        )
    OUT.write_text(
        json.dumps({"gdal": gdal.__version__, "cases": cases}, indent=1) + "\n"
    )


if __name__ == "__main__":
    main()
