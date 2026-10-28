#!/usr/bin/env python3
"""Compare ISEA coordinates through the CLI and GDAL."""

import argparse
import json
import math
import os
import subprocess
from pathlib import Path

from osgeo import gdal, osr

ROOT = Path(__file__).resolve().parents[1]


def oracle(operation, point):
    try:
        result = operation.TransformPoint(*point)[:2]
        return list(result) if all(map(math.isfinite, result)) else None
    except RuntimeError:
        return None


def native(executable, source, target, points):
    result = subprocess.run(
        [executable, "crs", source, "--to", target],
        input="".join(f"{x} {y}\n" for x, y in points),
        text=True,
        capture_output=True,
        check=True,
        env=dict(os.environ, TERRIO_NETWORK="OFF"),
    )
    output = []
    for line in result.stdout.splitlines():
        try:
            point = list(map(float, line.split()))
            output.append(point if len(point) == 2 and all(map(math.isfinite, point)) else None)
        except ValueError:
            output.append(None)
    assert len(output) == len(points), result.stdout
    return output


def transformation(source, target):
    definitions = []
    for text in [source, target]:
        srs = osr.SpatialReference()
        srs.SetFromUserInput(text)
        srs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
        definitions.append(srs)
    return definitions


def same_isea(reference):
    """Whether GDAL's PROJ computes isea as PROJ 9.8.1 does, which the corpus uses.

    PROJ 9.4 has no inverse and a forward up to a few millimetres different; PROJ 9.9
    replaced the method (another origin, the authalic latitude on an ellipsoid).
    """
    case = reference["cases"][0]
    source, target = transformation(case["source"], case["target"])
    try:
        forward = osr.CoordinateTransformation(source, target)
        inverse = osr.CoordinateTransformation(target, source)
    except RuntimeError:
        return False
    for sample in case["samples"]:
        got = oracle(forward, sample[:2])
        if got is None or max(abs(a - b) for a, b in zip(got, sample[2], strict=True)) > 0.001:
            return False
        back = oracle(inverse, sample[2])
        if (back is None) != (sample[3] is None):
            return False
    return True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", nargs="?")
    parser.add_argument("--terrio", default=str(ROOT / "target/release/terrio"))
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    osr.UseExceptions()
    osr.SetPROJEnableNetwork(False)
    gdal.PushErrorHandler("CPLQuietErrorHandler")
    reference = json.loads((ROOT / "crates/terrio-crs/tests/data/isea_reference.json").read_text())
    results = []
    comparable = same_isea(reference)
    version = f"{osr.GetPROJVersionMajor()}.{osr.GetPROJVersionMinor()}.{osr.GetPROJVersionMicro()}"
    for case in reference["cases"]:
        if not comparable:
            detail = f"PROJ {version} computes isea otherwise than PROJ 9.8.1, which the corpus uses"
            results.append(dict(name=case["name"], status="not compared", detail=detail))
            continue
        try:
            definitions = transformation(case["source"], case["target"])
            forward = osr.CreateCoordinateTransformation(*definitions)
            inverse = osr.CreateCoordinateTransformation(*reversed(definitions))
            points = [s[:2] for s in case["samples"]]
            expected = [oracle(forward, p) for p in points]
            # PROJ projects every point.
            assert all(p is not None for p in expected), "PROJ refused a point"
            maximum_forward = 0.0
            for target in [case["target"], case["wkt2"], case["wkt1"], case["wkt1_esri"]]:
                actual = native(args.terrio, case["source"], target, points)
                for got, want in zip(actual, expected, strict=True):
                    assert got is not None, "terrio refused a point"
                    error = max(abs(a - b) for a, b in zip(got, want, strict=True)) * case["metres_per_unit"]
                    assert error <= 0.001, f"forward {error} metres"
                    maximum_forward = max(maximum_forward, error)
            # GDAL reads the WKT1 that terrio writes (GDAL's custom_proj4, with the PROJ string).
            described = subprocess.run(
                [args.terrio, "crs", case["target"], "--wkt"],
                capture_output=True,
                text=True,
                check=True,
            ).stdout
            written = next(line for line in described.splitlines() if line.startswith("PROJCS["))
            reopened = osr.SpatialReference()
            reopened.SetFromUserInput(written)
            reopened.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
            through = osr.CreateCoordinateTransformation(definitions[0], reopened)
            for point, want in zip(points, expected, strict=True):
                got = oracle(through, point)
                error = max(abs(a - b) for a, b in zip(got, want, strict=True)) * case["metres_per_unit"]
                assert error <= 0.001, f"GDAL through terrio's WKT1: {error} metres"
                maximum_forward = max(maximum_forward, error)
            # PROJ's inverse, which is not the forward's on an ellipsoid and is refused outside
            # its default grid.
            inverse_points = expected + [s[0] for s in case["inverse_samples"]]
            expected_inverse = [oracle(inverse, p) for p in inverse_points]
            actual = native(args.terrio, case["target"], case["source"], inverse_points)
            maximum_inverse = 0.0
            for got, want in zip(actual, expected_inverse, strict=True):
                assert (got is None) == (want is None), "inverse domain differs"
                if want is not None:
                    dlon = ((got[0] - want[0] + 180) % 360 - 180) * math.cos(math.radians(want[1]))
                    error = max(abs(dlon), abs(got[1] - want[1]))
                    assert error <= 1e-8, f"inverse {error} degrees"
                    maximum_inverse = max(maximum_inverse, error)
            status = "identical" if maximum_forward == maximum_inverse == 0 else "within tolerance"
            detail = f"max forward {maximum_forward} m; inverse {maximum_inverse} deg"
            results.append(dict(name=case["name"], status=status, detail=detail))
        except Exception as error:
            results.append(dict(name=case["name"], status="differs", detail=repr(error)))
    gdal.PopErrorHandler()
    print(
        json.dumps(results)
        if args.json
        else "\n".join(f"{r['name']}: {r['status']} {r['detail']}" for r in results)
    )
    return int(any(r["status"] == "differs" for r in results))


if __name__ == "__main__":
    raise SystemExit(main())
