#!/usr/bin/env python3
"""Compare terrio's pixel-wise raster commands with GDAL's on real files.

Runs ``terrio unscale``, ``terrio nodata-to-alpha``, ``terrio reclassify`` and
``terrio as-features`` and the equivalent ``gdal raster unscale``, ``gdal raster
nodata-to-alpha``, ``gdal raster reclassify`` and ``gdal raster as-features`` commands
(GDAL 3.12 or later; the reference is GDAL 3.13.3) on the files fetched by ``scripts/fetch_test_data.sh``, then reports
for each case whether the outputs have the same size, type, nodata value, CRS, band color
interpretations, scales and georeferencing, and how many pixels agree. Feature outputs are compared
feature by feature: count, field names, field values and geometry coordinates.

Usage:
    python scripts/compare_raster_pixel_with_gdal.py ../terrio-data [--terrio target/release/terrio]
"""

import argparse
import json
import os
import shutil
import sys
import tempfile

import numpy as np
from osgeo import gdal, ogr, osr

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from compare_dem_calc_with_gdal import compare, format_row, run  # noqa: E402

gdal.UseExceptions()
ogr.UseExceptions()

S5P = (
    "S5P_OFFL_L2__CH4____20230101T000319_20230101T014449_27036_03_020400_20230103T110255.nc"
)


