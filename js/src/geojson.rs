//! A vector layer as a GeoJSON FeatureCollection object. The features and their properties are
//! written as JSON text a chunk at a time, each chunk given to `JSON.parse`: no text as long as
//! the whole layer's is held, in wasm memory (4 GB at most) or as a JavaScript string (about
//! 512 MB at most in V8). Geometries are made as arrays directly, not through text.

use serde_json::ser::{CompactFormatter, Formatter};
use wasm_bindgen::prelude::*;
use wbvector::{Coord, Feature, FieldValue, Geometry, Layer, Ring};

/// The length of the text parsed at once.
const CHUNK: usize = 8 << 20;

/// `{ type: "FeatureCollection", features }`, as `wbvector::geojson::to_string` writes it, with
/// strings escaped as JSON requires and property numbers that are not finite written as `null`.
pub(crate) fn feature_collection(layer: &Layer) -> Result<JsValue, JsError> {
    let keys: Vec<Vec<u8>> = layer.schema.fields().iter().map(|f| string(&f.name)).collect();
    let chunks = js_sys::Array::new();
    let mut s = Vec::with_capacity(CHUNK + (CHUNK >> 4));
    let mut first = 0;
    for (n, f) in layer.features.iter().enumerate() {
        s.push(if s.is_empty() { b'[' } else { b',' });
        feature(&mut s, f, &keys);
        if s.len() >= CHUNK || n + 1 == layer.features.len() {
            chunks.push(&parse(&mut s, &layer.features[first..=n])?);
            first = n + 1;
        }
    }
    let fc = js_sys::Object::new();
    js_sys::Reflect::set(&fc, &"type".into(), &"FeatureCollection".into()).map_err(|_| JsError::new("GeoJSON"))?;
    js_sys::Reflect::set(&fc, &"features".into(), &chunks.flat(1)).map_err(|_| JsError::new("GeoJSON"))?;
    Ok(fc.into())
}

/// The features written so far, as an array, given the geometries of `features`; `s` is emptied.
fn parse(s: &mut Vec<u8>, features: &[Feature]) -> Result<JsValue, JsError> {
    s.push(b']');
    let text = std::str::from_utf8(s).map_err(|e| JsError::new(&e.to_string()))?;
    let parsed: js_sys::Array = js_sys::JSON::parse(text).map_err(|_| JsError::new("invalid GeoJSON"))?.unchecked_into();
    s.clear();
    let (key, mut buf) = (JsValue::from_str("geometry"), Vec::new());
    for (i, f) in features.iter().enumerate() {
        if let Some(g) = &f.geometry {
            js_sys::Reflect::set(&parsed.get(i as u32), &key, &geometry(&mut buf, g)).map_err(|_| JsError::new("GeoJSON"))?;
        }
    }
    Ok(parsed.into())
}

fn string(v: &str) -> Vec<u8> {
    let mut s = Vec::with_capacity(v.len() + 2);
    text(&mut s, v);
    s
}

/// A JSON string, escaped.
fn text(s: &mut Vec<u8>, v: &str) {
    let _ = serde_json::to_writer(s, v);
}

