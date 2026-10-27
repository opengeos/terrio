//! Esri Shapefile: `.shp`, `.shx`, `.dbf`, `.cpg` and `.prj`.
//!
//! Reading keeps Z and M values, groups polygon rings into polygons by orientation and
//! containment, and decodes text as the `.cpg` file (or the DBF language driver) declares, falling
//! back to Latin-1 for text that is not UTF-8.
//!
//! Writing stores text as UTF-8, declared in a `.cpg` file. Field names are truncated to 10 bytes
//! and made unique the way `ogr2ogr` does it (`populati_1`, `NAME2`), and field widths follow GDAL's: 9 or 18
//! digits for integers, 24 with 15 decimals for reals, 80 characters (or the longest value) for
//! strings.

use super::profile::LayerProfile;
use crate::crs::SpatialRef;
use crate::{Error, Result};
use std::collections::HashMap;
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
    let dbf = sidecar(&base, "dbf").map(std::fs::read).transpose()?;
    let prj = sidecar(&base, "prj").and_then(|p| std::fs::read_to_string(p).ok());
    if prj.is_some() {
        crate::crs::prefetch();
    }
    let cpg = sidecar(&base, "cpg").and_then(|p| std::fs::read_to_string(p).ok());
    let name = base.file_name().and_then(|s| s.to_str()).unwrap_or("layer");
    parse(&shp, dbf.as_deref(), prj.as_deref(), cpg.as_deref(), name).map_err(|e| match e {
        Error::InvalidData(m) if m == "not a shapefile" => Error::InvalidData(format!("'{path}' is not a shapefile")),
        e => e,
    })
}

