#!/usr/bin/env python3
"""Compare versioned warp kernels and PixelIsPoint preservation on real USGS terrain."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile

from osgeo import gdal, osr

from compare_ops_with_gdal import compare, format_row

gdal.UseExceptions()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data")
    parser.add_argument("--terrio", default="target/debug/terrio")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    version = "3.13" if int(gdal.VersionInfo("VERSION_NUM")) >= 3130000 else "3.12"
    records = []
    with tempfile.TemporaryDirectory(prefix="terrio-warp-policy-") as directory:
        tmp = Path(directory)
        source = tmp / "point.tif"
        ds = gdal.Translate(str(source), str(Path(args.data) / "usgs_dem.tif"), srcWin=[0, 0, 180, 120], outputType=gdal.GDT_Float32)
        ds.SetMetadataItem("AREA_OR_POINT", "Point")
        ds = None
        env = {**os.environ, "TERRIO_NETWORK": "OFF", "TERRIO_GRID_CACHE": str(tmp / "grids")}

        def run(name, reference, command, tolerance):
            actual = tmp / "actual.tif"
            try:
                process = subprocess.run([args.terrio, *map(str, command)], capture_output=True, text=True, env=env)
                if process.returncode:
                    raise RuntimeError(process.stderr)
                record = compare(str(reference), str(actual), tolerance)
                if not all(record.get(k) for k in ("same_size", "same_type", "same_crs")) or record["gt_diff_px"] > 1e-7:
                    raise AssertionError(f"grid/type/CRS mismatch: {record}")
                ref_ds, actual_ds = gdal.Open(str(reference)), gdal.Open(str(actual))
                record["same_nodata"] = ref_ds.GetRasterBand(1).GetNoDataValue() == actual_ds.GetRasterBand(1).GetNoDataValue()
                expected = gdal.Open(str(reference)).GetMetadataItem("AREA_OR_POINT")
                observed = gdal.Open(str(actual)).GetMetadataItem("AREA_OR_POINT")
                record["same_metadata"] = record.get("same_metadata", True) and expected == observed == "Point"
            except Exception as error:
                record = {"error": str(error)}
            record["name"] = name
            records.append(record)

        for driver in ["GTiff", "COG"]:
            reference = tmp / "clip.tif"
            gdal.Translate(str(reference), str(source), srcWin=[7, 9, 60, 50], format=driver)
            run(f"point translate {driver}", reference,
                ["translate", source, tmp / "actual.tif", "--srcwin", 7, 9, 60, 50, "-f", driver], 0)
        for resolution in [10, 25, 60]:
            for method in ["near", "bilinear", "cubic", "cubicspline", "lanczos"]:
                reference = tmp / "warp.tif"
                result = gdal.Warp(str(reference), str(source), dstSRS="EPSG:32613", xRes=resolution, yRes=resolution,
                                   resampleAlg=method, errorThreshold=0, overviewLevel="NONE")
                gt = result.GetGeoTransform()
                width, height = result.RasterXSize, result.RasterYSize
                bounds = [gt[0], gt[3] + height * gt[5], gt[0] + width * gt[1], gt[3]]
                result = None
                run(f"kernel {version} {method} {resolution}m", reference,
                    ["warp", source, tmp / "actual.tif", "--t-srs", "EPSG:32613", "--te", *bounds,
                     "--ts", width, height, "-r", method, "--et", 0, "--overview", "NONE", "--kernel-version", version], 0 if method == "near" else 0.002)
        # A rotated target square cannot be represented accurately by source-window ratios.
        for method in ["bilinear", "cubic", "lanczos"]:
            reference = tmp / "rotated.tif"
            src = gdal.Open(str(source))
            gt = src.GetGeoTransform()
            target_gt = [gt[0] + 20.25 * gt[1], gt[1] * 2, gt[1], gt[3] + 10.125 * gt[5], -gt[5], gt[5] * 2]
            dst = gdal.GetDriverByName("GTiff").Create(str(reference), 40, 30, 1, gdal.GDT_Float32)
            dst.SetGeoTransform(target_gt)
            dst.SetProjection(src.GetProjection())
            dst.SetMetadataItem("AREA_OR_POINT", "Point")
            dst.GetRasterBand(1).SetNoDataValue(src.GetRasterBand(1).GetNoDataValue())
            gdal.Warp(dst, src, resampleAlg=method, errorThreshold=0, overviewLevel="NONE")
            dst, src = None, None
            run(f"kernel {version} rotated {method}", reference,
                ["warp", source, tmp / "actual.tif", "--dst-transform", *target_gt, "--ts", 40, 30,
                 "-r", method, "--et", 0, "--overview", "NONE", "--kernel-version", version], 0.002)
        # Strict policies must leave ordinary projection-domain failures as invalid pixels.
        domain_source = tmp / "domain-source.tif"
        dataset = gdal.GetDriverByName("GTiff").Create(str(domain_source), 36, 36, 1, gdal.GDT_Float32)
        srs = osr.SpatialReference()
        srs.ImportFromEPSG(3857)
        dataset.SetProjection(srs.ExportToWkt())
        dataset.SetMetadataItem("AREA_OR_POINT", "Point")
        dataset.SetGeoTransform([-20e6, 40e6 / 36, 0, 20e6, 0, -40e6 / 36])
        dataset.GetRasterBand(1).Fill(7)
        dataset = None
        reference = tmp / "domain-reference.tif"
        gdal.PushErrorHandler("CPLQuietErrorHandler")
        try:
            dataset = gdal.Warp(str(reference), str(domain_source), dstSRS="EPSG:4326",
                                outputBounds=[-10, 80, 10, 100], width=32, height=32,
                                dstNodata=-9999, errorThreshold=0,
                                transformerOptions=["ONLY_BEST=YES", "ALLOW_BALLPARK=NO"])
            dataset = None
        finally:
            gdal.PopErrorHandler()
        run("strict partial projection domain", reference,
            ["warp", domain_source, tmp / "actual.tif", "--t-srs", "EPSG:4326",
             "--te", -10, 80, 10, 100, "--ts", 32, 32, "--dstnodata", -9999,
             "--et", 0, "--to", "ONLY_BEST=YES", "--to", "ALLOW_BALLPARK=NO"], 0)
    print(json.dumps(records, indent=2) if args.json else "\n".join(format_row(r) for r in records))
    return int(any("error" in r or not r.get("same_metadata", False) or not r.get("same_nodata", False) or r.get("within_tol", 0) < 1 or r.get("valid_mismatch", 1) for r in records))


if __name__ == "__main__":
    raise SystemExit(main())
