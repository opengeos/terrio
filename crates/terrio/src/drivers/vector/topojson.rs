// Written to match the behavior of:
//   GDAL v3.13.3:
//     ogr/ogrsf_frmts/geojson/ogrtopojsonreader.cpp
// No code is translated from these sources.

//! TopoJSON, read as GDAL reads it and written without shared arcs.
//!
//! Each GeometryCollection object is a layer of its geometries; the other objects form a layer
//! named `TopoJSON`, as in GDAL, with the object's key as its `id`. Fields are `id` (a string)
//! and the properties, typed from their JSON values (objects and arrays become JSON text). The
//! CRS is the one a GeoJSON 2008 `crs` member of the topology names, as GDAL reads it; without
//! one, EPSG:4326 when every coordinate is a longitude/latitude, and unknown otherwise.
//!
//! Writing names the layers' CRS in a `crs` member (TopoJSON has no default CRS, and GDAL, which
//! has no TopoJSON writer, reads the member): `urn:ogc:def:crs:OGC:1.3:CRS84` for EPSG:4326,
//! `urn:ogc:def:crs:EPSG::<code>` for the others.

use crate::crs::SpatialRef;
use crate::{Error, Result};
use serde_json::{Map, Value, json};
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, Layer, Ring};

/// Reads a TopoJSON file.
pub fn read(path: &str) -> Result<Vec<Layer>> {
    parse(&std::fs::read(path)?)
}

struct Topology {
    arcs: Vec<Vec<Coord>>,
    transform: Option<([f64; 2], [f64; 2])>,
}

impl Topology {
    fn position(&self, v: &Value) -> Option<Coord> {
        let a = v.as_array()?;
        let (x, y) = (a.first()?.as_f64()?, a.get(1)?.as_f64()?);
        let (x, y) = match self.transform {
            Some((s, t)) => (x * s[0] + t[0], y * s[1] + t[1]),
            None => (x, y),
        };
        Some(Coord { x, y, z: a.get(2).and_then(Value::as_f64), m: None })
    }

    /// The coordinates of a line made of arcs (a negative index `~i` is arc `i` reversed).
    fn line(&self, arcs: &Value) -> Vec<Coord> {
        let mut out: Vec<Coord> = Vec::new();
        for i in arcs.as_array().into_iter().flatten().filter_map(Value::as_i64) {
            let (idx, reversed) = if i < 0 { ((!i) as usize, true) } else { (i as usize, false) };
            let Some(arc) = self.arcs.get(idx) else { continue };
            let mut pts = arc.clone();
            if reversed {
                pts.reverse();
            }
            // Consecutive arcs share their end and start points.
            let skip = usize::from(!out.is_empty());
            out.extend(pts.into_iter().skip(skip));
        }
        out
    }

    fn ring(&self, arcs: &Value) -> Ring {
        let mut cs = self.line(arcs);
        if cs.len() > 1 && cs.first().map(|c| (c.x, c.y)) == cs.last().map(|c| (c.x, c.y)) {
            cs.pop();
        }
        Ring::new(cs)
    }

    /// A polygon's rings, without degenerate rings of fewer than three points (dropped by GDAL).
    fn polygon(&self, rings: &Value) -> Option<(Ring, Vec<Ring>)> {
        let mut rs = rings.as_array()?.iter().map(|r| self.ring(r)).filter(|r| r.len() >= 3);
        let exterior = rs.next()?;
        Some((exterior, rs.collect()))
    }

    fn geometry(&self, g: &Value) -> Option<Geometry> {
        let list = |k: &str| g[k].as_array().cloned().unwrap_or_default();
        Some(match g["type"].as_str()? {
            "Point" => Geometry::Point(self.position(&g["coordinates"])?),
            "MultiPoint" => Geometry::MultiPoint(list("coordinates").iter().filter_map(|p| self.position(p)).collect()),
            "LineString" => Geometry::LineString(self.line(&g["arcs"])),
            "MultiLineString" => Geometry::MultiLineString(list("arcs").iter().map(|a| self.line(a)).collect()),
            "Polygon" => {
                let (exterior, interiors) = self.polygon(&g["arcs"])?;
                Geometry::Polygon { exterior, interiors }
            }
            "MultiPolygon" => Geometry::MultiPolygon(list("arcs").iter().filter_map(|p| self.polygon(p)).collect()),
            "GeometryCollection" => Geometry::GeometryCollection(list("geometries").iter().filter_map(|p| self.geometry(p)).collect()),
            _ => return None,
        })
    }
}

/// A geometry object, with the key it has when it is a top-level object.
type Member<'a> = (Option<String>, &'a Value);

