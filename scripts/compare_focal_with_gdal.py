#!/usr/bin/env python3
"""Compare focal statistics, collar cleaning and LUT enhancement with GDAL.

Terrio's `neighbors`, `nearblack`, `clean-collar` and `enhance` against GDAL's
`gdal raster neighbors`, `nearblack`, `gdal raster clean-collar` and `gdalenhance` on
real imagery and terrain from fetch_test_data.sh: the Sentinel-2 L2A true-color image
(its orbit-edge black collar), the Sentinel-2 L1C red band, the Landsat 7 scene (its
rotated black collar) and the USGS 3DEP DEM. Every pixel, the type, nodata, the
geotransform and per-dataset masks are compared; LUT files are compared as text.
"""

import argparse
import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path

import numpy as np
from osgeo import gdal

from compare_dem_calc_with_gdal import compare, format_row, run

gdal.UseExceptions()


def prepare(data, work):
    """Writes the input rasters of the cases as GeoTIFFs.

    Args:
        data: The test data directory.
        work: The working directory.

    Returns:
        dict: Input paths by name.
    """
    d = Path(data)
    paths = {}

    def tif(name, src, **kw):
        path = str(work / f"{name}.tif")
        gdal.Translate(path, str(d / src), **kw)
        paths[name] = path

    tif("tci", "s2_l2a_TCI_60m.jp2")
    tif("red", "s2_l1c_B04.jp2", srcWin=[3000, 3000, 1024, 768])
    tif("dem", "usgs_dem.tif", srcWin=[1000, 1000, 1024, 768])
    # The whole DEM (3612x3612): several strips of rows, with the kernel's margins.
    paths["dem_full"] = str(d / "usgs_dem.tif")
    tif("landsat7", "landsat7.tif")
    # The DEM with deliberate nodata holes, for the nodata handling of kernels.
    holes = str(work / "dem_holes.tif")
    ds = gdal.Translate(holes, paths["dem"])
    values = ds.GetRasterBand(1).ReadAsArray()
    nodata = ds.GetRasterBand(1).GetNoDataValue()
    values[100:140, 200:260] = nodata
    values[500:503, :] = nodata
    values[:, 700] = nodata
    ds.GetRasterBand(1).WriteArray(values)
    ds = None
    paths["dem_holes"] = holes
    # NaNs without a nodata tag must remain NaN in an all-NaN mode window.
    nan_path = str(work / "all_nan.tif")
    ds = gdal.GetDriverByName("GTiff").Create(nan_path, 3, 3, 1, gdal.GDT_Float32)
    ds.SetGeoTransform([0, 1, 0, 3, 0, -1])
    ds.GetRasterBand(1).Fill(float("nan"))
    ds = None
    paths["all_nan"] = nan_path
    # The Landsat scene with an alpha band and with a mask, cleaned once by GDAL.
    for name, flag in (("landsat7_alpha", "-setalpha"), ("landsat7_mask", "-setmask")):
        path = str(work / f"{name}.tif")
        run(["nearblack", "-q", flag, "-nb", "0", "-near", "0", "-o", path, paths["landsat7"]])
        paths[name] = path
    for label, nodata in [("fractional", 1.0000000001), ("negative", -9999.), ("above_byte", 300.),
                          ("huge", 1e30), ("infinity", float("inf")), ("nan", float("nan"))]:
        path = str(work / f"enhance_nodata_{label}.tif")
        ds = gdal.Translate(path, paths["dem"], srcWin=[0, 0, 8, 8], outputType=gdal.GDT_Float64, noData=nodata)
        band = ds.GetRasterBand(1)
        values = band.ReadAsArray()
        values[0, 0] = nodata
        band.WriteArray(values)
        ds = None
        paths[f"enhance_nodata_{label}"] = path
    return paths


