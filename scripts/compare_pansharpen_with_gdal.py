#!/usr/bin/env python3
"""Compare weighted Brovey on real Landsat 8 pan/spectral pairs with GDAL.

Input bands are the public rio-pansharpen Landsat test fixtures downloaded by
fetch_test_data.sh, retained at unequal spatial resolutions. Outputs are compared
for pixels, type, nodata and georeferencing, including weights, spectral selections,
interpolation, bit depth, missing-data borders and unequal extents.
"""

import argparse
import json
import os
import shutil
import tempfile
from pathlib import Path

import numpy as np
from osgeo import gdal, osr

from compare_dem_calc_with_gdal import compare, format_row, run

gdal.UseExceptions()


def cases(data, work):
    """Prepare real-image windows and return GDAL/Terrio command pairs."""
    root = Path(data) / "pansharpen"
    scene = "LC81070352015122LGN00"
    pan = str(work / "pan.tif")
    gdal.Translate(pan, str(root / f"{scene}_B8.tif"), srcWin=[1400, 1000, 256, 240])
    spectral = []
    for band in (4, 3, 2):
        path = str(work / f"spectral{band}.tif")
        gdal.Translate(path, str(root / f"{scene}_B{band}.tif"), srcWin=[700, 500, 128, 120])
        spectral.append(path)
    result = []

    def add(name, p=pan, s=spectral, opts=(), tol=0):
        flags = {
            "-r": "--resampling",
            "-w": "--weight",
            "-b": "--band",
            "-bitdepth": "--bitdepth",
            "-nodata": "--nodata",
            "-spat_adjust": "--spat-adjust",
        }
        result.append(
            {
                "name": f"pansharpen {name}",
                "gdal": ["gdal_pansharpen", "-q", *opts, p, *s, "{out}"],
                "terrio": ["pansharpen", p, *s, "{out}", *[flags.get(v, v) for v in opts]],
                "tol": tol,
            }
        )

    add("default cubic")
    small_pan = str(work / "pan_window.tif")
    gdal.Translate(small_pan, pan, srcWin=[20, 30, 100, 90])
    add("pan extent only", p=small_pan, opts=["-spat_adjust", "none"])
    for method in ("nearest", "bilinear", "cubicspline", "lanczos", "average"):
        add(method, opts=["-r", method])
    add("weights", opts=["-w", "0.6", "-w", "0.3", "-w", "0.1"])
    add("negative weights", opts=["-w", "-0.5", "-w", "1", "-w", "0.5"])
    add("selected outputs", opts=["-b", "3", "-b", "1"])
    add("bitdepth12", opts=["-bitdepth", "12"])
    add("nodata0", opts=["-nodata", "0"])
    # Deliberately mask the real imagery's border and an interior patch.
    holes = []
    for i, path in enumerate(spectral):
        target = str(work / f"masked{i}.tif")
        ds = gdal.Translate(target, path)
        values = ds.ReadAsArray()
        values[:4] = 0
        values[30:40, 50:60] = 0
        ds.GetRasterBand(1).WriteArray(values)
        ds.GetRasterBand(1).SetNoDataValue(0)
        ds = None
        holes.append(target)
    masked_pan = str(work / "masked_pan.tif")
    ds = gdal.Translate(masked_pan, pan)
    values = ds.ReadAsArray()
    values[:, :8] = 0
    ds.GetRasterBand(1).WriteArray(values)
    ds.GetRasterBand(1).SetNoDataValue(0)
    ds = None
    add("masked borders", p=masked_pan, s=holes, opts=["-nodata", "0"])
    add("disabled nodata nearest", p=masked_pan, s=holes, opts=["-nodata", "none", "-r", "nearest"])
    # Internal dataset masks, without nodata, are ignored by GDAL's pansharpen path.
    explicit_masks = []
    for i, path in enumerate(spectral):
        target = str(work / f"explicit_mask{i}.tif")
        ds = gdal.Translate(target, path)
        ds.CreateMaskBand(gdal.GMF_PER_DATASET)
        mask = np.full((ds.RasterYSize, ds.RasterXSize), 255, dtype=np.uint8)
        mask[:4] = 0
        mask[30:40, 50:60] = 0
        ds.GetRasterBand(1).GetMaskBand().WriteArray(mask)
        ds = None
        explicit_masks.append(target)
    add("internal dataset masks", s=explicit_masks)
    # Shift all spectral footprints together, preserving their native pixel sizes.
    shifted = []
    for i, path in enumerate(spectral):
        target = str(work / f"shifted{i}.tif")
        ds = gdal.Translate(target, path)
        gt = list(ds.GetGeoTransform())
        gt[0] += 4 * gt[1]
        gt[3] -= 2 * abs(gt[5])
        ds.SetGeoTransform(gt)
        ds = None
        shifted.append(target)
    for extent in ("union", "intersection"):
        add(f"extent {extent}", s=shifted, opts=["-spat_adjust", extent])
    # Nonmatching extents drop nodata in GDAL's adjustment VRT unless explicitly overridden.
    shifted_holes = []
    for i, path in enumerate(holes):
        target = str(work / f"shifted_holes{i}.tif")
        ds = gdal.Translate(target, path)
        gt = list(ds.GetGeoTransform())
        gt[0] += 4 * gt[1]
        gt[3] -= 2 * abs(gt[5])
        ds.SetGeoTransform(gt)
        ds = None
        shifted_holes.append(target)
    add("unequal nodata auto", p=masked_pan, s=shifted_holes)
    add("unequal nodata explicit", p=masked_pan, s=shifted_holes, opts=["-nodata", "0"])
    # GDAL's one-pixel footprint tolerance permits negative fractional source origins.
    fractional = []
    for i, path in enumerate(spectral):
        target = str(work / f"fractional{i}.tif")
        ds = gdal.Translate(target, path)
        gt = list(ds.GetGeoTransform())
        gt[0] += 10
        ds.SetGeoTransform(gt)
        ds = None
        fractional.append(target)
    for method in ("nearest", "bilinear", "cubic", "cubicspline", "lanczos", "average"):
        add(f"fractional origin {method}", s=fractional, opts=["-r", method])
    # Integer arithmetic is constrained by pan type before final output conversion.
    byte_pan = str(work / "byte_pan.tif")
    ds = gdal.Translate(byte_pan, pan, outputType=gdal.GDT_Byte, scaleParams=[[0, 20000, 0, 255]])
    ds = None
    byte_spectral = []
    for i, path in enumerate(spectral):
        target = str(work / f"byte{i}.tif")
        ds = gdal.Translate(
            target, path, outputType=gdal.GDT_Byte, scaleParams=[[0, 20000, 0, 255]]
        )
        ds = None
        byte_spectral.append(target)
    add("byte clipping", p=byte_pan, s=byte_spectral, opts=["-w", "0.1", "-w", "0.1", "-w", "0.1"])
    float_pan = str(work / "float_pan.tif")
    gdal.Translate(float_pan, pan, outputType=gdal.GDT_Float32)
    float_spectral = []
    for i, path in enumerate(spectral):
        target = str(work / f"float{i}.tif")
        gdal.Translate(target, path, outputType=gdal.GDT_Float32)
        float_spectral.append(target)
    # One Float32 ULP at this scene's radiometric scale; integer cases remain exact.
    add("float32", p=float_pan, s=float_spectral, tol=0.002)
    large_pan = str(work / "large_pan.tif")
    gdal.Translate(large_pan, str(root / f"{scene}_B8.tif"), srcWin=[1400, 1000, 1024, 1040])
    large_spectral = []
    for band in (4, 3, 2):
        path = str(work / f"large_spectral{band}.tif")
        gdal.Translate(path, str(root / f"{scene}_B{band}.tif"), srcWin=[700, 500, 512, 520])
        large_spectral.append(path)
    for method in ("nearest", "bilinear", "cubic", "cubicspline", "lanczos", "average"):
        add(f"multiple blocks {method}", p=large_pan, s=large_spectral, opts=["-r", method])
    return result