/// Parses TopoJSON text.
pub fn parse(bytes: &[u8]) -> Result<Vec<Layer>> {
    let doc: Value = serde_json::from_slice(bytes).map_err(|e| Error::InvalidData(format!("TopoJSON: {e}")))?;
    if doc["type"] != "Topology" {
        return Err(Error::InvalidData("TopoJSON: not a Topology".into()));
    }
    let transform = doc["transform"].as_object().and_then(|t| {
        let pair = |k: &str| -> Option<[f64; 2]> { Some([t.get(k)?.get(0)?.as_f64()?, t.get(k)?.get(1)?.as_f64()?]) };
        Some((pair("scale")?, pair("translate")?))
    });
    let crs = doc["crs"]["properties"]["name"].as_str().and_then(|n| SpatialRef::parse(n).ok());
    let mut topo = Topology { arcs: Vec::new(), transform };
    // Quantized arcs are delta-encoded.
    for arc in doc["arcs"].as_array().into_iter().flatten() {
        let mut pts = Vec::new();
        let (mut x, mut y) = (0.0, 0.0);
        for p in arc.as_array().into_iter().flatten() {
            let (Some(dx), Some(dy)) = (p.get(0).and_then(Value::as_f64), p.get(1).and_then(Value::as_f64)) else { continue };
            let z = p.get(2).and_then(Value::as_f64);
            pts.push(match transform {
                Some((s, t)) => {
                    x += dx;
                    y += dy;
                    Coord { x: x * s[0] + t[0], y: y * s[1] + t[1], z, m: None }
                }
                None => Coord { x: dx, y: dy, z, m: None },
            });
        }
        topo.arcs.push(pts);
    }

    let mut groups: Vec<(String, Vec<Member>)> = Vec::new();
    let mut loose: Vec<Member> = Vec::new();
    for (key, obj) in doc["objects"].as_object().into_iter().flatten() {
        if obj["type"] == "GeometryCollection" {
            let members = obj["geometries"].as_array().into_iter().flatten().map(|g| (None, g)).collect();
            groups.push((key.clone(), members));
        } else {
            loose.push((Some(key.clone()), obj));
        }
    }
    if !loose.is_empty() {
        groups.push(("TopoJSON".into(), loose));
    }

    let mut layers = Vec::new();
    for (name, members) in groups {
        let mut layer = Layer::new(&name);
        layer.add_field(FieldDef::new("id", FieldType::Text));
        let mut keys: Vec<(String, Option<FieldType>)> = Vec::new();
        for (_, g) in &members {
            for (k, v) in g["properties"].as_object().into_iter().flatten() {
                let t = json_type(v);
                match keys.iter_mut().find(|(n, _)| n == k) {
                    Some((_, prev)) => *prev = widen(*prev, t),
                    None => keys.push((k.clone(), t)),
                }
            }
        }
        for (k, t) in &keys {
            if layer.schema.field_index(k).is_none() {
                layer.add_field(FieldDef::new(k, t.unwrap_or(FieldType::Text)));
            }
        }
        let mut lonlat = true;
        for (fid, (key, g)) in members.iter().enumerate() {
            let mut attrs = vec![FieldValue::Null; layer.schema.len()];
            let id = match &g["id"] {
                Value::String(s) => Some(s.clone()),
                Value::Null => key.clone(),
                v => Some(v.to_string()),
            };
            attrs[0] = id.map_or(FieldValue::Null, FieldValue::Text);
            let props: &Map<String, Value> = match g["properties"].as_object() {
                Some(p) => p,
                None => &Map::new(),
            };
            for (k, v) in props {
                if let Some(i) = layer.schema.field_index(k).filter(|&i| i > 0) {
                    attrs[i] = value(v, layer.schema.fields()[i].field_type);
                }
            }
            let geometry = topo.geometry(g);
            if let Some(b) = geometry.as_ref().and_then(|g| g.bbox()) {
                lonlat &= b.min_x >= -180.0 && b.max_x <= 180.0 && b.min_y >= -90.0 && b.max_y <= 90.0;
            }
            layer.push(Feature { fid: fid as u64, geometry, attributes: attrs });
        }
        if let Some(crs) = &crs {
            crs.apply_to_layer(&mut layer);
        } else if lonlat && layer.features.iter().any(|f| f.geometry.is_some()) {
            layer.set_crs_epsg(Some(4326));
        }
        layers.push(layer);
    }
    Ok(layers)
}

fn json_type(v: &Value) -> Option<FieldType> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(_) => FieldType::Boolean,
        Value::Number(n) if n.is_i64() || n.is_u64() => FieldType::Integer,
        Value::Number(_) => FieldType::Float,
        Value::String(_) => FieldType::Text,
        _ => FieldType::Json,
    })
}

