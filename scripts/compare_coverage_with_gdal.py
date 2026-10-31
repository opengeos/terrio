"""Compare polygon coverage validation with GDAL 3.13 on real county boundaries.

Invalid edge sets, empty schema, IDs, geometry types and CRS must match exactly.
Fixtures include projected counties and real-boundary splits/overlaps/gaps.
"""
import argparse
import json
from pathlib import Path
import subprocess
import tempfile

from osgeo import gdal, ogr, osr

gdal.UseExceptions()
ogr.UseExceptions()


def run(args):
    process = subprocess.run([str(a) for a in args], capture_output=True, text=True)
    if process.returncode:
        raise RuntimeError(process.stderr[-3000:])
    return process.stdout


def counties(data):
    source = ogr.Open(str(data / "TN_Counties.geojson"))
    layer = source.GetLayer(0)
    target = osr.SpatialReference()
    target.ImportFromEPSG(5070)
    target.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    source_crs = layer.GetSpatialRef().Clone()
    source_crs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    transform = osr.CoordinateTransformation(source_crs, target)
    shapes = []
    for feature in layer:
        shape = feature.GetGeometryRef().Clone()
        shape.Transform(transform)
        shape.FlattenTo2D()
        if not shape.IsValid():
            shape = shape.MakeValid()
        if not shape.IsEmpty():
            shapes.append(shape)
    return shapes, target


def rectangle(x0, y0, x1, y1):
    ring = ogr.Geometry(ogr.wkbLinearRing)
    for x, y in [(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)]:
        ring.AddPoint_2D(x, y)
    polygon = ogr.Geometry(ogr.wkbPolygon)
    polygon.AddGeometry(ring)
    return polygon


def write(path, shapes, crs):
    dataset = ogr.GetDriverByName("GPKG").CreateDataSource(str(path))
    layer = dataset.CreateLayer("county", srs=crs, geom_type=ogr.wkbUnknown)
    layer.CreateField(ogr.FieldDefn("source_id", ogr.OFTInteger))
    layer.CreateField(ogr.FieldDefn("label", ogr.OFTString))
    for i, shape in enumerate(shapes):
        feature = ogr.Feature(layer.GetLayerDefn())
        feature.SetFID(100 + i)
        feature["source_id"] = i
        feature["label"] = f"county-{i}"
        if shape is not None:
            feature.SetGeometry(shape)
        layer.CreateFeature(feature)
    dataset = None


def read(path):
    dataset = ogr.Open(str(path))
    layer = dataset.GetLayer(0)
    result = {}
    for feature in layer:
        key = feature.GetFID()
        if key in result:
            raise AssertionError(f"duplicated source feature {key}")
        geometry = feature.GetGeometryRef()
        result[key] = (feature.GetFID(), feature.items(), geometry.Clone() if geometry is not None else None)
    return result, [(f.name, f.GetType(), f.GetSubType()) for f in layer.schema], layer.GetSpatialRef().Clone(), layer.GetGeomType(), layer.GetName()


def edges(geometry):
    """A multiset of original edge coordinates, independent of line/ring order."""
    if geometry is None or geometry.IsEmpty():
        return []
    if ogr.GT_Flatten(geometry.GetGeometryType()) == ogr.wkbLineString:
        points = geometry.GetPoints()
        return [tuple(sorted((a, b))) for a, b in zip(points, points[1:])]
    return [edge for i in range(geometry.GetGeometryCount()) for edge in edges(geometry.GetGeometryRef(i))]


