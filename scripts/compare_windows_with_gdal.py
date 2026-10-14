"""Compare terrio's windowed and overview reads of GeoTIFFs with GDAL's, locally and over HTTP.

Usage:
    python scripts/compare_windows_with_gdal.py local DATA_DIR
    python scripts/compare_windows_with_gdal.py remote

``local`` reads windows of every level (full resolution and each overview) of every ``.tif`` in
DATA_DIR with ``terrio convert --srcwin ... --overview N`` and with GDAL, and checks that the pixels
are identical. ``remote`` does the same on public Cloud Optimized GeoTIFFs (Sentinel-2 L2A, a USGS
3DEP tile and a Copernicus DEM tile), and reports the requests, bytes and time each tool takes for
``info`` and for a 512 x 512 window read, from a fresh process.

Needs GDAL's Python bindings (``osgeo``) and NumPy, and a built ``terrio`` binary
(``target/release/terrio``, or the path in ``TERRIO_BIN``). Exits non-zero when any read differs.
"""

import json
import os
import re
import subprocess
import sys
import tempfile
import time
import urllib.request

import numpy as np
from osgeo import gdal

gdal.UseExceptions()
gdal.PushErrorHandler("CPLQuietErrorHandler")

TERRIO = os.environ.get("TERRIO_BIN", os.path.join(os.path.dirname(__file__), "..", "target", "release", "terrio"))
STAC = "https://earth-search.aws.element84.com/v1/collections/sentinel-2-l2a/items?limit=1&bbox=-122.5,37.5,-122,38"
USGS = "https://prd-tnm.s3.amazonaws.com/StagedProducts/Elevation/1/TIFF/current/n40w106/USGS_1_n40w106.tif"
COPERNICUS = "copernicus-dem-30m/Copernicus_DSM_COG_10_N37_00_W123_00_DEM/Copernicus_DSM_COG_10_N37_00_W123_00_DEM.tif"

# GDAL reads remote files fastest without listing their directory; AWS_NO_SIGN_REQUEST reads the
# public buckets anonymously, in both tools.
ENV = dict(os.environ, GDAL_DISABLE_READDIR_ON_OPEN="EMPTY_DIR", AWS_NO_SIGN_REQUEST="YES")

GDAL_READ = """
import sys, numpy as np
from osgeo import gdal
gdal.UseExceptions()
gdal.PushErrorHandler("CPLQuietErrorHandler")
src, x, y, w, h, ovr, out = sys.argv[1], *map(int, sys.argv[2:7]), sys.argv[7]
ds = gdal.Open(src)
bands = [ds.GetRasterBand(i + 1) for i in range(ds.RasterCount)]
if ovr >= 0:
    bands = [b.GetOverview(ovr) for b in bands]
if w > 0:
    np.save(out, np.stack([b.ReadAsArray(x, y, w, h) for b in bands]))
"""


def terrio_run(args):
    """Run terrio with its HTTP requests logged.

    Args:
        args: The command-line arguments after ``terrio``.

    Returns:
        A tuple ``(stdout, seconds, requests, bytes)``.
    """
    env = dict(ENV, TERRIO_HTTP_LOG="YES")
    start = time.perf_counter()
    p = subprocess.run([TERRIO, *args], capture_output=True, text=True, env=env)
    elapsed = time.perf_counter() - start
    if p.returncode != 0:
        raise RuntimeError(f"terrio {' '.join(args)}: {p.stderr.strip()}")
    requests = len(re.findall(r"^terrio: GET ", p.stderr, re.M))
    received = sum(int(n) for n in re.findall(r"received (\d+) bytes", p.stderr))
    return p.stdout, elapsed, requests, received


def gdal_run(src, window, overview, out):
    """Read a window with GDAL in a fresh process, with its HTTP requests logged.

    Args:
        src: The GDAL dataset name.
        window: ``(x, y, width, height)``, or ``None`` to only open the dataset.
        overview: The overview number, or ``None`` for full resolution.
        out: Where to save the pixels (a ``.npy`` file).

    Returns:
        A tuple ``(seconds, requests, bytes)``.
    """
    x, y, w, h = window or (0, 0, 0, 0)
    ovr = -1 if overview is None else overview
    env = dict(ENV, CPL_DEBUG="ON")
    start = time.perf_counter()
    args = [sys.executable, "-I", "-c", GDAL_READ, src, str(x), str(y), str(w), str(h), str(ovr), out]
    p = subprocess.run(args, capture_output=True, text=True, env=env)
    elapsed = time.perf_counter() - start
    if p.returncode != 0:
        raise RuntimeError(f"GDAL {src}: {p.stderr.strip()[-500:]}")
    requests, nbytes = 0, 0
    for line in p.stderr.splitlines():
        # /vsis3/ first asks the default region's endpoint, then probes the size with a GET of the
        # first 16 KB.
        if "Switching to endpoint" in line:
            requests += 1
        if "GetFileSize(" in line:
            requests += 1
            if "response_code=206" in line:
                nbytes += 16384
        m = re.search(r"Downloading ([\d\-, ]+) \(", line)
        if m:
            requests += 1
            for a, b in re.findall(r"(\d+)-(\d+)", m.group(1)):
                nbytes += int(b) - int(a) + 1
    return elapsed, requests, nbytes


