"""Compare all seven XY layer-algebra operations with GDAL 3.13.3.

Check schema, attributes, sequential IDs, CRS, feature types, validity and geometry.
Normalized WKB is exact where possible. Polygon area tolerance is 1e-7 relative;
line length tolerance is 1e-8 relative with bidirectional vertex distance <=1e-7
CRS units; point distance tolerance is 1e-8 CRS units. Component order may differ.
"""

import argparse
import json
import shutil
import subprocess
import tempfile
from pathlib import Path

from osgeo import gdal, ogr, osr
from compare_coverage_with_gdal import counties, rectangle

gdal.UseExceptions()
ogr.UseExceptions()
osr.UseExceptions()

OPERATIONS = {
    "intersection": "Intersection",
    "union": "Union",
    "sym-difference": "SymDifference",
    "identity": "Identity",
    "update": "Update",
    "clip": "Clip",
    "erase": "Erase",
}
FIXTURES = {
    "polygon/polygon": (
        ["POLYGON ((0 0,3 0,3 2,0 2,0 0))"],
        ["POLYGON ((2 0,4 0,4 2,2 2,2 0))"],
    ),
    "line/polygon": (["LINESTRING (-1 1,4 1)"], ["POLYGON ((0 0,3 0,3 2,0 2,0 0))"]),
    "point/polygon": (
        ["MULTIPOINT ((-1 1),(0 1),(1 1),(3 1),(4 1))"],
        ["POLYGON ((0 0,3 0,3 2,0 2,0 0))"],
    ),
    "polygon/line": (["POLYGON ((0 0,3 0,3 2,0 2,0 0))"], ["LINESTRING (-1 1,4 1)"]),
    "crossing lines": (["LINESTRING (0 0,3 3)"], ["LINESTRING (0 3,3 0)"]),
    "overlapping lines": (["LINESTRING (0 0,4 0)"], ["LINESTRING (2 0,6 0)"]),
    "points/line": (["MULTIPOINT ((0 0),(1 0),(2 1))"], ["LINESTRING (0 0,3 0)"]),
    "points/points": (["MULTIPOINT ((0 0),(1 0),(2 1))"], ["MULTIPOINT ((1 0),(3 3))"]),
    "polygon touch": (
        ["POLYGON ((0 0,2 0,2 2,0 2,0 0))"],
        ["POLYGON ((2 0,4 0,4 2,2 2,2 0))"],
    ),
    "polygon hole": (
        ["POLYGON ((0 0,8 0,8 8,0 8,0 0),(2 2,2 6,6 6,6 2,2 2))"],
        ["POLYGON ((1 1,4 1,4 7,1 7,1 1))"],
    ),
    "multi-feature overlap": (
        ["POLYGON ((0 0,4 0,4 4,0 4,0 0))", "POLYGON ((2 2,6 2,6 6,2 6,2 2))"],
        ["POLYGON ((1 1,5 1,5 5,1 5,1 1))", "POLYGON ((3 3,7 3,7 7,3 7,3 3))"],
    ),
    "collection": (
        [
            "GEOMETRYCOLLECTION (POLYGON ((0 0,2 0,2 2,0 2,0 0)),LINESTRING (3 0,3 3),POINT (4 1))"
        ],
        ["POLYGON ((1 1,5 1,5 4,1 4,1 1))"],
    ),
    "point/overlapping collection": (
        ["POINT (1.5 1.5)"],
        [
            "GEOMETRYCOLLECTION (POLYGON ((0 0,2 0,2 2,0 2,0 0)),POLYGON ((1 1,3 1,3 3,1 3,1 1)))"
        ],
    ),
    "overlapping collection/point": (
        [
            "GEOMETRYCOLLECTION (POLYGON ((0 0,2 0,2 2,0 2,0 0)),POLYGON ((1 1,3 1,3 3,1 3,1 1)))"
        ],
        ["POINT (1.5 1.5)"],
    ),
    "disjoint polygon collection": (
        [
            "GEOMETRYCOLLECTION (POLYGON ((0 0,1 0,1 1,0 1,0 0)),POLYGON ((3 0,4 0,4 1,3 1,3 0)))"
        ],
        ["POLYGON ((10 10,11 10,11 11,10 11,10 10))"],
    ),
    "touching polygon collection": (
        [
            "GEOMETRYCOLLECTION (POLYGON ((0.1 0.1,1.1 0.1,1.1 1.1,0.1 1.1,0.1 0.1)),POLYGON ((1.1 0.1,2.1 0.1,2.1 1.1,1.1 1.1,1.1 0.1)))"
        ],
        ["POLYGON ((10 10,11 10,11 11,10 11,10 10))"],
    ),
    "single-part collection": (
        ["GEOMETRYCOLLECTION (POLYGON ((0 0,2 0,2 2,0 2,0 0)))"],
        ["POINT (3 3)"],
    ),
    "collection inside polygon hole": (
        ["GEOMETRYCOLLECTION (POINT (1 1))"],
        ["POLYGON ((0 0,4 0,4 4,0 4,0 0),(0.5 0.5,0.5 1.5,1.5 1.5,1.5 0.5,0.5 0.5))"],
    ),
    "null input": ([None], ["POINT (1 1)"]),
    "null method": (["POINT (1 1)"], [None]),
    "empty input": (["POLYGON EMPTY"], ["POINT (1 1)"]),
    "empty method": (["POINT (1 1)"], ["POLYGON EMPTY"]),
}


