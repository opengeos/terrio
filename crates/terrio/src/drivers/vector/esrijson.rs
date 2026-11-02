// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

// Written to match the behavior of:
//   GDAL v3.13.3:
//     ogr/ogrsf_frmts/geojson/ogresrijsonreader.cpp
//     ogr/ogrsf_frmts/geojson/ogrgeojsonutils.cpp (ESRIJSONIsObject)
//     ogr/ogresrijsongeometry.cpp
// No code is translated from these sources.

//! The JSON of ArcGIS feature-service queries (`f=json`), read as GDAL's `ESRIJSON` driver reads
//! it: one layer, its fields from `fields` (else `fieldAliases`, as text, else the attributes'
//! values), its geometry type from `geometryType` and its CRS from `spatialReference`. See
//! [`super::featureservice`] for how fields, values and geometries are read.
//!
//! A service's error answer (`{"error": {...}}`) is an error naming its message. Curve
//! geometries (`curvePaths`, `curveRings`) are refused, rather than read without their arcs.

use super::featureservice::{self, Field};
use crate::{Error, Result};
use serde_json::{Map, Value};
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, Layer};

fn invalid(message: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("ESRIJSON: {message}"))
}

/// How much of a document [`is_esrijson`] looks at, as GDAL's driver does.
pub(crate) const HEAD_BYTES: usize = 6000;

/// The memory for attribute values that any document may take, whatever its size.
const MAX_SPARSE_CELL_BYTES: usize = 1 << 30;

/// Whether a JSON text's root `type` says it is GeoJSON or TopoJSON. The signs of ESRI JSON can
/// appear among such a document's properties. ESRI JSON's own `type`, as a layer's description
/// has (`Feature Layer`), is none of these.
fn is_other_format(text: &str) -> bool {
    crate::inspection::root_json_type(text).is_some_and(|t| {
        matches!(
            t.as_str(),
            "FeatureCollection"
                | "Feature"
                | "Point"
                | "MultiPoint"
                | "LineString"
                | "MultiLineString"
                | "Polygon"
                | "MultiPolygon"
                | "GeometryCollection"
                | "Topology"
        )
    })
}

/// Whether the start of a JSON text, its first [`HEAD_BYTES`], is ESRI JSON, by GDAL's signs: a
/// geometry type, field aliases, ESRI field types, features with rings or attributes, or a spatial
/// reference by WKID. A service's error answer counts too, so that it is reported as one. A text
/// whose root `type` is GeoJSON's or TopoJSON's does not.
pub(crate) fn is_esrijson(bytes: &[u8]) -> bool {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes).trim_ascii_start();
    let text = String::from_utf8_lossy(bytes.get(..HEAD_BYTES).unwrap_or(bytes));
    if !text.starts_with('{') || is_other_format(&text) {
        return false;
    }
    if (text.contains("\"geometryType\"") && text.contains("\"esriGeometry"))
        || text.contains("\"fieldAliases\"")
        || (text.contains("\"fields\"") && text.contains("\"esriFieldType"))
    {
        return true;
    }
    let compact: String = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    compact.contains("\"features\":[{\"geometry\":{\"rings\":[")
        || compact.contains("\"features\":[{\"attributes\":{")
        || compact.starts_with("{\"error\":{")
        || compact.contains("\"spatialReference\":{\"wkid\":")
}

/// [`is_esrijson`] for a whole document, whose root `type` may follow its first bytes.
pub(crate) fn is_esrijson_document(text: &str) -> bool {
    is_esrijson(text.as_bytes()) && !is_other_format(text.trim_start_matches('\u{feff}').trim_start())
}

/// Reads an ESRI JSON file; the layer is named after it.
pub fn read(path: &str) -> Result<Layer> {
    let stem = std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("ESRIJSON");
    parse(&std::fs::read(path)?, stem)
}

/// Parses ESRI JSON into a layer named `name`.
pub fn parse(bytes: &[u8], name: &str) -> Result<Layer> {
    page(bytes, name).map(|page| page.layer)
}

