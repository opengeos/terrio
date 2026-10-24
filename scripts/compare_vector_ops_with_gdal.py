"""Compare terrio's vector conversion options, rasterize and polygonize with GDAL's.

Runs ``terrio convert`` against ``ogr2ogr``, ``terrio rasterize`` against ``gdal_rasterize`` and
``terrio polygonize`` against ``gdal_polygonize.py`` with equivalent options on the real-world
files of ``scripts/fetch_test_data.sh``, and reports how the outputs agree:

* convert: layer name, feature count, field names, attribute values and geometry types, feature
  by feature;
* rasterize: size, geotransform and the number of pixels that differ;
* polygonize: polygon count, polygon count and total area per value, validity, and whether
  rasterizing terrio's polygons reproduces the input raster.

Usage::

    python scripts/compare_vector_ops_with_gdal.py ../terrio-data [--terrio target/release/terrio]
"""

from __future__ import annotations

import argparse
import json
import math
import shutil
import subprocess
import sys
import tempfile
from collections import defaultdict
from pathlib import Path

import numpy as np
from osgeo import gdal, ogr, osr

gdal.UseExceptions()
ogr.UseExceptions()


def run(cmd: list[str]) -> None:
    """Runs a command, raising with its error output when it fails.

    Args:
        cmd: The program and its arguments.
    """
    r = subprocess.run([str(c) for c in cmd], capture_output=True, text=True)
    if r.returncode != 0:
        raise RuntimeError(f"{' '.join(map(str, cmd))}\n{r.stderr.strip()}")


def layer_rows(path: Path) -> dict:
    """Reads the first layer of a vector dataset.

    Args:
        path: The dataset.

    Returns:
        The layer name, field names, and per feature the attribute values and geometry
        (type name, area, length, envelope).
    """
    ds = ogr.Open(str(path))
    lyr = ds.GetLayer(0)
    defn = lyr.GetLayerDefn()
    fields = [defn.GetFieldDefn(i).GetName() for i in range(defn.GetFieldCount())]
    rows = []
    for f in lyr:
        g = f.GetGeometryRef()
        geom = None
        if g is not None:
            geom = (g.GetGeometryName(), g.GetArea(), g.Length(), g.GetEnvelope())
        rows.append(([f.GetField(i) for i in range(len(fields))], geom))
    return {"name": lyr.GetName(), "fields": fields, "rows": rows}


def close(a, b, tol: float = 1e-7) -> bool:
    """Compares two attribute values or geometry summaries with a relative tolerance.

    Args:
        a: The first value.
        b: The second value.
        tol: The relative tolerance for numbers.

    Returns:
        Whether they are equal, numbers within the tolerance.
    """
    # List fields come back as JSON text, formatted differently.
    if isinstance(a, str) and isinstance(b, str) and a.startswith("[") and b.startswith("["):
        try:
            a, b = json.loads(a), json.loads(b)
        except ValueError:
            pass
    if isinstance(a, (tuple, list)) and isinstance(b, (tuple, list)):
        return len(a) == len(b) and all(close(x, y, tol) for x, y in zip(a, b))
    if isinstance(a, (int, float)) and isinstance(b, (int, float)) and not isinstance(a, bool):
        return math.isclose(a, b, rel_tol=tol, abs_tol=tol)
    return a == b


