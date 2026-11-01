#!/usr/bin/env python3
"""Compare general oblique transformation (ob_tran) coordinates and refusals through the CLI and GDAL."""

import argparse
import json
import math
import os
import shlex
import subprocess
from pathlib import Path

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


def past_pole(case, point):
    northing = (point[1] * case["metres_per_unit"] - case["false_northing_metres"]) / case["radius_metres"]
    return case["refuses_past_pole"] and abs(northing) > math.pi / 2


def moll_family(case):
    return any(f"+o_proj={name}" in case["target"].split() for name in ["moll", "wag4", "wag5"])


def mollweide_longitude(case, point):
    """Recover Mollweide/Wagner longitude in radians, removing false origins in metres."""
    parameters = dict(token.lstrip("+").split("=", 1) for token in shlex.split(case["target"]) if "=" in token)
    radius, factor = case["radius_metres"], case["metres_per_unit"]
    x = (point[0] * factor - float(parameters.get("x_0", 0))) / radius
    y = (point[1] * factor - case["false_northing_metres"]) / radius
    if parameters.get("o_proj") == "wag5":
        cx, cy = 0.90977, 1.65014
    elif parameters.get("o_proj") == "wag4":
        p = math.pi / 3
        r = math.sqrt(2 * math.pi * math.sin(p) / (2 * p + math.sin(2 * p)))
        cx, cy = 2 * r / math.pi, r / math.sin(p)
    else:
        cx, cy = 2 * math.sqrt(2) / math.pi, math.sqrt(2)
    theta = math.asin(y / cy)
    return x / (cx * math.cos(theta))


def mollweide_roundtrip_error(case, point, back):
    """Compare the two valid antimeridian images after a GDAL forward roundtrip."""
    parameters = dict(token.lstrip("+").split("=", 1) for token in shlex.split(case["target"]) if "=" in token)
    factor = case["metres_per_unit"]
    x0 = float(parameters.get("x_0", 0))
    dx = min(abs(back[0] - point[0]) * factor, abs((back[0] + point[0]) * factor - 2 * x0))
    return max(dx, abs(back[1] - point[1]) * factor)


def inverse_probes(points, projected, case):
    """Pair inverse inputs with their own independent geographic reference, if available."""
    return [(image, geographic) for geographic, image in zip(points, projected, strict=True) if image is not None] + case["inverse_samples"]


def ellipsoidal_eqc():
    """Whether GDAL's PROJ has the ellipsoidal eqc (older PROJ applies the spherical formulas)."""
    from osgeo import osr

    source, target = osr.SpatialReference(), osr.SpatialReference()
    source.SetFromUserInput("+proj=longlat +datum=WGS84")
    target.SetFromUserInput("+proj=eqc +datum=WGS84")
    source.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    northing = osr.CoordinateTransformation(source, target).TransformPoint(0, 45)[1]
    return abs(northing - 6378137 * math.pi / 4) > 1000


def main():
    from osgeo import gdal, osr

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", nargs="?")
    parser.add_argument("--terrio", default=str(ROOT / "target/release/terrio"))
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    osr.UseExceptions()
    osr.SetPROJEnableNetwork(False)
    gdal.PushErrorHandler("CPLQuietErrorHandler")
    reference = json.loads(
        (ROOT / "crates/terrio-crs/tests/data/ob_tran_reference.json").read_text()
    )
    # The frozen grid supplies the geographic probes. Derive Wagner cases with GDAL's
    # own WKT encodings, so every member of the shared inverse kernel is compared live.
    extra = []
    for case in reference["cases"]:
        if "+o_proj=moll" not in case["target"].split():
            continue
        for name in ["wag4", "wag5"]:
            target = case["target"].replace("+o_proj=moll", f"+o_proj={name}")
            srs = osr.SpatialReference()
            srs.SetFromUserInput(target)
            extra.append(dict(case, name=case["name"].replace("ob_tran moll ", f"ob_tran {name} "), target=target,
                              inverse_samples=[(point, None) for point, _ in case["inverse_samples"]],
                              wkt1=srs.ExportToWkt(["FORMAT=WKT1_GDAL"]), wkt2=srs.ExportToWkt(["FORMAT=WKT2_2019"])))
    results = []
    for case in [*reference["cases"], *extra]:
        if "+o_proj=eqc " in case["target"] and "+datum=" in case["target"] and not ellipsoidal_eqc():
            detail = "this PROJ has no ellipsoidal eqc (PROJ 9.8.1, which the corpus uses, does)"
            results.append(dict(name=case["name"], status="not compared", detail=detail))
            continue
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
            for target in [case["target"], case["wkt2"], case["wkt1"]]:
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
            probes = inverse_probes(points, expected, case)
            inverse_points = [p for p, _ in probes]
            expected_inverse = [oracle(inverse, p) for p in inverse_points]
            actual = native(args.terrio, case["target"], case["source"], inverse_points)
            maximum_inverse = 0.0
            edge_differences = 0
            for (point, geographic_reference), got, want in zip(probes, actual, expected_inverse, strict=True):
                if past_pole(case, point):
                    # PROJ's spherical sinusoidal inverse returns a latitude past a pole, which
                    # the rotation turns into a point; terrio's sinusoidal refuses it.
                    assert got is None, "inverse past a pole"
                    continue
                if want is None and got is not None and moll_family(case):
                    # The only accepted GDAL refusal is the documented eight-ulp domain band.
                    # Verify the unrotated Mollweide domain analytically, then check the inverse
                    # against the original geographic sample rather than skipping its values.
                    lam = mollweide_longitude(case, point)
                    assert abs(abs(lam) - math.pi) <= 8 * math.ulp(math.pi), "not on Mollweide/Wagner edge"
                    edge_differences += 1
                    if geographic_reference is None:
                        # A standalone inverse probe may have no geographic oracle because
                        # PROJ refused its edge. Independently validate GDAL's forward image.
                        back = oracle(forward, got)
                        assert back is not None, "edge inverse has no valid forward image"
                        error = mollweide_roundtrip_error(case, point, back)
                        assert error <= 0.001, f"edge roundtrip {error} metres"
                        maximum_forward = max(maximum_forward, error)
                        continue
                    want = geographic_reference
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
                if maximum_forward == maximum_inverse == 0 and edge_differences == 0
                else "within tolerance"
            )
            detail = f"max forward {maximum_forward} m; inverse {maximum_inverse} deg; {sum(p is None for p in expected)} forward and {sum(p is None for p in expected_inverse)} inverse oracle refusals; {edge_differences} documented antimeridian acceptances"
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