def same_pixels(a, b):
    """Whether two arrays hold the same values, NaNs included.

    Args:
        a: An array.
        b: Another array.

    Returns:
        True when the shapes, types and values are identical.
    """
    if a.shape != b.shape or a.dtype != b.dtype:
        return False
    return bool(np.array_equal(a, b, equal_nan=a.dtype.kind == "f"))


def expected_geo_transform(ds, window, overview):
    """The geotransform GDAL gives a window of a level.

    Args:
        ds: The GDAL dataset.
        window: ``(x, y, width, height)``.
        overview: The overview number, or ``None``.

    Returns:
        The geotransform of the window.
    """
    gt = list(ds.GetGeoTransform())
    if overview is not None:
        o = ds.GetRasterBand(1).GetOverview(overview)
        sx, sy = ds.RasterXSize / o.XSize, ds.RasterYSize / o.YSize
        gt = [gt[0], gt[1] * sx, gt[2] * sy, gt[3], gt[4] * sx, gt[5] * sy]
    x, y = window[0], window[1]
    return [gt[0] + x * gt[1], gt[1], 0.0, gt[3] + y * gt[5], 0.0, gt[5]]


def check_window(terrio_src, gdal_src, window, overview, tmp, measure=False):
    """Read one window with both tools and compare the pixels and georeferencing.

    Args:
        terrio_src: The dataset as terrio names it.
        gdal_src: The dataset as GDAL names it.
        window: ``(x, y, width, height)``.
        overview: The overview number, or ``None`` for full resolution.
        tmp: A scratch directory.
        measure: Whether to read with GDAL in a fresh process, to measure it.

    Returns:
        A tuple ``(ok, message, terrio_stats, gdal_stats)``; the stats are ``(seconds, requests,
        bytes)``, or ``None`` when not measured.
    """
    out = os.path.join(tmp, "window.tif")
    args = ["convert", terrio_src, out, "--srcwin", *map(str, window)]
    if overview is not None:
        args += ["--overview", str(overview)]
    _, t_sec, t_req, t_bytes = terrio_run(args)
    gdal_stats = None
    if measure:
        npy = os.path.join(tmp, "window.npy")
        gdal_stats = gdal_run(gdal_src, window, overview, npy)
        expected = np.load(npy)
    else:
        ds = gdal.Open(gdal_src)
        bands = [ds.GetRasterBand(i + 1) for i in range(ds.RasterCount)]
        if overview is not None:
            bands = [b.GetOverview(overview) for b in bands]
        expected = np.stack([b.ReadAsArray(*window) for b in bands])
    got_ds = gdal.Open(out)
    got = np.stack([got_ds.GetRasterBand(i + 1).ReadAsArray() for i in range(got_ds.RasterCount)])
    ds = gdal.Open(gdal_src)
    gt = expected_geo_transform(ds, window, overview)
    gt_ok = np.allclose(got_ds.GetGeoTransform(), gt, rtol=1e-12, atol=1e-9)
    ok = same_pixels(got, expected) and gt_ok
    level = "full" if overview is None else f"ovr {overview}"
    msg = f"{level:7} {window}: " + ("ok" if ok else f"DIFFERENT (pixels {same_pixels(got, expected)}, geotransform {gt_ok})")
    return ok, msg, (t_sec, t_req, t_bytes), gdal_stats


