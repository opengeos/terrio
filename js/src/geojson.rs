//! A vector layer as a GeoJSON FeatureCollection object. The features and their properties are
//! written as JSON text a chunk at a time, each chunk given to `JSON.parse`: no text as long as
//! the whole layer's is held, in wasm memory (4 GB at most) or as a JavaScript string (about
//! 512 MB at most in V8). Geometries are made as arrays directly, not through text: a chunk's
//! coordinates and their structure are handed to JavaScript in one call.

use serde_json::ser::{CompactFormatter, Formatter};
use wasm_bindgen::prelude::*;
use wbvector::{Coord, Feature, FieldValue, Geometry, Layer, Ring};

/// The length of the text parsed at once, and the bytes of coordinates given to JavaScript at once.
const CHUNK: usize = 8 << 20;

/// `{ type: "FeatureCollection", features }`, as `wbvector::geojson::to_string` writes it, with
/// strings escaped as JSON requires and property numbers that are not finite written as `null`.
pub(crate) fn feature_collection(layer: &Layer) -> Result<JsValue, JsError> {
    let keys: Vec<Vec<u8>> = layer.schema.fields().iter().map(|f| string(&f.name)).collect();
    let chunks = js_sys::Array::new();
    let mut s = Vec::with_capacity(CHUNK + (CHUNK >> 4));
    let mut g = Geometries::default();
    for (n, f) in layer.features.iter().enumerate() {
        s.push(if s.is_empty() { b'[' } else { b',' });
        feature(&mut s, f, &keys);
        match &f.geometry {
            Some(geometry) => g.geometry(geometry),
            None => g.shape.push(0),
        }
        if s.len() >= CHUNK || g.coords.len() >= CHUNK / 8 || n + 1 == layer.features.len() {
            chunks.push(&parse(&mut s, &mut g)?);
        }
    }
    let fc = js_sys::Object::new();
    js_sys::Reflect::set(&fc, &"type".into(), &"FeatureCollection".into()).map_err(|_| JsError::new("GeoJSON"))?;
    js_sys::Reflect::set(&fc, &"features".into(), &chunks.flat(1)).map_err(|_| JsError::new("GeoJSON"))?;
    Ok(fc.into())
}

/// The features written so far, as an array, with their geometries `g`; `s` and `g` are emptied.
fn parse(s: &mut Vec<u8>, g: &mut Geometries) -> Result<JsValue, JsError> {
    s.push(b']');
    let text = std::str::from_utf8(s).map_err(|e| JsError::new(&e.to_string()))?;
    let parsed = js_sys::JSON::parse(text).map_err(|_| JsError::new("invalid GeoJSON"))?;
    s.clear();
    set_geometries(&parsed, &g.coords, &g.shape);
    g.coords.clear();
    g.shape.clear();
    Ok(parsed)
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

#[wasm_bindgen(inline_js = "export function setGeometries(features, c, s) {
  const T = [, 'Point', 'LineString', 'Polygon', 'MultiPoint', 'MultiLineString', 'MultiPolygon', 'GeometryCollection'];
  let i = 0, j = 0;
  const list = () => {
    const d = s[i++], n = s[i++], a = new Array(n);
    for (let k = 0; k < n; k++, j += d) a[k] = d === 3 ? [c[j], c[j + 1], c[j + 2]] : [c[j], c[j + 1]];
    return a;
  };
  const many = (f) => {
    const n = s[i++], a = new Array(n);
    for (let k = 0; k < n; k++) a[k] = f();
    return a;
  };
  const polygon = () => many(list);
  const geometry = () => {
    const t = s[i++];
    switch (t) {
      case 1: return { type: T[t], coordinates: list()[0] };
      case 2: case 4: return { type: T[t], coordinates: list() };
      case 3: case 5: return { type: T[t], coordinates: many(list) };
      case 6: return { type: T[t], coordinates: many(polygon) };
      default: return { type: T[t], geometries: many(geometry) };
    }
  };
  for (let f = 0; f < features.length; f++) {
    if (s[i] === 0) i++;
    else features[f].geometry = geometry();
  }
}")]
extern "C" {
    /// Sets the `geometry` of `features`, made in JavaScript from the coordinates `c` and the
    /// structure `s` that [`Geometries`] writes: one call for many features rather than calls
    /// for each list of coordinates.
    #[wasm_bindgen(js_name = setGeometries)]
    fn set_geometries(features: &JsValue, c: &[f64], s: &[u32]);
}

/// Geometries written for [`set_geometries`]: their coordinates' numbers, and their structure.
///
/// For each feature, `shape` holds 0 (no geometry) or a geometry: its type (1 Point, 2
/// LineString, 3 Polygon, 4 MultiPoint, 5 MultiLineString, 6 MultiPolygon, 7
/// GeometryCollection), then a list of positions for 1, 2 and 4, a count of lists for 3 and 5, a
/// count of polygons (each a count of lists) for 6, and a count of geometries for 7. A list is
/// its dimension (2 or 3) and its number of positions, their numbers in `coords`.
#[derive(Default)]
struct Geometries {
    coords: Vec<f64>,
    shape: Vec<u32>,
}

impl Geometries {
    /// Positions: `[x, y]`, or `[x, y, z]` (in a list where some coordinates have no height,
    /// those get `NaN`).
    fn list<'a>(&mut self, cs: impl Iterator<Item = &'a Coord> + Clone) {
        let d = if cs.clone().any(|c| c.z.is_some()) { 3 } else { 2 };
        let start = self.shape.len();
        self.shape.extend([d, 0]);
        let mut n = 0;
        for c in cs {
            self.coords.extend([c.x, c.y]);
            if d == 3 {
                self.coords.push(c.z.unwrap_or(f64::NAN));
            }
            n += 1;
        }
        self.shape[start + 1] = n;
    }

    /// A ring, closed: its first position repeated at its end, unless it already ends there (as
    /// `arrow.rs`'s `push_ring` does).
    fn ring(&mut self, r: &Ring) {
        let closed = r.0.len() > 1 && r.0.first().zip(r.0.last()).is_some_and(|(f, l)| f.x == l.x && f.y == l.y);
        self.list(r.0.iter().chain(r.0.first().filter(|_| !closed)));
    }

    fn polygon(&mut self, exterior: &Ring, interiors: &[Ring]) {
        self.shape.push(1 + interiors.len() as u32);
        self.ring(exterior);
        for r in interiors {
            self.ring(r);
        }
    }

    fn geometry(&mut self, g: &Geometry) {
        match g {
            Geometry::Point(c) => {
                self.shape.push(1);
                self.list(std::iter::once(c));
            }
            Geometry::LineString(cs) => {
                self.shape.push(2);
                self.list(cs.iter());
            }
            Geometry::Polygon { exterior, interiors } => {
                self.shape.push(3);
                self.polygon(exterior, interiors);
            }
            Geometry::MultiPoint(cs) => {
                self.shape.push(4);
                self.list(cs.iter());
            }
            Geometry::MultiLineString(ls) => {
                self.shape.extend([5, ls.len() as u32]);
                for l in ls {
                    self.list(l.iter());
                }
            }
            Geometry::MultiPolygon(ps) => {
                self.shape.extend([6, ps.len() as u32]);
                for (e, hs) in ps {
                    self.polygon(e, hs);
                }
            }
            Geometry::GeometryCollection(gs) => {
                self.shape.extend([7, gs.len() as u32]);
                for g in gs {
                    self.geometry(g);
                }
            }
        }
    }
}