def virtual_comparisons(case, terrio, work, index):
    """Cross-read GDAL/Terrio VRT descriptions as whole images and cropped requests."""
    gvrt, tvrt = (str(work / f"{prefix}_{index}.vrt") for prefix in ("gdal", "terrio"))
    run([v.replace("{out}", gvrt) for v in [*case["gdal"][:1], "-of", "VRT", *case["gdal"][1:]]])
    run([terrio, *[v.replace("{out}", tvrt) for v in case["terrio"]]])
    for label, path, window in (("GDAL VRT full", gvrt, None), ("Terrio VRT full", tvrt, None),
                                ("GDAL VRT window", gvrt, [17, 23, 53, 47]),
                                ("Terrio VRT window", tvrt, [17, 23, 53, 47])):
        gout = str(work / f"virtual_gdal_{index}.tif")
        tout = str(work / f"virtual_terrio_{index}.tif")
        for output in (gout, tout):
            if os.path.exists(output):
                os.remove(output)
        gdal.Translate(gout, path, srcWin=window)
        command = [terrio, "convert", path, tout]
        if window:
            command += ["--srcwin", *map(str, window)]
        run(command)
        result = compare(gout, tout, case["tol"])
        expected, actual = gdal.Open(gout), gdal.Open(tout)
        result["same_crs"] = bool(osr.SpatialReference(expected.GetProjection()).IsSame(osr.SpatialReference(actual.GetProjection())))
        result["name"] = f"{case['name']} {label}"
        yield result


