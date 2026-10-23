"""Compare terrio's GRIB driver with GDAL's, band by band, on real GRIB files.

For each GRIB file (``.grb``, ``.grb2``, ``.grib``, ``.grib2``, and GDAL's autotest files) of the
directories given, opens it with GDAL (without ``.idx`` sidecars, ``USE_IDX=NO``) and with terrio
(``terrio.open_raster``), and compares:

- the size, band count, geotransform (within 1e-9 relative) and CRS (``osr.IsSame``);
- each band's description and ``GRIB_*`` metadata items, exactly;
- each band's nodata value;
- each band's pixels: identical, or within one Float32 unit in the last place (GDAL decodes in
  single precision on platforms whose compiler fuses multiplications and additions, as on macOS
  arm64), else they differ.

Prints a line per file, ``OK``, ``TOL``, ``DIFF`` or ``SKIP`` (neither GDAL nor terrio opens
it), the file (as ``<directory name>/<path in it>``), and what differs; with ``--raw`` also with
units left as stored (``GRIB_NORMALIZE_UNITS=NO``). Exits non-zero when a file differs.

Usage:
    python scripts/compare_grib_with_gdal.py DIR [DIR ...] [--raw] [--files NAME ...]

Needs GDAL's Python bindings, NumPy and the terrio Python package.
"""

from __future__ import annotations

import argparse
import math
import os
import sys

import numpy as np
from osgeo import gdal, osr

import terrio

gdal.UseExceptions()
gdal.PushErrorHandler("CPLQuietErrorHandler")

EXTENSIONS = (".grb", ".grb2", ".grib", ".grib2", ".grib1", ".bin")
# Files of GDAL's autotest that GDAL itself does not open (truncated on purpose).
SKIP = {"blend.t17z.master.f001.co.grib2.head"}


def grib_files(dirs, names):
    """The GRIB files of some directories.

    Args:
        dirs: The directories.
        names: File names to keep, or None for all.

    Returns:
        ``(label, path)`` pairs, sorted: the label is the directory's name and the file's path in it.
    """
    out = []
    for d in dirs:
        if not os.path.isdir(d):
            continue
        for name in sorted(os.listdir(d)):
            path = os.path.join(d, name)
            if not os.path.isfile(path) or name in SKIP or (names and name not in names):
                continue
            if name.lower().endswith(EXTENSIONS):
                with open(path, "rb") as f:
                    if b"GRIB" in f.read(300 + 16):
                        out.append((os.path.join(os.path.basename(os.path.normpath(d)), name), path))
    return out


def terrio_bands(r):
    """Per-band items of a terrio raster's metadata.

    Args:
        r: The ``terrio.Raster``.

    Returns:
        A ``(metadata, descriptions, nodata)`` tuple: dicts of ``GRIB_*`` items, descriptions and
        nodata values, by band number.
    """
    meta, desc, nodata = {}, {}, {}
    for k, v in r.metadata:
        if k.startswith("TERRIO#BAND_"):
            n, _, what = k[len("TERRIO#BAND_") :].partition("_")
            if what == "DESCRIPTION":
                desc[int(n)] = v
            elif what == "NODATA":
                nodata[int(n)] = float(v)
        elif k.startswith("BAND_") and "#" in k:
            band, item = k.split("#", 1)
            meta.setdefault(int(band[5:]), {})[item] = v
    return meta, desc, nodata


def same_crs(a, b, gt, width, height):
    """Whether two CRSs are the same, or put a raster's corners and center at the same places.

    Rotated grids are a derived geographic CRS in GDAL (its GRIB convention) and a PROJ
    ``ob_tran`` one in terrio: equivalent, but not the same definition.

    Args:
        a: A CRS.
        b: The other CRS.
        gt: The raster's geotransform.
        width: Its width.
        height: Its height.

    Returns:
        Whether the CRSs are the same.
    """
    if a.IsSame(b, ["IGNORE_DATA_AXIS_TO_SRS_AXIS_MAPPING=YES", "CRITERION=EQUIVALENT_EXCEPT_AXIS_ORDER_GEOGCRS"]):
        return True
    wgs84 = osr.SpatialReference()
    wgs84.ImportFromEPSG(4326)
    wgs84.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    for s in (a, b):
        s.SetAxisMappingStrategy(osr.OAMS_TRADITIONAL_GIS_ORDER)
    try:
        ta, tb = osr.CoordinateTransformation(a, wgs84), osr.CoordinateTransformation(b, wgs84)
        for px, py in ((0.5, 0.5), (width - 0.5, 0.5), (0.5, height - 0.5), (width - 0.5, height - 0.5), (width / 2, height / 2)):
            x, y = gt[0] + px * gt[1] + py * gt[2], gt[3] + px * gt[4] + py * gt[5]
            pa, pb = ta.TransformPoint(x, y), tb.TransformPoint(x, y)
            if any(not math.isclose(u, v, abs_tol=1e-9) for u, v in zip(pa[:2], pb[:2])):
                return False
    except RuntimeError:
        return False
    return True


def ulps(a, b):
    """The largest distance between two arrays in Float32 units in the last place.

    Args:
        a: An array.
        b: An array of the same shape.

    Returns:
        The largest difference, in multiples of the spacing of Float32 numbers at the larger of
        the two values, over the points where both are finite.
    """
    ok = np.isfinite(a) & np.isfinite(b)
    if not ok.any():
        return 0.0
    d = np.abs(a[ok] - b[ok])
    spacing = np.spacing(np.maximum(np.abs(a[ok]), np.abs(b[ok])).astype(np.float32)).astype(np.float64)
    return float(np.max(d / spacing))