/// A feature, its geometry `null` until [`parse`] sets it.
fn feature(s: &mut Vec<u8>, f: &Feature, keys: &[Vec<u8>]) {
    s.extend_from_slice(br#"{"type":"Feature","geometry":null,"properties":"#);
    if keys.is_empty() {
        s.extend_from_slice(b"null");
    } else {
        for (i, k) in keys.iter().enumerate() {
            s.push(if i == 0 { b'{' } else { b',' });
            s.extend_from_slice(k);
            s.push(b':');
            value(s, f.attributes.get(i).unwrap_or(&FieldValue::Null));
        }
        s.push(b'}');
    }
    s.push(b'}');
}

fn value(s: &mut Vec<u8>, v: &FieldValue) {
    match v {
        FieldValue::Null => s.extend_from_slice(b"null"),
        FieldValue::Integer(v) => {
            let _ = CompactFormatter.write_i64(s, *v);
        }
        FieldValue::Float(v) if v.is_finite() => {
            let _ = CompactFormatter.write_f64(s, *v);
        }
        FieldValue::Float(_) => s.extend_from_slice(b"null"),
        FieldValue::Boolean(v) => s.extend_from_slice(if *v { b"true" } else { b"false" }),
        FieldValue::Text(v) | FieldValue::Date(v) | FieldValue::DateTime(v) => text(s, v),
        // Hexadecimal, as wbvector writes it.
        FieldValue::Blob(b) => {
            const HEX: &[u8; 16] = b"0123456789ABCDEF";
            s.push(b'"');
            for &byte in b {
                s.extend_from_slice(&[HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]]);
            }
            s.push(b'"');
        }
    }
}

#[wasm_bindgen(inline_js = "export function positions(c, d) {
  const n = c.length / d, a = new Array(n);
  for (let i = 0, j = 0; i < n; i++, j += d) a[i] = d === 3 ? [c[j], c[j + 1], c[j + 2]] : [c[j], c[j + 1]];
  return a;
}")]
extern "C" {
    /// The positions of the coordinates `c`, `d` numbers each, as arrays: made in JavaScript, a
    /// call for a list of coordinates rather than for each number.
    fn positions(c: &[f64], d: u32) -> JsValue;
}

/// Positions as arrays: `[x, y]`, or `[x, y, z]` (in a list where some coordinates have no
/// height, those get `NaN`). `buf` holds their numbers meanwhile.
fn list<'a>(buf: &mut Vec<f64>, cs: impl Iterator<Item = &'a Coord> + Clone) -> JsValue {
    let d = if cs.clone().any(|c| c.z.is_some()) { 3 } else { 2 };
    buf.clear();
    for c in cs {
        buf.extend([c.x, c.y]);
        if d == 3 {
            buf.push(c.z.unwrap_or(f64::NAN));
        }
    }
    positions(buf, d)
}

/// A ring, closed: its first position repeated at its end.
fn ring(buf: &mut Vec<f64>, r: &Ring) -> JsValue {
    list(buf, r.0.iter().chain(r.0.first()))
}

fn polygon(buf: &mut Vec<f64>, exterior: &Ring, interiors: &[Ring]) -> JsValue {
    std::iter::once(exterior).chain(interiors).map(|r| ring(buf, r)).collect::<js_sys::Array>().into()
}

/// A GeoJSON geometry object.
fn geometry(buf: &mut Vec<f64>, g: &Geometry) -> JsValue {
    let array = |items: js_sys::Array| -> JsValue { items.into() };
    let (kind, key, value) = match g {
        Geometry::Point(c) => ("Point", "coordinates", list(buf, std::iter::once(c)).unchecked_into::<js_sys::Array>().get(0)),
        Geometry::LineString(cs) => ("LineString", "coordinates", list(buf, cs.iter())),
        Geometry::Polygon { exterior, interiors } => ("Polygon", "coordinates", polygon(buf, exterior, interiors)),
        Geometry::MultiPoint(cs) => ("MultiPoint", "coordinates", list(buf, cs.iter())),
        Geometry::MultiLineString(ls) => ("MultiLineString", "coordinates", array(ls.iter().map(|l| list(buf, l.iter())).collect())),
        Geometry::MultiPolygon(ps) => ("MultiPolygon", "coordinates", array(ps.iter().map(|(e, hs)| polygon(buf, e, hs)).collect())),
        Geometry::GeometryCollection(gs) => ("GeometryCollection", "geometries", array(gs.iter().map(|g| geometry(buf, g)).collect())),
    };
    let o = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&o, &"type".into(), &kind.into());
    let _ = js_sys::Reflect::set(&o, &key.into(), &value);
    o.into()
}