def main():
    """Run and report all comparison cases."""
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("data")
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--only")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--keep")
    args = parser.parse_args()
    work = Path(args.keep or tempfile.mkdtemp(prefix="terrio_pansharpen_"))
    work.mkdir(parents=True, exist_ok=True)
    results = []
    for i, case in enumerate(cases(args.data, work)):
        if args.only and args.only not in case["name"]:
            continue
        gout, tout = str(work / f"gdal_{i}.tif"), str(work / f"terrio_{i}.tif")
        for output in (gout, tout):
            if os.path.exists(output):
                os.remove(output)
        try:
            run([v.replace("{out}", gout) for v in case["gdal"]])
            run([args.terrio, *[v.replace("{out}", tout) for v in case["terrio"]]])
            result = compare(gout, tout, case["tol"])
            expected, actual = gdal.Open(gout), gdal.Open(tout)
            result["same_crs"] = bool(
                osr.SpatialReference(expected.GetProjection()).IsSame(
                    osr.SpatialReference(actual.GetProjection())
                )
            )
        except Exception as exc:
            result = {"error": str(exc)}
        result["name"] = case["name"]
        results.append(result)
        if not args.json:
            print(format_row(result), flush=True)
        try:
            for virtual in virtual_comparisons(case, args.terrio, work, i):
                results.append(virtual)
                if not args.json:
                    print(format_row(virtual), flush=True)
        except Exception as exc:
            results.append({"name": case["name"] + " virtual", "error": str(exc)})
            if not args.json:
                print(format_row(results[-1]), flush=True)

    if args.json:
        print(json.dumps(results, indent=2))
    if not args.keep:
        shutil.rmtree(work)
    return int(
        any(
            "error" in r
            or not r.get("same_size")
            or not r.get("same_crs")
            or not r.get("same_type")
            or not r.get("same_nodata")
            or r.get("gt_diff_px", 1) > 1e-9
            or r.get("within_tol") != 1
            for r in results
        )
    )


if __name__ == "__main__":
    raise SystemExit(main())
