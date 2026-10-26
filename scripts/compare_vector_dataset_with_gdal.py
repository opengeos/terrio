"""Compare terrio's dataset-level vector commands with GDAL on real data.

Usage: python scripts/compare_vector_dataset_with_gdal.py ../terrio-data --terrio target/release/terrio

Commands compared (GDAL 3.12 has the gdal vector concat, partition, set-field-type and
make-point commands; export-schema and create are GDAL 3.13 commands, compared with what
GDAL 3.12 reports of the same files):

- export-schema: the OGR_SCHEMA of GeoPackage, Shapefile, GeoJSON, FlatGeobuf and GeoParquet
  files, against ``ogrinfo -json``'s layer descriptions (the same JSON members, as
  ``gdal vector export-schema`` takes them from ``ogrinfo -schema``): geometry and FID
  column names, geometry types, CRS codes, field names, types, subtypes, widths, precisions,
  nullability, unique constraints and defaults.
- create: datasets created from a template (``--like``), an exported schema and explicit
  fields, read back by GDAL, against ``ogr2ogr -limit 0`` and GDAL's API.
- set-field-type: conversions of real fields and of a matrix of edge values between every
  type, against ``gdal vector set-field-type``: types, kept features and values.
- make-point: points from the coordinate columns of a GPS trace (CSV), against
  ``gdal vector make-point``.
- concat: merge-per-layer-name, stack and single modes, source-layer fields, the
  intersection strategy and reprojection, against ``gdal vector concat`` and ``ogrmerge.py``.
- partition: Hive and flat partitions by string and integer fields, feature limits and name
  patterns, against ``gdal vector partition``: file paths, feature counts and features.
  (GDAL 3.12.2 crashes with ``--max-file-size``; file-size limits are covered by unit tests.)

Values are compared exactly; reprojected coordinates within 1e-9 degrees or 1e-6 metres.
Known differences of terrio's data model are reported in the case details: Int16 and Float32
fields are stored as 32-bit integers and doubles in GeoPackages (MEDIUMINT and REAL
columns, where GDAL writes SMALLINT and FLOAT).
"""

from __future__ import annotations

import argparse
import json
import math
import os
from pathlib import Path
import shutil
import subprocess
import sys

from osgeo import gdal, ogr

gdal.UseExceptions()
ogr.UseExceptions()

IDENTICAL = "identical"
TOLERANCE = "within tolerance"
DIFFERS = "differs"


def run(cmd, check=True):
    """Runs a command.

    Args:
        cmd: The command line.
        check: Raise when it fails.

    Returns:
        The completed process.
    """
    p = subprocess.run([str(c) for c in cmd], capture_output=True, text=True)
    if check and p.returncode != 0:
        raise RuntimeError(f"{' '.join(map(str, cmd))} failed:\n{p.stderr}{p.stdout}")
    return p


def field_type(fd):
    """GDAL's type name of a field definition, with its subtype.

    Args:
        fd: The ``ogr.FieldDefn``.

    Returns:
        A name such as ``Integer`` or ``Integer(Boolean)``.
    """
    name = fd.GetTypeName()
    if fd.GetSubType() != ogr.OFSTNone:
        name += f"({ogr.GetFieldSubTypeName(fd.GetSubType())})"
    return name


def dump(path, layer_names=None):
    """The layers of a dataset as GDAL reads them.

    Args:
        path: The dataset.
        layer_names: Only these layers.

    Returns:
        A dict of layer name to ``{"fields": [(name, type)], "crs": code, "features":
        [(geometry, values)]}``, geometries as coordinate lists.
    """
    ds = ogr.Open(str(path))
    out = {}
    for i in range(ds.GetLayerCount()):
        layer = ds.GetLayer(i)
        if layer_names and layer.GetName() not in layer_names:
            continue
        defn = layer.GetLayerDefn()
        fields = [(defn.GetFieldDefn(k).GetName(), field_type(defn.GetFieldDefn(k))) for k in range(defn.GetFieldCount())]
        srs = layer.GetSpatialRef()
        code = None
        if srs is not None:
            srs.AutoIdentifyEPSG()
            code = srs.GetAuthorityCode(None)
        features = []
        for f in layer:
            g = f.GetGeometryRef()
            features.append((g.ExportToIsoWkt() if g else None, [f.GetField(k) for k in range(defn.GetFieldCount())]))
        out[layer.GetName()] = {"fields": fields, "crs": code, "features": features}
    return out