/// Merges the type of a field with the type of another of its values (`None` for nulls).
fn widen(a: Option<FieldType>, b: Option<FieldType>) -> Option<FieldType> {
    match (a, b) {
        (a, None) => a,
        (None, b) => b,
        (Some(a), Some(b)) if a == b => Some(a),
        (Some(FieldType::Integer), Some(FieldType::Float)) | (Some(FieldType::Float), Some(FieldType::Integer)) => Some(FieldType::Float),
        _ => Some(FieldType::Text),
    }
}

fn value(v: &Value, t: FieldType) -> FieldValue {
    match (t, v) {
        (_, Value::Null) => FieldValue::Null,
        (FieldType::Integer, v) => v.as_i64().map_or(FieldValue::Null, FieldValue::Integer),
        (FieldType::Float, v) => v.as_f64().map_or(FieldValue::Null, FieldValue::Float),
        (FieldType::Boolean, Value::Bool(b)) => FieldValue::Boolean(*b),
        (_, Value::String(s)) => FieldValue::Text(s.clone()),
        (_, v) => FieldValue::Text(v.to_string()),
    }
}

/// Writes a layer as TopoJSON: one GeometryCollection object named after the layer, each line and
/// ring an arc of its own (no shared topology, no quantization), in two dimensions.
pub fn write(layer: &Layer, path: &str) -> Result<()> {
    write_layers(std::slice::from_ref(layer), path)
}

/// Writes layers as TopoJSON, one GeometryCollection object per layer.
pub fn write_layers(layers: &[Layer], path: &str) -> Result<()> {
    std::fs::write(path, to_string(layers)?)?;
    Ok(())
}

/// Encodes layers as a TopoJSON topology.
pub fn to_string(layers: &[Layer]) -> Result<String> {
    let mut arcs: Vec<Value> = Vec::new();
    let mut add = |cs: &[Coord], close: bool| -> Value {
        let mut pts: Vec<Value> = cs.iter().map(position).collect();
        if close && cs.len() > 1 && cs.first().map(|c| (c.x, c.y)) != cs.last().map(|c| (c.x, c.y)) {
            pts.push(position(&cs[0]));
        }
        arcs.push(Value::Array(pts));
        json!(arcs.len() - 1)
    };
    let mut objects = Map::new();
    for layer in layers {
        let mut geometries = Vec::new();
        for f in &layer.features {
            let mut g = match &f.geometry {
                Some(g) => encode(g, &mut add),
                None => json!({"type": null}),
            };
            let mut props = Map::new();
            for (d, v) in layer.schema.fields().iter().zip(&f.attributes) {
                if d.name == "id" {
                    if !v.is_null() {
                        g["id"] = field_json(v, d.field_type);
                    }
                    continue;
                }
                props.insert(d.name.clone(), field_json(v, d.field_type));
            }
            g["properties"] = Value::Object(props);
            geometries.push(g);
        }
        objects.insert(layer.name.clone(), json!({"type": "GeometryCollection", "geometries": geometries}));
    }
    let mut doc = json!({"type": "Topology", "objects": objects, "arcs": arcs});
    if let Some(name) = crs_name(layers) {
        doc["crs"] = json!({"type": "name", "properties": {"name": name}});
    }
    Ok(doc.to_string())
}

/// The name of the layers' CRS for a `crs` member: their EPSG code (when every layer with a CRS
/// has the same one), as GDAL's GeoJSON writer names it, but EPSG:4326 too.
fn crs_name(layers: &[Layer]) -> Option<String> {
    let mut codes = layers.iter().filter(|l| !SpatialRef::from_layer(l).is_unknown()).map(super::naming_epsg);
    let first = codes.next()??;
    if !codes.all(|c| c == Some(first)) {
        return None;
    }
    Some(if first == 4326 { "urn:ogc:def:crs:OGC:1.3:CRS84".to_string() } else { format!("urn:ogc:def:crs:EPSG::{first}") })
}

/// A position: x and y only, as TopoJSON readers (GDAL's among them) expect.
fn position(c: &Coord) -> Value {
    json!([c.x, c.y])
}

fn field_json(v: &FieldValue, t: FieldType) -> Value {
    match v {
        FieldValue::Null => Value::Null,
        FieldValue::Integer(x) => json!(x),
        FieldValue::Float(x) => json!(x),
        FieldValue::Boolean(b) => json!(b),
        FieldValue::Text(s) if t == FieldType::Json => serde_json::from_str(s).unwrap_or_else(|_| json!(s)),
        FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => json!(s),
        FieldValue::Blob(b) => json!(b.iter().map(|x| format!("{x:02X}")).collect::<String>()),
    }
}

