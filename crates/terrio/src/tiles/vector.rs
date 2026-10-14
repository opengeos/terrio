//! Vector tiles as vector layers, and vector layers as vector tiles, as GDAL's MVT, MBTiles and
//! PMTiles drivers read and write them.

use super::clip::{self, Pt, Rect};
use super::mvt::{self, MvtFeature, MvtLayer, MvtValue, TileGeom};
use super::{MAX_GM, TileArchive, TileOpenOptions, TileWriteOptions};
use crate::{Error, Result};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, HashMap};
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer, Ring};

/// The size of a tile at zoom `z`, in metres.
fn tile_dim(z: u8) -> f64 {
    2.0 * MAX_GM / (1u64 << z) as f64
}

/// A layer's description from the archive's metadata.
struct LayerDef {
    name: String,
    fields: Vec<FieldDef>,
    /// The geometry type the tile statistics give (multi-geometries, as GDAL reads them).
    geom_type: Option<GeometryType>,
}

/// The layers the metadata declares (`vector_layers`, refined by `tilestats`), with their
/// fields typed as GDAL types them: `Number` as a Real, or an Integer when the statistics' bounds
/// are integers and no value has a fraction; `Boolean`; `String` otherwise. Every layer starts
/// with the `mvt_id` field.
fn layer_defs(meta: &Json) -> Vec<LayerDef> {
    let stats: Vec<&Json> = meta.pointer("/tilestats/layers").and_then(Json::as_array).map(|a| a.iter().collect()).unwrap_or_default();
    let Some(layers) = meta.get("vector_layers").and_then(Json::as_array) else { return Vec::new() };
    let mut out = Vec::new();
    for l in layers {
        let Some(name) = l.get("id").and_then(Json::as_str) else { continue };
        let stat = stats.iter().find(|s| s.get("layer").and_then(Json::as_str) == Some(name));
        let attrs: Vec<&Json> =
            stat.and_then(|s| s.get("attributes")).and_then(Json::as_array).map(|a| a.iter().collect()).unwrap_or_default();
        let mut fields = vec![FieldDef::new("mvt_id", FieldType::Integer)];
        if let Some(f) = l.get("fields").and_then(Json::as_object) {
            for (k, v) in f {
                let t = match v.as_str() {
                    Some("Number") => {
                        let a = attrs.iter().find(|a| {
                            a.get("attribute").and_then(Json::as_str) == Some(k) && a.get("type").and_then(Json::as_str) == Some("number")
                        });
                        let int = a.is_some_and(|a| {
                            let is_int = |x: Option<&Json>| x.is_some_and(|x| x.is_i64() || x.is_u64());
                            is_int(a.get("min"))
                                && is_int(a.get("max"))
                                && !a.get("values").and_then(Json::as_array).is_some_and(|v| v.iter().any(|x| x.is_f64()))
                        });
                        if int { FieldType::Integer } else { FieldType::Float }
                    }
                    Some("Integer") => FieldType::Integer,
                    Some("Boolean") => FieldType::Boolean,
                    _ => FieldType::Text,
                };
                fields.push(FieldDef::new(k.clone(), t));
            }
        }
        let geom_type = stat.and_then(|s| s.get("geometry")).and_then(Json::as_str).and_then(|g| match g {
            "Point" => Some(GeometryType::MultiPoint),
            "LineString" => Some(GeometryType::MultiLineString),
            "Polygon" => Some(GeometryType::MultiPolygon),
            _ => None,
        });
        out.push(LayerDef { name: name.to_string(), fields, geom_type });
    }
    out
}

/// The tile `(x, y)` at zoom `z` and the extent of its integer grid, for converting coordinates.
#[derive(Clone, Copy)]
pub(crate) struct TileFrame {
    pub top_x: f64,
    pub top_y: f64,
    pub dim: f64,
}

impl TileFrame {
    pub(crate) fn new(z: u8, x: u32, y: u32) -> Self {
        let dim = tile_dim(z);
        Self { top_x: -MAX_GM + x as f64 * dim, top_y: MAX_GM - y as f64 * dim, dim }
    }

    fn rect(&self) -> Rect {
        (self.top_x, self.top_y - self.dim, self.top_x + self.dim, self.top_y)
    }
}