/// Reads a shapefile from the contents of its `.shp`, `.dbf`, `.prj` and `.cpg` files. `name`
/// names the layer.
pub fn parse(shp: &[u8], dbf: Option<&[u8]>, prj: Option<&str>, cpg: Option<&str>, name: &str) -> Result<Layer> {
    let mut layer = header_layer(shp, name)?;

    let mut geometries = Vec::new();
    let mut pos = 100;
    while pos + 8 <= shp.len() {
        let words = u32::from_be_bytes([shp[pos + 4], shp[pos + 5], shp[pos + 6], shp[pos + 7]]) as usize;
        let end = (pos + 8 + 2 * words).min(shp.len());
        geometries.push(parse_shape(&shp[pos + 8..end]));
        pos = end;
    }

    let (fields, records) = match dbf {
        Some(d) => parse_dbf(d, cpg)?,
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
    apply_prj(&mut layer, prj);
    Ok(layer)
}

/// The layer of a `.shp` header (its first 100 bytes), without fields or features.
pub(crate) fn header_layer(shp: &[u8], name: &str) -> Result<Layer> {
    if shp.len() < 100 || u32::from_be_bytes([shp[0], shp[1], shp[2], shp[3]]) != 9994 {
        return Err(Error::InvalidData("not a shapefile".into()));
    }
    let shape_type = i32::from_le_bytes([shp[32], shp[33], shp[34], shp[35]]);
    let mut layer = Layer::new(name);
    layer.geom_type = match if shape_type == 31 { 0 } else { shape_type % 10 } {
        1 => Some(GeometryType::Point),
        3 => Some(GeometryType::LineString),
        5 => Some(GeometryType::Polygon),
        8 => Some(GeometryType::MultiPoint),
        _ => None,
    };
    Ok(layer)
}

/// Sets a layer's CRS from a `.prj` file, as GDAL's Shapefile driver does (FindBestMatch()):
/// the EPSG CRS the `.prj` is identified as, and otherwise its definition.
pub(crate) fn apply_prj(layer: &mut Layer, prj: Option<&str>) {
    if let Some(prj) = prj.filter(|s| !s.trim().is_empty()) {
        match SpatialRef::from_wkt(prj.trim()).epsg.and_then(|code| SpatialRef::from_epsg(code).ok()) {
            Some(s) => s.apply_to_layer(layer),
            None => {
                SpatialRef::from_wkt(prj.trim()).apply_to_layer(layer);
                layer.set_crs_wkt(Some(prj.trim().to_string()));
            }
        }
    }
}

/// Decodes a `.shp` record's content (after its 8-byte record header).
pub(crate) fn decode_shape(rec: &[u8]) -> Option<Geometry> {
    parse_shape(rec)
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
    // GDAL organizes, and since 3.13 splits self-touching rings, only in records of several parts.
    let several = parts.len() > 1;
    let mut rings: Vec<Ring> = Vec::with_capacity(parts.len());
    let mut split_loops: Vec<Ring> = Vec::new();
    for mut cs in parts {
        if cs.len() > 1 && (cs[0].x, cs[0].y) == (cs[cs.len() - 1].x, cs[cs.len() - 1].y) {
            cs.pop();
        }
        let (ring, loops) = if several { split_self_touching(cs) } else { (cs, Vec::new()) };
        rings.push(Ring::new(ring));
        split_loops.extend(loops.into_iter().map(Ring::new));
    }
    // After the stored rings, where GDAL puts them.
    rings.extend(split_loops);
    let (mut exteriors, mut holes): (Vec<Ring>, Vec<Ring>) = rings.into_iter().partition(|r| r.signed_area() <= 0.0);
    if exteriors.is_empty() {
        // Wrongly wound: take every ring as an exterior.
        exteriors = std::mem::take(&mut holes);
    }
    let mut polygons: Vec<(Ring, Vec<Ring>)> = exteriors.into_iter().map(|r| (r, Vec::new())).collect();
    for h in holes {
        // Tested at the midpoint of the hole's first edge (to the first vertex that differs, as
        // the first may be repeated): a vertex of a hole may lie on the exterior (a hole may touch
        // it), where the even-odd test is unreliable.
        let Some(a) = h.0.first() else { continue };
        let b = h.0.iter().find(|b| (b.x, b.y) != (a.x, a.y)).unwrap_or(a);
        let (x, y) = ((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
        let owner = polygons
            .iter()
            .enumerate()
            .filter(|(_, (e, _))| ring_contains(e, x, y))
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

/// Splits off the loops of an Esri ring that touches itself at a vertex to enclose them. Esri
/// accepts such a ring; OGC simple features don't. Since 3.13, GDAL makes it valid in
/// `organizePolygons` when GEOS finds it valid under `ALLOW_SELFTOUCHING_RING_FORMING_HOLE`.
///
/// `cs` is an open ring, without its closing vertex. It is cut into cycles at each vertex it
/// passes twice: the loops between two occurrences of a vertex, and the rest, which holds the
/// stored start. The cycle with the largest area is the outer one, wherever the ring starts; the
/// ring is split when every other cycle has an area, winds the other way round and lies inside
/// it: holes of an exterior, or islands inside a hole part. The outer cycle and the others are
/// returned. Otherwise (a figure-eight, a loop outside, a spike) the ring is returned as it is,
/// with no others, as it is when it never touches itself. Consecutive duplicate points are not
/// loops.
///
/// The rings start where GEOS's polygonizer, which rebuilds them for GDAL, starts them: one vertex
/// after the stored start for the rest when it is the outer cycle, and otherwise one vertex after
/// the vertex a cycle hangs from. When the rest is inside, it comes after the loops.
fn split_self_touching(cs: Vec<Coord>) -> (Vec<Coord>, Vec<Vec<Coord>>) {
    // `+ 0.0` makes -0.0 and 0.0 the same vertex, as GDAL's comparison of doubles does.
    let key = |c: &Coord| ((c.x + 0.0).to_bits(), (c.y + 0.0).to_bits());
    let mut path: Vec<Coord> = Vec::with_capacity(cs.len());
    let mut at: HashMap<(u64, u64), usize> = HashMap::with_capacity(cs.len());
    let mut loops: Vec<Vec<Coord>> = Vec::new();
    for c in &cs {
        match at.get(&key(c)) {
            Some(&i) if i + 1 == path.len() => {}
            Some(&i) => {
                let mut lp = vec![path[i].clone()];
                lp.extend(path.split_off(i + 1));
                for removed in &lp[1..] {
                    at.remove(&key(removed));
                }
                loops.push(lp);
            }
            None => {
                at.insert(key(c), path.len());
                path.push(c.clone());
            }
        }
    }
    if loops.is_empty() {
        return (cs, Vec::new());
    }
    // The rest first, then the loops, each starting at the vertex it hangs from.
    let mut cycles = Vec::with_capacity(loops.len() + 1);
    cycles.push(path);
    cycles.append(&mut loops);
    let areas: Vec<f64> = cycles.iter().map(|c| Ring::new(c.clone()).signed_area()).collect();
    let Some(outer_i) = (0..cycles.len()).max_by(|&a, &b| areas[a].abs().total_cmp(&areas[b].abs())) else {
        return (cs, Vec::new());
    };
    let outer = areas[outer_i];
    let split = {
        let shell = Ring::new(cycles[outer_i].clone());
        // Tested at the midpoint of a cycle's first edge: its first vertex may lie on the outer
        // cycle, and no cycle holds consecutive duplicates, so the edge has a length.
        let inside = |i: usize| {
            let c = &cycles[i];
            c.len() >= 3
                && areas[i] != 0.0
                && (areas[i] > 0.0) != (outer > 0.0)
                && ring_contains(&shell, (c[0].x + c[1].x) / 2.0, (c[0].y + c[1].y) / 2.0)
        };
        outer != 0.0 && cycles[outer_i].len() >= 3 && (0..cycles.len()).filter(|&i| i != outer_i).all(inside)
    };
    if !split {
        return (cs, Vec::new());
    }
    let mut rest = cycles.remove(0);
    for lp in &mut cycles {
        lp.rotate_left(1);
    }
    if outer_i == 0 {
        rest.rotate_left(1);
        return (rest, cycles);
    }
    // The outer cycle is a loop, as when the stored ring starts inside a hole: the rest hangs from
    // the outer cycle's vertex, now its last, and starts one vertex after it.
    let outer = cycles.remove(outer_i - 1);
    if let Some(k) = outer.last().and_then(|t| rest.iter().position(|c| (c.x, c.y) == (t.x, t.y))) {
        rest.rotate_left(k + 1);
    }
    cycles.push(rest);
    (outer, cycles)
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

/// The fields and record layout of a `.dbf` file.
pub(crate) struct DbfLayout {
    /// The fields.
    pub fields: Vec<FieldDef>,
    layout: Vec<(usize, usize, u8)>,
    utf8: bool,
    /// The number of records the header declares.
    pub count: usize,
    /// The length of the header, where the records start.
    pub header_len: usize,
    /// The length of a record.
    pub record_len: usize,
}

/// The length of a `.dbf` header from its first 32 bytes.
pub(crate) fn dbf_header_len(d: &[u8]) -> Result<usize> {
    if d.len() < 32 {
        return Err(Error::InvalidData("truncated .dbf file".into()));
    }
    Ok(u16::from_le_bytes([d[8], d[9]]) as usize)
}

/// Reads a `.dbf` header (`d` holds at least the header).
pub(crate) fn dbf_layout(d: &[u8], cpg: Option<&str>) -> Result<DbfLayout> {
    let header_len = dbf_header_len(d)?;
    let utf8 = cpg.is_some_and(|c| {
        let c = c.trim().to_ascii_uppercase();
        c.contains("UTF-8") || c.contains("UTF8") || c == "65001"
    });
    let count = u32::from_le_bytes([d[4], d[5], d[6], d[7]]) as usize;
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
    Ok(DbfLayout { fields, layout, utf8, count, header_len, record_len })
}

impl DbfLayout {
    /// Decodes a record.
    pub(crate) fn record(&self, rec: &[u8]) -> Vec<FieldValue> {
        let utf8 = self.utf8;
        self.layout
            .iter()
            .zip(&self.fields)
            .map(|(&(off, width, kind), def)| {
                let raw = rec.get(off..(off + width).min(rec.len())).unwrap_or_default();
                if kind == b'I' && raw.len() == 4 {
                    return FieldValue::Integer(i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as i64);
                }
                let text = decode(raw, utf8);
                let v = text.trim_matches(|c: char| c == ' ' || c == '\0');
                if v.is_empty() || (matches!(kind, b'N' | b'F') && v.bytes().all(|b| b == b'*')) {
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
            .collect()
    }
}

/// Parses a `.dbf` file into field definitions and records.
fn parse_dbf(d: &[u8], cpg: Option<&str>) -> Result<(Vec<FieldDef>, Vec<Vec<FieldValue>>)> {
    let l = dbf_layout(d, cpg)?;
    // Records are decoded in parallel, up to the first one the file does not hold whole.
    let records = crate::par::map_range(l.count, |i| {
        let start = l.header_len + i * l.record_len;
        let rec = d.get(start..start + l.record_len)?;
        Some(l.record(rec))
    });
    let records = records.into_iter().map_while(|r| r).collect();
    Ok((l.fields, records))
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

/// Writes one layer to a ZIP containing `.shp`, `.shx`, `.dbf`, `.cpg` and,
/// when present, `.prj`. Pure Rust on native and WebAssembly, without filesystem I/O.
/// Save the returned bytes as `{stem}.shp.zip`, MIME type `application/zip`.
///
/// `stem` must be a single nonempty filename stem. Point/MultiPoint, line and
/// polygon (including multipart) geometries support XY and Z; M is refused.
/// Fields support text, integer, finite real, boolean, date and null values. DBF
/// names are laundered to ten bytes; reals use width 24 and 15 decimal places,
/// rounding to that precision (tiny magnitudes can become zero). Reals that do not
/// fit that fixed layout are refused; convert them to text explicitly.
/// Text over 254 UTF-8 bytes, empty/trailing-space/NUL text, integers requiring
/// over 18 decimal columns, and non-scalar/blob/datetime fields are refused rather
/// than silently losing data. Convert such fields explicitly before writing.
pub fn to_zip_bytes(layer: &Layer, stem: &str) -> Result<Vec<u8>> {
    use std::io::{Cursor, Write};
    if stem.is_empty() || matches!(stem, "." | "..") || stem.contains(['/', '\\', '\0', ':']) {
        return Err(Error::InvalidArgument("Shapefile archive stem must be a single nonempty filename stem".into()));
    }
    let profile = LayerProfile::of(layer);
    if profile.geometry.has_m {
        return Err(Error::Unsupported("Shapefile ZIP writing does not support M coordinates".into()));
    }
    if layer.features.iter().any(|f| f.attributes.len() > layer.schema.len()) {
        return Err(Error::InvalidArgument("Shapefile feature has more attributes than its schema".into()));
    }
    let fields = dbf_fields(layer, &profile);
    for (i, field) in layer.schema.fields().iter().enumerate() {
        let refuse = |reason: &str| Error::Unsupported(format!("Shapefile archive field '{}': {reason}", field.name));
        if !matches!(field.field_type, FieldType::Text | FieldType::Integer | FieldType::Float | FieldType::Boolean | FieldType::Date) {
            return Err(refuse("supports text, integer, finite real, boolean and date scalars only; convert explicitly"));
        }
        if field.field_type == FieldType::Integer && fields[i].width > 18 {
            return Err(refuse("integers requiring more than 18 decimal columns must be converted to text"));
        }
        for feature in &layer.features {
            let value = feature.attributes.get(i).unwrap_or(&FieldValue::Null);
            let valid = match (field.field_type, value) {
                (_, FieldValue::Null) => true,
                (FieldType::Text, FieldValue::Text(text)) => {
                    if text.len() > 254 {
                        return Err(refuse("text exceeds the DBF limit of 254 UTF-8 bytes; use a sidecar or another format"));
                    }
                    if text.is_empty() || text.ends_with(' ') || text.contains('\0') {
                        return Err(refuse("empty, trailing-space or NUL text cannot round-trip through DBF"));
                    }
                    true
                }
                (FieldType::Integer, FieldValue::Integer(_)) | (FieldType::Boolean, FieldValue::Boolean(_)) => true,
                (FieldType::Float, FieldValue::Float(x)) => {
                    if x.is_finite() && format!("{x:.15}").len() > 24 {
                        return Err(refuse("real exceeds the DBF width of 24 columns at 15 decimal places; convert to text"));
                    }
                    x.is_finite()
                }
                (FieldType::Date, FieldValue::Date(text)) => super::datetime::parse_date(text).is_some(),
                _ => false,
            };
            if !valid {
                return Err(refuse("value does not match its scalar field type or is not finite/a valid date"));
            }
        }
    }
    let (shp, shx) = encode_shapes(layer)?;
    let mut members = vec![("shp", shp), ("shx", shx), ("dbf", encode_dbf(layer)?), ("cpg", b"UTF-8".to_vec())];
    if let Some(wkt) = super::output_wkt(layer) {
        members.push(("prj", wkt.into_bytes()));
    }
    members.sort_by_key(|(ext, _)| *ext);
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (ext, bytes) in members {
        zip.start_file(format!("{stem}.{ext}"), opts)?;
        zip.write_all(&bytes)?;
    }
    Ok(zip.finish()?.into_inner())
}

/// The shape type for the layer's geometries, as GDAL chooses it.
fn shape_type(layer: &Layer, profile: &LayerProfile) -> Result<i32> {
    let has_z = profile.geometry.has_z;
    let base = match profile.features_type() {
        None if profile.no_geometry() => return Ok(NULL_SHAPE),
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
pub(crate) struct Bounds([f64; 6]);

impl Bounds {
    pub(crate) fn empty() -> Self {
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
    let enc = ShpEncoder::new(layer, &LayerProfile::of(layer))?;
    let mut total = Bounds::empty();
    let mut shp = vec![0u8; 100];
    let mut shx = vec![0u8; 100];
    for (i, f) in layer.features.iter().enumerate() {
        let (rec, idx) = enc.record(i as u64, f, shp.len() as u64, &mut total)?;
        shx.extend_from_slice(&idx);
        shp.extend_from_slice(&rec);
    }
    let (shp_len, shx_len) = (shp.len() as u64, shx.len() as u64);
    shp[..100].copy_from_slice(&enc.header(shp_len, &total));
    shx[..100].copy_from_slice(&enc.header(shx_len, &total));
    Ok((shp, shx))
}

/// Encodes a layer's features as shapefile records (`.shp`, `.shx` and `.dbf`), with the shape
/// type and field widths its profile decides.
pub(crate) struct ShpEncoder {
    st: i32,
    has_z: bool,
    fields: Vec<DbfField>,
}

impl ShpEncoder {
    /// An encoder for the features of `layer` (its fields), which `profile` describes.
    pub(crate) fn new(layer: &Layer, profile: &LayerProfile) -> Result<Self> {
        let st = shape_type(layer, profile)?;
        let fields = dbf_fields(layer, profile);
        let record_len = 1 + fields.iter().map(|f| f.width).sum::<usize>();
        let header_len = 32 + 32 * fields.len() + 1;
        if record_len > 65535 || header_len > 65535 {
            return Err(Error::Unsupported("too many fields for a shapefile".into()));
        }
        Ok(Self { st, has_z: st > 10, fields })
    }

    /// The `.shp` record of the `i`-th feature (from 0), written at byte `offset` of the file,
    /// with its `.shx` entry. Adds the feature's coordinates to `total`.
    pub(crate) fn record(&self, i: u64, f: &Feature, offset: u64, total: &mut Bounds) -> Result<(Vec<u8>, [u8; 8])> {
        let rec = match &f.geometry {
            Some(g) => encode_shape(g, self.st, self.has_z, total),
            None => NULL_SHAPE.to_le_bytes().to_vec(),
        };
        if (offset as usize + 8 + rec.len()) / 2 > i32::MAX as usize {
            return Err(Error::Unsupported("the shapefile would be larger than the format's 4 GB limit".into()));
        }
        let mut idx = [0u8; 8];
        idx[..4].copy_from_slice(&((offset / 2) as u32).to_be_bytes());
        idx[4..].copy_from_slice(&((rec.len() / 2) as u32).to_be_bytes());
        let mut out = Vec::with_capacity(rec.len() + 8);
        out.extend_from_slice(&((i + 1) as u32).to_be_bytes());
        out.extend_from_slice(&((rec.len() / 2) as u32).to_be_bytes());
        out.extend_from_slice(&rec);
        Ok((out, idx))
    }

    /// The 100-byte header of a `.shp` or `.shx` file of `len` bytes whose shapes are within
    /// `total`.
    pub(crate) fn header(&self, len: u64, total: &Bounds) -> [u8; 100] {
        let b = total.or_zero();
        let mut buf = [0u8; 100];
        buf[0..4].copy_from_slice(&9994u32.to_be_bytes());
        buf[24..28].copy_from_slice(&((len / 2) as u32).to_be_bytes());
        buf[28..32].copy_from_slice(&1000u32.to_le_bytes());
        buf[32..36].copy_from_slice(&self.st.to_le_bytes());
        for (k, v) in [b[0], b[1], b[2], b[3], b[4], b[5], 0.0, 0.0].iter().enumerate() {
            buf[36 + 8 * k..44 + 8 * k].copy_from_slice(&v.to_le_bytes());
        }
        buf
    }

    /// The `.dbf` header of a file of `count` records.
    pub(crate) fn dbf_header(&self, count: u64) -> Vec<u8> {
        let fields = &self.fields;
        let record_len = 1 + fields.iter().map(|f| f.width).sum::<usize>();
        let header_len = 32 + 32 * fields.len() + 1;
        let mut out = Vec::with_capacity(header_len);
        out.push(0x03);
        out.extend_from_slice(&[125, 1, 1]); // last update (2025-01-01): a fixed date keeps outputs reproducible
        out.extend_from_slice(&(count as u32).to_le_bytes());
        out.extend_from_slice(&(header_len as u16).to_le_bytes());
        out.extend_from_slice(&(record_len as u16).to_le_bytes());
        out.extend_from_slice(&[0u8; 20]);
        for f in fields {
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
        out
    }

    /// Appends a feature's `.dbf` record.
    pub(crate) fn dbf_record(&self, feat: &Feature, out: &mut Vec<u8>) {
        out.push(b' ');
        for (i, f) in self.fields.iter().enumerate() {
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
    let measure = |c: &Coord| c.m.filter(|m| m.is_finite() && *m > -1e38);
    if st % 10 == 1 {
        let c = &parts[0][0];
        put(&mut out, c.x);
        put(&mut out, c.y);
        if has_z {
            put(&mut out, c.z.unwrap_or(0.0));
            if let Some(m) = measure(c) {
                put(&mut out, m);
            }
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
        let ms: Vec<f64> = points.iter().filter_map(|c| measure(c)).collect();
        // Z-only records omit the optional M section, as GDAL does. When real
        // measures exist, missing samples do not participate in the M range.
        if !ms.is_empty() {
            put(&mut out, ms.iter().copied().fold(f64::INFINITY, f64::min));
            put(&mut out, ms.iter().copied().fold(f64::NEG_INFINITY, f64::max));
            for c in points {
                put(&mut out, measure(c).unwrap_or(-1e39));
            }
        }
    }
    out
}

/// Field names as `ogr2ogr` writes them into a shapefile, unique ignoring case.
///
/// First `ogr2ogr`'s renaming: a name already taken (ignoring case) gets the first number suffix,
/// from 2, that makes it unique among the names written and the source's names (`NAME` after
/// `name` becomes `NAME2`, or `NAME3` when the source has a `name2`; the next `Name`, `Name4`).
/// Then the Shapefile driver's: at most 10 bytes, trailing spaces trimmed and `:` replaced by
/// `_`, and a name taken by then made unique as `populati_1` to `populati_9`, then
/// `populati10` to `populati99`.
pub(crate) fn dbf_field_names(names: &[String]) -> Vec<String> {
    let truncate = |s: &str, n: usize| -> String {
        let mut end = s.len().min(n);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    };
    let upper = |s: &str| s.to_ascii_uppercase();
    let sources: std::collections::HashSet<String> = names.iter().map(|n| upper(n)).collect();
    let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut last_suffix: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut out: Vec<String> = Vec::new();
    for name in names {
        // ogr2ogr: a numbered name, when the name is taken.
        let mut name = name.clone();
        let radix = upper(&name);
        if taken.contains(&radix) {
            let mut n = last_suffix.get(&radix).copied().unwrap_or(1);
            loop {
                n += 1;
                let candidate = format!("{radix}{n}");
                if !taken.contains(&candidate) && !sources.contains(&candidate) {
                    name = format!("{name}{n}");
                    last_suffix.insert(radix, n);
                    break;
                }
            }
        }
        // The driver: truncated, trimmed and normalized, then made unique.
        let radix = truncate(&name, 10).trim_end_matches(|c: char| c.is_ascii_whitespace()).replace(':', "_");
        let mut candidate = radix.clone();
        let mut n = 1;
        while taken.contains(&upper(&candidate)) && n < 10 {
            candidate = format!("{}_{n}", truncate(&radix, 8));
            n += 1;
        }
        while taken.contains(&upper(&candidate)) && n < 100 {
            candidate = format!("{}{n:02}", truncate(&radix, 8));
            n += 1;
        }
        taken.insert(upper(&candidate));
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

fn dbf_fields(layer: &Layer, profile: &LayerProfile) -> Vec<DbfField> {
    let names: Vec<String> = layer.schema.fields().iter().map(|f| f.name.clone()).collect();
    let names = dbf_field_names(&names);
    layer
        .schema
        .fields()
        .iter()
        .enumerate()
        .zip(names)
        .map(|((i, f), name)| {
            let p = profile.fields.get(i).cloned().unwrap_or_default();
            let (kind, width, decimals) = match f.field_type {
                FieldType::Integer => {
                    let longest = p.longest_int;
                    (b'N', if longest <= 9 { 9 } else { longest.max(18) }, 0)
                }
                FieldType::Float => (b'N', 24, 15),
                FieldType::Boolean => (b'L', 1, 0),
                FieldType::Date => (b'D', 8, 0),
                _ => {
                    let longest = p.longest_text;
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
    let enc = ShpEncoder::new(layer, &LayerProfile::of(layer))?;
    let mut out = enc.dbf_header(layer.len() as u64);
    for feat in &layer.features {
        enc.dbf_record(feat, &mut out);
    }
    out.push(0x1A);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbvector::{Feature, FieldDef};

    #[test]
    fn measure_ranges_exclude_missing_samples() {
        let coordinates = vec![
            Coord { x: 0.0, y: 0.0, z: Some(1.0), m: Some(3.0) },
            Coord { x: 1.0, y: 1.0, z: Some(2.0), m: None },
            Coord { x: 2.0, y: 2.0, z: Some(3.0), m: Some(8.0) },
        ];
        let shape = Geometry::LineString(coordinates);
        let record = encode_shape(&shape, 13, true, &mut Bounds::empty());
        // One part, three XY/Z coordinates; M range starts after the Z array.
        let at = 60 + 4 + 24 * 3;
        let value = |i| f64::from_le_bytes(record[i..i + 8].try_into().unwrap());
        assert_eq!((value(at), value(at + 8)), (3.0, 8.0));
        assert_eq!((value(at + 16), value(at + 24), value(at + 32)), (3.0, -1e39, 8.0));
        assert_eq!(parse_shape(&record).unwrap(), shape);
    }

    #[test]
    fn launders_names_like_gdal() {
        // The names ogr2ogr 3.12 writes into a shapefile from these GeoJSON properties.
        let names: Vec<String> = [
            "population_total",
            "population_totals",
            "population_totalz",
            "name",
            "NAME",
            "Name",
            "name2",
            "abcdefghi :x",
            "abc:def",
            "abcdefgh_1",
            "abcdefghij",
            "abcdefghijk",
        ]
        .map(String::from)
        .into();
        assert_eq!(
            dbf_field_names(&names),
            [
                "population",
                "populati_1",
                "populati_2",
                "name",
                "NAME3",
                "Name4",
                "name2",
                "abcdefghi",
                "abc_def",
                "abcdefgh_1",
                "abcdefghij",
                "abcdefgh_2"
            ]
        );
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

    /// The x/y of the rings of each polygon, exterior first.
    fn polygon_rings(g: &Geometry) -> Vec<Vec<Vec<(f64, f64)>>> {
        let xy = |r: &Ring| -> Vec<(f64, f64)> { r.0.iter().map(|c| (c.x, c.y)).collect() };
        match g {
            Geometry::Polygon { exterior, interiors } => vec![std::iter::once(exterior).chain(interiors).map(xy).collect()],
            Geometry::MultiPolygon(polygons) => polygons.iter().map(|(e, is)| std::iter::once(e).chain(is).map(xy).collect()).collect(),
            _ => panic!("not polygonal: {g:?}"),
        }
    }

    fn closed(xy: &[(f64, f64)]) -> Vec<Coord> {
        let mut cs: Vec<Coord> = xy.iter().map(|&(x, y)| Coord::xy(x, y)).collect();
        cs.push(cs[0].clone());
        cs
    }

    #[test]
    fn splits_self_touching_rings_like_gdal() {
        // An Esri ring: clockwise round a 10 × 10 square, then round a hole touching the edge at
        // (5, 0), the way the ISTAT municipalities of Rutigliano and Santa Ninfa are stored. The
        // stored hole, the second part, comes before the split one, and each split ring starts one
        // vertex later than stored, both as in GDAL 3.13.
        let ring = [(0.0, 0.0), (0.0, 10.0), (10.0, 10.0), (10.0, 0.0), (5.0, 0.0), (6.0, 2.0), (4.0, 2.0), (5.0, 0.0)];
        let hole = [(2.0, 6.0), (3.0, 6.0), (3.0, 7.0), (2.0, 7.0)];
        assert_eq!(
            polygon_rings(&organize_rings(vec![closed(&ring), closed(&hole)])),
            [vec![
                vec![(0.0, 10.0), (10.0, 10.0), (10.0, 0.0), (5.0, 0.0), (0.0, 0.0)],
                hole.to_vec(),
                vec![(6.0, 2.0), (4.0, 2.0), (5.0, 0.0)]
            ]]
        );
        // Two holes on one ring, in the order the ring runs through them.
        let two = [(0.0, 0.0), (0.0, 10.0), (5.0, 10.0), (4.0, 8.0), (6.0, 8.0), (5.0, 10.0), (10.0, 10.0), (10.0, 0.0)]
            .into_iter()
            .chain([(5.0, 0.0), (6.0, 2.0), (4.0, 2.0), (5.0, 0.0)])
            .collect::<Vec<_>>();
        assert_eq!(
            polygon_rings(&organize_rings(vec![closed(&two), closed(&hole)])),
            [vec![
                vec![(0.0, 10.0), (5.0, 10.0), (10.0, 10.0), (10.0, 0.0), (5.0, 0.0), (0.0, 0.0)],
                hole.to_vec(),
                vec![(4.0, 8.0), (6.0, 8.0), (5.0, 10.0)],
                vec![(6.0, 2.0), (4.0, 2.0), (5.0, 0.0)]
            ]]
        );
        // The same ring stored from inside the hole, at (6, 2) or (4, 2): split all the same, the
        // square first. Both rings start one vertex after (5, 0), as in GDAL 3.13.1.
        for k in [5, 6] {
            let mut inside = ring.to_vec();
            inside.rotate_left(k);
            assert_eq!(
                polygon_rings(&organize_rings(vec![closed(&inside), closed(&hole)])),
                [vec![
                    vec![(0.0, 0.0), (0.0, 10.0), (10.0, 10.0), (10.0, 0.0), (5.0, 0.0)],
                    hole.to_vec(),
                    vec![(6.0, 2.0), (4.0, 2.0), (5.0, 0.0)]
                ]],
                "starting at {:?}",
                inside[0]
            );
        }
        // Two holes, stored from inside the upper one: the loop split off first comes first, the
        // rest, which holds the start, last.
        let mut upper = two.clone();
        upper.rotate_left(3);
        assert_eq!(
            polygon_rings(&organize_rings(vec![closed(&upper), closed(&hole)])),
            [vec![
                vec![(10.0, 10.0), (10.0, 0.0), (5.0, 0.0), (0.0, 0.0), (0.0, 10.0), (5.0, 10.0)],
                hole.to_vec(),
                vec![(6.0, 2.0), (4.0, 2.0), (5.0, 0.0)],
                vec![(4.0, 8.0), (6.0, 8.0), (5.0, 10.0)]
            ]]
        );
        // GDAL reads a record of a single ring as it is stored.
        assert_eq!(polygon_rings(&organize_rings(vec![closed(&ring)])), [vec![ring.to_vec()]]);
    }

    #[test]
    fn assigns_holes_touching_the_exterior() {
        // A stored hole whose first vertex lies on the exterior, at (5, 10): the even-odd test at
        // that vertex may say it is outside.
        let square = [(0.0, 0.0), (0.0, 10.0), (10.0, 10.0), (10.0, 0.0)];
        let hole = [(5.0, 10.0), (4.0, 8.0), (6.0, 8.0)];
        assert_eq!(polygon_rings(&organize_rings(vec![closed(&square), closed(&hole)])), [vec![square.to_vec(), hole.to_vec()]]);
        // The same hole with its first vertex repeated: the first edge has no length.
        let repeated = [(5.0, 10.0), (5.0, 10.0), (4.0, 8.0), (6.0, 8.0)];
        assert_eq!(polygon_rings(&organize_rings(vec![closed(&square), closed(&repeated)])), [vec![square.to_vec(), repeated.to_vec()]]);
    }

    #[test]
    fn keeps_rings_that_are_not_esri_holes() {
        // A figure-eight: the second lobe lies outside the first.
        let eight = [(0.0, 0.0), (0.0, 2.0), (2.0, 2.0), (2.0, 0.0), (4.0, 0.0), (4.0, -2.0), (2.0, -2.0), (2.0, 0.0)];
        // A spike out and back along the same edge.
        let spike = [(0.0, 0.0), (0.0, 4.0), (2.0, 4.0), (3.0, 5.0), (2.0, 4.0), (4.0, 4.0), (4.0, 0.0)];
        // A loop inside, but wound the same way as the ring: not a hole.
        let same = [(0.0, 0.0), (0.0, 10.0), (10.0, 10.0), (10.0, 0.0), (5.0, 0.0), (4.0, 2.0), (6.0, 2.0), (5.0, 0.0)];
        // Consecutive duplicate points.
        let dup = [(0.0, 0.0), (0.0, 4.0), (0.0, 4.0), (4.0, 4.0), (4.0, 0.0)];
        // An island as a second part, so that the record is one GDAL organizes.
        let island = [(20.0, 20.0), (20.0, 21.0), (21.0, 21.0), (21.0, 20.0)];
        for ring in [&eight[..], &spike, &same, &dup] {
            let g = organize_rings(vec![closed(ring), closed(&island)]);
            assert_eq!(polygon_rings(&g), [vec![ring.to_vec()], vec![island.to_vec()]], "{ring:?}");
        }
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

    #[test]
    fn takes_the_epsg_crs_of_an_esri_prj() {
        // GDAL's Shapefile driver replaces an ESRI .prj it identifies by the EPSG CRS, which a
        // GeoPackage then names EPSG:4326 (srs_id 4326), not a custom CRS.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.shp");
        let path = path.to_str().unwrap();
        let mut l = Layer::new("p");
        l.add_field(FieldDef::new("id", FieldType::Integer));
        l.push(Feature { fid: 0, geometry: Some(Geometry::point(1.0, 2.0)), attributes: vec![FieldValue::Integer(1)] });
        write(&l, path).unwrap();
        let esri = r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;
        std::fs::write(dir.path().join("p.prj"), esri).unwrap();
        let back = read(path).unwrap();
        assert_eq!(back.crs_epsg(), Some(4326));
        assert_eq!(super::super::trusted_epsg(&back), Some(4326));
        // A .prj matching no EPSG CRS is kept as it is.
        let custom = r#"PROJCS["custom",GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["False_Easting",123456.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",7.3],PARAMETER["Scale_Factor",0.9991],PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]"#;
        std::fs::write(dir.path().join("p.prj"), custom).unwrap();
        let back = read(path).unwrap();
        assert_eq!(back.crs_epsg(), None);
        assert_eq!(back.crs_wkt(), Some(custom));
    }
}
