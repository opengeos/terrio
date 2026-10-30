"""Strict kernel and production-encoder replay. An unimplemented candidate always fails."""

import argparse
import json
import math
from pathlib import Path
import subprocess

import shapely
from shapely.geometry import MultiPolygon, Polygon


def invoke(binary, polygons):
    result = subprocess.run(
        [str(binary)], input=json.dumps({"polygons": polygons}, allow_nan=False),
        text=True, capture_output=True, timeout=20, check=True,
    )
    return json.loads(result.stdout)


def geometry(polygons):
    for polygon in polygons:
        if not polygon:
            raise ValueError("empty output polygon")
        for ring in polygon:
            if len(ring) < 4 or ring[0] != ring[-1] or len({tuple(p) for p in ring}) < 3:
                raise ValueError("output ring is unclosed or collapsed")
            if any(len(p) != 2 or any(not math.isfinite(x) for x in p) for p in ring):
                raise ValueError("output contains non-finite or non-XY coordinates")
            if Polygon(ring).area == 0:
                raise ValueError("output contains zero-area ring")
    return MultiPolygon([Polygon(p[0], p[1:]) for p in polygons])


def check_case(case, polygons, adapter):
    actual = geometry(polygons)
    expected = shapely.from_wkb(bytes.fromhex(case["polygonal_wkb"]))
    if not actual.is_valid:
        raise ValueError("invalid polygonal output")
    if actual.is_empty != expected.is_empty:
        raise ValueError("empty/nonempty mismatch")
    if sum(len(p.interiors) for p in actual.geoms) != case["hole_count"]:
        raise ValueError("hole-count mismatch")
    exact_integral = all(x == int(x) for polygon in case["polygonal_coordinates"] for ring in polygon for point in ring for x in point)
    if exact_integral and not actual.equals(expected):
        raise ValueError("exact integral fixture topology mismatch")
    if not actual.is_empty:
        ratio = actual.symmetric_difference(expected).area / max(1, actual.area, expected.area)
        magnitude = max(1, *(abs(x) for x in actual.bounds + expected.bounds))
        distance = actual.boundary.hausdorff_distance(expected.boundary)
        if not math.isfinite(ratio) or not math.isfinite(distance) or ratio > 1e-10 or distance > max(1e-10, 8 * math.ulp(magnitude)):
            raise ValueError(f"floating mismatch: area ratio={ratio}, boundary distance={distance}")
    reference_tile = invoke(adapter, case["polygonal_coordinates"])
    if reference_tile["status"] == "encoder_overflow":
        if case["id"] != "wide-i32-bowtie":
            raise ValueError("unexpected reference encoder overflow")
        return "kernel-pass/encoder-domain-excluded"
    if reference_tile["status"] != "ok":
        raise ValueError(f"reference encoder failed: {reference_tile}")
    candidate_tile = invoke(adapter, polygons)
    if candidate_tile["status"] != "ok":
        raise ValueError(f"candidate encoder failed: {candidate_tile}")
    if candidate_tile["present"] != reference_tile["present"]:
        raise ValueError("tile feature-presence mismatch")
    tile_a = MultiPolygon([Polygon(p[0], p[1:]) for p in candidate_tile["polygons"]])
    tile_b = MultiPolygon([Polygon(p[0], p[1:]) for p in reference_tile["polygons"]])
    if tile_a.is_empty != tile_b.is_empty or (not tile_a.is_empty and not tile_a.equals(tile_b)):
        raise ValueError("encoded integer polygon geometry mismatch")
    return "pass"


def main():
    root = Path(__file__).resolve().parents[1]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--candidate", type=Path, default=Path("/build/debug/candidate"))
    parser.add_argument("--reference-self-check", action="store_true", help="check fixture/adapter plumbing only; not candidate acceptance")
    args = parser.parse_args()
    capture = json.loads((root / "fixtures/observations.json").read_text())
    if shapely.geos_version_string != capture["reference"]["geos"] or shapely.__version__ != capture["reference"]["shapely"]:
        raise SystemExit("reference runtime version mismatch")
    failures, results = [], []
    for case in capture["observations"]:
        try:
            if args.reference_self_check:
                polygons = case["polygonal_coordinates"]
            else:
                response = invoke(args.candidate, case["input_polygons"])
                if response.get("status") != "ok":
                    raise ValueError(f"candidate status: {response.get('status')}")
                polygons = response["polygons"]
            status = check_case(case, polygons, root / "reference/tile-encoder")
            results.append({"id": case["id"], "status": status})
        except (ValueError, KeyError, OSError, subprocess.SubprocessError, shapely.errors.GEOSException) as error:
            failures.append({"id": case["id"], "message": str(error)})
    print(json.dumps({
        "mode": "reference-self-check" if args.reference_self_check else "candidate-acceptance",
        "cases": len(capture["observations"]), "results": results, "failures": failures,
    }, indent=2))
    raise SystemExit(1 if failures else 0)


if __name__ == "__main__":
    main()