def prepare(d, tmp):
    """Writes the inputs derived from the real files.

    Args:
        d: The test data directory.
        tmp: The work directory.

    Returns:
        dict: Input paths by name.
    """
    p = {}
    usgs = f"{d}/usgs_dem.tif"
    # Real elevations packed as Int16 decimetres above 1000 m, with GDAL's scale and offset.
    p["packed"] = os.path.join(tmp, "packed.tif")
    ds = gdal.Translate(p["packed"], usgs, srcWin=[1000, 1000, 600, 500], outputType=gdal.GDT_Int16,
                        scaleParams=[[1000, 4276.7, 0, 32767]], noData=-32768)
    a = ds.GetRasterBand(1).ReadAsArray()
    a[:20, :30] = -32768
    ds.GetRasterBand(1).WriteArray(a)
    ds.GetRasterBand(1).SetScale(0.1)
    ds.GetRasterBand(1).SetOffset(1000)
    ds = None
    # A real DEM window shifted beyond Float32's unit precision. GDAL narrows
    # Float64 input before applying the offset when the requested output is Float32.
    p["f64_packed"] = os.path.join(tmp, "f64_packed.tif")
    ds = gdal.Translate(p["f64_packed"], usgs, srcWin=[1000, 1000, 16, 16], outputType=gdal.GDT_Float64)
    band = ds.GetRasterBand(1)
    band.WriteArray(band.ReadAsArray() + 100000000.0)
    band.DeleteNoDataValue()
    band.SetScale(1.0)
    band.SetOffset(-100000000.0)
    ds = None
    # Landsat bands with a scale and offset of their own (reflectance-like), nodata 0.
    p["landsat_scaled"] = os.path.join(tmp, "landsat_scaled.tif")
    ds = gdal.Translate(p["landsat_scaled"], f"{d}/landsat.tif", srcWin=[600, 500, 400, 300])
    for i in range(1, 5):
        ds.GetRasterBand(i).SetScale(2.75e-5 * i)
        ds.GetRasterBand(i).SetOffset(-0.2 + 0.05 * i)
    ds = None
    # Land surface temperature (Float32, NaN nodata) in hundredths of a kelvin.
    p["lst_scaled"] = os.path.join(tmp, "lst_scaled.tif")
    ds = gdal.Translate(p["lst_scaled"], f"{d}/ECOv002_L2T_LSTE_26860_001_10SGD_20230401T203733_0710_01_LST.tif",
                        srcWin=[300, 300, 400, 400])
    ds.GetRasterBand(1).SetScale(0.01)
    ds.GetRasterBand(1).SetOffset(-273.15)
    ds = None
    p["landsat_window"] = os.path.join(tmp, "landsat_window.tif")
    gdal.Translate(p["landsat_window"], f"{d}/landsat.tif", srcWin=[0, 0, 500, 400])
    p["landsat7_window"] = os.path.join(tmp, "landsat7_window.tif")
    gdal.Translate(p["landsat7_window"], f"{d}/landsat7.tif", srcWin=[0, 0, 500, 400])
    p["dem_window"] = os.path.join(tmp, "dem_window.tif")
    gdal.Translate(p["dem_window"], f"{d}/dem.tif", srcWin=[1300, 500, 400, 300])
    p["usgs_window"] = os.path.join(tmp, "usgs_window.tif")
    ds = gdal.Translate(p["usgs_window"], usgs, srcWin=[1500, 1500, 300, 300])
    a = ds.GetRasterBand(1).ReadAsArray()
    a[100:140, 50:90] = -999999
    ds.GetRasterBand(1).WriteArray(a)
    ds = None
    # UInt16 with nodata, from the Landsat scene.
    p["landsat_u16"] = os.path.join(tmp, "landsat_u16.tif")
    gdal.Translate(p["landsat_u16"], p["landsat_window"], outputType=gdal.GDT_UInt16, scaleParams=[[0, 255, 0, 65535]],
                   noData=0)
    p["nlcd"] = f"{d}/nlcd_knoxville.tif"
    p["nlcd_nodata"] = os.path.join(tmp, "nlcd_nodata.tif")
    ds = gdal.Translate(p["nlcd_nodata"], p["nlcd"], noData=11)
    ds = None
    p["usgs_small"] = os.path.join(tmp, "usgs_small.tif")
    ds = gdal.Translate(p["usgs_small"], usgs, srcWin=[2000, 2000, 60, 45])
    a = ds.GetRasterBand(1).ReadAsArray()
    a[:5, :7] = -999999
    ds.GetRasterBand(1).WriteArray(a)
    ds = None
    p["landsat_small"] = os.path.join(tmp, "landsat_small.tif")
    gdal.Translate(p["landsat_small"], f"{d}/landsat.tif", srcWin=[0, 0, 50, 40])
    p["s5p_qa"] = f'NETCDF:"{d}/{S5P}":/PRODUCT/qa_value'
    p["float_nodata"] = os.path.join(tmp, "float_nodata.tif")
    ds = gdal.Translate(p["float_nodata"], p["usgs_small"], outputType=gdal.GDT_Float32, noData=0.1)
    values = ds.ReadAsArray()
    values[0, 0] = 0.1
    values[0, 1] = np.nextafter(np.float32(0.1), np.float32(1))
    values[0, 2] = np.float32(0.1) + 4 * np.spacing(np.float32(0.1))
    ds.GetRasterBand(1).WriteArray(values)
    ds = None
    mapping = os.path.join(tmp, "nlcd_mapping.txt")
    with open(mapping, "w") as f:
        f.write("# NLCD classes to groups\n11=1  # water\n[21,25)=2\n31=3\n[41,44)=4 # forest\n"
                "52=5\n71=5\n[81,83)=6\n90=7\n95=7\nDEFAULT=0\n")
    p["nlcd_mapping"] = mapping
    return p