def compare_sql(data: Path, work: Path, terrio: Path) -> list[str]:
    """Compare richer SQL against OGRSQL on Natural Earth, including duplicate join keys.

    Integer32 and Integer64 both map to Terrio's Integer field type. Values, nulls,
    names, logical field types, CRS and complete geometry WKB must otherwise agree.
    """
    source = work / "sql_source.gpkg"
    gdal.VectorTranslate(str(source), str(data / "countries.geojson"), layerName="countries")
    ds = ogr.Open(str(source), update=1)
    countries = ds.GetLayerByName("countries")
    lookup = ds.CreateLayer("lookup", geom_type=ogr.wkbNone)
    lookup.CreateField(ogr.FieldDefn("continent", ogr.OFTString))
    lookup.CreateField(ogr.FieldDefn("label", ogr.OFTString))
    lookup.CreateField(ogr.FieldDefn("amount", ogr.OFTReal))
    for name in ("Africa", "Asia", "Europe", None):
        for label in ("first", "duplicate"):
            f = ogr.Feature(lookup.GetLayerDefn())
            f.SetField("continent", name) if name is not None else f.SetFieldNull("continent")
            f.SetField("label", label)
            if name is not None:
                f.SetField("amount", 1.5)
            lookup.CreateFeature(f)
    ds.CopyLayer(lookup, "lookup_copy")
    ds = None

    def snapshot(layer):
        defn = layer.GetLayerDefn()
        fields = []
        for i in range(defn.GetFieldCount()):
            f = defn.GetFieldDefn(i)
            ty = "Integer" if f.GetType() in (ogr.OFTInteger, ogr.OFTInteger64) else f.GetTypeName()
            fields.append((f.GetName(), ty))
        srs = layer.GetSpatialRef()
        rows = [(list(f.items().values()), bytes(f.GetGeometryRef().ExportToWkb()) if f.GetGeometryRef() else None) for f in layer]
        return fields, rows, srs.ExportToWkt() if srs else None

    cases = [
        ("computed integer", "SELECT ADMIN, POP_EST*2 AS doubled, POP_EST/3 AS third FROM countries LIMIT 20"),
        ("computed float", "SELECT ADMIN, POP_EST/1000000.0 AS millions FROM countries WHERE POP_EST>0 ORDER BY POP_EST DESC LIMIT 10"),
        ("computed predicate", "SELECT ADMIN, POP_EST>100000000 AS large FROM countries LIMIT 20"),
        ("computed nulls", "SELECT amount*2 AS doubled, amount/2 AS half, label='FIRST' AS same FROM lookup"),
        ("computed empty schema", "SELECT POP_EST*2 AS doubled FROM countries WHERE POP_EST<0"),
        ("distinct", "SELECT DISTINCT CONTINENT FROM countries ORDER BY CONTINENT DESC LIMIT 5"),
        ("distinct null and case", "SELECT DISTINCT continent FROM lookup ORDER BY continent"),
        ("summary numeric", "SELECT COUNT(*) AS n, SUM(POP_EST) AS total, AVG(POP_EST) AS average, MIN(POP_EST) AS smallest, MAX(POP_EST) AS largest FROM countries"),
        ("summary filtered", "SELECT COUNT(POP_EST) AS n, SUM(POP_EST) AS total FROM countries WHERE CONTINENT='Asia'"),
        ("summary empty", "SELECT COUNT(*) AS n, SUM(POP_EST) AS total, AVG(POP_EST) AS average, MIN(POP_EST) AS smallest FROM countries WHERE POP_EST<0"),
        ("summary nulls", "SELECT COUNT(continent) AS n, COUNT(DISTINCT continent) AS kinds, MIN(label) AS first FROM lookup"),
        ("left join duplicate keys", "SELECT c.ADMIN, l.label, c.POP_EST*2 AS doubled FROM countries c LEFT JOIN lookup l ON c.CONTINENT=l.continent ORDER BY POP_EST DESC LIMIT 20"),
        ("left join unmatched", "SELECT countries.ADMIN, lookup.label FROM countries LEFT JOIN lookup ON countries.CONTINENT=lookup.continent"),
        ("left join wildcards", "SELECT c.*, l.* FROM countries c LEFT JOIN lookup l ON c.CONTINENT=l.continent LIMIT 3"),
        ("join null keys", "SELECT a.label, b.label AS joined FROM lookup a LEFT JOIN lookup_copy b ON a.continent=b.continent"),
    ]
    lines = []
    for label, query in cases:
        try:
            ds = ogr.Open(str(source))
            expected_layer = ds.ExecuteSQL(query, dialect="OGRSQL")
            expected = snapshot(expected_layer)
            ds.ReleaseResultSet(expected_layer)
            ds = None
            errors = []
            for command in ("sql", "convert"):
                out = work / f"sql_{command}.gpkg"
                out.unlink(missing_ok=True)
                run([terrio, command, source, out, "--query" if command == "sql" else "--sql", query])
                actual_ds = ogr.Open(str(out))
                actual = snapshot(actual_ds.GetLayer(0))
                actual_ds = None
                if actual[0] != expected[0]:
                    errors.append(f"{command} fields {actual[0]} vs {expected[0]}")
                if not close(actual[1], expected[1]):
                    errors.append(f"{command} rows differ")
                if bool(actual[2]) != bool(expected[2]) or (
                    actual[2] and not osr.SpatialReference(wkt=actual[2]).IsSame(osr.SpatialReference(wkt=expected[2]))
                ):
                    errors.append(f"{command} CRS retention differs")
            lines.append(f"{label:<28} " + ("DIFF " + "; ".join(errors) if errors else "OK"))
        except Exception as exc:
            lines.append(f"{label:<28} ERROR {exc}")
    return lines