/// Converts a decoded geometry to world coordinates (or keeps tile coordinates without a
/// frame), clips it to the tile when asked, and returns it, promoted to a multi-geometry when
/// `multi`. `None` when clipping leaves nothing.
fn to_geometry(g: &TileGeom, extent: u32, frame: Option<TileFrame>, clip: bool, multi: bool) -> Option<Geometry> {
    let conv = |p: &(i32, i32)| -> Pt {
        match frame {
            Some(f) => (f.top_x + p.0 as f64 * f.dim / extent as f64, f.top_y - p.1 as f64 * f.dim / extent as f64),
            None => (p.0 as f64, extent as f64 - p.1 as f64),
        }
    };
    let rect = match (frame, clip) {
        (Some(f), true) => Some(f.rect()),
        _ => None,
    };
    let c = |p: &Pt| Coord::xy(p.0, p.1);
    match g {
        TileGeom::Points(pts) => {
            let pts: Vec<Pt> =
                pts.iter().map(conv).filter(|p| rect.is_none_or(|r| p.0 >= r.0 && p.0 <= r.2 && p.1 >= r.1 && p.1 <= r.3)).collect();
            match pts.len() {
                0 => None,
                1 if !multi => Some(Geometry::Point(c(&pts[0]))),
                _ => Some(Geometry::MultiPoint(pts.iter().map(c).collect())),
            }
        }
        TileGeom::Lines(lines) => {
            let world: Vec<Vec<Pt>> = lines.iter().map(|l| l.iter().map(conv).collect()).collect();
            let lines: Vec<Vec<Pt>> = match rect {
                Some(r) => {
                    let all = clip::bbox(world.iter().flatten())?;
                    if clip::contains(r, all) {
                        world
                    } else if !clip::overlaps(r, all) {
                        return None;
                    } else {
                        world.iter().flat_map(|l| clip::clip_line(l, r)).collect()
                    }
                }
                None => world,
            };
            match lines.len() {
                0 => None,
                1 if !multi => Some(Geometry::LineString(lines[0].iter().map(c).collect())),
                _ => Some(Geometry::MultiLineString(lines.iter().map(|l| l.iter().map(c).collect()).collect())),
            }
        }
        TileGeom::Polygons(polys) => {
            let world: Vec<Vec<Vec<Pt>>> = polys.iter().map(|p| p.iter().map(|r| r.iter().map(conv).collect()).collect()).collect();
            let polys: Vec<Vec<Vec<Pt>>> = match rect {
                Some(r) => {
                    let all = clip::bbox(world.iter().flatten().flatten())?;
                    if clip::contains(r, all) {
                        world
                    } else if !clip::overlaps(r, all) {
                        return None;
                    } else {
                        world.iter().flat_map(|p| clip::clip_polygon(p, r)).collect()
                    }
                }
                None => world,
            };
            let ring = |r: &Vec<Pt>| {
                let mut v: Vec<Coord> = r.iter().map(c).collect();
                if v.len() > 1 && v.first() == v.last() {
                    v.pop();
                }
                Ring::new(v)
            };
            let parts: Vec<(Ring, Vec<Ring>)> = polys.iter().map(|p| (ring(&p[0]), p[1..].iter().map(ring).collect())).collect();
            match parts.len() {
                0 => None,
                1 if !multi => {
                    let (exterior, interiors) = parts.into_iter().next()?;
                    Some(Geometry::Polygon { exterior, interiors })
                }
                _ => Some(Geometry::MultiPolygon(parts)),
            }
        }
    }
}

fn value_to_field(v: &MvtValue, t: FieldType) -> FieldValue {
    let num = match v {
        MvtValue::Float(f) => Some(*f as f64),
        MvtValue::Double(f) => Some(*f),
        MvtValue::Int(i) | MvtValue::SInt(i) => Some(*i as f64),
        MvtValue::UInt(u) => Some(*u as f64),
        MvtValue::Bool(b) => Some(f64::from(u8::from(*b))),
        MvtValue::String(_) => None,
    };
    match (t, v) {
        (FieldType::Text, MvtValue::String(s)) => FieldValue::Text(s.clone()),
        (FieldType::Text, MvtValue::Bool(b)) => FieldValue::Text(if *b { "true" } else { "false" }.into()),
        (FieldType::Text, MvtValue::Int(i) | MvtValue::SInt(i)) => FieldValue::Text(i.to_string()),
        (FieldType::Text, MvtValue::UInt(u)) => FieldValue::Text(u.to_string()),
        (FieldType::Text, _) => FieldValue::Text(num.map(|n| n.to_string()).unwrap_or_default()),
        (FieldType::Integer, MvtValue::Int(i) | MvtValue::SInt(i)) => FieldValue::Integer(*i),
        (FieldType::Integer, MvtValue::UInt(u)) => FieldValue::Integer(*u as i64),
        (FieldType::Integer, MvtValue::String(s)) => s.trim().parse().map(FieldValue::Integer).unwrap_or(FieldValue::Null),
        (FieldType::Integer, _) => num.map(|n| FieldValue::Integer(n as i64)).unwrap_or(FieldValue::Null),
        (FieldType::Boolean, MvtValue::Bool(b)) => FieldValue::Boolean(*b),
        (FieldType::Boolean, _) => num.map(|n| FieldValue::Boolean(n != 0.0)).unwrap_or(FieldValue::Null),
        (_, MvtValue::String(s)) => s.trim().parse().map(FieldValue::Float).unwrap_or(FieldValue::Null),
        _ => num.map(FieldValue::Float).unwrap_or(FieldValue::Null),
    }
}

/// The field type of a value, for layers whose fields the metadata does not declare.
fn value_type(v: &MvtValue) -> FieldType {
    match v {
        MvtValue::String(_) => FieldType::Text,
        MvtValue::Float(_) | MvtValue::Double(_) => FieldType::Float,
        MvtValue::Int(_) | MvtValue::SInt(_) | MvtValue::UInt(_) => FieldType::Integer,
        MvtValue::Bool(_) => FieldType::Boolean,
    }
}

/// Layers being filled from tiles.
struct Builder {
    layers: Vec<Layer>,
    by_name: HashMap<String, usize>,
    /// Whether the fields are declared by the metadata (otherwise they are added as found).
    declared: bool,
    multi: Vec<bool>,
    clip: bool,
}

impl Builder {
    fn add_tile(&mut self, bytes: &[u8], frame: Option<TileFrame>, wanted: &[String]) -> Result<()> {
        for tl in mvt::decode(bytes)? {
            if !wanted.is_empty() && !wanted.contains(&tl.name) {
                continue;
            }
            let idx = match self.by_name.get(&tl.name) {
                Some(i) => *i,
                None if self.declared => continue,
                None => {
                    let mut l = Layer::new(tl.name.clone());
                    if frame.is_some() {
                        l = l.with_epsg(3857);
                    }
                    l.add_field(FieldDef::new("mvt_id", FieldType::Integer));
                    self.layers.push(l);
                    self.multi.push(false);
                    self.by_name.insert(tl.name.clone(), self.layers.len() - 1);
                    self.layers.len() - 1
                }
            };
            for f in &tl.features {
                if !self.declared {
                    for (k, v) in &f.tags {
                        let l = &mut self.layers[idx];
                        match l.schema.field_index(k) {
                            None => l.add_field(FieldDef::new(k.clone(), value_type(v))),
                            Some(i) => {
                                let t = l.schema.fields()[i].field_type;
                                let w = FieldValue::widen_type(t, value_type(v));
                                if w != t {
                                    widen_field(l, i, w);
                                }
                            }
                        }
                    }
                }
                let Some(tg) = mvt::decode_geometry(f.geom_type, &f.geometry) else { continue };
                let Some(geom) = to_geometry(&tg, tl.extent, frame, self.clip, self.multi[idx]) else { continue };
                let l = &mut self.layers[idx];
                let n = l.schema.len();
                let mut attrs = vec![FieldValue::Null; n];
                if let Some(id) = f.id {
                    attrs[0] = FieldValue::Integer(id as i64);
                }
                for (k, v) in &f.tags {
                    if let Some(i) = l.schema.field_index(k) {
                        attrs[i] = value_to_field(v, l.schema.fields()[i].field_type);
                    }
                }
                let fid = l.features.len() as u64;
                l.features.push(Feature { fid, geometry: Some(geom), attributes: attrs });
            }
        }
        Ok(())
    }
}