def cases(d, tmp):
    """The comparison cases.

    Args:
        d: The test data directory.
        tmp: The work directory.

    Returns:
        list: Dicts with the case name, the GDAL and terrio command lines (``{out}`` standing for
        the output path), the tolerance and the output extension.
    """
    p = prepare(d, tmp)
    out = []

    def add(name, gdal_cmd, terrio_cmd, tol=0.0, ext="tif", kind="raster"):
        out.append({"name": name, "gdal": gdal_cmd, "terrio": terrio_cmd, "tol": tol, "ext": ext, "kind": kind})

    def unscale(name, src, ot=None):
        extra = ["--ot", ot] if ot else []
        add(f"unscale {name}", ["gdal", "raster", "unscale", "-q", src, "{out}", *extra], ["unscale", src, "{out}", *extra])

    unscale("Int16 elevation", p["packed"])
    unscale("Int16 elevation Float64", p["packed"], "Float64")
    unscale("Int16 elevation UInt16", p["packed"], "UInt16")
    unscale("Landsat 4 bands", p["landsat_scaled"])
    unscale("Landsat Byte", p["landsat_scaled"], "Byte")
    unscale("LST Float32 NaN", p["lst_scaled"])
    unscale("LST Int32", p["lst_scaled"], "Int32")
    unscale("S5P qa_value netCDF", p["s5p_qa"])
    out[-1]["requires"] = "netCDF"
    unscale("unscaled DEM Float64", p["dem_window"])
    unscale("Float64 packed DEM to Float32", p["f64_packed"], "Float32")

    def alpha(name, src, nodata=()):
        g = [x for v in nodata for x in ("--nodata", str(v))]
        add(f"nodata-to-alpha {name}", ["gdal", "raster", "nodata-to-alpha", "-q", src, "{out}", *g],
            ["nodata-to-alpha", src, "{out}", *g])

    alpha("Landsat 4 bands", p["landsat_window"])
    alpha("Landsat 7 RGB", p["landsat7_window"])
    alpha("Landsat UInt16", p["landsat_u16"])
    alpha("DEM Float64 NaN", p["dem_window"])
    alpha("USGS Float32", p["usgs_window"])
    alpha("one override", p["landsat7_window"], [12])
    alpha("per-band overrides", p["landsat7_window"], [0, 0, 0])
    alpha("no nodata", f"{d}/srtm90.tif")
    alpha("packed gray scale", p["packed"])

    def reclass(name, src, mapping, ot=None):
        extra = ["--ot", ot] if ot else []
        add(f"reclassify {name}", ["gdal", "raster", "reclassify", "-q", src, "{out}", "-m", mapping, *extra],
            ["reclassify", src, "{out}", "-m", mapping, *extra])

    reclass("NLCD groups file", p["nlcd"], "@" + p["nlcd_mapping"])
    reclass("NLCD NO_DATA", p["nlcd_nodata"], "NO_DATA=NO_DATA;[20,30)=1;DEFAULT=PASS_THROUGH")
    reclass("elevation ranges Byte", p["usgs_window"],
            "NO_DATA=0;[0,1500)=1;[1500,2000)=2;[2000,2500)=3;[2500,inf)=4", "Byte")
    reclass("elevation pass-through", p["usgs_window"], "[2000,2500]=0;DEFAULT=PASS_THROUGH")
    reclass("LST NaN and ranges", p["lst_scaled"], "NAN=-1;(-inf,29000)=1;[29000,30000)=2;[30000,inf]=3", "Int16")
    reclass("Landsat bands", p["landsat_window"], "0=NO_DATA;[1,100)=50;[100,255]=PASS_THROUGH")

    def features(name, src, opts, ext="gpkg"):
        # GDAL 3.12 names the layer "" by default (3.13 names it "pixels"): name it for both.
        opts = [*opts, "--output-layer", "pixels"]
        add(f"as-features {name}", ["gdal", "raster", "as-features", src, "{out}", *opts],
            ["as-features", src, "{out}", *opts], ext=ext, kind="vector")

    features("polygons xy row-col", p["usgs_small"], ["--geometry-type", "polygon", "--include-xy", "--include-row-col"])
    features("points skip-nodata", p["usgs_small"], ["--geometry-type", "point", "--skip-nodata"])
    features("table Landsat bands", p["landsat_small"], ["--include-row-col"], ext="csv")
    features("Landsat polygons skip-nodata", p["landsat_small"], ["--geometry-type", "polygon", "--skip-nodata"])
    features("Float32 fractional skip-nodata", p["float_nodata"], ["--skip-nodata"])
    features("duplicate bands", p["usgs_small"], ["-b", "1", "-b", "1"])
    return out