def compare_convert(data: Path, work: Path, terrio: Path) -> list[str]:
    """Compares ``terrio convert`` with ``ogr2ogr`` on vector options.

    Args:
        data: The test data directory.
        work: A scratch directory.
        terrio: The terrio executable.

    Returns:
        One report line per case.
    """
    countries = data / "countries.geojson"
    places = data / "ne_places/ne_10m_populated_places_simple.shp"
    counties = data / "UScounties.gpkg"
    cables = data / "cables.geojson"
    tn = data / "TN_Counties.geojson"
    cases = [
        ("where numeric", countries, ["-where", "POP_EST > 50000000"]),
        ("where AND/OR/NOT", countries, ["-where", "NOT (CONTINENT = 'Africa' OR CONTINENT = 'Asia') AND POP_EST >= 1e7"]),
        ("where IN", countries, ["-where", "continent IN ('Europe', 'Oceania')"]),
        ("where LIKE", countries, ["-where", "ADMIN LIKE '%Republic%' OR ADMIN LIKE '%republic%'"]),
        ("where ILIKE", countries, ["-where", "ADMIN ILIKE '%REPUBLIC%'"]),
        ("where LIKE _", places, ["-where", "name LIKE 'S_n %'"]),
        ("where BETWEEN", places, ["-where", "pop_max BETWEEN 1000000 AND 2000000"]),
        ("where IS NULL", places, ["-where", "namealt IS NULL OR namealt = ''"]),
        ("where NOT IN, <>", counties, ["-where", "STATE NOT IN ('TX', 'GA') AND LSAD <> 'County'"]),
        ("where arithmetic", tn, ["-where", "POPULATION / SQMI > 500"]),
        ("spat", countries, ["-spat", "-10", "35", "30", "60"]),
        ("spat on lines", cables, ["-spat", "-80", "20", "-60", "40"]),
        ("spat on points", places, ["-spat", "100", "-10", "120", "10"]),
        ("spat + spat_srs", counties, ["-spat", "-9000000", "4000000", "-8500000", "4500000", "-spat_srs", "EPSG:3857"]),
        ("select", countries, ["-select", "ADMIN,pop_est,Continent"]),
        ("limit", places, ["-limit", "100"]),
        ("where + select + limit", places, ["-where", "adm0name = 'China'", "-select", "name,pop_max", "-limit", "25"]),
        ("nln", countries, ["-nln", "world"]),
        ("nlt PROMOTE_TO_MULTI", countries, ["-nlt", "PROMOTE_TO_MULTI"]),
        ("nlt MULTILINESTRING", tn, ["-nlt", "MULTILINESTRING"]),
        ("nlt POLYGON", tn, ["-nlt", "POLYGON"]),
        ("nlt CONVERT_TO_LINEAR", countries, ["-nlt", "CONVERT_TO_LINEAR"]),
        ("explodecollections", countries, ["-explodecollections"]),
        ("explode + limit", cables, ["-explodecollections", "-limit", "50"]),
        ("t_srs", tn, ["-t_srs", "EPSG:3857"]),
        ("s_srs + t_srs", tn, ["-s_srs", "EPSG:4269", "-t_srs", "EPSG:5070"]),
        ("a_srs", tn, ["-a_srs", "EPSG:4269"]),
        ("sql", countries, ["-sql", "SELECT ADMIN, POP_EST AS pop FROM ne_110m_admin_0_countries WHERE CONTINENT = 'Asia' ORDER BY POP_EST DESC"]),
    ]
    lines = []
    for label, src, gdal_args in cases:
        # GeoPackage outputs: terrio's GeoJSON writer always writes WGS 84 coordinates.
        g_out, t_out = work / "g.gpkg", work / "t.gpkg"
        for p in (g_out, t_out):
            p.unlink(missing_ok=True)
        terrio_args = []
        i = 0
        while i < len(gdal_args):
            a = gdal_args[i]
            if a == "-explodecollections":
                terrio_args.append("--explodecollections")
                i += 1
                continue
            n = 4 if a == "-spat" else 1
            terrio_args += ["--" + a.lstrip("-").replace("_", "-")] + gdal_args[i + 1 : i + 1 + n]
            i += 1 + n
        try:
            run(["ogr2ogr", "-f", "GPKG", g_out, src, *gdal_args])
            run([terrio, "convert", src, t_out, *terrio_args])
        except RuntimeError as e:
            lines.append(f"{label:<26} ERROR {e}")
            continue
        g, t = layer_rows(g_out), layer_rows(t_out)
        g_fields = [f.lower() for f in g["fields"]]
        t_fields = [f.lower() for f in t["fields"]]
        problems = []
        if len(g["rows"]) != len(t["rows"]):
            problems.append(f"count {len(t['rows'])} != {len(g['rows'])}")
        if g_fields != t_fields:
            problems.append(f"fields {t['fields']} != {g['fields']}")
        g_rows, t_rows = g["rows"], t["rows"]
        if not all(rows_match(a, b) for a, b in zip(g_rows, t_rows)):
            # GDAL reads GeoPackages through their spatial index when filtering: compare unordered.
            g_rows, t_rows = sorted(g_rows, key=repr), sorted(t_rows, key=repr)
        bad_attr = bad_geom = encoding = 0
        for (ga, gg), (ta, tg) in zip(g_rows, t_rows):
            if g_fields == t_fields and not all(value_match(x, y) for x, y in zip(ga, ta)):
                if all(value_match(x, y) or mojibake(x, y) for x, y in zip(ga, ta)):
                    encoding += 1
                else:
                    bad_attr += 1
            if (gg is None) != (tg is None) or (gg and not (gg[0] == tg[0] and close(gg[1:], tg[1:], 1e-6))):
                bad_geom += 1
        if bad_attr:
            problems.append(f"{bad_attr} features with other attributes")
        if bad_geom:
            problems.append(f"{bad_geom} features with other geometries")
        status = "OK" if not problems else "DIFF " + "; ".join(problems)
        if encoding:
            status += f" ({encoding} features differ only by the GeoJSON reader's UTF-8 decoding)"
        lines.append(f"{label:<26} {len(g['rows']):>6} features, {len(g['fields']):>3} fields  {status}")
    return lines