/// A page of a query's results: its layer, and whether the service has more
/// (`exceededTransferLimit`).
pub(crate) fn page(bytes: &[u8], name: &str) -> Result<featureservice::Page> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    super::geojson::reserve_parse_memory(bytes.len(), "ESRIJSON")?;
    let doc: Value = serde_json::from_slice(bytes).map_err(invalid)?;
    let doc = doc.as_object().ok_or_else(|| invalid("not a JSON object"))?;
    if let Some(error) = doc.get("error") {
        let message = error["message"].as_str().unwrap_or("no message");
        let code = error["code"].as_i64().map(|c| format!(" ({c})")).unwrap_or_default();
        return Err(invalid(format!("the service answered with an error{code}: {message}")));
    }
    let features = match doc.get("features") {
        Some(Value::Array(features)) => features.as_slice(),
        Some(_) => return Err(invalid("features is not an array")),
        // A service's description (a URL without its `/query`) shows the signs and has no features.
        None => return Err(invalid("the document has no features: it is not a query's result")),
    };
    let mut fields = schema(doc, features);
    // The layer keeps a name once, and values are by position.
    let mut names = std::collections::HashSet::new();
    fields.retain(|f| names.insert(f.def.name.clone()));
    // Every feature takes a value per field, given or not, so a small document can ask for far more
    // than its size.
    let cells = features.len().saturating_mul(fields.len()).saturating_mul(std::mem::size_of::<FieldValue>());
    if cells > MAX_SPARSE_CELL_BYTES.max(bytes.len().saturating_mul(super::geojson::PARSE_MEMORY_PER_BYTE)) {
        // An invalid argument, as GDAL fallback does not retry those: GDAL would build these cells.
        return Err(Error::InvalidArgument(format!(
            "ESRIJSON: {} features of {} fields need about {cells} bytes of memory, too many for a document of {} bytes",
            features.len(),
            fields.len(),
            bytes.len()
        )));
    }
    let mut layer = Layer::new(name);
    for f in &fields {
        layer.add_field(f.def.clone());
    }
    layer.geom_type = doc.get("geometryType").and_then(Value::as_str).and_then(featureservice::layer_geometry_type);
    let crs = spatial_reference(doc.get("spatialReference"))
        .or_else(|| features.iter().find_map(|f| spatial_reference(f.get("geometry").and_then(|g| g.get("spatialReference")))));
    if let Some(crs) = crs {
        crs.apply_to_layer(&mut layer);
    }
    let has_z = doc.get("hasZ").and_then(Value::as_bool).unwrap_or(false);
    let has_m = doc.get("hasM").and_then(Value::as_bool).unwrap_or(false);
    let object_id = fields.iter().position(|f| f.object_id);
    let mut by_position = Vec::new();
    for (n, f) in features.iter().enumerate() {
        let empty = Map::new();
        let attributes = f.get("attributes").and_then(Value::as_object).unwrap_or(&empty);
        let values: Vec<FieldValue> = fields.iter().map(|field| value(attributes.get(&field.def.name), &field.def)).collect();
        let fid = object_id.and_then(|i| match values.get(i) {
            Some(FieldValue::Integer(id)) => u64::try_from(*id).ok(),
            _ => None,
        });
        let geometry = match f.get("geometry") {
            None | Some(Value::Null) => None,
            Some(g) => geometry(g, has_z, has_m).map_err(|e| invalid(format!("feature {n}: {e}")))?,
        };
        if fid.is_none() {
            by_position.push(n);
        }
        layer.push(Feature { fid: fid.unwrap_or(n as u64), geometry, attributes: values });
    }
    featureservice::number_by_position(&mut layer, &by_position);
    Ok(featureservice::Page { layer, more: doc.get("exceededTransferLimit").and_then(Value::as_bool).unwrap_or(false), by_position })
}

fn spatial_reference(sr: Option<&Value>) -> Option<crate::crs::SpatialRef> {
    let sr = sr?;
    let code = |key: &str| sr.get(key).and_then(Value::as_u64).and_then(|c| u32::try_from(c).ok());
    featureservice::spatial_reference(code("latestWkid"), code("wkid"), sr.get("wkt").and_then(Value::as_str))
}

