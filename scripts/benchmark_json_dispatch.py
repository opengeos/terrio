#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Measure early/late JSON root-type dispatch and flat semantics expansion.

Writes generated inputs under the external data directory. On Linux, /proc I/O
counters measure actual read bytes/calls after subtracting counter-read overhead.
Times are observations, not pass/fail performance thresholds.
"""
import argparse
import json
from pathlib import Path
from statistics import mean
import time


def counters():
    path = Path('/proc/self/io')
    return {k: int(v) for k, v in (line.split(':') for line in path.read_text().splitlines())}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('data', type=Path)
    parser.add_argument('--padding', type=int, default=16 * 1024 * 1024)
    parser.add_argument('--faces', type=int, default=1000)
    parser.add_argument('--repeat', type=int, default=3)
    args = parser.parse_args()
    import terrio
    folder = args.data / 'json-dispatch-benchmark'
    folder.mkdir(parents=True, exist_ok=True)
    padding = 'x' * args.padding
    early = dict(type='FeatureCollection', padding=padding, features=[])
    late = dict(padding=padding, features=[], type='FeatureCollection')
    results = []
    for name, doc in [('early', early), ('late', late)]:
        path = folder / f'{name}.json'
        path.write_text(json.dumps(doc, separators=(',', ':')))
        terrio.open_vector(str(path))  # Warm runtime and filesystem caches.
        start = counters(); end = counters()
        overhead = {k: end[k] - start[k] for k in ('rchar', 'syscr')}
        timings, reads, calls = [], [], []
        for _ in range(args.repeat):
            before = counters()
            start = time.perf_counter()
            dataset = terrio.open_vector(str(path))
            timings.append(time.perf_counter() - start)
            after = counters()
            assert len(dataset[0]) == 0
            reads.append(after['rchar'] - before['rchar'] - overhead['rchar'])
            calls.append(after['syscr'] - before['syscr'] - overhead['syscr'])
        results.append(dict(case=name, file_bytes=path.stat().st_size, seconds=mean(timings),
                            read_bytes=reads, read_calls=calls))
    vertices = [[0, y, z] for y in range(args.faces + 1) for z in (0, 1)]
    faces = [[[2*i, 2*i+2, 2*i+3, 2*i+1]] for i in range(args.faces)]
    doc = dict(type='CityJSON', version='2.0', vertices=vertices,
               transform=dict(scale=[1,1,1], translate=[0,0,0]),
               CityObjects=dict(wall=dict(type='Building', geometry=[dict(type='CompositeSurface', lod='2',
                   boundaries=faces, semantics=dict(surfaces=[dict(type='WallSurface') for _ in faces],
                                                   values=list(range(args.faces))))])))
    path = folder / 'semantics.city.json'
    path.write_text(json.dumps(doc, separators=(',', ':')))
    start = time.perf_counter()
    layer = terrio.open_vector(str(path))[0]
    elapsed = time.perf_counter() - start
    features = layer.to_geojson()['features']
    results.append(dict(case='semantics', file_bytes=path.stat().st_size, faces=len(features), seconds=elapsed,
                        owned_field_text_bytes=sum(len(v.encode()) for f in features for v in f['properties'].values() if isinstance(v,str))))
    print(json.dumps(results, indent=2))


if __name__ == '__main__':
    main()
