"""Writes `crates/terrio/tests/data/gml_lists`: GML properties that GDAL's GML reader types as
lists, and the types and values GDAL reads of them, with and without the `.xsd` schema.

Usage:
    python scripts/make_gml_lists_fixtures.py

`written_gml2.gml`, `written_gml3.gml` and `written_gml32.gml` (each with its `.xsd`) are what
`gdal.VectorTranslate` writes (`FORMAT=GML2`, `GML3` and `GML3.2`) of GeoJSON features whose
properties are integer, 64-bit integer, real, string and boolean lists: lists of two values and of
one, empty lists (which GDAL leaves out), null lists, integers and reals mixed, and a property that
holds a list of one in every feature. GDAL writes a list as the property repeated, and declares it
with `maxOccurs="unbounded"` in the `.xsd`. Trailing spaces are removed from their lines.

`inferred.gml` is GML 3.2 without a schema, written by hand: properties whose types GDAL infers
from their values, in feature order, as `GMLPropertyDefn::AnalysePropertyValue` does: a scalar
before or after a list it widens with, integers widening to 64-bit integers and reals, numbers and
text, booleans (`true`, `false`, and `1` after them), dates and times repeated (which GDAL keeps
as one date or time, the first), empty elements (left out) and elements of spaces (empty values),
leading spaces, `xsi:nil` first in a feature, after a value and between values, and numbers in C syntax that GDAL
reads as text (`007`) or reals (`1e3`, `.5`).

`declared.gml` and `declared.xsd` are GML 3.2 with a schema written by hand: properties declared
with `maxOccurs` other than `1` (`unbounded`, `2`) of each simple type GDAL lists (`integer`,
`int`, `short`, `long`, `decimal`, `double`, `float`, `string`, `boolean`), a date (which GDAL
does not list), and `maxOccurs="1"` properties repeated (which GDAL reads as their first value);
with values that GDAL converts as C does (`atoi`, `CPLAtof`): leading spaces, empty and
whitespace-only elements, `xsi:nil` after a value, text in numbers, and integers beyond 32 bits.

`expected.json` holds, for each file and each read (`with_xsd`, the file as it is, and
`without_xsd`, the file alone in a directory), the OGR type and subtype GDAL gives each property
(not `gml_id` or `fid`) and its value in each feature as GDAL reads it (`GetField`): a list, a
scalar, or null for a null or unset field. GDAL reads them with `WRITE_GFS=NO`, so that no `.gfs`
is written or read. Needs GDAL's Python bindings (3.13).
"""

from __future__ import annotations

import json
import shutil
import tempfile
from pathlib import Path

from osgeo import gdal, ogr

gdal.UseExceptions()
ogr.UseExceptions()

OUT = Path(__file__).resolve().parent.parent / "crates" / "terrio" / "tests" / "data" / "gml_lists"

# Each property: its values in the first features, in order (`None` is a JSON null).
WRITTEN = {
    "ints": [[1, 2], [3], [], None],
    "int64s": [[5000000000, 1], [2], [-7000000000]],
    "reals": [[1.25, 2.5], [], [7.5]],
    "texts": [["a", "b"], ["c"], [], ["d e", " f"]],
    "mixed": [[1, 2.5], [3], [4.25, 5]],
    "booleans": [[True, False], [True]],
    "single": [[5], [6], [7], [8]],
}

INFERRED = [
    # Each feature: its properties in document order, as (name, text), (name, None) for an
    # element with xsi:nil="true", or (name, "") for an empty element.
    [
        ("scalar_then_list", "3"),
        ("list_then_real", "1"),
        ("list_then_real", "2"),
        ("ints_then_int64", "1"),
        ("ints_then_int64", "2"),
        ("int64_then_ints", "5000000000"),
        ("ints_then_text", "1"),
        ("ints_then_text", "2"),
        ("text_then_ints", "a"),
        ("booleans", "true"),
        ("booleans", "false"),
        ("boolean_then_booleans", "true"),
        ("dates", "2020-01-01"),
        ("dates", "2020-01-02"),
        ("datetimes", "2020-01-01T10:00:00Z"),
        ("datetimes", "2020-01-02"),
        ("times", "10:00:00"),
        ("times", "11:30:00"),
        ("date_then_text", "2020-01-01"),
        ("date_then_text", "abc"),
        ("empty_elements", ""),
        ("empty_elements", "4"),
        ("blank_first", "  "),
        ("blank_first", "5"),
        ("leading_spaces", "  6"),
        ("leading_spaces", " 7.5"),
        ("nil_first", None),
        ("nil_first", "1"),
        ("nil_after", "1"),
        ("nil_after", None),
        ("c_numbers", "007"),
        ("c_numbers", "1e3"),
        ("reals_c", ".5"),
        ("reals_c", "1e3"),
        ("nil_feature", "1"),
        ("nil_feature", "2"),
        ("nil_in_list", "1"),
        ("nil_in_list", None),
        ("nil_in_list", "2"),
    ],
    [
        ("scalar_then_list", "1"),
        ("scalar_then_list", "2"),
        ("list_then_real", "2.5"),
        ("ints_then_int64", "5000000000"),
        ("int64_then_ints", "1"),
        ("int64_then_ints", "2"),
        ("ints_then_text", "x"),
        ("text_then_ints", "1"),
        ("text_then_ints", "2"),
        ("booleans", "1"),
        ("booleans", "true"),
        ("boolean_then_booleans", "false"),
        ("boolean_then_booleans", "true"),
        ("dates", "2020-02-01"),
        ("times", "12:00:00"),
        ("empty_elements", "5"),
        ("empty_elements", "6"),
        ("leading_spaces", "8"),
        ("nil_feature", None),
        ("c_numbers", "2"),
    ],
    [
        ("scalar_then_list", "4"),
        ("ints_then_text", "3"),
        ("ints_then_text", "4"),
        ("empty_elements", ""),
        ("nil_feature", "3"),
    ],
]