/// Retypes a field of a layer (values read so far are converted).
fn widen_field(l: &mut Layer, i: usize, t: FieldType) {
    let name = l.schema.fields()[i].name.clone();
    let mut schema = wbvector::Schema::new();
    for (j, f) in l.schema.fields().iter().enumerate() {
        schema.add_field(if j == i { FieldDef::new(name.clone(), t) } else { f.clone() });
    }
    l.schema = schema;
    for f in &mut l.features {
        if let Some(v) = f.attributes.get_mut(i) {
            *v = match (&*v, t) {
                (FieldValue::Integer(x), FieldType::Float) => FieldValue::Float(*x as f64),
                (FieldValue::Null, _) => FieldValue::Null,
                (x, FieldType::Text) => FieldValue::Text(match x {
                    FieldValue::Integer(n) => n.to_string(),
                    FieldValue::Float(n) => n.to_string(),
                    FieldValue::Boolean(b) => b.to_string(),
                    FieldValue::Text(s) => s.clone(),
                    _ => String::new(),
                }),
                (x, _) => x.clone(),
            };
        }
    }
}

/// Reads the vector layers of an archive: every feature of the tiles of one zoom level (the
/// highest by default), in EPSG:3857. `wanted` limits the layers; `filter` (in EPSG:3857) the
/// tiles read.
pub(crate) fn read_layers(
    archive: &mut TileArchive,
    options: &TileOpenOptions,
    wanted: &[String],
    filter: Option<(f64, f64, f64, f64)>,
) -> Result<Vec<Layer>> {
    let (min_z, max_z) = archive.zoom_range();
    let z = options.zoom_level.unwrap_or(max_z);
    if z < min_z || z > max_z {
        return Err(Error::InvalidArgument(format!("invalid zoom level {z}: the archive has zoom levels {min_z} to {max_z}")));
    }
    let meta = archive.json();
    let defs = layer_defs(&meta);
    if !wanted.is_empty() {
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        if let Some(w) = wanted.iter().find(|w| !defs.is_empty() && !names.contains(&w.as_str())) {
            return Err(Error::InvalidArgument(format!("no layer named {w} (layers: {})", names.join(", "))));
        }
    }
    let mut b = Builder {
        layers: Vec::new(),
        by_name: HashMap::new(),
        declared: !defs.is_empty(),
        multi: Vec::new(),
        clip: options.clip.unwrap_or(true),
    };
    for d in defs.into_iter().filter(|d| wanted.is_empty() || wanted.contains(&d.name)) {
        let mut l = Layer::new(d.name.clone()).with_epsg(3857);
        for f in d.fields {
            l.add_field(f);
        }
        b.multi.push(d.geom_type.is_some());
        l.geom_type = d.geom_type;
        b.by_name.insert(d.name, b.layers.len());
        b.layers.push(l);
    }
    let mut tiles = archive.tiles_at_zoom(z)?;
    if let Some(f) = filter {
        tiles.retain(|&(x, y)| clip::overlaps(TileFrame::new(z, x, y).rect(), f) || clip::contains(TileFrame::new(z, x, y).rect(), f));
    }
    for chunk in tiles.chunks(256) {
        let wanted_tiles: Vec<(u8, u32, u32)> = chunk.iter().map(|&(x, y)| (z, x, y)).collect();
        let data = archive.tiles(&wanted_tiles)?;
        for (&(_, x, y), bytes) in wanted_tiles.iter().zip(data) {
            if let Some(bytes) = bytes {
                b.add_tile(&bytes, Some(TileFrame::new(z, x, y)), wanted)?;
            }
        }
    }
    Ok(b.layers)
}

/// Reads a single MVT tile file. A path ending in `z/x/y.pbf` (or `.mvt`) is georeferenced in
/// EPSG:3857; other tiles keep their integer tile coordinates (y up), as GDAL reads them. Fields
/// come from a `metadata.json` next to the zoom level directories, when there is one, otherwise
/// from the values.
pub(crate) fn read_tile_file(bytes: &[u8], path: &str, options: &TileOpenOptions) -> Result<Vec<Layer>> {
    let frame = zxy_of_path(path).map(|(z, x, y)| TileFrame::new(z, x, y));
    let meta = metadata_json_near(path);
    let defs = meta.as_ref().map(layer_defs).unwrap_or_default();
    let mut b = Builder {
        layers: Vec::new(),
        by_name: HashMap::new(),
        declared: !defs.is_empty(),
        multi: Vec::new(),
        clip: options.clip.unwrap_or(true),
    };
    for d in defs {
        let mut l = Layer::new(d.name.clone()).with_epsg(3857);
        for f in d.fields {
            l.add_field(f);
        }
        b.multi.push(d.geom_type.is_some());
        l.geom_type = d.geom_type;
        b.by_name.insert(d.name, b.layers.len());
        b.layers.push(l);
    }
    b.add_tile(bytes, frame, &[])?;
    Ok(b.layers)
}