def neighbor_cases(p):
    """The `gdal raster neighbors` cases.

    Args:
        p: The input paths.

    Returns:
        list: Cases with the GDAL and Terrio arguments (without input and output).
    """
    cases = []

    def add(name, src, opts):
        cases.append({"name": f"neighbors {name}", "kind": "neighbors", "src": p[src], "opts": opts})

    add("all NaN mode without nodata", "all_nan", ["--kernel", "equal", "--method", "mode"])
    for kernel in ("u", "v", "edge1", "edge2", "sharpen", "gaussian", "unsharp-masking", "equal"):
        add(f"{kernel} dem", "dem", ["--kernel", kernel])
    add("gaussian 5x5 dem", "dem", ["--kernel", "gaussian", "--size", "5"])
    for method in ("sum", "mean", "min", "max", "stddev", "median", "mode"):
        add(f"equal 5x5 {method} dem holes", "dem_holes", ["--kernel", "equal", "--size", "5", "--method", method])
    add("equal 9x9 mean red", "red", ["--kernel", "equal", "--size", "9"])
    add("matrix red", "red", ["--kernel", "[[1,0,-1],[2,0,-2],[1,0,-1]]"])
    add("matrix 5x5 median red", "red", ["--kernel", "[[0,0,1,0,0],[0,1,1,1,0],[1,1,1,1,1],[0,1,1,1,0],[0,0,1,0,0]]", "--method", "median"])
    add("kernels and methods tci", "tci", ["--kernel", "gaussian", "--kernel", "edge2", "--kernel", "equal", "--method", "mean", "--method", "sum", "--method", "mode", "--ot", "Byte"])
    add("band 2 sharpen tci Int16", "tci", ["-b", "2", "--kernel", "sharpen", "--ot", "Int16"])
    add("nodata 0 Float32 dem holes", "dem_holes", ["--kernel", "gaussian", "--nodata", "0", "--ot", "Float32"])
    add("nodata none dem holes", "dem_holes", ["--kernel", "equal", "--method", "max", "--nodata", "none"])
    add("gaussian 5x5 whole dem", "dem_full", ["--kernel", "gaussian", "--size", "5", "--ot", "Float32"])
    add("equal 9x9 max whole dem", "dem_full", ["--kernel", "equal", "--size", "9", "--method", "max", "--ot", "Float32"])
    add("UInt16 mode red", "red", ["--kernel", "equal", "--size", "3", "--method", "mode", "--ot", "UInt16"])
    return cases


def collar_cases(p):
    """The `nearblack` and `gdal raster clean-collar` cases.

    Args:
        p: The input paths.

    Returns:
        list: Cases with GDAL's and Terrio's arguments.
    """
    cases = []
    nb = {"-nb": "--pixel-distance", "-near": "--color-threshold", "-setalpha": "--add-alpha", "-setmask": "--add-mask",
          "-color": "--color", "-white": "--white", "-alg": "--algorithm"}
    for src in ("tci", "landsat7"):
        for opts in ([], ["-nb", "0"], ["-near", "5", "-nb", "4"], ["-setalpha"], ["-setmask"],
                     ["-color", "0,0,0", "-color", "30,30,30"], ["-white"], ["-alg", "floodfill"],
                     ["-alg", "floodfill", "-nb", "0", "-setalpha"], ["-alg", "floodfill", "-nb", "3", "-setmask"]):
            name = " ".join(opts) or "default"
            cases.append({"name": f"nearblack {src} {name}", "kind": "nearblack", "src": p[src], "gdal": opts,
                          "terrio": [nb.get(v, v) for v in opts]})
    for src in ("tci", "landsat7", "landsat7_alpha", "landsat7_mask"):
        for opts in ([], ["--color", "white"], ["--color", "0,0,0", "--color", "20,20,20", "--color-threshold", "5"],
                     ["--pixel-distance", "0"], ["--add-alpha"], ["--add-mask"], ["--algorithm", "twopasses"]):
            name = " ".join(opts) or "default"
            cases.append({"name": f"clean-collar {src} {name}", "kind": "clean-collar", "src": p[src], "gdal": opts, "terrio": opts})
    for src in ("tci", "landsat7_alpha", "landsat7_mask"):
        for opts in ([], ["--add-mask"], ["--algorithm", "twopasses", "--pixel-distance", "3"]):
            name = " ".join(opts) or "default"
            cases.append({"name": f"clean-collar update {src} {name}", "kind": "update", "src": p[src], "gdal": opts, "terrio": opts})
    cases.append({"name": "nearblack update keeps existing mask without mask flags", "kind": "nearblack-update",
                  "src": p["landsat7_mask"]})
    return cases


def enhance_cases(p):
    """The `gdalenhance` cases.

    Args:
        p: The input paths.

    Returns:
        list: Cases.
    """
    cases = [{"name": f"enhance {src}", "kind": "enhance", "src": p[src]} for src in ("tci", "landsat7", "red", "dem")]
    cases.extend({"name": f"enhance Byte nodata {label}", "kind": "enhance-nodata", "src": path}
                 for key, path in p.items() if key.startswith("enhance_nodata_")
                 for label in [key.removeprefix("enhance_nodata_")])
    return cases


