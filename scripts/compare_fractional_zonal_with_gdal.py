"""Compare fractional/weighted zonal statistics with GDAL 3.13 and exactextract.

Real USGS DEM and NLCD pixels, with generated fractional polygon boundaries.
Current Python bindings are required; Node/WASM is optional with --node-package.
"""

import argparse
import json
import math
from pathlib import Path
import subprocess
import tempfile

import numpy as np
from osgeo import gdal, ogr
from shapely import from_wkb
from shapely.affinity import affine_transform
from shapely.geometry import Polygon, MultiPolygon, box
from exactextract import exact_extract
from exactextract.raster import NumPyRasterSource

from compare_raster_analysis_with_gdal import rows

STATS = ["count", "min", "max", "mean", "sum", "stddev", "median",
         "quantile_25", "quantile_75", "mode", "minority", "variety", "variance", "cv"]
WEIGHTED = ["weighted_mean", "weighted_sum", "weighted_stddev"]
ORACLE_NAMES = {"stddev": "stdev", "weighted_stddev": "weighted_stdev", "cv": "coefficient_of_variation"}
# Statistics taken from exactextract alone (GDAL 3.13.3 rejects median; the others are compared
# with exactextract 0.3.0 on the same Float64 samples), and their exactextract requests.
EXACT_ONLY = {"median": "median", "quantile_25": "quantile(q=0.25)", "quantile_75": "quantile(q=0.75)",
              "mode": "mode", "minority": "minority", "variety": "variety", "variance": "variance",
              "cv": "coefficient_of_variation"}
# Values of the zone itself, compared exactly.
EXACT_VALUES = {"mode", "minority", "variety"}
INTEGER = {"variety"}
RTOL, ATOL = 2e-7, 1e-7

gdal.UseExceptions()
ogr.UseExceptions()


def run(args):
    result = subprocess.run(list(map(str, args)), capture_output=True, text=True)
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or result.stdout.strip())


