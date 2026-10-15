//! Mapbox Vector Tiles (version 2): the protobuf encoding of tiles, layers, features and their
//! geometry commands. See <https://github.com/mapbox/vector-tile-spec/tree/master/2.1>.

use crate::{Error, Result};

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("MVT: {msg}"))
}

/// Geometry types.
pub(crate) const POINT: u32 = 1;
pub(crate) const LINESTRING: u32 = 2;
pub(crate) const POLYGON: u32 = 3;

const CMD_MOVETO: u32 = 1;
const CMD_LINETO: u32 = 2;
const CMD_CLOSEPATH: u32 = 7;
/// The commands, for encoders outside this module.
pub(crate) const MOVE_TO: u32 = CMD_MOVETO;
pub(crate) const LINE_TO: u32 = CMD_LINETO;
pub(crate) const CLOSE_PATH: u32 = CMD_CLOSEPATH;

/// An attribute value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MvtValue {
    String(String),
    Float(f32),
    Double(f64),
    Int(i64),
    UInt(u64),
    SInt(i64),
    Bool(bool),
}

/// A feature: its id, geometry type, geometry commands and attributes.
#[derive(Debug, Clone, Default)]
pub(crate) struct MvtFeature {
    pub id: Option<u64>,
    pub geom_type: u32,
    pub geometry: Vec<u32>,
    pub tags: Vec<(String, MvtValue)>,
    /// The area (polygons) or length (lines) in tile units, which decides which features a tile
    /// too large keeps (not encoded).
    pub size: f64,
}

/// A layer of a tile.
#[derive(Debug, Clone)]
pub(crate) struct MvtLayer {
    pub name: String,
    pub extent: u32,
    pub features: Vec<MvtFeature>,
}

// ─── Protobuf ───────────────────────────────────────────────────────────────────────────────────

struct Pb<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Pb<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, pos: 0 }
    }

    fn done(&self) -> bool {
        self.pos >= self.b.len()
    }

    fn varint(&mut self) -> Result<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self.b.get(self.pos).ok_or_else(|| bad("truncated varint"))?;
            self.pos += 1;
            v |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(bad("varint too long"))
    }

    /// A field key: `(field number, wire type)`.
    fn key(&mut self) -> Result<(u32, u8)> {
        let k = self.varint()?;
        Ok(((k >> 3) as u32, (k & 7) as u8))
    }

    fn bytes(&mut self) -> Result<&'a [u8]> {
        let len = self.varint()? as usize;
        let end = self.pos.checked_add(len).filter(|e| *e <= self.b.len()).ok_or_else(|| bad("truncated field"))?;
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn fixed(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).filter(|e| *e <= self.b.len()).ok_or_else(|| bad("truncated field"))?;
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn skip(&mut self, wire: u8) -> Result<()> {
        match wire {
            0 => {
                self.varint()?;
            }
            1 => {
                self.fixed(8)?;
            }
            2 => {
                self.bytes()?;
            }
            5 => {
                self.fixed(4)?;
            }
            w => return Err(bad(format!("unsupported wire type {w}"))),
        }
        Ok(())
    }

    fn packed(&mut self) -> Result<Vec<u32>> {
        let mut p = Pb::new(self.bytes()?);
        let mut out = Vec::new();
        while !p.done() {
            out.push(p.varint()? as u32);
        }
        Ok(out)
    }
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_key(out: &mut Vec<u8>, field: u32, wire: u8) {
    put_varint(out, (u64::from(field) << 3) | u64::from(wire));
}

