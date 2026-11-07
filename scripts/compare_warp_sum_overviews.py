#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Check overview sum estimates and exact NONE sums against GDAL 3.13.3.

The estimator deliberately differs from GDAL's unscaled overview sum. Its reference is
GDAL's sum multiplied by the exact source/overview pixel-area ratio, only at valid pixels.
NONE uses GDAL's full-resolution result directly. Includes a real USGS DEM crop.
Each fixture has one overview and a coarse target, so AUTO checks level-0 selection;
it does not exercise selection among multiple levels.
"""

import argparse
import json
import subprocess
import tempfile
from pathlib import Path

import numpy as np
from osgeo import gdal, osr

from compare_dem_calc_with_gdal import compare

gdal.UseExceptions()


def fixtures(root, data):
    """Real TIFFs with known overview means and a real terrain subset."""
    contrast = np.zeros((4, 4), dtype="float64")
    contrast[1, 1] = 100
    cases = [
        ("float", np.arange(16, dtype="float64").reshape(4, 4), None),
        ("oblong", np.arange(45, dtype="float32").reshape(5, 9), None),
        ("crop", contrast, None),
        ("partial-nodata", np.array([[100, -9999], [-9999, -9999]], dtype="float64"), -9999),
        ("all-nodata", np.full((2, 2), -9999, dtype="float64"), -9999),
        ("integer", np.array([[0, 0], [0, 2]], dtype="uint16"), None),
    ]
    srs = osr.SpatialReference()
    srs.ImportFromEPSG(3857)
    for name, values, nodata in cases:
        path = root / f"{name}.tif"
        dtype = {"float64": gdal.GDT_Float64, "float32": gdal.GDT_Float32, "uint16": gdal.GDT_UInt16}[str(values.dtype)]
        ds = gdal.GetDriverByName("GTiff").Create(str(path), values.shape[1], values.shape[0], 1, dtype)
        ds.SetProjection(srs.ExportToWkt())
        ds.SetGeoTransform((0, 1, 0, values.shape[0], 0, -1))
        ds.GetRasterBand(1).WriteArray(values)
        if nodata is not None:
            ds.GetRasterBand(1).SetNoDataValue(nodata)
        ds.BuildOverviews("AVERAGE", [2])
        ds = None
        extent = (1, 1, 3, 3) if name == "crop" else (0, 0, values.shape[1], values.shape[0])
        size = (3, 2) if values.size > 16 else (1, 1)
        yield name, path, extent, size, 1e-8
    path = root / "terrain.tif"
    ds = gdal.Translate(str(path), str(Path(data) / "usgs_dem.tif"), srcWin=[100, 100, 64, 64], outputType=gdal.GDT_Float64)
    ds.BuildOverviews("AVERAGE", [2])
    gt = ds.GetGeoTransform()
    extent = (gt[0], gt[3] + 64 * gt[5], gt[0] + 64 * gt[1], gt[3])
    ds = None
    yield "USGS", path, extent, (16, 16), 1e-4


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data")
    parser.add_argument("--terrio", default="target/debug/terrio")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    records = []
    with tempfile.TemporaryDirectory(prefix="terrio-sum-overviews-") as directory:
        root = Path(directory)
        for name, source, extent, size, tol in fixtures(root, args.data):
            for mode in ("NONE", "0", "AUTO"):
                reference, actual = root/f"{name}-{mode}-gdal.tif", root/f"{name}-{mode}-terrio.tif"
                record = {"name": f"sum overview {name} {mode}", "tol": tol, "gdal_version": gdal.VersionInfo("RELEASE_NAME")}
                try:
                    ds = gdal.Warp(str(reference), str(source), format="GTiff", outputBounds=extent, width=size[0], height=size[1],
                                   resampleAlg="sum", outputType=gdal.GDT_Float64, overviewLevel=0 if mode == "0" else mode)
                    if mode != "NONE":
                        src = gdal.Open(str(source))
                        # Every fixture has one available level, and AUTO targets are coarse
                        # enough to choose it. GDAL still receives the real AUTO selector.
                        assert src.GetRasterBand(1).GetOverviewCount() == 1
                        overview = src.GetRasterBand(1).GetOverview(0)
                        scale = (src.RasterXSize/overview.XSize)*(src.RasterYSize/overview.YSize)
                        band = ds.GetRasterBand(1)
                        values, valid = band.ReadAsArray(), band.GetMaskBand().ReadAsArray() != 0
                        values[valid] *= scale
                        band.WriteArray(values)
                        record["reference_scale"] = scale
                    ds = None
                    command = [args.terrio, "warp", str(source), str(actual), "-r", "sum", "--overview", mode, "--ot", "Float64",
                               "--te", *map(str, extent), "--ts", *map(str, size)]
                    result = subprocess.run(command, capture_output=True, text=True)
                    if result.returncode:
                        raise RuntimeError(result.stderr.strip())
                    record.update(compare(str(reference), str(actual), tol))
                    record["same_georeferencing"] = record["gt_diff_px"] <= 1e-8
                    expected, got = gdal.Open(str(reference)), gdal.Open(str(actual))
                    if not expected.GetSpatialRef().IsSame(got.GetSpatialRef()):
                        raise AssertionError("CRS differs")
                    if not np.array_equal(expected.GetRasterBand(1).GetMaskBand().ReadAsArray(), got.GetRasterBand(1).GetMaskBand().ReadAsArray()):
                        raise AssertionError("mask differs")
                except Exception as error:
                    record["error"] = str(error)
                records.append(record)
                ds = expected = got = src = overview = band = None
    passed = ["error" not in r and r.get("same_size") and r.get("same_type") and r.get("same_nodata")
              and r.get("gt_diff_px", 1) <= 1e-8 and r.get("within_tol", 0) == 1 for r in records]
    if args.json:
        print(json.dumps(records, indent=2))
    else:
        for record, ok in zip(records, passed, strict=True):
            detail = record.get("error", f"max abs difference {record.get('max_diff')}; tolerance {record['tol']}")
            print(f"{'PASS' if ok else 'FAIL'} {record['name']}: {detail}")
    return int(not all(passed))


if __name__ == "__main__":
    raise SystemExit(main())
