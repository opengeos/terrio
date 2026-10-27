"""Compare terrio's dataset summaries with GDAL's, on real-world files.

Usage:
    python scripts/compare_with_gdal.py [--pixels] DATA_DIR [FILE ...]
    python scripts/compare_with_gdal.py --subdatasets DATA_DIR [FILE ...]
    python scripts/compare_with_gdal.py --roundtrip DATA_DIR FILE EXT [EXT ...]

The first form compares ``terrio info`` with GDAL for each file (and, with ``--pixels``, the
raster pixels). With ``--subdatasets``, the netCDF and HDF5 files are compared subdataset by
subdataset instead: the list of subdatasets (names and descriptions) and, for HDF5, the metadata
GDAL reports, then each subdataset's summary and pixels. The second converts FILE with ``terrio convert`` into each format (by extension,
such as ``fgb`` or ``csv``) and compares what GDAL reads from the output with what GDAL reads from
FILE: feature counts, fields, CRSs, extents and a sample of geometries and attribute values. A
difference GDAL's own conversion (``ogr2ogr``, with the creation options terrio's defaults stand
for) has too is the format's, not terrio's: KML naming the ``name`` field ``Name``, a shapefile
truncating field names, MapInfo rounding coordinates to its grid, a CSV without a CRS.

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
RASTER_EXT = {".tif", ".tiff", ".asc", ".jp2", ".img", ".hdr", ".zarr", ".flt", ".bil", ".envi", ".sdat", ".rst", ".ers", ".dt1", ".dt2", ".hgt", ".xyz", ".grd", ".png", ".jpg", ".nc", ".h5"}
# Containers of subdatasets, compared one subdataset at a time with --subdatasets.
CONTAINER_EXT = {".nc", ".h5", ".he5"}
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


def epsg_of(crs):
    """Identify the EPSG code of a CRS with GDAL.

    Args:
        crs: The CRS: an ``osr.SpatialReference`` (identified as GDAL holds it, axis order
            included, which a WKT1 export loses), or a WKT definition.

    Returns:
        The EPSG code as an int, or None.
    """
    if not crs:
        return None
    if isinstance(crs, osr.SpatialReference):
        srs = crs.Clone()
    else:
        srs = osr.SpatialReference()
        try:
            srs.ImportFromWkt(crs)
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
    # A raster without georeferencing: GDAL's default geotransform is south-up, terrio's north-up.
    if list(gt) == [0.0, 1.0, 0.0, 0.0, 0.0, 1.0]:
        gt = [0.0, 1.0, 0.0, float(ds.RasterYSize), 0.0, -1.0]
    if not all(close(a, b, 1e-9) for a, b in zip(t["geo_transform"], gt)):
        diffs.append(f"geotransform {t['geo_transform']} vs {list(gt)}")
    g_epsg = epsg_of(ds.GetSpatialRef())
    if t["crs"]["epsg"] != g_epsg:
        diffs.append(f"epsg {t['crs']['epsg']} vs {g_epsg}")
    # Geolocation arrays (swaths): GDAL's GEOLOCATION metadata, item for item.
    g_geoloc = ds.GetMetadata("GEOLOCATION") or {}
    t_geoloc = dict(t.get("geolocation", []))
    if "SRS" in g_geoloc and "SRS" in t_geoloc and g_geoloc["SRS"] != t_geoloc["SRS"]:
        # The same geographic CRS, in WKT 1 or 2.
        a, b = osr.SpatialReference(), osr.SpatialReference()
        if a.SetFromUserInput(g_geoloc["SRS"]) == 0 and b.SetFromUserInput(t_geoloc["SRS"]) == 0 and a.IsSameGeogCS(b):
            t_geoloc["SRS"] = g_geoloc["SRS"]
    if t_geoloc != g_geoloc:
        keys = sorted(k for k in set(g_geoloc) | set(t_geoloc) if g_geoloc.get(k) != t_geoloc.get(k))
        diffs.append(f"geolocation {keys[0]}: {t_geoloc.get(keys[0])!r} vs {g_geoloc.get(keys[0])!r}")
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
            try:
                mn, mx, mean, std = b.ComputeStatistics(False)
            except RuntimeError:
                diffs.append(f"band {tb['band']} has statistics, GDAL finds no valid pixel")
                continue
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
            try:
                a = g.GetRasterBand(i).ReadAsArray()
            except RuntimeError:
                continue  # pixels GDAL cannot read
            b = t.GetRasterBand(i).ReadAsArray()
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
    is_osm = path.lower().endswith((".pbf", ".osm"))
    for tl, gl in zip(t["layers"], g_layers):
        n = gl.GetFeatureCount()
        if n < 0:
            n = sum(1 for _ in gl)
        if tl["feature_count"] != n:
            diffs.append(f"{tl['name']}: features {tl['feature_count']} vs {n}")
        g_fields = [gl.GetLayerDefn().GetFieldDefn(i).GetName() for i in range(gl.GetLayerDefn().GetFieldCount())]
        # terrio keeps feature styles (GDAL's OGR_STYLE) in a field.
        t_fields = [f["name"] for f in tl["fields"] if f["name"] != "OGR_STYLE"]
        if t_fields != g_fields:
            missing = [f for f in g_fields if f not in t_fields]
            extra = [f for f in t_fields if f not in g_fields]
            diffs.append(f"{tl['name']}: fields differ (missing {missing[:5]}, extra {extra[:5]}, order differs: {not missing and not extra})")
        srs = gl.GetSpatialRef()
        g_epsg = epsg_of(srs)
        if tl["crs"]["epsg"] != g_epsg and not (no_gdal_crs and g_epsg is None):
            diffs.append(f"{tl['name']}: epsg {tl['crs']['epsg']} vs {g_epsg}")
        # Layers without geometries have no extent (GDAL reports zeros).
        if n and not is_osm and gl.GetGeomType() != ogr.wkbNone:
            e = gl.GetExtent()
            ge = [e[0], e[2], e[1], e[3]]
            te = tl["extent"]
            # GDAL reports zeros for a layer whose features have no geometry.
            no_geometry = te is None and ge == [0.0, 0.0, 0.0, 0.0]
            if not no_geometry and (te is None or not all(close(a, b, 1e-6) for a, b in zip(te, ge))):
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


def compare_roundtrip(src, out, limit=200, missing_fields=None):
    """Compare what GDAL reads from a source and from terrio's conversion of it.

    Features are matched by position when both keep the order, and otherwise by their
    attribute values (formats with spatial indexes reorder features).

    Args:
        src: The source dataset path.
        out: The converted dataset path.
        limit: How many features to compare per layer.
        missing_fields: A dict that receives, for each layer missing fields, its difference
            message and the set of the missing fields.

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
            if missing_fields is not None:
                missing_fields[diffs[-1]] = set(missing)
        sa_srs, sb_srs = la.GetSpatialRef(), lb.GetSpatialRef()
        if (sa_srs is None) != (sb_srs is None) or (sa_srs is not None and not sa_srs.IsSame(sb_srs)):
            ea_code = epsg_of(sa_srs)
            eb_code = epsg_of(sb_srs)
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


