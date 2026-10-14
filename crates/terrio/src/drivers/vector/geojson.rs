//! GeoJSON, read and written as GDAL does.
//!
//! Reading: fields in the order the properties first appear, typed from their JSON values
//! (integers, reals, booleans, ISO 8601 dates and date-times, strings; arrays and objects become
//! JSON text), string feature ids as a leading `id` field and integer ids as feature ids, and the
//! CRS from a GeoJSON 2008 `crs` member (EPSG:4326 otherwise).
//!
//! Writing keeps the layer's coordinates and names its CRS in a `crs` member when it is not
//! WGS 84, as GDAL does by default; [`GeoJsonOptions::rfc7946`] reprojects to WGS 84 instead and
//! orients polygon rings counter-clockwise, like GDAL's `RFC7946=YES`.

use crate::crs::SpatialRef;
use crate::{Error, Result};
use serde_json::{Map, Value, json};
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, Layer, Ring};

/// Options for writing GeoJSON.
#[derive(Debug, Clone, Copy, Default)]
pub struct GeoJsonOptions {
    /// Write RFC 7946 GeoJSON: coordinates reprojected to WGS 84, no `crs` member,
    /// counter-clockwise exterior rings (GDAL's `RFC7946=YES`).
    pub rfc7946: bool,
}

impl GeoJsonOptions {
    /// Sets an option by its GDAL name: `RFC7946` (YES/NO). Returns `false` for other names.
    pub fn set(&mut self, key: &str, value: &str) -> Result<bool> {
        if key.eq_ignore_ascii_case("RFC7946") {
            self.rfc7946 = matches!(value.to_ascii_uppercase().as_str(), "YES" | "TRUE" | "ON" | "1");
            return Ok(true);
        }
        Ok(false)
    }
}

/// Reads a GeoJSON file.
pub fn read(path: &str) -> Result<Layer> {
    let stem = std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("layer");
    parse(&std::fs::read(path)?, stem)
}

/// Parses GeoJSON. `default_name` names a layer whose collection has no `name`.
pub fn parse(bytes: &[u8], default_name: &str) -> Result<Layer> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let doc: Value = serde_json::from_slice(bytes).map_err(|e| Error::InvalidData(format!("GeoJSON: {e}")))?;
    let features: Vec<&Value> = match doc["type"].as_str() {
        Some("FeatureCollection") => doc["features"].as_array().map(|a| a.iter().collect()).unwrap_or_default(),
        Some("Feature") => vec![&doc],
        Some(_) => vec![&doc],
        None => return Err(Error::InvalidData("GeoJSON: no type".into())),
    };
    let geometry_of = |f: &'_ Value| -> Option<Geometry> { if f["type"] == "Feature" { geometry(&f["geometry"]) } else { geometry(f) } };

    let mut layer = Layer::new(doc["name"].as_str().unwrap_or(default_name));
    let string_ids = features.iter().any(|f| f["id"].is_string());
    if string_ids {
        layer.add_field(FieldDef::new("id", FieldType::Text));
    }
    let mut kinds: Vec<(String, Option<Kind>)> = Vec::new();
    for f in &features {
        for (k, v) in f["properties"].as_object().into_iter().flatten() {
            let t = kind(v);
            match kinds.iter_mut().find(|(n, _)| n == k) {
                Some((_, prev)) => *prev = merge(*prev, t),
                None => kinds.push((k.clone(), t)),
            }
        }
    }
    for (k, t) in &kinds {
        if layer.schema.field_index(k).is_none() {
            layer.add_field(FieldDef::new(k, t.map_or(FieldType::Text, Kind::field_type)));
        }
    }
    let types: Vec<FieldType> = layer.schema.fields().iter().map(|f| f.field_type).collect();
    for (n, f) in features.iter().enumerate() {
        let mut attributes = vec![FieldValue::Null; types.len()];
        if string_ids {
            attributes[0] = match &f["id"] {
                Value::Null => FieldValue::Null,
                Value::String(s) => FieldValue::Text(s.clone()),
                v => FieldValue::Text(v.to_string()),
            };
        }
        for (k, v) in f["properties"].as_object().into_iter().flatten() {
            if let Some(i) = layer.schema.field_index(k).filter(|&i| !(string_ids && i == 0)) {
                attributes[i] = value(v, types[i]);
            }
        }
        let fid = if string_ids { n as u64 } else { f["id"].as_u64().unwrap_or(n as u64) };
        layer.push(Feature { fid, geometry: geometry_of(f), attributes });
    }
    let crs = doc["crs"]["properties"]["name"].as_str().and_then(|n| SpatialRef::parse(n).ok());
    match crs {
        Some(s) => s.apply_to_layer(&mut layer),
        None => layer.set_crs_epsg(Some(4326)),
    }
    Ok(layer)
}

