#!/usr/bin/env python3
"""Compare geocentric XYZ endpoints with GDAL and the frozen reference suite.

Cartesian/projected coordinates and heights allow 1 mm; geographic angles allow
1e-8 degrees. Two nonzero custom BoundCRS cases use their declared Helmert operation as an
explicit GDAL pipeline because PROJ 9.8.1's CRS factory skips that operation.
An explicit no-op also verifies zero Helmert Cartesian vector preservation.
"""
import argparse
import json
import os
import subprocess
from pathlib import Path

from osgeo import osr

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", nargs="?")
    parser.add_argument("--terrio", default=str(ROOT / "target/release/terrio"))
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    reference = json.loads((ROOT / "crates/terrio-crs/tests/data/geocentric_reference.json").read_text())
    osr.UseExceptions()
    osr.SetPROJEnableNetwork(False)
    results = []
    for case in reference["cases"]:
        try:
            definitions = []
            for text in [case["src"], case["dst"]]:
                srs = osr.SpatialReference()
                srs.SetFromUserInput(text)
                srs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
                definitions.append(srs)
            if case.get("source_epoch") is not None:
                definitions[0].SetCoordinateEpoch(case["source_epoch"])
            options = osr.CoordinateTransformationOptions()
            if case.get("pipeline"):
                options.SetOperation(case["pipeline"])
            operation = osr.CreateCoordinateTransformation(*definitions, options)
            expected = [operation.TransformPoint(*p) for p in case["points"]]
            command = [args.terrio, "crs", case["src"], "--to", case["dst"]]
            if case.get("source_epoch") is not None:
                command += ["--s_coord_epoch", str(case["source_epoch"])]
            process = subprocess.run(command, input="".join(" ".join(map(str, p)) + "\n" for p in case["points"]), text=True,
                                     capture_output=True, check=True, env=dict(os.environ, TERRIO_NETWORK="OFF"))
            actual = [list(map(float, line.split())) for line in process.stdout.splitlines()]
            assert len(actual) == len(expected)
            maxima = [0.0, 0.0, 0.0]
            for row, oracle in zip(actual, expected, strict=True):
                assert len(row) == 3
                for i in range(3):
                    error = abs(row[i] - oracle[i])
                    maxima[i] = max(maxima[i], error)
                    assert error <= case["tolerance"][i], f"axis {i}: {error} > {case['tolerance'][i]}"
            status = "identical" if maxima == [0.0, 0.0, 0.0] else "within tolerance"
            detail = f"{len(actual)} points; max component errors {maxima}"
            if case.get("pipeline") == "+proj=noop":
                detail += "; explicit zero Helmert (Cartesian vector unchanged)"
            elif case.get("pipeline"):
                detail += "; explicit declared Helmert pipeline (PROJ factory discrepancy)"
            results.append({"name": case["name"], "status": status, "detail": detail})
        except Exception as error:
            results.append({"name": case["name"], "status": "differs", "detail": repr(error)})
    print(json.dumps(results, indent=2) if args.json else "\n".join(f"{r['name']}: {r['status']} {r['detail']}" for r in results))
    return int(any(r["status"] == "differs" for r in results))


if __name__ == "__main__":
    raise SystemExit(main())