def compare(reference, actual):
    a, sa, ca, ta, na = read(reference)
    b, sb, cb, tb, nb = read(actual)
    if a.keys() != b.keys() or sa != sb or not ca.IsSame(cb) or ta != tb or na != nb:
        raise AssertionError(f"source coverage, schema/CRS/type/name differs: IDs {a.keys()} vs {b.keys()}; types {ta}/{tb}; names {na}/{nb}")
    points = 0
    for key in a:
        fa, attrs_a, ga = a[key]
        fb, attrs_b, gb = b[key]
        if fa != fb or attrs_a != attrs_b:
            raise AssertionError(f"{key}: source FID or attributes differ: {fa}/{fb}")
        if (ga is None) != (gb is None):
            raise AssertionError(f"{key}: null/empty diagnostic geometry differs")
        if ga is not None and gb is not None and ga.GetGeometryType() != gb.GetGeometryType():
            raise AssertionError(f"{key}: diagnostic feature geometry type differs")
        empty_a = ga is None or ga.IsEmpty()
        empty_b = gb is None or gb.IsEmpty()
        if empty_a != empty_b or (not empty_a and sorted(edges(ga)) != sorted(edges(gb))):
            raise AssertionError(f"{key}: invalid edge set differs: {ga.ExportToWkt() if ga else None} vs {gb.ExportToWkt() if gb else None}")
        if ga is not None:
            points += ga.GetGeometryCount()
    return f"{len(a)} features/{points} invalid lines; exact edge sets, empty schema, output IDs, CRS, type and layer name"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    shapes, crs = counties(args.data)
    first = shapes[0]
    x0, x1, y0, y1 = first.GetEnvelope()
    mid = (x0 + x1) / 2
    left = first.Intersection(rectangle(x0 - 1, y0 - 1, mid, y1 + 1))
    right = first.Intersection(rectangle(mid, y0 - 1, x1 + 1, y1 + 1))
    gap_left = first.Intersection(rectangle(x0 - 1, y0 - 1, mid - 5, y1 + 1))
    gap_right = first.Intersection(rectangle(mid + 5, y0 - 1, x1 + 1, y1 + 1))
    overlap_left = first.Intersection(rectangle(x0 - 1, y0 - 1, mid + 1000, y1 + 1))
    overlap_right = first.Intersection(rectangle(mid - 1000, y0 - 1, x1 + 1, y1 + 1))
    repeated = first.Clone()

    def repeat_vertex(geometry):
        if geometry.GetGeometryName() == "LINEARRING":
            points = geometry.GetPoints()
            if len(points) > 2:
                points.insert(2, points[1])
                geometry.Empty()
                for x, y, *_ in points:
                    geometry.AddPoint_2D(x, y)
                return True
        return any(repeat_vertex(geometry.GetGeometryRef(i)) for i in range(geometry.GetGeometryCount()))
    assert repeat_vertex(repeated)
    fixtures = [
        ("Tennessee counties", shapes, 0),
        ("real county split", [left, right], 0),
        ("duplicate county", [first, first], 0),
        ("repeated county vertex", [repeated, first], 0),
        ("empty polygons", [first, ogr.CreateGeometryFromWkt("POLYGON EMPTY")], 0),
        ("overlapping county parts", [overlap_left, overlap_right], 0),
        ("county gap disabled", [gap_left, gap_right], 0),
        ("county gap below threshold", [gap_left, gap_right], 5),
        ("county narrow gap", [gap_left, gap_right], 20),
    ]
    for label, wkt in [
        ("Z diagnostics", "POLYGON Z ((0 0 1,2 0 2,0 2 3,0 0 1))"),
        ("M diagnostics", "POLYGON M ((0 0 10,2 0 20,0 2 30,0 0 10))"),
        ("ZM diagnostics", "POLYGON ZM ((0 0 1 10,2 0 2 20,0 2 3 30,0 0 1 10))"),
    ]:
        shape = ogr.CreateGeometryFromWkt(wkt)
        fixtures.append((label, [shape, shape], 0))
    results = []
    with tempfile.TemporaryDirectory(prefix="terrio-coverage-") as temp:
        root = Path(temp)
        for i, (name, features, gap) in enumerate(fixtures):
            source = root / f"source-{i}.gpkg"
            write(source, features, crs)
            for include in [False, True]:
                reference = root / f"ref-{i}-{include}.gpkg"
                actual = root / f"out-{i}-{include}.gpkg"
                options = ["--maximum-gap-width", str(gap)] + (["--include-valid"] if include else [])
                try:
                    run(["gdal", "vector", "check-coverage", source, reference, *options])
                    run([args.terrio, "check-coverage", source, actual, *options])
                    detail = compare(reference, actual)
                    results.append(dict(name=f"{name}, include-valid={include}", status="identical", detail=detail))
                except Exception as error:
                    results.append(dict(name=f"{name}, include-valid={include}", status="differs", detail=str(error)))
    print(json.dumps(results, indent=2) if args.json else "\n".join(f"{r['status']}: {r['name']}: {r['detail']}" for r in results))
    return int(any(r["status"] == "differs" for r in results))


if __name__ == "__main__":
    raise SystemExit(main())
