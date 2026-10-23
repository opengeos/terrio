"""Compare terrio's STAC and GeoZarr support with GDAL's STACIT, STACTA, GeoJSON and Zarr
drivers, and with rioxarray.

Usage:
    python scripts/compare_stac_geozarr.py stac
    python scripts/compare_stac_geozarr.py stacta DATA_DIR
    python scripts/compare_stac_geozarr.py geozarr DATA_DIR

``stac`` queries Earth Search (Sentinel-2 L2A over Colorado: four MGRS tiles in one UTM zone,
and, paged one item at a time, tiles in two UTM zones) and checks that ``terrio info`` lists the
subdatasets GDAL's STACIT driver lists, that the mosaics have GDAL's size, geotransform and CRS,
that windows read from them (where tiles meet) hold GDAL's pixels, and that the item collection's
footprints have the fields ``ogrinfo`` reads from the same GeoJSON.

``stacta`` reads STAC items with tiled assets: GDAL's own STACTA test data (fetched from GDAL's
repository) and pyramids GDAL's ``gdal raster tile --webviewer stac`` writes from DATA_DIR's
``las_vegas_dem.tif`` and ``landsat.tif``, local, over HTTP (a local server answering range
requests) and with missing tiles. It checks that ``terrio info`` reports GDAL's size,
geotransform, CRS, band count, data type, nodata, overviews, metadata and subdatasets, and that
the whole raster, windows across tiles and every overview hold GDAL's pixels.

``geozarr`` writes rasters of DATA_DIR as Zarr v2 and v3 with each georeferencing convention
(``GDAL``, ``SPATIAL_PROJ``, ``CF``) and overviews, and checks that GDAL reads the arrays and
their georeferencing, that GDAL's ``gdaladdo -r nearest`` overviews equal terrio's, and, when
xarray and rioxarray are installed, that rioxarray reads the CF stores' CRS and transform.

Needs GDAL's Python bindings (``osgeo``) and NumPy, and a built ``terrio`` binary
(``target/release/terrio``, or the path in ``TERRIO_BIN``). Exits non-zero when anything differs.
"""

import functools
import http.server
import json
import math
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import urllib.request

import numpy as np
from osgeo import gdal, ogr

gdal.UseExceptions()
gdal.PushErrorHandler("CPLQuietErrorHandler")

TERRIO = os.environ.get("TERRIO_BIN", os.path.join(os.path.dirname(__file__), "..", "target", "release", "terrio"))
SEARCH = "https://earth-search.aws.element84.com/v1/search?collections=sentinel-2-l2a"
QUERIES = [
    SEARCH + "&bbox=-105.05,40.5,-104.95,40.6&datetime=2023-07-05T00:00:00Z/2023-07-05T23:59:59Z&limit=10",
    SEARCH + "&bbox=-108.02,40.0,-107.98,40.05&datetime=2023-07-03T00:00:00Z/2023-07-08T23:59:59Z&limit=1",
]
FAILURES = []


def check(ok, what):
    """Record and print a comparison.

    Args:
        ok: Whether terrio agrees with the reference.
        what: A description of the comparison.
    """
    print(("ok   " if ok else "FAIL ") + what)
    if not ok:
        FAILURES.append(what)


def terrio(*args):
    """Run terrio.

    Args:
        *args: The command-line arguments after ``terrio``.

    Returns:
        str: The standard output.
    """
    return subprocess.run([TERRIO, *args], check=True, capture_output=True, text=True).stdout


def subdatasets(summary):
    """The subdataset names of a ``terrio info --json`` summary.

    Args:
        summary: The summary.

    Returns:
        list: The names.
    """
    return [s["name"] for s in summary.get("subdatasets", [])]


