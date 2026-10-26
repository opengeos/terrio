#!/usr/bin/env python3
"""Compare supported PROJJSON definitions through the public CLI and live GDAL.

Geographic results allow 1e-8 degrees and projected results 1 mm, componentwise.
Definitions are supplied in full, with and without identifiers, in GIS order.
"""
import argparse
import json
import os
import subprocess
from pathlib import Path

from projjson_reference_env import no_grid_proj

from osgeo import osr

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('data', nargs='?')
    parser.add_argument('--terrio', default=str(ROOT / 'target/release/terrio'))
    parser.add_argument('--json', action='store_true')
    args = parser.parse_args()
    reference = json.loads((ROOT / 'crates/terrio-crs/tests/data/projjson_reference.json').read_text())
    osr.UseExceptions()
    osr.SetPROJEnableNetwork(False)
    results = []
    for case in reference['cases']:
        try:
            definitions = []
            texts = [json.dumps(case[key]) for key in ['src', 'dst']]
            for text in texts:
                srs = osr.SpatialReference()
                srs.SetFromUserInput(text)
                srs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
                definitions.append(srs)
            operation = osr.CreateCoordinateTransformation(*definitions)
            expected = [operation.TransformPoint(*p)[:2] for p in case['points']]
            # Independently validate Terrio's exported representation as well as
            # its numeric output. This catches invalid WKT that our own parser
            # might accept and ensures metadata works with other backends.
            exported = []
            for text in texts:
                result = subprocess.run([args.terrio, 'crs', text, '--wkt'], text=True, capture_output=True, check=True,
                                        env=dict(os.environ, TERRIO_NETWORK='OFF'))
                srs = osr.SpatialReference()
                srs.ImportFromWkt(result.stdout.splitlines()[-1])
                assert srs.Validate() == 0
                srs.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
                exported.append(srs)
            roundtrip = osr.CreateCoordinateTransformation(*exported)
            for point, oracle in zip(case['points'], expected, strict=True):
                actual_wkt = roundtrip.TransformPoint(*point)
                for i in range(2):
                    assert abs(actual_wkt[i] - oracle[i]) <= case['tolerance'][i], 'exported WKT differs'
            process = subprocess.run([args.terrio, 'crs', texts[0], '--to', texts[1]],
                                     input=''.join(' '.join(map(str, p)) + '\n' for p in case['points']),
                                     text=True, capture_output=True, check=True, env=dict(os.environ, TERRIO_NETWORK='OFF'))
            actual = [list(map(float, line.split())) for line in process.stdout.splitlines()]
            assert len(actual) == len(expected)
            maxima = [0.0, 0.0]
            for row, oracle in zip(actual, expected, strict=True):
                assert len(row) == 2
                for i in range(2):
                    error = abs(row[i] - oracle[i])
                    assert error <= case['tolerance'][i], f'axis {i}: {error} > {case["tolerance"][i]}'
                    maxima[i] = max(maxima[i], error)
            results.append(dict(name=case['name'], status='identical' if maxima == [0.0, 0.0] else 'within tolerance',
                                detail=f'{len(actual)} points; max component errors {maxima}'))
        except Exception as error:
            results.append(dict(name=case['name'], status='differs', detail=repr(error)))
    print(json.dumps(results, indent=2) if args.json else '\n'.join(f'{r["name"]}: {r["status"]} {r["detail"]}' for r in results))
    return int(any(r['status'] == 'differs' for r in results))


if __name__ == '__main__':
    with no_grid_proj():
        raise SystemExit(main())