def write(path, shapes, crs, field):
    ds = ogr.GetDriverByName("GPKG").CreateDataSource(str(path))
    layer = ds.CreateLayer("features", crs, geom_type=ogr.wkbUnknown)
    layer.CreateField(ogr.FieldDefn(field, ogr.OFTInteger64))
    layer.CreateField(ogr.FieldDefn("label", ogr.OFTString))
    for i, geometry in enumerate(shapes):
        f = ogr.Feature(layer.GetLayerDefn())
        f.SetFID(100 + i)
        f[field] = i
        f["label"] = f"feature-{i}"
        if geometry is not None:
            f.SetGeometry(geometry)
        layer.CreateFeature(f)
    ds = None


def read(path):
    ds = ogr.Open(str(path))
    layer = ds.GetLayer(0)
    schema = [(f.name, f.GetType(), f.GetSubType()) for f in layer.schema]
    rows = [
        (
            f.GetFID(),
            f.items(),
            f.GetGeometryRef().Clone() if f.GetGeometryRef() is not None else None,
        )
        for f in layer
    ]
    return rows, schema, layer.GetSpatialRef().Clone(), layer.GetName()


def points(g):
    if g.GetGeometryCount():
        return [
            p for i in range(g.GetGeometryCount()) for p in points(g.GetGeometryRef(i))
        ]
    return g.GetPoints()


def by_dimension(g, dimension):
    kinds = {2: ogr.wkbMultiPolygon, 1: ogr.wkbMultiLineString, 0: ogr.wkbMultiPoint}
    result = ogr.Geometry(kinds[dimension])

    def collect(part):
        if (
            part.GetGeometryName().startswith("MULTI")
            or part.GetGeometryName() == "GEOMETRYCOLLECTION"
        ):
            for i in range(part.GetGeometryCount()):
                collect(part.GetGeometryRef(i))
        elif part.GetDimension() == dimension and not part.IsEmpty():
            result.AddGeometry(part)

    collect(g)
    return (
        result.UnionCascaded()
        if dimension == 2 and result.GetGeometryCount() > 1
        else result
    )


def compare(actual, reference):
    a, sa, ca, na = read(actual)
    b, sb, cb, nb = read(reference)
    if not ((len(a), sa, na) == (len(b), sb, nb)):
        raise AssertionError(
            f"count/schema/name differs: {(len(a), sa, na)}/{(len(b), sb, nb)}"
        )
    if not (ca.IsSame(cb)):
        raise AssertionError("CRS differs")
    if not ([f[0] for f in a] == list(range(1, len(a) + 1))):
        raise AssertionError("IDs are not sequential from one")
    if not ([f[0] for f in a] == [f[0] for f in b]):
        raise AssertionError("reference feature IDs differ")
    exact = True
    for (fid, attrs, g), (_, expected, e) in zip(a, b, strict=True):
        if not (attrs == expected):
            raise AssertionError(f"FID {fid}: attributes differ")
        if not ((g is None) == (e is None)):
            raise AssertionError(f"FID {fid}: null geometry differs")
        if g is None:
            continue
        if not (g.GetGeometryType() == e.GetGeometryType()):
            raise AssertionError(
                f"FID {fid}: types {g.GetGeometryName()}/{e.GetGeometryName()}"
            )
        if not (g.IsValid()):
            raise AssertionError(f"FID {fid}: invalid output")
        if not (g.IsEmpty() == e.IsEmpty()):
            raise AssertionError(f"FID {fid}: emptiness differs")
        same = bytes(g.Normalize().ExportToIsoWkb()) == bytes(
            e.Normalize().ExportToIsoWkb()
        )
        exact &= same
        if same:
            continue
        dimensions = (
            [2, 1, 0]
            if e.GetGeometryName() == "GEOMETRYCOLLECTION"
            else [e.GetDimension()]
        )
        for dim in dimensions:
            actual_part, expected_part = by_dimension(g, dim), by_dimension(e, dim)
            if not (actual_part.IsEmpty() == expected_part.IsEmpty()):
                raise AssertionError(f"FID {fid}: lost dimension {dim}")
            if expected_part.IsEmpty():
                continue
            if dim == 2:
                error = actual_part.SymDifference(expected_part).GetArea() / max(
                    1e-300, expected_part.GetArea()
                )
                if not (error <= 1e-07):
                    raise AssertionError(
                        f"FID {fid}: relative polygon area difference {error}"
                    )
            elif dim == 1:
                error = abs(actual_part.Length() - expected_part.Length()) / max(
                    1e-300, expected_part.Length()
                )
                if not (error <= 1e-08):
                    raise AssertionError(
                        f"FID {fid}: relative length difference {error}"
                    )
                for source, target in [
                    (actual_part, expected_part),
                    (expected_part, actual_part),
                ]:
                    uncovered = source.Difference(target.Buffer(1e-7)).Length()
                    if uncovered / max(1e-300, source.Length()) > 1e-8:
                        raise AssertionError(f"FID {fid}: line edges differ")
                    for p in points(source):
                        point = ogr.Geometry(ogr.wkbPoint)
                        point.AddPoint_2D(p[0], p[1])
                        if not (point.Distance(target) <= 1e-07):
                            raise AssertionError(f"FID {fid}: line vertex differs")
            else:
                left, right = points(actual_part), points(expected_part)
                if not (len(left) == len(right)):
                    raise AssertionError(f"FID {fid}: point count differs")
                for p in left:
                    j = min(
                        range(len(right)),
                        key=lambda j: (right[j][0] - p[0]) ** 2
                        + (right[j][1] - p[1]) ** 2,
                    )
                    q = right.pop(j)
                    if not ((q[0] - p[0]) ** 2 + (q[1] - p[1]) ** 2 <= 1e-16):
                        raise AssertionError(f"FID {fid}: point position differs")
    return exact, len(a)