def compare(path):
    """Compares a file.

    Args:
        path: The file.

    Returns:
        A ``(status, details)`` pair: ``OK``, ``TOL`` or ``DIFF``, and what differs.
    """
    try:
        g = gdal.OpenEx(path, gdal.OF_RASTER, open_options=["USE_IDX=NO"])
    except RuntimeError as e:
        g = None
        gdal_error = str(e)
    try:
        t = terrio.open_raster(path, lazy=True)
    except Exception as e:  # noqa: BLE001 (any terrio error is a difference)
        if g is None:
            return "SKIP", "neither GDAL nor terrio opens it"
        return "DIFF", f"terrio cannot open it: {e}"
    if g is None:
        return "DIFF", f"GDAL cannot open it ({gdal_error}), terrio does"
    diffs = []
    if (g.RasterXSize, g.RasterYSize, g.RasterCount) != (t.width, t.height, t.count):
        return "DIFF", f"size {t.width}x{t.height}x{t.count} vs GDAL's {g.RasterXSize}x{g.RasterYSize}x{g.RasterCount}"
    gt, tt = g.GetGeoTransform(), t.transform
    if any(not math.isclose(a, b, rel_tol=1e-9, abs_tol=1e-9) for a, b in zip(tt, gt)):
        diffs.append(f"geotransform {tt} vs {gt}")
    gs = g.GetSpatialRef()
    if gs is None and t.crs_wkt:
        diffs.append("a CRS GDAL does not give")
    elif gs is not None:
        if not t.crs_wkt:
            diffs.append("no CRS")
        else:
            ts = osr.SpatialReference()
            ts.ImportFromWkt(t.crs_wkt)
            if not same_crs(ts, gs, gt, g.RasterXSize, g.RasterYSize):
                diffs.append(f"CRS {ts.ExportToProj4()} vs {gs.ExportToProj4()}")
    meta, desc, nodata = terrio_bands(t)
    worst = 0.0
    for b in range(1, g.RasterCount + 1):
        gb = g.GetRasterBand(b)
        gnd = gb.GetNoDataValue()
        gmeta = gb.GetMetadata()
        if gb.GetDescription() != desc.get(b, ""):
            diffs.append(f"band {b} description {desc.get(b)!r} vs {gb.GetDescription()!r}")
        if meta.get(b, {}) != gmeta:
            keys = sorted(set(meta.get(b, {})) | set(gmeta))
            bad = [k for k in keys if meta.get(b, {}).get(k) != gmeta.get(k)]
            diffs.append(f"band {b} metadata {bad[0]}: {meta.get(b, {}).get(bad[0])!r} vs {gmeta.get(bad[0])!r}")
        tnd = nodata.get(b)
        if not (tnd == gnd or (tnd is not None and gnd is not None and math.isnan(tnd) and math.isnan(gnd))):
            diffs.append(f"band {b} nodata {tnd} vs {gnd}")
        try:
            ga = gb.ReadAsArray().astype(np.float64)
        except RuntimeError:
            ga = None
        try:
            ta = np.asarray(t.read(b), dtype=np.float64)
        except Exception:  # noqa: BLE001 (any terrio error is a difference, unless GDAL fails too)
            ta = None
        if ga is None or ta is None:
            if (ga is None) != (ta is None):
                diffs.append(f"band {b}: {'GDAL' if ga is None else 'terrio'} cannot read it")
            continue
        same = np.array_equal(ga, ta, equal_nan=True)
        if not same:
            # Non-finite values (NaN, infinities) must be the same, at the same pixels.
            odd = ~np.isfinite(ga) | ~np.isfinite(ta)
            if not np.array_equal(ga[odd], ta[odd], equal_nan=True):
                diffs.append(f"band {b}: non-finite values at other pixels")
                continue
            u = ulps(ga, ta)
            worst = max(worst, u)
            if u > 1.0:
                diffs.append(f"band {b}: pixels differ by up to {u:.3g} Float32 ulps")
    if diffs:
        more = f" (and {len(diffs) - 3} more)" if len(diffs) > 3 else ""
        return "DIFF", "; ".join(diffs[:3]) + more
    if worst > 0:
        return "TOL", f"{g.RasterCount} bands, pixels within {worst:.2g} Float32 ulp"
    return "OK", f"{g.RasterCount} bands"


def main():
    """Compares the files and prints the results."""
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("dirs", nargs="+", help="directories of GRIB files")
    ap.add_argument("--raw", action="store_true", help="also compare with units as stored (GRIB_NORMALIZE_UNITS=NO)")
    ap.add_argument("--files", nargs="*", help="only these file names")
    args = ap.parse_args()
    files = grib_files(args.dirs, set(args.files or []))
    if not files:
        print("no GRIB file found", file=sys.stderr)
        return 2
    modes = [("YES", "")] + ([("NO", " (units as stored)")] if args.raw else [])
    failed = False
    for value, label in modes:
        os.environ["GRIB_NORMALIZE_UNITS"] = value
        gdal.SetConfigOption("GRIB_NORMALIZE_UNITS", value)
        for name, path in files:
            status, details = compare(path)
            failed |= status == "DIFF"
            print(f"{status:4} {name}{label}: {details}", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
