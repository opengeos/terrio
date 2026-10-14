"""Compare terrio's dataset summaries with GDAL's, on real-world files.

Usage: python scripts/compare_with_gdal.py DATA_DIR [FILE ...]

Needs GDAL's Python bindings (``osgeo``) and a built ``terrio`` binary (``target/release/terrio``,
or the path in ``TERRIO_BIN``). Prints one line per file and exits non-zero when any differ.
"""

import json
import math
import os
import subprocess
import sys
import zipfile

from osgeo import gdal, ogr, osr

gdal.UseExceptions()

TERRIO = os.environ.get("TERRIO_BIN", os.path.join(os.path.dirname(__file__), "..", "target", "release", "terrio"))
RASTER_EXT = {".tif", ".tiff", ".asc", ".jp2", ".img", ".hdr", ".zarr", ".flt", ".sdat", ".rst", ".ers", ".dt1", ".dt2", ".xyz"}
VECTOR_EXT = {".geojson", ".json", ".gpkg", ".parquet", ".fgb", ".shp", ".kml", ".kmz", ".gml", ".gpx", ".topojson", ".mif", ".csv", ".zip"}

GDAL_TYPES = {
    ogr.wkbPoint: "Point",
    ogr.wkbLineString: "LineString",
    ogr.wkbPolygon: "Polygon",
    ogr.wkbMultiPoint: "MultiPoint",
    ogr.wkbMultiLineString: "MultiLineString",
    ogr.wkbMultiPolygon: "MultiPolygon",
}


def terrio_info(path, stats=False):
    """Run ``terrio info --json`` on a path.

    Args:
        path: The dataset path.
        stats: Whether to compute band statistics.

    Returns:
        The parsed JSON summary, or a string with the error message.
    """
    cmd = [TERRIO, "info", "--json", path] + (["--stats"] if stats else [])
    p = subprocess.run(cmd, capture_output=True, text=True)
    if p.returncode != 0:
        return p.stderr.strip() or "failed"
    return json.loads(p.stdout)


def epsg_of(wkt):
    """Identify the EPSG code of a WKT definition with GDAL.

    Args:
        wkt: The WKT definition.

    Returns:
        The EPSG code as an int, or None.
    """
    if not wkt:
        return None
    srs = osr.SpatialReference()
    try:
        srs.ImportFromWkt(wkt)
    except RuntimeError:
        return None
    try:
        srs.AutoIdentifyEPSG()
    except RuntimeError:
        pass
    code = srs.GetAuthorityCode(None)
    if code is None:
        matches = srs.FindMatches()
        if matches and matches[0][1] >= 90:
            code = matches[0][0].GetAuthorityCode(None)
    return int(code) if code else None


def close(a, b, rel=1e-6):
    """Whether two numbers (or NaNs) are equal within a relative tolerance.

    Args:
        a: First number.
        b: Second number.
        rel: Relative tolerance.

    Returns:
        True when they match.
    """
    if a is None or b is None:
        return a is b
    if math.isnan(a) and math.isnan(b):
        return True
    return math.isclose(a, b, rel_tol=rel, abs_tol=1e-9)


def compare_raster(path, t):
    """Compare a raster's summary with GDAL's.

    Args:
        path: The dataset path.
        t: terrio's JSON summary.

    Returns:
        A list of differences.
    """
    ds = gdal.Open(path)
    diffs = []
    if t["size"] != [ds.RasterXSize, ds.RasterYSize]:
        diffs.append(f"size {t['size']} vs {[ds.RasterXSize, ds.RasterYSize]}")
    if t["band_count"] != ds.RasterCount:
        diffs.append(f"bands {t['band_count']} vs {ds.RasterCount}")
    gt = ds.GetGeoTransform()
    if not all(close(a, b, 1e-9) for a, b in zip(t["geo_transform"], gt)):
        diffs.append(f"geotransform {t['geo_transform']} vs {list(gt)}")
    g_epsg = epsg_of(ds.GetProjection())
    if t["crs"]["epsg"] != g_epsg:
        diffs.append(f"epsg {t['crs']['epsg']} vs {g_epsg}")
    for tb in t["bands"]:
        b = ds.GetRasterBand(tb["band"])
        gtype = gdal.GetDataTypeName(b.DataType)
        if tb["data_type"] != gtype:
            diffs.append(f"band {tb['band']} type {tb['data_type']} vs {gtype}")
        nd = tb["nodata"]
        nd = float(nd) if isinstance(nd, str) else nd
        if not close(nd, b.GetNoDataValue()):
            diffs.append(f"band {tb['band']} nodata {tb['nodata']} vs {b.GetNoDataValue()}")
        if tb.get("statistics"):
            mn, mx, mean, std = b.ComputeStatistics(False)
            s = tb["statistics"]
            for name, a, g in (("min", s["min"], mn), ("max", s["max"], mx), ("mean", s["mean"], mean), ("std", s["std_dev"], std)):
                if not close(a, g, 1e-4):
                    diffs.append(f"band {tb['band']} {name} {a} vs {g}")
    return diffs


