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

/// Whether the start of a JSON text is ESRI JSON, by GDAL's signs: a geometry type, field
/// aliases, ESRI field types, features with rings or attributes, or a spatial reference by WKID.
/// A service's error answer counts too, so that it is reported as one. A GeoJSON
/// `FeatureCollection` is not.
pub(crate) fn is_esrijson(text: &str) -> bool {
    let text = text.trim_start_matches('\u{feff}').trim_start();
    let text = text.get(..text.char_indices().nth(HEAD_BYTES).map_or(text.len(), |(i, _)| i)).unwrap_or(text);
    if !text.starts_with('{') || text.contains("\"FeatureCollection\"") {
        return false;
    }
    if (text.contains("\"geometryType\"") && text.contains("\"esriGeometry"))
        || text.contains("\"fieldAliases\"")
        || (text.contains("\"fields\"") && text.contains("\"esriFieldType"))
    {
        return true;
    }
    let compact: String = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    compact.starts_with("{\"features\":[{\"geometry\":{\"rings\":[")
        || compact.starts_with("{\"features\":[{\"attributes\":{")
        || compact.starts_with("{\"error\":{\"code\":")
        || compact.contains("\"spatialReference\":{\"wkid\":")
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
        None => &[],
    };
    let fields = schema(doc, features);
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
        layer.push(Feature { fid: fid.unwrap_or(n as u64), geometry, attributes: values });
    }
    Ok(featureservice::Page { layer, more: doc.get("exceededTransferLimit").and_then(Value::as_bool).unwrap_or(false) })
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
    let mut seen: Vec<(String, Option<FieldType>)> = Vec::new();
    for attributes in features.iter().filter_map(|f| f.get("attributes").and_then(Value::as_object)) {
        for (name, v) in attributes {
            let t = match v {
                Value::Null => None,
                Value::Number(n) if n.is_i64() || n.is_u64() => Some(FieldType::Integer),
                Value::Number(_) => Some(FieldType::Float),
                _ => Some(FieldType::Text),
            };
            match seen.iter_mut().find(|(n, _)| n == name) {
                Some((_, kind)) => {
                    *kind = match (*kind, t) {
                        (k, None) => k,
                        (None, t) => t,
                        (Some(a), Some(b)) if a == b => Some(a),
                        (Some(FieldType::Integer), Some(FieldType::Float)) | (Some(FieldType::Float), Some(FieldType::Integer)) => {
                            Some(FieldType::Float)
                        }
                        _ => Some(FieldType::Text),
                    }
                }
                None => seen.push((name.clone(), t)),
            }
        }
    }
    seen.into_iter().map(|(name, t)| Field { def: FieldDef::new(&name, t.unwrap_or(FieldType::Text)), object_id: false }).collect()
}

/// An attribute value, as the field's type reads it.
fn value(v: Option<&Value>, field: &FieldDef) -> FieldValue {
    let Some(v) = v.filter(|v| !v.is_null()) else { return FieldValue::Null };
    match field.field_type {
        FieldType::Integer => v
            .as_i64()
            .or_else(|| v.as_f64().filter(|f| f.is_finite()).map(|f| f as i64))
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
            .map_or(FieldValue::Null, FieldValue::Integer),
        FieldType::Float => {
            v.as_f64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())).map_or(FieldValue::Null, FieldValue::Float)
        }
        FieldType::DateTime => match v.as_i64().or_else(|| v.as_f64().filter(|f| f.is_finite()).map(|f| f as i64)) {
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

/// A position's numbers.
fn numbers(v: &Value) -> std::result::Result<Vec<f64>, String> {
    let a = v.as_array().ok_or("a position is not an array")?;
    if !(2..=4).contains(&a.len()) {
        return Err(format!("a position has {} numbers, not 2 to 4", a.len()));
    }
    a.iter().map(|n| n.as_f64().filter(|f| f.is_finite()).ok_or_else(|| "a coordinate is not a finite number".to_string())).collect()
}

fn positions(v: &Value, has_z: bool, has_m: bool) -> std::result::Result<Vec<Coord>, String> {
    let a = v.as_array().ok_or("a path or ring is not an array")?;
    a.iter()
        .map(|p| {
            let n = numbers(p)?;
            featureservice::coordinate(&n, has_z, has_m).ok_or_else(|| "a position has fewer than two numbers".into())
        })
        .collect()
}

fn parts(v: &Value, has_z: bool, has_m: bool) -> std::result::Result<Vec<Vec<Coord>>, String> {
    v.as_array().ok_or("paths or rings are not an array")?.iter().map(|p| positions(p, has_z, has_m)).collect()
}

/// A geometry. A point whose `x` is null or `"NaN"` is empty: no geometry.
fn geometry(g: &Value, has_z: bool, has_m: bool) -> std::result::Result<Option<Geometry>, String> {
    let g = g.as_object().ok_or("a geometry is not an object")?;
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
        return Ok(Some(featureservice::polyline(parts(paths, has_z, has_m)?)));
    }
    if let Some(rings) = g.get("rings") {
        return Ok(featureservice::polygon(parts(rings, has_z, has_m)?));
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
    fn errors_name_what_is_wrong() {
        for (doc, says) in [
            (r#"{"error": {"code": 400, "message": "Invalid query"}}"#, "Invalid query"),
            (r#"{"features": [{"geometry": {"curveRings": []}}]}"#, "curve"),
            (r#"{"features": [{"geometry": {"rings": [[[0,0],[1]]]}}]}"#, "1 numbers, not 2 to 4"),
            (r#"{"features": [{"geometry": {"paths": [[[0,"a"]]]}}]}"#, "finite"),
            (r#"{"features": [{"geometry": {"envelope": 1}}]}"#, "no x"),
            (r#"{"features": 3}"#, "not an array"),
            (r#"[1]"#, "not a JSON object"),
            ("{", "ESRIJSON"),
        ] {
            let e = parse(doc.as_bytes(), "x").unwrap_err().to_string();
            assert!(e.contains(says), "{doc}: {e}");
        }
    }

    #[test]
    fn recognized_as_gdal_recognizes_it() {
        assert!(is_esrijson(r#"{"objectIdFieldName":"OID","geometryType":"esriGeometryPoint"}"#));
        assert!(is_esrijson(r#"{"displayFieldName":"","fieldAliases":{"A":"A"}}"#));
        assert!(is_esrijson(r#"{"fields":[{"name":"A","type":"esriFieldTypeString"}]}"#));
        assert!(is_esrijson("{ \"features\" : [ { \"attributes\" : { } } ] }"));
        assert!(is_esrijson(r#"{"x":1,"y":2,"spatialReference":{"wkid":4326}}"#));
        assert!(!is_esrijson(r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{}}]}"#));
        assert!(!is_esrijson(r#"{"type":"Topology","objects":{}}"#));
        assert!(!is_esrijson("not json"));
    }
}
