#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Deleted Shapefile rows and live-feature pages against an independent OGR read.

Generate small real SHP/DBF files with OGR, then mark DBF slots without repacking.
Compare physical FIDs, exact scalar attributes/nulls and ISO WKB bytes. A live row
with null geometry still counts toward paging. No downloaded fixtures are needed.
Build the bridge with cargo build -p terrio --example shapefile_rows.
"""

import argparse
import json
import subprocess
import tempfile
from pathlib import Path

from osgeo import gdal, ogr

DELETIONS = ((0, 2, 3, 5), tuple(range(6)), (), (1,), tuple(range(5)))
PAGES = ((0, None), (0, 0), (0, 2), (1, 4), (2, 2), (5, 1), (6, 2), (1, None))


def fixture(path):
    """Write six independent OGR records, including null geometry and fields."""
    dataset = ogr.GetDriverByName("ESRI Shapefile").CreateDataSource(str(path))
    layer = dataset.CreateLayer(path.stem, geom_type=ogr.wkbPoint)
    integer = ogr.FieldDefn("id", ogr.OFTInteger64)
    integer.SetWidth(18)
    layer.CreateField(integer)
    layer.CreateField(ogr.FieldDefn("label", ogr.OFTString))
    for i in range(6):
        feature = ogr.Feature(layer.GetLayerDefn())
        if i != 3:
            feature.SetField("id", 9007199254740993 + i)
        if i != 2:
            feature.SetField("label", f"row{i}")
        if i != 4:
            feature.SetGeometry(ogr.CreateGeometryFromWkt(f"POINT ({i} 0)"))
        layer.CreateFeature(feature)
    dataset = None
    return path.with_suffix(".dbf").read_bytes()


def reference(path, skip, maximum):
    """Slice OGR's sequential live rows without assuming physical seek semantics."""
    dataset = ogr.Open(str(path))
    layer = dataset.GetLayer(0)
    # GDAL 3.13.3's Shapefile fast SetNextByIndex counts physical slots when rows
    # are deleted. Sequential GetNextFeature is the live-feature result set.
    for _ in range(skip):
        if layer.GetNextFeature() is None:
            return []
    rows = []
    while maximum is None or len(rows) < maximum:
        feature = layer.GetNextFeature()
        if feature is None:
            break
        geometry = feature.GetGeometryRef()
        rows.append({"fid": feature.GetFID(), "wkb": bytes(geometry.ExportToIsoWkb()).hex() if geometry else None,
                     "attributes": [feature.GetField(i) for i in range(feature.GetFieldCount())]})
    return rows


def compare_case(probe, path, skip, maximum, expected, name, stream=False):
    """Record a failed process or malformed output as one differing case, then continue."""
    command = [str(probe), str(path), str(skip), "-" if maximum is None else str(maximum)]
    if stream:
        command.append("--stream")
    try:
        process = subprocess.run(command, capture_output=True, text=True, check=False, timeout=60)
        if process.returncode:
            raise ValueError(f"probe exit {process.returncode}: {process.stderr[-1000:]}")
        actual = json.loads(process.stdout)
    except (OSError, ValueError, subprocess.TimeoutExpired) as error:
        return {"name": name, "status": "differs", "detail": str(error)}
    return {"name": name, "status": "identical" if actual == expected else "differs",
            "detail": f"GDAL {gdal.__version__}; {len(expected)} live features; "
            + ("FIDs, attributes/nulls and WKB exact" if actual == expected else f"expected {expected!r}, got {actual!r}")}


def compare(probe, directory):
    """Exercise full reads and paging, indexed and header-scan SHP access."""
    path = directory / "deleted.shp"
    original = fixture(path)
    header = int.from_bytes(original[8:10], "little")
    width = int.from_bytes(original[10:12], "little")
    results = []
    references = {}
    for indexed in (True, False):
        if not indexed:
            path.with_suffix(".shx").unlink()
        for deleted in DELETIONS:
            dbf = bytearray(original)
            for slot in deleted:
                dbf[header + slot * width] = ord("*")
            path.with_suffix(".dbf").write_bytes(dbf)
            for skip, maximum in PAGES:
                key = (deleted, skip, maximum)
                if indexed:
                    references[key] = reference(path, skip, maximum)
                # Reuse the OGR result before SHX removal: OGR requires that index.
                expected = references[key]
                name = f"indexed={indexed} deleted={deleted} skip={skip} max={maximum}"
                results.append(compare_case(probe, path, skip, maximum, expected, name))
            name = f"stream indexed={indexed} deleted={deleted}"
            results.append(compare_case(probe, path, 0, None, references[(deleted, 0, None)], name, stream=True))
    return results


def main():
    """Print the same case records that scripts/conformance.py registers."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--probe", type=Path, required=True)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    gdal.UseExceptions()
    ogr.DontUseExceptions()
    with tempfile.TemporaryDirectory(prefix="shapefile-deleted-") as directory:
        results = compare(args.probe.resolve(), Path(directory))
    print(json.dumps(results, indent=2) if args.json else "\n".join(f"{r['name']}: {r['status']}" for r in results))
    return int(any(result["status"] == "differs" for result in results))


if __name__ == "__main__":
    raise SystemExit(main())
