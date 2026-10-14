//! Esri Shapefile: `.shp`, `.shx`, `.dbf`, `.cpg` and `.prj`.
//!
//! Reading keeps Z and M values, groups polygon rings into polygons by orientation and
//! containment, and decodes text as the `.cpg` file (or the DBF language driver) declares, falling
//! back to Latin-1 for text that is not UTF-8.
//!
//! Writing stores text as UTF-8, declared in a `.cpg` file. Field names are truncated to 10 bytes
//! and made unique the way GDAL does it (`populati_1`), and field widths follow GDAL's: 9 or 18
//! digits for integers, 24 with 15 decimals for reals, 80 characters (or the longest value) for
//! strings.

use crate::crs::SpatialRef;
use crate::{Error, Result};
use std::path::{Path, PathBuf};
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer, Ring};

const NULL_SHAPE: i32 = 0;

/// Finds a sidecar file (`dbf`, `prj`, `cpg`) next to the `.shp`, in either letter case.
fn sidecar(base: &Path, ext: &str) -> Option<PathBuf> {
    [ext.to_string(), ext.to_ascii_uppercase()].into_iter().map(|e| base.with_extension(e)).find(|p| p.exists())
}

/// Reads a shapefile. `path` names the `.shp` file.
pub fn read(path: &str) -> Result<Layer> {
    let base = Path::new(path).with_extension("");
    let shp = std::fs::read(path)?;
    if shp.len() < 100 || u32::from_be_bytes([shp[0], shp[1], shp[2], shp[3]]) != 9994 {
        return Err(Error::InvalidData(format!("'{path}' is not a shapefile")));
    }
    let shape_type = i32::from_le_bytes([shp[32], shp[33], shp[34], shp[35]]);
    let mut layer = Layer::new(base.file_name().and_then(|s| s.to_str()).unwrap_or("layer"));
    layer.geom_type = match if shape_type == 31 { 0 } else { shape_type % 10 } {
        1 => Some(GeometryType::Point),
        3 => Some(GeometryType::LineString),
        5 => Some(GeometryType::Polygon),
        8 => Some(GeometryType::MultiPoint),
        _ => None,
    };

    let mut geometries = Vec::new();
    let mut pos = 100;
    while pos + 8 <= shp.len() {
        let words = u32::from_be_bytes([shp[pos + 4], shp[pos + 5], shp[pos + 6], shp[pos + 7]]) as usize;
        let end = (pos + 8 + 2 * words).min(shp.len());
        geometries.push(parse_shape(&shp[pos + 8..end]));
        pos = end;
    }

    let encoding = sidecar(&base, "cpg").and_then(|p| std::fs::read_to_string(p).ok());
    let (fields, records) = match sidecar(&base, "dbf") {
        Some(p) => parse_dbf(&std::fs::read(p)?, encoding.as_deref())?,
        None => (Vec::new(), Vec::new()),
    };
    for f in fields {
        layer.add_field(f);
    }
    let n = if records.is_empty() { geometries.len() } else { records.len().max(geometries.len()) };
    let mut records = records.into_iter();
    let mut geometries = geometries.into_iter();
    for fid in 0..n {
        let geometry = geometries.next().flatten();
        let attributes = records.next().unwrap_or_else(|| vec![FieldValue::Null; layer.schema.len()]);
        layer.push(Feature { fid: fid as u64, geometry, attributes });
    }
    if let Some(prj) = sidecar(&base, "prj").and_then(|p| std::fs::read_to_string(p).ok()).filter(|s| !s.trim().is_empty()) {
        SpatialRef::from_wkt(prj.trim()).apply_to_layer(&mut layer);
        layer.set_crs_wkt(Some(prj.trim().to_string()));
    }
    Ok(layer)
}

