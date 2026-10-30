#!/usr/bin/env python3
"""Compare how terrio and GDAL read self-touching Shapefile polygon rings (#308).

Esri allows a ring that touches itself at a vertex to enclose a hole. Since 3.13.0, GDAL splits
it in ``organizePolygons``, for records of several parts; GDAL 3.12 keeps it as stored. Each case
is a synthetic Shapefile written by OGR, which stores the rings as given (checked in the
``.shp``), read by OGR and converted by terrio to GeoPackage. With GDAL 3.12, terrio is compared
with GDAL 3.13.1's reading, frozen below. Identical: the same ISO WKB bytes. Within tolerance:
geometries equal under GEOS, both valid, but rings in another direction or from another start
(polygon by polygon for SKIP, whose polygons may overlap).

With GDAL 3.13 or later, each record is also read with ``OGR_ORGANIZE_POLYGONS`` set to
``DEFAULT``, ``CCW_INNER_JUST_AFTER_CW_OUTER`` and ``SKIP``, which GDAL splits the rings for as
well. The cases in ``KNOWN`` are reported as not compared.
"""

import argparse
import json
import os
import struct
import subprocess
import tempfile
from pathlib import Path

from osgeo import gdal, ogr

ROOT = Path(__file__).resolve().parents[1]

SQUARE = [(0, 0), (0, 10), (10, 10), (10, 0)]
HOLE = [(2, 6), (3, 6), (3, 7), (2, 7)]
ISLAND = [(20, 20), (20, 21), (21, 21), (21, 20)]
BIG = [(0, 0), (0, 30), (30, 30), (30, 0)]
ESRI_HOLE = SQUARE + [(5, 0), (6, 2), (4, 2), (5, 0)]

