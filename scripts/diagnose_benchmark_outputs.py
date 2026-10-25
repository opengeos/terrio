#!/usr/bin/env python3
"""Measure full output differences without changing benchmark equivalence gates.

Usage: python scripts/diagnose_benchmark_outputs.py A B [--key FIPS] [--output report.json]
Reads rasters in 128-row strips. Vector pairing requires a unique attribute key;
exact order-independent comparison always uses benchmark_validation's comparator.
"""

import argparse
import json
from collections import Counter
from pathlib import Path

import numpy as np
from osgeo import gdal, ogr

from benchmark_validation import compare_rasters, compare_vectors, same_crs

gdal.UseExceptions()
ogr.UseExceptions()


def max_ulp(a, b):
    """Distance between finite, same-precision IEEE samples (signed zeros equal)."""
    if a.dtype not in (np.dtype("float32"), np.dtype("float64")) or a.dtype != b.dtype:
        return None
    unsigned = np.uint32 if a.dtype.itemsize == 4 else np.uint64
    sign = unsigned(1 << (8 * a.dtype.itemsize - 1))

    def ordered(x):
        bits = x.view(unsigned)
        magnitude = bits & (sign - unsigned(1))
        return np.where(bits & sign != 0, sign - magnitude, sign + magnitude)

    x, y = ordered(a), ordered(b)
    # Avoid unsigned wraparound when the first value is smaller.
    return int((np.maximum(x, y) - np.minimum(x, y)).max(initial=0))


def band_metadata(band):
    """Properties checked separately from the pixel error measurements."""
    return {
        "size": [band.XSize, band.YSize],
        "type": gdal.GetDataTypeName(band.DataType),
        "nodata": band.GetNoDataValue(),
        "color": gdal.GetColorInterpretationName(band.GetColorInterpretation()),
        "scale": band.GetScale(),
        "offset": band.GetOffset(),
        "unit": band.GetUnitType(),
    }


def raster_differences(a, b):
    """Count every difference, retaining separate valid-data and mask measurements."""
    result = {
        "exact_gate": compare_rasters(a, b),
        "crs_equal": same_crs(a.GetSpatialRef(), b.GetSpatialRef()),
        "geotransforms": [a.GetGeoTransform(), b.GetGeoTransform()],
        "gcp_counts": [a.GetGCPCount(), b.GetGCPCount()],
        "bands": [],
    }
    for i in range(min(a.RasterCount, b.RasterCount)):
        x, y = [ds.GetRasterBand(i + 1) for ds in (a, b)]
        record = {
            "band": i + 1,
            "metadata": [band_metadata(x), band_metadata(y)],
            "overview_counts": [x.GetOverviewCount(), y.GetOverviewCount()],
            "levels": [],
        }
        for level in range(-1, min(x.GetOverviewCount(), y.GetOverviewCount())):
            u, v = (
                (x, y) if level == -1 else (x.GetOverview(level), y.GetOverview(level))
            )
            stats = {"level": level, "sizes": [[z.XSize, z.YSize] for z in (u, v)]}
            record["levels"].append(stats)
            if stats["sizes"][0] != stats["sizes"][1]:
                continue
            stats.update(
                pixels=u.XSize * u.YSize,
                differing_pixels=0,
                common_valid=0,
                differing_valid=0,
                mask_differences=0,
                nonfinite_differences=0,
                max_absolute=0.0,
                max_relative=0.0,
                max_ulp=None,
            )
            for row in range(0, u.YSize, 128):
                h = min(128, u.YSize - row)
                p, q = [z.ReadAsArray(0, row, z.XSize, h) for z in (u, v)]
                m, n = [
                    z.GetMaskBand().ReadAsArray(0, row, z.XSize, h) != 0 for z in (u, v)
                ]
                equal = (p == q) | (np.isnan(p) & np.isnan(q))
                finite = np.isfinite(p) & np.isfinite(q)
                valid = m & n & finite
                stats["differing_pixels"] += int(np.count_nonzero(~equal))
                stats["common_valid"] += int(np.count_nonzero(valid))
                stats["differing_valid"] += int(np.count_nonzero(valid & ~equal))
                stats["mask_differences"] += int(np.count_nonzero(m != n))
                stats["nonfinite_differences"] += int(
                    np.count_nonzero(~finite & ~equal)
                )
                pv, qv = p[valid], q[valid]
                if pv.dtype.kind in "iu" and pv.dtype.itemsize == 8:
                    # Python integers preserve unit differences even where longdouble is Float64.
                    delta = np.asarray(
                        np.abs(pv.astype(object) - qv.astype(object)),
                        dtype=np.longdouble,
                    )
                else:
                    delta = np.abs(pv.astype(np.longdouble) - qv.astype(np.longdouble))
                relative = delta / np.maximum(
                    np.abs(qv.astype(np.longdouble)), np.finfo(np.longdouble).tiny
                )
                stats["max_absolute"] = max(
                    stats["max_absolute"], float(delta.max(initial=0))
                )
                stats["max_relative"] = max(
                    stats["max_relative"], float(relative.max(initial=0))
                )
                ulp = max_ulp(pv, qv)
                if ulp is not None:
                    stats["max_ulp"] = max(stats["max_ulp"] or 0, ulp)
        result["bands"].append(record)
    return result


