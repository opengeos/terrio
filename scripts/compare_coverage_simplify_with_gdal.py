"""Compare shared-edge coverage simplification with GDAL 3.13.3/GEOS.

Retained polygon edge coordinates and multiplicity must match exactly, ignoring
ring/component order and direction. Attributes, IDs, types, names and CRS match.
GDAL independently checks output coverage validity.
"""

import argparse
import json
import sqlite3
from pathlib import Path
import tempfile

from osgeo import ogr

from compare_coverage_with_gdal import counties, run, write


def polygon_edges(geometry):
    if geometry is None or geometry.IsEmpty():
        return []
    if geometry.GetGeometryName() == "LINEARRING":
        points = (
            [geometry.GetPointZM(i) for i in range(geometry.GetPointCount())]
            if geometry.IsMeasured()
            else geometry.GetPoints()
        )
        return [tuple(sorted((a, b))) for a, b in zip(points, points[1:])]
    return [
        edge
        for i in range(geometry.GetGeometryCount())
        for edge in polygon_edges(geometry.GetGeometryRef(i))
    ]


def read(path):
    dataset = ogr.Open(str(path))
    result = {}
    for layer in dataset:
        rows = []
        for feature in layer:
            g = feature.GetGeometryRef()
            if g is None or not g.IsValid():
                raise AssertionError(f"{path}: missing or invalid geometry")
            kind = ogr.GT_Flatten(g.GetGeometryType())
            fingerprint = (
                sorted(polygon_edges(g))
                if kind in (ogr.wkbPolygon, ogr.wkbMultiPolygon)
                else bytes(g.ExportToIsoWkb())
            )
            rows.append(
                (feature.GetFID(), feature.items(), g.GetGeometryType(), fingerprint)
            )
        with sqlite3.connect(path) as connection:
            dimensions = connection.execute(
                "SELECT geometry_type_name,z,m FROM gpkg_geometry_columns WHERE table_name=?",
                (layer.GetName(),),
            ).fetchone()
        result[layer.GetName()] = (
            rows,
            [(f.name, f.GetType(), f.GetSubType()) for f in layer.schema],
            layer.GetSpatialRef().Clone(),
            (layer.GetGeomType(), dimensions),
        )
    return result


