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
    let mut tiles = match filter.and_then(|f| tiles_in_rect(z, f)) {
        // A remote archive and a small rectangle: the tiles under it, rather than every tile of the
        // zoom level from the directories (missing tiles are skipped).
        Some(t) if archive.is_remote() => t,
        _ => archive.tiles_at_zoom(z)?,
    };
    if let Some(f) = filter {
        tiles.retain(|&(x, y)| clip::overlaps(TileFrame::new(z, x, y).rect(), f) || clip::contains(TileFrame::new(z, x, y).rect(), f));
    }
    // Remote archives whose bytes the caller fetches: every tile at once, in one round of
    // requests.
    let chunk_len = if archive.is_sparse() { tiles.len().max(1) } else { 256 };
    for chunk in tiles.chunks(chunk_len) {
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

/// The tiles `(x, y)` at zoom `z` under a rectangle in EPSG:3857, in tile id order (as the
/// directories list them), when there are at most 4096.
fn tiles_in_rect(z: u8, r: Rect) -> Option<Vec<(u32, u32)>> {
    let dim = tile_dim(z);
    let n = 1u64 << z;
    let col = |x: f64| (((x + MAX_GM) / dim).floor().max(0.0) as u64).min(n - 1);
    let row = |y: f64| (((MAX_GM - y) / dim).floor().max(0.0) as u64).min(n - 1);
    let (x0, x1, y0, y1) = (col(r.0), col(r.2), row(r.3), row(r.1));
    if (x1 - x0 + 1) * (y1 - y0 + 1) > 4096 {
        return None;
    }
    let mut out: Vec<(u32, u32)> = (y0..=y1).flat_map(|y| (x0..=x1).map(move |x| (x as u32, y as u32))).collect();
    out.sort_by_key(|&(x, y)| super::pmtiles::zxy_to_tile_id(z, x, y));
    Some(out)
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

/// Whether a closed ring is clockwise (y up), as OGR decides it: by the turn at its lowest
/// (then rightmost) vertex, or by its area when that vertex is repeated or its neighbours are
/// too close.
fn ogr_is_clockwise(p: &[Pt]) -> bool {
    let n = p.len();
    if n < 2 {
        return true;
    }
    let mut fallback = false;
    let mut v = 0;
    for i in 1..n - 1 {
        if p[i].1 < p[v].1 || (p[i].1 == p[v].1 && p[i].0 > p[v].0) {
            v = i;
            fallback = false;
        } else if p[i].1 == p[v].1 && p[i].0 == p[v].0 {
            fallback = true;
        }
    }
    let close = |a: Pt, b: Pt| (a.0 - b.0).abs() < 1e-5 && (a.1 - b.1).abs() < 1e-5;
    let prev = if v == 0 { n - 2 } else { v - 1 };
    fallback |= close(p[prev], p[v]);
    let (dx0, dy0) = (p[prev].0 - p[v].0, p[prev].1 - p[v].1);
    let next = if v + 1 >= n - 1 { 0 } else { v + 1 };
    fallback |= close(p[next], p[v]);
    let (dx1, dy1) = (p[next].0 - p[v].0, p[next].1 - p[v].1);
    let cross = dx1 * dy0 - dx0 * dy1;
    if !fallback {
        if cross > 0.0 {
            return false;
        }
        if cross < 0.0 {
            return true;
        }
    }
    let mut sum = p[0].0 * (p[1].1 - p[n - 1].1);
    for i in 1..n - 1 {
        sum += p[i].0 * (p[i + 1].1 - p[i - 1].1);
    }
    sum += p[n - 1].0 * (p[0].1 - p[n - 2].1);
    sum < 0.0
}

/// How points go to tile coordinates: rounded on the tile grid, or (for polygons GEOS made
/// valid, already in tile coordinates) truncated.
#[derive(Clone, Copy)]
enum ToTile<'a> {
    Grid(&'a TileFrame, u32),
    Truncate,
}

impl ToTile<'_> {
    fn apply(self, p: Pt) -> (i32, i32) {
        match self {
            ToTile::Grid(f, extent) => {
                (((p.0 - f.top_x) * extent as f64 / f.dim).round() as i32, ((f.top_y - p.1) * extent as f64 / f.dim).round() as i32)
            }
            ToTile::Truncate => (p.0 as i32, p.1 as i32),
        }
    }
}

/// Encodes a polygon ring (closed) as GDAL's `EncodeLineString` does: exteriors clockwise on
/// screen and holes counter-clockwise (judged on the ring in tile coordinates), a hole dropped
/// when rounding flips its orientation, repeated points dropped, and the ring dropped when fewer
/// than three points are left. Returns the ring as encoded (closed), and moves the cursor.
fn encode_ring(out: &mut Vec<u32>, ring: &[Pt], to: ToTile, exterior: bool, cursor: &mut (i32, i32)) -> Option<Vec<(i32, i32)>> {
    let n = ring.len();
    let np = n - 1;
    // The winding order, in integer coordinates (y up).
    let mut ri: Vec<Pt> = Vec::with_capacity(n);
    let mut last = *cursor;
    for (i, p) in ring[..np].iter().enumerate() {
        let q = to.apply(*p);
        if i == 0 || q != last {
            ri.push((q.0 as f64, -(q.1 as f64)));
            last = q;
        }
    }
    if ri.len() > 1 && ri.first() != ri.last() {
        ri.push(ri[0]);
    }
    if ri.len() < 4 {
        return None;
    }
    let cw = ogr_is_clockwise(&ri);
    let reverse = if exterior {
        !cw
    } else {
        let original = ogr_is_clockwise(ring);
        let truncate = matches!(to, ToTile::Truncate);
        if (!truncate && cw != original) || (truncate && cw == original) {
            return None;
        }
        cw
    };
    let start = out.len();
    let origin = *cursor;
    let (mut lineto, mut first, mut last, mut last_valid) = (0u32, (0, 0), *cursor, *cursor);
    let mut pts = Vec::with_capacity(n);
    for i in 0..np {
        let q = to.apply(ring[if reverse { n - 1 - i } else { i }]);
        if i == 0 || q != last {
            if i > 0 {
                lineto += 1;
                if lineto == 1 {
                    out.push(mvt::MOVE_TO | (1 << 3));
                    out.push(mvt::zigzag(last.0 - origin.0));
                    out.push(mvt::zigzag(last.1 - origin.1));
                    out.push(mvt::LINE_TO);
                    pts.push(last);
                }
                out.push(mvt::zigzag(q.0 - last.0));
                out.push(mvt::zigzag(q.1 - last.1));
                pts.push(q);
            } else {
                first = q;
            }
            last_valid = last;
            last = q;
        }
    }
    if lineto > 0 && first == last {
        out.truncate(out.len() - 2);
        pts.pop();
        lineto -= 1;
        last = last_valid;
    }
    if lineto < 2 {
        out.truncate(start);
        return None;
    }
    out[start + 3] = mvt::LINE_TO | (lineto << 3);
    out.push(mvt::CLOSE_PATH | (1 << 3));
    *cursor = last;
    pts.push(pts[0]);
    Some(pts)
}

/// Encodes a polygon's rings as GDAL's `EncodePolygon` does: `false` (and nothing encoded) when
/// its exterior is dropped. Returns the polygon as encoded.
fn encode_polygon(out: &mut Vec<u32>, rings: &[Vec<Pt>], to: ToTile, cursor: &mut (i32, i32)) -> (bool, Vec<Vec<(i32, i32)>>) {
    let mut encoded = Vec::new();
    for (i, r) in rings.iter().enumerate() {
        let ok = r.len() >= 4 && r.first() == r.last();
        match ok.then(|| encode_ring(out, r, to, i == 0, cursor)).flatten() {
            Some(pts) => encoded.push(pts),
            None if i == 0 => return (false, encoded),
            None => {}
        }
    }
    (true, encoded)
}

/// Encodes polygons in a tile as GDAL's MVT writer does: each polygon encoded, and when the
/// polygons encoded are not valid (rounding to the grid made rings cross or touch, parts
/// overlap), GEOS's `MakeValid` of them encoded instead.
fn encode_polygons(polys: &[Vec<Vec<Pt>>], f: &TileFrame, extent: u32) -> Option<Vec<u32>> {
    let mut out = Vec::new();
    let mut cursor = (0, 0);
    let mut ok = false;
    let mut encoded = Vec::with_capacity(polys.len());
    for rings in polys {
        let (o, e) = encode_polygon(&mut out, rings, ToTile::Grid(f, extent), &mut cursor);
        ok |= o;
        encoded.push(e);
    }
    if !super::valid::is_valid(&encoded) {
        let fixed = super::valid::make_valid(&encoded);
        out.clear();
        cursor = (0, 0);
        ok = false;
        for rings in &fixed {
            ok |= encode_polygon(&mut out, rings, ToTile::Truncate, &mut cursor).0;
        }
    }
    (ok && !out.is_empty()).then_some(out)
}

/// A place where a polygonal geometry is invalid in a way GEOS's overlay cannot handle: a point,
/// the bounding boxes of the two parts that conflict there, and whether they belong to different
/// polygons (overlapping parts of a multipolygon).
type Crossing = (Pt, Rect, Rect, bool);

/// Where a polygonal geometry is invalid in ways that make GEOS fail to clip it to a tile when
/// they are near the tile: segments of its rings crossing, segments overlapping with the
/// interior on the same side (overlapping parts), and parts inside other parts.
fn ring_crossings(polys: &[Vec<Vec<Pt>>]) -> Vec<Crossing> {
    // Segments: (ring id, index, segment count, from, to, interior on the left).
    let mut segs: Vec<(usize, usize, usize, Pt, Pt, bool)> = Vec::new();
    for (pi, p) in polys.iter().enumerate() {
        for (ri, r) in p.iter().enumerate() {
            let left = (clip::ring_area(r) > 0.0) != (ri > 0);
            for (i, w) in r.windows(2).enumerate() {
                if w[0] != w[1] {
                    segs.push((pi * 1_000_000 + ri, i, r.len() - 1, w[0], w[1], left));
                }
            }
        }
    }
    let boxes: Vec<(f64, f64, f64, f64)> =
        segs.iter().map(|s| (s.3.0.min(s.4.0), s.3.1.min(s.4.1), s.3.0.max(s.4.0), s.3.1.max(s.4.1))).collect();
    let cross = |a: Pt, b: Pt, c: Pt| (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0);
    let orient = |a: Pt, b: Pt, c: Pt| {
        let v = cross(a, b, c);
        if v > 0.0 {
            1.0
        } else if v < 0.0 {
            -1.0
        } else {
            0.0
        }
    };
    let mut out = Vec::new();
    for (s, t_) in super::valid::box_pairs(&boxes) {
        let (a, b) = (&segs[s], &segs[t_]);
        if a.0 == b.0 && (a.1.abs_diff(b.1) == 1 || a.1.abs_diff(b.1) == a.2 - 1) {
            continue;
        }
        let (o1, o2, o3, o4) = (orient(a.3, a.4, b.3), orient(a.3, a.4, b.4), orient(b.3, b.4, a.3), orient(b.3, b.4, a.4));
        // Crossings of a few ulps (rings touching before reprojection) are snapped away by
        // GEOS's overlay: only those deeper than 1e-10 of the coordinates count.
        let depth = |p: Pt, s: (Pt, Pt)| cross(s.0, s.1, p).abs() / (s.1.0 - s.0.0).hypot(s.1.1 - s.0.1);
        let deep = || {
            let magnitude = [a.3, a.4, b.3, b.4].iter().fold(0.0f64, |m, p| m.max(p.0.abs()).max(p.1.abs()));
            let d = depth(a.3, (b.3, b.4)).min(depth(a.4, (b.3, b.4))).min(depth(b.3, (a.3, a.4))).min(depth(b.4, (a.3, a.4)));
            d > magnitude * 1e-10
        };
        if o1 * o2 < 0.0 && o3 * o4 < 0.0 && deep() {
            // The crossing point, along `a`.
            let (d1, d2) = (cross(b.3, b.4, a.3), cross(b.3, b.4, a.4));
            let t = d1 / (d1 - d2);
            out.push(((a.3.0 + t * (a.4.0 - a.3.0), a.3.1 + t * (a.4.1 - a.3.1)), boxes[s], boxes[t_], a.0 != b.0));
        } else if o1 == 0.0 && o2 == 0.0 && o3 == 0.0 && o4 == 0.0 && a.0 != b.0 {
            // Collinear segments of two rings, overlapping with the interior on the same side.
            let (dx, dy) = (a.4.0 - a.3.0, a.4.1 - a.3.1);
            let along = |p: Pt| (p.0 - a.3.0) * dx + (p.1 - a.3.1) * dy;
            let len = dx * dx + dy * dy;
            let (lo, hi) = (along(b.3).min(along(b.4)).max(0.0), along(b.3).max(along(b.4)).min(len));
            let same_direction = (b.4.0 - b.3.0) * dx + (b.4.1 - b.3.1) * dy > 0.0;
            if hi > lo && (a.5 == b.5) == same_direction {
                let t = (lo + hi) / 2.0 / len;
                out.push(((a.3.0 + t * dx, a.3.1 + t * dy), boxes[s], boxes[t_], a.0 != b.0));
            }
        }
    }
    // Parts inside other parts (outside their holes).
    let part_boxes: Vec<Option<Rect>> = polys.iter().map(|p| p.first().and_then(|r| clip::bbox(r.iter()))).collect();
    for (i, p) in polys.iter().enumerate() {
        let (Some(bi), Some(first)) = (part_boxes[i], p.first().and_then(|r| r.first())) else { continue };
        for (j, q) in polys.iter().enumerate() {
            let Some(bj) = part_boxes[j] else { continue };
            if i == j || !clip::contains(bj, bi) {
                continue;
            }
            if clip::point_in_ring(*first, &q[0]) && !q[1..].iter().any(|h| clip::point_in_ring(*first, h)) {
                out.push((*first, bi, bi, false));
            }
        }
    }
    out
}

/// Whether GEOS fails to intersect a polygonal geometry (with crossings `crossings`, bounding box
/// `env`) with the rectangle `r` (a "side location conflict"): when a crossing falls in the
/// rectangle, or in the envelope OverlayNG clips its inputs to (the inputs' envelopes, each grown
/// by a tenth, intersected; grown to the segments crossing it, then by a tenth again) with one
/// of its segments near the rectangle (or anywhere in that envelope for overlapping parts). This
/// rule agrees with GEOS 3.14 on 97% of random self-crossing polygons and rectangles, and 97% of
/// random overlapping parts.
fn clip_fails(polys: &[Vec<Vec<Pt>>], crossings: &[Crossing], env: Rect, r: Rect) -> bool {
    if crossings.is_empty() {
        return false;
    }
    let grow = |e: Rect| {
        let (w, h) = (e.2 - e.0, e.3 - e.1);
        let d = 0.1 * if w.min(h) > 0.0 { w.min(h) } else { w.max(h) };
        (e.0 - d, e.1 - d, e.2 + d, e.3 + d)
    };
    let (a, b) = (grow(env), grow(r));
    let result = (a.0.max(b.0), a.1.max(b.1), a.2.min(b.2), a.3.min(b.3));
    if result.0 > result.2 || result.1 > result.3 {
        return false;
    }
    let mut clip = result;
    let corners = [(r.0, r.1), (r.2, r.1), (r.2, r.3), (r.0, r.3), (r.0, r.1)];
    let rings = polys.iter().flatten().map(Vec::as_slice).chain(std::iter::once(&corners[..]));
    for ring in rings {
        for w in ring.windows(2) {
            let (p, q) = (w[0], w[1]);
            if p.0.max(q.0) >= result.0 && p.0.min(q.0) <= result.2 && p.1.max(q.1) >= result.1 && p.1.min(q.1) <= result.3 {
                clip = (clip.0.min(p.0.min(q.0)), clip.1.min(p.1.min(q.1)), clip.2.max(p.0.max(q.0)), clip.3.max(p.1.max(q.1)));
            }
        }
    }
    let clip = grow(clip);
    let within = |p: Pt, e: Rect| p.0 >= e.0 && p.0 <= e.2 && p.1 >= e.1 && p.1 <= e.3;
    let meets = |a: Rect, b: Rect| a.0 <= b.2 && b.0 <= a.2 && a.1 <= b.3 && b.1 <= a.3;
    crossings.iter().any(|&(p, s1, s2, parts)| within(p, r) || (within(p, clip) && (parts || meets(s1, r) || meets(s2, r))))
}

/// The rings of a polygon or multipolygon, by polygon, closed.
fn polygon_rings(g: &Geometry) -> Option<Vec<Vec<Vec<Pt>>>> {
    let ring = |r: &Ring| {
        let mut v: Vec<Pt> = r.0.iter().map(|c| (c.x, c.y)).collect();
        if v.len() > 1 && v.first() != v.last() {
            v.push(v[0]);
        }
        v
    };
    match g {
        Geometry::Polygon { exterior, interiors } => Some(vec![std::iter::once(exterior).chain(interiors).map(ring).collect()]),
        Geometry::MultiPolygon(ps) => Some(ps.iter().map(|(e, h)| std::iter::once(e).chain(h).map(ring).collect()).collect()),
        _ => None,
    }
}

/// A feature's geometry, prepared for one tile: clipped to the buffered tile, simplified and
/// encoded. `None` when nothing is left, or when GEOS would fail to clip it (`crossings`, where
/// its rings cross, are near the tile).
fn encode_for_tile(
    g: &Geometry,
    f: &TileFrame,
    extent: u32,
    buffer: f64,
    tolerance: f64,
    crossings: &[Crossing],
) -> Option<(u32, Vec<u32>)> {
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
            let polys = polygon_rings(g)?;
            let all = clip::bbox(polys.iter().flatten().flatten())?;
            let inside = clip::contains(r, all);
            if !inside && clip_fails(&polys, crossings, all, r) {
                return None;
            }
            let mut parts: Vec<Vec<Vec<Pt>>> = Vec::new();
            for mut p in polys {
                if !inside && p.len() > 1 {
                    // GEOS's overlay leaves out holes outside their exterior.
                    let exterior = p.remove(0);
                    p.retain(|h| h.iter().any(|v| clip::point_in_ring(*v, &exterior)));
                    p.insert(0, exterior);
                }
                let clipped: Vec<Vec<Vec<Pt>>> = if inside { vec![p] } else { clip::clip_polygon(&p, r) };
                parts.extend(clipped.into_iter().map(|rings| rings.into_iter().map(simplify).collect()));
            }
            encode_polygons(&parts, f, extent).map(|out| (mvt::POLYGON, out))
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
    let cut = cut_layers(layers, o)?;
    let tiles = cut.tiles.into_iter().map(|((z, x, y), layers)| (z, x, y, encode_limited(layers, o))).collect();
    Ok(VectorTiles { tiles, json: cut.json, bounds: cut.bounds, min_zoom: cut.min_zoom, max_zoom: cut.max_zoom })
}

/// Vector layers cut into tiles, before the tiles are encoded.
struct CutLayers {
    tiles: BTreeMap<(u8, u32, u32), Vec<MvtLayer>>,
    json: Json,
    bounds: [f64; 4],
    min_zoom: u8,
    max_zoom: u8,
}

/// The tiles' layers of [`make_tiles`], not yet encoded.
fn cut_layers(layers: &[Layer], o: &TileWriteOptions) -> Result<CutLayers> {
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
                let crossings = polygon_rings(g).map(|p| ring_crossings(&p)).unwrap_or_default();
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
                            let Some((geom_type, geometry)) = encode_for_tile(g, &frame, o.extent, buffer, tol, &crossings) else {
                                continue;
                            };
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
    Ok(CutLayers { tiles, json, bounds: [lon0, lat0, lon1, lat1], min_zoom, max_zoom })
}

/// Adds vector layers to an archive of vector tiles: their features cut into the archive's zoom
/// levels (or `MINZOOM` to `MAXZOOM`) and tile extent, and added to the tiles' layers of the same
/// name (or, with `overwrite`, replacing those layers in every tile), other layers kept; the
/// metadata (`vector_layers`, `tilestats`, bounds, zoom levels) merged.
pub(crate) fn update_tiles(archive: &mut TileArchive, layers: &[Layer], o: &TileWriteOptions, overwrite: bool) -> Result<VectorTiles> {
    if !archive.is_vector() {
        return Err(Error::InvalidArgument("the archive holds image tiles: vector layers cannot be added to it".into()));
    }
    let (min_z, max_z) = archive.zoom_range();
    let mut o = o.clone();
    o.min_zoom = Some(o.min_zoom.unwrap_or(min_z));
    o.max_zoom = Some(o.max_zoom.unwrap_or(max_z));
    let mut tiles: BTreeMap<(u8, u32, u32), Vec<MvtLayer>> = BTreeMap::new();
    for z in min_z..=max_z {
        let present = archive.tiles_at_zoom(z)?;
        for chunk in present.chunks(256) {
            let wanted: Vec<(u8, u32, u32)> = chunk.iter().map(|&(x, y)| (z, x, y)).collect();
            for (key, bytes) in wanted.iter().zip(archive.tiles(&wanted)?) {
                let Some(bytes) = bytes else { continue };
                let mut layers = mvt::decode(&bytes)?;
                for f in layers.iter_mut().flat_map(|l| l.features.iter_mut()) {
                    f.size = mvt::decode_geometry(f.geom_type, &f.geometry).map_or(0.0, |g| mvt::geometry_size(&g));
                }
                tiles.insert(*key, layers);
            }
        }
    }
    // The archive's tile extent.
    if let Some(e) = tiles.values().flatten().map(|l| l.extent).next() {
        o.extent = e;
    }
    if overwrite {
        let names: Vec<&str> = layers.iter().map(|l| l.name.as_str()).collect();
        for t in tiles.values_mut() {
            t.retain(|l| !names.contains(&l.name.as_str()));
        }
    }
    let cut = cut_layers(layers, &o)?;
    for (key, added) in cut.tiles {
        let t = tiles.entry(key).or_default();
        for layer in added {
            match t.iter_mut().find(|l| l.name == layer.name) {
                Some(l) => l.features.extend(layer.features),
                None => t.push(layer),
            }
        }
    }
    let encoded = tiles
        .into_iter()
        .filter(|(_, ls)| ls.iter().any(|l| !l.features.is_empty()))
        .map(|((z, x, y), ls)| (z, x, y, encode_limited(ls, &o)))
        .collect();
    let bounds = match archive.bounds() {
        Some(b) => [b[0].min(cut.bounds[0]), b[1].min(cut.bounds[1]), b[2].max(cut.bounds[2]), b[3].max(cut.bounds[3])],
        None => cut.bounds,
    };
    Ok(VectorTiles {
        tiles: encoded,
        json: merge_metadata(&archive.json(), &cut.json, overwrite),
        bounds,
        min_zoom: cut.min_zoom.min(min_z),
        max_zoom: cut.max_zoom.max(max_z),
    })
}

/// Merges the metadata of layers added to an archive (`vector_layers` and `tilestats`) into the
/// archive's: a layer written replaces the archive's of the same name with `overwrite`, or
/// extends it (fields, zoom levels and statistics combined).
fn merge_metadata(old: &Json, new: &Json, overwrite: bool) -> Json {
    let mut out = if old.is_object() { old.clone() } else { json!({}) };
    let list = |v: &Json, k: &str| v.get(k).and_then(Json::as_array).cloned().unwrap_or_default();
    // vector_layers, by id.
    let mut layers = list(old, "vector_layers");
    for n in list(new, "vector_layers") {
        match layers.iter_mut().find(|l| l.get("id") == n.get("id")) {
            Some(l) if !overwrite => {
                if let (Some(fields), Some(added)) =
                    (l.get_mut("fields").and_then(Json::as_object_mut), n.get("fields").and_then(Json::as_object))
                {
                    for (k, v) in added {
                        fields.entry(k.clone()).or_insert_with(|| v.clone());
                    }
                }
                for (k, pick) in [("minzoom", u64::min as fn(u64, u64) -> u64), ("maxzoom", u64::max)] {
                    if let (Some(a), Some(b)) = (l.get(k).and_then(Json::as_u64), n.get(k).and_then(Json::as_u64)) {
                        l[k] = json!(pick(a, b));
                    }
                }
            }
            Some(l) => *l = n,
            None => layers.push(n),
        }
    }
    out["vector_layers"] = Json::Array(layers);
    // tilestats, by layer.
    let old_stats = old.pointer("/tilestats/layers").and_then(Json::as_array).cloned().unwrap_or_default();
    let mut stats = old_stats;
    for n in new.pointer("/tilestats/layers").and_then(Json::as_array).cloned().unwrap_or_default() {
        match stats.iter_mut().find(|l| l.get("layer") == n.get("layer")) {
            Some(l) if !overwrite => {
                let count = l.get("count").and_then(Json::as_u64).unwrap_or(0) + n.get("count").and_then(Json::as_u64).unwrap_or(0);
                l["count"] = json!(count);
                let mut attrs = l.get("attributes").and_then(Json::as_array).cloned().unwrap_or_default();
                for na in n.get("attributes").and_then(Json::as_array).cloned().unwrap_or_default() {
                    match attrs.iter_mut().find(|a| a.get("attribute") == na.get("attribute")) {
                        Some(a) => {
                            let c =
                                a.get("count").and_then(Json::as_u64).unwrap_or(0) + na.get("count").and_then(Json::as_u64).unwrap_or(0);
                            a["count"] = json!(c);
                            let mut values = a.get("values").and_then(Json::as_array).cloned().unwrap_or_default();
                            for v in na.get("values").and_then(Json::as_array).cloned().unwrap_or_default() {
                                if values.len() < 1000 && !values.contains(&v) {
                                    values.push(v);
                                }
                            }
                            a["values"] = Json::Array(values);
                            for (k, smaller) in [("min", true), ("max", false)] {
                                if let (Some(x), Some(y)) = (a.get(k).and_then(Json::as_f64), na.get(k).and_then(Json::as_f64)) {
                                    let keep_old = if smaller { x <= y } else { x >= y };
                                    if !keep_old {
                                        a[k] = na[k].clone();
                                    }
                                }
                            }
                        }
                        None => attrs.push(na),
                    }
                }
                l["attributeCount"] = json!(attrs.len());
                l["attributes"] = Json::Array(attrs);
            }
            Some(l) => *l = n,
            None => stats.push(n),
        }
    }
    out["tilestats"] = json!({ "layerCount": stats.len(), "layers": stats });
    out
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