# Each declared property: its XML Schema type and maxOccurs.
DECLARED_TYPES = {
    "integers": ("integer", "unbounded"),
    "ints": ("int", "unbounded"),
    "shorts": ("short", "unbounded"),
    "longs": ("long", "unbounded"),
    "decimals": ("decimal", "unbounded"),
    "doubles": ("double", "2"),
    "floats": ("float", "unbounded"),
    "strings": ("string", "unbounded"),
    "booleans": ("boolean", "unbounded"),
    "dates": ("date", "unbounded"),
    "one_integer": ("integer", "1"),
    "one_string": ("string", "1"),
}

DECLARED = [
    [
        ("integers", "1"),
        ("integers", "2"),
        ("ints", "3"),
        ("shorts", "4"),
        ("shorts", "5"),
        ("longs", "5000000000"),
        ("longs", "1"),
        ("decimals", "1.5"),
        ("decimals", "2"),
        ("doubles", "3.25"),
        ("doubles", "4"),
        ("floats", "0.5"),
        ("strings", "a"),
        ("strings", "b"),
        ("booleans", "true"),
        ("booleans", "0"),
        ("booleans", "1"),
        ("dates", "2020-01-01"),
        ("dates", "2020-01-02"),
        ("one_integer", "10"),
        ("one_integer", "11"),
        ("one_string", "first"),
        ("one_string", "second"),
    ],
    [
        ("integers", " 7"),
        ("integers", "8x"),
        ("integers", "  "),
        ("integers", "5000000000"),
        ("ints", "abc"),
        ("longs", "-3"),
        ("decimals", " 2.5e1"),
        ("decimals", "x"),
        ("strings", "  c"),
        ("strings", "  "),
        ("strings", "d "),
        ("booleans", "false"),
        ("booleans", "yes"),
        ("dates", "2020-03-01"),
    ],
    [
        ("integers", ""),
        ("integers", "9"),
        ("strings", ""),
        ("strings", "e"),
        ("strings", None),
        ("doubles", "1"),
        ("doubles", None),
        ("booleans", None),
        ("longs", None),
    ],
]

GML32 = "http://www.opengis.net/gml/3.2"
HEADER = (
    '<?xml version="1.0" encoding="utf-8" ?>\n'
    '<ogr:FeatureCollection gml:id="aFeatureCollection"{schema}\n'
    '     xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"\n'
    '     xmlns:ogr="http://ogr.maptools.org/"\n'
    f'     xmlns:gml="{GML32}">\n'
)


def hand_written(name: str, features: list, schema: str | None) -> str:
    """A GML 3.2 collection of `name` features with points and these properties."""
    location = f'\n     xsi:schemaLocation="http://ogr.maptools.org/ {schema}"' if schema else ""
    out = [HEADER.format(schema=location)]
    for i, props in enumerate(features):
        out.append("  <ogr:featureMember>\n")
        out.append(f'    <ogr:{name} gml:id="{name}.{i}">\n')
        out.append(
            f'      <ogr:geometryProperty><gml:Point srsName="urn:ogc:def:crs:EPSG::4326" gml:id="{name}.geom.{i}">'
            f"<gml:pos>{-33 - i} {151 + i}</gml:pos></gml:Point></ogr:geometryProperty>\n"
        )
        for key, text in props:
            if text is None:
                out.append(f'      <ogr:{key} xsi:nil="true"/>\n')
            else:
                out.append(f"      <ogr:{key}>{text}</ogr:{key}>\n")
        out.append(f"    </ogr:{name}>\n")
        out.append("  </ogr:featureMember>\n")
    out.append("</ogr:FeatureCollection>\n")
    return "".join(out)


