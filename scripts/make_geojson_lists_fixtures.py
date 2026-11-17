"""Writes `crates/terrio/tests/data/geojson_lists`: GeoJSON properties that GDAL's GeoJSON reader
types as lists, and the types and values GDAL reads of them.

Usage:
    python scripts/make_geojson_lists_fixtures.py

`cases.geojson` (and the same features as a GeoJSON text sequence, `cases.geojsonl`) has a
property per case, each a run of values over the features (absent from the features after the
run): arrays of integers, 64-bit integers, reals, strings and booleans; empty arrays and nulls
before and after a list; scalars before and after lists they widen with; mixtures that make JSON;
null elements; and strings set into a string list (`(n:a,b)` lists, JSON arrays, empty
strings). The reals in the string lists are not used: GDAL keeps their text as written, which
terrio does not.

`expected.json` holds, for each property, the OGR type and subtype GDAL gives it, and its value in
each feature as GDAL reads it (`GetField`): a list, a scalar, the JSON text of a JSON field, or
null for a null or unset field. Needs GDAL's Python bindings (3.13).
"""

from __future__ import annotations

import json
from pathlib import Path

from osgeo import gdal, ogr

gdal.UseExceptions()
ogr.UseExceptions()

OUT = Path(__file__).resolve().parent.parent / "crates" / "terrio" / "tests" / "data" / "geojson_lists"

# Each case: its values in the first features, in order (`None` is a JSON null).
CASES = {
    "ints": [[1, 2], [3], None],
    "int64s": [[9007199254740993, 1], [], [-5]],
    "reals": [[1.25, 2], [], 7],
    "strings": [["a", "b"], [], "c"],
    "booleans": [[True, False], [True], None],
    "booleans_then_ints": [[True], [2]],
    "empty_only": [[], []],
    "empty_first": [[], [1]],
    "list_then_empty": [[1], []],
    "scalar_then_list": [3, [1, 2]],
    "int64_scalar_then_list": [5000000000, [1]],
    "list_then_real": [[1], 2.5],
    "string_then_strings": ["a", ["b", "c"]],
    "strings_then_int": [["a"], 1],
    "strings_then_ints": [["a"], [1, 2]],
    "strings_then_booleans": [["a"], [True]],
    "list_then_string": [[1], "a"],
    "mixed": [[1, "a"]],
    "nested": [[[1], [2]]],
    "object_then_list": [{"a": 1}, [1]],
    "null_elements": [[None, 1], [1, None]],
    "strings_with_null": [["a", None, "b"]],
    "strings_set": [["x"], "(2:p,q)", '["r", null]', ""],
    "null_first": [None, [4]],
    "boolean_after_null": [[None, True]],
    "date_then_list": ["2020-01-01", [1]],
    "real_then_ints": [1.5, [2, 3]],
    "int64s_then_real": [[5000000000], 1.5],
    "boolean_then_list": [True, [1]],
    "boolean_then_booleans": [True, [False]],
    "reals_then_booleans": [[1.5], [True]],
}


def features():
    n = max(len(v) for v in CASES.values())
    out = []
    for i in range(n):
        props = {k: v[i] for k, v in CASES.items() if i < len(v)}
        out.append({"type": "Feature", "properties": props, "geometry": {"type": "Point", "coordinates": [i, i]}})
    return out


def field_value(f: ogr.Feature, i: int, defn: ogr.FieldDefn):
    if not f.IsFieldSetAndNotNull(i):
        return None
    t = defn.GetType()
    if t == ogr.OFTIntegerList:
        return f.GetFieldAsIntegerList(i)
    if t == ogr.OFTInteger64List:
        return f.GetFieldAsInteger64List(i)
    if t == ogr.OFTRealList:
        return f.GetFieldAsDoubleList(i)
    if t == ogr.OFTStringList:
        # The bindings return None for an empty list, which GDAL holds as set and not null.
        return f.GetFieldAsStringList(i) or []
    if t == ogr.OFTInteger64:
        return f.GetFieldAsInteger64(i)
    if t == ogr.OFTInteger:
        return f.GetFieldAsInteger(i)
    if t == ogr.OFTReal:
        return f.GetFieldAsDouble(i)
    return f.GetFieldAsString(i)


def read(path: Path):
    ds = ogr.Open(str(path))
    layer = ds.GetLayer(0)
    defn = layer.GetLayerDefn()
    fields = {}
    for i in range(defn.GetFieldCount()):
        fd = defn.GetFieldDefn(i)
        fields[fd.GetName()] = {
            "type": ogr.GetFieldTypeName(fd.GetType()),
            "subtype": ogr.GetFieldSubTypeName(fd.GetSubType()),
            "values": [],
        }
    for f in layer:
        for i in range(defn.GetFieldCount()):
            fd = defn.GetFieldDefn(i)
            fields[fd.GetName()]["values"].append(field_value(f, i, fd))
    return fields


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    feats = features()
    doc = {"type": "FeatureCollection", "name": "cases", "features": feats}
    (OUT / "cases.geojson").write_text(json.dumps(doc, indent=None, separators=(",", ":")) + "\n")
    (OUT / "cases.geojsonl").write_text("".join(json.dumps(f, separators=(",", ":")) + "\n" for f in feats))
    collection = read(OUT / "cases.geojson")
    sequence = read(OUT / "cases.geojsonl")
    assert {k: v["type"] for k, v in collection.items()} == {k: v["type"] for k, v in sequence.items()}
    expected = {"gdal_version": gdal.__version__, "fields": collection}
    (OUT / "expected.json").write_text(json.dumps(expected, indent=1) + "\n")


if __name__ == "__main__":
    main()