/// The type of a property value, as GDAL infers it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Integer,
    Real,
    Boolean,
    Date,
    DateTime,
    String,
    Json,
}

impl Kind {
    fn field_type(self) -> FieldType {
        match self {
            Kind::Integer => FieldType::Integer,
            Kind::Real => FieldType::Float,
            Kind::Boolean => FieldType::Boolean,
            Kind::Date => FieldType::Date,
            Kind::DateTime => FieldType::DateTime,
            Kind::String => FieldType::Text,
            Kind::Json => FieldType::Json,
        }
    }
}

fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10 && b[4] == b'-' && b[7] == b'-' && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

fn is_datetime(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 19
        && is_date(&s[..10])
        && (b[10] == b'T' || b[10] == b' ')
        && b[13] == b':'
        && b[16] == b':'
        && [11, 12, 14, 15, 17, 18].iter().all(|&i| b[i].is_ascii_digit())
}

fn kind(v: &Value) -> Option<Kind> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(_) => Kind::Boolean,
        Value::Number(n) if n.is_i64() || n.is_u64() => Kind::Integer,
        Value::Number(_) => Kind::Real,
        Value::String(s) if is_datetime(s) => Kind::DateTime,
        Value::String(s) if is_date(s) => Kind::Date,
        Value::String(_) => Kind::String,
        _ => Kind::Json,
    })
}

fn merge(a: Option<Kind>, b: Option<Kind>) -> Option<Kind> {
    use Kind as K;
    Some(match (a, b) {
        (a, None) => return a,
        (None, b) => return b,
        (Some(a), Some(b)) if a == b => a,
        (Some(K::Integer | K::Real), Some(K::Integer | K::Real)) => K::Real,
        (Some(K::Date | K::DateTime), Some(K::Date | K::DateTime)) => K::DateTime,
        _ => K::String,
    })
}

fn value(v: &Value, t: FieldType) -> FieldValue {
    match (t, v) {
        (_, Value::Null) => FieldValue::Null,
        (FieldType::Integer, v) => v.as_i64().map_or(FieldValue::Null, FieldValue::Integer),
        (FieldType::Float, v) => v.as_f64().map_or(FieldValue::Null, FieldValue::Float),
        (FieldType::Boolean, Value::Bool(b)) => FieldValue::Boolean(*b),
        (FieldType::Date, Value::String(s)) => FieldValue::Date(s.clone()),
        (FieldType::DateTime, Value::String(s)) => FieldValue::DateTime(s.clone()),
        (_, Value::String(s)) => FieldValue::Text(s.clone()),
        (_, v) => FieldValue::Text(v.to_string()),
    }
}

fn position(v: &Value) -> Option<Coord> {
    let a = v.as_array()?;
    Some(Coord { x: a.first()?.as_f64()?, y: a.get(1)?.as_f64()?, z: a.get(2).and_then(Value::as_f64), m: None })
}

fn positions(v: &Value) -> Vec<Coord> {
    v.as_array().into_iter().flatten().filter_map(position).collect()
}

fn ring(v: &Value) -> Ring {
    let mut cs = positions(v);
    if cs.len() > 1 && cs.first().map(|c| (c.x, c.y)) == cs.last().map(|c| (c.x, c.y)) {
        cs.pop();
    }
    Ring::new(cs)
}

fn polygon(v: &Value) -> Option<(Ring, Vec<Ring>)> {
    let mut rings = v.as_array()?.iter().map(ring);
    let exterior = rings.next()?;
    Some((exterior, rings.collect()))
}

