"""Compare terrio's dataset summaries with GDAL's, on real-world files.

Usage:
    python scripts/compare_with_gdal.py [--pixels] DATA_DIR [FILE ...]
    python scripts/compare_with_gdal.py --roundtrip DATA_DIR FILE EXT [EXT ...]

The first form compares ``terrio info`` with GDAL for each file (and, with ``--pixels``, the
raster pixels). The second converts FILE with ``terrio convert`` into each format (by extension,
such as ``fgb`` or ``csv``) and compares what GDAL reads from the output with what GDAL reads from
FILE: feature counts, fields, CRSs, extents and a sample of geometries and attribute values.

Needs GDAL's Python bindings (``osgeo``) and a built ``terrio`` binary (``target/release/terrio``,
or the path in ``TERRIO_BIN``). Prints one line per file and exits non-zero when any differ.
With ``--pixels``, rasters are also compared pixel by pixel (terrio converts them to GeoTIFF;
needs NumPy).
"""

import json
import math
import os
import subprocess
import sys
import tempfile
import zipfile

from osgeo import gdal, ogr, osr

gdal.UseExceptions()
gdal.PushErrorHandler("CPLQuietErrorHandler")

TERRIO = os.environ.get("TERRIO_BIN", os.path.join(os.path.dirname(__file__), "..", "target", "release", "terrio"))
RASTER_EXT = {".tif", ".tiff", ".asc", ".jp2", ".img", ".hdr", ".zarr", ".flt", ".bil", ".envi", ".sdat", ".rst", ".ers", ".dt1", ".dt2", ".xyz", ".grd", ".png", ".jpg", ".nc"}
VECTOR_EXT = {".geojson", ".json", ".gpkg", ".parquet", ".fgb", ".shp", ".kml", ".kmz", ".gml", ".gpx", ".topojson", ".mif", ".tab", ".osm", ".csv", ".zip", ".pbf"}

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
        elif matches and matches[0][1] >= 70 and matches[0][0].GetAuthorityCode(None) == "CRS84":
            # Longitude/latitude WGS 84 (an ESRI .prj): EPSG:4326 in traditional GIS order.
            code = "4326"
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
    # A file of several variables (netCDF): terrio opens the first, as its first subdataset.
    subdatasets = ds.GetSubDatasets()
    if ds.RasterCount == 0 and subdatasets:
        ds = gdal.Open(subdatasets[0][0])
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


def compare_pixels(path, gdal_path):
    """Compare a raster's pixels as terrio and GDAL read them.

    Args:
        path: The dataset path, as given to terrio.
        gdal_path: The dataset path, as given to GDAL.

    Returns:
        A list of differences.
    """
    import numpy as np

    with tempfile.TemporaryDirectory() as tmp:
        out = os.path.join(tmp, "pixels.tif")
        p = subprocess.run([TERRIO, "convert", path, out], capture_output=True, text=True)
        if p.returncode != 0:
            return [f"terrio convert failed: {p.stderr.strip()}"]
        g = gdal.Open(gdal_path)
        subdatasets = g.GetSubDatasets()
        if g.RasterCount == 0 and subdatasets:
            g = gdal.Open(subdatasets[0][0])
        t = gdal.Open(out)
        diffs = []
        for i in range(1, g.RasterCount + 1):
            a, b = g.GetRasterBand(i).ReadAsArray(), t.GetRasterBand(i).ReadAsArray()
            if a.shape != b.shape:
                return [f"band {i} shape {b.shape} vs {a.shape}"]
            same = (a == b) | (np.isnan(a) & np.isnan(b)) if a.dtype.kind == "f" else a == b
            if not same.all():
                err = np.abs(a.astype(np.float64) - b.astype(np.float64))
                diffs.append(f"band {i} pixels differ ({int((~same).sum())} of {a.size}, up to {np.nanmax(err)})")
        return diffs


# GDAL reads CSV columns as strings and ignores coordinate columns unless told otherwise; terrio
# detects them, like GDAL with these open options.
CSV_OPEN_OPTIONS = [
    "AUTODETECT_TYPE=YES",
    "X_POSSIBLE_NAMES=x,lon,lng,long,longitude,easting,xcoord,x_coord,point_x",
    "Y_POSSIBLE_NAMES=y,lat,latitude,northing,ycoord,y_coord,point_y",
    "Z_POSSIBLE_NAMES=z",
    "GEOM_POSSIBLE_NAMES=wkt,geometry,geom,the_geom",
]