/// The fields: from `fields`, else every `fieldAliases` name as text, else the attributes' names
/// in the order they first appear, typed from their values.
fn schema(doc: &Map<String, Value>, features: &[Value]) -> Vec<Field> {
    if let Some(Value::Array(fields)) = doc.get("fields") {
        return fields
            .iter()
            .filter_map(|f| {
                let name = f.get("name")?.as_str()?;
                let esri = f.get("type").and_then(Value::as_str).unwrap_or("");
                Some(featureservice::field(name, esri, f.get("length").and_then(Value::as_i64)))
            })
            .collect();
    }
    if let Some(Value::Object(aliases)) = doc.get("fieldAliases") {
        return aliases.keys().map(|name| Field { def: FieldDef::new(name, FieldType::Text), object_id: false }).collect();
    }
    let mut seen: Vec<(&str, Option<FieldType>)> = Vec::new();
    let mut index = std::collections::HashMap::new();
    for attributes in features.iter().filter_map(|f| f.get("attributes").and_then(Value::as_object)) {
        for (name, v) in attributes {
            // Booleans and nested values are read as text.
            let t =
                super::topojson::json_type(v).map(|t| if matches!(t, FieldType::Integer | FieldType::Float) { t } else { FieldType::Text });
            let at = *index.entry(name.as_str()).or_insert_with(|| {
                seen.push((name.as_str(), None));
                seen.len() - 1
            });
            if let Some((_, kind)) = seen.get_mut(at) {
                *kind = super::topojson::widen(*kind, t);
            }
        }
    }
    seen.into_iter().map(|(name, t)| Field { def: FieldDef::new(name, t.unwrap_or(FieldType::Text)), object_id: false }).collect()
}

/// A number as a whole one, when it fits in 64 bits: one outside that range is none, not the
/// nearest that fits.
fn whole(v: &Value) -> Option<i64> {
    // 2^63, which an `f64` holds exactly; `i64::MAX` as an `f64` rounds up to it.
    const LIMIT: f64 = 9_223_372_036_854_775_808.0;
    v.as_i64().or_else(|| v.as_f64().filter(|f| f.is_finite() && (-LIMIT..LIMIT).contains(f)).map(|f| f as i64))
}

/// An attribute value, as the field's type reads it.
fn value(v: Option<&Value>, field: &FieldDef) -> FieldValue {
    let Some(v) = v.filter(|v| !v.is_null()) else { return FieldValue::Null };
    match field.field_type {
        FieldType::Integer => {
            whole(v).or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())).map_or(FieldValue::Null, FieldValue::Integer)
        }
        FieldType::Float => v
            .as_f64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
            .filter(|f: &f64| f.is_finite())
            .map_or(FieldValue::Null, FieldValue::Float),
        FieldType::DateTime => match whole(v) {
            Some(ms) => featureservice::date_time(ms),
            None => v.as_str().map_or(FieldValue::Null, |s| FieldValue::DateTime(s.to_string())),
        },
        FieldType::Date => match v {
            Value::String(s) => FieldValue::Date(s.clone()),
            v => v.as_i64().map_or(FieldValue::Null, |ms| FieldValue::Date(super::datetime::format_date(ms.div_euclid(86_400_000)))),
        },
        _ => match v {
            Value::String(s) => FieldValue::Text(s.clone()),
            other => FieldValue::Text(other.to_string()),
        },
    }
}

/// A position. X and Y must be finite numbers; a Z or M of null or `"NaN"` is none.
fn position(v: &Value, has_z: bool, has_m: bool) -> std::result::Result<Coord, String> {
    let a = v.as_array().ok_or("a position is not an array")?;
    let not_finite = || "a coordinate is not a finite number".to_string();
    let [x, y, extra @ ..] = a.as_slice() else {
        return Err(format!("a position has {} numbers, not 2 to 4", a.len()));
    };
    if extra.len() > 2 {
        return Err(format!("a position has {} numbers, not 2 to 4", a.len()));
    }
    let number = |n: &Value| n.as_f64().filter(|f| f.is_finite()).ok_or_else(not_finite);
    let extra = extra
        .iter()
        .map(|n| if n.is_null() || n.as_str() == Some("NaN") { Ok(None) } else { number(n).map(Some) })
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(featureservice::coordinate(number(x)?, number(y)?, &extra, has_z, has_m))
}

fn positions(v: &Value, has_z: bool, has_m: bool) -> std::result::Result<Vec<Coord>, String> {
    v.as_array().ok_or("a path or ring is not an array")?.iter().map(|p| position(p, has_z, has_m)).collect()
}

fn parts(v: &Value, has_z: bool, has_m: bool) -> std::result::Result<Vec<Vec<Coord>>, String> {
    v.as_array().ok_or("paths or rings are not an array")?.iter().map(|p| positions(p, has_z, has_m)).collect()
}