def declared_xsd(name: str) -> str:
    """The application schema of `declared.gml`, as GDAL writes its schemas."""
    elements = "".join(
        f'        <xs:element name="{key}" nillable="true" minOccurs="0" maxOccurs="{occurs}">\n'
        f'          <xs:simpleType><xs:restriction base="xs:{base}"></xs:restriction></xs:simpleType>\n'
        "        </xs:element>\n"
        for key, (base, occurs) in DECLARED_TYPES.items()
    )
    return f"""<?xml version="1.0" encoding="UTF-8"?>
<xs:schema targetNamespace="http://ogr.maptools.org/" xmlns:ogr="http://ogr.maptools.org/" xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:gml="{GML32}" elementFormDefault="qualified" version="1.0">
<xs:import namespace="{GML32}" schemaLocation="http://schemas.opengis.net/gml/3.2.1/gml.xsd"/>
<xs:element name="FeatureCollection" type="ogr:FeatureCollectionType" substitutionGroup="gml:AbstractFeature"/>
<xs:complexType name="FeatureCollectionType">
  <xs:complexContent>
    <xs:extension base="gml:AbstractFeatureType">
      <xs:sequence minOccurs="0" maxOccurs="unbounded">
        <xs:element name="featureMember">
          <xs:complexType>
            <xs:complexContent>
              <xs:extension base="gml:AbstractFeatureMemberType">
                <xs:sequence>
                  <xs:element ref="gml:AbstractFeature"/>
                </xs:sequence>
              </xs:extension>
            </xs:complexContent>
          </xs:complexType>
        </xs:element>
      </xs:sequence>
    </xs:extension>
  </xs:complexContent>
</xs:complexType>
<xs:element name="{name}" type="ogr:{name}_Type" substitutionGroup="gml:AbstractFeature"/>
<xs:complexType name="{name}_Type">
  <xs:complexContent>
    <xs:extension base="gml:AbstractFeatureType">
      <xs:sequence>
        <xs:element name="geometryProperty" type="gml:PointPropertyType" nillable="true" minOccurs="0" maxOccurs="1"/>
{elements}      </xs:sequence>
    </xs:extension>
  </xs:complexContent>
</xs:complexType>
</xs:schema>
"""


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
    """The types and values GDAL reads of each property of the file's one layer."""
    ds = gdal.OpenEx(str(path), gdal.OF_VECTOR, open_options=["WRITE_GFS=NO"])
    assert not path.with_suffix(".gfs").exists()
    layer = ds.GetLayer(0)
    defn = layer.GetLayerDefn()
    fields = {}
    names = [defn.GetFieldDefn(i).GetName() for i in range(defn.GetFieldCount())]
    for i, name in enumerate(names):
        fd = defn.GetFieldDefn(i)
        if name in ("gml_id", "fid"):
            continue
        fields[name] = {
            "type": ogr.GetFieldTypeName(fd.GetType()),
            "subtype": ogr.GetFieldSubTypeName(fd.GetSubType()),
            "values": [],
        }
    for f in layer:
        for i, name in enumerate(names):
            if name in fields:
                fields[name]["values"].append(field_value(f, i, defn.GetFieldDefn(i)))
    return fields


def reads(path: Path):
    """What GDAL reads of a GML file with its `.xsd` and alone."""
    out = {"with_xsd": read(path)} if path.with_suffix(".xsd").exists() else {}
    with tempfile.TemporaryDirectory() as tmp:
        alone = Path(tmp) / path.name
        shutil.copy(path, alone)
        out["without_xsd"] = read(alone)
    return out


def written(format_name: str, stem: str):
    """`stem.gml` and `stem.xsd` as GDAL writes the GeoJSON features in this GML format."""
    n = max(len(v) for v in WRITTEN.values())
    feats = []
    for i in range(n):
        props = {k: v[i] for k, v in WRITTEN.items() if i < len(v)}
        feats.append({"type": "Feature", "properties": props, "geometry": {"type": "Point", "coordinates": [151 + i, -33 - i]}})
    doc = {"type": "FeatureCollection", "name": "lists", "features": feats}
    with tempfile.TemporaryDirectory() as tmp:
        src = Path(tmp) / "lists.geojson"
        src.write_text(json.dumps(doc))
        gdal.VectorTranslate(str(OUT / f"{stem}.gml"), str(src), format="GML", datasetCreationOptions=[f"FORMAT={format_name}"])
    # GDAL names the schema after the file in the GML. Its lines lose their trailing spaces (GDAL
    # pads a line it may rewrite), outside any value, as the repository's checks require.
    for path in (OUT / f"{stem}.gml", OUT / f"{stem}.xsd"):
        path.write_text("".join(line.rstrip() + "\n" for line in path.read_text().splitlines()))


def main() -> None:
    if OUT.exists():
        shutil.rmtree(OUT)
    OUT.mkdir(parents=True)
    files = {}
    for format_name, stem in [("GML2", "written_gml2"), ("GML3", "written_gml3"), ("GML3.2", "written_gml32")]:
        written(format_name, stem)
        files[f"{stem}.gml"] = None
    (OUT / "inferred.gml").write_text(hand_written("inferred", INFERRED, None))
    (OUT / "declared.gml").write_text(hand_written("declared", DECLARED, "declared.xsd"))
    (OUT / "declared.xsd").write_text(declared_xsd("declared"))
    files["inferred.gml"] = None
    files["declared.gml"] = None
    expected = {"gdal_version": gdal.__version__, "files": {name: reads(OUT / name) for name in files}}
    (OUT / "expected.json").write_text(json.dumps(expected, indent=1) + "\n")


if __name__ == "__main__":
    main()
