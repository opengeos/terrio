//! GeoJSON properties that GDAL types as lists, typed and read as GDAL 3.13.2 types and reads them
//! (`tests/data/geojson_lists`, from `scripts/make_geojson_lists_fixtures.py`): every property's OGR
//! type (lists, and the JSON and scalar types around them), and every value of each feature, read
//! whole, from memory and as a stream, from a FeatureCollection and from a text sequence.

use serde_json::Value;
use terrio::ops::vector_schema::{OgrFieldSubType, OgrType};
use terrio::{FieldValue, Layer};

fn data(name: &str) -> String {
    format!("{}/tests/data/geojson_lists/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn expected() -> serde_json::Map<String, Value> {
    let text = std::fs::read_to_string(data("expected.json")).unwrap();
    let doc: Value = serde_json::from_str(&text).unwrap();
    doc["fields"].as_object().unwrap().clone()
}

/// A value as GDAL's Python bindings give it: lists of boolean lists as 0 or 1, the JSON of JSON
/// fields parsed (other text as a string), integers and reals as numbers, null for null.
fn as_gdal(v: &FieldValue, t: OgrType) -> Value {
    match v {
        FieldValue::Null => Value::Null,
        FieldValue::Integer(i) => Value::from(*i),
        FieldValue::Float(f) => Value::from(*f),
        FieldValue::Text(s) if t.field_type.is_list() => {
            let list: Value = serde_json::from_str(s).unwrap_or_else(|e| panic!("{s} is no JSON list: {e}"));
            // Only the elements of a boolean list are 0 or 1; other lists keep their booleans.
            let boolean = t.subtype == OgrFieldSubType::Boolean;
            let items = list.as_array().unwrap().iter().map(|e| match e.as_bool() {
                Some(b) if boolean => Value::from(u8::from(b)),
                _ => e.clone(),
            });
            Value::Array(items.collect())
        }
        FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => {
            serde_json::from_str(s).unwrap_or_else(|_| Value::from(s.as_str()))
        }
        other => panic!("unexpected value {other:?}"),
    }
}

/// GDAL's value of a JSON field parsed (other text as a string), its other values as they are.
fn gdal_value(v: &Value) -> Value {
    match v {
        Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| v.clone()),
        v => v.clone(),
    }
}

/// Checks the type of each property and its value in each feature.
fn check(case: &str, layer: &Layer, type_of: impl Fn(usize) -> OgrType) {
    let expected = expected();
    let fields = layer.schema.fields();
    assert_eq!(fields.len(), expected.len(), "{case}: fields");
    for (name, want) in &expected {
        let i = layer.schema.field_index(name).unwrap_or_else(|| panic!("{case}: no field {name}"));
        let t = type_of(i);
        assert_eq!(
            (t.field_type.name(), t.subtype.name()),
            (want["type"].as_str().unwrap(), want["subtype"].as_str().unwrap()),
            "{case}: type of {name}"
        );
        let values: Vec<Value> = layer.features.iter().map(|f| as_gdal(&f.attributes[i], t)).collect();
        let want: Vec<Value> = want["values"].as_array().unwrap().iter().map(gdal_value).collect();
        assert_eq!(values, want, "{case}: values of {name}");
    }
}

#[test]
fn geojson_lists_are_gdal_lists() {
    for file in ["cases.geojson", "cases.geojsonl"] {
        let ds = terrio::open_vector(&data(file), &Default::default()).unwrap();
        let layer = &ds.layers[0];
        check(file, layer, |i| ds.ogr_type(layer, i));
        // The types follow a layer made from it.
        let first = Layer { features: layer.features[..1].to_vec(), ..layer.clone() };
        for i in 0..first.schema.len() {
            assert_eq!(ds.ogr_type(&first, i), ds.ogr_type(layer, i), "{file}: field {i} of a filtered layer");
        }

        let bytes = std::fs::read(data(file)).unwrap();
        let ds = terrio::memory::open_vector_bytes(bytes, file, &Default::default()).unwrap();
        check(&format!("{file} in memory"), &ds.layers[0], |i| ds.ogr_type(&ds.layers[0], i));

        let mut readers = terrio::stream::open_layers(&data(file), &Default::default(), 2).unwrap();
        let reader = readers.remove(0);
        let types: Vec<OgrType> = (0..reader.layer().schema.len()).map(|i| reader.ogr_type(i)).collect();
        let streamed = reader.read_all().unwrap();
        check(&format!("{file} streamed"), &streamed, |i| types[i]);
    }
}

#[test]
fn list_types_are_exported_in_the_schema() {
    let schema = terrio::ops::vector_schema::export_schema_path(&data("cases.geojson"), &[]).unwrap();
    let fields = &schema.layers[0].fields;
    let of = |name: &str| fields.iter().find(|f| f.name.as_deref() == Some(name)).unwrap();
    assert_eq!(of("ints").field_type.as_deref(), Some("IntegerList"));
    assert_eq!(of("int64s").field_type.as_deref(), Some("Integer64List"));
    assert_eq!(of("reals").field_type.as_deref(), Some("RealList"));
    assert_eq!(of("strings").field_type.as_deref(), Some("StringList"));
    assert_eq!((of("booleans").field_type.as_deref(), of("booleans").sub_type.as_deref()), (Some("IntegerList"), Some("Boolean")));
    // GDAL's GeoJSON reader gives boolean lists a width of 1.
    assert_eq!(of("booleans").width, Some(1));
    assert_eq!((of("mixed").field_type.as_deref(), of("mixed").sub_type.as_deref()), (Some("String"), Some("JSON")));
}

#[test]
fn list_types_are_not_set_on_fields() {
    let ds = terrio::open_vector(&data("cases.geojson"), &Default::default()).unwrap();
    let to = terrio::ops::vector_schema::OgrType::new(terrio::ops::vector_schema::OgrFieldType::IntegerList);
    let selector = terrio::ops::vector_schema::FieldSelector::Name("ints".into());
    assert!(terrio::ops::vector_schema::set_field_type(&ds.layers[0], &selector, to).is_err());
}