def prepare(data, work, name):
    source = gdal.Open(str(data / name))
    value_path = work / (name + "-values.tif")
    w, h = min(43, source.RasterXSize), min(37, source.RasterYSize)
    raster = gdal.Translate(str(value_path), source,
                            srcWin=[(source.RasterXSize - w) // 2, (source.RasterYSize - h) // 2, w, h], bandList=[1])
    gt, crs = raster.GetGeoTransform(), raster.GetSpatialRef()
    values = raster.ReadAsArray().astype(float)
    nodata = raster.GetRasterBand(1).GetNoDataValue()
    valid = np.isfinite(values) & (values != nodata if nodata is not None else True)
    if not valid.any():
        raise AssertionError(f"{name}: selected real pixels contain no valid values")
    weight_values = np.where(valid, np.abs(values) + 1, -9999.)
    weights = {}
    for label in ["positive", "nodata"]:
        path = work / (name + f"-{label}-weights.tif")
        array = weight_values.copy()
        if label == "nodata":
            array[h // 2, w // 2] = -9999.
        ds = gdal.GetDriverByName("GTiff").Create(str(path), w, h, 1, gdal.GDT_Float64)
        ds.SetGeoTransform(gt)
        ds.SetSpatialRef(crs)
        ds.GetRasterBand(1).SetNoDataValue(-9999.)
        ds.GetRasterBand(1).WriteArray(array)
        ds.Close()
        weights[label] = path
    raster.Close()
    zones = work / (name + "-zones.gpkg")
    ds = ogr.GetDriverByName("GPKG").CreateDataSource(str(zones))
    layer = ds.CreateLayer("zones", crs, ogr.wkbUnknown)
    layer.CreateField(ogr.FieldDefn("zone", ogr.OFTInteger))
    layer.CreateField(ogr.FieldDefn("label", ogr.OFTString))
    shapes = [
        box(.2, .3, w - .2, h - .4),
        box(w + 1, h + 1, w + 2, h + 2),  # explicit empty-zone policy regression
        Polygon([(.5, .3), (w - 7.6, 2.2), (10.2, h - .1)]),
        Polygon(box(5.2, 4.4, w - 7.7, h - 5.3).exterior.coords,
                [box(15.1, 13.1, w - 17.6, h - 13.8).exterior.coords]),
        MultiPolygon([box(1.1, 1.1, 3.3, 4.2), box(w - 5.8, h - 6.9, w - 2.9, h - 2.5)]),
    ]
    for i, shape in enumerate(shapes):
        geom = affine_transform(shape, [gt[1], gt[2], gt[4], gt[5], gt[0], gt[3]])
        feature = ogr.Feature(layer.GetLayerDefn())
        feature.SetField("zone", i)
        feature.SetField("label", f"zone {i}")
        feature.SetGeometry(ogr.CreateGeometryFromWkb(geom.wkb))
        layer.CreateFeature(feature)
    ds = None
    return value_path, weights, zones


def expected(value, weights, zones, work, label, stats):
    names = [ORACLE_NAMES.get(s, s) for s in stats]
    output = work / (label + "-gdal.gpkg")
    args = ["gdal", "raster", "zonal-stats", value, output, "--zones", zones,
            "--pixels", "fractional", "--include-field", "zone", "--include-geom"]
    if weights:
        args += ["--weights", weights]
    # GDAL 3.13.3 advertises median but rejects it; exactextract supplies that oracle.
    for stat in stats:
        if stat not in EXACT_ONLY:
            args += ["--stat", ORACLE_NAMES.get(stat, stat)]
    run(args)
    reference = []
    for feature in rows(output):
        fields = {feature.GetFieldDefnRef(i).GetName(): feature.GetField(i) for i in range(feature.GetFieldCount())}
        for stat in stats:
            if stat not in EXACT_ONLY:
                index = feature.GetFieldIndex(ORACLE_NAMES.get(stat, stat))
                if feature.GetFieldDefnRef(index).GetType() != ogr.OFTReal:
                    raise AssertionError(f"GDAL {stat} is not a Real field")
        # Single input bands receive the statistic's name in GDAL's schema.
        reference.append({s: fields[ORACLE_NAMES.get(s, s)] for s in stats if s not in EXACT_ONLY})
    ds = gdal.Open(str(value))
    gt = ds.GetGeoTransform()
    # Exactextract casts quantiles to the source type. Use the same stored samples
    # as Float64 so the oracle retains Terrio's fractional median output contract.
    source = NumPyRasterSource(ds.ReadAsArray().astype(np.float64),
        xmin=gt[0], ymin=gt[3] + ds.RasterYSize * gt[5],
        xmax=gt[0] + ds.RasterXSize * gt[1], ymax=gt[3],
        nodata=ds.GetRasterBand(1).GetNoDataValue(), srs_wkt=ds.GetProjection())
    requests = [EXACT_ONLY.get(s, ORACLE_NAMES.get(s, s)) for s in stats] + ["values", "coverage"]
    exact = exact_extract(source, str(zones), requests, weights=str(weights) if weights else None)
    for got, ref in zip(exact, reference, strict=True):
        ref["candidates"] = candidates(got["properties"])
        for stat in stats:
            if stat in EXACT_ONLY:
                ref[stat] = got["properties"].get(ORACLE_NAMES.get(stat, stat))
            else:
                check_number(got["properties"].get(ORACLE_NAMES.get(stat, stat)), ref[stat], f"oracle {label} {stat}")
    return reference


def candidates(properties):
    """Mode and minority candidates: values whose total coverage is within the coverage
    tolerance of the winner's. Coverages differ between terrio (binary64) and exactextract
    (Float32) by up to about 1e-7, so near-ties on continuous data may resolve either way."""
    values, coverage = properties.get("values"), properties.get("coverage")
    if values is None or len(values) == 0:
        return {}
    totals = {}
    for v, c in zip(values, coverage):
        totals[float(v)] = totals.get(float(v), 0.0) + float(c)
    high, low = max(totals.values()), min(totals.values())
    near = lambda a, b: math.isclose(a, b, rel_tol=RTOL, abs_tol=ATOL)
    return {"mode": {v for v, t in totals.items() if near(t, high)},
            "minority": {v for v, t in totals.items() if near(t, low)}}


def missing(v):
    return v is None or isinstance(v, float) and not math.isfinite(v)


def check_number(actual, expected, label):
    if missing(expected):
        if not missing(actual):
            raise AssertionError(f"{label}: expected null, got {actual}")
        return 0.
    if missing(actual) or not isinstance(actual, (int, float)) or isinstance(actual, bool):
        raise AssertionError(f"{label}: expected a finite number, got {actual}")
    error = abs(actual - expected)
    if not math.isclose(actual, expected, rel_tol=RTOL, abs_tol=ATOL):
        raise AssertionError(f"{label}: {actual} != {expected}, error {error}")
    return error


def compare(path, zones, expected_rows, stats):
    source_ds, actual_ds = ogr.Open(str(zones)), ogr.Open(str(path))
    source, actual = source_ds.GetLayer(0), actual_ds.GetLayer(0)
    if not source.GetSpatialRef().IsSame(actual.GetSpatialRef()):
        raise AssertionError("source CRS changed")
    for stat in stats:
        field = actual.GetLayerDefn().GetFieldDefn(actual.GetLayerDefn().GetFieldIndex(f"v_b1_{stat}"))
        expected_type = ogr.OFTInteger64 if stat in INTEGER else ogr.OFTReal
        if field.GetType() not in (expected_type, ogr.OFTInteger if stat in INTEGER else ogr.OFTReal):
            raise AssertionError(f"fractional {stat} field has type {field.GetType()}")
    maximum = 0.
    normalized_empty_sums = False
    near_ties = 0
    for got, src, ref in zip(actual, source, expected_rows, strict=True):
        if got.GetFID() != src.GetFID():
            raise AssertionError("source feature IDs changed")
        if got["zone"] != src["zone"] or got["label"] != src["label"]:
            raise AssertionError("source attributes changed")
        if not from_wkb(bytes(got.GetGeometryRef().ExportToWkb())).equals_exact(from_wkb(bytes(src.GetGeometryRef().ExportToWkb())), 0):
            raise AssertionError("source geometry changed")
        for stat in stats:
            value = got[f"v_b1_{stat}"]
            if missing(ref[stat]) and value is not None:
                raise AssertionError("nonfinite reference statistics must be exposed as null")
            if stat in ("sum", "weighted_sum") and ref["count"] == 0 and value is None and ref[stat] == 0:
                normalized_empty_sums = True  # native null versus reference zero is documented
                continue
            if stat == "variety" and ref["count"] == 0 and value == 0 and missing(ref[stat]):
                continue  # native 0 distinct values for an empty zone
            if stat in EXACT_VALUES and not missing(ref[stat]):
                tied = ref["candidates"].get(stat, set())
                if value != ref[stat]:
                    if len(tied) > 1 and value in tied:
                        near_ties += 1  # a near-tie within the coverage tolerance
                        continue
                    raise AssertionError(f"{stat}: {value} != {ref[stat]}")
                continue
            maximum = max(maximum, check_number(value, ref[stat], stat))
    return maximum, normalized_empty_sums, near_ties


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--node-package", type=Path)
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    import terrio
    results = []
    with tempfile.TemporaryDirectory(prefix="terrio_fractional_") as directory:
        work = Path(directory)
        for name in ["usgs_dem.tif", "nlcd_knoxville.tif"]:
            value, weights, zones = prepare(args.data, work, name)
            for kind in ["coverage", "positive", "nodata"]:
                label = name + "-" + kind
                weight = None if kind == "coverage" else weights[kind]
                stats = STATS + (WEIGHTED if weight else [])
                try:
                    reference = expected(value, weight, zones, work, label, stats)
                except Exception as error:
                    results.append(dict(name=label + " reference", status="differs", detail=str(error)))
                    continue
                for api in ["CLI", "Python"] + (["Node/WASM"] if args.node_package else []):
                    try:
                        output = work / (label + "-" + api.split('/')[0] + ".gpkg")
                        if api == "CLI":
                            cmd = [args.terrio, "zonal-stats", zones, output, "--raster", value, "--prefix", "v", "--fractional", "--stats", ",".join(stats)]
                            if weight:
                                cmd += ["--weights", weight]
                            run(cmd)
                        elif api == "Python":
                            terrio.zonal_stats(str(zones), str(value), prefixes=["v"], stats=stats,
                                               fractional=True, weights=str(weight) if weight else None).write(str(output))
                        else:
                            run(["node", "-e", """
const fs = require('node:fs'), t = require(process.argv[1]);
const options = JSON.parse(process.argv[5]);
if (process.argv[6]) options.weights = fs.readFileSync(process.argv[6]);
const out = t.zonalStats({bytes:fs.readFileSync(process.argv[2]),filename:'zones.gpkg'}, fs.readFileSync(process.argv[3]), options);
fs.writeFileSync(process.argv[4], out.toBytes('result.gpkg'));
""", args.node_package.resolve(), zones, value, output,
                                 json.dumps(dict(prefixes=["v"], stats=stats, fractional=True)), str(weight) if weight else ""])
                        maximum, empty_sums, near_ties = compare(output, zones, reference, stats)
                        notes = ("documented empty-zone sums: native null versus reference zero; " if empty_sums else "")
                        notes += (f"{near_ties} mode/minority near-ties (coverages within tolerance) resolved to another tied value; " if near_ties else "")
                        results.append(dict(name=f"{label} {api}", status="tolerance" if maximum or empty_sums or near_ties else "identical", detail=notes + f"max absolute statistic error {maximum:.9g}; relative {RTOL}, absolute {ATOL}; mode/minority/variety exact otherwise; exact source geometry/attributes/CRS and Real fields"))
                    except Exception as error:
                        results.append(dict(name=f"{label} {api}", status="differs", detail=str(error)))
    print(json.dumps(results, indent=2) if args.json else "\n".join(str(r) for r in results))
    return int(any(r["status"] == "differs" for r in results))


if __name__ == "__main__":
    raise SystemExit(main())