def projected_roads(data):
    ds = ogr.Open(str(data / "las_vegas_roads.geojson"))
    layer = ds.GetLayer(0)
    crs = osr.SpatialReference()
    crs.ImportFromEPSG(32611)
    crs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    source = layer.GetSpatialRef().Clone()
    source.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    transform = osr.CoordinateTransformation(source, crs)
    rows = []
    for f in layer:
        g = f.GetGeometryRef().Clone()
        g.Transform(transform)
        g.FlattenTo2D()
        if g.IsValid() and not g.IsEmpty():
            rows.append(g)
        if len(rows) == 40:
            break
    return rows, crs


def compare_update_types(root, binary, crs):
    results = []
    for index, (input_type, method_type, value) in enumerate(
        [
            (ogr.OFTInteger64, ogr.OFTString, "42"),
            (ogr.OFTReal, ogr.OFTString, "2.5"),
            (ogr.OFTString, ogr.OFTInteger64, 7),
        ]
    ):
        row = {"name": f"update field conversion {index}"}
        try:
            paths = [
                root / f"typed-{index}-{side}.gpkg"
                for side in ["input", "method", "actual", "reference"]
            ]
            for side, dtype, attribute in [(0, input_type, 1), (1, method_type, value)]:
                ds = ogr.GetDriverByName("GPKG").CreateDataSource(str(paths[side]))
                layer = ds.CreateLayer("features", crs, geom_type=ogr.wkbPoint)
                layer.CreateField(ogr.FieldDefn("value", dtype))
                feature = ogr.Feature(layer.GetLayerDefn())
                feature.SetField(
                    "value", str(attribute) if dtype == ogr.OFTString else attribute
                )
                feature.SetGeometry(ogr.CreateGeometryFromWkt(f"POINT ({side * 2} 0)"))
                layer.CreateFeature(feature)
                feature = layer = ds = None
            da, db = ogr.Open(str(paths[0])), ogr.Open(str(paths[1]))
            ds = ogr.GetDriverByName("GPKG").CreateDataSource(str(paths[3]))
            layer = ds.CreateLayer("result", crs, geom_type=ogr.wkbUnknown)
            layer.CreateField(ogr.FieldDefn("value", input_type))
            if da.GetLayer(0).Update(db.GetLayer(0), layer) != 0:
                raise AssertionError("reference update failed")
            layer = ds = da = db = None
            process = subprocess.run(
                [
                    str(binary),
                    "layer-algebra",
                    "update",
                    *map(str, paths[:3]),
                    "--nln",
                    "result",
                ],
                capture_output=True,
                text=True,
            )
            if process.returncode:
                raise AssertionError(process.stderr)
            exact, count = compare(paths[2], paths[3])
            row.update(
                status="identical" if exact else "within tolerance",
                detail=f"{count} features; typed Update values checked",
            )
        except Exception as error:
            row.update(status="differs", detail=str(error))
        results.append(row)
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", type=Path, required=True)
    parser.add_argument("--json", action="store_true")
    parser.add_argument(
        "--keep-failures",
        type=Path,
        help="save failing inputs and outputs for diagnosis",
    )
    args = parser.parse_args()
    crs = osr.SpatialReference()
    crs.ImportFromEPSG(5070)
    fixtures = [
        (
            name,
            [ogr.CreateGeometryFromWkt(w) if w else None for w in a],
            [ogr.CreateGeometryFromWkt(w) if w else None for w in b],
            crs,
        )
        for name, (a, b) in FIXTURES.items()
    ]
    shapes, crs = counties(args.data)
    fixtures.append(("real counties", shapes[:12], shapes[8:20], crs))
    roads, crs = projected_roads(args.data)
    envelope = ogr.Geometry(ogr.wkbGeometryCollection)
    for g in roads:
        envelope.AddGeometry(g)
    x0, x1, y0, y1 = envelope.GetEnvelope()
    region = rectangle(
        x0 + (x1 - x0) * 0.25,
        y0 + (y1 - y0) * 0.25,
        x0 + (x1 - x0) * 0.75,
        y0 + (y1 - y0) * 0.75,
    )
    fixtures.append(("real roads/region", roads, [region], crs))
    fixtures.append(("real road junctions", roads[:20], roads[10:30], crs))
    results = []
    with tempfile.TemporaryDirectory(prefix="terrio-layer-algebra-") as temp:
        root = Path(temp)
        for index, (name, left, right, crs) in enumerate(fixtures):
            a, b = root / f"a{index}.gpkg", root / f"b{index}.gpkg"
            write(a, left, crs, "input_id")
            write(b, right, crs, "method_id")
            for operation, method in OPERATIONS.items():
                for lower in [False, True]:
                    actual, reference = root / "actual.gpkg", root / "reference.gpkg"
                    actual.unlink(missing_ok=True)
                    reference.unlink(missing_ok=True)
                    row = {"name": f"{name}: {operation}, lower={lower}"}
                    try:
                        da, db = ogr.Open(str(a)), ogr.Open(str(b))
                        ds = ogr.GetDriverByName("GPKG").CreateDataSource(
                            str(reference)
                        )
                        layer = ds.CreateLayer("result", crs, geom_type=ogr.wkbUnknown)
                        status = getattr(da.GetLayer(0), method)(
                            db.GetLayer(0),
                            layer,
                            options=[
                                "INPUT_PREFIX=a_",
                                "METHOD_PREFIX=b_",
                                f"KEEP_LOWER_DIMENSION_GEOMETRIES={'YES' if lower else 'NO'}",
                            ],
                        )
                        if not (status == 0):
                            raise AssertionError(f"GDAL operation returned {status}")
                        layer = ds = da = db = None
                        p = subprocess.run(
                            [
                                str(args.terrio),
                                "layer-algebra",
                                operation,
                                str(a),
                                str(b),
                                str(actual),
                                "--nln",
                                "result",
                                "--input-prefix",
                                "a_",
                                "--method-prefix",
                                "b_",
                                "--keep-lower-dimension-geometries",
                                str(lower).lower(),
                            ],
                            capture_output=True,
                            text=True,
                        )
                        if not (p.returncode == 0):
                            raise AssertionError(p.stderr)
                        validation = subprocess.run(
                            [
                                str(args.terrio),
                                "gpkg-validate",
                                str(actual),
                                "--full-check",
                                "--json",
                            ],
                            capture_output=True,
                            text=True,
                        )
                        if not (validation.returncode == 0):
                            raise AssertionError(
                                f"native GeoPackage validation failed: {validation.stdout}{validation.stderr}"
                            )
                        exact, count = compare(actual, reference)
                        row.update(
                            status="identical" if exact else "within tolerance",
                            detail=f"{count} features; schema/attributes/IDs/CRS/types and geometry checked",
                        )
                    except Exception as error:
                        row.update(status="differs", detail=str(error))
                        if args.keep_failures:
                            args.keep_failures.mkdir(parents=True, exist_ok=True)
                            for role, path in [
                                ("input", a),
                                ("method", b),
                                ("actual", actual),
                                ("reference", reference),
                            ]:
                                if path.exists():
                                    shutil.copyfile(
                                        path,
                                        args.keep_failures
                                        / f"{index}-{operation}-{lower}-{role}.gpkg",
                                    )
                    results.append(row)
        results.extend(compare_update_types(root, args.terrio, crs))
    print(
        json.dumps(results, indent=2)
        if args.json
        else "\n".join(f"{r['status']}: {r['name']}: {r['detail']}" for r in results)
    )
    raise SystemExit(any(r["status"] == "differs" for r in results))


if __name__ == "__main__":
    main()
