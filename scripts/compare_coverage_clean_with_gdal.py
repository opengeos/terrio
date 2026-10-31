"""Compare coverage cleaning with GDAL 3.13.3 / GEOS 3.14.1.

Compare source attributes, feature correspondence, schema, CRS, geometry and
coverage validity. Geometric comparison tolerates floating noding arithmetic;
it never treats approximate coordinates as identical. Ordering-dependent cases
report exact cyclic-input references or independently verified greedy merge
witnesses, explicitly without per-feature geometric equality.
"""

import argparse
import json
import itertools
import random
import subprocess
import tempfile
from pathlib import Path

from compare_coverage_with_gdal import counties, rectangle, run, write
from osgeo import ogr


def read(path):
    ds = ogr.Open(str(path))
    layer = ds.GetLayer(0)
    features = []
    for f in layer:
        g = f.GetGeometryRef()
        features.append((f.GetFID(), f.items(), g.Clone() if g is not None else None))
    return (
        features,
        [(f.name, f.GetType(), f.GetSubType()) for f in layer.schema],
        layer.GetSpatialRef().ExportToWkt(),
        layer.GetName(),
    )


def coordinates(geometry):
    if geometry.GetGeometryCount():
        return [
            p
            for i in range(geometry.GetGeometryCount())
            for p in coordinates(geometry.GetGeometryRef(i))
        ]
    return geometry.GetPoints()


def compare(actual, reference):
    a, sa, ca, la = read(actual)
    b, sb, cb, lb = read(reference)
    assert (len(a), sa, ca, la) == (len(b), sb, cb, lb), (
        "count/schema/CRS/layer mismatch"
    )
    maximum = 0.0
    exact = True
    for (fa, aa, ga), (fb, ab, gb) in zip(a, b, strict=True):
        assert (fa, aa) == (fb, ab), "feature IDs or attributes differ"
        assert (ga is None) == (gb is None), "null geometry differs"
        if ga is None:
            continue
        assert ga.GetGeometryType() == gb.GetGeometryType(), (
            "feature geometry type differs"
        )
        assert ga.IsEmpty() == gb.IsEmpty(), "empty geometry differs"
        assert ga.IsValid(), "invalid output polygon"
        if ga.IsEmpty():
            continue
        difference = ga.SymDifference(gb).GetArea() / max(gb.GetArea(), 1.0)
        maximum = max(maximum, difference)
        assert difference <= 1e-8, f"relative symmetric difference {difference}"
        if ga.Is3D():
            reference_points = coordinates(gb)
            for point in coordinates(ga):
                nearest = min(
                    reference_points,
                    key=lambda p: (p[0] - point[0]) ** 2 + (p[1] - point[1]) ** 2,
                )
                assert (
                    abs(nearest[0] - point[0]) <= 1e-6
                    and abs(nearest[1] - point[1]) <= 1e-6
                ), "Z vertex XY mismatch"
                assert abs(nearest[2] - point[2]) <= 1e-9 * max(1.0, abs(nearest[2])), (
                    "Z ordinate differs"
                )
        # Normalize component, ring and starting-vertex order before exact WKB comparison.
        exact &= bytes(ga.Normalize().ExportToIsoWkb()) == bytes(
            gb.Normalize().ExportToIsoWkb()
        )
    # Independent GEOS coverage validity, including identical shared nodes.
    invalid_path = actual.with_name("invalid.geojson")
    invalid_path.unlink(missing_ok=True)
    run(["gdal", "vector", "check-coverage", actual, invalid_path])
    invalid = ogr.Open(str(invalid_path))
    assert invalid.GetLayer(0).GetFeatureCount() == 0, "output is not a valid coverage"
    return (
        "identical" if exact else "within tolerance",
        f"{len(a)} features; schema/attributes/IDs/types/CRS match; maximum relative area difference {maximum:.3g}",
    )