def raster_extra(gout, tout, allow_s5p_default=False):
    """Compares what ``compare`` leaves out: CRS, color interpretations, scales and offsets.

    Args:
        gout: GDAL's output.
        tout: terrio's output.

    Returns:
        dict: ``same_crs`` and ``same_metadata``.
    """
    g, t = gdal.Open(gout), gdal.Open(tout)
    gs, ts = osr.SpatialReference(), osr.SpatialReference()
    same_crs = (not g.GetProjection() and not t.GetProjection()) or (
        bool(g.GetProjection()) and bool(t.GetProjection()) and gs.ImportFromWkt(g.GetProjection()) == 0
        and ts.ImportFromWkt(t.GetProjection()) == 0 and bool(gs.IsSame(ts))
    )

    def props(ds):
        out = []
        for i in range(1, ds.RasterCount + 1):
            b = ds.GetRasterBand(i)
            scale, offset = b.GetScale(), b.GetOffset()
            out.append((gdal.GetColorInterpretationName(b.GetColorInterpretation()),
                        1.0 if scale is None else scale, 0.0 if offset is None else offset))
        return out

    gp, tp = props(g), props(t)
    ggt = g.GetGeoTransform(can_return_null=True)
    tgt = t.GetGeoTransform(can_return_null=True)
    res = {"same_crs": same_crs, "same_metadata": gp == tp,
           "same_georeferencing": ggt == tgt}
    if ggt != tgt:
        res["georeferencing"] = {"gdal": ggt, "terrio": tgt}
        if (allow_s5p_default and ggt is None
                and tgt == (0.0, 1.0, 0.0, float(t.RasterYSize), 0.0, -1.0)):
            res["accepted_georeferencing_difference"] = (
                "Sentinel-5P: GDAL writes no affine transform; terrio writes its default grid"
            )
    if gp != tp:
        res["metadata"] = f"GDAL {gp}, terrio {tp}"
    return res


def read_layer(path):
    """Reads a vector output's single layer.

    Args:
        path: The dataset.

    Returns:
        tuple: Field names/types, then per feature its field values and geometry coordinates.
    """
    ds = gdal.OpenEx(path, gdal.OF_VECTOR)
    layer = ds.GetLayer(0)
    defn = layer.GetLayerDefn()
    names = [(defn.GetFieldDefn(i).GetName(), defn.GetFieldDefn(i).GetTypeName())
             for i in range(defn.GetFieldCount())]
    feats = []
    for f in layer:
        values = [f.GetField(i) for i in range(len(names))]
        g = f.GetGeometryRef()
        coords = []
        if g is not None:
            g = g.Clone()
            g.FlattenTo2D()
            if g.GetGeometryType() == ogr.wkbPolygon:
                coords = list(g.GetGeometryRef(0).GetPoints())
            else:
                coords = [(g.GetX(), g.GetY())]
        feats.append((values, coords))
    srs = layer.GetSpatialRef()
    return names, feats, layer.GetName(), srs


def compare_vector(gout, tout):
    """Compares two vector outputs feature by feature.

    Args:
        gout: GDAL's output.
        tout: terrio's output.

    Returns:
        dict: Comparison results in the raster comparison's terms (``differing`` features).
    """
    gn, gf, gl, gs = read_layer(gout)
    tn, tf, tl, ts = read_layer(tout)
    res = {"size": f"{len(gf)} features", "same_size": len(gf) == len(tf), "type": ",".join(f"{name}:{kind}" for name, kind in gn),
           "same_type": gn == tn,
           "same_crs": (gs is None and ts is None) or (gs is not None and ts is not None and bool(gs.IsSame(ts))),
           "same_metadata": gl == tl or gout.endswith(".csv")}
    if not res["same_size"] or not res["same_type"]:
        return res
    differing, max_diff = 0, 0.0
    for (gv, gc), (tv, tc) in zip(gf, tf):
        diff = 0.0
        same = len(gc) == len(tc)
        for a, b in zip(gv, tv):
            a, b = float(a), float(b)
            if not (a == b or (np.isnan(a) and np.isnan(b))):
                same = False
                diff = max(diff, abs(a - b))
        for a, b in zip(gc, tc):
            if a != b:
                same = False
                diff = max(diff, abs(a[0] - b[0]), abs(a[1] - b[1]))
        differing += not same
        max_diff = max(max_diff, diff)
    n = max(len(gf), 1)
    res.update({"differing": differing, "exact": 1 - differing / n, "within_tol": 1 - differing / n,
                "max_diff": max_diff, "inf_diff": 0})
    return res