fn parse_shape(rec: &[u8]) -> Option<Geometry> {
    let i32_at = |o: usize| rec.get(o..o + 4).map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let f64_at = |o: usize| rec.get(o..o + 8).map(|b| f64::from_le_bytes(b.try_into().unwrap_or([0; 8])));
    let st = i32_at(0)?;
    if st == 31 {
        return None; // MultiPatch
    }
    let (has_z, has_m) = (st / 10 == 1, st / 10 == 1 || st / 10 == 2);
    // M values below -1e38 mean "no data".
    let m_value = |v: Option<f64>| v.filter(|m| *m > -1e38);
    if st % 10 == 1 {
        let (x, y) = (f64_at(4)?, f64_at(12)?);
        let z = if has_z { f64_at(20) } else { None };
        let m = m_value(if has_z {
            f64_at(28)
        } else if has_m {
            f64_at(20)
        } else {
            None
        });
        return Some(Geometry::Point(Coord { x, y, z, m }));
    }
    if !matches!(st % 10, 3 | 5 | 8) {
        return None;
    }
    let multipoint = st % 10 == 8;
    let (num_parts, num_points, parts_at) =
        if multipoint { (1, i32_at(36)? as usize, 40) } else { (i32_at(36)? as usize, i32_at(40)? as usize, 44) };
    let points_at = parts_at + if multipoint { 0 } else { 4 * num_parts };
    let mut starts: Vec<usize> =
        if multipoint { vec![0] } else { (0..num_parts).map(|i| i32_at(parts_at + 4 * i).map(|v| v as usize)).collect::<Option<_>>()? };
    starts.push(num_points);
    let z_at = points_at + 16 * num_points + 16;
    let m_at = if has_z { z_at + 8 * num_points + 16 } else { z_at };
    let coords: Vec<Coord> = (0..num_points)
        .map(|i| {
            Some(Coord {
                x: f64_at(points_at + 16 * i)?,
                y: f64_at(points_at + 16 * i + 8)?,
                z: if has_z { f64_at(z_at + 8 * i) } else { None },
                m: if has_m { m_value(f64_at(m_at + 8 * i)) } else { None },
            })
        })
        .collect::<Option<_>>()?;
    let parts: Vec<Vec<Coord>> =
        starts.windows(2).filter(|w| w[0] < w[1] && w[1] <= coords.len()).map(|w| coords[w[0]..w[1]].to_vec()).collect();
    if parts.is_empty() {
        return None;
    }
    Some(match st % 10 {
        8 => Geometry::MultiPoint(coords),
        3 if parts.len() == 1 => Geometry::LineString(parts.into_iter().next()?),
        3 => Geometry::MultiLineString(parts),
        _ => organize_rings(parts),
    })
}

/// Groups rings into polygons: clockwise rings are exteriors, and each counter-clockwise ring is
/// a hole of the smallest exterior containing it.
fn organize_rings(parts: Vec<Vec<Coord>>) -> Geometry {
    let rings: Vec<Ring> = parts
        .into_iter()
        .map(|mut cs| {
            if cs.len() > 1 && (cs[0].x, cs[0].y) == (cs[cs.len() - 1].x, cs[cs.len() - 1].y) {
                cs.pop();
            }
            Ring::new(cs)
        })
        .collect();
    let (mut exteriors, mut holes): (Vec<Ring>, Vec<Ring>) = rings.into_iter().partition(|r| r.signed_area() <= 0.0);
    if exteriors.is_empty() {
        // Wrongly wound: take every ring as an exterior.
        exteriors = std::mem::take(&mut holes);
    }
    let mut polygons: Vec<(Ring, Vec<Ring>)> = exteriors.into_iter().map(|r| (r, Vec::new())).collect();
    for h in holes {
        let Some(p) = h.0.first() else { continue };
        let owner = polygons
            .iter()
            .enumerate()
            .filter(|(_, (e, _))| ring_contains(e, p.x, p.y))
            .min_by(|a, b| a.1.0.signed_area().abs().total_cmp(&b.1.0.signed_area().abs()))
            .map(|(i, _)| i);
        match owner {
            Some(i) => polygons[i].1.push(h),
            None => polygons.push((Ring::new(h.0.into_iter().rev().collect()), Vec::new())),
        }
    }
    if polygons.len() == 1 {
        let (exterior, interiors) = polygons.pop().unwrap_or_default();
        Geometry::Polygon { exterior, interiors }
    } else {
        Geometry::MultiPolygon(polygons)
    }
}