def ordering_witness(source, output, strategy):
    """Independently replay greedy merging on GDAL-polygonized input faces.

    Different graph traversals may assign different owners. A witness must prove
    that every assignment is a legal next greedy step, with input-order ties.
    This does not accept arbitrary valid coverages or widen coordinate tolerances.
    """
    dataset = ogr.Open(str(source))
    input_layer = dataset.GetLayer(0)
    inputs = [feature.GetGeometryRef().Clone() for feature in input_layer]
    result = dataset.ExecuteSQL(
        "SELECT ST_Polygonize(g) FROM (SELECT ST_Union(ST_Boundary(geom)) AS g FROM county)",
        dialect="SQLITE",
    )
    feature = result.GetNextFeature()
    collection = feature.GetGeometryRef().Clone()
    dataset.ReleaseResultSet(result)
    faces = [
        collection.GetGeometryRef(i).Clone()
        for i in range(collection.GetGeometryCount())
    ]
    output_features, _, _, _ = read(output)
    outputs = [feature[2] for feature in output_features]
    assert len(outputs) == len(inputs), "ownership feature correspondence differs"
    assert all(
        g is not None
        and g.GetGeometryType() in (ogr.wkbPolygon, ogr.wkbMultiPolygon)
        and g.IsValid()
        for g in outputs
    ), "invalid XY polygon output"

    def union(geometries):
        accumulated = ogr.Geometry(ogr.wkbPolygon)
        for geometry in geometries:
            accumulated = accumulated.Union(geometry)
        return accumulated

    assert union(inputs).SymDifference(union(outputs)).IsEmpty(), (
        "cleaning changed the input XY union"
    )
    parents = []
    owners = []
    for face in faces:
        point = face.PointOnSurface()
        ps = [i for i, geometry in enumerate(inputs) if geometry.Contains(point)]
        candidates = [
            i for i, geometry in enumerate(outputs) if geometry.Contains(point)
        ]
        if not ps:
            assert not candidates, "unrequested gap filling"
            parents.append([])
            owners.append(None)
            continue
        assert len(candidates) == 1, "face has no unique output owner"
        owner = candidates[0]
        assert owner in ps, "face assigned outside its input parents"
        assert face.Difference(outputs[owner]).IsEmpty(), (
            "output splits a resultant face"
        )
        for i, geometry in enumerate(outputs):
            if i != owner:
                assert face.Intersection(geometry).GetArea() == 0, (
                    "overlapping output interiors"
                )
        parents.append(ps)
        owners.append(owner)
    overlaps = [i for i, ps in enumerate(parents) if len(ps) > 1]
    assert len(overlaps) <= 16, "ownership replay exceeds its explicit state bound"
    areas = [face.GetArea() for face in faces]

    def line_length(geometry):
        if geometry.GetGeometryName() in ("LINESTRING", "LINEARRING"):
            return geometry.Length()
        return sum(
            line_length(geometry.GetGeometryRef(i))
            for i in range(geometry.GetGeometryCount())
        )

    borders = [
        [line_length(a.Intersection(b)) if i != j else 0 for j, b in enumerate(faces)]
        for i, a in enumerate(faces)
    ]
    initial = [
        [i for i, ps in enumerate(parents) if ps == [owner]]
        for owner in range(len(inputs))
    ]
    failed = set()

    def replay(mask):
        if mask == (1 << len(overlaps)) - 1:
            return ()
        if mask in failed:
            return None
        assigned = [list(fs) for fs in initial]
        for bit, i in enumerate(overlaps):
            if mask & (1 << bit):
                assigned[owners[i]].append(i)
        for bit, i in enumerate(overlaps):
            if mask & (1 << bit):
                continue

            def score(parent):
                if strategy == "min-index":
                    return -parent
                if strategy == "longest-border":
                    return sum(borders[i][j] for j in assigned[parent])
                area = sum(areas[j] for j in assigned[parent])
                return -area if strategy == "min-area" else area

            target = max(parents[i], key=score)
            if target == owners[i]:
                remainder = replay(mask | (1 << bit))
                if remainder is not None:
                    return (i, *remainder)
        failed.add(mask)
        return None

    witness = replay(0)
    assert witness is not None, "no legal progressive merge order produces these owners"
    invalid = output.with_name("ordering-invalid.geojson")
    invalid.unlink(missing_ok=True)
    run(["gdal", "vector", "check-coverage", output, invalid])
    checked = ogr.Open(str(invalid))
    assert checked.GetLayer(0).GetFeatureCount() == 0, (
        "output coverage has mismatched shared nodes"
    )
    return witness


