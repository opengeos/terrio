#!/usr/bin/env python3
"""Compare terrio's raster operations with GDAL's tools on real files.

Runs ``terrio translate``/``warp``/``mosaic`` and the equivalent ``gdal_translate``,
``gdalwarp``, ``gdalbuildvrt`` and ``gdal_merge.py`` commands on the files fetched by
``scripts/fetch_test_data.sh``, then reports for each case whether the outputs have the same
size, geotransform and CRS, and how many pixels agree.

Usage:
    python scripts/compare_ops_with_gdal.py ../terrio-data [--terrio target/release/terrio]
        [--only NAME] [--json]

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
from osgeo import gdal, osr

gdal.UseExceptions()


def run(cmd):
    """Runs a command and raises with its output when it fails.

    Args:
        cmd: The command and its arguments.
    """
    p = subprocess.run(cmd, capture_output=True, text=True)
    if p.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd)}\n{p.stdout}{p.stderr}")


def read(path):
    """Reads a raster's pixels, geotransform, CRS and nodata values.

    Args:
        path: The raster's path.

    Returns:
        A dict with ``data`` (bands × rows × cols, float64), ``gt``, ``srs`` and ``nodata``.
    """
    ds = gdal.Open(path)
    data = ds.ReadAsArray().astype(np.float64)
    if data.ndim == 2:
        data = data[np.newaxis]
    srs = ds.GetSpatialRef()
    nodata = [ds.GetRasterBand(i + 1).GetNoDataValue() for i in range(ds.RasterCount)]
    types = {gdal.GetDataTypeName(ds.GetRasterBand(i + 1).DataType) for i in range(ds.RasterCount)}
    return {"data": data, "gt": ds.GetGeoTransform(), "srs": srs, "nodata": nodata, "type": ",".join(sorted(types))}


def compare(gdal_path, terrio_path, tol):
    """Compares two rasters.

    Pixels are compared where GDAL's output is valid (not nodata). A pixel agrees exactly when
    the values are equal, and within tolerance when they differ by at most ``tol``.

    Args:
        gdal_path: GDAL's output.
        terrio_path: terrio's output.
        tol: The tolerance for the "within tolerance" count.

    Returns:
        A dict of comparison results.
    """
    g, t = read(gdal_path), read(terrio_path)
    res = {
        "size": f"{g['data'].shape[2]}x{g['data'].shape[1]}x{g['data'].shape[0]}",
        "same_size": g["data"].shape == t["data"].shape,
        "type": g["type"],
        "same_type": g["type"] == t["type"],
    }
    px = abs(g["gt"][1]) or 1.0
    res["gt_diff_px"] = max(abs(a - b) for a, b in zip(g["gt"], t["gt"])) / px
    if g["srs"] is None or t["srs"] is None:
        res["same_crs"] = g["srs"] is None and t["srs"] is None
    else:
        res["same_crs"] = bool(g["srs"].IsSame(t["srs"], ["IGNORE_DATA_AXIS_TO_SRS_AXIS_MAPPING=YES", "CRITERION=EQUIVALENT_EXCEPT_AXIS_ORDER_GEOGCRS"]))
    if not res["same_size"]:
        return res
    gd, td = g["data"], t["data"]
    gnd = g["nodata"][0]
    tnd = t["nodata"][0]

    def invalid(a, nd):
        if nd is None:
            return np.zeros(a.shape, bool)
        return np.isnan(a) if np.isnan(nd) else a == nd

    gv, tv = ~invalid(gd, gnd), ~invalid(td, tnd)
    both = gv & tv
    n = int(both.sum())
    with np.errstate(invalid="ignore"):
        diff = np.abs(gd - td)
    diff[np.isnan(gd) & np.isnan(td)] = 0
    # Equal infinities (valid pixels once a nodata other than -inf is assigned) are equal.
    diff[gd == td] = 0
    diff[np.isnan(diff)] = np.inf
    diff = diff[both]
    res["valid_gdal"] = int(gv.sum())
    res["valid_mismatch"] = int((gv != tv).sum())
    res["differing"] = int((diff != 0).sum())
    res["exact"] = float((diff == 0).sum() / n) if n else 1.0
    res["within_tol"] = float((diff <= tol).sum() / n) if n else 1.0
    res["max_diff"] = float(diff.max()) if n else 0.0
    res["mean_diff"] = float(diff.mean()) if n else 0.0
    return res


def cases(d, tmp):
    """The comparison cases: name, GDAL commands, terrio commands, outputs and tolerance.

    Args:
        d: The test data directory.
        tmp: A directory for intermediate files (VRTs written by GDAL).

    Returns:
        A list of dicts.
    """
    dem = f"{d}/usgs_dem.tif"
    lv = f"{d}/las_vegas_dem.tif"
    landsat = f"{d}/landsat.tif"
    eco = f"{d}/ECOv002_L2T_LSTE_26860_001_10SGD_20230401T203733_0710_01_LST.tif"
    cog = f"{d}/cog.tif"
    tiles = [f"{d}/USGS_1_n40w106.tif", f"{d}/USGS_1_n40w107.tif", f"{d}/USGS_1_n41w106.tif", f"{d}/USGS_1_n41w107.tif"]
    cut = f"{d}/las_vegas_bounds.geojson"
    out = []

    def add(name, gdal_cmd, terrio_args, tol=0.0, out_ext="tif"):
        out.append({"name": name, "gdal": gdal_cmd, "terrio": terrio_args, "tol": tol, "ext": out_ext})

    # gdal_translate
    add("translate srcwin", ["gdal_translate", "-srcwin", "100", "200", "500", "400", dem], ["translate", dem, "--srcwin", "100", "200", "500", "400"])
    add("translate projwin", ["gdal_translate", "-projwin", "-105.7", "39.8", "-105.3", "39.5", dem], ["translate", dem, "--projwin", "-105.7", "39.8", "-105.3", "39.5"])
    add(
        "translate projwin_srs",
        ["gdal_translate", "-projwin", "450000", "4400000", "470000", "4380000", "-projwin_srs", "EPSG:32613", dem],
        ["translate", dem, "--projwin", "450000", "4400000", "470000", "4380000", "--projwin-srs", "EPSG:32613"],
    )
    add("translate outsize 25% near", ["gdal_translate", "-outsize", "25%", "25%", lv], ["translate", lv, "--outsize", "25%", "25%"])
    add("translate outsize 37% near (no ovr)", ["gdal_translate", "-oo", "OVERVIEW_LEVEL=NONE", "-outsize", "37%", "37%", landsat], ["translate", landsat, "--outsize", "37%", "37%"])
    add("translate outsize 333 bilinear", ["gdal_translate", "-outsize", "333", "0", "-r", "bilinear", lv], ["translate", lv, "--outsize", "333", "0", "-r", "bilinear"], 1e-3)
    add("translate outsize 50% cubic", ["gdal_translate", "-outsize", "50%", "50%", "-r", "cubic", lv], ["translate", lv, "--outsize", "50%", "50%", "-r", "cubic"], 1e-3)
    add("translate outsize 200% bilinear", ["gdal_translate", "-outsize", "200%", "200%", "-r", "bilinear", lv], ["translate", lv, "--outsize", "200%", "200%", "-r", "bilinear"], 1e-3)
    add("translate outsize 30% average", ["gdal_translate", "-outsize", "30%", "30%", "-r", "average", lv], ["translate", lv, "--outsize", "30%", "30%", "-r", "average"], 1e-3)
    add("translate bands + ot", ["gdal_translate", "-b", "3", "-b", "1", "-ot", "Int16", landsat], ["translate", landsat, "-b", "3", "-b", "1", "--ot", "Int16"])
    add(
        "translate scale Byte",
        ["gdal_translate", "-ot", "Byte", "-scale", "1000", "4000", "0", "255", dem],
        ["translate", dem, "--ot", "Byte", "--scale", "1000", "4000", "0", "255"],
    )
    add(
        "translate scale exponent",
        ["gdal_translate", "-ot", "UInt16", "-scale", "1000", "4000", "0", "1000", "-exponent", "0.5", dem],
        ["translate", dem, "--ot", "UInt16", "--scale", "1000", "4000", "0", "1000", "--exponent", "0.5"],
    )
    add("translate ot Byte clamp", ["gdal_translate", "-ot", "Byte", eco], ["translate", eco, "--ot", "Byte"])
    add(
        "translate a_nodata a_ullr a_srs",
        ["gdal_translate", "-a_nodata", "0", "-a_ullr", "0", "100", "200", "0", "-a_srs", "EPSG:3857", lv],
        ["translate", lv, "--a-nodata", "0", "--a-ullr", "0", "100", "200", "0", "--a-srs", "EPSG:3857"],
    )
    # gdalwarp
    for method, tol in [("near", 0.0), ("bilinear", 1e-2), ("cubic", 1e-2), ("cubicspline", 1e-2), ("lanczos", 1e-2), ("average", 1e-2), ("mode", 0.0), ("max", 0.0), ("min", 0.0), ("med", 0.0), ("q1", 0.0), ("q3", 0.0), ("rms", 1e-2)]:
        add(f"warp DEM NAD83->UTM {method}", ["gdalwarp", "-t_srs", "EPSG:32613", "-r", method, dem], ["warp", dem, "--t-srs", "EPSG:32613", "-r", method], tol)
    add("warp DEM ->3857 near", ["gdalwarp", "-t_srs", "EPSG:3857", dem], ["warp", dem, "--t-srs", "EPSG:3857"])
    add("warp DEM ->3857 tr 100 bilinear (-ovr NONE)", ["gdalwarp", "-ovr", "NONE", "-t_srs", "EPSG:3857", "-tr", "100", "100", "-r", "bilinear", dem], ["warp", dem, "--t-srs", "EPSG:3857", "--tr", "100", "100", "-r", "bilinear"], 1e-2)
    add("warp DEM ->3857 tr 100 tap average (-ovr NONE)", ["gdalwarp", "-ovr", "NONE", "-t_srs", "EPSG:3857", "-tr", "100", "100", "-tap", "-r", "average", dem], ["warp", dem, "--t-srs", "EPSG:3857", "--tr", "100", "100", "--tap", "-r", "average"], 1e-2)
    add(
        "warp DEM te te_srs ts",
        ["gdalwarp", "-ovr", "NONE", "-t_srs", "EPSG:32613", "-te", "-105.8", "39.2", "-105.2", "39.8", "-te_srs", "EPSG:4326", "-ts", "1000", "1000", dem],
        ["warp", dem, "--t-srs", "EPSG:32613", "--te", "-105.8", "39.2", "-105.2", "39.8", "--te-srs", "EPSG:4326", "--ts", "1000", "1000"],
    )
    add(
        "warp DEM te only",
        ["gdalwarp", "-t_srs", "EPSG:32613", "-te", "430000", "4380000", "470000", "4420000", dem],
        ["warp", dem, "--t-srs", "EPSG:32613", "--te", "430000", "4380000", "470000", "4420000"],
    )
    add(
        "warp DEM proj string lcc",
        ["gdalwarp", "-t_srs", "+proj=lcc +lat_1=33 +lat_2=45 +lat_0=39 +lon_0=-96 +x_0=0 +y_0=0 +datum=WGS84 +units=m", dem],
        ["warp", dem, "--t-srs", "+proj=lcc +lat_1=33 +lat_2=45 +lat_0=39 +lon_0=-96 +x_0=0 +y_0=0 +datum=WGS84 +units=m"],
    )
    add("warp UTM->WGS84 ECOSTRESS near", ["gdalwarp", "-t_srs", "EPSG:4326", eco], ["warp", eco, "--t-srs", "EPSG:4326"])
    add("warp UTM->WGS84 ECOSTRESS bilinear", ["gdalwarp", "-t_srs", "EPSG:4326", "-r", "bilinear", eco], ["warp", eco, "--t-srs", "EPSG:4326", "-r", "bilinear"], 1e-3)
    add("warp landsat ->4326 near", ["gdalwarp", "-t_srs", "EPSG:4326", landsat], ["warp", landsat, "--t-srs", "EPSG:4326"])
    add("warp landsat ->3857 cubic", ["gdalwarp", "-t_srs", "EPSG:3857", "-r", "cubic", landsat], ["warp", landsat, "--t-srs", "EPSG:3857", "-r", "cubic"], 1.0)
    add("warp cog 4326->3857 near", ["gdalwarp", "-t_srs", "EPSG:3857", cog], ["warp", cog, "--t-srs", "EPSG:3857"])
    add("warp las vegas ->4326 srcnodata dstnodata", ["gdalwarp", "-t_srs", "EPSG:4326", "-srcnodata", "0", "-dstnodata", "-1", lv], ["warp", lv, "--t-srs", "EPSG:4326", "--srcnodata", "0", "--dstnodata", "-1"])
    add("warp cutline crop", ["gdalwarp", "-cutline", cut, "-crop_to_cutline", lv], ["warp", lv, "--cutline", cut, "--crop-to-cutline"])
    add("warp cutline reproject", ["gdalwarp", "-t_srs", "EPSG:32611", "-cutline", cut, "-dstnodata", "-9999", lv], ["warp", lv, "--t-srs", "EPSG:32611", "--cutline", cut, "--dstnodata", "-9999"])
    if all(os.path.exists(t) for t in tiles):
        add("warp mosaic 4 tiles ->UTM (-ovr NONE)", ["gdalwarp", "-ovr", "NONE", "-t_srs", "EPSG:32613", "-tr", "90", "90"] + tiles, ["warp"] + tiles + ["--t-srs", "EPSG:32613", "--tr", "90", "90"])
        # Mosaics
        add("mosaic 4 tiles vs gdal_merge", ["gdal_merge.py", "-q"] + tiles[::-1], ["mosaic"] + tiles[::-1])
        add("mosaic vrt vs gdalbuildvrt", ["gdalbuildvrt"] + tiles, ["mosaic"] + tiles, out_ext="vrt")
        add("mosaic separate", ["gdalbuildvrt", "-separate"] + tiles[:2], ["mosaic"] + tiles[:2] + ["--separate"], out_ext="vrt")
        add("mosaic tr nodata", ["gdalbuildvrt", "-tr", "0.001", "0.001", "-srcnodata", "-999999", "-vrtnodata", "-1"] + tiles, ["mosaic"] + tiles + ["--tr", "0.001", "0.001", "--srcnodata", "-999999", "--dstnodata", "-1"], out_ext="vrt")
        add(
            "mosaic tr tap",
            ["gdalbuildvrt", "-tr", "0.0005", "0.0005", "-tap"] + tiles,
            ["mosaic"] + tiles + ["--tr", "0.0005", "0.0005", "--tap"],
            out_ext="vrt",
        )
        # terrio reading VRTs written by GDAL.
        mosaic_vrt = os.path.join(tmp, "gdal_mosaic.vrt")
        run(["gdalbuildvrt", "-q", mosaic_vrt] + tiles)
        add("read GDAL mosaic VRT", ["gdal_translate", mosaic_vrt], ["translate", mosaic_vrt])
    subset_vrt = os.path.join(tmp, "gdal_subset.vrt")
    run(["gdal_translate", "-q", "-of", "VRT", "-srcwin", "10", "20", "300", "200", "-scale", "500", "1500", "0", "1", "-ot", "Float32", lv, subset_vrt])
    add("read GDAL scaled subset VRT", ["gdal_translate", subset_vrt], ["translate", subset_vrt])
    return [c for c in out if c["terrio"] is not None]


def main():
    """Runs the comparisons and prints a report."""
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("data", help="the test data directory")
    ap.add_argument("--terrio", default=os.path.join(os.path.dirname(__file__), "..", "target", "release", "terrio"))
    ap.add_argument("--only", help="only cases whose name contains this")
    ap.add_argument("--json", action="store_true", help="print JSON")
    ap.add_argument("--keep", help="keep the outputs in this directory")
    a = ap.parse_args()
    tmp = a.keep or tempfile.mkdtemp(prefix="terrio_ops_")
    os.makedirs(tmp, exist_ok=True)
    results = []
    for i, c in enumerate(cases(os.path.abspath(a.data), tmp)):
        if a.only and a.only not in c["name"]:
            continue
        gout = os.path.join(tmp, f"case{i}_gdal.{c['ext']}")
        tout = os.path.join(tmp, f"case{i}_terrio.{c['ext']}")
        for p in (gout, tout):
            if os.path.exists(p):
                os.remove(p)
        try:
            run(gdal_command(c["gdal"], gout))
            run(terrio_command(a.terrio, c["terrio"], tout))
            r = compare(gout, tout, c["tol"])
        except Exception as e:  # noqa: BLE001 - report and continue
            r = {"error": str(e).strip().splitlines()[-1][:200]}
        r["name"] = c["name"]
        results.append(r)
        if not a.json:
            print(format_row(r), flush=True)
    if a.json:
        print(json.dumps(results, indent=2))
    if not a.keep:
        shutil.rmtree(tmp, ignore_errors=True)


def gdal_command(g, out):
    """Builds the GDAL command line of a case.

    Args:
        g: The tool and its arguments, without the output.
        out: The output path.

    Returns:
        The command line.
    """
    if g[0] == "gdal_merge.py":
        return [g[0], "-o", out] + g[1:]
    if g[0] == "gdalbuildvrt":
        return [g[0], "-q", out] + g[1:]
    if g[0] == "gdalwarp":
        return [g[0], "-q", "-overwrite"] + g[1:] + [out]
    return [g[0], "-q"] + g[1:] + [out]


def terrio_command(exe, t, out):
    """Builds the terrio command line of a case: the output follows the inputs.

    Args:
        exe: The terrio executable.
        t: The subcommand, its inputs, then its options.
        out: The output path.

    Returns:
        The command line.
    """
    args = t[1:]
    n = next((i for i, x in enumerate(args) if x.startswith("-")), len(args))
    return [exe, t[0]] + args[:n] + [out] + args[n:]


def format_row(r):
    """Formats one result as a line of text.

    Args:
        r: A result from ``compare``.

    Returns:
        The line.
    """
    if "error" in r:
        return f"{r['name']:<42} ERROR {r['error']}"
    if not r["same_size"]:
        return f"{r['name']:<42} size differs (GDAL {r['size']})"
    return (
        f"{r['name']:<42} {r['size']:>14} type={'ok' if r['same_type'] else 'DIFF'} crs={'ok' if r['same_crs'] else 'DIFF'} "
        f"gt={r['gt_diff_px']:.1e}px valid_mismatch={r['valid_mismatch']} exact={100 * r['exact']:.3f}% ({r['differing']} differ) "
        f"tol={100 * r['within_tol']:.2f}% max={r['max_diff']:.3g}"
    )


if __name__ == "__main__":
    sys.exit(main())