def mask_mismatch(gdal_path, terrio_path):
    """Counts the pixels whose per-dataset masks differ.

    Args:
        gdal_path: GDAL's output.
        terrio_path: Terrio's output.

    Returns:
        int: The number of differing mask pixels (all of them when only one has a mask).
    """
    da, db = gdal.Open(gdal_path), gdal.Open(terrio_path)
    a, b = da.GetRasterBand(1), db.GetRasterBand(1)
    fa, fb = a.GetMaskFlags(), b.GetMaskFlags()
    if fa != gdal.GMF_PER_DATASET and fb != gdal.GMF_PER_DATASET:
        return 0
    if fa != fb:
        return a.XSize * a.YSize
    return int((a.GetMaskBand().ReadAsArray() != b.GetMaskBand().ReadAsArray()).sum())


def changed(src, out):
    """Counts the pixels of the color bands an operation changed.

    Args:
        src: The input.
        out: The output.

    Returns:
        int: The number of changed values.
    """
    a = gdal.Open(src).ReadAsArray()
    b = gdal.Open(out).ReadAsArray()
    n = min(a.shape[0], b.shape[0]) if a.ndim == 3 else 1
    return int((a[:n] != b[:n]).sum()) if a.ndim == 3 else int((a != b).sum())


def text_result(name, a, b):
    """A result comparing two text files.

    Args:
        name: The case name.
        a: GDAL's file.
        b: Terrio's file.

    Returns:
        dict: A result as ``compare`` returns them.
    """
    ta, tb = Path(a).read_text(), Path(b).read_text()
    differ = sum(1 for x, y in zip(ta.split(), tb.split()) if x != y) + abs(len(ta.split()) - len(tb.split()))
    return {"name": name, "size": f"{len(ta.split())} tokens", "same_size": True, "same_type": True, "same_nodata": True,
            "gt_diff_px": 0.0, "differing": differ, "exact": 1.0 if differ == 0 else 0.0, "within_tol": 1.0 if differ == 0 else 0.0,
            "max_diff": 0.0, "inf_diff": 0}


def run_case(case, terrio, work, i):
    """Runs one case with both tools and compares the outputs.

    Args:
        case: The case.
        terrio: Terrio's executable.
        work: The working directory.
        i: The case number.

    Returns:
        list: The results.
    """
    gout, tout = str(work / f"gdal_{i}.tif"), str(work / f"terrio_{i}.tif")
    for path in (gout, tout):
        if os.path.exists(path):
            os.remove(path)
    kind = case["kind"]
    if kind == "neighbors":
        run(["gdal", "raster", "neighbors", "-q", *case["opts"], case["src"], gout])
        run([terrio, "neighbors", case["src"], tout, *case["opts"]])
    elif kind == "nearblack":
        run(["nearblack", "-q", *case["gdal"], "-o", gout, case["src"]])
        run([terrio, "nearblack", case["src"], tout, *case["terrio"]])
    elif kind == "clean-collar":
        run(["gdal", "raster", "clean-collar", "-q", *case["gdal"], case["src"], gout])
        run([terrio, "clean-collar", case["src"], tout, *case["terrio"]])
    elif kind == "update":
        shutil.copy(case["src"], gout)
        shutil.copy(case["src"], tout)
        run(["gdal", "raster", "clean-collar", "-q", "--update", *case["gdal"], gout])
        run([terrio, "clean-collar", "--update", tout, *case["terrio"]])
    elif kind == "nearblack-update":
        gdal.Translate(gout, case["src"])
        gdal.Translate(tout, case["src"])
        run(["nearblack", "-q", "-o", gout, gout])
        run([terrio, "nearblack", "--update", tout])
    elif kind == "enhance-nodata":
        config = str(work / f"nodata_lut_{i}.txt")
        Path(config).write_text("1:Band -0.5:ScaleMin 255.5:ScaleMax " + " ".join(map(str, range(256))) + "\n")
        run(["gdalenhance", "-quiet", "-config", config, case["src"], gout])
        run([terrio, "enhance", case["src"], tout, "--config", config])
    elif kind == "enhance":
        return enhance(case, terrio, work, i)
    result = compare(gout, tout, 0)
    if kind == "update" and case["src"].endswith("landsat7_alpha.tif") and not case["gdal"]:
        # GDAL 3.12's in-place flood fill reads only the color bands into a line buffer it
        # writes back whole: the alpha band of every modified line takes stale buffer values
        # (zeros). Compare the color bands, and Terrio's alpha band with GDAL's output of the
        # same cleaning written to a new file.
        fresh = str(work / f"gdal_{i}_new.tif")
        run(["gdal", "raster", "clean-collar", "-q", case["src"], fresh])
        g, t, f = (gdal.Open(x).ReadAsArray() for x in (gout, tout, fresh))
        result = compare(gout, tout, 0)
        result["differing"] = int((g[:3] != t[:3]).sum()) + int((f[3] != t[3]).sum())
        result["exact"] = 1 - result["differing"] / t.size
        result["within_tol"] = result["exact"]
        result["max_diff"] = 0.0 if result["differing"] == 0 else result["max_diff"]
        result["gdal_stale_alpha"] = int((g[3] != f[3]).sum())
        result["name"] = case["name"] + " (alpha vs GDAL's new-file output)"
    result["valid_mismatch"] = mask_mismatch(gout, tout)
    if kind != "neighbors":
        result["changed"] = changed(case["src"], gout)
    return [result]