def compare_stac():
    """Compare STAC footprints, subdatasets and STACIT mosaics with GDAL on Earth Search."""
    tmp = tempfile.mkdtemp()
    try:
        for q in QUERIES:
            name = f'STACIT:"{q}"'
            ref = gdal.Open(name)
            ref_subs = [v for k, v in ref.GetMetadata("SUBDATASETS").items() if k.endswith("_NAME")]
            ref_subs.sort(key=lambda s: int(next(k for k, v in ref.GetMetadata("SUBDATASETS").items() if v == s).split("_")[1]))
            subs = subdatasets(json.loads(terrio("info", "--json", name)))
            check(subs == ref_subs, f"{q[-60:]}: {len(subs)} subdatasets as GDAL's {len(ref_subs)}")
            for sub in [s for s in ref_subs if s.split(":")[-1].split(",")[0] in ("asset=red", "asset=scl")]:
                g = gdal.Open(sub)
                t = json.loads(terrio("info", "--json", sub))
                same = (
                    t["size"] == [g.RasterXSize, g.RasterYSize]
                    and np.allclose(t["geo_transform"], g.GetGeoTransform())
                    and str(t["crs"]["epsg"]) == g.GetSpatialRef().GetAuthorityCode(None)
                )
                check(same, f"{sub[-40:]}: size {t['size']}, geotransform and CRS as GDAL's")
                # A window where tiles meet (or the middle of the mosaic).
                w = 48
                x = min(g.RasterXSize // 2, g.RasterXSize - w)
                y = min(g.RasterYSize // 2, g.RasterYSize - w)
                out = os.path.join(tmp, "w.tif")
                terrio("convert", "--srcwin", str(x), str(y), str(w), str(w), sub, out)
                a = gdal.Open(out).ReadAsArray()
                b = g.ReadAsArray(x, y, w, w)
                check(np.array_equal(a, b), f"{sub[-40:]}: window {w}x{w} at ({x}, {y}) identical")
        # Footprints and fields, against ogrinfo on the same GeoJSON.
        path = os.path.join(tmp, "items.json")
        with urllib.request.urlopen(QUERIES[0]) as r, open(path, "wb") as f:
            f.write(r.read())
        src = ogr.Open(path)
        lyr = src.GetLayer(0)
        ref_fields = [lyr.GetLayerDefn().GetFieldDefn(i).GetName() for i in range(lyr.GetLayerDefn().GetFieldCount())]
        v = json.loads(terrio("info", "--json", path))["layers"][0]
        check([f["name"] for f in v["fields"]] == ref_fields, f"footprints: {len(v['fields'])} fields as ogrinfo's")
        check(v["feature_count"] == lyr.GetFeatureCount(), f"footprints: {v['feature_count']} features")
    finally:
        shutil.rmtree(tmp)


GDAL_STACTA = "https://raw.githubusercontent.com/OSGeo/gdal/v3.12.2/autotest/gdrivers/data/stacta/"
GDAL_STACTA_FILES = [
    "test.json",
    "test_east_hemisphere.json",
    "test_multiple_asset_templates.json",
    "test_multiple_tms.json",
    "test_stac_1_1.json",
    "test_with_raster_extension.json",
    "test_with_raster_extension_no_eo_bands.json",
    "WorldCRS84Quad/0/0/0.tif",
    "WorldCRS84Quad/1/0/0.tif",
    "WorldCRS84Quad/2/0/0.tif",
    "WorldCRS84Quad/2/0/1.tif",
]


class RangeHandler(http.server.SimpleHTTPRequestHandler):
    """A static file server with range requests and without logging."""

    def log_message(self, *args):
        """Log nothing."""

    def send_head(self):
        """Serve a byte range when asked, else the whole file.

        Returns:
            The file to send whole, or None when a range was sent.
        """
        rng = self.headers.get("Range")
        path = self.translate_path(self.path)
        if not rng or not os.path.isfile(path):
            return super().send_head()
        size = os.path.getsize(path)
        m = re.match(r"bytes=(\d*)-(\d*)", rng)
        if m.group(1):
            a = int(m.group(1))
            b = min(int(m.group(2)) if m.group(2) else size - 1, size - 1)
        else:
            a, b = max(0, size - int(m.group(2))), size - 1
        with open(path, "rb") as f:
            f.seek(a)
            self.send_response(206)
            self.send_header("Content-Range", f"bytes {a}-{b}/{size}")
            self.send_header("Content-Length", str(b - a + 1))
            self.end_headers()
            self.wfile.write(f.read(b - a + 1))
        return None


def serve(root):
    """Serve a directory over HTTP on a local port.

    Args:
        root: The directory.

    Returns:
        str: The server's base URL.
    """
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(RangeHandler, directory=root))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return f"http://127.0.0.1:{server.server_address[1]}"


def same_nodata(a, b):
    """Whether two nodata values (or None) are the same, NaN included.

    Args:
        a: A nodata value, a string such as ``"nan"`` or None.
        b: Another.

    Returns:
        bool: Whether they are equal.
    """
    if a is None or b is None:
        return a is None and b is None
    a, b = float(a), float(b)
    return (math.isnan(a) and math.isnan(b)) or a == b


def compare_stacta_info(name, label, mixed_types=False, gdal_name=None):
    """Compare ``terrio info`` on a STACTA dataset with GDAL's.

    Args:
        name: The dataset name, as terrio opens it.
        label: A short label for the report.
        mixed_types: Whether the bands have several data types (terrio reads them as Float64,
            with the first band's nodata value), so that only the first band's are compared.
        gdal_name: The name GDAL opens it by, when not ``name``.

    Returns:
        The GDAL dataset.
    """
    g = gdal.Open(gdal_name or name)
    t = json.loads(terrio("info", "--json", name))
    ref_subs = [v for k, v in sorted(g.GetMetadata("SUBDATASETS").items(), key=lambda kv: int(kv[0].split("_")[1])) if k.endswith("_NAME")]
    check(subdatasets(t) == ref_subs, f"{label}: {len(ref_subs)} subdatasets as GDAL's")
    if g.RasterCount == 0:
        # terrio opens the first subdataset: compare it.
        g = gdal.Open(ref_subs[0])
    b1 = g.GetRasterBand(1)
    ref_type = "Float64" if mixed_types else gdal.GetDataTypeName(b1.DataType)
    ref_md = [kv.split("=", 1) for kv in g.GetMetadata_List() or []]
    same = (
        t["size"] == [g.RasterXSize, g.RasterYSize]
        and t["band_count"] == g.RasterCount
        and np.allclose(t["geo_transform"], g.GetGeoTransform(), rtol=1e-12, atol=1e-9)
        and str(t["crs"]["epsg"]) == g.GetSpatialRef().GetAuthorityCode(None)
        and all(b["data_type"] == ref_type for b in t["bands"])
        and same_nodata(t["bands"][0]["nodata"], b1.GetNoDataValue())
        and t.get("overviews", []) == [[b1.GetOverview(i).XSize, b1.GetOverview(i).YSize] for i in range(b1.GetOverviewCount())]
        and [m for m in t["metadata"] if not m[0].startswith("SUBDATASET_")] == ref_md
    )
    check(same, f"{label}: {t['size'][0]}x{t['size'][1]}x{t['band_count']}, geotransform, CRS, type, nodata, {len(t.get('overviews', []))} overviews and {len(ref_md)} metadata items as GDAL's")
    return g


def compare_stacta_pixels(name, g, label, tmp):
    """Compare a STACTA dataset's pixels read by terrio with GDAL's: the whole raster, windows
    across tiles, and every overview whole.

    Args:
        name: The dataset name, as terrio opens it.
        g: The GDAL dataset.
        label: A short label for the report.
        tmp: A directory for terrio's output.
    """
    out = os.path.join(tmp, "out.tif")
    w, h = g.RasterXSize, g.RasterYSize
    windows = [(0, 0, w, h), (w // 2 - 37, h // 2 - 29, 74, 58), (w - 300, h - 200, 300, 200), (255, 255, 3, 3)]
    for x, y, ww, hh in windows:
        x, y = max(0, x), max(0, y)
        ww, hh = min(ww, w - x), min(hh, h - y)
        terrio("convert", "--srcwin", str(x), str(y), str(ww), str(hh), name, out)
        a = gdal.Open(out).ReadAsArray()
        b = g.ReadAsArray(x, y, ww, hh)
        check(np.array_equal(a, b, equal_nan=True), f"{label}: window {ww}x{hh} at ({x}, {y}) identical")
    b1 = g.GetRasterBand(1)
    for i in range(b1.GetOverviewCount()):
        terrio("convert", "--overview", str(i), name, out)
        a = gdal.Open(out).ReadAsArray()
        b = np.stack([g.GetRasterBand(k + 1).GetOverview(i).ReadAsArray() for k in range(g.RasterCount)]).squeeze()
        check(np.array_equal(a, b, equal_nan=True), f"{label}: overview {i} ({b.shape[-1]}x{b.shape[-2]}) identical")


def compare_stacta(data_dir):
    """Compare terrio's STACTA reading with GDAL's on GDAL's test data and pyramids of real data.

    Args:
        data_dir: The directory of test rasters (``las_vegas_dem.tif``, ``landsat.tif``).
    """
    tmp = tempfile.mkdtemp()
    try:
        gd = os.path.join(tmp, "gdal")
        for f in GDAL_STACTA_FILES:
            dest = os.path.join(gd, f)
            os.makedirs(os.path.dirname(dest), exist_ok=True)
            with urllib.request.urlopen(GDAL_STACTA + f) as r, open(dest, "wb") as o:
                o.write(r.read())
        # GDAL's test data: info everywhere, pixels where the tiles exist.
        for f in GDAL_STACTA_FILES[:7]:
            path = os.path.join(gd, f)
            mixed = "raster_extension" in f or "1_1" in f
            g = compare_stacta_info(path, f, mixed_types=mixed)
            if not mixed:
                compare_stacta_pixels(path, g, f, tmp)
        for asset in ("other_bands", "bands"):
            name = f'STACTA:"{os.path.join(gd, "test_multiple_asset_templates.json")}":{asset}'
            compare_stacta_pixels(name, compare_stacta_info(name, f"asset {asset}"), f"asset {asset}", tmp)

        # Pyramids of real data, written by GDAL.
        for src, zooms in (("las_vegas_dem.tif", ("11", "15")), ("landsat.tif", ("9", "12"))):
            pyr = os.path.join(tmp, src[:-4])
            subprocess.run(
                ["gdal", "raster", "tile", "--format", "GTiff", "--webviewer", "stac", "--min-zoom", zooms[0],
                 "--max-zoom", zooms[1], "-r", "nearest", os.path.join(data_dir, src), pyr],
                check=True, capture_output=True,
            )
            path = os.path.join(pyr, "stacta.json")
            compare_stacta_pixels(path, compare_stacta_info(path, src), src, tmp)

        # Over HTTP: the item's URL (which GDAL opens as a STACTA: name), and a STACTA: name.
        base = serve(tmp)
        url = f"{base}/landsat/stacta.json"
        compare_stacta_pixels(url, compare_stacta_info(url, "http landsat", gdal_name=f'STACTA:"{url}"'), "http landsat", tmp)
        name = f'STACTA:"{base}/gdal/test.json":bands'
        compare_stacta_pixels(name, compare_stacta_info(name, "http STACTA:test.json"), "http STACTA:test.json", tmp)

        # A missing tile: an error, or nodata with GDAL_STACTA_SKIP_MISSING_METATILE=YES.
        miss = os.path.join(tmp, "missing")
        shutil.copytree(gd, miss)
        os.remove(os.path.join(miss, "WorldCRS84Quad", "2", "0", "1.tif"))
        path = os.path.join(miss, "test.json")
        p = subprocess.run([TERRIO, "convert", path, os.path.join(tmp, "m.tif")], capture_output=True, text=True)
        check(p.returncode != 0 and "Cannot open" in p.stderr, "missing tile: an error, as GDAL's")
        os.environ["GDAL_STACTA_SKIP_MISSING_METATILE"] = "YES"
        gdal.SetConfigOption("GDAL_STACTA_SKIP_MISSING_METATILE", "YES")
        try:
            compare_stacta_pixels(path, compare_stacta_info(path, "missing tile"), "missing tile skipped", tmp)
        finally:
            del os.environ["GDAL_STACTA_SKIP_MISSING_METATILE"]
            gdal.SetConfigOption("GDAL_STACTA_SKIP_MISSING_METATILE", None)
    finally:
        shutil.rmtree(tmp)


def compare_geozarr(data_dir):
    """Compare terrio's GeoZarr stores with GDAL's reading and overviews, and rioxarray's.

    Args:
        data_dir: The directory of test rasters.
    """
    tmp = tempfile.mkdtemp()
    try:
        for name in ("srtm_sub.tif", "dem.tif", "landsat.tif"):
            src = os.path.join(data_dir, name)
            ref = gdal.Open(src)
            tif = os.path.join(tmp, "ref_" + name)
            gdal.Translate(tif, ref)
            upd = gdal.Open(tif, gdal.GA_Update)
            upd.BuildOverviews("NEAREST", [2])
            upd = None
            ref_ovr = gdal.Open(tif)
            ovr = ref_ovr.GetRasterBand(1).GetOverview(0).ReadAsArray()
            for conv in ("GDAL", "SPATIAL_PROJ", "CF"):
                for fmt in ("ZARR_V2", "ZARR_V3"):
                    store = os.path.join(tmp, f"{name[:-4]}_{conv}_{fmt}.zarr")
                    arr = os.path.basename(store)[:-5]
                    terrio(
                        "convert", src, store, "--co", f"FORMAT={fmt}", "--co", f"GEOREFERENCING_CONVENTION={conv}",
                        "--co", "OVERVIEW_LEVELS=2",
                    )
                    g = gdal.Open(f'ZARR:"{store}":/{arr}')
                    same = (
                        np.array_equal(g.ReadAsArray(), ref.ReadAsArray(), equal_nan=True)
                        and np.allclose(g.GetGeoTransform(), ref.GetGeoTransform(), rtol=0, atol=1e-9)
                        and g.GetSpatialRef().IsSame(ref.GetSpatialRef())
                    )
                    check(same, f"{name} {conv} {fmt}: GDAL reads the pixels, geotransform and CRS")
                    o_ds = gdal.Open(f'ZARR:"{store}":/ovr_2x/{arr}')
                    o = o_ds.GetRasterBand(1).ReadAsArray()
                    check(np.array_equal(o, ovr, equal_nan=True), f"{name} {conv} {fmt}: overview equals gdaladdo -r nearest")
                    info = json.loads(terrio("info", "--json", store))
                    check(info["overviews"] == [[o.shape[1], o.shape[0]]], f"{name} {conv} {fmt}: terrio lists the overview")
                    if conv == "CF":
                        try:
                            import rioxarray  # noqa: F401
                            import xarray as xr
                        except ImportError:
                            continue
                        da = xr.open_zarr(store, consolidated=False)[arr]
                        ok = da.rio.crs is not None and np.allclose(tuple(da.rio.transform())[:6], [
                            ref.GetGeoTransform()[i] for i in (1, 2, 0, 4, 5, 3)
                        ])
                        check(ok, f"{name} CF {fmt}: rioxarray reads the CRS and transform")
    finally:
        shutil.rmtree(tmp)


def main():
    """Run the comparison named on the command line."""
    if len(sys.argv) < 2 or sys.argv[1] not in ("stac", "stacta", "geozarr"):
        sys.exit(__doc__)
    if sys.argv[1] == "stac":
        compare_stac()
    elif sys.argv[1] == "stacta":
        compare_stacta(sys.argv[2])
    else:
        compare_geozarr(sys.argv[2])
    print(f"{len(FAILURES)} difference(s)")
    sys.exit(1 if FAILURES else 0)


if __name__ == "__main__":
    main()