def schema(layer):
    definition = layer.GetLayerDefn()
    return [
        (f.GetName(), ogr.GetFieldTypeName(f.GetType()), f.GetSubType())
        for f in (definition.GetFieldDefn(i) for i in range(definition.GetFieldCount()))
    ]


def coordinates(geometry):
    if geometry.GetGeometryCount():
        return [
            p
            for i in range(geometry.GetGeometryCount())
            for p in coordinates(geometry.GetGeometryRef(i))
        ]
    return geometry.GetPoints()


def vector_differences(a, b, key=None):
    """Report exact schemas and pair features by an explicitly chosen unique key."""
    result = {"exact_gate": compare_vectors(a, b), "layers": []}
    for i in range(min(a.GetLayerCount(), b.GetLayerCount())):
        layers = [d.GetLayer(i) for d in (a, b)]
        record = {
            "schemas": [schema(l) for l in layers],
            "counts": [l.GetFeatureCount() for l in layers],
        }
        result["layers"].append(record)
        if key is None:
            continue
        features = []
        for layer in layers:
            layer.ResetReading()
            rows = {}
            for feature in layer:
                value = feature[key]
                if value is None or value in rows:
                    raise ValueError(f"{key} must be unique and non-null")
                rows[value] = feature
            features.append(rows)
        record["unmatched_keys"] = [
            sorted(set(x) - set(y)) for x, y in (features, features[::-1])
        ]
        attributes = Counter()
        geometry_count, max_error, structure_count = 0, 0.0, 0
        for value in features[0].keys() & features[1].keys():
            x, y = [f[value] for f in features]
            xa, ya = x.items(), y.items()
            for field in xa.keys() | ya.keys():
                if xa.get(field) != ya.get(field):
                    attributes[field] += 1
            gx, gy = [f.GetGeometryRef() for f in (x, y)]
            if gx is None or gy is None:
                structure_count += int((gx is None) != (gy is None))
                continue
            gx, gy = gx.Normalize(), gy.Normalize()
            if gx.ExportToWkb() != gy.ExportToWkb():
                geometry_count += 1
                px, py = [np.asarray(coordinates(g)) for g in (gx, gy)]
                if px.shape != py.shape or gx.GetGeometryType() != gy.GetGeometryType():
                    structure_count += 1
                elif px.size:
                    max_error = max(max_error, float(np.abs(px - py).max()))
        record.update(
            attribute_differences=dict(attributes),
            differing_geometries=geometry_count,
            structure_differences=structure_count,
            max_coordinate_absolute=max_error,
        )
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("a")
    parser.add_argument("b")
    parser.add_argument("--key", help="Unique vector attribute for feature pairing")
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    datasets = [gdal.OpenEx(path) for path in (args.a, args.b)]
    a, b = datasets
    if a.RasterCount and b.RasterCount:
        result = raster_differences(a, b)
    else:
        result = vector_differences(a, b, args.key)
    result["gdal_version"] = gdal.VersionInfo("RELEASE_NAME")
    text = json.dumps(result, indent=2, allow_nan=True) + "\n"
    if args.output:
        args.output.write_text(text)
    else:
        print(text, end="")


if __name__ == "__main__":
    main()