fn encode(g: &Geometry, add: &mut impl FnMut(&[Coord], bool) -> Value) -> Value {
    let polygon = |e: &Ring, hs: &[Ring], add: &mut dyn FnMut(&[Coord], bool) -> Value| -> Value {
        Value::Array(std::iter::once(e).chain(hs).map(|r| json!([add(&r.0, true)])).collect())
    };
    match g {
        Geometry::Point(c) => json!({"type": "Point", "coordinates": position(c)}),
        Geometry::MultiPoint(cs) => json!({"type": "MultiPoint", "coordinates": cs.iter().map(position).collect::<Vec<_>>()}),
        Geometry::LineString(cs) => json!({"type": "LineString", "arcs": [add(cs, false)]}),
        Geometry::MultiLineString(ls) => {
            json!({"type": "MultiLineString", "arcs": ls.iter().map(|l| json!([add(l, false)])).collect::<Vec<_>>()})
        }
        Geometry::Polygon { exterior, interiors } => json!({"type": "Polygon", "arcs": polygon(exterior, interiors, add)}),
        Geometry::MultiPolygon(ps) => {
            json!({"type": "MultiPolygon", "arcs": ps.iter().map(|(e, h)| polygon(e, h, add)).collect::<Vec<_>>()})
        }
        Geometry::GeometryCollection(gs) => {
            json!({"type": "GeometryCollection", "geometries": gs.iter().map(|g| encode(g, add)).collect::<Vec<_>>()})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_quantized_topology() {
        let t = br#"{"type":"Topology","transform":{"scale":[0.5,0.5],"translate":[10,20]},
            "objects":{"a":{"type":"GeometryCollection","geometries":[
              {"type":"Polygon","id":"x","arcs":[[0,1]],"properties":{"n":1,"f":true}},
              {"type":"LineString","arcs":[-2],"properties":{"n":2.5}}]},
             "b":{"type":"Point","coordinates":[2,2]}},
            "arcs":[[[0,0],[2,0],[0,2]],[[2,2],[-2,0],[0,-2]]]}"#;
        let layers = parse(t).unwrap();
        assert_eq!(layers.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["a", "TopoJSON"]);
        let a = &layers[0];
        assert_eq!(
            a.schema.fields().iter().map(|f| (f.name.as_str(), f.field_type)).collect::<Vec<_>>(),
            [("id", FieldType::Text), ("n", FieldType::Float), ("f", FieldType::Boolean)]
        );
        let Some(Geometry::Polygon { exterior, .. }) = &a.features[0].geometry else { panic!("no polygon") };
        let xy: Vec<(f64, f64)> = exterior.0.iter().map(|c| (c.x, c.y)).collect();
        assert_eq!(xy, [(10.0, 20.0), (11.0, 20.0), (11.0, 21.0), (10.0, 21.0)]);
        let Some(Geometry::LineString(l)) = &a.features[1].geometry else { panic!("no line") };
        assert_eq!((l[0].x, l[0].y), (10.0, 20.0));
        assert_eq!(layers[1].features[0].attributes[0], FieldValue::Text("b".into()));
        assert_eq!(a.crs_epsg(), Some(4326));

        let text = to_string(std::slice::from_ref(a)).unwrap();
        // GDAL reads EPSG:4326 from the crs member (it assumes no CRS without one).
        assert!(text.contains(r#""crs":{"type":"name","properties":{"name":"urn:ogc:def:crs:OGC:1.3:CRS84"}}"#), "{text}");
        let back = parse(text.as_bytes()).unwrap();
        assert_eq!(back[0].features[0].geometry, a.features[0].geometry);
        assert_eq!(back[0].features[0].attributes, a.features[0].attributes);
        assert_eq!(back[0].crs_epsg(), Some(4326));
    }

    #[test]
    fn names_a_projected_crs() {
        let mut l = Layer::new("utm");
        l.add_field(FieldDef::new("id", FieldType::Text));
        l.push(Feature { fid: 0, geometry: Some(Geometry::point(500000.0, 4e6)), attributes: vec![FieldValue::Null] });
        l.set_crs_epsg(Some(32633));
        let text = to_string(std::slice::from_ref(&l)).unwrap();
        assert!(text.contains("urn:ogc:def:crs:EPSG::32633"), "{text}");
        assert_eq!(parse(text.as_bytes()).unwrap()[0].crs_epsg(), Some(32633));
        // Without a CRS, no member, and coordinates beyond longitudes and latitudes have none.
        l.set_crs_epsg(None);
        let text = to_string(std::slice::from_ref(&l)).unwrap();
        assert!(!text.contains("\"crs\""), "{text}");
        assert_eq!(parse(text.as_bytes()).unwrap()[0].crs_epsg(), None);
    }
}
