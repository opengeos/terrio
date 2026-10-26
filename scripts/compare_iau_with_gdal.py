#!/usr/bin/env python3
"""Live IAU_2015 comparisons through the public CLI and GDAL/PROJ.

Projected components allow 1 mm and inverse angular components 1e-8 degrees.
Where GDAL refuses an inverse, native refusal or accurate recovery of the
original input is accepted and explicitly reported.
"""
import argparse
import json
import math
import os
import subprocess
from pathlib import Path

from osgeo import gdal, osr

ROOT = Path(__file__).resolve().parents[1]


def coordinates(executable, source, target, points):
    result = subprocess.run([executable, 'crs', source, '--to', target],
                            input=''.join(' '.join(map(str, point)) + '\n' for point in points),
                            capture_output=True, text=True, check=True,
                            env=dict(os.environ, TERRIO_NETWORK='OFF'))
    values = []
    for line in result.stdout.splitlines():
        try:
            value = list(map(float, line.split()))
            values.append(value if len(value) == 2 and all(map(math.isfinite, value)) else None)
        except ValueError:
            values.append(None)
    assert len(values) == len(points), result.stdout
    return values


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('data', nargs='?')
    parser.add_argument('--terrio', default=str(ROOT / 'target/release/terrio'))
    parser.add_argument('--json', action='store_true')
    args = parser.parse_args()
    osr.UseExceptions()
    osr.SetPROJEnableNetwork(False)
    reference = json.loads((ROOT / 'crates/terrio-crs/tests/data/iau_reference.json').read_text())
    results = []
    for case in reference['cases'] + reference['extras']:
        name = case.get('name') or f"IAU_2015:{case['base']} to IAU_2015:{case['code']}"
        try:
            source, target = (case['source'], case['target']) if 'source' in case else [f'IAU_2015:{case[key]}' for key in ['base', 'code']]
            factor = case.get('metres_per_unit', 1.0)
            definitions = []
            for text in [source, target]:
                srs = osr.SpatialReference()
                srs.SetFromUserInput(text)
                srs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
                definitions.append(srs)
            forward = osr.CreateCoordinateTransformation(*definitions)
            expected = [list(forward.TransformPoint(*point)[:2]) for point in case['points']]
            actual = coordinates(args.terrio, source, target, case['points'])
            maxima = [0.0, 0.0]
            for point, oracle in zip(actual, expected, strict=True):
                assert point is not None
                error = max(abs(a-b) for a, b in zip(point, oracle, strict=True)) * factor
                assert error <= 0.001, f'forward {error} metres'
                maxima[0] = max(maxima[0], error)
            inverse = osr.CreateCoordinateTransformation(*reversed(definitions))
            actual = coordinates(args.terrio, target, source, expected)
            refused, recovered = 0, 0
            for index, (point, projected) in enumerate(zip(actual, expected, strict=True)):
                try:
                    oracle = inverse.TransformPoint(*projected)[:2]
                except RuntimeError:
                    refused += 1
                    if point is None:
                        continue
                    oracle = case['points'][index]
                    recovered += 1
                assert point is not None, 'native inverse refused a defined reference point'
                errors = [a-b for a, b in zip(point, oracle, strict=True)]
                longitude_axis = case['longitude_axis']
                errors[longitude_axis] = (errors[longitude_axis]+180)%360-180
                error = max(map(abs, errors))
                assert error <= 1e-8, f'inverse {error} degrees'
                maxima[1] = max(maxima[1], error)
            status = 'identical' if maxima == [0.0, 0.0] and recovered == 0 else 'within tolerance'
            detail = f'max forward {maxima[0]} m; inverse {maxima[1]} degrees; GDAL refused {refused} inverse samples, native recovered {recovered}'
            results.append(dict(name=name, status=status, detail=detail))
        except Exception as error:
            results.append(dict(name=name, status='differs', detail=repr(error)))
    environment = f'GDAL {gdal.VersionInfo("RELEASE_NAME")} / PROJ {osr.GetPROJVersionMajor()}.{osr.GetPROJVersionMinor()}.{osr.GetPROJVersionMicro()}'
    for result in results:
        result['reference'] = environment
    print(json.dumps(results) if args.json else '\n'.join(f'{r["name"]}: {r["status"]} {r["detail"]}' for r in results))
    return int(any(result['status'] == 'differs' for result in results))


if __name__ == '__main__':
    raise SystemExit(main())