# Each case: its name, its polygons (each a list of rings, shell first) and GDAL 3.13.1's reading.
# OGR writes shells clockwise and holes counter-clockwise, so a ring meant to be stored clockwise
# is a polygon of its own.
CASES = [
    ("Esri hole, several parts", [[ESRI_HOLE, HOLE]],
     "POLYGON ((0 10,10 10,10 0,5 0,0 0,0 10),(2 6,3 6,3 7,2 7,2 6),(6 2,4 2,5 0,6 2))"),
    ("Esri hole, single part", [[ESRI_HOLE]],
     "POLYGON ((0 0,0 10,10 10,10 0,5 0,6 2,4 2,5 0,0 0))"),
    ("two Esri holes on one ring",
     [[[(0, 0), (0, 10), (5, 10), (4, 8), (6, 8), (5, 10), (10, 10), (10, 0), (5, 0), (6, 2), (4, 2), (5, 0)], HOLE]],
     "POLYGON ((0 10,5 10,10 10,10 0,5 0,0 0,0 10),(2 6,3 6,3 7,2 7,2 6),(4 8,6 8,5 10,4 8),(6 2,4 2,5 0,6 2))"),
    ("ring starting at the touch", [[[(5, 0), (6, 2), (4, 2), (5, 0), (0, 0), (0, 10), (10, 10), (10, 0)], HOLE]],
     "POLYGON ((0 0,0 10,10 10,10 0,5 0,0 0),(2 6,3 6,3 7,2 7,2 6),(6 2,4 2,5 0,6 2))"),
    ("loop on the left edge", [[[(0, 0), (0, 4), (2, 6), (2, 5), (0, 4), (0, 10), (10, 10), (10, 0)], HOLE]],
     "POLYGON ((0 0,0 4,2 6,2 5,0 4,0 10,10 10,10 0,0 0),(2 6,3 6,3 7,2 7,2 6))"),
    ("figure-eight", [[[(0, 0), (0, 2), (2, 2), (2, 0), (4, 0), (4, -2), (2, -2), (2, 0)]], [ISLAND]],
     "MULTIPOLYGON (((0 0,0 2,2 2,2 0,4 0,4 -2,2 -2,2 0,0 0)),((20 20,20 21,21 21,21 20,20 20)))"),
    ("spike", [[[(0, 0), (0, 4), (2, 4), (3, 5), (2, 4), (4, 4), (4, 0)]], [ISLAND]],
     "MULTIPOLYGON (((0 0,0 4,2 4,3 5,2 4,4 4,4 0,0 0)),((20 20,20 21,21 21,21 20,20 20)))"),
    ("loop wound like the ring", [[SQUARE + [(5, 0), (4, 2), (6, 2), (5, 0)]], [ISLAND]],
     "MULTIPOLYGON (((0 0,0 10,10 10,10 0,5 0,4 2,6 2,5 0,0 0)),((20 20,20 21,21 21,21 20,20 20)))"),
    ("consecutive duplicate points", [[[(0, 0), (0, 4), (0, 4), (4, 4), (4, 0)]], [ISLAND]],
     "MULTIPOLYGON (((0 0,0 4,0 4,4 4,4 0,0 0)),((20 20,20 21,21 21,21 20,20 20)))"),
    ("hole part as a figure-eight", [[BIG, [(2, 2), (6, 2), (6, 6), (10, 6), (10, 10), (6, 10), (6, 6), (2, 6)]]],
     "POLYGON ((0 0,0 30,30 30,30 0,0 0),(2 2,6 2,6 6,10 6,10 10,6 10,6 6,2 6,2 2))"),
    ("hole part enclosing an island", [[BIG, [(2, 2), (8, 2), (8, 8), (5, 8), (6, 6), (4, 6), (5, 8), (2, 8)]]],
     "MULTIPOLYGON (((0 0,0 30,30 30,30 0,0 0),(8 8,8 2,2 2,2 8,5 8,8 8)),((4 6,6 6,5 8,4 6)))"),
    ("island nested in an Esri hole",
     [[SQUARE + [(5, 0), (8, 2), (5, 6), (5, 4), (6, 5), (6, 4), (5, 4), (2, 2), (5, 0)], HOLE]],
     "POLYGON ((0 0,0 10,10 10,10 0,5 0,8 2,5 6,5 4,6 5,6 4,5 4,2 2,5 0,0 0),(2 6,3 6,3 7,2 7,2 6))"),
    ("stored hole touching the exterior", [[SQUARE, [(5, 10), (4, 8), (6, 8)]]],
     "POLYGON ((0 0,0 10,10 10,10 0,0 0),(5 10,4 8,6 8,5 10))"),
    ("stored holes, the first touching the exterior", [[SQUARE, [(5, 10), (4, 8), (6, 8)], HOLE]],
     "POLYGON ((0 0,0 10,10 10,10 0,0 0),(5 10,4 8,6 8,5 10),(2 6,3 6,3 7,2 7,2 6))"),
    ("stored hole touching the exterior, first vertex repeated", [[SQUARE, [(5, 10), (5, 10), (4, 8), (6, 8)]]],
     "POLYGON ((0 0,0 10,10 10,10 0,0 0),(5 10,5 10,4 8,6 8,5 10))"),
    ("Esri hole, ring stored from inside the hole",
     [[[(6, 2), (4, 2), (5, 0), (0, 0), (0, 10), (10, 10), (10, 0), (5, 0)], HOLE]],
     "POLYGON ((0 0,0 10,10 10,10 0,5 0,0 0),(2 6,3 6,3 7,2 7,2 6),(6 2,4 2,5 0,6 2))"),
    ("two Esri holes, ring stored from inside the upper one",
     [[[(4, 8), (6, 8), (5, 10), (10, 10), (10, 0), (5, 0), (6, 2), (4, 2), (5, 0), (0, 0), (0, 10), (5, 10)], HOLE]],
     "POLYGON ((10 10,10 0,5 0,0 0,0 10,5 10,10 10),(2 6,3 6,3 7,2 7,2 6),(6 2,4 2,5 0,6 2),(4 8,6 8,5 10,4 8))"),
    ("larger Esri hole, ring stored from halfway along it",
     [[[(6, 4), (5, 5), (4, 4), (4, 2), (5, 0), (0, 0), (0, 10), (10, 10), (10, 0), (5, 0), (6, 2)], HOLE]],
     "POLYGON ((0 0,0 10,10 10,10 0,5 0,0 0),(2 6,3 6,3 7,2 7,2 6),(6 2,6 4,5 5,4 4,4 2,5 0,6 2))"),
]

# The methods compared besides the default, ONLY_CCW, with GDAL 3.13 or later.
METHODS = ["DEFAULT", "CCW_INNER_JUST_AFTER_CW_OUTER", "SKIP"]

# (method, case) pairs terrio reads differently, and why.
CCW_INNER_AREAS = ("GDAL switches from CCW_INNER_JUST_AFTER_CW_OUTER to DEFAULT when it splits a ring, "
                   "with no area for the rings before it, which then enclose no other ring; "
                   "terrio organizes them as for ONLY_CCW")
KNOWN = {
    ("CCW_INNER_JUST_AFTER_CW_OUTER", "hole part enclosing an island"): CCW_INNER_AREAS,
    ("CCW_INNER_JUST_AFTER_CW_OUTER", "stored hole touching the exterior, first vertex repeated"):
        CCW_INNER_AREAS + "; GDAL also takes a repeated first vertex for a self-touching ring",
}