/// `(z, x, y)` from a path ending in `z/x/y.pbf`, `z/x/y.mvt` or `z/x/y.mvt.gz`.
pub(crate) fn zxy_of_path(path: &str) -> Option<(u8, u32, u32)> {
    let p = path.replace('\\', "/");
    let mut parts = p.rsplit('/');
    let file = parts.next()?;
    let y: u32 = file.split('.').next()?.parse().ok()?;
    let x: u32 = parts.next()?.parse().ok()?;
    let z: u8 = parts.next()?.parse().ok()?;
    (z <= 30 && x < (1u64 << z) as u32 && y < (1u64 << z) as u32).then_some((z, x, y))
}

/// The `json` of a `metadata.json` next to a tile's zoom level directory.
fn metadata_json_near(path: &str) -> Option<Json> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let p = std::path::Path::new(path);
        let dir = p.parent()?.parent()?.parent()?;
        let text = std::fs::read_to_string(dir.join("metadata.json")).ok()?;
        let v: Json = serde_json::from_str(&text).ok()?;
        match v.get("json").and_then(Json::as_str) {
            Some(s) => serde_json::from_str(s).ok(),
            None => Some(v),
        }
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = path;
        None
    }
}

// ─── Writing ────────────────────────────────────────────────────────────────────────────────────

/// A feature's attributes as MVT values, as GDAL's writer encodes them: integers as `uint` (or
/// `sint` when negative), reals as integers when they have no fraction, else as `float` when
/// exactly representable, else `double`; booleans as `bool`; everything else as strings.
fn encode_attributes(l: &Layer, f: &Feature) -> Vec<(String, MvtValue)> {
    let mut out = Vec::new();
    for (def, v) in l.schema.fields().iter().zip(&f.attributes) {
        let mv = match v {
            FieldValue::Null => continue,
            FieldValue::Integer(i) => int_value(*i),
            FieldValue::Boolean(b) => MvtValue::Bool(*b),
            FieldValue::Float(d) => double_value(*d),
            FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => MvtValue::String(s.clone()),
            FieldValue::Blob(b) => MvtValue::String(b.iter().map(|x| format!("{x:02X}")).collect()),
        };
        out.push((def.name.clone(), mv));
    }
    out
}

fn int_value(i: i64) -> MvtValue {
    if i < 0 { MvtValue::SInt(i) } else { MvtValue::UInt(i as u64) }
}

fn double_value(d: f64) -> MvtValue {
    if d >= 0.0 && d <= u64::MAX as f64 && d == (d as u64) as f64 {
        MvtValue::UInt(d as u64)
    } else if d < 0.0 && d >= i64::MIN as f64 && d == (d as i64) as f64 {
        MvtValue::SInt(d as i64)
    } else if !d.is_finite() || (d.abs() <= f32::MAX as f64 && d == (d as f32) as f64) {
        MvtValue::Float(d as f32)
    } else {
        MvtValue::Double(d)
    }
}

/// Quantizes a path to a tile's integer grid, dropping repeated points.
fn quantize(pts: &[Pt], f: &TileFrame, extent: u32) -> Vec<(i32, i32)> {
    let mut out: Vec<(i32, i32)> = Vec::with_capacity(pts.len());
    for p in pts {
        let q = (((p.0 - f.top_x) * extent as f64 / f.dim).round() as i32, ((f.top_y - p.1) * extent as f64 / f.dim).round() as i32);
        if out.last() != Some(&q) {
            out.push(q);
        }
    }
    out
}

/// The orientation of `c` relative to `a -> b`: positive left, negative right, zero collinear.
fn orient(a: (i32, i32), b: (i32, i32), c: (i32, i32)) -> i64 {
    (b.0 as i64 - a.0 as i64) * (c.1 as i64 - a.1 as i64) - (b.1 as i64 - a.1 as i64) * (c.0 as i64 - a.0 as i64)
}

/// Whether segments `p1-p2` and `q1-q2` intersect or touch.
fn segments_meet(p1: (i32, i32), p2: (i32, i32), q1: (i32, i32), q2: (i32, i32)) -> bool {
    let (d1, d2, d3, d4) = (orient(q1, q2, p1), orient(q1, q2, p2), orient(p1, p2, q1), orient(p1, p2, q2));
    if ((d1 > 0 && d2 < 0) || (d1 < 0 && d2 > 0)) && ((d3 > 0 && d4 < 0) || (d3 < 0 && d4 > 0)) {
        return true;
    }
    let on = |a: (i32, i32), b: (i32, i32), c: (i32, i32)| {
        c.0 >= a.0.min(b.0) && c.0 <= a.0.max(b.0) && c.1 >= a.1.min(b.1) && c.1 <= a.1.max(b.1)
    };
    (d1 == 0 && on(q1, q2, p1)) || (d2 == 0 && on(q1, q2, p2)) || (d3 == 0 && on(p1, p2, q1)) || (d4 == 0 && on(p1, p2, q2))
}

