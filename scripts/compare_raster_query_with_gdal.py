#!/usr/bin/env python3
"""Compare terrio compare, pixel-info and xyz with GDAL on real rasters.

``terrio compare`` is run against ``gdal raster compare -q`` on pairs of real files and
derived copies (types, nodata, edited pixels, overviews, internal masks, sizes, CRS,
metadata): the reports must have the same lines (apart from two documented
differences, normalized here: CRS WKT text, and GDAL swapping the two values in its
metadata value message) and the same exit status.

``terrio pixel-info`` is compared with ``gdal raster pixel-info`` (GeoJSON and CSV,
values parsed and compared exactly) and with ``gdallocationinfo`` (text report and
``-valonly``, byte for byte), for pixel, dataset and WGS 84 positions, the four
interpolations, nodata, scale/offset, overviews, several bands, standard input with
extra text and positions outside the raster. Positions read from a vector dataset
(GDAL 3.13's ``--position-dataset``, absent from 3.12) are compared with GDAL 3.12
queried at the same coordinates.

``terrio xyz`` is compared with ``gdal2xyz.py`` (and its ``gdal2xyz`` function for
``skip``, which the 3.12 script's command line cannot pass) byte for byte.

Prints one line per case, or a JSON list with ``--json``.
"""

import argparse
import csv
import io
import json
import re
import subprocess
import sys
import tempfile
from pathlib import Path

import numpy as np
from osgeo import gdal

gdal.UseExceptions()

CRS_LINE = re.compile(r"^Reference and input CRS are not equivalent\. ")
NL = b"\n"
VALUE_LINE = re.compile(r"^Reference metadata (.*) has value '(.*)' for key '(.*)' but input metadata has value '(.*)'\.$")


def run(cmd, stdin=None):
    """Runs a command.

    Args:
        cmd: The command line.
        stdin: Text for its standard input.

    Returns:
        A ``(returncode, stdout, stderr)`` tuple.
    """
    try:
        p = subprocess.run(cmd, input=stdin, capture_output=True, text=True, check=False, timeout=300)
    except subprocess.TimeoutExpired:
        return 124, "", "command timed out after 300 seconds"
    return p.returncode, p.stdout, p.stderr


def normalize_report(text, swap):
    """Normalizes the two documented differences of compare reports.

    Args:
        text: The report.
        swap: Whether metadata value messages name the values the other way round (GDAL's).

    Returns:
        The report lines, with CRS WKT text dropped and metadata values in reference/input order.
    """
    out = []
    for line in text.splitlines():
        if not line.strip() or line.startswith(("Warning", "0...")):
            continue
        if CRS_LINE.match(line):
            out.append("Reference and input CRS are not equivalent.")
            continue
        m = VALUE_LINE.match(line)
        if m and swap:
            domain, a, key, b = m.groups()
            line = f"Reference metadata {domain} has value '{b}' for key '{key}' but input metadata has value '{a}'."
        out.append(line)
    return out