def compare_reference_orders(actual, reference, shapes, crs, options, root, source):
    try:
        return compare(actual, reference)
    except AssertionError as original:
        # GDAL 3.13.3/GEOS 3.14.1 can change overlap ownership when only ring
        # start vertices change. Verify against an exact executable-oracle result
        # for an equivalent input; never relax geometric error tolerances.
        if not 1 <= len(shapes) <= 3 or any(
            g is None
            or g.GetGeometryType() != ogr.wkbPolygon
            or g.IsEmpty()
            or g.GetGeometryCount() != 1
            or g.GetGeometryRef(0).GetPointCount() > 13
            for g in shapes
        ):
            raise
        rings = [g.GetGeometryRef(0).GetPoints()[:-1] for g in shapes]
        for rotations in itertools.product(*(range(len(ring)) for ring in rings)):
            if not any(rotations):
                continue
            alternatives = []
            for shape, ring, rotation in zip(shapes, rings, rotations, strict=True):
                points = ring[rotation:] + ring[:rotation]
                boundary = ogr.Geometry(ogr.wkbLinearRing)
                for x, y in points + points[:1]:
                    boundary.AddPoint_2D(x, y)
                polygon = ogr.Geometry(ogr.wkbPolygon)
                polygon.AddGeometry(boundary)
                assert polygon.SymDifference(shape).IsEmpty(), (
                    "reference variant changed input geometry"
                )
                alternatives.append(polygon)
            cyclic_source = root / "cyclic-input.gpkg"
            output = root / "cyclic-reference.geojson"
            cyclic_source.unlink(missing_ok=True)
            output.unlink(missing_ok=True)
            write(cyclic_source, alternatives, crs)
            run(["gdal", "vector", "clean-coverage", cyclic_source, output, *options])
            try:
                status, detail = compare(actual, output)
            except AssertionError:
                continue
            if status == "identical":
                return "within tolerance", (
                    f"Ordering-dependent ownership: original comparison: {original}; "
                    f"matches GDAL exactly for equivalent cyclic ring starts {rotations}; {detail}"
                )
        # A fixed Rust traversal need not be one of the reference hash traversals
        # induced by cyclic input changes. Independently prove the complete greedy
        # contract for both outputs rather than accepting geometric disagreement.
        gap = float(options[options.index("--maximum-gap-width") + 1])
        if gap != 0:
            raise original
        a, sa, ca, la = read(actual)
        b, sb, cb, lb = read(reference)
        assert (len(a), sa, ca, la) == (len(b), sb, cb, lb)
        assert [(f, attrs) for f, attrs, _ in a] == [(f, attrs) for f, attrs, _ in b]
        strategy = options[options.index("--merge-strategy") + 1]
        try:
            actual_order = ordering_witness(source, actual, strategy)
            reference_order = ordering_witness(source, reference, strategy)
        except AssertionError as failure:
            raise AssertionError(
                f"{original}; ownership contract failed: {failure}"
            ) from failure
        return "within tolerance", (
            f"Semantic ordering agreement, not per-feature geometric equality: {original}; "
            f"GDAL-polygonized face replay proves legal {strategy} owners in orders "
            f"Terrio={actual_order}, reference={reference_order}; union/attributes/IDs/CRS "
            "and coverage validity match. Polygon/MultiPolygon types can follow the differing owners."
        )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    real, crs = counties(args.data)
    fixtures = [
        ("overlap", [rectangle(0, 0, 3, 2), rectangle(2, 0, 4, 2)]),
        ("contained", [rectangle(0, 0, 4, 4), rectangle(1, 1, 2, 2)]),
        ("duplicate", [rectangle(0, 0, 2, 2)] * 2),
        (
            "enclosed gap",
            [
                rectangle(0, 0, 1.9, 4),
                rectangle(2.1, 0, 4, 4),
                rectangle(1.9, 0, 2.1, 0.2),
                rectangle(1.9, 3.8, 2.1, 4),
            ],
        ),
        (
            "inlet",
            [
                rectangle(0, 0, 1.9, 4),
                rectangle(2.1, 0, 4, 4),
                rectangle(1.9, 0, 2.1, 0.2),
            ],
        ),
        ("snap partial edge", [rectangle(0, 0, 2, 4), rectangle(2.001, 1, 4, 3)]),
        ("zero snap", [rectangle(0, 0, 2, 4), rectangle(2, 1, 4, 3)]),
        ("collapse", [rectangle(0, 0, 1, 1)]),
        (
            "triangle hole",
            [
                ogr.CreateGeometryFromWkt(
                    "POLYGON ((0 0,4 0,4 4,0 4,0 0),(1 1,2 3,3 1,1 1))"
                )
            ],
        ),
        (
            "convex quad hole",
            [
                ogr.CreateGeometryFromWkt(
                    "POLYGON ((0 0,4 0,4 4,0 4,0 0),(1 2,2 3,3 2,2 1,1 2))"
                )
            ],
        ),
        (
            "three overlaps",
            [rectangle(0, 0, 4, 3), rectangle(1, 1, 5, 4), rectangle(2, -1, 3, 5)],
        ),
        (
            "crossing triangles",
            [
                ogr.CreateGeometryFromWkt("POLYGON ((0 0,4 0,2 4,0 0))"),
                ogr.CreateGeometryFromWkt("POLYGON ((0 3,4 3,2 -1,0 3))"),
            ],
        ),
        (
            "hole",
            [
                ogr.CreateGeometryFromWkt(
                    "POLYGON ((0 0,4 0,4 4,0 4,0 0),(1 1,1 1.2,3 1.2,3 1,1 1))"
                )
            ],
        ),
        ("empty", [rectangle(0, 0, 1, 1), ogr.Geometry(ogr.wkbPolygon)]),
        (
            "measured",
            [ogr.CreateGeometryFromWkt("POLYGON M ((0 0 1,2 0 2,2 2 3,0 2 4,0 0 1))")],
        ),
        (
            "elevated",
            [
                ogr.CreateGeometryFromWkt(
                    "POLYGON Z ((0 0 1,3 0 2,3 2 3,0 2 4,0 0 1))"
                ),
                ogr.CreateGeometryFromWkt(
                    "POLYGON Z ((2 0 10,4 0 20,4 2 30,2 2 40,2 0 10))"
                ),
            ],
        ),
        (
            "elevated crossing",
            [
                ogr.CreateGeometryFromWkt("POLYGON Z ((0 0 0,4 0 4,2 4 8,0 0 0))"),
                ogr.CreateGeometryFromWkt("POLYGON Z ((0 3 10,4 3 20,2 -1 30,0 3 10))"),
            ],
        ),
        ("real counties", real),
        ("real overlap", [real[0], real[0].Buffer(1.0), *real[1:4]]),
    ]
    generator = random.Random(289)
    for index in range(32):
        shapes = []
        for _ in range(3):
            x, y = generator.randrange(6), generator.randrange(6)
            shapes.append(
                rectangle(
                    x, y, x + generator.randrange(2, 7), y + generator.randrange(2, 7)
                )
            )
        fixtures.append((f"three-rectangle probe {index}", shapes))
    results = []
    with tempfile.TemporaryDirectory(
        prefix="terrio-coverage-clean-", dir=args.data
    ) as directory:
        root = Path(directory)
        for index, (name, shapes) in enumerate(fixtures):
            source = root / f"source-{index}.gpkg"
            write(source, shapes, crs)
            for strategy in ["longest-border", "max-area", "min-area", "min-index"]:
                for gap in (
                    [1.0, 1.5]
                    if name in {"triangle hole", "convex quad hole"}
                    else [0.15, 0.25]
                    if name in {"enclosed gap", "inlet", "hole"}
                    else [0.0]
                ):
                    label = f"{name}, {strategy}, gap={gap}"
                    actual, reference = (
                        root / "actual.geojson",
                        root / "reference.geojson",
                    )
                    actual.unlink(missing_ok=True)
                    reference.unlink(missing_ok=True)
                    options = [
                        "--merge-strategy",
                        strategy,
                        "--maximum-gap-width",
                        str(gap),
                    ]
                    if name == "snap partial edge":
                        options += ["--snapping-distance", "0.01"]
                    elif name == "zero snap":
                        options += ["--snapping-distance", "0"]
                    elif name == "collapse":
                        options += ["--snapping-distance", "2"]
                    try:
                        run(
                            [
                                "gdal",
                                "vector",
                                "clean-coverage",
                                source,
                                reference,
                                *options,
                            ]
                        )
                        run([args.terrio, "clean-coverage", source, actual, *options])
                        status, detail = compare_reference_orders(
                            actual, reference, shapes, crs, options, root, source
                        )
                    except Exception as error:
                        status, detail = "different", str(error)
                    results.append({"name": label, "status": status, "detail": detail})
        for name, geometry in [
            ("null", None),
            ("nonpolygon", ogr.CreateGeometryFromWkt("POINT (4 5)")),
        ]:
            source = root / f"bad-{name}.gpkg"
            write(source, [rectangle(0, 0, 2, 2), geometry], crs)
            for strategy in ["longest-border", "max-area", "min-area", "min-index"]:
                actual = root / "bad-actual.geojson"
                reference = root / "bad-reference.geojson"
                actual.unlink(missing_ok=True)
                reference.unlink(missing_ok=True)
                try:
                    for command, output in [
                        (["gdal", "vector", "clean-coverage"], reference),
                        ([args.terrio, "clean-coverage"], actual),
                    ]:
                        process = subprocess.run(
                            [
                                str(v)
                                for v in [
                                    *command,
                                    source,
                                    output,
                                    "--merge-strategy",
                                    strategy,
                                ]
                            ],
                            capture_output=True,
                            text=True,
                        )
                        assert (
                            process.returncode != 0
                            and "polygon" in process.stderr.lower()
                        ), "input was not rejected as non-polygonal"
                    assert not actual.exists(), (
                        "Terrio created output before rejecting the input"
                    )
                    status, detail = (
                        "identical",
                        "both reject non-polygonal active geometry; Terrio creates no output",
                    )
                except Exception as error:
                    status, detail = "different", str(error)
                results.append(
                    {
                        "name": f"{name} rejection, {strategy}",
                        "status": status,
                        "detail": detail,
                    }
                )
    if args.json:
        print(json.dumps(results))
    else:
        for r in results:
            print(f"{r['status']}: {r['name']}: {r['detail']}")
    return int(any(r["status"] == "different" for r in results))


if __name__ == "__main__":
    raise SystemExit(main())