# The ogr2ogr options terrio's default conversion into a format stands for (None: GDAL cannot
# write the format).
OGR2OGR_OPTIONS = {
    "csv": ["-lco", "GEOMETRY=AS_WKT"],
    "topojson": None,
}


def ogr2ogr_reference(src, out, ext):
    """Convert a file with ``ogr2ogr`` as terrio converts it by default, as a reference.

    Args:
        src: The source dataset path.
        out: The output path.
        ext: The output extension.

    Returns:
        The output path, or None when GDAL cannot write the format or the conversion failed.
    """
    options = OGR2OGR_OPTIONS.get(ext, [])
    if options is None:
        return None
    # Features GDAL fails to write (an invalid polygon in KML) are skipped, not fatal.
    p = subprocess.run(["ogr2ogr", "-skipfailures", *options, out, src], capture_output=True, text=True)
    return out if p.returncode == 0 and os.path.exists(out) else None


def roundtrip_main(args):
    """Convert a file into each format with terrio and compare GDAL's reading of the results.

    Differences from the source that GDAL's reading of ``ogr2ogr``'s conversion has too are the
    format's, and are reported as such on an ``OK`` line.

    Args:
        args: DATA_DIR, FILE, then the output extensions.
    """
    data, name, exts = args[0], args[1], args[2:]
    src = os.path.join(data, name)
    failed = 0
    with tempfile.TemporaryDirectory() as tmp:
        os.mkdir(os.path.join(tmp, "gdal"))
        for ext in exts:
            base = os.path.splitext(os.path.basename(name))[0] + "." + ext
            out = os.path.join(tmp, base)
            p = subprocess.run([TERRIO, "convert", src, out], capture_output=True, text=True)
            label = f"{name} -> {ext}"
            if p.returncode != 0:
                print(f"FAIL  {label}: terrio error: {p.stderr.strip()}")
                failed += 1
                continue
            try:
                missing = {}
                diffs = compare_roundtrip(src, out, missing_fields=missing)
                shared = []
                ref = ogr2ogr_reference(src, os.path.join(tmp, "gdal", base), ext) if diffs else None
                if ref:
                    gdal_missing = {}
                    gdal_diffs = set(compare_roundtrip(src, ref, missing_fields=gdal_missing))
                    # Per layer, the fields GDAL's conversion is missing too.
                    gdal_missing = {d.split(": fields missing")[0]: f for d, f in gdal_missing.items()}

                    def as_gdal(d):
                        if d in missing:
                            return missing[d] <= gdal_missing.get(d.split(": fields missing")[0], set())
                        return d in gdal_diffs

                    shared = [d for d in diffs if as_gdal(d)]
                    diffs = [d for d in diffs if not as_gdal(d)]
            except Exception as e:  # noqa: BLE001 - report and continue with the next format
                diffs, shared = [f"GDAL error: {e}"], []
            if diffs:
                failed += 1
                print(f"DIFF  {label}: " + "; ".join(diffs))
            elif shared:
                print(f"OK    {label}: as ogr2ogr: " + "; ".join(shared))
            else:
                print(f"OK    {label}")
    sys.exit(1 if failed else 0)


