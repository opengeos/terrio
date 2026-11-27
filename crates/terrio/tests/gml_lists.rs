//! GML properties that GDAL types as lists, typed and read as GDAL 3.13.2 types and reads them
//! (`tests/data/gml_lists`, from `scripts/make_gml_lists_fixtures.py`): GML 2, GML 3 and GML 3.2
//! that GDAL writes, and GML with and without a schema written by hand, each read with its `.xsd`
//! and without it. Every property's OGR type, and its value in each feature, read from a file, from
//! memory and as a stream.

use serde_json::Value;
use std::path::Path;
use terrio::ops::vector_schema::{OgrFieldSubType, OgrType};
use terrio::{FieldValue, Layer};

fn data(name: &str) -> String {
    format!("{}/tests/data/gml_lists/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn expected() -> serde_json::Map<String, Value> {
    let text = std::fs::read_to_string(data("expected.json")).unwrap();
    let doc: Value = serde_json::from_str(&text).unwrap();
    doc["files"].as_object().unwrap().clone()
}

/// Writes a minimal feature and application schema beside one another.
fn schema_case(property: &str, declaration: &str, global: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let schema = std::fs::read_to_string(data("declared.xsd")).unwrap();
    let start = schema.find("        <xs:element name=\"geometryProperty\"").unwrap();
    let end = start + schema[start..].find("      </xs:sequence>").unwrap();
    let schema = format!("{}{}{}", &schema[..start], declaration, &schema[end..]);
    std::fs::write(dir.path().join("declared.xsd"), schema.replace("</xs:schema>", &format!("{global}</xs:schema>"))).unwrap();
    let xml = format!(
        r#"<ogr:FeatureCollection xmlns:ogr="http://ogr.maptools.org/" xmlns:gml="http://www.opengis.net/gml/3.2"><ogr:featureMember><ogr:declared gml:id="declared.1">{property}</ogr:declared></ogr:featureMember></ogr:FeatureCollection>"#
    );
    std::fs::write(dir.path().join("declared.gml"), xml).unwrap();
    dir
}

#[test]
fn declared_scalar_strings_keep_their_whitespace() {
    for text in ["  Main  ", "  "] {
        let dir = schema_case(&format!("<ogr:p>{text}</ogr:p>"), r#"<xs:element name="p" type="xs:string"/>"#, "");
        let ds = terrio::open_vector(dir.path().join("declared.gml").to_str().unwrap(), &Default::default()).unwrap();
        assert_eq!(ds.layers[0].features[0].attributes[1], FieldValue::Text(text.into()));
    }
}

/// GDAL 3.13.3 infers lists when an xs:token schema type is not mapped by its GML reader.
#[test]
fn unmapped_repeating_schema_types_keep_their_values() {
    for (a, b, expected, kind) in [("A", "B", r#"["A","B"]"#, "StringList"), ("1", "2", "[1,2]", "IntegerList")] {
        let dir = schema_case(
            &format!("<ogr:p>{a}</ogr:p><ogr:undeclared>ignored</ogr:undeclared><ogr:p>{b}</ogr:p>"),
            r#"<xs:element name="p" type="xs:token" maxOccurs="unbounded"/>"#,
            "",
        );
        let ds = terrio::open_vector(dir.path().join("declared.gml").to_str().unwrap(), &Default::default()).unwrap();
        assert_eq!(ds.ogr_type(&ds.layers[0], 1).field_type.name(), kind);
        assert_eq!(ds.layers[0].features[0].attributes[1], FieldValue::Text(expected.into()));
    }
}

#[test]
fn referenced_properties_use_the_occurrence_limit_at_the_reference() {
    for (occurs, expected, list) in [(" maxOccurs=\"unbounded\"", r#"["A","B"]"#, true), ("", "A", false)] {
        let dir = schema_case(
            "<ogr:p>A</ogr:p><ogr:p>B</ogr:p>",
            &format!(r#"<xs:element ref="ogr:p"{occurs}/>"#),
            r#"<xs:element name="p" type="xs:string"/>"#,
        );
        let ds = terrio::open_vector(dir.path().join("declared.gml").to_str().unwrap(), &Default::default()).unwrap();
        assert_eq!(ds.ogr_type(&ds.layers[0], 1).field_type.is_list(), list);
        assert_eq!(ds.layers[0].features[0].attributes[1], FieldValue::Text(expected.into()));
    }
}

/// Terrio resolves custom element references that GDAL 3.13.3 instead infers without a schema.
#[test]
fn referenced_properties_retain_the_declared_element_type() {
    let dir = schema_case(
        "<ogr:p>1</ogr:p><ogr:p>2</ogr:p>",
        r#"<xs:element ref="ogr:p" maxOccurs="unbounded"/>"#,
        r#"<xs:element name="p" type="xs:string"/>"#,
    );
    let ds = terrio::open_vector(dir.path().join("declared.gml").to_str().unwrap(), &Default::default()).unwrap();
    assert_eq!(ds.ogr_type(&ds.layers[0], 1).field_type.name(), "StringList");
    assert_eq!(ds.layers[0].features[0].attributes[1], FieldValue::Text(r#"["1","2"]"#.into()));
}

/// The fields of the scalars where terrio differs from GDAL, which are not lists: their types (as
/// terrio infers or maps them) and their values, read as text.
///
/// - `lowerCorner` and `upperCorner`: GDAL makes fields of the corners of a GML 3 feature's own
///   `gml:boundedBy`, which terrio leaves out.
/// - `times`: terrio has no time type; it reads times as text.
/// - `ints` of `declared.gml` read with its schema, a restriction of `xs:int`: GDAL does not know
///   the base and reads the property as a string; terrio reads it as an integer.
///
/// Each names the reads it applies to by the start of their case name (file, then read).
const SCALAR_DIFFERENCES: &[(&str, &str, Option<&str>)] = &[
    ("*", "lowerCorner", None),
    ("*", "upperCorner", None),
    ("inferred.gml", "times", Some("String")),
    ("declared.gml with_xsd", "ints", Some("Integer")),
];

/// A value as GDAL's Python bindings give it: list elements of boolean lists as 0 or 1, integers
/// and reals as numbers, other values as text (dates and date-times as GDAL writes them as
/// strings), null for null.
fn as_gdal(v: &FieldValue, t: OgrType) -> Value {
    match v {
        FieldValue::Null => Value::Null,
        FieldValue::Integer(i) => Value::from(*i),
        FieldValue::Float(f) => Value::from(*f),
        FieldValue::Text(s) if t.field_type.is_list() => {
            let list: Value = serde_json::from_str(s).unwrap_or_else(|e| panic!("{s} is no JSON list: {e}"));
            let boolean = t.subtype == OgrFieldSubType::Boolean;
            Value::Array(
                list.as_array()
                    .unwrap()
                    .iter()
                    .map(|e| if boolean { Value::from(u8::from(e.as_bool().unwrap())) } else { e.clone() })
                    .collect(),
            )
        }
        FieldValue::Text(s) => Value::from(s.as_str()),
        // GDAL's text of a date (`2020/01/01`) and a UTC date-time (`2020/01/01 10:00:00+00`).
        FieldValue::Date(s) => Value::from(s.replace('-', "/")),
        FieldValue::DateTime(s) => Value::from(format!("{} {}", s[..10].replace('-', "/"), s[11..].replace('Z', "+00"))),
        other => panic!("unexpected value {other:?}"),
    }
}

/// A value as text, for the fields whose scalars terrio reads differently.
fn as_text(v: &Value) -> Value {
    match v {
        Value::Null | Value::String(_) => v.clone(),
        other => Value::from(other.to_string()),
    }
}

/// Checks the type of each property GDAL reads and its value in each feature.
fn check(case: &str, want: &serde_json::Map<String, Value>, layer: &Layer, type_of: impl Fn(usize) -> OgrType) {
    for (name, want) in want {
        let difference = SCALAR_DIFFERENCES.iter().find(|(reads, field, _)| (*reads == "*" || case.starts_with(reads)) && field == name);
        let Some(i) = layer.schema.field_index(name) else {
            assert!(difference.is_some_and(|d| d.2.is_none()), "{case}: no field {name}");
            continue;
        };
        let t = type_of(i);
        let values: Vec<Value> = layer.features.iter().map(|f| as_gdal(&f.attributes[i], t)).collect();
        let want_values = want["values"].as_array().unwrap();
        if let Some((_, _, terrio_type)) = difference {
            assert!(!t.field_type.is_list(), "{case}: {name} is no list");
            assert_eq!(Some(t.field_type.name()), *terrio_type, "{case}: type of {name}");
            assert_eq!(
                values.iter().map(as_text).collect::<Vec<_>>(),
                want_values.iter().map(as_text).collect::<Vec<_>>(),
                "{case}: {name}"
            );
            continue;
        }
        assert_eq!(
            (t.field_type.name(), t.subtype.name()),
            (want["type"].as_str().unwrap(), want["subtype"].as_str().unwrap()),
            "{case}: type of {name}"
        );
        assert_eq!(&values, want_values, "{case}: values of {name}");
    }
    // terrio reads no list GDAL does not.
    for (i, field) in layer.schema.fields().iter().enumerate() {
        assert!(!type_of(i).field_type.is_list() || want.contains_key(&field.name), "{case}: {} is a list", field.name);
    }
}

#[test]
fn gml_lists_are_gdal_lists() {
    let dir = tempfile::tempdir().unwrap();
    for (file, reads) in expected() {
        for (read, want) in reads.as_object().unwrap() {
            let want = want.as_object().unwrap();
            let case = format!("{file} {read}");
            let path = if read == "with_xsd" {
                data(&file)
            } else {
                // The file alone, without its schema.
                let alone = dir.path().join(&file);
                std::fs::copy(data(&file), &alone).unwrap();
                alone.to_str().unwrap().to_string()
            };
            assert_eq!(read == "with_xsd", Path::new(&path).with_extension("xsd").exists(), "{case}: schema");
            let ds = terrio::open_vector(&path, &Default::default()).unwrap();
            let layer = &ds.layers[0];
            check(&case, want, layer, |i| ds.ogr_type(layer, i));

            let mut readers = terrio::stream::open_layers(&path, &Default::default(), 2).unwrap();
            let reader = readers.remove(0);
            let types: Vec<OgrType> = (0..reader.layer().schema.len()).map(|i| reader.ogr_type(i)).collect();
            check(&format!("{case} streamed"), want, &reader.read_all().unwrap(), |i| types[i]);

            if read == "without_xsd" {
                // From memory, a file is read without a schema.
                let ds = terrio::memory::open_vector_bytes(std::fs::read(&path).unwrap(), &file, &Default::default()).unwrap();
                check(&format!("{case} in memory"), want, &ds.layers[0], |i| ds.ogr_type(&ds.layers[0], i));
            }
        }
    }
}

#[test]
fn gml_list_types_are_exported_in_the_schema() {
    let schema = terrio::ops::vector_schema::export_schema_path(&data("written_gml32.gml"), &[]).unwrap();
    let fields = &schema.layers[0].fields;
    let of = |name: &str| {
        let f = fields.iter().find(|f| f.name.as_deref() == Some(name)).unwrap();
        (f.field_type.as_deref(), f.sub_type.as_deref())
    };
    assert_eq!(of("ints").0, Some("IntegerList"));
    assert_eq!(of("int64s").0, Some("Integer64List"));
    assert_eq!(of("reals").0, Some("RealList"));
    assert_eq!(of("texts").0, Some("StringList"));
    assert_eq!(of("booleans"), (Some("IntegerList"), Some("Boolean")));
    // The bindings' export of a file held in memory: read without its schema, where a property of
    // one value in every feature is a scalar.
    let bytes = std::fs::read(data("written_gml32.gml")).unwrap();
    let schema = terrio::ops::vector_schema::export_schema_bytes(bytes, "written_gml32.gml", None).unwrap();
    let fields = &schema.layers[0].fields;
    let of = |name: &str| fields.iter().find(|f| f.name.as_deref() == Some(name)).unwrap().field_type.as_deref();
    assert_eq!((of("ints"), of("texts"), of("single")), (Some("IntegerList"), Some("StringList"), Some("Integer")));
}