def write(path, polygons):
    """Writes one feature of the given polygons to a Shapefile and returns the rings as stored."""
    dataset = ogr.GetDriverByName("ESRI Shapefile").CreateDataSource(str(path))
    layer = dataset.CreateLayer(path.stem, None, ogr.wkbMultiPolygon)
    multi = ogr.Geometry(ogr.wkbMultiPolygon)
    for rings in polygons:
        polygon = ogr.Geometry(ogr.wkbPolygon)
        for ring in rings:
            linear = ogr.Geometry(ogr.wkbLinearRing)
            for x, y in ring + [ring[0]]:
                linear.AddPoint_2D(float(x), float(y))
            polygon.AddGeometry(linear)
        multi.AddGeometry(polygon)
    feature = ogr.Feature(layer.GetLayerDefn())
    feature.SetGeometry(multi)
    layer.CreateFeature(feature)
    feature = layer = dataset = None
    record = path.read_bytes()[108:]
    parts, points = struct.unpack("<ii", record[36:44])
    starts = list(struct.unpack(f"<{parts}i", record[44:44 + 4 * parts])) + [points]
    xy = struct.unpack(f"<{2 * points}d", record[44 + 4 * parts:44 + 4 * parts + 16 * points])
    coords = [(xy[2 * i], xy[2 * i + 1]) for i in range(points)]
    return [coords[starts[i]:starts[i + 1]] for i in range(parts)]


def read(path):
    """The geometry of the first feature."""
    dataset = ogr.Open(str(path))
    feature = dataset.GetLayer(0).GetNextFeature()
    return feature.GetGeometryRef().Clone()


def geos_equal(a, b):
    """Whether two valid geometries are equal under GEOS: an empty symmetric difference."""
    return a.IsValid() and b.IsValid() and a.SymDifference(b).IsEmpty()


def parts(g):
    """The polygons of a geometry."""
    return [g.GetGeometryRef(i) for i in range(g.GetGeometryCount())] if g.GetGeometryType() == ogr.wkbMultiPolygon else [g]


def compare(terrio, directory, name, polygons, frozen, live, method=None):
    path = directory / (name.replace(" ", "_").replace(",", "") + ".shp")
    stored = write(path, polygons)
    given = [[(float(x), float(y)) for x, y in ring + [ring[0]]] for rings in polygons for ring in rings]
    if stored != given:
        raise RuntimeError("OGR did not store the rings as given")
    output = path.with_suffix(".gpkg")
    command = [terrio, "convert", str(path), str(output)] + (["--oo", f"ORGANIZE_POLYGONS={method}"] if method else [])
    result = subprocess.run(command, capture_output=True, text=True)
    if result.returncode:
        raise RuntimeError(f"terrio convert failed: {result.stderr.strip()}")
    gdal.SetConfigOption("OGR_ORGANIZE_POLYGONS", method)
    try:
        reference = read(path) if live else ogr.CreateGeometryFromWkt(frozen)
    finally:
        gdal.SetConfigOption("OGR_ORGANIZE_POLYGONS", None)
    actual = read(output)
    against = "" if live else "GDAL 3.13.1's reading (frozen); "
    if bytes(actual.ExportToIsoWkb()) == bytes(reference.ExportToIsoWkb()):
        return "identical", against.rstrip("; ")
    # OGR's Equals compares the structure; GEOS's equality ignores ring direction and start.
    if geos_equal(reference, actual):
        return "within tolerance", f"{against}equal under GEOS, both valid; rings in another direction or from another start"
    if method == "SKIP" and len(parts(reference)) == len(parts(actual)) and all(map(geos_equal, parts(reference), parts(actual))):
        return "within tolerance", "polygon by polygon equal under GEOS, each valid; rings in another direction or from another start"
    return "differs", f"{against}GDAL {reference.ExportToWkt()}; terrio {actual.ExportToWkt()}"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", nargs="?", help="unused: the cases are synthetic")
    parser.add_argument("--terrio", default=str(ROOT / "target/release/terrio"))
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    # Each run picks its method itself: an inherited one would change terrio's default or GDAL's.
    for name in ("OGR_ORGANIZE_POLYGONS", "TERRIO_ORGANIZE_POLYGONS"):
        os.environ.pop(name, None)
    ogr.UseExceptions()
    # GDAL warns about the wrongly wound rings of some cases; they are part of the comparison.
    gdal.PushErrorHandler("CPLQuietErrorHandler")
    live = int(gdal.VersionInfo("VERSION_NUM")) >= 3130000
    results = []
    runs = [(None, name, polygons, frozen) for name, polygons, frozen in CASES]
    if live:
        runs += [(method, name, polygons, None) for method in METHODS for name, polygons, _ in CASES]
    for method, name, polygons, frozen in runs:
        label = f"{name}, {method}" if method else name
        if (method, name) in KNOWN:
            results.append({"name": label, "status": "not compared", "detail": f"known difference: {KNOWN[method, name]}"})
            continue
        with tempfile.TemporaryDirectory(prefix="terrio-shapefile-rings-") as tmp:
            try:
                status, detail = compare(args.terrio, Path(tmp), name, polygons, frozen, live, method)
            except Exception as error:
                status, detail = "differs", repr(error)
        results.append({"name": label, "status": status, "detail": detail})
    print(json.dumps(results, indent=2) if args.json else "\n".join(f"{r['name']}: {r['status']} {r['detail']}" for r in results))
    return int(any(r["status"] == "differs" for r in results))


if __name__ == "__main__":
    raise SystemExit(main())
