#!/usr/bin/env python3
"""Compare terrio's terrain and raster algebra tools with GDAL's on real files.

Runs ``terrio dem``, ``terrio calc``, ``terrio proximity`` and ``terrio sieve`` and the
equivalent ``gdaldem``, ``gdal_calc.py``, ``gdal_proximity.py`` and ``gdal_sieve.py`` commands
on the files fetched by ``scripts/fetch_test_data.sh``, then reports for each case whether the
outputs have the same size, type, geotransform and nodata value, and how many pixels agree.

Usage:
    python scripts/compare_dem_calc_with_gdal.py ../terrio-data [--terrio target/release/terrio]
        [--only NAME] [--json] [--keep DIR]

Needs GDAL's command-line tools and Python bindings (``osgeo``) with NumPy.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile

import numpy as np
from osgeo import gdal

gdal.UseExceptions()

COLORS = """# elevation colors
nv 0 0 0 0
1500 46 154 88
2000 251 255 128
2500 224 108 31
3000 200 55 55
3500 215 244 244
4000 255 255 255
"""

COLORS_PCT = """0% blue
25% 0 255 0 200
50%:yellow
50% red
75%,brown,128
100% white
"""


def run(cmd):
    """Runs a command and raises with its output when it fails.

    Args:
        cmd: The command and its arguments.
    """
    p = subprocess.run(cmd, capture_output=True, text=True)
    if p.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd)}\n{p.stdout}{p.stderr}")


def read(path):
    """Reads a raster's pixels and properties.

    Args:
        path: The raster's path.

    Returns:
        A dict with ``data`` (bands × rows × cols, float64), ``gt``, ``nodata`` and ``type``.
    """
    ds = gdal.Open(path)
    data = ds.ReadAsArray().astype(np.float64)
    if data.ndim == 2:
        data = data[np.newaxis]
    bands = [ds.GetRasterBand(i + 1) for i in range(ds.RasterCount)]
    return {
        "data": data,
        "gt": ds.GetGeoTransform(),
        "nodata": [b.GetNoDataValue() for b in bands],
        "type": ",".join(sorted({gdal.GetDataTypeName(b.DataType) for b in bands})),
    }


def same_value(a, b):
    """Whether two optional numbers are equal, NaN equal to NaN.

    Args:
        a: A number or None.
        b: A number or None.

    Returns:
        bool: Whether they are the same.
    """
    if a is None or b is None:
        return a is None and b is None
    return a == b or (np.isnan(a) and np.isnan(b))


def compare(gdal_path, terrio_path, tol):
    """Compares two rasters pixel by pixel, nodata pixels included.

    Args:
        gdal_path: GDAL's output.
        terrio_path: terrio's output.
        tol: The tolerance of the "within tolerance" count.

    Returns:
        A dict of comparison results.
    """
    g, t = read(gdal_path), read(terrio_path)
    res = {
        "size": "x".join(str(s) for s in g["data"].shape[::-1]),
        "same_size": g["data"].shape == t["data"].shape,
        "type": g["type"],
        "same_type": g["type"] == t["type"],
        "same_nodata": all(same_value(a, b) for a, b in zip(g["nodata"], t["nodata"])),
    }
    px = abs(g["gt"][1]) or 1.0
    res["gt_diff_px"] = max(abs(a - b) for a, b in zip(g["gt"], t["gt"])) / px
    if not res["same_size"]:
        return res
    gd, td = g["data"], t["data"]
    both_nan = np.isnan(gd) & np.isnan(td)
    with np.errstate(invalid="ignore"):
        diff = np.abs(gd - td)
    diff[gd == td] = 0
    diff[both_nan] = 0
    diff[np.isnan(diff)] = np.inf
    n = diff.size
    res["differing"] = int((diff != 0).sum())
    res["exact"] = float((diff == 0).sum() / n)
    res["within_tol"] = float((diff <= tol).sum() / n)
    finite = diff[np.isfinite(diff)]
    res["max_diff"] = float(finite.max()) if finite.size else 0.0
    res["inf_diff"] = int(np.isinf(diff).sum())
    return res


def cases(d, tmp):
    """The comparison cases.

    Args:
        d: The test data directory.
        tmp: A directory for inputs written by the script (color files, copies).

    Returns:
        A list of dicts with the case name, the GDAL and terrio command lines (with ``{out}``
        standing for the output path) and the tolerance.
    """
    # The DEMs are copied with their statistics computed, which gdaldem reads (from the .aux.xml
    # file, rounded) for color percentages, so that both tools use the same and the data
    # directory is left untouched.
    def dem_copy(name):
        path = os.path.join(tmp, name)
        shutil.copy(f"{d}/{name}", path)
        run(["gdalinfo", "-stats", path])
        return path

    usgs = dem_copy("usgs_dem.tif")
    srtm = dem_copy("srtm90.tif")
    merc = dem_copy("dem.tif")
    lv = dem_copy("las_vegas_dem.tif")
    landsat = f"{d}/landsat.tif"
    nlcd = f"{d}/nlcd_knoxville.tif"
    colors = os.path.join(tmp, "colors.txt")
    with open(colors, "w") as f:
        f.write(COLORS)
    colors_pct = os.path.join(tmp, "colors_pct.txt")
    with open(colors_pct, "w") as f:
        f.write(COLORS_PCT)
    out = []

    def add(name, gdal_cmd, terrio_cmd, tol=0.0, ext="tif"):
        out.append({"name": name, "gdal": gdal_cmd, "terrio": terrio_cmd, "tol": tol, "ext": ext})

    def dem(name, mode, path, gdal_opts=(), terrio_opts=(), tol=0.0):
        add(
            f"dem {mode} {name}",
            ["gdaldem", mode, path, "{out}", "-q", *gdal_opts],
            ["dem", mode, path, "{out}", *terrio_opts],
            tol,
        )

    dems = [("usgs (NAD83, Float32)", usgs), ("srtm90 (WGS84, Int16)", srtm), ("dem (3857, Float64 NaN)", merc), ("las_vegas (3857, -inf)", lv)]
    for label, p in dems:
        dem(label, "hillshade", p)
        dem(label, "slope", p)
        dem(label, "aspect", p)
        dem(label, "TRI", p)
        dem(label, "TPI", p)
        dem(label, "roughness", p)
        dem(label + " edges", "hillshade", p, ["-compute_edges"], ["--compute-edges"])
        dem(label + " edges", "slope", p, ["-compute_edges"], ["--compute-edges"])
    for label, p in [dems[0], dems[3]]:
        dem(label + " z2 az200 alt30", "hillshade", p, ["-z", "2", "-az", "200", "-alt", "30"], ["-z", "2", "--az", "200", "--alt", "30"])
        dem(label + " combined", "hillshade", p, ["-combined"], ["--combined"])
        dem(label + " multidirectional", "hillshade", p, ["-multidirectional"], ["--multidirectional"])
        dem(label + " igor", "hillshade", p, ["-igor"], ["--igor"])
        dem(label + " ZevenbergenThorne", "hillshade", p, ["-alg", "ZevenbergenThorne"], ["--alg", "ZevenbergenThorne"])
        dem(label + " ZT combined edges", "hillshade", p, ["-alg", "ZevenbergenThorne", "-combined", "-compute_edges"], ["--alg", "ZevenbergenThorne", "--combined", "--compute-edges"])
        dem(label + " multidirectional ZT", "hillshade", p, ["-multidirectional", "-alg", "ZevenbergenThorne"], ["--multidirectional", "--alg", "ZevenbergenThorne"])
        dem(label + " percent", "slope", p, ["-p"], ["-p"])
        dem(label + " ZevenbergenThorne", "slope", p, ["-alg", "ZevenbergenThorne"], ["--alg", "ZevenbergenThorne"])
        dem(label + " trigonometric", "aspect", p, ["-trigonometric"], ["--trigonometric"])
        dem(label + " zero_for_flat edges", "aspect", p, ["-zero_for_flat", "-compute_edges"], ["--zero-for-flat", "--compute-edges"])
        dem(label + " ZevenbergenThorne", "aspect", p, ["-alg", "ZevenbergenThorne"], ["--alg", "ZevenbergenThorne"])
        dem(label + " Wilson", "TRI", p, ["-alg", "Wilson"], ["--alg", "Wilson"])
        dem(label + " edges", "TPI", p, ["-compute_edges"], ["--compute-edges"])
        dem(label + " edges", "roughness", p, ["-compute_edges"], ["--compute-edges"])
    dem("usgs s=111120", "hillshade", usgs, ["-s", "111120"], ["-s", "111120"])
    dem("usgs s=111120", "slope", usgs, ["-s", "111120"], ["-s", "111120"])
    dem("srtm90 s=111120 z=3", "hillshade", srtm, ["-s", "111120", "-z", "3"], ["-s", "111120", "-z", "3"])
    dem("srtm90 xscale yscale", "slope", srtm, ["-xscale", "90000", "-yscale", "111120"], ["--xscale", "90000", "--yscale", "111120"])
    dem("landsat band 2 (Byte)", "hillshade", landsat, ["-b", "2"], ["-b", "2"])
    dem("landsat band 2 (Byte) edges", "TRI", landsat, ["-b", "2", "-compute_edges"], ["-b", "2", "--compute-edges"])

    def relief(name, path, cfile, opts=()):
        mine = ["-" + o.replace("_", "-") for o in opts]
        add(f"dem color-relief {name}", ["gdaldem", "color-relief", path, cfile, "{out}", "-q", *opts], ["dem", "color-relief", path, cfile, "{out}", *mine])

    for label, p in dems:
        relief(label, p, colors)
        relief(label + " percentages", p, colors_pct)
    relief("usgs alpha", usgs, colors, ["-alpha"])
    relief("usgs nearest", usgs, colors, ["-nearest_color_entry"])
    relief("usgs exact", usgs, colors, ["-exact_color_entry", "-alpha"])
    relief("srtm90 exact alpha", srtm, colors, ["-exact_color_entry", "-alpha"])
    relief("srtm90 nearest", srtm, colors, ["-nearest_color_entry"])

    # Color relief as a VRT of lookup tables: both VRTs read by GDAL, and gdaldem's read by terrio.
    def relief_vrt(name, path, cfile, opts=()):
        mine = ["-" + o.replace("_", "-") for o in opts]
        gdal = ["gdaldem", "color-relief", "-of", "VRT", path, cfile, "{out}", "-q", *opts]
        add(f"dem color-relief VRT {name}", gdal, ["dem", "color-relief", path, cfile, "{out}", "-f", "VRT", *mine], ext="vrt")
        add(f"dem color-relief VRT {name} read by terrio", gdal, ["convert", "{gout}", "{out}"], ext="vrt")

    for label, p in dems:
        relief_vrt(label, p, colors)
    relief_vrt("usgs percentages alpha", usgs, colors_pct, ["-alpha"])
    relief_vrt("usgs nearest", usgs, colors, ["-nearest_color_entry"])
    relief_vrt("srtm90 exact alpha", srtm, colors, ["-exact_color_entry", "-alpha"])
    relief_vrt("las_vegas nearest alpha", lv, colors, ["-nearest_color_entry", "-alpha"])

    # gdal_calc.py: terrio takes the same arguments.
    def calc(name, args, tol=0.0):
        add(f"calc {name}", ["gdal_calc.py", "--quiet", "--overwrite", "--outfile", "{out}", *args], ["calc", "--outfile", "{out}", *args], tol)

    red = ["-A", landsat, "--A_band", "3"]
    nir = ["-B", landsat, "--B_band", "4"]
    calc("landsat NDVI Float32", [*red, *nir, "--calc", "(B.astype(float32) - A) / (B.astype(float32) + A)", "--type", "Float32"])
    calc("landsat NDVI float64 promotion", [*red, *nir, "--calc", "(B - A * 1.0) / (B + A * 1.0)", "--type", "Float32"])
    calc("landsat Byte wrap-around A+B", [*red, *nir, "--calc", "A + B"])
    calc("landsat (B-A)/(B+A) uint8", [*red, *nir, "--calc", "(B - A) / (B + A)", "--type", "Float64"])
    calc("landsat sqrt float16", [*red, "--calc", "sqrt(A) * 10"])
    calc("landsat sqrt float16 Float32", [*red, "--calc", "sqrt(A)", "--type", "Float32"])
    calc("landsat mask logical_and", [*red, *nir, "--calc", "logical_and(A > 50, B < 100)"])
    calc("landsat where NoDataValue", [*red, *nir, "--calc", "where(B > A, B - A, 0)", "--NoDataValue", "0"])
    calc("landsat hideNoData", [*red, *nir, "--calc", "A // 3 + B % 7", "--hideNoData"])
    calc("landsat two bands", [*red, *nir, "--calc", "A", "--calc", "B * 2", "--type", "UInt16"])
    calc("landsat allBands", ["-A", landsat, "--allBands", "A", "--calc", "A / 2"])
    calc("landsat clip minimum maximum", [*red, *nir, "--calc", "clip(A, 30, 90) + minimum(A, B) - maximum(A, 10)"])
    calc("landsat bitwise", [*red, *nir, "--calc", "(A & 15) | (B ^ 3)"])
    dem_a = ["-A", usgs]
    calc("usgs threshold bool", [*dem_a, "--calc", "A > 3000"])
    calc("usgs threshold where", [*dem_a, "--calc", "where(A > 3000, A, 0)", "--NoDataValue", "-1"])
    calc("usgs A*(A>2500)", [*dem_a, "--calc", "A * (A > 2500)"])
    calc("usgs Int16 rounding", [*dem_a, "--calc", "A / 3", "--type", "Int16"])
    calc("usgs UInt16 none nodata", [*dem_a, "--calc", "A - 1000", "--type", "UInt16", "--NoDataValue", "none"])
    calc("usgs functions (NumPy SIMD float32)", [*dem_a, "--calc", "log10(A) + exp(A / 2000) - sqrt(abs(A - 2500)) + sin(A) * cos(A)"], 1e-5)
    calc("usgs floor ceil round", [*dem_a, "--calc", "floor(A / 7) + ceil(A / 11) + round(A / 13)"])
    calc("usgs arctan2 hypot (NumPy SIMD float32)", [*dem_a, "--calc", "arctan2(A, 1000) + hypot(A, 300)"], 1e-3)
    calc("usgs power modulo", [*dem_a, "--calc", "(A % 97) ** 2 + A // 50", "--type", "Float64"])
    calc("usgs comparisons chain", [*dem_a, "--calc", "where((A > 2000) & (A < 3000), 1, 0) + 2 * (A >= 3500)", "--type", "Byte"])
    dem_b = ["-A", srtm]
    calc("srtm90 Int16 wrap A*20", [*dem_b, "--calc", "A * 20"])
    calc("srtm90 int16 sqrt float32", [*dem_b, "--calc", "sqrt(A)", "--type", "Float32"])
    calc("srtm90 int16 true division", [*dem_b, "--calc", "A / 7"])
    calc("srtm90 astype", [*dem_b, "--calc", "A.astype(float32) / 7", "--type", "Float32"])
    calc("dem NaN nodata Float64", ["-A", merc, "--calc", "A * 2 + 1"])
    calc("dem where isnan", ["-A", merc, "--calc", "where(isnan(A), -1, A / 3)", "--type", "Float32"])
    calc("las_vegas -inf nodata", ["-A", lv, "--calc", "A - 600"])
    calc("nlcd class mask", ["-A", nlcd, "--calc", "(A == 41) | (A == 42) | (A == 43)"])
    calc("nlcd no nodata default Byte", ["-A", nlcd, "--calc", "A + 100"])
    nir32 = os.path.join(tmp, "nir_float32.tif")
    run(["gdal_translate", "-q", "-b", "4", "-ot", "Float32", "-a_nodata", "-1", landsat, nir32])
    calc("mixed types Byte + Float32", [*red, "-B", nir32, "--calc", "B / 10 + A"])
    calc("mixed types Int16 output", [*red, "-B", nir32, "--calc", "B - A * 2", "--type", "Int16"])

    # Inputs with different extents.
    part1 = os.path.join(tmp, "part1.tif")
    part2 = os.path.join(tmp, "part2.tif")
    run(["gdal_translate", "-q", "-srcwin", "0", "0", "2000", "1500", usgs, part1])
    run(["gdal_translate", "-q", "-srcwin", "1200", "800", "2000", "2000", usgs, part2])
    calc("extent union", ["-A", part1, "-B", part2, "--calc", "A + B", "--extent", "union"])
    calc("extent intersect", ["-A", part1, "-B", part2, "--calc", "A - B", "--extent", "intersect"])
    calc("projwin", ["-A", part1, "-B", part2, "--calc", "A * 2", "--projwin", "-105.75", "39.85", "-105.55", "39.7"])

    # gdal_proximity.py and gdal_sieve.py.
    high = os.path.join(tmp, "high.tif")
    run(["gdal_calc.py", "--quiet", "--overwrite", "-A", usgs, "--calc", "A > 3600", "--outfile", high, "--NoDataValue", "none"])
    classes = os.path.join(tmp, "classes.tif")
    run(["gdal_calc.py", "--quiet", "--overwrite", "-A", usgs, "--calc", "floor(A / 150)", "--type", "Int16", "--outfile", classes, "--NoDataValue", "0"])

    def prox(name, path, gopts=(), topts=()):
        add(f"proximity {name}", ["gdal_proximity.py", "-q", path, "{out}", *gopts], ["proximity", path, "{out}", *topts])

    prox("nlcd water", nlcd, ["-values", "11"], ["--values", "11"])
    prox("nlcd developed GEO maxdist", nlcd, ["-values", "21,22,23,24", "-distunits", "GEO", "-maxdist", "2000", "-nodata", "-1"], ["--values", "21,22,23,24", "--distunits", "GEO", "--maxdist", "2000", "--nodata", "-1"])
    prox("usgs > 3600 (non-zero)", high)
    prox("usgs > 3600 Int16", high, ["-ot", "Int16"], ["--ot", "Int16"])
    prox("usgs > 3600 Byte fixed-buf-val", high, ["-ot", "Byte", "-maxdist", "100", "-fixed-buf-val", "1", "-nodata", "0"], ["--ot", "Byte", "--maxdist", "100", "--fixed-buf-val", "1", "--nodata", "0"])
    prox("usgs > 3600 UInt16 GEO", high, ["-ot", "UInt16", "-distunits", "GEO"], ["--ot", "UInt16", "--distunits", "GEO"])
    prox("landsat band 1 use_input_nodata", landsat, ["-values", "60,61,62", "-use_input_nodata", "YES"], ["--values", "60,61,62", "--use-input-nodata"])

    def sieve(name, path, gopts=(), topts=()):
        add(f"sieve {name}", ["gdal_sieve.py", "-q", *gopts, path, "{out}"], ["sieve", path, "{out}", *topts])

    sieve("nlcd st 10", nlcd, ["-st", "10"], ["--st", "10"])
    sieve("nlcd st 50 8-connected", nlcd, ["-st", "50", "-8"], ["--st", "50", "-8"])
    sieve("nlcd st 5 nomask", nlcd, ["-st", "5", "-nomask"], ["--st", "5", "--nomask"])
    sieve("usgs classes st 100 (nodata 0)", classes, ["-st", "100"], ["--st", "100"])
    sieve("usgs classes st 30 8-connected mask", classes, ["-st", "30", "-8", "-mask", high], ["--st", "30", "-8", "--mask", high])
    sieve("landsat st 4", landsat, ["-st", "4"], ["--st", "4"])
    # Real USGS terrain with controlled holes: edges, interior blocks and an
    # isolated donor. Keep Float32 so valid-pixel preservation is exact in both.
    hole_path = os.path.join(tmp, "fill_holes.tif")
    mask_path = os.path.join(tmp, "fill_mask.tif")
    terrain = gdal.Translate(hole_path, usgs, srcWin=[0, 0, 96, 80], outputType=gdal.GDT_Float32)
    band = terrain.GetRasterBand(1)
    pixels = band.ReadAsArray()
    validity = np.full(pixels.shape, 255, dtype=np.uint8)
    validity[0:7, :13] = 0
    validity[23:35, 37:49] = 0
    validity[50:65, 60:79] = 0
    validity[58, 69] = 255
    mask_ds = gdal.GetDriverByName("GTiff").Create(mask_path, 96, 80, 1, gdal.GDT_Byte)
    mask_ds.GetRasterBand(1).WriteArray(validity)
    mask_ds = None
    pixels[validity == 0] = -9999
    band.WriteArray(pixels)
    band.SetNoDataValue(-9999)
    terrain = None
    for method in ["inv_dist", "nearest"]:
        for distance in [2, 10, 0]:
            for smooth in [0, 1, 3]:
                for explicit in [False, True]:
                    add(
                        f"fill-nodata USGS {method} d{distance} smooth{smooth} mask{explicit}",
                        ["gdal_fillnodata", "-q", "-interp", method, "-md", str(distance), "-si", str(smooth),
                         *(["-mask", mask_path] if explicit else []), hole_path, "{out}"],
                        ["fill-nodata", hole_path, "{out}", "--interpolation", method,
                         "--max-distance", str(distance), "--smoothing-iterations", str(smooth),
                         *(["--mask", mask_path] if explicit else [])],
                    )
    return out


def main():
    """Runs the comparisons and prints a report."""
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("data", help="the test data directory")
    ap.add_argument("--terrio", default=os.path.join(os.path.dirname(__file__), "..", "target", "release", "terrio"))
    ap.add_argument("--only", help="only cases whose name contains this")
    ap.add_argument("--json", action="store_true", help="print JSON")
    ap.add_argument("--keep", help="keep the outputs in this directory")
    a = ap.parse_args()
    tmp = a.keep or tempfile.mkdtemp(prefix="terrio_dem_calc_")
    os.makedirs(tmp, exist_ok=True)
    if not a.json:
        print(f"GDAL {gdal.__version__} (terrio reproduces GDAL 3.12's arithmetic)\n")
    results = []
    for i, c in enumerate(cases(os.path.abspath(a.data), tmp)):
        if a.only and a.only not in c["name"]:
            continue
        ext = c.get("ext", "tif")
        gout = os.path.join(tmp, f"case{i}_gdal.{ext}")
        # terrio converting GDAL's output writes a GeoTIFF.
        tout = os.path.join(tmp, f"case{i}_terrio.{'tif' if '{gout}' in c['terrio'] else ext}")
        for p in (gout, tout):
            if os.path.exists(p):
                os.remove(p)
        try:
            run([x.replace("{out}", gout) for x in c["gdal"]])
            run([a.terrio] + [x.replace("{out}", tout).replace("{gout}", gout) for x in c["terrio"]])
            r = compare(gout, tout, c["tol"])
        except Exception as e:  # noqa: BLE001 - report and continue
            r = {"error": str(e).strip().splitlines()[-1][:200]}
        r["name"] = c["name"]
        results.append(r)
        if not a.json:
            print(format_row(r), flush=True)
    if a.json:
        print(json.dumps(results, indent=2))
    else:
        ok = sum(1 for r in results if r.get("exact") == 1.0 and r.get("same_type") and r.get("same_nodata"))
        tol = sum(1 for r in results if r.get("within_tol") == 1.0 and r.get("same_type") and r.get("same_nodata"))
        print(f"\n{ok}/{len(results)} cases identical to GDAL, {tol}/{len(results)} within tolerance")
    if not a.keep:
        shutil.rmtree(tmp, ignore_errors=True)


def format_row(r):
    """Formats one result as a line of text.

    Args:
        r: A result from ``compare``.

    Returns:
        str: The line.
    """
    if "error" in r:
        return f"{r['name']:<58} ERROR {r['error']}"
    if not r["same_size"]:
        return f"{r['name']:<58} size differs (GDAL {r['size']})"
    return (
        f"{r['name']:<58} {r['size']:>14} type={'ok' if r['same_type'] else 'DIFF'} "
        f"nodata={'ok' if r['same_nodata'] else 'DIFF'} gt={r['gt_diff_px']:.0e}px "
        f"exact={100 * r['exact']:.4f}% ({r['differing']} differ) max={r['max_diff']:.3g}"
        + (f" within_tol={100 * r['within_tol']:.4f}%" if r["exact"] < 1 else "")
    )


if __name__ == "__main__":
    sys.exit(main())
