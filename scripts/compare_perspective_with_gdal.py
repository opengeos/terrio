#!/usr/bin/env python3
"""Compare perspective coordinates and domain rejection through the CLI and GDAL."""

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
            output.append(
                point if len(point) == 2 and all(map(math.isfinite, point)) else None
            )
        except ValueError:
            output.append(None)
    assert len(output) == len(points), result.stdout
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", nargs="?")
    parser.add_argument("--terrio", default=str(ROOT / "target/release/terrio"))
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    osr.UseExceptions()
    osr.SetPROJEnableNetwork(False)
    gdal.PushErrorHandler("CPLQuietErrorHandler")
    reference = json.loads(
        (ROOT / "crates/terrio-crs/tests/data/perspective_reference.json").read_text()
    )
    results = []
    for case in reference["cases"]:
        try:
            definitions = []
            for text in [case["source"], case["target"]]:
                srs = osr.SpatialReference()
                srs.SetFromUserInput(text)
                srs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
                definitions.append(srs)
            forward = osr.CreateCoordinateTransformation(*definitions)
            inverse = osr.CreateCoordinateTransformation(*reversed(definitions))
            points = [s[:2] for s in case["samples"]]
            expected = [oracle(forward, p) for p in points]
            maximum_forward = 0.0
            for target in [case["target"], case["wkt2"]]:
                actual = native(args.terrio, case["source"], target, points)
                for got, want in zip(actual, expected, strict=True):
                    assert (got is None) == (want is None), "forward domain differs"
                    if want is not None:
                        error = (
                            max(abs(a - b) for a, b in zip(got, want, strict=True))
                            * case["metres_per_unit"]
                        )
                        assert error <= 0.001, f"forward {error} metres"
                        maximum_forward = max(maximum_forward, error)
            inverse_points = [p for p in expected if p is not None] + [
                s[0] for s in case["inverse_samples"]
            ]
            expected_inverse = [oracle(inverse, p) for p in inverse_points]
            actual = native(args.terrio, case["target"], case["source"], inverse_points)
            maximum_inverse = 0.0
            for got, want in zip(actual, expected_inverse, strict=True):
                assert (got is None) == (want is None), "inverse domain differs"
                if want is not None:
                    dlon = ((got[0] - want[0] + 180) % 360 - 180) * math.cos(
                        math.radians(want[1])
                    )
                    error = max(abs(dlon), abs(got[1] - want[1]))
                    assert error <= 1e-8, f"inverse {error} degrees"
                    maximum_inverse = max(maximum_inverse, error)
            status = (
                "identical"
                if maximum_forward == maximum_inverse == 0
                else "within tolerance"
            )
            detail = f"max forward {maximum_forward} m; inverse {maximum_inverse} deg; {sum(p is None for p in expected)} forward and {sum(p is None for p in expected_inverse)} inverse refusals matched"
            results.append(dict(name=case["name"], status=status, detail=detail))
        except Exception as error:
            results.append(
                dict(name=case["name"], status="differs", detail=repr(error))
            )
    gdal.PopErrorHandler()
    print(
        json.dumps(results)
        if args.json
        else "\n".join(f"{r['name']}: {r['status']} {r['detail']}" for r in results)
    )
    return int(any(r["status"] == "differs" for r in results))


if __name__ == "__main__":
    raise SystemExit(main())