def compare_vector(path, t):
    """Compare a vector dataset's summary with GDAL's.

    Args:
        path: The dataset path.
        t: terrio's JSON summary.

    Returns:
        A list of differences.
    """
    ds = ogr.Open(path)
    diffs = []
    g_layers = [ds.GetLayer(i) for i in range(ds.GetLayerCount())]
    if len(t["layers"]) != len(g_layers):
        diffs.append(f"layer count {len(t['layers'])} vs {len(g_layers)}")
    for tl, gl in zip(t["layers"], g_layers):
        n = gl.GetFeatureCount()
        if tl["feature_count"] != n:
            diffs.append(f"{tl['name']}: features {tl['feature_count']} vs {n}")
        g_fields = [gl.GetLayerDefn().GetFieldDefn(i).GetName() for i in range(gl.GetLayerDefn().GetFieldCount())]
        t_fields = [f["name"] for f in tl["fields"]]
        if t_fields != g_fields:
            missing = [f for f in g_fields if f not in t_fields]
            extra = [f for f in t_fields if f not in g_fields]
            diffs.append(f"{tl['name']}: fields differ (missing {missing[:5]}, extra {extra[:5]}, order differs: {not missing and not extra})")
        srs = gl.GetSpatialRef()
        g_epsg = epsg_of(srs.ExportToWkt()) if srs else None
        if tl["crs"]["epsg"] != g_epsg:
            diffs.append(f"{tl['name']}: epsg {tl['crs']['epsg']} vs {g_epsg}")
        if n:
            e = gl.GetExtent()
            ge = [e[0], e[2], e[1], e[3]]
            te = tl["extent"]
            if te is None or not all(close(a, b, 1e-6) for a, b in zip(te, ge)):
                diffs.append(f"{tl['name']}: extent {te} vs {ge}")
        gtype = GDAL_TYPES.get(ogr.GT_Flatten(gl.GetGeomType()))
        ttype = tl["geometry_type"].replace(" Z", "")
        if gtype and ttype != gtype:
            diffs.append(f"{tl['name']}: geometry {tl['geometry_type']} vs {gtype}")
    return diffs


def zip_member(path):
    """Return the GDAL path of a zip archive's main dataset, as terrio picks it.

    Args:
        path: The archive path.

    Returns:
        A ``/vsizip/`` path to the first shapefile (or other dataset) in the archive.
    """
    names = sorted(zipfile.ZipFile(path).namelist())
    for ext in (".shp", ".gpkg", ".geojson", ".json", ".fgb", ".tif"):
        for n in names:
            if n.lower().endswith(ext):
                return f"/vsizip/{path}/{n}"
    return "/vsizip/" + path


def main():
    """Compare every dataset in the data directory (or the given files)."""
    data = sys.argv[1]
    files = sys.argv[2:] or sorted(os.listdir(data))
    failed = 0
    for name in files:
        path = os.path.join(data, name)
        ext = os.path.splitext(name)[1].lower()
        if ext not in RASTER_EXT | VECTOR_EXT:
            continue
        if ext == ".hdr":
            continue  # GDAL opens ENVI data files, not headers
        gdal_path = zip_member(path) if ext == ".zip" else path
        t = terrio_info(path, stats=ext in RASTER_EXT)
        if isinstance(t, str):
            print(f"FAIL  {name}: terrio error: {t}")
            failed += 1
            continue
        try:
            diffs = compare_raster(gdal_path, t) if t["type"] == "raster" else compare_vector(gdal_path, t)
        except Exception as e:  # noqa: BLE001 - report and continue with the next file
            diffs = [f"GDAL error: {e}"]
        if diffs:
            failed += 1
            print(f"DIFF  {name}: " + "; ".join(diffs))
        else:
            print(f"OK    {name}")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