def coords(wkt):
    """The numbers of a WKT geometry.

    Args:
        wkt: The WKT, or None.

    Returns:
        A list of floats.
    """
    if wkt is None:
        return []
    out = []
    for tok in wkt.replace("(", " ").replace(")", " ").replace(",", " ").split():
        try:
            out.append(float(tok))
        except ValueError:
            pass
    return out


def same_value(a, b):
    """Whether two field values are equal (NaN equal to NaN).

    Args:
        a: A value.
        b: A value.

    Returns:
        Whether they are equal.
    """
    if isinstance(a, float) and isinstance(b, float):
        return a == b or (math.isnan(a) and math.isnan(b))
    return a == b


def compare_dumps(name, mine, ref, tolerance=0.0, ignore_types=(), check_crs=True):
    """Compares two dumps.

    Args:
        name: The case name.
        mine: terrio's output, dumped.
        ref: GDAL's output, dumped.
        tolerance: The largest coordinate difference accepted.
        ignore_types: Field types (as GDAL names them) whose difference is a known one.
        check_crs: Compare CRS codes.

    Returns:
        A case dict.
    """
    problems = []
    notes = []
    worst = 0.0
    if sorted(mine) != sorted(ref):
        problems.append(f"layers {sorted(mine)} vs {sorted(ref)}")
    for lname in sorted(set(mine) & set(ref)):
        a, b = mine[lname], ref[lname]
        if [f for f, _ in a["fields"]] != [f for f, _ in b["fields"]]:
            problems.append(f"{lname}: fields {[f for f, _ in a['fields']]} vs {[f for f, _ in b['fields']]}")
            continue
        for (fname, ta), (_, tb) in zip(a["fields"], b["fields"]):
            if ta != tb:
                if tb in ignore_types or ta in ignore_types:
                    notes.append(f"{lname}.{fname} {ta} (GDAL {tb})")
                else:
                    problems.append(f"{lname}.{fname}: type {ta} vs {tb}")
        if check_crs and a["crs"] != b["crs"]:
            problems.append(f"{lname}: CRS {a['crs']} vs {b['crs']}")
        if len(a["features"]) != len(b["features"]):
            problems.append(f"{lname}: {len(a['features'])} features vs {len(b['features'])}")
            continue
        bad_values = 0
        for k, ((ga, va), (gb, vb)) in enumerate(zip(a["features"], b["features"])):
            if not all(same_value(x, y) for x, y in zip(va, vb)):
                bad_values += 1
                if bad_values <= 3:
                    problems.append(f"{lname} feature {k}: {va} vs {vb}")
            if ga != gb:
                ca, cb = coords(ga), coords(gb)
                if len(ca) != len(cb) or (ga or "").split("(")[0] != (gb or "").split("(")[0]:
                    problems.append(f"{lname} feature {k}: geometry {str(ga)[:60]} vs {str(gb)[:60]}")
                    continue
                d = max((abs(x - y) for x, y in zip(ca, cb)), default=0.0)
                worst = max(worst, d)
        if bad_values > 3:
            problems.append(f"{lname}: {bad_values} features with different values")
    if worst > tolerance:
        problems.append(f"largest coordinate difference {worst:.3g} > {tolerance:.3g}")
    status = DIFFERS if problems else (TOLERANCE if worst > 0 or notes else IDENTICAL)
    detail = "; ".join(problems or ([f"largest coordinate difference {worst:.3g}"] if worst > 0 else []) + notes)
    features = sum(len(v["features"]) for v in ref.values())
    return {"name": name, "status": status, "detail": detail, "features": features, "max_coordinate_difference": worst}


# ------------------------------------------------------------------------------------------------
# export-schema
# ------------------------------------------------------------------------------------------------

SCHEMA_FIELD_KEYS = ("name", "type", "subType", "width", "precision", "nullable", "uniqueConstraint", "defaultValue")