/// Parses a GeoJSON geometry object.
pub fn geometry(g: &Value) -> Option<Geometry> {
    let c = &g["coordinates"];
    let list = || c.as_array().into_iter().flatten();
    Some(match g["type"].as_str()? {
        "Point" => Geometry::Point(position(c)?),
        "MultiPoint" => Geometry::MultiPoint(positions(c)),
        "LineString" => Geometry::LineString(positions(c)),
        "MultiLineString" => Geometry::MultiLineString(list().map(positions).collect()),
        "Polygon" => {
            let (exterior, interiors) = polygon(c)?;
            Geometry::Polygon { exterior, interiors }
        }
        "MultiPolygon" => Geometry::MultiPolygon(list().filter_map(polygon).collect()),
        "GeometryCollection" => {
            Geometry::GeometryCollection(g["geometries"].as_array().into_iter().flatten().filter_map(geometry).collect())
        }
        _ => return None,
    })
}

/// Writes a layer as GeoJSON.
pub fn write(layer: &Layer, path: &str, options: &GeoJsonOptions) -> Result<()> {
    std::fs::write(path, to_string(layer, options)?)?;
    Ok(())
}

/// Encodes a layer as a GeoJSON FeatureCollection.
pub fn to_string(layer: &Layer, options: &GeoJsonOptions) -> Result<String> {
    let srs = SpatialRef::from_layer(layer);
    let reprojected;
    let layer = if options.rfc7946 && !srs.is_unknown() && srs.epsg != Some(4326) {
        reprojected = crate::vector::reproject_layer(layer, &SpatialRef::from_epsg(4326)?)?;
        &reprojected
    } else {
        layer
    };
    let mut doc = Map::new();
    doc.insert("type".into(), json!("FeatureCollection"));
    doc.insert("name".into(), json!(layer.name));
    if !options.rfc7946
        && !srs.is_unknown()
        && srs.epsg != Some(4326)
        && let Some(code) = super::trusted_epsg(layer)
    {
        doc.insert("crs".into(), json!({"type": "name", "properties": {"name": format!("urn:ogc:def:crs:EPSG::{code}")}}));
    }
    let fields = layer.schema.fields();
    let features: Vec<Value> = layer
        .features
        .iter()
        .map(|f| {
            let props: Map<String, Value> = fields
                .iter()
                .zip(&f.attributes)
                .map(|(d, v)| {
                    let v = match v {
                        FieldValue::Null => Value::Null,
                        FieldValue::Integer(x) => json!(x),
                        FieldValue::Float(x) if x.is_finite() => json!(x),
                        FieldValue::Float(_) => Value::Null,
                        FieldValue::Boolean(b) => json!(b),
                        FieldValue::Text(s) if d.field_type == FieldType::Json => serde_json::from_str(s).unwrap_or_else(|_| json!(s)),
                        FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => json!(s),
                        FieldValue::Blob(b) => json!(b.iter().map(|x| format!("{x:02X}")).collect::<String>()),
                    };
                    (d.name.clone(), v)
                })
                .collect();
            json!({
                "type": "Feature",
                "properties": props,
                "geometry": f.geometry.as_ref().map_or(Value::Null, |g| encode(g, options.rfc7946)),
            })
        })
        .collect();
    doc.insert("features".into(), Value::Array(features));
    // One feature per line, as GDAL writes it.
    let text = Value::Object(doc).to_string();
    Ok(text.replace("{\"type\":\"Feature\",", "\n{\"type\":\"Feature\",") + "\n")
}

fn pos(c: &Coord) -> Value {
    match c.z {
        Some(z) => json!([c.x, c.y, z]),
        None => json!([c.x, c.y]),
    }
}

/// A closed ring's positions; with `ccw`, exterior rings counter-clockwise and holes clockwise.
fn ring_json(r: &Ring, exterior: bool, ccw: bool) -> Value {
    let mut cs = r.0.clone();
    if ccw && (r.signed_area() > 0.0) != exterior {
        cs.reverse();
    }
    if let Some(first) = cs.first().cloned()
        && cs.len() > 1
        && cs.last().map(|l| (l.x, l.y)) != Some((first.x, first.y))
    {
        cs.push(first);
    }
    Value::Array(cs.iter().map(pos).collect())
}