def open_vector(path):
    """Open a vector dataset with GDAL, with terrio's CSV conventions for CSV files.

    Args:
        path: The dataset path.

    Returns:
        The GDAL dataset.
    """
    if path.lower().endswith((".csv", ".tsv")):
        return gdal.OpenEx(path, gdal.OF_VECTOR, open_options=CSV_OPEN_OPTIONS)
    return ogr.Open(path)


def compare_vector(path, t):
    """Compare a vector dataset's summary with GDAL's.

    Args:
        path: The dataset path.
        t: terrio's JSON summary.

    Returns:
        A list of differences.
    """
    ds = open_vector(path)
    # GDAL gives CSV and TopoJSON layers no CRS; terrio assumes EPSG:4326 for longitudes and latitudes.
    no_gdal_crs = path.lower().endswith((".csv", ".tsv", ".topojson"))
    diffs = []
    g_layers = [ds.GetLayer(i) for i in range(ds.GetLayerCount())]
    if len(t["layers"]) != len(g_layers):
        diffs.append(f"layer count {len(t['layers'])} vs {len(g_layers)}")
    # GDAL's OSM layers do not know their counts, and report the file's bounds as their extent.
    is_osm = path.lower().endswith(".pbf")
    for tl, gl in zip(t["layers"], g_layers):
        n = gl.GetFeatureCount()
        if n < 0:
            n = sum(1 for _ in gl)
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
        if tl["crs"]["epsg"] != g_epsg and not (no_gdal_crs and g_epsg is None):
            diffs.append(f"{tl['name']}: epsg {tl['crs']['epsg']} vs {g_epsg}")
        if n and not is_osm:
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


def layer_features(layer, limit):
    """Read the first features of a GDAL layer, keyed for matching.

    Args:
        layer: The GDAL layer.
        limit: How many features to read.

    Returns:
        A list of (attributes dict, geometry) pairs.
    """
    out = []
    layer.ResetReading()
    for i, f in enumerate(layer):
        if i >= limit:
            break
        g = f.GetGeometryRef()
        out.append((f.items(), g.Clone() if g else None))
    return out


def same_value(a, b):
    """Whether two attribute values match, allowing for type changes between formats.

    Args:
        a: The source value.
        b: The converted value.

    Returns:
        True when they match.
    """
    if a is None or b is None:
        return a is None and b is None or a == "" or b == ""
    if isinstance(a, list) or isinstance(b, list):
        # List fields become JSON text in formats without list types.
        norm = [json.dumps(v, separators=(",", ":")) if isinstance(v, list) else str(v).replace(" ", "") for v in (a, b)]
        return norm[0] == norm[1]
    if isinstance(a, (int, float)) or isinstance(b, (int, float)):
        try:
            return close(float(a), float(b), 1e-9)
        except (TypeError, ValueError):
            return False
    return normalize_text(a) == normalize_text(b)


def normalize_text(v):
    """Normalize a text value, writing date-times the same way whatever the format.

    Args:
        v: The value.

    Returns:
        The value as text, with GDAL's ``2013/01/02 02:03:00+00`` and ISO 8601
        ``2013-01-02T02:03:00Z`` date-times made equal.
    """
    s = str(v).rstrip()
    if len(s) >= 19 and s[4] in "-/" and s[7] in "-/" and s[10] in "T ":
        s = s[:4] + "-" + s[5:7] + "-" + s[8:10] + " " + s[11:]
        for utc in ("Z", "+00:00", "+00"):
            if s.endswith(utc):
                s = s[: -len(utc)]
    return s


def same_geometry(a, b):
    """Whether two geometries match: same area, length and bounds within a tolerance.

    Args:
        a: The source GDAL geometry, or None.
        b: The converted GDAL geometry, or None.

    Returns:
        True when they match.
    """
    if a is None or b is None or a.IsEmpty() or b.IsEmpty():
        return (a is None or a.IsEmpty()) == (b is None or b.IsEmpty())
    if not all(close(x, y, 1e-7) for x, y in zip(a.GetEnvelope(), b.GetEnvelope())):
        return False
    return close(a.GetArea(), b.GetArea(), 1e-6) and close(a.Length(), b.Length(), 1e-6)