fn put_bytes(out: &mut Vec<u8>, field: u32, b: &[u8]) {
    put_key(out, field, 2);
    put_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

fn put_packed(out: &mut Vec<u8>, field: u32, v: &[u32]) {
    let mut body = Vec::with_capacity(v.len() * 2);
    for x in v {
        put_varint(&mut body, u64::from(*x));
    }
    put_bytes(out, field, &body);
}

/// Zigzag encoding of a signed integer.
pub(crate) fn zigzag(v: i32) -> u32 {
    ((v << 1) ^ (v >> 31)) as u32
}

fn unzigzag(v: u32) -> i32 {
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

// ─── Decoding ───────────────────────────────────────────────────────────────────────────────────

fn decode_value(b: &[u8]) -> Result<Option<MvtValue>> {
    let mut p = Pb::new(b);
    let mut v = None;
    while !p.done() {
        let (field, wire) = p.key()?;
        v = Some(match (field, wire) {
            (1, 2) => MvtValue::String(String::from_utf8_lossy(p.bytes()?).into_owned()),
            (2, 5) => MvtValue::Float(f32::from_le_bytes(p.fixed(4)?.try_into().unwrap_or_default())),
            (3, 1) => MvtValue::Double(f64::from_le_bytes(p.fixed(8)?.try_into().unwrap_or_default())),
            (4, 0) => MvtValue::Int(p.varint()? as i64),
            (5, 0) => MvtValue::UInt(p.varint()?),
            (6, 0) => {
                let u = p.varint()?;
                MvtValue::SInt(((u >> 1) as i64) ^ -((u & 1) as i64))
            }
            (7, 0) => MvtValue::Bool(p.varint()? != 0),
            (_, w) => {
                p.skip(w)?;
                continue;
            }
        });
    }
    Ok(v)
}

/// Decodes a tile (gzipped or not) into its layers.
pub(crate) fn decode(bytes: &[u8]) -> Result<Vec<MvtLayer>> {
    let raw;
    let bytes = if bytes.starts_with(&[0x1f, 0x8b]) {
        raw = super::pmtiles::decompress(bytes, super::pmtiles::COMPRESSION_GZIP)?;
        &raw[..]
    } else {
        bytes
    };
    let mut p = Pb::new(bytes);
    let mut layers = Vec::new();
    while !p.done() {
        let (field, wire) = p.key()?;
        if field == 3 && wire == 2 {
            layers.push(decode_layer(p.bytes()?)?);
        } else {
            p.skip(wire)?;
        }
    }
    Ok(layers)
}

fn decode_layer(b: &[u8]) -> Result<MvtLayer> {
    let mut p = Pb::new(b);
    let (mut name, mut extent) = (String::new(), 4096u32);
    let mut keys = Vec::new();
    let mut values = Vec::new();
    let mut raw_features = Vec::new();
    while !p.done() {
        let (field, wire) = p.key()?;
        match (field, wire) {
            (1, 2) => name = String::from_utf8_lossy(p.bytes()?).into_owned(),
            (2, 2) => raw_features.push(p.bytes()?),
            (3, 2) => keys.push(String::from_utf8_lossy(p.bytes()?).into_owned()),
            (4, 2) => values.push(decode_value(p.bytes()?)?),
            (5, 0) => extent = (p.varint()? as u32).max(1),
            (_, w) => p.skip(w)?,
        }
    }
    let mut features = Vec::with_capacity(raw_features.len());
    for fb in raw_features {
        let mut f = MvtFeature::default();
        let mut p = Pb::new(fb);
        while !p.done() {
            let (field, wire) = p.key()?;
            match (field, wire) {
                (1, 0) => f.id = Some(p.varint()?),
                (2, 2) => {
                    let tags = p.packed()?;
                    for kv in tags.as_chunks::<2>().0 {
                        if let (Some(k), Some(Some(v))) = (keys.get(kv[0] as usize), values.get(kv[1] as usize)) {
                            f.tags.push((k.clone(), v.clone()));
                        }
                    }
                }
                (3, 0) => f.geom_type = p.varint()? as u32,
                (4, 2) => f.geometry = p.packed()?,
                (_, w) => p.skip(w)?,
            }
        }
        features.push(f);
    }
    Ok(MvtLayer { name, extent, features })
}

/// A decoded geometry in tile coordinates: points, lines, or polygons (exterior ring first, then
/// holes; rings closed).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TileGeom {
    Points(Vec<(i32, i32)>),
    Lines(Vec<Vec<(i32, i32)>>),
    Polygons(Vec<Vec<Vec<(i32, i32)>>>),
}

/// Twice the signed area of a ring, by the surveyor's formula.
pub(crate) fn ring_area2(r: &[(i32, i32)]) -> f64 {
    let mut a = 0.0;
    for w in r.windows(2) {
        a += w[0].0 as f64 * w[1].1 as f64 - w[1].0 as f64 * w[0].1 as f64;
    }
    if let (Some(f), Some(l)) = (r.first(), r.last()) {
        a += l.0 as f64 * f.1 as f64 - f.0 as f64 * l.1 as f64;
    }
    a
}

/// Whether a point is inside a ring (even-odd rule; points on the boundary count as inside).
fn point_in_ring(p: (i32, i32), ring: &[(i32, i32)]) -> Option<bool> {
    let (px, py) = (p.0 as f64, p.1 as f64);
    let mut inside = false;
    for w in ring.windows(2) {
        let ((x1, y1), (x2, y2)) = ((w[0].0 as f64, w[0].1 as f64), (w[1].0 as f64, w[1].1 as f64));
        // On the segment: on the boundary.
        let cross = (x2 - x1) * (py - y1) - (y2 - y1) * (px - x1);
        if cross == 0.0 && px >= x1.min(x2) && px <= x1.max(x2) && py >= y1.min(y2) && py <= y1.max(y2) {
            return None;
        }
        if (y1 > py) != (y2 > py) && px < (x2 - x1) * (py - y1) / (y2 - y1) + x1 {
            inside = !inside;
        }
    }
    Some(inside)
}

/// Whether a ring lies within a polygon (its exterior, outside its holes): every vertex is
/// inside or on the boundary, and at least one strictly inside.
fn inside_polygon(ring: &[(i32, i32)], poly: &[Vec<(i32, i32)>]) -> bool {
    let bb = |r: &[(i32, i32)]| {
        r.iter().fold((i32::MAX, i32::MAX, i32::MIN, i32::MIN), |b, p| (b.0.min(p.0), b.1.min(p.1), b.2.max(p.0), b.3.max(p.1)))
    };
    let (o, r) = (bb(&poly[0]), bb(ring));
    if !(r.0 >= o.0 && r.1 >= o.1 && r.2 <= o.2 && r.3 <= o.3) {
        return false;
    }
    let mut strict = false;
    for &p in ring {
        match point_in_ring(p, &poly[0]) {
            Some(false) => return false,
            Some(true) => {
                if poly[1..].iter().any(|h| point_in_ring(p, h) == Some(true)) {
                    return false;
                }
                strict = true;
            }
            None => {}
        }
    }
    strict
}

/// Decodes a feature's geometry commands. Polygon rings are grouped as GDAL groups them: a ring
/// wound like the first ring starts a new polygon, other rings are holes of the current one.
pub(crate) fn decode_geometry(geom_type: u32, cmds: &[u32]) -> Option<TileGeom> {
    let (mut x, mut y) = (0i32, 0i32);
    let mut i = 0;
    let mut parts: Vec<Vec<(i32, i32)>> = Vec::new();
    while i < cmds.len() {
        let (id, count) = (cmds[i] & 7, cmds[i] >> 3);
        i += 1;
        match id {
            CMD_MOVETO | CMD_LINETO => {
                for _ in 0..count {
                    let (Some(&dx), Some(&dy)) = (cmds.get(i), cmds.get(i + 1)) else { return None };
                    i += 2;
                    x = x.wrapping_add(unzigzag(dx));
                    y = y.wrapping_add(unzigzag(dy));
                    if id == CMD_MOVETO || geom_type == POINT {
                        parts.push(vec![(x, y)]);
                    } else if let Some(p) = parts.last_mut() {
                        p.push((x, y));
                    }
                }
            }
            CMD_CLOSEPATH => {
                if let Some(p) = parts.last_mut()
                    && let Some(&first) = p.first()
                {
                    p.push(first);
                }
            }
            _ => return None,
        }
    }
    match geom_type {
        POINT => Some(TileGeom::Points(parts.into_iter().flatten().collect())),
        LINESTRING => {
            let lines: Vec<_> = parts.into_iter().filter(|l| l.len() >= 2).collect();
            (!lines.is_empty()).then_some(TileGeom::Lines(lines))
        }
        POLYGON => {
            let mut polys: Vec<Vec<Vec<(i32, i32)>>> = Vec::new();
            let mut exterior_sign = 0.0f64;
            for mut ring in parts {
                if ring.first() != ring.last() {
                    let f = ring[0];
                    ring.push(f);
                }
                if ring.len() < 4 {
                    continue;
                }
                let a = ring_area2(&ring);
                let same = a != 0.0 && a.signum() == exterior_sign;
                // A ring wound like the exterior but inside the current polygon is a badly
                // oriented hole, as GDAL detects them.
                if let Some(p) = polys.last_mut()
                    && (!same || inside_polygon(&ring, p))
                {
                    p.push(ring);
                    continue;
                }
                if polys.is_empty() {
                    exterior_sign = if a == 0.0 { 1.0 } else { a.signum() };
                }
                polys.push(vec![ring]);
            }
            (!polys.is_empty()).then_some(TileGeom::Polygons(polys))
        }
        _ => None,
    }
}

// ─── Encoding ───────────────────────────────────────────────────────────────────────────────────

/// Encodes a tile's layers. Each layer's keys and values are shared by its features.
pub(crate) fn encode(layers: &[MvtLayer]) -> Vec<u8> {
    let mut out = Vec::new();
    for layer in layers {
        if layer.features.is_empty() {
            continue;
        }
        let mut lb = Vec::new();
        put_key(&mut lb, 15, 0);
        put_varint(&mut lb, 2);
        put_bytes(&mut lb, 1, layer.name.as_bytes());
        let mut keys: Vec<&str> = Vec::new();
        let mut key_ix: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
        let mut values: Vec<Vec<u8>> = Vec::new();
        let mut value_ix: std::collections::HashMap<Vec<u8>, u32> = std::collections::HashMap::new();
        for f in &layer.features {
            let mut fb = Vec::new();
            if let Some(id) = f.id {
                put_key(&mut fb, 1, 0);
                put_varint(&mut fb, id);
            }
            let mut tags = Vec::with_capacity(f.tags.len() * 2);
            for (k, v) in &f.tags {
                let ki = *key_ix.entry(k.as_str()).or_insert_with(|| {
                    keys.push(k.as_str());
                    keys.len() as u32 - 1
                });
                let vb = encode_value(v);
                let vi = match value_ix.get(&vb) {
                    Some(i) => *i,
                    None => {
                        values.push(vb.clone());
                        value_ix.insert(vb, values.len() as u32 - 1);
                        values.len() as u32 - 1
                    }
                };
                tags.extend([ki, vi]);
            }
            if !tags.is_empty() {
                put_packed(&mut fb, 2, &tags);
            }
            put_key(&mut fb, 3, 0);
            put_varint(&mut fb, u64::from(f.geom_type));
            put_packed(&mut fb, 4, &f.geometry);
            put_bytes(&mut lb, 2, &fb);
        }
        for k in keys {
            put_bytes(&mut lb, 3, k.as_bytes());
        }
        for v in values {
            put_bytes(&mut lb, 4, &v);
        }
        put_key(&mut lb, 5, 0);
        put_varint(&mut lb, u64::from(layer.extent));
        put_bytes(&mut out, 3, &lb);
    }
    out
}

fn encode_value(v: &MvtValue) -> Vec<u8> {
    let mut b = Vec::new();
    match v {
        MvtValue::String(s) => put_bytes(&mut b, 1, s.as_bytes()),
        MvtValue::Float(f) => {
            put_key(&mut b, 2, 5);
            b.extend(f.to_le_bytes());
        }
        MvtValue::Double(f) => {
            put_key(&mut b, 3, 1);
            b.extend(f.to_le_bytes());
        }
        MvtValue::Int(i) => {
            put_key(&mut b, 4, 0);
            put_varint(&mut b, *i as u64);
        }
        MvtValue::UInt(u) => {
            put_key(&mut b, 5, 0);
            put_varint(&mut b, *u);
        }
        MvtValue::SInt(i) => {
            put_key(&mut b, 6, 0);
            put_varint(&mut b, ((i << 1) ^ (i >> 63)) as u64);
        }
        MvtValue::Bool(x) => {
            put_key(&mut b, 7, 0);
            put_varint(&mut b, u64::from(*x));
        }
    }
    b
}

/// Geometry commands for points.
pub(crate) fn encode_points(pts: &[(i32, i32)]) -> Vec<u32> {
    let mut out = vec![CMD_MOVETO | ((pts.len() as u32) << 3)];
    let (mut lx, mut ly) = (0, 0);
    for &(x, y) in pts {
        out.extend([zigzag(x - lx), zigzag(y - ly)]);
        (lx, ly) = (x, y);
    }
    out
}

/// Appends commands for a line or (with `close`) a ring, without its closing point, from the
/// cursor `(lx, ly)`. Points must already be free of consecutive duplicates.
pub(crate) fn encode_path(out: &mut Vec<u32>, pts: &[(i32, i32)], close: bool, cursor: &mut (i32, i32)) {
    let Some(&(x0, y0)) = pts.first() else { return };
    out.extend([CMD_MOVETO | (1 << 3), zigzag(x0 - cursor.0), zigzag(y0 - cursor.1)]);
    out.push(CMD_LINETO | (((pts.len() - 1) as u32) << 3));
    let (mut lx, mut ly) = (x0, y0);
    for &(x, y) in &pts[1..] {
        out.extend([zigzag(x - lx), zigzag(y - ly)]);
        (lx, ly) = (x, y);
    }
    *cursor = (lx, ly);
    if close {
        out.push(CMD_CLOSEPATH | (1 << 3));
    }
}

/// Encodes a decoded geometry (rings closed) back to commands, dropping repeated points and the
/// lines and rings that collapse. `None` when nothing is left.
pub(crate) fn encode_geometry(g: &TileGeom) -> Option<(u32, Vec<u32>)> {
    let mut out = Vec::new();
    let mut cursor = (0, 0);
    match g {
        TileGeom::Points(p) => {
            let mut p = p.clone();
            p.dedup();
            (!p.is_empty()).then(|| (POINT, encode_points(&p)))
        }
        TileGeom::Lines(lines) => {
            for l in lines {
                let mut l = l.clone();
                l.dedup();
                if l.len() >= 2 {
                    encode_path(&mut out, &l, false, &mut cursor);
                }
            }
            (!out.is_empty()).then_some((LINESTRING, out))
        }
        TileGeom::Polygons(polys) => {
            for p in polys {
                for (i, r) in p.iter().enumerate() {
                    let mut r = r.clone();
                    r.dedup();
                    while r.len() > 1 && r.first() == r.last() {
                        r.pop();
                    }
                    let mut closed = r.clone();
                    if let Some(&f) = r.first() {
                        closed.push(f);
                    }
                    if r.len() < 3 || ring_area2(&closed) == 0.0 {
                        if i == 0 {
                            break;
                        }
                        continue;
                    }
                    encode_path(&mut out, &r, true, &mut cursor);
                }
            }
            (!out.is_empty()).then_some((POLYGON, out))
        }
    }
}

/// The area (polygons) or length (lines) of a geometry, in tile units; 0 for points.
pub(crate) fn geometry_size(g: &TileGeom) -> f64 {
    match g {
        TileGeom::Points(_) => 0.0,
        TileGeom::Lines(lines) => lines
            .iter()
            .flat_map(|l| l.windows(2))
            .map(|w| ((w[1].0 as f64 - w[0].0 as f64).powi(2) + (w[1].1 as f64 - w[0].1 as f64).powi(2)).sqrt())
            .sum(),
        TileGeom::Polygons(polys) => polys
            .iter()
            .map(|p| p.iter().enumerate().map(|(i, r)| if i == 0 { ring_area2(r).abs() } else { -ring_area2(r).abs() }).sum::<f64>() / 2.0)
            .sum(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiles_round_trip() {
        let mut geometry = Vec::new();
        let mut cursor = (0, 0);
        // A clockwise square (in tile coordinates, y down) with a hole.
        encode_path(&mut geometry, &[(0, 0), (10, 0), (10, 10), (0, 10)], true, &mut cursor);
        encode_path(&mut geometry, &[(2, 2), (2, 8), (8, 8), (8, 2)], true, &mut cursor);
        let f = MvtFeature {
            id: Some(7),
            geom_type: POLYGON,
            geometry,
            tags: vec![("name".into(), MvtValue::String("a".into())), ("n".into(), MvtValue::SInt(-3))],
            size: 0.0,
        };
        let layer = MvtLayer { name: "l".into(), extent: 4096, features: vec![f] };
        let back = decode(&encode(&[layer])).unwrap();
        assert_eq!(back[0].name, "l");
        let f = &back[0].features[0];
        assert_eq!(f.id, Some(7));
        assert_eq!(f.tags[1], ("n".to_string(), MvtValue::SInt(-3)));
        let Some(TileGeom::Polygons(p)) = decode_geometry(f.geom_type, &f.geometry) else { panic!() };
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].len(), 2);
        assert_eq!(p[0][0], vec![(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)]);
        let Some(TileGeom::Points(pts)) = decode_geometry(POINT, &encode_points(&[(5, 5), (-3, 2)])) else { panic!() };
        assert_eq!(pts, vec![(5, 5), (-3, 2)]);
    }
}