def compare_append(data: Path, work: Path, terrio: Path) -> list[str]:
    """Compares ``--append`` and ``--overwrite`` on GeoPackages with ogr2ogr's.

    Args:
        data: The test data directory.
        work: A scratch directory.
        terrio: The terrio executable.

    Returns:
        One report line.
    """
    countries, tn = data / "countries.geojson", data / "TN_Counties.geojson"
    steps = [
        (countries, ["-nln", "world"], []),
        (tn, ["-nln", "tn"], ["-append"]),
        (tn, ["-nln", "tn", "-where", "POPULATION > 100000"], ["-append"]),
        (tn, ["-nln", "world", "-limit", "10"], ["-overwrite"]),
        (countries, ["-nln", "world", "-limit", "5"], ["-append"]),
    ]
    g_out, t_out = work / "ga.gpkg", work / "ta.gpkg"
    for p in (g_out, t_out):
        p.unlink(missing_ok=True)
    try:
        for src, opts, mode in steps:
            run(["ogr2ogr", *mode, "-f", "GPKG", g_out, src, *opts])
            run([terrio, "convert", src, t_out, *[o.replace("-", "--", 1).replace("_", "-") for o in mode + opts]])
    except RuntimeError as e:
        return [f"{'append/overwrite':<26} ERROR {e}"]
    summary = []
    for path in (g_out, t_out):
        ds = ogr.Open(str(path))
        summary.append(sorted((ds.GetLayer(i).GetName(), ds.GetLayer(i).GetFeatureCount()) for i in range(ds.GetLayerCount())))
    status = "OK" if summary[0] == summary[1] else f"DIFF {summary[1]} != {summary[0]}"
    return [f"{'append/overwrite':<26} layers {summary[0]}  {status}"]


def value_match(a, b) -> bool:
    """Compares two attribute values, taking nulls and empty strings as equal.

    Args:
        a: GDAL's value.
        b: terrio's value.

    Returns:
        Whether they match.
    """
    return close(a, b) or (a in (None, "") and b in (None, ""))