def main():
    """Runs the cases and reports them."""
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("data")
    ap.add_argument("--terrio", default="target/release/terrio")
    ap.add_argument("--only", help="run the cases whose name contains this")
    ap.add_argument("--json", action="store_true", help="print the results as JSON")
    ap.add_argument("--keep", help="keep the outputs in this directory")
    a = ap.parse_args()
    tmp = a.keep or tempfile.mkdtemp(prefix="terrio_pixel_")
    os.makedirs(tmp, exist_ok=True)
    results = []
    try:
        for i, c in enumerate(cases(a.data, tmp)):
            if a.only and a.only not in c["name"]:
                continue
            gout = os.path.join(tmp, f"gdal_{i}.{c['ext']}")
            tout = os.path.join(tmp, f"terrio_{i}.{c['ext']}")
            for path in (gout, tout):
                if os.path.exists(path):
                    os.remove(path)
            try:
                if c.get("requires") and gdal.GetDriverByName(c["requires"]) is None:
                    raise RuntimeError(f"reference case requires GDAL's {c['requires']} driver")
                run([x.replace("{out}", gout) for x in c["gdal"]])
                run([a.terrio] + [x.replace("{out}", tout) for x in c["terrio"]])
                if c["kind"] == "vector":
                    r = compare_vector(gout, tout)
                else:
                    r = compare(gout, tout, c["tol"])
                    r.update(raster_extra(gout, tout, c["name"] == "unscale S5P qa_value netCDF"))
            except Exception as e:  # noqa: BLE001 - report and continue
                r = {"error": str(e).strip().splitlines()[-1][:300]}
            r["name"] = c["name"]
            results.append(r)
            if not a.json:
                if c["kind"] == "vector" and "error" not in r:
                    line = (f"{r['name']:<58} {r['size']} count={'ok' if r['same_size'] else 'DIFF'} "
                            f"fields={'ok' if r['same_type'] else 'DIFF'}")
                    if "differing" in r:
                        line += f" exact={100 * r['exact']:.4f}% ({r['differing']} features differ) max={r['max_diff']:.3g}"
                else:
                    line = format_row(r)
                if "error" not in r:
                    line += f" crs={'ok' if r.get('same_crs') else 'DIFF'} meta={'ok' if r.get('same_metadata') else 'DIFF'}"
                    if "same_georeferencing" in r:
                        line += f" georef={'ok' if r['same_georeferencing'] else 'DIFF'}"
                    if r.get("accepted_georeferencing_difference"):
                        line += f" ({r['accepted_georeferencing_difference']})"
                    if r.get("metadata"):
                        line += f" ({r['metadata']})"
                print(line, flush=True)
    finally:
        if not a.keep:
            shutil.rmtree(tmp, ignore_errors=True)
    if a.json:
        print(json.dumps(results, indent=1))
    flags = ("same_type", "same_nodata", "same_crs", "same_metadata")
    bad = [
        r for r in results
        if "error" in r or not r.get("same_size") or r.get("differing")
        or any(r.get(k) is False for k in flags)
        or (r.get("same_georeferencing") is False
            and not r.get("accepted_georeferencing_difference"))
    ]
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