def level_windows(w, h):
    """Windows to test in an image: corners, the centre and a cropped edge.

    Args:
        w: The image width.
        h: The image height.

    Returns:
        A list of ``(x, y, width, height)``.
    """
    s = min(512, w, h)
    return sorted({(0, 0, s, s), (w - s, h - s, s, s), ((w - s) // 2, (h - s) // 3, s, max(1, s // 2)), (w - 1, 0, 1, h)})


def compare_local(data_dir):
    """Compare windows of every level of every GeoTIFF in a directory.

    Args:
        data_dir: The directory.

    Returns:
        The number of windows that differ.
    """
    failures = 0
    with tempfile.TemporaryDirectory() as tmp:
        for name in sorted(os.listdir(data_dir)):
            if not name.endswith(".tif"):
                continue
            path = os.path.join(data_dir, name)
            ds = gdal.Open(path)
            b = ds.GetRasterBand(1)
            levels = [(None, ds.RasterXSize, ds.RasterYSize)]
            levels += [(i, b.GetOverview(i).XSize, b.GetOverview(i).YSize) for i in range(b.GetOverviewCount())]
            for ovr, w, h in levels:
                for window in level_windows(w, h):
                    ok, msg, _, _ = check_window(path, path, window, ovr, tmp)
                    failures += not ok
                    print(f"{name}: {msg}")
    return failures


def sentinel2_hrefs():
    """The red band and true-colour image of a current Sentinel-2 L2A scene, from Earth Search.

    Returns:
        A list of ``(label, url)``.
    """
    with urllib.request.urlopen(STAC) as r:
        item = json.load(r)["features"][0]
    return [(f"Sentinel-2 {item['id']} {k}", item["assets"][k]["href"]) for k in ("red", "visual")]


def compare_remote():
    """Compare, measure and print reads of public Cloud Optimized GeoTIFFs.

    Returns:
        The number of windows that differ.
    """
    targets = [(label, url, "/vsicurl/" + url) for label, url in sentinel2_hrefs()]
    targets.append(("USGS 3DEP 1\" n40w106", USGS, "/vsicurl/" + USGS))
    targets.append(("Copernicus DEM 30 m N37 W123", "s3://" + COPERNICUS, "/vsis3/" + COPERNICUS))
    failures = 0
    rows = []
    with tempfile.TemporaryDirectory() as tmp:
        for label, src, gsrc in targets:
            info_json, i_sec, i_req, i_bytes = terrio_run(["info", "--json", src])
            info = json.loads(info_json)
            g_info = gdal_run(gsrc, None, None, os.path.join(tmp, "none.npy"))
            rows.append((label, "info", (i_sec, i_req, i_bytes), g_info))
            w, h = info["size"]
            ds = gdal.Open(gsrc)
            same_info = [w, h] == [ds.RasterXSize, ds.RasterYSize] and info["overviews"] == [
                [ds.GetRasterBand(1).GetOverview(i).XSize, ds.GetRasterBand(1).GetOverview(i).YSize]
                for i in range(ds.GetRasterBand(1).GetOverviewCount())
            ]
            print(f"{label}: info {'ok' if same_info else 'DIFFERENT'} ({w}x{h}, overviews {info['overviews']})")
            failures += not same_info
            checks = [(None, (w - 512) // 2, (h - 512) // 2)]
            for ovr in range(len(info["overviews"])):
                ow, oh = info["overviews"][ovr]
                checks.append((ovr, max(0, (ow - 512) // 2), max(0, (oh - 512) // 2)))
            for ovr, x, y in checks:
                lw, lh = (w, h) if ovr is None else info["overviews"][ovr]
                window = (x, y, min(512, lw), min(512, lh))
                ok, msg, t_stats, g_stats = check_window(src, gsrc, window, ovr, tmp, measure=True)
                failures += not ok
                print(f"{label}: {msg}")
                level = "full" if ovr is None else f"ovr {ovr}"
                rows.append((label, f"{level} {window[2]}x{window[3]} window", t_stats, g_stats))
    print("\n| Dataset | Operation | terrio requests | terrio bytes | terrio time | GDAL requests | GDAL bytes | GDAL time |")
    print("|---|---|---|---|---|---|---|---|")
    for label, op, (ts, tr, tb), (gs, gr, gb) in rows:
        print(f"| {label} | {op} | {tr} | {tb:,} | {ts:.2f} s | {gr} | {gb:,} | {gs:.2f} s |")
    return failures


def main():
    """Run the comparison named on the command line.

    Returns:
        The exit code: 0 when every read matches GDAL's.
    """
    if len(sys.argv) >= 2 and sys.argv[1] == "remote":
        failures = compare_remote()
    elif len(sys.argv) == 3 and sys.argv[1] == "local":
        failures = compare_local(sys.argv[2])
    else:
        print(__doc__)
        return 2
    print(f"\n{failures} difference(s)")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