def compare_cases(data, work, terrio):
    """Runs terrio compare and gdal raster compare on real pairs.

    Args:
        data: The test data directory.
        work: A scratch directory.
        terrio: The terrio executable.

    Returns:
        A list of result dicts.
    """
    dem = str(data / "dem.tif")
    landsat = str(data / "landsat.tif")
    w = lambda name: str(work / name)  # noqa: E731
    gdal.Translate(w("dem32.tif"), dem, outputType=gdal.GDT_Float32)
    gdal.Translate(w("dem8.tif"), dem, outputType=gdal.GDT_Byte, scaleParams=[[0, 4000, 0, 255]])
    gdal.Translate(w("dem_lzw.tif"), dem, creationOptions=["COMPRESS=LZW"])
    # Edited pixels: a block of valid elevations raised by 1.5 m and one set to NaN.
    ds = gdal.Translate(w("dem_edit.tif"), dem)
    a = ds.GetRasterBand(1).ReadAsArray()
    rows, cols = np.nonzero(np.isfinite(a))
    r, c = rows[len(rows) // 2], cols[len(cols) // 2]
    a[r : r + 20, c : c + 100] += 1.5
    a[rows[0], cols[0]] = np.nan
    ds.GetRasterBand(1).WriteArray(a)
    ds = None
    gdal.Translate(w("dem_win.tif"), dem, srcWin=[100, 100, 1000, 800])
    gdal.Translate(w("dem_win_shift.tif"), dem, srcWin=[101, 100, 1000, 800])
    usgs = str(data / "USGS_1_n40w106.tif")
    # The source's own overviews are left out, so that the COG driver builds them.
    gdal.Translate(w("usgs_plain.tif"), usgs, creationOptions=["TILED=YES", "COPY_SRC_OVERVIEWS=NO"])
    gdal.Translate(w("usgs_cog_near.tif"), w("usgs_plain.tif"), format="COG", creationOptions=["OVERVIEW_RESAMPLING=NEAREST"])
    gdal.Translate(w("usgs_cog_avg.tif"), w("usgs_plain.tif"), format="COG", creationOptions=["OVERVIEW_RESAMPLING=AVERAGE"])
    # Internal masks: landsat's first three bands with a per-dataset mask, and a second mask.
    for name, cut in (("mask_a.tif", 0), ("mask_b.tif", 40)):
        ds = gdal.Translate(w(name), landsat, bandList=[1, 2, 3], noData="none", creationOptions=["TILED=YES"])
        ds.CreateMaskBand(gdal.GMF_PER_DATASET)
        m = (ds.GetRasterBand(1).ReadAsArray() > cut).astype(np.uint8) * 255
        ds.GetRasterBand(1).GetMaskBand().WriteArray(m)
        ds = None
    gdal.Translate(w("nomask.tif"), landsat, bandList=[1, 2, 3], noData="none", creationOptions=["TILED=YES"])
    gdal.Translate(w("rgba.tif"), landsat, bandList=[1, 2, 3, 4], noData="none",
                   creationOptions=["PHOTOMETRIC=RGB", "ALPHA=YES"])
    cases = [
        ("same file", dem, dem, []),
        ("Float32 copy", dem, w("dem32.tif"), []),
        ("Byte scaled copy", dem, w("dem8.tif"), []),
        ("recompressed (binary only)", dem, w("dem_lzw.tif"), []),
        ("edited pixels", dem, w("dem_edit.tif"), ["--skip-binary"]),
        ("edited pixels, all optional skipped", dem, w("dem_edit.tif"), ["--skip-all-optional"]),
        ("different sizes", dem, w("dem_win.tif"), []),
        ("shifted window", w("dem_win.tif"), w("dem_win_shift.tif"), ["--skip-binary", "--skip-metadata"]),
        ("COG overview resampling", w("usgs_cog_near.tif"), w("usgs_cog_avg.tif"), ["--skip-binary"]),
        ("overviews vs none", w("usgs_cog_near.tif"), w("usgs_plain.tif"), []),
        ("overviews skipped", w("usgs_cog_near.tif"), w("usgs_plain.tif"), ["--skip-overview", "--skip-binary"]),
        ("internal masks", w("mask_a.tif"), w("mask_b.tif"), ["--skip-binary"]),
        ("mask vs none", w("mask_a.tif"), w("nomask.tif"), ["--skip-binary"]),
        ("RGBA alpha vs bands", w("rgba.tif"), landsat, ["--skip-binary"]),
        ("multiband", landsat, str(data / "landsat7.tif"), []),
        ("geographic vs projected", str(data / "cog.tif"), landsat, []),
        ("CRS skipped", str(data / "cog.tif"), landsat, ["--skip-crs", "--skip-geotransform"]),
        ("UTM rasters", str(data / "OPERA_L3_DSWx_WTR.tif"),
         str(data / "ECOv002_L2T_LSTE_26860_001_10SGD_20230401T203733_0710_01_LST.tif"), []),
        ("adjacent DEM tiles", usgs, str(data / "USGS_1_n40w107.tif"), []),
        ("metadata skipped", usgs, str(data / "USGS_1_n40w107.tif"), ["--skip-metadata"]),
        ("netCDF subdatasets", f'NETCDF:"{data / "wind_global.nc"}":u_wind', f'NETCDF:"{data / "wind_global.nc"}":v_wind', []),
    ]
    results = []
    for name, ref, inp, opts in cases:
        gcode, gout, gerr = run(["gdal", "raster", "compare", "-q", *opts, ref, inp])
        tcode, tout, terr = run([terrio, "compare", *opts, ref, inp])
        g = normalize_report(gout, swap=True)
        t = normalize_report(tout, swap=False)
        status = "identical" if (g, gcode) == (t, tcode) else "differs"
        detail = ""
        if status != "identical":
            detail = f"gdal exit {gcode}, terrio exit {tcode}; gdal-only: {sorted(set(g) - set(t))[:3]}, terrio-only: {sorted(set(t) - set(g))[:3]} {terr.strip()[:200]}"
        else:
            detail = f"{len(t)} difference(s)"
        # The JSON report's per-band counts must agree with the text report.
        jcode, jout, _ = run([terrio, "compare", "--json", *opts, ref, inp])
        if jcode == tcode and jout.strip():
            j = json.loads(jout)
            for b in j["bands"]:
                if b["pixels_differing"]:
                    line = f"{b['band']}: pixels differing: {b['pixels_differing']}"
                    if b.get("subdataset") is None and line not in t:
                        status, detail = "differs", f"JSON band counts disagree with the report: {line}"
            if j["difference_count"] != len(t):
                status, detail = "differs", "JSON difference count disagrees with the report"
        results.append({"operation": "compare", "name": f"compare {name}", "status": status, "detail": detail})
    return results


def geojson_values(doc):
    """The comparable content of a pixel-info GeoJSON document.

    Args:
        doc: The parsed GeoJSON.

    Returns:
        A list of ``(properties, geometry)`` per feature, and the CRS member.
    """
    feats = [(f["properties"], f["geometry"]) for f in doc["features"]]
    return feats, doc.get("crs")


def close(a, b, tol):
    """Compares two JSON values, numbers within a relative tolerance.

    Args:
        a: A value.
        b: Another value.
        tol: The relative tolerance for floats (0 for exact).

    Returns:
        Whether they agree.
    """
    if isinstance(a, dict) and isinstance(b, dict):
        return a.keys() == b.keys() and all(close(a[k], b[k], tol) for k in a)
    if isinstance(a, (list, tuple)) and isinstance(b, (list, tuple)):
        return len(a) == len(b) and all(close(x, y, tol) for x, y in zip(a, b))
    if isinstance(a, (int, float)) and isinstance(b, (int, float)) and not isinstance(a, bool):
        if a != a and b != b:  # NaN in both
            return True
        return a == b or abs(a - b) <= tol * max(abs(a), abs(b), 1.0)
    return a == b


def max_rel_diff(a, b):
    """The largest relative difference between the numbers of two JSON values of the same shape.

    Args:
        a: A value.
        b: Another value.

    Returns:
        The largest ``|x - y| / max(|x|, |y|, 1)``.
    """
    if isinstance(a, dict) and isinstance(b, dict):
        return max((max_rel_diff(a[k], b[k]) for k in a if k in b), default=0.0)
    if isinstance(a, (list, tuple)) and isinstance(b, (list, tuple)):
        return max((max_rel_diff(x, y) for x, y in zip(a, b)), default=0.0)
    if isinstance(a, (int, float)) and isinstance(b, (int, float)) and not isinstance(a, bool):
        if a != a or b != b or a == b:
            return 0.0
        return abs(a - b) / max(abs(a), abs(b), 1.0)
    return 0.0


def csv_rows(text, gdal_output):
    """Parses pixel-info CSV, mapping GDAL 3.12's columns to GDAL 3.13's.

    Args:
        text: The CSV text.
        gdal_output: GDAL output; accepts legacy 3.12 column names when present.

    Returns:
        A list of dicts of floats (strings for extra content, None for empty fields).
    """
    rename = {"input_x": "geom_x", "input_y": "geom_y", "extra_input": "extra_content"} if gdal_output else {}
    rows = []
    for row in csv.DictReader(io.StringIO(text)):
        out = {}
        for k, v in row.items():
            k = rename.get(k, k)
            if k == "extra_content":
                out[k] = v or None
            elif v in ("", None):
                out[k] = None
            else:
                # GDAL 3.12 writes 17 significant digits, 3.13 (OGR's CSV driver) 15.
                out[k] = float(f"{float(v):.15g}")
        rows.append(out)
    return rows


def pixel_info_cases(data, work, terrio):
    """Runs terrio pixel-info against gdal raster pixel-info and gdallocationinfo.

    Args:
        data: The test data directory.
        work: A scratch directory.
        terrio: The terrio executable.

    Returns:
        A list of result dicts.
    """
    dem = str(data / "dem.tif")
    usgs = str(data / "USGS_1_n40w106.tif")
    landsat = str(data / "landsat.tif")
    nlcd = str(data / "nlcd_knoxville.tif")
    opera = str(data / "OPERA_L3_DSWx_WTR.tif")
    scaled = str(work / "scaled.tif")
    gdal.Translate(scaled, usgs, srcWin=[1000, 1000, 300, 300], outputType=gdal.GDT_Int16, scaleParams=[[1500, 4500, 0, 30000]])
    ds = gdal.Open(scaled, gdal.GA_Update)
    ds.GetRasterBand(1).SetScale(0.1)
    ds.GetRasterBand(1).SetOffset(1500)
    ds = None
    results = []

    def add(name, status, detail=""):
        results.append({"operation": "pixel-info", "name": f"pixel-info {name}", "status": status, "detail": detail})

    # (name, raster, positional values, options for both, gdallocationinfo options)
    queries = [
        ("pixel nearest", dem, ["100.3", "200.7", "2879.9", "1772.2", "5000", "10", "-1", "3"], [], []),
        ("pixel bilinear", dem, ["100.3", "200.7", "0.2", "0.2", "2879.9", "1772.9"], ["-r", "bilinear"], ["-r", "bilinear"]),
        ("pixel cubic", usgs, ["10.25", "3000.5", "1.1", "1.1", "3610.9", "3610.9"], ["-r", "cubic"], ["-r", "cubic"]),
        ("pixel cubicspline", usgs, ["10.25", "3000.5", "1800.5", "2.4"], ["-r", "cubicspline"], ["-r", "cubicspline"]),
        ("nodata nearest and bilinear", dem, ["10", "10", "1500.5", "1500.5"], ["-r", "bilinear"], ["-r", "bilinear"]),
        ("dataset coordinates", usgs, ["-105.6", "39.4", "-105.9", "39.95"], ["--position-crs", "dataset"], ["-geoloc"]),
        ("WGS 84 to Pseudo-Mercator", dem, ["-105.5", "39.5", "-104.9", "39.7"], ["--position-crs", "WGS84"], ["-wgs84"]),
        ("WGS 84 to custom Albers", nlcd, ["-83.95", "35.95", "-84.1", "36.0"], ["--position-crs", "WGS84"], ["-wgs84"]),
        ("WGS 84 to UTM, bilinear", opera, ["-114.8", "36.1"], ["--position-crs", "EPSG:4326", "-r", "bilinear"],
         ["-l_srs", "EPSG:4326", "-r", "bilinear"]),
        ("selected bands", landsat, ["300", "400", "1200", "700"], ["-b", "3", "-b", "1"], ["-b", "3", "-b", "1"]),
        ("all bands", landsat, ["300", "400"], [], []),
        ("overview", usgs, ["1000", "2000", "3600", "3600"], ["--overview", "1"], ["-overview", "2"]),
        ("overview bilinear", usgs, ["1000.7", "2000.2"], ["--overview", "0", "-r", "bilinear"], ["-overview", "1", "-r", "bilinear"]),
        ("scale and offset", scaled, ["10", "10", "150.5", "20.25"], [], []),
        ("scale and offset, cubic", scaled, ["150.5", "20.25"], ["-r", "cubic"], ["-r", "cubic"]),
        ("palette", opera, ["1000", "1000", "3659", "0"], [], []),
    ]
    for name, raster, pos, opts, li_opts in queries:
        # GeoJSON.
        gc, go, ge = run(["gdal", "raster", "pixel-info", *opts, raster, *pos])
        tc, to, te = run([terrio, "pixel-info", *opts, raster, *pos])
        if gc or tc:
            add(f"{name} (GeoJSON)", "differs", f"exit {gc}/{tc}: {ge.strip()[:120]} {te.strip()[:120]}")
        else:
            g, t = geojson_values(json.loads(go)), geojson_values(json.loads(to))
            if close(g, t, 0):
                add(f"{name} (GeoJSON)", "identical")
            elif close(g, t, 1e-9):
                add(f"{name} (GeoJSON)", "within tolerance", f"numbers within {max_rel_diff(g, t):.1e} relative")
            else:
                add(f"{name} (GeoJSON)", "differs", f"gdal {g!r:.300} terrio {t!r:.300}")
        # CSV.
        gc, go, _ = run(["gdal", "raster", "pixel-info", "--of", "csv", *opts, raster, *pos])
        tc, to, _ = run([terrio, "pixel-info", "-f", "csv", *opts, raster, *pos])
        if gc or tc:
            add(f"{name} (CSV)", "differs", f"exit {gc}/{tc}")
            continue
        g, t = csv_rows(go, True), csv_rows(to, False)
        if close(g, t, 0):
            add(f"{name} (CSV)", "identical")
        elif close(g, t, 1e-9):
            add(f"{name} (CSV)", "within tolerance", f"numbers within {max_rel_diff(g, t):.1e} relative")
        else:
            add(f"{name} (CSV)", "differs", f"gdal {g!r:.300} terrio {t!r:.300}")
        # gdallocationinfo text and -valonly, byte for byte.
        for fmt, extra in (("text", []), ("value", ["-valonly"])):
            positions = "".join(f"{pos[i]} {pos[i + 1]}\n" for i in range(0, len(pos), 2))
            gc, go, _ = run(["gdallocationinfo", *li_opts, *extra, raster], positions)
            tc, to, _ = run([terrio, "pixel-info", "-f", fmt, *opts, raster], positions)
            ok = go == to and gc == tc
            add(f"{name} ({'gdallocationinfo' if fmt == 'text' else '-valonly'})", "identical" if ok else "differs",
                "" if ok else f"exit {gc}/{tc}: {go!r:.200} vs {to!r:.200}")
    # Standard input, with extra text after the coordinates.
    stdin = "-105.6 39.4 first site\n-105.2 39.1\n-105.9 39.95 third, quoted \"x\"\n"
    gc, go, _ = run(["gdal", "raster", "pixel-info", "--position-crs", "WGS84", usgs], stdin)
    tc, to, _ = run([terrio, "pixel-info", "--position-crs", "WGS84", usgs], stdin)
    if gc or tc:
        add("standard input (GeoJSON)", "differs", f"exit {gc}/{tc}")
    else:
        g, t = geojson_values(json.loads(go)), geojson_values(json.loads(to))
        add("standard input (GeoJSON)", "identical" if close(g, t, 0) else ("within tolerance" if close(g, t, 1e-9) else "differs"),
            "" if g == t else f"{g!r:.200} vs {t!r:.200}")
    gc, go, _ = run(["gdallocationinfo", "-wgs84", usgs], stdin)
    tc, to, _ = run([terrio, "pixel-info", "-f", "text", "--position-crs", "WGS84", usgs], stdin)
    add("standard input (gdallocationinfo)", "identical" if (go, gc) == (to, tc) else "differs", "" if go == to else f"{go!r:.200} vs {to!r:.200}")
    # Positions from a vector dataset (GDAL 3.13), against GDAL 3.12 at the same coordinates.
    pts = work / "points.geojson"
    coords = [(-105.6, 39.4), (-105.2, 39.1), (-105.9, 39.95), (-106.5, 39.5)]
    pts.write_text(json.dumps({
        "type": "FeatureCollection",
        "features": [{"type": "Feature", "properties": {"id": i, "name": f"p{i}"},
                      "geometry": {"type": "Point", "coordinates": list(c)}} for i, c in enumerate(coords)],
    }))
    pos = [str(v) for c in coords for v in c]
    gc, go, _ = run(["gdal", "raster", "pixel-info", "--position-crs", "EPSG:4326", "-r", "bilinear", usgs, *pos])
    tc, to, te = run([terrio, "pixel-info", "--position-dataset", str(pts), "-r", "bilinear", usgs])
    if gc or tc:
        add("vector positions", "differs", f"exit {gc}/{tc}: {te.strip()[:200]}")
    else:
        g = [(f["properties"]["column"], f["properties"]["line"], f["properties"]["bands"]) for f in json.loads(go)["features"]]
        tf = json.loads(to)["features"]
        t = [(f["properties"]["column"], f["properties"]["line"], f["properties"]["bands"]) for f in tf]
        fields_ok = [f["properties"].get("name") for f in tf] == [f"p{i}" for i in range(len(coords))]
        add("vector positions", "identical" if g == t and fields_ok else ("within tolerance" if close(g, t, 1e-9) and fields_ok else "differs"),
            "" if g == t else f"{g!r:.200} vs {t!r:.200}")
    return results


def xyz_cases(data, work, terrio):
    """Runs terrio xyz against gdal2xyz.py.

    Args:
        data: The test data directory.
        work: A scratch directory.
        terrio: The terrio executable.

    Returns:
        A list of result dicts.
    """
    from osgeo_utils.gdal2xyz import gdal2xyz

    usgs = str(data / "USGS_1_n40w106.tif")
    small = str(work / "usgs_small.tif")
    gdal.Translate(small, usgs, srcWin=[1200, 1500, 120, 90])
    lsmall = str(work / "landsat_small.tif")
    gdal.Translate(lsmall, str(data / "landsat.tif"), srcWin=[0, 0, 200, 150])
    osmall = str(work / "opera_small.tif")
    # The scene's right edge, where nodata (255) begins.
    gdal.Translate(osmall, str(data / "OPERA_L3_DSWx_WTR.tif"), srcWin=[3560, 700, 100, 160])
    i32 = str(work / "int32.tif")
    gdal.Translate(i32, small, outputType=gdal.GDT_Int32)
    dem = str(data / "dem.tif")
    ds = gdal.Open(lsmall)
    values, counts = np.unique(ds.GetRasterBand(1).ReadAsArray(), return_counts=True)
    ds = None
    common = str(int(values[counts.argmax()]))
    results = []
    # (name, src, gdal2xyz.py options, terrio options)
    cases = [
        ("geographic Float32", small, [], []),
        ("projected, all bands", lsmall, ["-allbands"], ["--allbands"]),
        ("selected bands, CSV", lsmall, ["-b", "3", "1", "-csv"], ["-b", "3", "-b", "1", "--csv"]),
        ("source window", usgs, ["-srcwin", "100", "200", "40", "30"], ["--srcwin", "100", "200", "40", "30"]),
        ("skip nodata", lsmall, ["-allbands", "-skipnodata"], ["--allbands", "--skipnodata"]),
        ("palette, skip nodata", osmall, ["-skipnodata"], ["--skipnodata"]),
        ("replace nodata", osmall, ["-dstnodata", "200"], ["--dstnodata", "200"]),
        ("source nodata", lsmall, ["-srcnodata", common, "-skipnodata"], ["--srcnodata", common, "--skipnodata"]),
        ("Int32 values", i32, [], []),
        ("Pseudo-Mercator window", dem, ["-srcwin", "1000", "500", "60", "40"], ["--srcwin", "1000", "500", "60", "40"]),
    ]
    for name, src, gopts, topts in cases:
        gout, tout = str(work / "g.xyz"), str(work / "t.xyz")
        gc, _, ge = run([sys.executable, "-m", "osgeo_utils.gdal2xyz", *gopts, src, gout])
        tc, _, te = run([terrio, "xyz", src, tout, *topts])
        if gc or tc:
            results.append({"operation": "xyz", "name": f"xyz {name}", "status": "differs", "detail": f"{ge.strip()[-200:]} {te.strip()[:200]}"})
            continue
        g, t = Path(gout).read_bytes(), Path(tout).read_bytes()
        detail = f"{g.count(NL)} lines" if g == t else f"{g[:120]!r} vs {t[:120]!r}"
        results.append({"operation": "xyz", "name": f"xyz {name}", "status": "identical" if g == t else "differs", "detail": detail})
    # skip: through the function (the 3.12 script's -skip takes no value).
    gout, tout = str(work / "g_skip.xyz"), str(work / "t_skip.xyz")
    gdal2xyz(lsmall, gout, skip=3, band_nums=[1, 2], progress_callback=None)
    tc, _, te = run([terrio, "xyz", lsmall, tout, "--skip", "3", "-b", "1,2"])
    if tc or not Path(tout).exists():
        results.append({"operation": "xyz", "name": "xyz skip 3", "status": "differs", "detail": f"exit {tc}: {te.strip()[:200]}"})
    else:
        g, t = Path(gout).read_bytes(), Path(tout).read_bytes()
        results.append({"operation": "xyz", "name": "xyz skip 3", "status": "identical" if g == t else "differs",
                        "detail": f"{g.count(NL)} lines" if g == t else f"{g[:120]!r} vs {t[:120]!r}"})
    # Real DEM samples, exposed as mixed band types with unrepresentable integer nodata.
    mixed = str(work / "mixed_nonfinite_nodata.vrt")
    dataset = gdal.BuildVRT(mixed, [i32, small], separate=True)
    dataset.GetRasterBand(1).SetNoDataValue(0)
    dataset.GetRasterBand(2).SetNoDataValue(float("nan"))
    dataset = None
    gout, tout = work / "g_mixed_nodata.xyz", work / "t_mixed_nodata.xyz"
    gc, _, ge = run([sys.executable, "-m", "osgeo_utils.gdal2xyz", "-allbands", "-skipnodata", mixed, gout])
    tc, _, te = run([terrio, "xyz", mixed, tout, "--allbands", "--skipnodata"])
    rejected = (gc != 0 and tc != 0 and "cannot convert float NaN to integer" in ge
                and "non-finite source nodata" in te
                and (not gout.exists() or gout.stat().st_size == 0)
                and (not tout.exists() or tout.stat().st_size == 0))
    results.append({"operation": "xyz", "name": "xyz mixed nonfinite integer nodata refused",
                    "status": "identical" if rejected else "differs",
                    "detail": "both reject non-finite integer nodata without writing sample data" if rejected else f"GDAL {gc}: {ge[-200:]}; Terrio {tc}: {te[-200:]}"})
    return results


def main():
    """Runs the comparisons."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("data", type=Path)
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--json", action="store_true")
    parser.add_argument("--only", choices=["compare", "pixel-info", "xyz"])
    args = parser.parse_args()
    data = args.data.resolve()
    results = []
    with tempfile.TemporaryDirectory(prefix="terrio-query-") as tmp:
        work = Path(tmp)
        if args.only in (None, "compare"):
            results += compare_cases(data, work, args.terrio)
        if args.only in (None, "pixel-info"):
            results += pixel_info_cases(data, work, args.terrio)
        if args.only in (None, "xyz"):
            results += xyz_cases(data, work, args.terrio)
    if args.json:
        print(json.dumps(results))
    else:
        for r in results:
            print(f"{r['status']:>16}  {r['name']}  {r['detail']}")


if __name__ == "__main__":
    main()
