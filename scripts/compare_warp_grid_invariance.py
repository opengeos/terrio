#!/usr/bin/env python3
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Compare real terrain whole-grid warps with independently requested tiles.

GDAL 3.13.3 prepares the source crop; equality is a Terrio grid consistency
contract, not a claim of equality to GDAL's chunk-dependent sampling.
"""

import argparse
import json
import os
import tempfile
from pathlib import Path

import numpy as np
from affine import Affine
from osgeo import gdal

import terrio
import terrio.rio as rio
from terrio.rio.enums import Resampling
from terrio.rio.vrt import WarpedVRT
from terrio.rio.windows import Window

METHODS = {
    "near": Resampling.nearest, "bilinear": Resampling.bilinear,
    "cubic": Resampling.cubic, "cubicspline": Resampling.cubic_spline,
    "lanczos": Resampling.lanczos, "average": Resampling.average,
    "rms": Resampling.rms, "mode": Resampling.mode, "min": Resampling.min,
    "max": Resampling.max, "med": Resampling.med, "q1": Resampling.q1,
    "q3": Resampling.q3, "sum": Resampling.sum,
}


def check(actual, expected, record):
    """Accumulate exact validity and absolute value comparisons."""
    record["same_size"] = record.get("same_size", True) and actual.shape == expected.shape
    assert record["same_size"], (actual.shape, expected.shape)
    valid = ~np.isnan(expected)
    mismatch = np.count_nonzero(valid != ~np.isnan(actual))
    record["valid_mismatch"] += int(mismatch)
    both_valid = valid & ~np.isnan(actual)
    differences = np.abs(actual[both_valid] - expected[both_valid])
    assert np.isfinite(differences).all(), "non-finite value difference"
    record["max_diff"] = max(record["max_diff"], float(differences.max(initial=0)))
    record["compared"] += int(expected.size)
    record["differing"] += int(np.count_nonzero(differences)) + int(mismatch)
    record["outside_tol"] += int(np.count_nonzero(differences > 1e-7)) + int(mismatch)
    record["exact"] = 1.0 - record["differing"] / record["compared"]
    record["within_tol"] = 1.0 - record["outside_tol"] / record["compared"]
    assert mismatch == 0, "validity differs"
    assert record["max_diff"] <= 1e-7, record["max_diff"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", help="Accepted for conformance runner compatibility")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    gdal.UseExceptions()
    os.environ.pop("TERRIO_WARP_GRID", None)
    records = []
    with tempfile.TemporaryDirectory(prefix="terrio-warp-grid-") as directory:
        source = Path(directory) / "terrain.tif"
        crop = gdal.Translate(str(source), str(args.data / "usgs_dem.tif"),
                              srcWin=[1000, 1000, 256, 256], outputType=gdal.GDT_Float64)
        # Add an interior hole without changing the real elevations around it.
        band = crop.GetRasterBand(1)
        values = band.ReadAsArray()
        values[40:180, 70:130] = -9999
        band.SetNoDataValue(-9999)
        band.WriteArray(values)
        band = crop = None
        raster = terrio.open_raster(str(source))
        for version in ("3.12", "3.13"):
            os.environ["TERRIO_GDAL_COMPAT"] = version
            assert terrio.gdal_compat() == version
            for threshold in (0.0, 0.125):
                for method, resampling in METHODS.items():
                    record = dict(name=f"terrain {method} {version} et={threshold}",
                                  reference_gdal=gdal.VersionInfo("RELEASE_NAME"),
                                  valid_mismatch=0, max_diff=0., differing=0,
                                  compared=0, outside_tol=0)
                    try:
                        options = dict(t_srs="EPSG:3857", tr=(38., 38.), ot="Float64",
                                       dstnodata=float("nan"), resampling=method,
                                       kernel_version=version, et=threshold)
                        whole = terrio.warp(raster, **options)
                        expected = whole.read(1)
                        gt = whole.transform
                        height, width = expected.shape
                        with rio.open(source) as src, WarpedVRT(
                            src, crs="EPSG:3857", transform=Affine.from_gdal(*gt),
                            width=width, height=height, nodata=float("nan"),
                            resampling=resampling, tolerance=threshold,
                        ) as vrt:
                            vrt_whole = vrt.read(1)
                            xs, ys = (0, width // 3, width), (0, height // 3, height)
                            for y0, y1 in zip(ys[:-1], ys[1:], strict=True):
                                for x0, x1 in zip(xs[:-1], xs[1:], strict=True):
                                    extent = (gt[0] + x0 * gt[1], gt[3] + y1 * gt[5],
                                              gt[0] + x1 * gt[1], gt[3] + y0 * gt[5])
                                    tile = terrio.warp(raster, te=extent, **options).read(1)
                                    check(tile, expected[y0:y1, x0:x1], record)
                                    tile = vrt.read(1, window=Window(x0, y0, x1-x0, y1-y0))
                                    check(tile, vrt_whole[y0:y1, x0:x1], record)
                    except Exception as error:
                        record["error"] = str(error)
                    records.append(record)
    if args.json:
        print(json.dumps(records, indent=2))
    else:
        for record in records:
            print(f"{record['name']}: {record.get('error', record['max_diff'])}")
    return int(any("error" in record for record in records))


if __name__ == "__main__":
    raise SystemExit(main())