def gdal_schema(path):
    """GDAL's OGR_SCHEMA of a dataset, from ``ogrinfo -json`` (as GDAL 3.13 exports it).

    Args:
        path: The dataset.

    Returns:
        The schema dict.
    """
    info = json.loads(run(["ogrinfo", "-json", "-so", "-nomd", "-nocount", "-noextent", "-al", path]).stdout)
    layers = []
    for l in info["layers"]:
        geoms = []
        for g in l.get("geometryFields", []):
            cs = g.get("coordinateSystem")
            authid = None
            if cs:
                ident = (cs.get("projjson") or {}).get("id")
                if ident:
                    authid = f"{ident['authority']}:{ident['code']}"
            geoms.append({"name": g["name"], "type": g["type"], "nullable": g["nullable"], "authid": authid})
        layers.append(
            {
                "name": l["name"],
                "fidColumnName": l.get("fidColumnName"),
                "geometryFields": geoms,
                "fields": [{k: f[k] for k in SCHEMA_FIELD_KEYS if k in f} for f in l["fields"]],
            }
        )
    return layers


def terrio_schema(terrio, path):
    """terrio's OGR_SCHEMA of a dataset, reduced to what :func:`gdal_schema` gives.

    Args:
        terrio: The terrio executable.
        path: The dataset.

    Returns:
        The schema dict.
    """
    doc = json.loads(run([terrio, "export-schema", path]).stdout)
    layers = []
    for l in doc["layers"]:
        assert l["schemaType"] == "Full"
        geoms = []
        for g in l.get("geometryFields", []):
            cs = g.get("coordinateSystem")
            geoms.append({"name": g["name"], "type": g["type"], "nullable": g["nullable"], "authid": cs.get("authid") if cs else None})
        layers.append(
            {
                "name": l["name"],
                "fidColumnName": l.get("fidColumnName"),
                "geometryFields": geoms,
                "fields": [{k: f[k] for k in SCHEMA_FIELD_KEYS if k in f} for f in l["fields"]],
            }
        )
    return layers


def compare_schemas(name, mine, ref, known=()):
    """Compares two reduced schemas member by member.

    Args:
        name: The case name.
        mine: terrio's schema.
        ref: GDAL's schema.
        known: ``(layer, field, member)`` differences that are known ones.

    Returns:
        A case dict.
    """
    problems, notes = [], []
    if [l["name"] for l in mine] != [l["name"] for l in ref]:
        return {"name": name, "status": DIFFERS, "detail": f"layers {[l['name'] for l in mine]} vs {[l['name'] for l in ref]}"}
    fields = 0
    for a, b in zip(mine, ref):
        for key in ("fidColumnName", "geometryFields"):
            if a[key] != b[key]:
                problems.append(f"{a['name']}.{key}: {a[key]} vs {b[key]}")
        if [f["name"] for f in a["fields"]] != [f["name"] for f in b["fields"]]:
            problems.append(f"{a['name']}: field names differ")
            continue
        for fa, fb in zip(a["fields"], b["fields"]):
            fields += 1
            for k in sorted(set(fa) | set(fb)):
                if fa.get(k) != fb.get(k):
                    msg = f"{a['name']}.{fa['name']}.{k}: {fa.get(k)} vs {fb.get(k)}"
                    (notes if (a["name"], fa["name"], k) in known or ("*", "*", k) in known else problems).append(msg)
    status = DIFFERS if problems else (TOLERANCE if notes else IDENTICAL)
    return {"name": name, "status": status, "detail": "; ".join(problems + notes), "fields": fields}


# ------------------------------------------------------------------------------------------------
# set-field-type edge matrix
# ------------------------------------------------------------------------------------------------