def compare(reference, actual):
    a, b = read(reference), read(actual)
    if a.keys() != b.keys():
        raise AssertionError("layer names differ")
    samples = features = 0
    for name in a:
        ra, sa, ca, ta = a[name]
        rb, sb, cb, tb = b[name]
        if sa != sb or not ca.IsSame(cb):
            raise AssertionError(f"{name}: schema/CRS differ")
        if len(ra) != len(rb):
            raise AssertionError(f"{name}: feature count differs")
        for fa, fb in zip(ra, rb):
            if fa[:3] != fb[:3]:
                raise AssertionError(
                    f"{name}: ID/attributes/feature geometry type differs: {fa[:3]} vs {fb[:3]}"
                )
            if fa[3] != fb[3]:
                if isinstance(fa[3], bytes):
                    raise AssertionError(
                        f"{name} FID {fa[0]}: inactive geometry coordinates/structure differ"
                    )
                left, right = set(fa[3]), set(fb[3])
                raise AssertionError(
                    f"{name} FID {fa[0]}: retained edge multiset differs: {len(fa[3])}/{len(fb[3])} edges; {len(left - right)} reference-only, {len(right - left)} output-only"
                )
            features += 1
            samples += len(fa[3])
        if ta != tb:
            raise AssertionError(
                f"{name}: all feature geometries/attributes/IDs match exactly, but declared layer type differs: {ta}/{tb}"
            )
    return f"{features} features/{samples} exact retained edges, attributes, IDs, types, CRS and layer names"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    shapes, crs = counties(args.data)
    zigzag = [
        ogr.CreateGeometryFromWkt(wkt)
        for wkt in [
            "POLYGON ((0 0,2 0,1.8 1,2.2 2,1.8 3,2 4,0 4,0 0))",
            "POLYGON ((2 0,4 0,4 4,2 4,1.8 3,2.2 2,1.8 1,2 0))",
        ]
    ]
    small = [
        ogr.CreateGeometryFromWkt(
            "MULTIPOLYGON (((0 0,100 0,100 100,0 100,0 0),(10 10,10 11,11 11,11 10,10 10)),((200 200,201 200,201 201,200 201,200 200)))"
        )
    ]
    fixtures = [
        ("Tennessee counties", shapes, [0, 50, 200, 1000]),
        ("shared zigzag", zigzag, [0, 0.1, 1, 5]),
        ("small rings", small, [0, 5, 100]),
        (
            "empty polygons",
            [shapes[0], ogr.CreateGeometryFromWkt("POLYGON EMPTY")],
            [0, 100],
        ),
    ]
    for repeats in [1, 2]:
        coordinates = "0 0," + "1 0," * (repeats + 1) + "2 0,2 2,0 2,0 0"
        fixtures.append(
            (
                f"consecutive repeats {repeats}",
                [ogr.CreateGeometryFromWkt(f"POLYGON (({coordinates}))")],
                [0, 100],
            )
        )
    for dimensions in ["Z", "M", "ZM"]:
        values = {"Z": "10", "M": "100", "ZM": "10 100"}[dimensions]
        ring = ",".join(
            f"{x} {y} {values}"
            for x, y in [(0, 0), (2, 0), (4, 0), (4, 4), (0, 4), (0, 0)]
        )
        fixtures.append(
            (
                f"{dimensions} polygon",
                [ogr.CreateGeometryFromWkt(f"POLYGON {dimensions} (({ring}))")],
                [0, 1],
            )
        )
    results = []
    with tempfile.TemporaryDirectory(prefix="terrio-simplify-coverage-") as temp:
        root = Path(temp)
        for i, (name, features, tolerances) in enumerate(fixtures):
            source = root / f"source-{i}.gpkg"
            write(source, features, crs)
            for tolerance in tolerances:
                for preserve in [False, True]:
                    reference = root / f"ref-{i}-{tolerance}-{preserve}.gpkg"
                    actual = root / f"out-{i}-{tolerance}-{preserve}.gpkg"
                    options = ["--tolerance", str(tolerance)] + (
                        ["--preserve-boundary"] if preserve else []
                    )
                    try:
                        run(
                            [
                                "gdal",
                                "vector",
                                "simplify-coverage",
                                source,
                                reference,
                                *options,
                            ]
                        )
                        run(
                            [args.terrio, "simplify-coverage", source, actual, *options]
                        )
                        detail = compare(reference, actual)
                        for output in [reference, actual]:
                            errors = root / f"errors-{output.stem}.gpkg"
                            run(["gdal", "vector", "check-coverage", output, errors])
                            ds = ogr.Open(str(errors))
                            if any(layer.GetFeatureCount() for layer in ds):
                                raise AssertionError(
                                    f"{output.name}: output coverage invalid"
                                )
                            ds = None
                        results.append(
                            dict(
                                name=f"{name} tolerance={tolerance} preserve-boundary={preserve}",
                                status="identical",
                                detail=detail,
                            )
                        )
                    except Exception as error:
                        results.append(
                            dict(
                                name=f"{name} tolerance={tolerance} preserve-boundary={preserve}",
                                status="differs",
                                detail=str(error),
                            )
                        )
        source = root / "active.gpkg"
        write(source, [shapes[0]], crs)
        dataset = ogr.Open(str(source), 1)
        layer = dataset.CreateLayer("points", srs=crs, geom_type=ogr.wkbPointZM)
        feature = ogr.Feature(layer.GetLayerDefn())
        feature.SetFID(300)
        feature.SetGeometry(ogr.CreateGeometryFromWkt("POINT ZM (1.25 -2.5 12 37)"))
        layer.CreateFeature(feature)
        dataset = None
        for preserve in [False, True]:
            reference = root / f"active-ref-{preserve}.gpkg"
            actual = root / f"active-out-{preserve}.gpkg"
            options = ["--tolerance", "100", "--active-layer", "county"] + (
                ["--preserve-boundary"] if preserve else []
            )
            try:
                run(
                    ["gdal", "vector", "simplify-coverage", source, reference, *options]
                )
                run([args.terrio, "simplify-coverage", source, actual, *options])
                detail = compare(reference, actual)
                for output in [reference, actual]:
                    errors = root / f"errors-{output.stem}.gpkg"
                    run(
                        [
                            "gdal",
                            "vector",
                            "check-coverage",
                            output,
                            errors,
                            "--layer",
                            "county",
                        ]
                    )
                    dataset = ogr.Open(str(errors))
                    if any(layer.GetFeatureCount() for layer in dataset):
                        raise AssertionError("active polygon coverage invalid")
                    dataset = None
                results.append(
                    dict(
                        name=f"active layer preserve-boundary={preserve}",
                        status="identical",
                        detail=detail,
                    )
                )
            except Exception as error:
                results.append(
                    dict(
                        name=f"active layer preserve-boundary={preserve}",
                        status="differs",
                        detail=str(error),
                    )
                )
    print(
        json.dumps(results, indent=2)
        if args.json
        else "\n".join(f"{r['status']}: {r['name']}: {r['detail']}" for r in results)
    )
    return int(any(r["status"] == "differs" for r in results))


if __name__ == "__main__":
    raise SystemExit(main())