def enhance(case, terrio, work, i):
    """Runs the `gdalenhance` comparisons of an input: equalization, LUT file output, and
    enhancement by a LUT file read back (with reversed tables).

    Args:
        case: The case.
        terrio: Terrio's executable.
        work: The working directory.
        i: The case number.

    Returns:
        list: The results.
    """
    results = []
    # Separate copies: gdalenhance saves the histograms it computes next to its input.
    g_dir, t_dir = work / f"enhance_gdal_{i}", work / f"enhance_terrio_{i}"
    for d in (g_dir, t_dir):
        shutil.rmtree(d, ignore_errors=True)
        d.mkdir()
    g_src, t_src = str(g_dir / "src.tif"), str(t_dir / "src.tif")
    shutil.copy(case["src"], g_src)
    shutil.copy(case["src"], t_src)
    gout, tout = str(g_dir / "eq.tif"), str(t_dir / "eq.tif")
    run(["gdalenhance", "-quiet", "-equalize", g_src, gout])
    run([terrio, "enhance", t_src, tout, "--equalize"])
    r = compare(gout, tout, 0)
    r["name"] = case["name"] + " equalize"
    results.append(r)
    for d in (g_dir, t_dir):
        for aux in d.glob("*.aux.xml"):
            aux.unlink()
    g_lut, t_lut = str(g_dir / "lut.txt"), str(t_dir / "lut.txt")
    run(["gdalenhance", "-quiet", "-equalize", "-config", g_lut, g_src])
    run([terrio, "enhance", t_src, "--equalize", "--config", t_lut])
    results.append(text_result(case["name"] + " config output", g_lut, t_lut))
    reversed_lut = str(work / f"reversed_{i}.txt")
    with open(g_lut) as f, open(reversed_lut, "w") as out:
        for line in f:
            tokens = line.split()
            out.write(" ".join(tokens[:3] + tokens[3:][::-1]) + " \n")
    gout, tout = str(g_dir / "lut.tif"), str(t_dir / "lut.tif")
    run(["gdalenhance", "-quiet", "-config", reversed_lut, g_src, gout])
    run([terrio, "enhance", t_src, tout, "--config", reversed_lut])
    r = compare(gout, tout, 0)
    r["name"] = case["name"] + " config input"
    results.append(r)
    return results


def main():
    """Runs and reports the comparisons."""
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("data")
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--only")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--keep")
    args = parser.parse_args()
    work = Path(args.keep or tempfile.mkdtemp(prefix="terrio_focal_"))
    work.mkdir(parents=True, exist_ok=True)
    results = []
    try:
        version = subprocess.run(["gdal", "--version"], capture_output=True, text=True).stdout
        paths = prepare(args.data, work)
        cases = neighbor_cases(paths) + collar_cases(paths) + enhance_cases(paths)
    except Exception as exc:
        results.append({"name": "focal setup", "error": str(exc)})
        cases, version = [], ""
    for i, case in enumerate(cases):
        if args.only and args.only not in case["name"]:
            continue
        try:
            out = run_case(case, args.terrio, work, i)
        except Exception as exc:
            out = [{"error": str(exc)}]
        for r in out:
            r.setdefault("name", case["name"])
            r["gdal_version"] = version.strip()
            results.append(r)
            if not args.json:
                extra = f" changed={r['changed']}" if "changed" in r else ""
                extra += f" mask_differ={r['valid_mismatch']}" if r.get("valid_mismatch") else ""
                extra += f" gdal_stale_alpha={r['gdal_stale_alpha']}" if "gdal_stale_alpha" in r else ""
                print(format_row(r) + extra, flush=True)
    if args.json:
        print(json.dumps(results, indent=2))
    if not args.keep:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    main()