/// A geometry. An empty object, or a point whose `x` is null or `"NaN"`, is no geometry.
fn geometry(g: &Value, has_z: bool, has_m: bool) -> std::result::Result<Option<Geometry>, String> {
    let g = g.as_object().ok_or("a geometry is not an object")?;
    if g.is_empty() {
        return Ok(None);
    }
    let has_z = g.get("hasZ").and_then(Value::as_bool).unwrap_or(has_z);
    let has_m = g.get("hasM").and_then(Value::as_bool).unwrap_or(has_m);
    if let Some(x) = g.get("x") {
        let coordinate = |key: &str| g.get(key).and_then(Value::as_f64).filter(|f| f.is_finite());
        let (Some(x), Some(y)) = (x.as_f64().filter(|f| f.is_finite()), coordinate("y")) else { return Ok(None) };
        return Ok(Some(Geometry::Point(Coord { x, y, z: coordinate("z"), m: coordinate("m") })));
    }
    if let Some(points) = g.get("points") {
        return Ok(Some(Geometry::MultiPoint(positions(points, has_z, has_m)?)));
    }
    if let Some(paths) = g.get("paths") {
        return Ok(featureservice::polyline(parts(paths, has_z, has_m)?));
    }
    if let Some(rings) = g.get("rings") {
        return Ok(super::polygons_from_rings(parts(rings, has_z, has_m)?));
    }
    if g.contains_key("curvePaths") || g.contains_key("curveRings") {
        return Err("curve geometries are not read".into());
    }
    Err("the geometry has no x, points, paths or rings".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LAYER: &str = r#"{
        "objectIdFieldName": "OBJECTID",
        "geometryType": "esriGeometryPolygon",
        "spatialReference": {"wkid": 102100, "latestWkid": 3857},
        "fields": [
            {"name": "OBJECTID", "type": "esriFieldTypeOID", "alias": "OBJECTID"},
            {"name": "NAME", "type": "esriFieldTypeString", "length": 20},
            {"name": "POP", "type": "esriFieldTypeInteger"},
            {"name": "AREA", "type": "esriFieldTypeDouble"},
            {"name": "BUILT", "type": "esriFieldTypeDate", "length": 8},
            {"name": "DAY", "type": "esriFieldTypeDateOnly"},
            {"name": "GUID", "type": "esriFieldTypeGlobalID"}
        ],
        "features": [
            {"attributes": {"OBJECTID": 7, "NAME": "Lake", "POP": 12, "AREA": 1.5, "BUILT": 0, "DAY": "2020-01-31", "GUID": "{A}"},
             "geometry": {"rings": [[[0,0],[0,4],[4,4],[4,0],[0,0]], [[1,1],[2,1],[2,2],[1,2],[1,1]]]}},
            {"attributes": {"OBJECTID": 8, "NAME": null, "POP": "3", "AREA": 2, "BUILT": null},
             "geometry": null}
        ]
    }"#;

    fn field<'a>(layer: &'a Layer, row: usize, name: &str) -> &'a FieldValue {
        &layer.features[row].attributes[layer.schema.field_index(name).unwrap()]
    }

    #[test]
    fn reads_fields_values_ids_and_the_crs() {
        let l = parse(LAYER.as_bytes(), "lakes").unwrap();
        assert_eq!(l.name, "lakes");
        assert_eq!(l.features.len(), 2);
        assert_eq!((l.features[0].fid, l.features[1].fid), (7, 8));
        assert_eq!(l.crs_epsg(), Some(3857));
        assert_eq!(l.geom_type, Some(wbvector::GeometryType::Polygon));
        assert_eq!(l.schema.fields()[1].width, 20);
        assert_eq!(field(&l, 0, "NAME"), &FieldValue::Text("Lake".into()));
        assert_eq!(field(&l, 0, "POP"), &FieldValue::Integer(12));
        assert_eq!(field(&l, 1, "POP"), &FieldValue::Integer(3));
        assert_eq!(field(&l, 1, "AREA"), &FieldValue::Float(2.0));
        assert_eq!(field(&l, 0, "BUILT"), &FieldValue::DateTime("1970-01-01T00:00:00Z".into()));
        assert_eq!(field(&l, 0, "DAY"), &FieldValue::Date("2020-01-31".into()));
        assert_eq!(field(&l, 1, "NAME"), &FieldValue::Null);
        assert_eq!(field(&l, 1, "DAY"), &FieldValue::Null);
        let Some(Geometry::Polygon { interiors, .. }) = &l.features[0].geometry else { panic!("not a polygon") };
        assert_eq!(interiors.len(), 1);
        assert!(l.features[1].geometry.is_none());
    }

    #[test]
    fn points_lines_and_multipoints_with_z_and_m() {
        let doc = r#"{"geometryType": "esriGeometryPolyline", "hasZ": true, "fieldAliases": {"A": "A"},
            "features": [
                {"attributes": {"A": 1}, "geometry": {"paths": [[[0,0,5],[1,1,6]]]}},
                {"attributes": {"A": 2}, "geometry": {"paths": [[[0,0,1],[1,1,1]], [[5,5,1],[6,6,1]]]}},
                {"attributes": {"A": 3}, "geometry": {"x": 1, "y": 2, "z": 3, "m": 4}},
                {"attributes": {"A": 4}, "geometry": {"hasM": true, "hasZ": false, "points": [[1,2,9]]}},
                {"attributes": {"A": 5}, "geometry": {"x": "NaN", "y": "NaN"}}
            ]}"#;
        let l = parse(doc.as_bytes(), "x").unwrap();
        assert_eq!(field(&l, 0, "A"), &FieldValue::Text("1".into()), "fieldAliases fields are text");
        assert!(matches!(&l.features[0].geometry, Some(Geometry::LineString(p)) if p[1].z == Some(6.0)));
        assert!(matches!(&l.features[1].geometry, Some(Geometry::MultiLineString(p)) if p.len() == 2));
        assert!(matches!(&l.features[2].geometry, Some(Geometry::Point(c)) if (c.z, c.m) == (Some(3.0), Some(4.0))));
        assert!(matches!(&l.features[3].geometry, Some(Geometry::MultiPoint(p)) if (p[0].z, p[0].m) == (None, Some(9.0))));
        assert!(l.features[4].geometry.is_none());
    }

    #[test]
    fn without_fields_the_attributes_give_them() {
        let doc = r#"{"features": [{"attributes": {"A": 1, "B": "x"}}, {"attributes": {"A": 1.5, "C": null}}]}"#;
        let l = parse(doc.as_bytes(), "x").unwrap();
        let types: Vec<FieldType> = l.schema.fields().iter().map(|f| f.field_type).collect();
        assert_eq!(types, [FieldType::Float, FieldType::Text, FieldType::Text]);
    }

    #[test]
    fn a_whole_number_past_64_bits_is_null_not_the_largest() {
        let doc = r#"{"fields": [{"name": "A", "type": "esriFieldTypeBigInteger"}, {"name": "D", "type": "esriFieldTypeDate"}],
            "features": [{"attributes": {"A": 18446744073709551615, "D": 1e300}}, {"attributes": {"A": -9.3e18, "D": 1.5}},
                         {"attributes": {"A": 9223372036854775807, "D": -1}}]}"#;
        let l = parse(doc.as_bytes(), "x").unwrap();
        assert_eq!(field(&l, 0, "A"), &FieldValue::Null);
        assert_eq!(field(&l, 0, "D"), &FieldValue::Null);
        assert_eq!(field(&l, 1, "A"), &FieldValue::Null);
        assert_eq!(field(&l, 1, "D"), &FieldValue::DateTime("1970-01-01T00:00:00.001Z".into()));
        assert_eq!(field(&l, 2, "A"), &FieldValue::Integer(i64::MAX));
    }

    #[test]
    fn errors_name_what_is_wrong() {
        for (doc, says) in [
            (r#"{"error": {"code": 400, "message": "Invalid query"}}"#, "Invalid query"),
            (r#"{"features": [{"geometry": {"curveRings": []}}]}"#, "curve"),
            (r#"{"features": [{"geometry": {"rings": [[[0,0],[1]]]}}]}"#, "1 numbers, not 2 to 4"),
            (r#"{"features": [{"geometry": {"paths": [[[0,"a"]]]}}]}"#, "finite"),
            (r#"{"features": [{"geometry": {"envelope": 1}}]}"#, "no x"),
            (r#"{"features": 3}"#, "not an array"),
            (r#"{"id": 0, "type": "Feature Layer", "geometryType": "esriGeometryPoint"}"#, "no features"),
            (r#"{"x": 1, "y": 2, "spatialReference": {"wkid": 4326}}"#, "no features"),
            (r#"[1]"#, "not a JSON object"),
            ("{", "ESRIJSON"),
        ] {
            let e = parse(doc.as_bytes(), "x").unwrap_err().to_string();
            assert!(e.contains(says), "{doc}: {e}");
        }
    }

    #[test]
    fn a_null_z_or_m_is_none_and_an_empty_geometry_no_geometry() {
        let doc = r#"{"hasM": true, "hasZ": true, "features": [
            {"geometry": {"paths": [[[0,0,1,null],[1,1,null,2],[2,2,null,null],[3,3,"NaN",4]]]}},
            {"geometry": {}},
            {"geometry": {"paths": []}}
        ]}"#;
        let l = parse(doc.as_bytes(), "x").unwrap();
        let Some(Geometry::LineString(line)) = &l.features[0].geometry else { panic!("not a line") };
        let extra: Vec<_> = line.iter().map(|c| (c.z, c.m)).collect();
        assert_eq!(extra, [(Some(1.0), None), (None, Some(2.0)), (None, None), (None, Some(4.0))]);
        assert!(l.features[1].geometry.is_none());
        assert!(l.features[2].geometry.is_none());
        // X and Y are required, and so is a number where a Z is given.
        for bad in ["[null,1]", "[1,null]", r#"[1,2,"a"]"#, "[1,2,3,4,5]", "[1]"] {
            let doc = format!(r#"{{"features": [{{"geometry": {{"paths": [[{bad}]]}}}}]}}"#);
            assert!(parse(doc.as_bytes(), "x").is_err(), "{bad}");
        }
    }

    #[test]
    fn a_repeated_field_name_does_not_shift_the_values() {
        let doc = r#"{"fields": [{"name": "A", "type": "esriFieldTypeString"}, {"name": "A", "type": "esriFieldTypeString"},
            {"name": "B", "type": "esriFieldTypeString"}], "features": [{"attributes": {"A": "a", "B": "b"}}]}"#;
        let l = parse(doc.as_bytes(), "x").unwrap();
        assert_eq!(l.schema.fields().len(), 2);
        assert_eq!(l.features[0].attributes, [FieldValue::Text("a".into()), FieldValue::Text("b".into())]);
    }

    #[test]
    fn a_real_of_text_that_is_not_finite_is_null() {
        let doc = r#"{"fields": [{"name": "A", "type": "esriFieldTypeDouble"}], "features": [
            {"attributes": {"A": "NaN"}}, {"attributes": {"A": "inf"}}, {"attributes": {"A": "1.5"}}]}"#;
        let l = parse(doc.as_bytes(), "x").unwrap();
        assert_eq!(field(&l, 0, "A"), &FieldValue::Null);
        assert_eq!(field(&l, 1, "A"), &FieldValue::Null);
        assert_eq!(field(&l, 2, "A"), &FieldValue::Float(1.5));
    }

    #[test]
    fn booleans_and_nested_values_without_fields_are_text() {
        let doc = r#"{"features": [{"attributes": {"A": true, "B": [1]}}]}"#;
        let l = parse(doc.as_bytes(), "x").unwrap();
        assert_eq!(field(&l, 0, "A"), &FieldValue::Text("true".into()));
        assert_eq!(field(&l, 0, "B"), &FieldValue::Text("[1]".into()));
    }

    #[test]
    fn content_signs_are_recognized() {
        let is = |text: &str| is_esrijson(text.as_bytes());
        assert!(is(r#"{"objectIdFieldName":"OID","geometryType":"esriGeometryPoint"}"#));
        assert!(is(r#"{"displayFieldName":"","fieldAliases":{"A":"A"}}"#));
        assert!(is(r#"{"fields":[{"name":"A","type":"esriFieldTypeString"}]}"#));
        assert!(is("\u{feff} { \"features\" : [ { \"attributes\" : { } } ] }"));
        assert!(is(r#"{"x":1,"y":2,"spatialReference":{"wkid":4326}}"#));
        assert!(is(r#"{"fieldAliases":{"A":"A"},"features":[{"attributes":{"A":"FeatureCollection"}}]}"#));
        assert!(!is(r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{}}]}"#));
        assert!(!is(r#"{"type":"Feature","properties":{"spatialReference":{"wkid":4326}},"geometry":null}"#));
        assert!(!is(r#"{"type":"Point","coordinates":[1,2],"spatialReference":{"wkid":4326}}"#));
        assert!(!is(r#"{"type":"Topology","objects":{"a":{"properties":{"fieldAliases":1}}}}"#));
        assert!(!is("not json"));
    }

    #[test]
    fn a_root_type_past_the_head_is_seen_in_the_whole_document() {
        let padding = "x".repeat(HEAD_BYTES);
        let doc = format!(r#"{{"features":[{{"properties":{{"fieldAliases":"{padding}"}}}}],"type":"FeatureCollection"}}"#);
        assert!(is_esrijson(doc.as_bytes()), "its first bytes show only the sign");
        assert!(!is_esrijson_document(&doc));
    }
}