/// The first pair of non-adjacent edges `(i, j)` (`i < j`) of a closed ring (without its closing
/// point) that meet, found through a grid of the edges' boxes.
fn first_self_intersection(q: &[(i32, i32)]) -> Option<(usize, usize)> {
    let n = q.len();
    if n < 4 {
        return None;
    }
    let (mut x0, mut y0, mut x1, mut y1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for p in q {
        x0 = x0.min(p.0);
        y0 = y0.min(p.1);
        x1 = x1.max(p.0);
        y1 = y1.max(p.1);
    }
    let cells = ((n as f64).sqrt().ceil() as i64).clamp(1, 256);
    let cw = ((x1 as i64 - x0 as i64) / cells + 1).max(1);
    let ch = ((y1 as i64 - y0 as i64) / cells + 1).max(1);
    let mut grid: HashMap<(i64, i64), Vec<usize>> = HashMap::new();
    for i in 0..n {
        let (a, b) = (q[i], q[(i + 1) % n]);
        let (gx0, gx1) = ((a.0.min(b.0) as i64 - x0 as i64) / cw, (a.0.max(b.0) as i64 - x0 as i64) / cw);
        let (gy0, gy1) = ((a.1.min(b.1) as i64 - y0 as i64) / ch, (a.1.max(b.1) as i64 - y0 as i64) / ch);
        for gx in gx0..=gx1 {
            for gy in gy0..=gy1 {
                grid.entry((gx, gy)).or_default().push(i);
            }
        }
    }
    let mut best: Option<(usize, usize)> = None;
    for edges in grid.values() {
        for (k, &i) in edges.iter().enumerate() {
            for &j in &edges[k + 1..] {
                let (i, j) = (i.min(j), i.max(j));
                if j == i + 1 || (i == 0 && j == n - 1) {
                    continue;
                }
                if segments_meet(q[i], q[(i + 1) % n], q[j], q[(j + 1) % n]) && best.is_none_or(|b| (i, j) < b) {
                    best = Some((i, j));
                }
            }
        }
    }
    best
}

/// Removes the small loops that rounding to the tile grid creates in a ring (an edge crossing or
/// touching a non-adjacent one): the smaller side of each crossing is dropped, a few times at
/// most, as GDAL makes such polygons valid.
fn fix_ring(q: &mut Vec<(i32, i32)>) {
    for _ in 0..32 {
        let Some((i, j)) = first_self_intersection(q) else { return };
        let inner: Vec<(i32, i32)> = q[i + 1..=j].to_vec();
        let outer: Vec<(i32, i32)> = q[..=i].iter().chain(&q[j + 1..]).copied().collect();
        let area = |r: &[(i32, i32)]| {
            let mut c = r.to_vec();
            if let Some(&f) = r.first() {
                c.push(f);
            }
            mvt::ring_area2(&c).abs()
        };
        *q = if area(&inner) > area(&outer) { inner } else { outer };
        if q.len() < 3 {
            return;
        }
    }
}

/// Encodes a polygon's rings in a tile (exterior rings clockwise on screen, holes
/// counter-clockwise), dropping rings that collapse. Returns `false` when the exterior collapses.
fn encode_polygon(out: &mut Vec<u32>, rings: &[Vec<Pt>], f: &TileFrame, extent: u32, cursor: &mut (i32, i32)) -> bool {
    for (i, r) in rings.iter().enumerate() {
        let mut q = quantize(r, f, extent);
        while q.len() > 1 && q.first() == q.last() {
            q.pop();
        }
        fix_ring(&mut q);
        q.dedup();
        let mut closed = q.clone();
        if let Some(&first) = q.first() {
            closed.push(first);
        }
        if q.len() < 3 || mvt::ring_area2(&closed) == 0.0 {
            if i == 0 {
                return false;
            }
            continue;
        }
        // In tile coordinates (y down) a positive area is clockwise on screen: exterior rings.
        let a = mvt::ring_area2(&closed);
        if (i == 0) != (a > 0.0) {
            q.reverse();
        }
        mvt::encode_path(out, &q, true, cursor);
    }
    true
}

/// A feature's geometry, prepared for one tile: clipped to the buffered tile, simplified and
/// encoded. `None` when nothing is left.
fn encode_for_tile(g: &Geometry, f: &TileFrame, extent: u32, buffer: f64, tolerance: f64) -> Option<(u32, Vec<u32>)> {
    let r: Rect = (f.top_x - buffer, f.top_y - f.dim - buffer, f.top_x + f.dim + buffer, f.top_y + buffer);
    let xy = |c: &Coord| (c.x, c.y);
    let simplify = |pts: Vec<Pt>| if tolerance > 0.0 { clip::simplify(&pts, tolerance) } else { pts };
    match g {
        Geometry::Point(c) => {
            let p = xy(c);
            (p.0 >= r.0 && p.0 <= r.2 && p.1 >= r.1 && p.1 <= r.3).then(|| (mvt::POINT, mvt::encode_points(&quantize(&[p], f, extent))))
        }
        Geometry::MultiPoint(cs) => {
            let mut seen = std::collections::BTreeSet::new();
            let mut pts = Vec::new();
            for c in cs {
                let p = xy(c);
                if p.0 >= r.0 && p.0 <= r.2 && p.1 >= r.1 && p.1 <= r.3 {
                    let q = quantize(&[p], f, extent)[0];
                    if seen.insert(q) {
                        pts.push(q);
                    }
                }
            }
            (!pts.is_empty()).then(|| (mvt::POINT, mvt::encode_points(&pts)))
        }
        Geometry::LineString(_) | Geometry::MultiLineString(_) => {
            let lines: Vec<Vec<Pt>> = match g {
                Geometry::LineString(cs) => vec![cs.iter().map(xy).collect()],
                Geometry::MultiLineString(ls) => ls.iter().map(|l| l.iter().map(xy).collect()).collect(),
                _ => unreachable!(),
            };
            let all = clip::bbox(lines.iter().flatten())?;
            let lines: Vec<Vec<Pt>> =
                if clip::contains(r, all) { lines } else { lines.iter().flat_map(|l| clip::clip_line(l, r)).collect() };
            let mut out = Vec::new();
            let mut cursor = (0, 0);
            for l in lines {
                let q = quantize(&simplify(l), f, extent);
                if q.len() >= 2 {
                    mvt::encode_path(&mut out, &q, false, &mut cursor);
                }
            }
            (!out.is_empty()).then_some((mvt::LINESTRING, out))
        }
        Geometry::Polygon { .. } | Geometry::MultiPolygon(_) => {
            let polys: Vec<Vec<Vec<Pt>>> = match g {
                Geometry::Polygon { exterior, interiors } => {
                    vec![std::iter::once(exterior).chain(interiors).map(|r| r.0.iter().map(xy).collect()).collect()]
                }
                Geometry::MultiPolygon(ps) => {
                    ps.iter().map(|(e, h)| std::iter::once(e).chain(h).map(|r| r.0.iter().map(xy).collect()).collect()).collect()
                }
                _ => unreachable!(),
            };
            let all = clip::bbox(polys.iter().flatten().flatten())?;
            let inside = clip::contains(r, all);
            let close = |mut v: Vec<Pt>| {
                if v.len() > 1 && v.first() != v.last() {
                    let f = v[0];
                    v.push(f);
                }
                v
            };
            let mut out = Vec::new();
            let mut cursor = (0, 0);
            for p in polys {
                let parts: Vec<Vec<Vec<Pt>>> = if inside { vec![p.into_iter().map(close).collect()] } else { clip::clip_polygon(&p, r) };
                for rings in parts {
                    let rings: Vec<Vec<Pt>> = rings.into_iter().map(simplify).collect();
                    encode_polygon(&mut out, &rings, f, extent, &mut cursor);
                }
            }
            (!out.is_empty()).then_some((mvt::POLYGON, out))
        }
        Geometry::GeometryCollection(_) => None,
    }
}

/// Statistics of a layer, for the metadata.
#[derive(Default)]
struct LayerStats {
    count: usize,
    geom_counts: [usize; 4],
    /// Per field: count, and the distinct values (up to 1000) and numeric bounds.
    attrs: BTreeMap<usize, AttrStats>,
}

#[derive(Default)]
struct AttrStats {
    count: usize,
    values: Vec<Json>,
    min: Option<f64>,
    max: Option<f64>,
}

/// Vector layers cut into tiles: `(z, x, y, tile bytes)` (uncompressed MVT), and the metadata
/// (`vector_layers`, `tilestats`) as JSON.
pub(crate) struct VectorTiles {
    pub tiles: Vec<(u8, u32, u32, Vec<u8>)>,
    pub json: Json,
    /// The bounds `(min_lon, min_lat, max_lon, max_lat)` of the features.
    pub bounds: [f64; 4],
    pub min_zoom: u8,
    pub max_zoom: u8,
}

/// Cuts vector layers into MVT tiles from `min_zoom` to `max_zoom` (default 0 to 5): each
/// feature is reprojected to EPSG:3857, clipped to every tile it touches (with the buffer),
/// simplified when asked, quantized to the tile grid, and keeps its attributes.
pub(crate) fn make_tiles(layers: &[Layer], o: &TileWriteOptions) -> Result<VectorTiles> {
    let min_zoom = o.min_zoom.unwrap_or(0);
    let max_zoom = o.max_zoom.unwrap_or(5);
    if min_zoom > max_zoom {
        return Err(Error::InvalidArgument(format!("the minimum zoom level {min_zoom} is above the maximum {max_zoom}")));
    }
    let web = crate::crs::SpatialRef::from_epsg(3857)?;
    let mut tiles: BTreeMap<(u8, u32, u32), Vec<MvtLayer>> = BTreeMap::new();
    let mut env: Option<Rect> = None;
    let mut vector_layers = Vec::new();
    let mut stat_layers = Vec::new();
    for layer in layers {
        let l = match crate::crs::SpatialRef::from_layer(layer) {
            s if s.is_unknown() || s.epsg == Some(3857) => layer.clone(),
            _ => crate::vector::reproject_layer(layer, &web)?,
        };
        let mut stats = LayerStats::default();
        for f in &l.features {
            let Some(g) = &f.geometry else { continue };
            let parts: Vec<&Geometry> = match g {
                Geometry::GeometryCollection(gs) => gs.iter().collect(),
                g => vec![g],
            };
            let tags = encode_attributes(&l, f);
            let mut counted = false;
            for g in parts {
                let mut pts: Vec<Pt> = Vec::new();
                collect_points(g, &mut pts);
                let Some(bb) = clip::bbox(pts.iter()) else { continue };
                env = Some(match env {
                    Some(e) => (e.0.min(bb.0), e.1.min(bb.1), e.2.max(bb.2), e.3.max(bb.3)),
                    None => bb,
                });
                if !counted {
                    counted = true;
                    stats.count += 1;
                    for (i, v) in f.attributes.iter().enumerate() {
                        add_attr_stat(&mut stats, i, v);
                    }
                }
                for z in min_zoom..=max_zoom {
                    let dim = tile_dim(z);
                    let buffer = dim * o.buffer as f64 / o.extent as f64;
                    let n = (1u64 << z) as i64 - 1;
                    let tx0 = (((bb.0 + MAX_GM - buffer) / dim) as i64).max(0);
                    let ty0 = (((MAX_GM - bb.3 - buffer) / dim) as i64).max(0);
                    let tx1 = (((bb.2 + MAX_GM + buffer) / dim) as i64).min(n);
                    let ty1 = (((MAX_GM - bb.1 + buffer) / dim) as i64).min(n);
                    let simpl = if z == max_zoom { o.simplification_max_zoom.unwrap_or(o.simplification) } else { o.simplification };
                    let tol = dim / o.extent as f64 * simpl;
                    for tx in tx0..=tx1 {
                        for ty in ty0..=ty1 {
                            let frame = TileFrame::new(z, tx as u32, ty as u32);
                            let Some((geom_type, geometry)) = encode_for_tile(g, &frame, o.extent, buffer, tol) else { continue };
                            stats.geom_counts[geom_type as usize] += 1;
                            let layers = tiles.entry((z, tx as u32, ty as u32)).or_default();
                            let tl = match layers.iter_mut().position(|t| t.name == l.name) {
                                Some(i) => &mut layers[i],
                                None => {
                                    layers.push(MvtLayer { name: l.name.clone(), extent: o.extent, features: Vec::new() });
                                    layers.last_mut().expect("just pushed")
                                }
                            };
                            // Like `ogr2ogr` without `-preserve_fid`, GDAL's writer leaves feature ids out.
                            let size = mvt::decode_geometry(geom_type, &geometry).map_or(0.0, |g| mvt::geometry_size(&g));
                            tl.features.push(MvtFeature { id: None, geom_type, geometry, tags: tags.clone(), size });
                        }
                    }
                }
            }
        }
        let mut fields = serde_json::Map::new();
        for def in l.schema.fields() {
            let t = match def.field_type {
                FieldType::Integer | FieldType::Float => "Number",
                FieldType::Boolean => "Boolean",
                _ => "String",
            };
            fields.insert(def.name.clone(), json!(t));
        }
        vector_layers.push(json!({
            "id": l.name, "description": "", "minzoom": min_zoom, "maxzoom": max_zoom, "fields": fields,
        }));
        let geom = ["", "Point", "LineString", "Polygon"][(1..4).max_by_key(|&i| stats.geom_counts[i]).unwrap_or(3)];
        let attributes: Vec<Json> = stats
            .attrs
            .iter()
            .filter_map(|(&i, a)| {
                let def = l.schema.fields().get(i)?;
                let t = match def.field_type {
                    FieldType::Integer | FieldType::Float => "number",
                    FieldType::Boolean => "boolean",
                    _ => "string",
                };
                let mut o = json!({ "attribute": def.name, "count": a.count, "type": t, "values": a.values });
                if t == "number" {
                    let n = |v: f64| if v.fract() == 0.0 && v.abs() < 9e15 { json!(v as i64) } else { json!(v) };
                    if let (Some(mn), Some(mx)) = (a.min, a.max) {
                        o["min"] = n(mn);
                        o["max"] = n(mx);
                    }
                }
                Some(o)
            })
            .collect();
        stat_layers.push(json!({
            "layer": l.name, "count": stats.count, "geometry": geom,
            "attributeCount": attributes.len(), "attributes": attributes,
        }));
    }
    let (x0, y0, x1, y1) = env.ok_or_else(|| Error::InvalidArgument("no geometries to write as vector tiles".into()))?;
    let (lon0, lat0) = super::mercator_to_lonlat(x0, y0);
    let (lon1, lat1) = super::mercator_to_lonlat(x1, y1);
    let json = json!({
        "vector_layers": vector_layers,
        "tilestats": { "layerCount": stat_layers.len(), "layers": stat_layers },
    });
    let tiles = tiles.into_iter().map(|((z, x, y), layers)| (z, x, y, encode_limited(layers, o))).collect();
    Ok(VectorTiles { tiles, json, bounds: [lon0, lat0, lon1, lat1], min_zoom, max_zoom })
}

/// A tile's layers with their geometries at a lower resolution (`extent` instead of `from`), as
/// GDAL reduces them: coordinates scaled and truncated, repeated points and collapsed parts
/// dropped.
fn reduce(layers: &[MvtLayer], from: u32, extent: u32) -> Vec<MvtLayer> {
    let s = |p: &(i32, i32)| ((p.0 as i64 * extent as i64 / from as i64) as i32, (p.1 as i64 * extent as i64 / from as i64) as i32);
    layers
        .iter()
        .map(|l| MvtLayer {
            name: l.name.clone(),
            extent,
            features: l
                .features
                .iter()
                .filter_map(|f| {
                    let g = match mvt::decode_geometry(f.geom_type, &f.geometry)? {
                        TileGeom::Points(p) => TileGeom::Points(p.iter().map(s).collect()),
                        TileGeom::Lines(ls) => TileGeom::Lines(ls.iter().map(|l| l.iter().map(s).collect()).collect()),
                        TileGeom::Polygons(ps) => {
                            TileGeom::Polygons(ps.iter().map(|p| p.iter().map(|r| r.iter().map(s).collect()).collect()).collect())
                        }
                    };
                    let (geom_type, geometry) = mvt::encode_geometry(&g)?;
                    Some(MvtFeature { geom_type, geometry, ..f.clone() })
                })
                .collect(),
        })
        .collect()
}

/// Encodes a tile within GDAL's limits (`MAX_FEATURES` features, `MAX_SIZE` bytes once
/// compressed): a tile too large is encoded at lower resolutions (the extent halved down to 256),
/// then, if still too large, keeps its largest features (by area or length) that fit.
fn encode_limited(mut layers: Vec<MvtLayer>, o: &TileWriteOptions) -> Vec<u8> {
    let gz_len = |b: &[u8]| super::pmtiles::compress(b, super::pmtiles::COMPRESSION_GZIP).map(|v| v.len()).unwrap_or(b.len());
    // Features beyond the cap are not read, in order.
    let mut left = o.max_features;
    let mut too_many = false;
    for l in &mut layers {
        if l.features.len() >= left {
            l.features.truncate(left);
            too_many = true;
        }
        left -= l.features.len();
    }
    let raw = mvt::encode(&layers);
    let gz = gz_len(&raw);
    let ratio = gz as f64 / raw.len().max(1) as f64;
    let mut too_big = gz > o.max_size;
    if !too_big && !too_many {
        return raw;
    }
    let mut extent = o.extent;
    let mut current = raw;
    while too_big && !too_many && extent >= 256 {
        extent /= 2;
        current = mvt::encode(&reduce(&layers, o.extent, extent));
        too_big = gz_len(&current) > o.max_size;
    }
    if !too_big && !too_many {
        return current;
    }
    let source = if extent < o.extent { reduce(&layers, o.extent, extent) } else { layers };
    let mut all: Vec<(usize, &MvtFeature)> = source.iter().enumerate().flat_map(|(i, l)| l.features.iter().map(move |f| (i, f))).collect();
    all.sort_by(|a, b| b.1.size.total_cmp(&a.1.size));
    let total = all.len();
    let step = (total / 100).max(1);
    let mut kept: Vec<MvtLayer> =
        source.iter().map(|l| MvtLayer { name: l.name.clone(), extent: l.extent, features: Vec::new() }).collect();
    for (n, (i, f)) in all.into_iter().enumerate() {
        kept[i].features.push(f.clone());
        let count = n + 1;
        if (count == total || (too_big && count % step == 0)) && mvt::encode(&kept).len() as f64 * ratio > o.max_size as f64 {
            break;
        }
    }
    mvt::encode(&kept)
}

fn add_attr_stat(stats: &mut LayerStats, i: usize, v: &FieldValue) {
    let a = stats.attrs.entry(i).or_default();
    let j = match v {
        FieldValue::Null => return,
        FieldValue::Integer(n) => {
            let x = *n as f64;
            a.min = Some(a.min.map_or(x, |m| m.min(x)));
            a.max = Some(a.max.map_or(x, |m| m.max(x)));
            json!(n)
        }
        FieldValue::Float(x) => {
            a.min = Some(a.min.map_or(*x, |m| m.min(*x)));
            a.max = Some(a.max.map_or(*x, |m| m.max(*x)));
            if x.fract() == 0.0 && x.abs() < 9e15 { json!(*x as i64) } else { json!(x) }
        }
        FieldValue::Boolean(b) => json!(b),
        FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => json!(s),
        FieldValue::Blob(_) => return,
    };
    a.count += 1;
    if a.values.len() < 1000 && !a.values.contains(&j) {
        a.values.push(j);
    }
}

fn collect_points(g: &Geometry, out: &mut Vec<Pt>) {
    match g {
        Geometry::Point(c) => out.push((c.x, c.y)),
        Geometry::LineString(cs) | Geometry::MultiPoint(cs) => out.extend(cs.iter().map(|c| (c.x, c.y))),
        Geometry::Polygon { exterior, .. } => out.extend(exterior.0.iter().map(|c| (c.x, c.y))),
        Geometry::MultiLineString(ls) => out.extend(ls.iter().flatten().map(|c| (c.x, c.y))),
        Geometry::MultiPolygon(ps) => out.extend(ps.iter().flat_map(|(e, _)| e.0.iter()).map(|c| (c.x, c.y))),
        Geometry::GeometryCollection(gs) => gs.iter().for_each(|g| collect_points(g, out)),
    }
}

/// The MBTiles-style metadata items GDAL writes for vector tiles (`name`, `description`,
/// `version`, `minzoom`, `maxzoom`, `center`, `bounds`, `type`, `format`), plus `json` (MBTiles)
/// or the JSON members themselves (PMTiles).
pub(crate) fn metadata_items(vt: &VectorTiles, o: &TileWriteOptions, name: &str) -> Vec<(String, String)> {
    let b = vt.bounds;
    vec![
        ("name".into(), o.name.clone().unwrap_or_else(|| name.to_string())),
        ("description".into(), o.description.clone().unwrap_or_default()),
        ("version".into(), "2".into()),
        ("minzoom".into(), vt.min_zoom.to_string()),
        ("maxzoom".into(), vt.max_zoom.to_string()),
        ("center".into(), format!("{:.7},{:.7},{}", (b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0, vt.min_zoom)),
        ("bounds".into(), format!("{:.7},{:.7},{:.7},{:.7}", b[0], b[1], b[2], b[3])),
        ("type".into(), o.tileset_type.clone()),
        ("format".into(), "pbf".into()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layers_round_trip_through_tiles() {
        let mut l = Layer::new("places").with_epsg(4326);
        l.add_field(FieldDef::new("name", FieldType::Text));
        l.add_field(FieldDef::new("pop", FieldType::Integer));
        l.add_feature(Some(Geometry::point(10.0, 20.0)), &[("name", "a".into()), ("pop", 5i64.into())]).unwrap();
        l.add_feature(
            Some(Geometry::polygon(vec![Coord::xy(0.0, 0.0), Coord::xy(5.0, 0.0), Coord::xy(5.0, 5.0)], vec![])),
            &[("name", "b".into()), ("pop", (-2i64).into())],
        )
        .unwrap();
        let o = TileWriteOptions { max_zoom: Some(3), ..Default::default() };
        let vt = make_tiles(&[l], &o).unwrap();
        assert!(vt.tiles.iter().any(|t| t.0 == 3));
        let pm = super::super::write_vector_archive(&vt, &o, "p", super::super::ArchiveKind::PMTiles).unwrap();
        let mut a = TileArchive::from_bytes(pm).unwrap();
        let layers = read_layers(&mut a, &Default::default(), &[], None).unwrap();
        assert_eq!(layers.len(), 1);
        let l = &layers[0];
        assert_eq!(l.schema.fields()[2].field_type, FieldType::Integer);
        assert_eq!(l.len(), 2);
        let p = l.features.iter().find(|f| f.attributes[1] == FieldValue::Text("a".into())).unwrap();
        let Some(Geometry::MultiPoint(c)) = &p.geometry else { panic!("{:?}", p.geometry) };
        let (x, y) = super::super::lonlat_to_mercator(10.0, 20.0);
        let res = tile_dim(3) / 4096.0;
        assert!((c[0].x - x).abs() <= res && (c[0].y - y).abs() <= res);
        let poly = l.features.iter().find(|f| f.attributes[2] == FieldValue::Integer(-2)).unwrap();
        assert!(matches!(poly.geometry, Some(Geometry::MultiPolygon(_))));
    }
}