fn polygon_json(e: &Ring, hs: &[Ring], ccw: bool) -> Value {
    Value::Array(std::iter::once(ring_json(e, true, ccw)).chain(hs.iter().map(|h| ring_json(h, false, ccw))).collect())
}

fn encode(g: &Geometry, ccw: bool) -> Value {
    match g {
        Geometry::Point(c) => json!({"type": "Point", "coordinates": pos(c)}),
        Geometry::MultiPoint(cs) => json!({"type": "MultiPoint", "coordinates": cs.iter().map(pos).collect::<Vec<_>>()}),
        Geometry::LineString(cs) => json!({"type": "LineString", "coordinates": cs.iter().map(pos).collect::<Vec<_>>()}),
        Geometry::MultiLineString(ls) => json!({
            "type": "MultiLineString",
            "coordinates": ls.iter().map(|l| l.iter().map(pos).collect::<Vec<_>>()).collect::<Vec<_>>(),
        }),
        Geometry::Polygon { exterior, interiors } => json!({"type": "Polygon", "coordinates": polygon_json(exterior, interiors, ccw)}),
        Geometry::MultiPolygon(ps) => {
            json!({"type": "MultiPolygon", "coordinates": ps.iter().map(|(e, h)| polygon_json(e, h, ccw)).collect::<Vec<_>>()})
        }
        Geometry::GeometryCollection(gs) => {
            json!({"type": "GeometryCollection", "geometries": gs.iter().map(|g| encode(g, ccw)).collect::<Vec<_>>()})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_like_gdal() {
        let text = r#"{"type":"FeatureCollection","name":"places","crs":{"type":"name","properties":{"name":"urn:ogc:def:crs:EPSG::32633"}},
            "features":[
            {"type":"Feature","id":"a","properties":{"name":"Côte d'Ivoire","n":3.0,"k":1,"when":"2020-01-02","tags":[1,2]},"geometry":{"type":"Point","coordinates":[500000,10]}},
            {"type":"Feature","id":"b","properties":{"name":null,"n":2,"k":2,"when":"2020-01-03T10:00:00Z","flag":true},"geometry":null}]}"#;
        let l = parse(text.as_bytes(), "file").unwrap();
        assert_eq!((l.name.as_str(), l.crs_epsg()), ("places", Some(32633)));
        let t: Vec<(&str, FieldType)> = l.schema.fields().iter().map(|f| (f.name.as_str(), f.field_type)).collect();
        assert_eq!(
            t,
            [
                ("id", FieldType::Text),
                ("name", FieldType::Text),
                ("n", FieldType::Float),
                ("k", FieldType::Integer),
                ("when", FieldType::DateTime),
                ("tags", FieldType::Json),
                ("flag", FieldType::Boolean)
            ]
        );
        assert_eq!(l.features[0].attributes[1], FieldValue::Text("Côte d'Ivoire".into()));
        assert_eq!(l.features[0].attributes[2], FieldValue::Float(3.0));

        // GDAL's default keeps the coordinates and names the CRS.
        let out = to_string(&l, &GeoJsonOptions::default()).unwrap();
        assert!(out.contains("urn:ogc:def:crs:EPSG::32633"), "{out}");
        let back = parse(out.as_bytes(), "file").unwrap();
        assert_eq!(back.crs_epsg(), Some(32633));
        assert_eq!(back.features[0].geometry, Some(Geometry::point(500000.0, 10.0)));
        assert_eq!(back.features[0].attributes, l.features[0].attributes);
        // RFC 7946: WGS 84.
        let out = to_string(&l, &GeoJsonOptions { rfc7946: true }).unwrap();
        let back = parse(out.as_bytes(), "file").unwrap();
        assert!(!out.contains("\"crs\""));
        let Some(Geometry::Point(c)) = &back.features[0].geometry else { panic!("no point") };
        assert!((c.x - 15.0).abs() < 1e-6, "{}", c.x);
    }
}