def compare_subdatasets(path):
    """Compare a netCDF or HDF5 file's subdatasets with GDAL's, one by one.

    Args:
        path: The file.

    Returns:
        A list of ``(name, differences)``: first the subdataset list (and HDF5 metadata), then
        each subdataset GDAL opens.
    """
    out = []
    t = terrio_info(path)
    try:
        g = gdal.Open(path)
    except RuntimeError:
        return []  # what GDAL cannot open
    gsubs = g.GetSubDatasets()
    if isinstance(t, str):
        return [("subdataset list", [f"terrio error: {t}"])]
    tsubs = [(s["name"], s["description"]) for s in t.get("subdatasets", [])]
    diffs = []
    if len(tsubs) != len(gsubs):
        diffs.append(f"{len(tsubs)} subdatasets vs {len(gsubs)}")
    for (tn, td), (gn, gd) in zip(tsubs, gsubs):
        if tn != gn or td != gd:
            diffs.append(f"{tn} [{td}] vs {gn} [{gd}]")
            break
    if not gsubs and g.RasterCount > 0:
        # A file with one raster opens it directly.
        ts = terrio_info(path, stats=True)
        if isinstance(ts, str):
            return [(os.path.basename(path), [f"terrio error: {ts}"])]
        return [(os.path.basename(path), diffs + compare_raster(path, ts) + compare_pixels(path, path))]
    if path.lower().endswith((".h5", ".he5")):
        # GDAL's metadata of an HDF5 container: every group's attributes and its datasets'.
        tmd = {k: v for k, v in t["metadata"]}
        gmd = g.GetMetadata()
        if tmd != gmd:
            keys = sorted(set(tmd) ^ set(gmd)) or sorted(k for k in gmd if tmd.get(k) != gmd[k])
            diffs.append(f"metadata: {len(keys)} items differ, e.g. {keys[0]}: {tmd.get(keys[0])!r} vs {gmd.get(keys[0])!r}")
    out.append(("subdataset list", diffs))
    for name, desc in gsubs:
        if desc.endswith("()"):
            continue  # strings: GDAL reads them as Float32 garbage, terrio declines them
        try:
            gdal.Open(name)
        except RuntimeError:
            continue  # what GDAL cannot open as a raster
        ts = terrio_info(name, stats=True)
        short = name.rsplit(":", 1)[-1]
        if isinstance(ts, str):
            out.append((short, [f"terrio error: {ts}"]))
            continue
        try:
            out.append((short, compare_raster(name, ts) + compare_pixels(name, name)))
        except Exception as e:  # noqa: BLE001 - report and continue with the next subdataset
            out.append((short, [f"GDAL error: {e}"]))
    return out


def main():
    """Compare every dataset in the data directory (or the given files)."""
    if sys.argv[1] == "--roundtrip":
        roundtrip_main(sys.argv[2:])
        return
    if sys.argv[1] == "--subdatasets":
        data = sys.argv[2]
        files = sys.argv[3:] or sorted(f for f in os.listdir(data) if os.path.splitext(f)[1].lower() in CONTAINER_EXT)
        failed = 0
        for name in files:
            for sub, diffs in compare_subdatasets(os.path.join(data, name)):
                if diffs:
                    failed += 1
                    print(f"DIFF  {name} {sub}: " + "; ".join(diffs))
                else:
                    print(f"OK    {name} {sub}")
        sys.exit(1 if failed else 0)
    args = [a for a in sys.argv[1:] if a != "--pixels"]
    pixels = "--pixels" in sys.argv[1:]
    data = args[0]
    # The data directory's files, and the GeoTIFFs of each compression in gtiff_codecs/.
    codecs = os.path.join(data, "gtiff_codecs")
    extra = [f"gtiff_codecs/{n}" for n in sorted(os.listdir(codecs))] if os.path.isdir(codecs) else []
    files = args[1:] or sorted(os.listdir(data)) + extra
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