def compare_roundtrip(src, out, limit=200):
    """Compare what GDAL reads from a source and from terrio's conversion of it.

    Features are matched by position when both keep the order, and otherwise by their
    attribute values (formats with spatial indexes reorder features).

    Args:
        src: The source dataset path.
        out: The converted dataset path.
        limit: How many features to compare per layer.

    Returns:
        A list of differences.
    """
    a, b = open_vector(src), open_vector(out)
    diffs = []
    for i in range(min(a.GetLayerCount(), b.GetLayerCount())):
        la, lb = a.GetLayer(i), b.GetLayer(i)
        name = la.GetName()
        if la.GetFeatureCount() != lb.GetFeatureCount():
            diffs.append(f"{name}: features {lb.GetFeatureCount()} vs {la.GetFeatureCount()}")
        fa = [la.GetLayerDefn().GetFieldDefn(j).GetName() for j in range(la.GetLayerDefn().GetFieldCount())]
        fb = [lb.GetLayerDefn().GetFieldDefn(j).GetName() for j in range(lb.GetLayerDefn().GetFieldCount())]
        missing = [f for f in fa if f not in fb]
        if missing:
            diffs.append(f"{name}: fields missing {missing[:5]}")
        sa_srs, sb_srs = la.GetSpatialRef(), lb.GetSpatialRef()
        if (sa_srs is None) != (sb_srs is None) or (sa_srs is not None and not sa_srs.IsSame(sb_srs)):
            ea_code = epsg_of(sa_srs.ExportToWkt()) if sa_srs else None
            eb_code = epsg_of(sb_srs.ExportToWkt()) if sb_srs else None
            if ea_code != eb_code or ea_code is None:
                diffs.append(f"{name}: crs EPSG:{eb_code} vs EPSG:{ea_code}")
        if la.GetFeatureCount():
            ea, eb = la.GetExtent(), lb.GetExtent()
            if not all(close(x, y, 1e-7) for x, y in zip(ea, eb)):
                diffs.append(f"{name}: extent {eb} vs {ea}")
        sa = layer_features(la, limit)
        sb = layer_features(lb, 10**9 if lb.GetFeatureCount() < 100000 else limit)
        common = [f for f in fa if f in fb]
        bad = 0
        for j, (attrs, geom) in enumerate(sa):
            cands = [sb[j]] if j < len(sb) else []
            cands += [c for c in sb if all(same_value(attrs[f], c[0].get(f)) for f in common)]
            ok = any(all(same_value(attrs[f], c[0].get(f)) for f in common) and same_geometry(geom, c[1]) for c in cands)
            if not ok:
                bad += 1
                if bad == 1:
                    diffs.append(f"{name}: feature {j} differs ({ {f: attrs[f] for f in common[:3]} })")
        if bad > 1:
            diffs.append(f"{name}: {bad} of {len(sa)} sampled features differ")
    if a.GetLayerCount() != b.GetLayerCount():
        diffs.append(f"layer count {b.GetLayerCount()} vs {a.GetLayerCount()}")
    return diffs


def roundtrip_main(args):
    """Convert a file into each format with terrio and compare GDAL's reading of the results.

    Args:
        args: DATA_DIR, FILE, then the output extensions.
    """
    data, name, exts = args[0], args[1], args[2:]
    src = os.path.join(data, name)
    failed = 0
    with tempfile.TemporaryDirectory() as tmp:
        for ext in exts:
            out = os.path.join(tmp, os.path.splitext(os.path.basename(name))[0] + "." + ext)
            p = subprocess.run([TERRIO, "convert", src, out], capture_output=True, text=True)
            label = f"{name} -> {ext}"
            if p.returncode != 0:
                print(f"FAIL  {label}: terrio error: {p.stderr.strip()}")
                failed += 1
                continue
            try:
                diffs = compare_roundtrip(src, out)
            except Exception as e:  # noqa: BLE001 - report and continue with the next format
                diffs = [f"GDAL error: {e}"]
            if diffs:
                failed += 1
                print(f"DIFF  {label}: " + "; ".join(diffs))
            else:
                print(f"OK    {label}")
    sys.exit(1 if failed else 0)


def main():
    """Compare every dataset in the data directory (or the given files)."""
    if sys.argv[1] == "--roundtrip":
        roundtrip_main(sys.argv[2:])
        return
    args = [a for a in sys.argv[1:] if a != "--pixels"]
    pixels = "--pixels" in sys.argv[1:]
    data = args[0]
    files = args[1:] or sorted(os.listdir(data))
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
            if pixels and t["type"] == "raster":
                diffs += compare_pixels(path, gdal_path)
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
