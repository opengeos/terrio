# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Compare live OGC API Features through GDAL, CLI, Python and optional Node/WASM.

Uses static Natural Earth lakes on pygeoapi and PDOK BAG buildings. Network access
is explicit (--remote); unavailable services are reported as not compared.
"""

import argparse
import json
import math
from pathlib import Path
import subprocess
import tempfile

from osgeo import gdal, ogr, osr


gdal.UseExceptions()
ogr.UseExceptions()
osr.UseExceptions()
LIMIT = 4
XY_TOLERANCE = 1e-8
NUMERIC_REL, NUMERIC_ABS = 1e-12, 1e-10
LAKES = "https://demo.pygeoapi.io/master/collections/lakes"
BUILDINGS = "https://api.pdok.nl/kadaster/bag/ogc/v2/collections/pand"


def open_reference(url, options, bbox=None):
    source = gdal.OpenEx("OAPIF:" + url, gdal.OF_VECTOR,
                         open_options=["PAGE_SIZE=3", "INITIAL_REQUEST_PAGE_SIZE=3", *options])
    layer = source.GetLayer(0)
    if bbox:
        layer.SetSpatialFilterRect(*bbox)
    return source, layer


def profiles():
    cases = [
        ("lakes paged schema", LAKES, [], None),
        ("lakes preferred CRS fallback", LAKES, ["PREFERRED_CRS=EPSG:3857"], None),
        ("BAG projected storage CRS", BUILDINGS, [], None),
        ("BAG geographic authority axes", BUILDINGS, ["CRS=EPSG:4258"], None),
    ]
    source, layer = open_reference(LAKES, [])
    for first in layer:
        geometry = first.GetGeometryRef()
        if geometry is not None and not geometry.IsEmpty():
            break
    else:
        raise RuntimeError("no lake feature has a nonempty geometry")
    while geometry.GetGeometryCount():
        geometry = geometry.GetGeometryRef(0)
    x, y, *_ = geometry.GetPoint(0)
    cases.insert(2, ("lakes bbox", LAKES, [], [x - .0001, y - .0001, x + .0001, y + .0001]))
    return cases


def field_kind(field):
    kind = field.GetType()
    if kind == ogr.OFTInteger:
        return "bool" if field.GetSubType() == ogr.OFSTBoolean else "integer"
    if kind == ogr.OFTInteger64:
        return "integer64"
    return {ogr.OFTReal: "float", ogr.OFTString: "text", ogr.OFTDate: "date", ogr.OFTDateTime: "datetime"}.get(kind, field.GetTypeName())


def value(feature, index, kind):
    if not feature.IsFieldSetAndNotNull(index):
        return None
    if kind in ("date", "datetime"):
        return tuple(feature.GetFieldAsDateTime(index))
    return feature.GetField(index)


def reference_rows(url, options, bbox):
    source, layer = open_reference(url, options, bbox)
    schema = {field.GetName(): field_kind(field) for field in layer.schema}
    rows = []
    for _ in range(LIMIT):
        feature = layer.GetNextFeature()
        if feature is None:
            break
        rows.append(dict(properties={field.GetName(): value(feature, i, field_kind(field))
                                     for i, field in enumerate(layer.schema)},
                         geometry=json.loads(feature.GetGeometryRef().ExportToJson()) if feature.GetGeometryRef() else None))
    return schema, layer.GetSpatialRef().Clone(), rows


def coordinates(geometry):
    if geometry is None:
        return None
    if geometry["type"] == "GeometryCollection":
        return [coordinates(g) for g in geometry["geometries"]]
    return geometry["coordinates"]


def check_coordinates(a, b):
    if a is None or b is None:
        if a != b:
            raise AssertionError("null geometry differs")
        return 0.
    if isinstance(a, (int, float)):
        if not isinstance(b, (int, float)) or not math.isfinite(a) or not math.isfinite(b):
            raise AssertionError("nonfinite or structurally different coordinates")
        error = abs(a - b)
        if error > XY_TOLERANCE:
            raise AssertionError(f"coordinate error {error} exceeds {XY_TOLERANCE}")
        return error
    if not isinstance(b, list) or len(a) != len(b):
        raise AssertionError("geometry/ring/vertex count differs")
    return max((check_coordinates(x, y) for x, y in zip(a, b, strict=True)), default=0.)


def compare(path, schema, crs, expected):
    actual = ogr.Open(str(path))
    layer = actual.GetLayer(0)
    got_schema = {field.GetName(): field_kind(field) for field in layer.schema}
    # The service returns a flattened href array although its JSON schema advertises
    # an absent nested object. Check both the array and the extra all-null schema field.
    normalized_schema = dict(got_schema)
    differences = []
    if got_schema.get("verblijfsobject.href") == "text" and schema.get("verblijfsobject.href") == "StringList":
        normalized_schema["verblijfsobject.href"] = "StringList"
        differences.append("href array preserved as JSON text versus GDAL StringList")
    if "verblijfsobject" in got_schema and "verblijfsobject" not in schema:
        assert got_schema["verblijfsobject"] == "text"
        del normalized_schema["verblijfsobject"]
        differences.append("advertised absent verblijfsobject retained as checked all-null field")
    if got_schema.get("documentdatum") == "text" and schema.get("documentdatum") == "date":
        normalized_schema["documentdatum"] = "date"
        differences.append("documentdatum honors advertised string versus GDAL inferred Date")
    for name, kind in schema.items():
        if kind == "integer" and normalized_schema.get(name) == "integer64":
            normalized_schema[name] = "integer"
            differences.append(f"{name}: Int32 widened losslessly to Int64")
    if normalized_schema != schema:
        raise AssertionError(f"field names/types differ: {got_schema} != {schema}")
    if not crs.IsSame(layer.GetSpatialRef(), ["IGNORE_DATA_AXIS_TO_SRS_AXIS_MAPPING=YES"]):
        raise AssertionError("CRS differs")
    maximum = 0.
    rows = list(layer)
    if len(rows) != len(expected):
        raise AssertionError(f"feature count {len(rows)} != {len(expected)}")
    for got, ref in zip(rows, expected, strict=True):
        for i, field in enumerate(layer.schema):
            name, kind = field.GetName(), field_kind(field)
            a = value(got, i, kind)
            if name == "verblijfsobject" and name not in schema:
                assert a is None, "advertised absent field must remain null"
                continue
            b = ref["properties"][name]
            if name == "verblijfsobject.href" and kind == "text" and schema[name] == "StringList":
                a = json.loads(a) if a is not None else None
            if name == "documentdatum" and kind == "text" and schema[name] == "date" and b is not None:
                assert b[3:] == (0, 0, 0.0, 0), "reference date has unexpected time/zone"
                b = f"{b[0]:04d}-{b[1]:02d}-{b[2]:02d}"
            if kind == "float" and a is not None and b is not None:
                if not math.isclose(a, b, rel_tol=NUMERIC_REL, abs_tol=NUMERIC_ABS):
                    raise AssertionError(f"numeric property differs: {name}")
                if a != b and "numeric attributes within tolerance" not in differences:
                    differences.append("numeric attributes within tolerance")
            elif a != b:
                raise AssertionError(f"attribute/null/id differs: {name}: {a} != {b}")
        geom = json.loads(got.GetGeometryRef().ExportToJson()) if got.GetGeometryRef() else None
        if geom is not None and ref["geometry"] is not None and geom["type"] != ref["geometry"]["type"]:
            raise AssertionError("geometry type differs")
        maximum = max(maximum, check_coordinates(coordinates(geom), coordinates(ref["geometry"])))
    return maximum, differences


def run(args):
    result = subprocess.run(list(map(str, args)), capture_output=True, text=True, timeout=120)
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or result.stdout.strip())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--terrio", default="target/release/terrio")
    parser.add_argument("--node-package", type=Path)
    parser.add_argument("--remote", action="store_true")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    if not args.remote:
        rows = [dict(name="live OAPIF", status="not compared", detail="enable --remote for public OGC API services")]
    else:
        import terrio
        rows = []
        with tempfile.TemporaryDirectory(prefix="terrio_oapif_") as directory:
            work = Path(directory)
            try:
                cases = profiles()
            except Exception as error:
                cases = [
                    ("BAG projected storage CRS", BUILDINGS, [], None),
                    ("BAG geographic authority axes", BUILDINGS, ["CRS=EPSG:4258"], None),
                ]
                rows.append(dict(name="public OAPIF reference discovery", status="not compared", detail=str(error)))
            for index, (name, url, options, bbox) in enumerate(cases):
                try:
                    schema, crs, expected = reference_rows(url, options, bbox)
                except Exception as error:
                    rows.append(dict(name=name + " GDAL reference", status="not compared", detail=str(error)))
                    continue
                for api in ["CLI", "Python"] + (["Node/WASM"] if args.node_package else []):
                    try:
                        output = work / f"{index}-{api.split('/')[0]}.gpkg"
                        if api == "CLI":
                            cmd = [args.terrio, "convert", "OAPIF:" + url, output, "--limit", str(LIMIT), "--oo", "PAGE_SIZE=3"]
                            for option in options:
                                cmd += ["--oo", option]
                            if bbox:
                                cmd += ["--spat", *map(str, bbox)]
                            run(cmd)
                        elif api == "Python":
                            opts = dict(option.split("=", 1) for option in options)
                            opts["PAGE_SIZE"] = "3"
                            opts["OAPIF_MAX_FEATURES"] = str(LIMIT)
                            result = terrio.open_vector("OAPIF:" + url, options=opts, bbox=tuple(bbox) if bbox else None)
                            result.layers[0].write(str(output))
                        else:
                            opts = dict(pageSize=3, maxFeatures=LIMIT)
                            for option in options:
                                key, val = option.split("=", 1)
                                opts[{"CRS": "crs", "PREFERRED_CRS": "preferredCrs"}[key]] = val
                            if bbox:
                                opts["bbox"] = bbox
                            run(["node", "-e", """
const fs = require('node:fs');
const t = require(process.argv[1]);
(async () => {
 const result = await t.openOapif(process.argv[2], JSON.parse(process.argv[4]));
 fs.writeFileSync(process.argv[3], result.toBytes('result.gpkg'));
})().catch(e => { console.error(e.message); process.exitCode=1; });
""", args.node_package.resolve(), url, output, json.dumps(opts)])
                        maximum, differences = compare(output, schema, crs, expected)
                        rows.append(dict(name=f"{name} {api}", status="tolerance" if maximum or differences else "identical",
                                         detail=f"{len(expected)} features; all attributes/nulls/source IDs checked; physical field schema exact except documented normalization (real values rel {NUMERIC_REL}/abs {NUMERIC_ABS}); every coordinate within {XY_TOLERANCE} source units, observed max {maximum:.9g}; CRS equivalent; generated FIDs excluded; " + "; ".join(differences)))
                    except Exception as error:
                        rows.append(dict(name=f"{name} {api}", status="differs", detail=str(error)))
    print(json.dumps(rows, indent=2) if args.json else "\n".join(str(row) for row in rows))
    return int(any(row["status"] == "differs" for row in rows))


if __name__ == "__main__":
    raise SystemExit(main())