/// Whether a point is inside a ring (even-odd rule).
pub(crate) fn ring_contains(r: &Ring, x: f64, y: f64) -> bool {
    let cs = &r.0;
    let mut inside = false;
    let mut j = cs.len().wrapping_sub(1);
    for i in 0..cs.len() {
        let (a, b) = (&cs[i], &cs[j]);
        if (a.y > y) != (b.y > y) && x < (b.x - a.x) * (y - a.y) / (b.y - a.y) + a.x {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// Decodes DBF text: UTF-8 when the `.cpg` says so or the bytes are valid UTF-8, otherwise
/// Latin-1 (Windows-1252 and ISO-8859-1 agree on letters).
fn decode(bytes: &[u8], utf8: bool) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) if utf8 => String::from_utf8_lossy(bytes).into_owned(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

/// Parses a `.dbf` file into field definitions and records.
fn parse_dbf(d: &[u8], cpg: Option<&str>) -> Result<(Vec<FieldDef>, Vec<Vec<FieldValue>>)> {
    if d.len() < 32 {
        return Err(Error::InvalidData("truncated .dbf file".into()));
    }
    let utf8 = cpg.is_some_and(|c| {
        let c = c.trim().to_ascii_uppercase();
        c.contains("UTF-8") || c.contains("UTF8") || c == "65001"
    });
    let count = u32::from_le_bytes([d[4], d[5], d[6], d[7]]) as usize;
    let header_len = u16::from_le_bytes([d[8], d[9]]) as usize;
    let record_len = u16::from_le_bytes([d[10], d[11]]) as usize;
    let mut fields = Vec::new();
    let mut layout = Vec::new();
    let mut offset = 1;
    let mut pos = 32;
    while pos + 32 <= d.len().min(header_len) && d[pos] != 0x0D {
        let raw = &d[pos..pos + 11];
        let name = decode(&raw[..raw.iter().position(|&b| b == 0).unwrap_or(11)], utf8).trim().to_string();
        let kind = d[pos + 11];
        let width = d[pos + 16] as usize;
        let decimals = d[pos + 17] as usize;
        let t = match kind {
            b'N' | b'F' if decimals == 0 && width < 19 && kind == b'N' => FieldType::Integer,
            b'N' | b'F' | b'O' => FieldType::Float,
            b'I' => FieldType::Integer,
            b'L' => FieldType::Boolean,
            b'D' => FieldType::Date,
            _ => FieldType::Text,
        };
        let mut def = FieldDef::new(name, t).width(width);
        if t == FieldType::Float {
            def = def.precision(decimals);
        }
        fields.push(def);
        layout.push((offset, width, kind));
        offset += width;
        pos += 32;
    }
    let mut records = Vec::with_capacity(count);
    for i in 0..count {
        let start = header_len + i * record_len;
        let Some(rec) = d.get(start..start + record_len) else { break };
        let values = layout
            .iter()
            .zip(&fields)
            .map(|(&(off, width, kind), def)| {
                let raw = rec.get(off..(off + width).min(rec.len())).unwrap_or_default();
                if kind == b'I' && raw.len() == 4 {
                    return FieldValue::Integer(i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as i64);
                }
                let text = decode(raw, utf8);
                let v = text.trim_matches(|c: char| c == ' ' || c == '\0');
                if v.is_empty() || v.bytes().all(|b| b == b'*') {
                    return FieldValue::Null;
                }
                match def.field_type {
                    FieldType::Integer => v.parse().map(FieldValue::Integer).unwrap_or(FieldValue::Null),
                    FieldType::Float => v.parse().map(FieldValue::Float).unwrap_or(FieldValue::Null),
                    FieldType::Boolean => match v {
                        "T" | "t" | "Y" | "y" => FieldValue::Boolean(true),
                        "F" | "f" | "N" | "n" => FieldValue::Boolean(false),
                        _ => FieldValue::Null,
                    },
                    FieldType::Date if v.len() == 8 && v != "00000000" && v.bytes().all(|b| b.is_ascii_digit()) => {
                        FieldValue::Date(format!("{}-{}-{}", &v[..4], &v[4..6], &v[6..]))
                    }
                    FieldType::Date => FieldValue::Null,
                    // Text keeps leading spaces, as GDAL does.
                    _ => FieldValue::Text(text.trim_end_matches([' ', '\0']).to_string()),
                }
            })
            .collect();
        records.push(values);
    }
    Ok((fields, records))
}

/// Writes a layer as a shapefile. `path` names the `.shp` file; the other files are written next
/// to it.
pub fn write(layer: &Layer, path: &str) -> Result<()> {
    let base = Path::new(path).with_extension("");
    let (shp, shx) = encode_shapes(layer)?;
    std::fs::write(base.with_extension("shp"), shp)?;
    std::fs::write(base.with_extension("shx"), shx)?;
    std::fs::write(base.with_extension("dbf"), encode_dbf(layer)?)?;
    std::fs::write(base.with_extension("cpg"), "UTF-8")?;
    // The source definition when there is one; GDAL reads OGC WKT in `.prj` files too.
    let prj = super::output_wkt(layer);
    let prj_path = base.with_extension("prj");
    match prj {
        Some(wkt) => std::fs::write(prj_path, wkt)?,
        None if prj_path.exists() => std::fs::remove_file(prj_path)?,
        None => {}
    }
    Ok(())
}

/// The shape type for the layer's geometries, as GDAL chooses it.
fn shape_type(layer: &Layer) -> Result<i32> {
    let has_z = layer.features.iter().filter_map(|f| f.geometry.as_ref()).any(crate::vector::geometry_has_z);
    let base = match super::features_type(layer) {
        None if layer.features.iter().all(|f| f.geometry.is_none()) => return Ok(NULL_SHAPE),
        Some(GeometryType::Point) => 1,
        Some(GeometryType::MultiPoint) => 8,
        Some(GeometryType::LineString | GeometryType::MultiLineString) => 3,
        Some(GeometryType::Polygon | GeometryType::MultiPolygon) => 5,
        _ => {
            return Err(Error::Unsupported(format!(
                "a shapefile holds one geometry type, and layer '{}' mixes types or has collections",
                layer.name
            )));
        }
    };
    Ok(if has_z { base + 10 } else { base })
}

#[derive(Clone, Copy)]
struct Bounds([f64; 6]);

impl Bounds {
    fn empty() -> Self {
        Self([f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY])
    }
    fn add(&mut self, c: &Coord) {
        let b = &mut self.0;
        b[0] = b[0].min(c.x);
        b[1] = b[1].min(c.y);
        b[2] = b[2].max(c.x);
        b[3] = b[3].max(c.y);
        let z = c.z.unwrap_or(0.0);
        b[4] = b[4].min(z);
        b[5] = b[5].max(z);
    }
    fn or_zero(self) -> [f64; 6] {
        if self.0[0].is_finite() { self.0 } else { [0.0; 6] }
    }
}

/// Encodes the `.shp` and `.shx` files.
fn encode_shapes(layer: &Layer) -> Result<(Vec<u8>, Vec<u8>)> {
    let st = shape_type(layer)?;
    let has_z = st > 10;
    let mut total = Bounds::empty();
    let mut shp = vec![0u8; 100];
    let mut shx = vec![0u8; 100];
    for (i, f) in layer.features.iter().enumerate() {
        let rec = match &f.geometry {
            Some(g) => encode_shape(g, st, has_z, &mut total),
            None => NULL_SHAPE.to_le_bytes().to_vec(),
        };
        let offset = shp.len() / 2;
        if (shp.len() + 8 + rec.len()) / 2 > i32::MAX as usize {
            return Err(Error::Unsupported("the shapefile would be larger than the format's 4 GB limit".into()));
        }
        shx.extend_from_slice(&(offset as u32).to_be_bytes());
        shx.extend_from_slice(&((rec.len() / 2) as u32).to_be_bytes());
        shp.extend_from_slice(&((i + 1) as u32).to_be_bytes());
        shp.extend_from_slice(&((rec.len() / 2) as u32).to_be_bytes());
        shp.extend_from_slice(&rec);
    }
    let b = total.or_zero();
    for buf in [&mut shp, &mut shx] {
        let words = (buf.len() / 2) as u32;
        buf[0..4].copy_from_slice(&9994u32.to_be_bytes());
        buf[24..28].copy_from_slice(&words.to_be_bytes());
        buf[28..32].copy_from_slice(&1000u32.to_le_bytes());
        buf[32..36].copy_from_slice(&st.to_le_bytes());
        for (k, v) in [b[0], b[1], b[2], b[3], b[4], b[5], 0.0, 0.0].iter().enumerate() {
            buf[36 + 8 * k..44 + 8 * k].copy_from_slice(&v.to_le_bytes());
        }
    }
    Ok((shp, shx))
}

/// Closes a ring and orients it: clockwise for exterior rings, counter-clockwise for holes.
fn oriented(r: &Ring, exterior: bool) -> Vec<Coord> {
    let mut cs = r.0.clone();
    if cs.len() > 1 && (cs[0].x, cs[0].y) != (cs[cs.len() - 1].x, cs[cs.len() - 1].y) {
        cs.push(cs[0].clone());
    }
    let clockwise = Ring::new(cs.clone()).signed_area() < 0.0;
    if clockwise != exterior {
        cs.reverse();
    }
    cs
}

fn encode_shape(g: &Geometry, st: i32, has_z: bool, total: &mut Bounds) -> Vec<u8> {
    let parts: Vec<Vec<Coord>> = match g {
        Geometry::Point(c) => vec![vec![c.clone()]],
        Geometry::MultiPoint(cs) => vec![cs.clone()],
        Geometry::LineString(cs) => vec![cs.clone()],
        Geometry::MultiLineString(ls) => ls.clone(),
        Geometry::Polygon { exterior, interiors } => {
            std::iter::once(oriented(exterior, true)).chain(interiors.iter().map(|r| oriented(r, false))).collect()
        }
        Geometry::MultiPolygon(ps) => {
            ps.iter().flat_map(|(e, hs)| std::iter::once(oriented(e, true)).chain(hs.iter().map(|r| oriented(r, false)))).collect()
        }
        Geometry::GeometryCollection(_) => Vec::new(),
    };
    let parts: Vec<Vec<Coord>> = parts.into_iter().filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return NULL_SHAPE.to_le_bytes().to_vec();
    }
    let mut out = st.to_le_bytes().to_vec();
    let mut b = Bounds::empty();
    for c in parts.iter().flatten() {
        b.add(c);
        total.add(c);
    }
    let put = |out: &mut Vec<u8>, v: f64| out.extend_from_slice(&v.to_le_bytes());
    if st % 10 == 1 {
        let c = &parts[0][0];
        put(&mut out, c.x);
        put(&mut out, c.y);
        if has_z {
            put(&mut out, c.z.unwrap_or(0.0));
            put(&mut out, c.m.unwrap_or(0.0));
        }
        return out;
    }
    let b = b.0;
    for v in &b[..4] {
        put(&mut out, *v);
    }
    let points: Vec<&Coord> = parts.iter().flatten().collect();
    let multipoint = st % 10 == 8;
    if !multipoint {
        out.extend_from_slice(&(parts.len() as i32).to_le_bytes());
    }
    out.extend_from_slice(&(points.len() as i32).to_le_bytes());
    if !multipoint {
        let mut start = 0i32;
        for p in &parts {
            out.extend_from_slice(&start.to_le_bytes());
            start += p.len() as i32;
        }
    }
    for c in &points {
        put(&mut out, c.x);
        put(&mut out, c.y);
    }
    if has_z {
        put(&mut out, b[4]);
        put(&mut out, b[5]);
        for c in &points {
            put(&mut out, c.z.unwrap_or(0.0));
        }
        let ms: Vec<f64> = points.iter().map(|c| c.m.unwrap_or(0.0)).collect();
        put(&mut out, ms.iter().copied().fold(f64::INFINITY, f64::min));
        put(&mut out, ms.iter().copied().fold(f64::NEG_INFINITY, f64::max));
        for m in ms {
            put(&mut out, m);
        }
    }
    out
}

/// Field names as GDAL launders them: at most 10 bytes, unique ignoring case.
pub(crate) fn dbf_field_names(names: &[String]) -> Vec<String> {
    let truncate = |s: &str, n: usize| -> String {
        let mut end = s.len().min(n);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    };
    let mut out: Vec<String> = Vec::new();
    for name in names {
        let taken = |c: &str, out: &[String]| out.iter().any(|o| o.eq_ignore_ascii_case(c));
        let mut candidate = truncate(name, 10);
        let mut n = 1;
        while taken(&candidate, &out) && n < 10 {
            candidate = format!("{}_{n}", truncate(name, 8));
            n += 1;
        }
        while taken(&candidate, &out) && n < 100 {
            candidate = format!("{}{n:02}", truncate(name, 8));
            n += 1;
        }
        out.push(candidate);
    }
    out
}

struct DbfField {
    name: String,
    kind: u8,
    width: usize,
    decimals: usize,
}

fn dbf_fields(layer: &Layer) -> Vec<DbfField> {
    let names: Vec<String> = layer.schema.fields().iter().map(|f| f.name.clone()).collect();
    let names = dbf_field_names(&names);
    layer
        .schema
        .fields()
        .iter()
        .enumerate()
        .zip(names)
        .map(|((i, f), name)| {
            let values = || layer.features.iter().filter_map(move |ft| ft.attributes.get(i)).filter(|v| !v.is_null());
            let (kind, width, decimals) = match f.field_type {
                FieldType::Integer => {
                    let longest = values().filter_map(|v| v.as_i64()).map(|v| v.to_string().len()).max().unwrap_or(0);
                    (b'N', if longest <= 9 { 9 } else { longest.max(18) }, 0)
                }
                FieldType::Float => (b'N', 24, 15),
                FieldType::Boolean => (b'L', 1, 0),
                FieldType::Date => (b'D', 8, 0),
                _ => {
                    let longest = values().map(|v| text(v).len()).max().unwrap_or(0);
                    let declared = if f.width > 0 { f.width } else { 80 };
                    (b'C', declared.max(longest).clamp(1, 254), 0)
                }
            };
            DbfField { name, kind, width, decimals }
        })
        .collect()
}

fn text(v: &FieldValue) -> String {
    match v {
        FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => s.clone(),
        FieldValue::Integer(x) => x.to_string(),
        FieldValue::Float(x) => x.to_string(),
        FieldValue::Boolean(b) => b.to_string(),
        FieldValue::Blob(b) => b.iter().map(|x| format!("{x:02X}")).collect(),
        FieldValue::Null => String::new(),
    }
}

/// A real number in a numeric field: 15 decimals when they fit, fewer or an exponent otherwise.
fn format_real(v: f64, width: usize, decimals: usize) -> String {
    for d in (0..=decimals).rev() {
        let s = format!("{v:.d$}");
        if s.len() <= width {
            return s;
        }
    }
    let s = format!("{v:.e$e}", e = width.saturating_sub(8).min(15));
    s.chars().take(width).collect()
}

/// Encodes the `.dbf` file.
fn encode_dbf(layer: &Layer) -> Result<Vec<u8>> {
    let fields = dbf_fields(layer);
    let record_len = 1 + fields.iter().map(|f| f.width).sum::<usize>();
    let header_len = 32 + 32 * fields.len() + 1;
    if record_len > 65535 || header_len > 65535 {
        return Err(Error::Unsupported("too many fields for a shapefile".into()));
    }
    let mut out = Vec::with_capacity(header_len + record_len * layer.len() + 1);
    out.push(0x03);
    out.extend_from_slice(&[125, 1, 1]); // last update (2025-01-01): a fixed date keeps outputs reproducible
    out.extend_from_slice(&(layer.len() as u32).to_le_bytes());
    out.extend_from_slice(&(header_len as u16).to_le_bytes());
    out.extend_from_slice(&(record_len as u16).to_le_bytes());
    out.extend_from_slice(&[0u8; 20]);
    for f in &fields {
        let mut name = [0u8; 11];
        name[..f.name.len()].copy_from_slice(f.name.as_bytes());
        out.extend_from_slice(&name);
        out.push(f.kind);
        out.extend_from_slice(&[0u8; 4]);
        out.push(f.width as u8);
        out.push(f.decimals as u8);
        out.extend_from_slice(&[0u8; 14]);
    }
    out.push(0x0D);
    for feat in &layer.features {
        out.push(b' ');
        for (i, f) in fields.iter().enumerate() {
            let v = feat.attributes.get(i).unwrap_or(&FieldValue::Null);
            let cell: Vec<u8> = match (f.kind, v) {
                (b'N', FieldValue::Null) => vec![b'*'; f.width],
                (b'D', FieldValue::Null) => vec![b'0'; f.width],
                (b'L', FieldValue::Null) => vec![b'?'],
                (_, FieldValue::Null) => vec![b' '; f.width],
                (b'N', v) => {
                    let s = if f.decimals == 0 {
                        v.as_i64().or_else(|| v.as_f64().map(|x| x.round() as i64)).map(|x| x.to_string()).unwrap_or_default()
                    } else {
                        v.as_f64().map(|x| format_real(x, f.width, f.decimals)).unwrap_or_default()
                    };
                    format!("{s:>w$}", w = f.width).into_bytes()
                }
                (b'L', v) => vec![if v.as_bool().unwrap_or(false) { b'T' } else { b'F' }],
                (b'D', v) => {
                    let digits: String = text(v).chars().filter(char::is_ascii_digit).take(8).collect();
                    format!("{digits:0<8}").into_bytes()
                }
                (_, v) => {
                    let s = text(v);
                    let mut end = s.len().min(f.width);
                    while !s.is_char_boundary(end) {
                        end -= 1;
                    }
                    let mut b = s.as_bytes()[..end].to_vec();
                    b.resize(f.width, b' ');
                    b
                }
            };
            out.extend_from_slice(&cell[..f.width.min(cell.len())]);
            out.extend(std::iter::repeat_n(b' ', f.width.saturating_sub(cell.len())));
        }
    }
    out.push(0x1A);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbvector::{Feature, FieldDef};

    #[test]
    fn launders_names_like_gdal() {
        let names: Vec<String> = ["population_total", "population_totals", "population_totalz", "name", "NAME"].map(String::from).into();
        assert_eq!(dbf_field_names(&names), ["population", "populati_1", "populati_2", "name", "NAME_1"]);
    }

    #[test]
    fn reads_back_z_holes_and_latin1() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.shp");
        let path = path.to_str().unwrap();
        let mut l = Layer::new("t");
        l.add_field(FieldDef::new("name", FieldType::Text));
        let ring = |x: f64, s: f64| {
            Ring::new(vec![Coord::xyz(x, 0.0, 1.0), Coord::xyz(x + s, 0.0, 2.0), Coord::xyz(x + s, s, 3.0), Coord::xyz(x, s, 4.0)])
        };
        let g = Geometry::MultiPolygon(vec![(ring(0.0, 10.0), vec![ring(1.0, 2.0)]), (ring(20.0, 5.0), vec![])]);
        l.push(Feature { fid: 0, geometry: Some(g.clone()), attributes: vec![FieldValue::Text("Zürich".into())] });
        write(&l, path).unwrap();
        let back = read(path).unwrap();
        let Some(Geometry::MultiPolygon(ps)) = &back.features[0].geometry else { panic!("not a multipolygon") };
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0].1.len(), 1);
        assert_eq!(ps[0].0.0.iter().map(|c| c.z.unwrap()).sum::<f64>(), 10.0);
        assert_eq!(back.features[0].attributes[0], FieldValue::Text("Zürich".into()));
        // Without a .cpg, text that is not UTF-8 is Latin-1.
        std::fs::remove_file(dir.path().join("t.cpg")).unwrap();
        let dbf = dir.path().join("t.dbf");
        let bytes = std::fs::read(&dbf).unwrap();
        let at = bytes.windows(7).position(|w| w == "Zürich".as_bytes()).unwrap();
        let mut latin1 = bytes[..at].to_vec();
        latin1.extend_from_slice(b"Z\xfcrich ");
        latin1.extend_from_slice(&bytes[at + 7..]);
        std::fs::write(&dbf, latin1).unwrap();
        assert_eq!(read(path).unwrap().features[0].attributes[0], FieldValue::Text("Zürich".into()));
    }

    #[test]
    fn orients_rings() {
        let square = Ring::new(vec![Coord::xy(0.0, 0.0), Coord::xy(1.0, 0.0), Coord::xy(1.0, 1.0), Coord::xy(0.0, 1.0)]);
        let ext = oriented(&square, true);
        assert_eq!(ext.len(), 5);
        assert!(Ring::new(ext).signed_area() < 0.0);
        assert!(Ring::new(oriented(&square, false)).signed_area() > 0.0);
    }

    #[test]
    fn writes_dbf_like_gdal() {
        let mut l = Layer::new("t");
        for (n, t) in
            [("n", FieldType::Integer), ("x", FieldType::Float), ("s", FieldType::Text), ("d", FieldType::Date), ("b", FieldType::Boolean)]
        {
            l.add_field(FieldDef::new(n, t));
        }
        let attrs = vec![
            FieldValue::Integer(-12),
            FieldValue::Float(1.5),
            FieldValue::Text("Zürich".into()),
            FieldValue::Date("2020-01-02".into()),
            FieldValue::Boolean(true),
        ];
        l.push(Feature { fid: 0, geometry: Some(Geometry::point(1.0, 2.0)), attributes: attrs });
        l.push(Feature { fid: 1, geometry: None, attributes: vec![FieldValue::Null; 5] });
        let dbf = encode_dbf(&l).unwrap();
        let rec = &dbf[32 + 5 * 32 + 1..];
        let rec_len = 1 + 9 + 24 + 80 + 8 + 1;
        let first = String::from_utf8_lossy(&rec[..rec_len]).into_owned();
        assert!(first.starts_with("       -12       1.500000000000000Zürich"), "{first}");
        assert!(first.ends_with("20200102T"));
        let second = &rec[rec_len..2 * rec_len];
        assert_eq!(&second[1..10], b"*********");
        let (shp, shx) = encode_shapes(&l).unwrap();
        assert_eq!(shx.len(), 100 + 16);
        assert_eq!(i32::from_le_bytes(shp[32..36].try_into().unwrap()), 1);
    }
}