def mojibake(a, b) -> bool:
    """Whether ``b`` is ``a`` UTF-8 encoded then decoded as Latin-1.

    Args:
        a: GDAL's value.
        b: terrio's value.

    Returns:
        Whether the values differ only by that decoding.
    """
    try:
        return isinstance(a, str) and isinstance(b, str) and a.encode("utf-8").decode("latin-1") == b
    except UnicodeError:
        return False


def rows_match(a: tuple, b: tuple) -> bool:
    """Compares two features' attributes and geometry summaries.

    Args:
        a: GDAL's feature.
        b: terrio's feature.

    Returns:
        Whether they match.
    """
    return all(value_match(x, y) or mojibake(x, y) for x, y in zip(a[0], b[0])) and close(a[1], b[1], 1e-6)


def raster_array(path: Path) -> tuple[np.ndarray, tuple, tuple, float | None]:
    """Reads a raster's first band.

    Args:
        path: The raster.

    Returns:
        The pixels, the geotransform, the size and the nodata value.
    """
    ds = gdal.Open(str(path))
    b = ds.GetRasterBand(1)
    return b.ReadAsArray(), ds.GetGeoTransform(), (ds.RasterXSize, ds.RasterYSize), b.GetNoDataValue()


def compare_rasterize(data: Path, work: Path, terrio: Path) -> list[str]:
    """Compares ``terrio rasterize`` with ``gdal_rasterize``.

    Args:
        data: The test data directory.
        work: A scratch directory.
        terrio: The terrio executable.

    Returns:
        One report line per case.
    """
    countries = data / "countries.geojson"
    counties = data / "UScounties.gpkg"
    tn = data / "TN_Counties.geojson"
    cables = data / "cables.geojson"
    places = data / "ne_places/ne_10m_populated_places_simple.shp"
    nyc = data / "nyc_buildings.geojson"
    nlcd = data / "nlcd_knoxville.tif"
    # (label, input, gdal_rasterize options, terrio rasterize options)
    cases = [
        ("countries burn", countries, "-burn 1 -tr 0.1 0.1 -ot Byte", "--burn 1 --tr 0.1 0.1 --ot Byte"),
        ("countries all touched", countries, "-burn 1 -tr 0.5 0.5 -at -ot Byte", "--burn 1 --tr 0.5 0.5 --at --ot Byte"),
        ("countries attribute", countries, "-a POP_EST -ts 2000 800", "-a POP_EST --ts 2000 800"),
        ("countries where, te", countries, "-burn 7 -where CONTINENT='Europe' -te -30 30 50 75 -tr 0.05 0.05 -ot Int16 -init -1 -a_nodata -1",
         "--burn 7 --where CONTINENT='Europe' --te -30 30 50 75 --tr 0.05 0.05 --ot Int16 --init -1 --a-nodata -1"),
        ("countries tap", countries, "-burn 1 -tr 0.3 0.3 -tap -ot Byte", "--burn 1 --tr 0.3 0.3 --tap --ot Byte"),
        ("US counties", counties, "-burn 1 -tr 0.01 0.01 -te -125 24 -66 50 -ot Byte", "--burn 1 --tr 0.01 0.01 --te -125 24 -66 50 --ot Byte"),
        ("US counties at", counties, "-burn 1 -tr 0.02 0.02 -te -125 24 -66 50 -at -ot Byte", "--burn 1 --tr 0.02 0.02 --te -125 24 -66 50 --at --ot Byte"),
        ("TN counties attribute", tn, "-a POPULATION -tr 0.002 0.002 -ot Int32", "-a POPULATION --tr 0.002 0.002 --ot Int32"),
        ("cables lines", cables, "-burn 1 -tr 0.05 0.05 -ot Byte", "--burn 1 --tr 0.05 0.05 --ot Byte"),
        ("cables lines at", cables, "-burn 1 -tr 0.1 0.1 -at -ot Byte", "--burn 1 --tr 0.1 0.1 --at --ot Byte"),
        ("cables add", cables, "-burn 1 -tr 0.25 0.25 -add -ot UInt16", "--burn 1 --tr 0.25 0.25 --add --ot UInt16"),
        ("places points", places, "-burn 1 -tr 0.1 0.1 -ot Byte", "--burn 1 --tr 0.1 0.1 --ot Byte"),
        ("places add (count)", places, "-burn 1 -tr 1 1 -add -ot UInt16", "--burn 1 --tr 1 1 --add --ot UInt16"),
        ("places attribute", places, "-a pop_max -tr 0.5 0.5 -ot Float32", "-a pop_max --tr 0.5 0.5 --ot Float32"),
        ("nyc buildings fine", nyc, "-burn 1 -tr 0.00001 0.00001 -ot Byte", "--burn 1 --tr 0.00001 0.00001 --ot Byte"),
        ("nyc buildings at", nyc, "-burn 1 -tr 0.00002 0.00002 -at -ot Byte", "--burn 1 --tr 0.00002 0.00002 --at --ot Byte"),
        ("nyc buildings add", nyc, "-burn 1 -tr 0.00005 0.00005 -add -at -ot Byte", "--burn 1 --tr 0.00005 0.00005 --add --at --ot Byte"),
    ]
    lines = []
    for label, src, g_opts, t_opts in cases:
        g_out, t_out = work / "g.tif", work / "t.tif"
        for p in (g_out, t_out):
            p.unlink(missing_ok=True)
        try:
            run(["gdal_rasterize", "-q", *g_opts.split(), src, g_out])
            run([terrio, "rasterize", src, t_out, *t_opts.split()])
        except RuntimeError as e:
            lines.append(f"{label:<24} ERROR {e}")
            continue
        g, ggt, gsize, gnd = raster_array(g_out)
        t, tgt, tsize, tnd = raster_array(t_out)
        if gsize != tsize:
            lines.append(f"{label:<24} DIFF size {tsize} != {gsize}")
            continue
        problems = []
        if not close(ggt, tgt, 1e-12):
            problems.append(f"geotransform {tgt} != {ggt}")
        if not (gnd == tnd or (gnd is not None and tnd is not None and math.isnan(gnd) and math.isnan(tnd))):
            problems.append(f"nodata {tnd} != {gnd}")
        diff = int((g != t).sum())
        burned = int((g != g.flat[0]).sum()) if g.size else 0
        status = "OK" if not problems and diff == 0 else "DIFF " + "; ".join(problems)
        lines.append(f"{label:<24} {gsize[0]}x{gsize[1]}, {diff} of {g.size} pixels differ ({burned} burned)  {status}")
    # Burning into the grid of an existing raster, reprojecting the vectors. terrio does not read
    # the user-defined Albers CRS of the NLCD file: label the grid as NAD83 / Conus Albers.
    like, g_out, t_out = work / "like.tif", work / "g.tif", work / "t.tif"
    for p in (like, g_out, t_out):
        p.unlink(missing_ok=True)
    run(["gdal_translate", "-q", "-a_srs", "EPSG:5070", "-a_nodata", "0", nlcd, like])
    shutil.copy(like, g_out)
    ds = gdal.Open(str(g_out), gdal.GA_Update)
    ds.GetRasterBand(1).Fill(0)
    ds = None
    label = "TN counties, like NLCD"
    try:
        run(["gdal_rasterize", "-q", "-a", "COUNTY_FIP", tn, g_out])
        run([terrio, "rasterize", tn, t_out, "-a", "COUNTY_FIP", "--like", like, "--ot", "Byte"])
        g = raster_array(g_out)[0]
        t = raster_array(t_out)[0]
        diff = int((g != t).sum())
        lines.append(f"{label:<24} {g.shape[1]}x{g.shape[0]}, {diff} of {g.size} pixels differ ({int((g != 0).sum())} burned, reprojected)")
    except RuntimeError as e:
        lines.append(f"{label:<24} ERROR {e}")
    return lines


