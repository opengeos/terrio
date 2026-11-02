#!/usr/bin/env python3
"""Compare two-point oblique Mercator coordinates and refusals through the CLI and GDAL."""

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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", nargs="?")
    parser.add_argument("--terrio", default=str(ROOT / "target/release/terrio"))
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    osr.UseExceptions()
    osr.SetPROJEnableNetwork(False)
    gdal.PushErrorHandler("CPLQuietErrorHandler")
    reference = json.loads((ROOT / "crates/terrio-crs/tests/data/omerc_two_point_reference.json").read_text())
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
            for target in [case["target"], case["wkt2"], case["wkt1"], case["wkt1_esri"]]:
                actual = native(args.terrio, case["source"], target, points)
                for got, want in zip(actual, expected, strict=True):
                    assert (got is None) == (want is None), "forward domain differs"
                    if want is not None:
                        error = max(abs(a - b) for a, b in zip(got, want, strict=True)) * case["metres_per_unit"]
                        assert error <= 0.001, f"forward {error} metres"
                        maximum_forward = max(maximum_forward, error)
            inverse_points = [p for p in expected if p is not None] + [s[0] for s in case["inverse_samples"]]
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
            detail = (
                f"max forward {maximum_forward} m (PROJ string, WKT2, WKT1, ESRI WKT1); inverse {maximum_inverse} deg; "
                f"{sum(p is None for p in expected_inverse)} inverse refusals matched"
            )
            results.append(dict(name=case["name"], status=status, detail=detail))
        except Exception as error:
            results.append(dict(name=case["name"], status="differs", detail=repr(error)))
    # Explicit line poles, their finite neighbours and PROJ-only setup switches. These
    # supplement the frozen corpus with the installed GDAL/PROJ oracle.
    for figure in ["+R=6371000", "+datum=WGS84"]:
        source = "+proj=longlat " + figure
        # At lat_0=0 the same-meridian line has lam_0=20 and gamma_0=0.
        b = 1.0 if figure.startswith("+R") else 1 / math.sqrt(1 - 0.0066943799901413165)
        pole = 20 + 90 / b
        points = [(pole, 0), (20 - 90 / b, 0), (pole - 0.1, 0), (pole + 0.1, 0), (20, 90), (20, -90)]
        for flags in ["", " +lon_0=120 +no_off", " +no_rot"]:
            target = "+proj=omerc +lat_1=10 +lon_1=20 +lat_2=40 +lon_2=20 " + figure + flags
            name = "line poles and switches: " + figure + flags
            try:
                definitions = []
                for text in [source, target]:
                    srs = osr.SpatialReference()
                    srs.SetFromUserInput(text)
                    srs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
                    definitions.append(srs)
                forward = osr.CreateCoordinateTransformation(*definitions)
                expected = [oracle(forward, point) for point in points]
                assert expected[0] is None and expected[1] is None, "oracle did not refuse line poles"
                actual = native(args.terrio, source, target, points)
                maximum = 0.0
                for got, want in zip(actual, expected, strict=True):
                    assert (got is None) == (want is None), "line-pole domain differs"
                    if want is not None:
                        error = max(abs(a - b) for a, b in zip(got, want, strict=True))
                        assert error <= 0.001, f"forward {error} metres"
                        maximum = max(maximum, error)
                results.append(dict(name=name, status="identical" if maximum == 0 else "within tolerance",
                                    detail=f"max forward {maximum} m; both line-pole refusals matched"))
            except Exception as error:
                results.append(dict(name=name, status="differs", detail=repr(error)))
    for case in reference["invalid"]:
        definition = case["definition"] + " +datum=WGS84"
        proj_refuses = True
        try:
            srs = osr.SpatialReference()
            srs.SetFromUserInput(definition)
            source = osr.SpatialReference()
            source.SetFromUserInput("+proj=longlat +datum=WGS84")
            osr.CoordinateTransformation(source, srs)
            proj_refuses = False
        except RuntimeError:
            pass
        refused = subprocess.run(
            [args.terrio, "crs", "+proj=longlat +datum=WGS84", "--to", definition],
            input="0 0\n",
            text=True,
            capture_output=True,
            env=dict(os.environ, TERRIO_NETWORK="OFF"),
        ).returncode != 0
        status = "identical" if refused == proj_refuses else "differs"
        results.append(dict(name=f"refusal: {case['definition']}", status=status, detail=f"PROJ refuses {proj_refuses}, terrio refuses {refused}"))
    gdal.PopErrorHandler()
    print(json.dumps(results) if args.json else "\n".join(f"{r['name']}: {r['status']} {r['detail']}" for r in results))
    return int(any(r["status"] == "differs" for r in results))


if __name__ == "__main__":
    raise SystemExit(main())