def make_matrix_source(path):
    """A GeoPackage of edge values of every type, written by GDAL.

    Args:
        path: The output path.
    """
    if path.exists():
        path.unlink()
    from osgeo import osr

    ds = ogr.GetDriverByName("GPKG").CreateDataSource(str(path))
    s = osr.SpatialReference()
    s.ImportFromEPSG(4326)
    layer = ds.CreateLayer("t", s, ogr.wkbPoint)
    defs = [("s", ogr.OFTString), ("r", ogr.OFTReal), ("i", ogr.OFTInteger), ("i64", ogr.OFTInteger64), ("d", ogr.OFTDate), ("dt", ogr.OFTDateTime), ("b", ogr.OFTInteger)]
    for n, t in defs:
        fd = ogr.FieldDefn(n, t)
        if n == "b":
            fd.SetSubType(ogr.OFSTBoolean)
        layer.CreateField(fd)
    rows = [
        ("12", 1.7, 5, 3000000000, "2020-01-02", "2020-01-02T03:04:05", 1),
        ("abc", -1.7, -5, -3000000000, None, "2020-01-02T03:04:05.123Z", 0),
        ("12abc", 3e10, 0, 7, "1999-12-31", None, 1),
        ("3.9", 0.1, 2147483647, 2147483648, None, None, None),
        ("2021-05-06", 1e300, None, None, None, None, 0),
        ("", None, 1, 1, None, None, 1),
        ("1e3", 2.5, 1, 1, None, None, 1),
        ("2021-05-06 07:08:09", 123456789.123, 1, 1, None, None, 1),
        (" 42 ", 1 / 3, -7, -7, None, None, 0),
        ("07:08:09", -2147483649.0, 0, 0, None, None, 0),
    ]
    for i, row in enumerate(rows):
        f = ogr.Feature(layer.GetLayerDefn())
        for (n, _), v in zip(defs, row):
            if v is not None:
                f.SetField(n, v)
        f.SetGeometry(ogr.CreateGeometryFromWkt(f"POINT ({i} {i})"))
        layer.CreateFeature(f)
    ds = None


# Known differences: terrio stores these subtypes in its own types (see the module docstring;
# GeoPackage JSON fields are written as TEXT without their gpkg_data_columns MIME type);
# GDAL keeps Integer values unchanged when only the subtype changes (Integer to Boolean or
# Int16), while terrio's booleans hold 0 or 1 and its Int16 values are clamped.
KNOWN_TYPES = ("Integer(Int16)", "Real(Float32)", "String(JSON)")


