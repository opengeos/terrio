"""Strict GDAL/GEOS differential tile writes for the independent linework repair.

Compare feature presence and every feature geometry at each zoom, not just total
area. Requires the provisioned GDAL/Shapely Python environment and CLI programs.
All generated data and archives must go under --scratch, outside the repository.
"""

import argparse
from collections import Counter, defaultdict
import json
import math
from pathlib import Path
import subprocess

from osgeo import gdal
import shapely

gdal.UseExceptions()


def run(command):
    completed = subprocess.run(command, capture_output=True, text=True)
    if completed.returncode:
        raise RuntimeError(f"{Path(command[0]).name}: {completed.stderr[-2000:]}")


def read_features(path, key, zoom):
    dataset = gdal.OpenEx(str(path), gdal.OF_VECTOR, open_options=[f"ZOOM_LEVEL={zoom}", "CLIP=NO"])
    layer = dataset.GetLayer(0)
    if layer.GetLayerDefn().GetFieldIndex(key) < 0:
        raise RuntimeError(f"required key {key} missing from {path.name}")
    groups = defaultdict(list)
    for feature in layer:
        geometry = feature.GetGeometryRef()
        if geometry is None:
            raise RuntimeError("null geometry in polygon tile result")
        groups[str(feature.GetField(key))].append(shapely.from_wkb(bytes(geometry.ExportToWkb())))
    return groups


def equivalent(a, b):
    if not b.is_valid:
        # The reference writer can also yield invalid rings after integer
        # quantization. Require its exact normalized ring representation here;
        # never claim that such a matched result has become OGC-valid.
        magnitude = max(1, *(abs(x) for x in a.bounds + b.bounds))
        agrees = a.normalize().equals_exact(b.normalize(), max(1e-9, 8 * math.ulp(magnitude)))
        return agrees, "reference-invalid exact ring comparison", 0.0 if agrees else None
    if not a.is_valid:
        return False, "new invalid geometry", None
    if a.is_empty or b.is_empty:
        return a.is_empty == b.is_empty, "empty status", 0.0
    magnitude = max(1, *(abs(x) for x in a.bounds + b.bounds))
    tolerance = max(1e-9, 8 * math.ulp(magnitude))
    if any(abs(x - y) > tolerance for x, y in zip(a.bounds, b.bounds)):
        return False, "bounds", None
    ratio = a.symmetric_difference(b).area / max(1, a.area, b.area)
    distance = a.boundary.hausdorff_distance(b.boundary)
    return math.isfinite(ratio) and math.isfinite(distance) and ratio <= 1e-10 and distance <= tolerance, "geometry", ratio


def compare(reference, actual):
    failures, worst = [], 0.0
    for key in sorted(set(reference) | set(actual)):
        expected, observed = reference.get(key, []), actual.get(key, [])
        if len(expected) != len(observed):
            failures.append({"key": key, "reason": "feature count", "reference": len(expected), "actual": len(observed)})
            continue
        remaining = list(expected)
        for geometry in observed:
            match = None
            for index, candidate in enumerate(remaining):
                agrees, _, ratio = equivalent(geometry, candidate)
                if agrees:
                    match = index
                    worst = max(worst, ratio)
                    break
            if match is None:
                failures.append({"key": key, "reason": "no matching feature geometry", "bounds": geometry.bounds})
                break
            remaining.pop(match)
    return failures, worst



def invalid_collinear_order_signature(geometry, zoom):
    """Exact oriented grid rings, retaining every non-collinear polygon group.

    OGR may attach a collapsed collinear MVT ring to a different preceding shell.
    This is a reported serialization difference, never a valid-geometry match.
    Ring direction, multiplicity and all coordinates must still agree exactly.
    """
    if geometry.is_valid or geometry.geom_type not in {"Polygon", "MultiPolygon"}:
        return None
    world = 20037508.342789244
    step = 2 * world / (4096 * (2 ** zoom))
    all_rings, groups, collapsed = Counter(), Counter(), 0
    polygons = geometry.geoms if geometry.geom_type == "MultiPolygon" else [geometry]
    for polygon in polygons:
        group = []
        for ring in [polygon.exterior, *polygon.interiors]:
            points = []
            for x, y in list(ring.coords)[:-1]:
                gx, gy = round((x + world) / step), round((world - y) / step)
                tolerance = max(1e-9, 8 * math.ulp(max(world, abs(x), abs(y))))
                if abs(x - (gx * step - world)) > tolerance or abs(y - (world - gy * step)) > tolerance:
                    return None
                points.append((gx, gy))
            if len(points) < 3:
                return None
            minimum = min(points)
            canonical = min(tuple(points[i:] + points[:i]) for i, point in enumerate(points) if point == minimum)
            all_rings[canonical] += 1
            first = points[0]
            second = next((point for point in points if point != first), first)
            collinear = all((second[0] - first[0]) * (point[1] - first[1]) == (second[1] - first[1]) * (point[0] - first[0]) for point in points)
            if collinear:
                collapsed += 1
            else:
                group.append(canonical)
        if group:
            groups[tuple(sorted(group))] += 1
    if not collapsed:
        return None
    return tuple(sorted(all_rings.items())), tuple(sorted(groups.items())), collapsed