def polygon_summary(path: Path, field: str = "DN") -> dict:
    """Summarizes polygonize output.

    Args:
        path: The vector dataset.
        field: The value field.

    Returns:
        The polygon count, polygon count and total area per value, hole count and the number of
        invalid geometries.
    """
    ds = ogr.Open(str(path))
    lyr = ds.GetLayer(0)
    count, area = defaultdict(int), defaultdict(float)
    holes = invalid = 0
    for f in lyr:
        g = f.GetGeometryRef()
        v = f.GetField(field)
        count[v] += 1
        area[v] += g.GetArea()
        holes += g.GetGeometryCount() - 1
        invalid += not g.IsValid()
    return {"n": sum(count.values()), "count": dict(count), "area": dict(area), "holes": holes, "invalid": invalid}


def compare_polygonize(data: Path, work: Path, terrio: Path) -> list[str]:
    """Compares ``terrio polygonize`` with ``gdal_polygonize.py``.

    Args:
        data: The test data directory.
        work: A scratch directory.
        terrio: The terrio executable.

    Returns:
        One report line per case.
    """
    cases = [
        ("NLCD land cover", data / "nlcd_knoxville.tif", [], []),
        ("NLCD land cover, 8", data / "nlcd_knoxville.tif", ["-8"], ["--8"]),
        ("OPERA DSWx water", data / "OPERA_L3_DSWx_WTR.tif", [], []),
        ("OPERA DSWx, nomask", data / "OPERA_L3_DSWx_WTR.tif", ["-nomask"], ["--nomask"]),
    ]
    lines = []
    gdal_pol = shutil.which("gdal_polygonize.py") or shutil.which("gdal_polygonize")
    for label, src, g_opts, t_opts in cases:
        g_out, t_out = work / "g.gpkg", work / "t.gpkg"
        for p in (g_out, t_out):
            p.unlink(missing_ok=True)
        try:
            run([gdal_pol, "-q", *g_opts, src, "-f", "GPKG", g_out])
            run([terrio, "polygonize", src, t_out, *t_opts])
        except RuntimeError as e:
            lines.append(f"{label:<20} ERROR {e}")
            continue
        g, t = polygon_summary(g_out), polygon_summary(t_out)
        problems = []
        if g["n"] != t["n"]:
            problems.append(f"count {t['n']} != {g['n']}")
        if g["count"] != t["count"]:
            problems.append("counts per value differ")
        if g["area"].keys() != t["area"].keys() or not all(math.isclose(g["area"][k], t["area"][k], rel_tol=1e-9) for k in g["area"]):
            problems.append("areas per value differ")
        # Rasterizing terrio's polygons onto the input grid must give back the input.
        rt = work / "roundtrip.tif"
        rt.unlink(missing_ok=True)
        run(["gdal_rasterize", "-q", "-a", "DN", "-init", "-1", "-a_nodata", "-1", "-ot", "Int32", "-te",
             *map(str, extent(src)), "-ts", *map(str, size(src)), t_out, rt])
        src_arr, _, _, nd = raster_array(src)
        back = raster_array(rt)[0]
        valid = np.ones(src_arr.shape, bool) if ("--nomask" in t_opts or nd is None) else src_arr != nd
        mismatch = int((back[valid] != src_arr[valid]).sum()) + int((back[~valid] != -1).sum())
        status = "OK" if not problems and mismatch == 0 else "DIFF " + "; ".join(problems)
        lines.append(
            f"{label:<20} {g['n']} polygons ({len(g['count'])} values), holes {t['holes']} vs {g['holes']}, "
            f"invalid {t['invalid']} vs {g['invalid']}, round trip: {mismatch} pixels differ  {status}"
        )
    return lines


