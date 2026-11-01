#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Measure CityGML dispatch I/O and cached-CRS parsing on external fixtures.

Linux read counters include identification and parsing, with counter-read overhead
subtracted. Warm timings are observations, not performance thresholds.
"""
import argparse
import json
from pathlib import Path
from statistics import median
import time


def counters():
    return {k: int(v) for k, v in (line.split(":") for line in Path("/proc/self/io").read_text().splitlines())}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--repeat", type=int, default=5)
    parser.add_argument("--restrict-drivers", action="store_true", help="exercise the CityGML-only identification gate")
    args = parser.parse_args()
    if args.repeat < 1:
        parser.error("--repeat must be at least 1")
    if not Path("/proc/self/io").is_file():
        parser.error("this I/O benchmark requires Linux with /proc/self/io")
    import terrio

    if args.restrict_drivers:
        terrio.set_allowed_drivers(["CityGML"])
    folder = args.data / "citygml-dispatch-benchmark"
    folder.mkdir(parents=True, exist_ok=True)
    root = '<c:CityModel xmlns:c="http://www.opengis.net/citygml/3.0"/>'
    comment = "<!--" + "x" * 32768 + "-->"
    paths = []
    for name, text in [("early", root + comment), ("late", comment + root)]:
        path = folder / f"{name}.gml"
        path.write_text(text)
        paths.append(path)
    paths.append(args.data / "citymodels/lod3_building_v2.gml")
    results = []
    for path in paths:
        terrio.open_vector(str(path))
        before, after = counters(), counters()
        overhead = {key: after[key] - before[key] for key in ("rchar", "syscr")}
        times, reads, calls = [], [], []
        for _ in range(args.repeat):
            before = counters()
            start = time.perf_counter()
            dataset = terrio.open_vector(str(path))
            times.append(time.perf_counter() - start)
            after = counters()
            reads.append(after["rchar"] - before["rchar"] - overhead["rchar"])
            calls.append(after["syscr"] - before["syscr"] - overhead["syscr"])
        results.append(dict(name=path.name, restrict_drivers=args.restrict_drivers, file_bytes=path.stat().st_size,
                            features=len(dataset[0]), median_seconds=median(times),
                            seconds=times, read_bytes=reads, read_calls=calls))
    # Sparse files exercise rejection without allocating a large source buffer.
    for suffix in ["gml", "citygml"]:
        path = folder / f"oversized.{suffix}"
        with path.open("wb") as stream:
            stream.write(root.encode())
            stream.truncate(256 * 1024 * 1024 + 1)
        before, after = counters(), counters()
        overhead = {key: after[key] - before[key] for key in ("rchar", "syscr")}
        before = counters()
        start = time.perf_counter()
        try:
            terrio.open_vector(str(path))
        except terrio.errors.InvalidDataError as error:
            assert "256 MiB XML limit" in str(error), str(error)
        else:
            raise AssertionError("oversized XML was accepted")
        elapsed = time.perf_counter() - start
        after = counters()
        results.append(dict(name=path.name, restrict_drivers=args.restrict_drivers,
                            file_bytes=path.stat().st_size, rejected_before_parsing=True,
                            seconds=elapsed, read_bytes=after["rchar"] - before["rchar"] - overhead["rchar"],
                            read_calls=after["syscr"] - before["syscr"] - overhead["syscr"]))
    print(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