def main():
    """Runs the comparisons and writes the JSON report."""
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("data", type=Path)
    p.add_argument("--terrio", default=os.environ.get("TERRIO_BIN", "target/release/terrio"))
    p.add_argument("--work-dir", type=Path)
    p.add_argument("--json", type=Path)
    args = p.parse_args()
    data = args.data
    work = args.work_dir or data / "vector-dataset-validation"
    if work.exists():
        shutil.rmtree(work)
    work.mkdir(parents=True)
    t = args.terrio
    cases = []

    def case(c):
        cases.append(c)
        print(f"{c['status']:>16}  {c['name']}  {c.get('detail', '')[:200]}", file=sys.stderr)

    # -- export-schema ----------------------------------------------------------------------
    shp = data / "ne_countries" / "ne_110m_admin_0_countries.shp"
    for src in ["countries.gpkg", "UScounties.gpkg", "multi.gpkg", shp, data / "ne_places" / "ne_10m_populated_places_simple.shp",
                "countries.geojson", "countries.fgb", "countries.parquet", "TN_Counties.geojson"]:
        path = data / src
        if not path.exists():
            continue
        known = ()
        if path.suffix == ".parquet":
            # GeoParquet int64 columns are Integer64 in GDAL whatever their values.
            known = (("*", "*", "type"),)
        case(compare_schemas(f"export-schema-{path.name}", terrio_schema(t, path), gdal_schema(path), known))

    # -- create ------------------------------------------------------------------------------
    for src, layer in [("countries.gpkg", "countries"), ("UScounties.gpkg", "UScounties")]:
        ref = work / f"create-ref-{layer}.gpkg"
        run(["ogr2ogr", "-limit", "0", ref, data / src, layer])
        mine = work / f"create-like-{layer}.gpkg"
        run([t, "create", mine, "--like", data / src, "--layer", layer])
        case(compare_schemas(f"create-like-{layer}", gdal_schema(mine), gdal_schema(ref)))
        schema_file = work / f"{layer}.schema.json"
        run([t, "export-schema", data / src, "--layer", layer, "-o", schema_file])
        mine = work / f"create-schema-{layer}.gpkg"
        run([t, "create", mine, "--schema", schema_file])
        case(compare_schemas(f"create-schema-{layer}", gdal_schema(mine), gdal_schema(ref)))
    # Explicit fields, against GDAL's API.
    ref = work / "create-fields-ref.gpkg"
    from osgeo import osr

    ds = ogr.GetDriverByName("GPKG").CreateDataSource(str(ref))
    s = osr.SpatialReference()
    s.ImportFromEPSG(32633)
    lyr = ds.CreateLayer("sites", s, ogr.wkbPoint)
    for n, ty, w in [("name", ogr.OFTString, 40), ("count", ogr.OFTInteger, 0), ("total", ogr.OFTInteger64, 0), ("ratio", ogr.OFTReal, 0), ("day", ogr.OFTDate, 0), ("seen", ogr.OFTDateTime, 0), ("raw", ogr.OFTBinary, 0)]:
        fd = ogr.FieldDefn(n, ty)
        fd.SetWidth(w)
        lyr.CreateField(fd)
    ds = None
    mine = work / "create-fields.gpkg"
    run([t, "create", mine, "--output-layer", "sites", "--geometry-type", "POINT", "--crs", "EPSG:32633",
         "--field", "name:STRING(40)", "--field", "count:INTEGER", "--field", "total:INTEGER64", "--field", "ratio:REAL",
         "--field", "day:DATE", "--field", "seen:DATETIME", "--field", "raw:BINARY"])
    case(compare_schemas("create-fields", gdal_schema(mine), gdal_schema(ref)))

    # -- set-field-type ----------------------------------------------------------------------
    matrix = work / "matrix.gpkg"
    make_matrix_source(matrix)
    targets = ["Integer", "Integer64", "Real", "String", "Date", "DateTime", "Boolean", "Int16", "Float32", "JSON", "Binary"]
    total, same, notes, bad = 0, 0, [], []
    for fname in ["s", "r", "i", "i64", "d", "dt", "b"]:
        for target in targets:
            ref = work / f"sft-ref-{fname}-{target}.gpkg"
            mine = work / f"sft-{fname}-{target}.gpkg"
            run(["gdal", "vector", "set-field-type", matrix, ref, "--field-name", fname, "--field-type", target, "--overwrite"])
            run([t, "set-field-type", matrix, mine, "--field-name", fname, "--dst-field-type", target])
            c = compare_dumps(f"{fname}->{target}", dump(mine), dump(ref), ignore_types=KNOWN_TYPES)
            total += 1
            # Subtype-only changes of Integer fields keep GDAL's values unchanged, which terrio's
            # booleans and 16-bit integers cannot hold.
            known_value = fname == "i" and target in ("Boolean", "Int16")
            if c["status"] == IDENTICAL:
                same += 1
            elif c["status"] == TOLERANCE or known_value:
                notes.append(f"{fname}->{target}: {c['detail'][:120]}")
            else:
                bad.append(f"{fname}->{target}: {c['detail'][:200]}")
    status = DIFFERS if bad else (TOLERANCE if notes else IDENTICAL)
    case({"name": "set-field-type-matrix", "status": status, "detail": f"{same}/{total} conversions identical; " + "; ".join(bad + notes), "conversions": total})
    # Real fields of real data.
    for fname, target in [("POP_EST", "Integer"), ("POP_EST", "Integer64"), ("POP_EST", "String"), ("ISO_N3", "Integer"), ("ISO_N3", "Real"),
                          ("LABELRANK", "Int16"), ("GDP_MD_EST", "Float32"), ("scalerank", "String"), ("scalerank", "Boolean")]:
        ref = work / f"sft-countries-ref-{fname}-{target}.gpkg"
        mine = work / f"sft-countries-{fname}-{target}.gpkg"
        if run(["gdal", "vector", "set-field-type", data / "countries.gpkg", ref, "--field-name", fname, "--field-type", target, "--overwrite"], check=False).returncode:
            continue
        run([t, "set-field-type", data / "countries.gpkg", mine, "--field-name", fname, "--dst-field-type", target])
        case(compare_dumps(f"set-field-type-countries-{fname}-{target}", dump(mine), dump(ref), ignore_types=KNOWN_TYPES))
    ref = work / "sft-type-ref.gpkg"
    mine = work / "sft-type.gpkg"
    run(["gdal", "vector", "set-field-type", data / "countries.gpkg", ref, "--src-field-type", "Real", "--field-type", "String", "--overwrite"])
    run([t, "set-field-type", data / "countries.gpkg", mine, "--src-field-type", "Real", "--dst-field-type", "String"])
    case(compare_dumps("set-field-type-countries-real-to-string", dump(mine), dump(ref)))

    # -- make-point --------------------------------------------------------------------------
    csv = data / "hike_gps_trace.csv"
    if csv.exists():
        for extra, label in [([], "xy"), (["--dst-crs", "EPSG:4326"], "xy-crs")]:
            ref = work / f"make-point-ref-{label}.gpkg"
            mine = work / f"make-point-{label}.gpkg"
            # terrio's CSV reader detects column types, as GDAL's AUTODETECT_TYPE=YES does.
            run(["gdal", "vector", "make-point", csv, ref, "--x", "longitude", "--y", "latitude", *extra, "--oo", "AUTODETECT_TYPE=YES", "--overwrite"])
            run([t, "make-point", csv, mine, "--x", "longitude", "--y", "latitude", *extra])
            case(compare_dumps(f"make-point-{label}", dump(mine), dump(ref)))
    # Z from an attribute of a real layer: populated places' coordinates and zoom (terrio's
    # GeoPackage writer writes XY and XYZ geometries only, so M is compared in unit tests).
    places = data / "ne_places" / "ne_10m_populated_places_simple.shp"
    if places.exists():
        ref = work / "make-point-ref-z.gpkg"
        mine = work / "make-point-z.gpkg"
        run(["gdal", "vector", "make-point", places, ref, "--x", "longitude", "--y", "latitude", "--z", "min_zoom", "--dst-crs", "EPSG:4326", "--overwrite"])
        run([t, "make-point", places, mine, "--x", "longitude", "--y", "latitude", "--z", "min_zoom", "--dst-crs", "EPSG:4326"])
        case(compare_dumps("make-point-places-z", dump(mine), dump(ref)))

    # -- concat ------------------------------------------------------------------------------
    multi = data / "multi.gpkg"
    inputs = [data / "countries.gpkg", data / "UScounties.gpkg", multi]
    for label, opts in [
        ("merge-per-layer-name", []),
        ("stack", ["--mode", "stack"]),
        ("stack-template", ["--mode", "stack", "--output-layer", "{DS_INDEX}_{LAYER_INDEX}_{LAYER_NAME}"]),
        ("single", ["--mode", "single"]),
        ("single-intersection", ["--mode", "single", "--field-strategy", "intersection", "--source-layer-field-content", "{DS_BASENAME}/{LAYER_NAME}"]),
        ("single-reprojected", ["--mode", "single", "--dst-crs", "EPSG:3857", "--output-layer", "all"]),
    ]:
        ins = inputs if "reprojected" not in label else [data / "countries.gpkg", data / "UScounties.gpkg"]
        ref = work / f"concat-ref-{label}.gpkg"
        mine = work / f"concat-{label}.gpkg"
        run(["gdal", "vector", "concat", *ins, ref, *opts, "--overwrite"])
        run([t, "concat", *ins, mine, *opts])
        # UScounties (NAD83) is reprojected to the first layer's CRS (WGS 84) in every mode
        # that merges it with countries.
        tol = 1e-6 if "reprojected" in label else 1e-9
        case(compare_dumps(f"concat-{label}", dump(mine), dump(ref), tolerance=tol))
    # A source-layer field in union mode: GDAL (3.12 and 3.13) merges the fields of later
    # layers into the field before the one of their name when a source-layer field is added (an
    # off-by-one in OGRUnionLayer), so the fields are compared with GDAL's output without it, and
    # the source-layer field with GDAL's.
    ref = work / "concat-ref-single-src.gpkg"
    plain = work / "concat-ref-single.gpkg"
    mine = work / "concat-single-src.gpkg"
    run(["gdal", "vector", "concat", *inputs, ref, "--mode", "single", "--source-layer-field-name", "src", "--overwrite"])
    run([t, "concat", *inputs, mine, "--mode", "single", "--source-layer-field-name", "src"])
    m, r, pl = dump(mine), dump(ref), dump(plain)
    src_mine = [v[0] for _, v in m["merged"]["features"]]
    src_ref = [v[0] for _, v in r["merged"]["features"]]
    m["merged"]["fields"] = m["merged"]["fields"][1:]
    m["merged"]["features"] = [(g, v[1:]) for g, v in m["merged"]["features"]]
    c = compare_dumps("concat-single-source-field", m, pl, tolerance=1e-9)
    if src_mine != src_ref and c["status"] != DIFFERS:
        c["status"], c["detail"] = DIFFERS, "source-layer field values differ"
    case(c)
    # ogrmerge.py -single, against concat --mode single (layers with the same geometry column
    # name: ogrmerge's union VRT drops geometries of columns named otherwise).
    ref = work / "ogrmerge-single.gpkg"
    run(["ogrmerge.py", "-single", "-o", ref, data / "countries.gpkg", data / "UScounties.gpkg", "-nln", "merged", "-overwrite_ds"])
    mine = work / "concat-ogrmerge.gpkg"
    run([t, "concat", data / "countries.gpkg", data / "UScounties.gpkg", mine, "--mode", "single"])
    case(compare_dumps("concat-ogrmerge-single", dump(mine), dump(ref), tolerance=1e-9))

    # -- partition ---------------------------------------------------------------------------
    def tree(root):
        out = {}
        for dirpath, _, files in os.walk(root):
            for f in files:
                p = Path(dirpath) / f
                rel = p.relative_to(root).as_posix()
                if p.suffix in (".gpkg", ".fgb", ".json", ".shp", ".parquet"):
                    out[rel] = p
        return out

    for label, src, fmt, opts in [
        ("hive-string", data / "UScounties.gpkg", [], ["--field", "STATE"]),
        ("hive-two-fields-limit", data / "UScounties.gpkg", [], ["--field", "STATE", "--field", "LSAD", "--feature-limit", "20"]),
        ("flat-integer", places, ["-f", "GPKG"], ["--field", "adm0cap", "--scheme", "flat"]),
        ("flat-omit-pattern", places, ["-f", "FlatGeobuf"], ["--field", "featurecla", "--scheme", "flat", "--omit-partitioned-field", "--pattern", "{FIELD_VALUE}_%03d"]),
        ("limit-only", data / "countries.gpkg", ["-f", "GeoJSON"], ["--feature-limit", "40"]),
    ]:
        if not src.exists():
            continue
        ref = work / f"partition-ref-{label}"
        mine = work / f"partition-{label}"
        p = run(["gdal", "vector", "partition", src, ref, *fmt, *opts], check=False)
        if p.returncode:
            # GDAL 3.12 requires --field (3.13 accepts limits alone).
            print(f"   GDAL refused partition-{label}: {p.stderr.strip()[:100]}", file=sys.stderr)
            continue
        run([t, "partition", src, mine, *fmt, *opts])
        a, b = tree(mine), tree(ref)
        problems = []
        if sorted(a) != sorted(b):
            only_a = sorted(set(a) - set(b))[:3]
            only_b = sorted(set(b) - set(a))[:3]
            problems.append(f"{len(a)} files vs {len(b)}; terrio only {only_a}, GDAL only {only_b}")
        worst = []
        for rel in sorted(set(a) & set(b)):
            c = compare_dumps(rel, dump(a[rel]), dump(b[rel]))
            if c["status"] == DIFFERS:
                worst.append(f"{rel}: {c['detail'][:150]}")
        problems += worst[:3]
        case({"name": f"partition-{label}", "status": DIFFERS if problems else IDENTICAL, "detail": "; ".join(problems), "files": len(b)})

    report = {"reference": "GDAL " + gdal.VersionInfo("RELEASE_NAME"), "cases": cases}
    (args.json or work / "report.json").write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