def extent(path: Path) -> tuple[float, float, float, float]:
    """The extent of a north-up raster.

    Args:
        path: The raster.

    Returns:
        ``(min_x, min_y, max_x, max_y)``.
    """
    ds = gdal.Open(str(path))
    gt = ds.GetGeoTransform()
    return gt[0], gt[3] + gt[5] * ds.RasterYSize, gt[0] + gt[1] * ds.RasterXSize, gt[3]


def size(path: Path) -> tuple[int, int]:
    """The size of a raster.

    Args:
        path: The raster.

    Returns:
        ``(width, height)``.
    """
    ds = gdal.Open(str(path))
    return ds.RasterXSize, ds.RasterYSize


def main() -> int:
    """Runs the comparisons and prints the report.

    Returns:
        The process exit status: 1 when a case differs or fails.
    """
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("data", type=Path, help="the test data directory")
    parser.add_argument("--terrio", type=Path, default=Path(__file__).resolve().parents[1] / "target/release/terrio")
    args = parser.parse_args()
    lines = []
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        sections = [
            ("convert vs ogr2ogr", compare_convert),
            ("sql vs OGRSQL", compare_sql),
            ("convert --append/--overwrite vs ogr2ogr", compare_append),
            ("rasterize vs gdal_rasterize", compare_rasterize),
            ("polygonize vs gdal_polygonize", compare_polygonize),
        ]
        for title, fn in sections:
            print(f"## {title}")
            for line in fn(args.data, work, args.terrio):
                print(line, flush=True)
                lines.append(line)
            print()
    return 1 if any(" DIFF " in l or " ERROR " in l for l in lines) else 0


if __name__ == "__main__":
    sys.exit(main())