def only_collinear_order_differs(expected, actual, zoom):
    if len(expected) != len(actual):
        return None
    remaining, changed = list(expected), 0
    for geometry in actual:
        match = next((i for i, reference in enumerate(remaining) if equivalent(geometry, reference)[0]), None)
        if match is None:
            signature = invalid_collinear_order_signature(geometry, zoom)
            if signature is None:
                return None
            match = next((i for i, reference in enumerate(remaining) if invalid_collinear_order_signature(reference, zoom) == signature), None)
            if match is None:
                return None
            changed += 1
        remaining.pop(match)
    return changed or None

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--terrio", required=True, type=Path)
    parser.add_argument("--data", required=True, type=Path, help="directory containing countries.geojson and UScounties.gpkg")
    parser.add_argument("--scratch", required=True, type=Path)
    parser.add_argument("--fixtures", type=Path, default=Path("crates/terrio/tests/data/make-valid.json"))
    parser.add_argument("--baseline-dir", type=Path, help="previous binary's frozen tile archives; unchanged known GDAL differences are reported separately")
    args = parser.parse_args()
    args.scratch.mkdir(parents=True, exist_ok=True)
    countries, counties = args.data / "countries.geojson", args.data / "UScounties.gpkg"
    for path in (countries, counties, args.terrio):
        if not path.is_file():
            raise SystemExit("required input missing: " + str(path))
    filtered = args.scratch / "countries.geojson"
    if filtered.exists():
        raise SystemExit("use a new scratch directory; countries output already exists")
    gdal.VectorTranslate(str(filtered), str(countries), format="GeoJSON", where="ADMIN <> 'Antarctica'")
    observations = json.loads(args.fixtures.read_text())["observations"]
    features = []
    for case in observations:
        if case["id"] in {"wide-i32-bowtie", "near-i32-max", "empty", "empty-component"}:
            continue
        features.append({"type": "Feature", "properties": {"case_id": case["id"]}, "geometry": {"type": "MultiPolygon", "coordinates": case["input_polygons"]}})
    synthetic = args.scratch / "original-invalid.geojson"
    synthetic.write_text(json.dumps({"type": "FeatureCollection", "features": features}) + "\n")
    results = []
    cases = [(filtered, "ADM0_A3", 6), (counties, "FIPS", 3), (synthetic, "case_id", 3)]
    for source, key, maxzoom in cases:
        reference = args.scratch / (source.stem + "-gdal.pmtiles")
        actual = args.scratch / (source.stem + "-terrio.pmtiles")
        run(["ogr2ogr", "-dsco", f"MAXZOOM={maxzoom}", str(reference), str(source)])
        run([str(args.terrio.resolve()), "convert", "--maxzoom", str(maxzoom), str(source), str(actual)])
        for zoom in range(maxzoom + 1):
            expected, observed = read_features(reference, key, zoom), read_features(actual, key, zoom)
            failures, worst = compare(expected, observed)
            collinear_order, geometry_failures = [], []
            for failure in failures:
                value = failure["key"]
                changed = only_collinear_order_differs(expected.get(value, []), observed.get(value, []), zoom)
                if changed:
                    collinear_order.append({"key": value, "features": changed, "reason": "identical oriented integer rings; only collinear-ring association differs"})
                else:
                    geometry_failures.append(failure)
            failures = geometry_failures
            unchanged = []
            if args.baseline_dir and failures:
                baseline = read_features(args.baseline_dir / (source.stem + "-terrio.pmtiles"), key, zoom)
                new_failures = []
                for failure in failures:
                    value = failure["key"]
                    delta, _ = compare({value: baseline.get(value, [])}, {value: observed.get(value, [])})
                    if delta:
                        new_failures.append(failure)
                    else:
                        unchanged.append(failure)
                failures = new_failures
            invalid_reference = {k: sum(not g.is_valid for g in v) for k, v in expected.items() if any(not g.is_valid for g in v)}
            result = {"dataset": source.stem, "zoom": zoom, "reference_features": sum(map(len, expected.values())), "actual_features": sum(map(len, observed.values())), "worst_symmetric_difference_ratio": worst, "failures": failures, "unchanged_baseline_differences": unchanged, "invalid_collinear_order_differences": collinear_order, "reference_invalid_geometries": invalid_reference}
            results.append(result)
            print(json.dumps(result), flush=True)
    report = {"gdal": gdal.VersionInfo("RELEASE_NAME"), "geos": shapely.geos_version_string, "shapely": shapely.__version__, "comparisons": results}
    (args.scratch / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    raise SystemExit(1 if any(r["failures"] for r in results) else 0)


if __name__ == "__main__":
    main()
