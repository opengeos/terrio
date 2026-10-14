//! MapInfo TAB (native tables): a `.tab` header, attributes in a `.dat` file (dBase-like with
//! binary MapInfo types), objects in a `.map` file and their offsets in a `.id` file.
//!
//! Read as GDAL's "MapInfo File" driver reads them (`ogr/ogrsf_frmts/mitab`): feature ids are
//! record numbers from 1, deleted records are skipped, regions become polygons from their section
//! headers (a ring and the holes that follow it), rectangles and ellipses polygons, arcs lines,
//! text objects points (the text's lower-left corner), and collections geometry collections.
//! Text is decoded with the table's charset. The CRS comes from the projection in the `.map`
//! header, for the projections and datums the MIF reader knows.

use super::mapinfo;
use crate::{Error, Result};
use std::path::{Path, PathBuf};
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer, Ring};

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("MapInfo TAB: {msg}"))
}

/// The file next to `path` with extension `ext`, in either case.
fn sibling(path: &Path, ext: &str) -> Option<PathBuf> {
    [ext.to_ascii_lowercase(), ext.to_ascii_uppercase()].iter().map(|e| path.with_extension(e)).find(|p| p.exists())
}

/// Reads a `.tab` file and the `.dat`, `.map` and `.id` files next to it.
pub fn read(path: &str) -> Result<Layer> {
    let p = Path::new(path);
    let tab = std::fs::read(p)?;
    if String::from_utf8_lossy(&tab).to_ascii_lowercase().replace(' ', "").contains("\"\\isseamless\"=\"true\"") {
        return read_seamless(p, &tab);
    }
    let dat = sibling(p, "dat").map(std::fs::read).transpose()?.ok_or_else(|| bad("no .dat file"))?;
    let map = sibling(p, "map").map(std::fs::read).transpose()?;
    let id = sibling(p, "id").map(std::fs::read).transpose()?;
    let name = p.file_stem().and_then(|s| s.to_str()).unwrap_or("layer");
    parse(&tab, &dat, map.as_deref(), id.as_deref(), name)
}

/// Reads a seamless table: the features of the tables its records name, with feature ids made of
/// the table's number (from 1) in the high 32 bits, as GDAL numbers them.
fn read_seamless(p: &Path, tab: &[u8]) -> Result<Layer> {
    let dat = sibling(p, "dat").map(std::fs::read).transpose()?.ok_or_else(|| bad("no .dat file"))?;
    let index = parse(tab, &dat, None, None, "index")?;
    let column = index.schema.fields().iter().position(|f| f.name.eq_ignore_ascii_case("table")).ok_or_else(|| bad("no Table column"))?;
    let dir = p.parent().unwrap_or(Path::new("."));
    let name = p.file_stem().and_then(|s| s.to_str()).unwrap_or("layer");
    let mut out: Option<Layer> = None;
    for (i, f) in index.features.iter().enumerate() {
        let Some(FieldValue::Text(part)) = f.attributes.get(column) else { continue };
        let part_path = dir.join(part.replace('\\', "/"));
        let part_path = if part_path.exists() { part_path } else { part_path.with_extension("tab") };
        let mut layer = read(part_path.to_str().ok_or_else(|| bad("table path"))?)?;
        let target = out.get_or_insert_with(|| {
            let mut l = layer.clone();
            l.features.clear();
            l.name = name.to_string();
            l
        });
        for mut feature in layer.features.drain(..) {
            feature.fid |= (i as u64 + 1) << 32;
            target.push(feature);
        }
    }
    out.ok_or_else(|| bad("a seamless table without tables"))
}

/// The text encodings of MapInfo charsets.
#[derive(Clone, Copy, PartialEq)]
enum Charset {
    /// No recoding (`Neutral`): UTF-8 when valid, Latin-1 otherwise.
    Raw,
    Cp1252,
    Cp1251,
    Utf8,
}

impl Charset {
    fn parse(name: &str) -> Self {
        match name.trim_matches('"').to_ascii_lowercase().as_str() {
            "windowslatin1" | "iso8859_1" | "windowslatin2" => Charset::Cp1252,
            "windowscyrillic" => Charset::Cp1251,
            "utf-8" | "utf8" => Charset::Utf8,
            _ => Charset::Raw,
        }
    }

    fn decode(self, b: &[u8]) -> String {
        match self {
            Charset::Raw | Charset::Utf8 => match std::str::from_utf8(b) {
                Ok(s) => s.to_string(),
                Err(_) if self == Charset::Utf8 => String::from_utf8_lossy(b).into_owned(),
                Err(_) => b.iter().map(|&c| c as char).collect(),
            },
            Charset::Cp1252 => b.iter().map(|&c| cp1252(c)).collect(),
            Charset::Cp1251 => b.iter().map(|&c| cp1251(c)).collect(),
        }
    }
}

fn cp1252(c: u8) -> char {
    const HIGH: [u16; 32] = [
        0x20AC, 0x81, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0x8D, 0x017D, 0x8F, 0x90,
        0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x9D, 0x017E, 0x0178,
    ];
    match c {
        0x80..=0x9F => char::from_u32(HIGH[(c - 0x80) as usize] as u32).unwrap_or('?'),
        _ => c as char,
    }
}

fn cp1251(c: u8) -> char {
    const HIGH: [u16; 64] = [
        0x0402, 0x0403, 0x201A, 0x0453, 0x201E, 0x2026, 0x2020, 0x2021, 0x20AC, 0x2030, 0x0409, 0x2039, 0x040A, 0x040C, 0x040B, 0x040F,
        0x0452, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x98, 0x2122, 0x0459, 0x203A, 0x045A, 0x045C, 0x045B, 0x045F,
        0x00A0, 0x040E, 0x045E, 0x0408, 0x00A4, 0x0490, 0x00A6, 0x00A7, 0x0401, 0x00A9, 0x0404, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x0407,
        0x00B0, 0x00B1, 0x0406, 0x0456, 0x0491, 0x00B5, 0x00B6, 0x00B7, 0x0451, 0x2116, 0x0454, 0x00BB, 0x0458, 0x0405, 0x0455, 0x0457,
    ];
    match c {
        0x80..=0xBF => char::from_u32(HIGH[(c - 0x80) as usize] as u32).unwrap_or('?'),
        0xC0..=0xFF => char::from_u32(0x0410 + (c - 0xC0) as u32).unwrap_or('?'),
        _ => c as char,
    }
}

/// A MapInfo field type.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Char,
    Integer,
    SmallInt,
    LargeInt,
    Float,
    Decimal,
    Date,
    Time,
    DateTime,
    Logical,
}

/// The `Definition Table` of a `.tab` file: the charset and the fields (name, type, width,
/// precision).
/// A field of a `.tab` file: name, type, width and precision.
type FieldSpec = (String, Kind, usize, usize);

fn definition(text: &str) -> Result<(Charset, Vec<FieldSpec>)> {
    let lower = text.to_ascii_lowercase();
    if !lower.contains("definition table") {
        return Err(Error::Unsupported("MapInfo TAB: only native tables are supported (not views or seamless tables)".into()));
    }
    let mut charset = Charset::Raw;
    let mut fields = Vec::new();
    let mut lines = text.lines().map(str::trim);
    while let Some(line) = lines.next() {
        let l = line.to_ascii_lowercase();
        if l.starts_with("!charset") {
            charset = Charset::parse(line[8..].trim());
        } else if l.starts_with("type ") {
            let kind = l.split_whitespace().nth(1).unwrap_or_default();
            if kind != "native" {
                return Err(Error::Unsupported(format!("MapInfo TAB: {kind} tables are not supported")));
            }
            if let Some(i) = l.find("charset") {
                charset = Charset::parse(line[i + 7..].trim());
            }
        } else if l.starts_with("fields") {
            let n: usize = l.split_whitespace().nth(1).and_then(|n| n.parse().ok()).unwrap_or(0);
            for _ in 0..n {
                let Some(f) = lines.next() else { break };
                fields.push(field_line(f)?);
            }
            break;
        }
    }
    Ok((charset, fields))
}

/// Parses `name Type [(width[,precision])] [Index n] ;`.
fn field_line(line: &str) -> Result<(String, Kind, usize, usize)> {
    let line = line.trim().trim_end_matches(';').trim();
    let (name, rest) = if let Some(stripped) = line.strip_prefix(['"', '`']) {
        let q = line.chars().next().unwrap_or('"');
        let end = stripped.find(q).ok_or_else(|| bad(format!("field '{line}'")))?;
        (stripped[..end].to_string(), &stripped[end + 1..])
    } else {
        let (n, r) = line.split_once(char::is_whitespace).ok_or_else(|| bad(format!("field '{line}'")))?;
        (n.to_string(), r)
    };
    let rest = rest.trim();
    let t: String = rest.chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_lowercase();
    let kind = match t.as_str() {
        "char" => Kind::Char,
        "integer" => Kind::Integer,
        "smallint" => Kind::SmallInt,
        "largeint" => Kind::LargeInt,
        "float" => Kind::Float,
        "decimal" => Kind::Decimal,
        "date" => Kind::Date,
        "time" => Kind::Time,
        "datetime" => Kind::DateTime,
        "logical" => Kind::Logical,
        _ => return Err(bad(format!("unknown field type in '{line}'"))),
    };
    let mut nums = rest.split(['(', ')']).nth(1).unwrap_or_default().split(',').filter_map(|v| v.trim().parse::<usize>().ok());
    let (w, p) = (nums.next().unwrap_or(0), nums.next().unwrap_or(0));
    Ok((name, kind, w, p))
}

fn le_i16(b: &[u8], i: usize) -> Option<i16> {
    Some(i16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?))
}

fn le_i32(b: &[u8], i: usize) -> Option<i32> {
    Some(i32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

fn le_f64(b: &[u8], i: usize) -> Option<f64> {
    Some(f64::from_le_bytes(b.get(i..i + 8)?.try_into().ok()?))
}

/// Parses the files of a TAB table held in memory. `name` names the layer.
pub fn parse(tab: &[u8], dat: &[u8], map: Option<&[u8]>, id: Option<&[u8]>, name: &str) -> Result<Layer> {
    // The header is ASCII but for field names, in the table's charset.
    let head = String::from_utf8_lossy(tab).to_string();
    let charset = definition(&head)?.0;
    let (_, fields) = definition(&charset.decode(tab))?;

    // The .dat header: record count, header and record sizes, field widths.
    if dat.len() < 32 {
        return Err(bad("the .dat file is too short"));
    }
    let records = u32::from_le_bytes([dat[4], dat[5], dat[6], dat[7]]) as usize;
    let header_len = u16::from_le_bytes([dat[8], dat[9]]) as usize;
    let record_len = u16::from_le_bytes([dat[10], dat[11]]) as usize;
    let mut widths = Vec::new();
    let mut p = 32;
    while p + 32 <= header_len && dat[p] != 0x0d {
        widths.push(dat[p + 16] as usize);
        p += 32;
    }
    if widths.len() != fields.len() {
        return Err(bad(format!("the .tab file declares {} fields, the .dat file has {}", fields.len(), widths.len())));
    }

    let mut layer = Layer::new(name);
    for (n, k, w, prec) in &fields {
        let t = match k {
            Kind::Char | Kind::Time => FieldType::Text,
            Kind::Integer | Kind::SmallInt | Kind::LargeInt => FieldType::Integer,
            Kind::Float | Kind::Decimal => FieldType::Float,
            Kind::Date => FieldType::Date,
            Kind::DateTime => FieldType::DateTime,
            Kind::Logical => FieldType::Boolean,
        };
        let mut def = FieldDef::new(n, t);
        if matches!(k, Kind::Char | Kind::Decimal) {
            def.width = *w;
            def.precision = *prec;
        }
        layer.add_field(def);
    }

    let map = match map.filter(|m| m.len() >= 512) {
        Some(m) => Some(MapFile::new(m)?),
        None => None,
    };
    if let Some(m) = &map {
        if let Some(srs) = m.srs() {
            srs.apply_to_layer(&mut layer);
        }
        // A table of points only is a point layer, as GDAL declares it.
        if m.counts[0] > 0 && m.counts[1..].iter().all(|&c| c == 0) {
            layer.geom_type = Some(GeometryType::Point);
        }
    }

    for r in 0..records {
        let start = header_len + r * record_len;
        let Some(rec) = dat.get(start..start + record_len) else { break };
        if rec[0] != b' ' {
            continue; // deleted
        }
        let mut pos = 1;
        let mut attributes = Vec::with_capacity(fields.len());
        for ((_, kind, _, _), w) in fields.iter().zip(&widths) {
            let b = rec.get(pos..pos + w).unwrap_or_default();
            pos += w;
            attributes.push(field_value(b, *kind, charset));
        }
        let geometry = match (&map, id) {
            (Some(m), Some(id)) => match le_i32(id, r * 4) {
                Some(off) if off > 0 => m.object(off as usize, r as i32 + 1, charset)?,
                _ => None,
            },
            _ => None,
        };
        layer.push(Feature { fid: r as u64 + 1, geometry, attributes });
    }
    Ok(layer)
}

fn field_value(b: &[u8], kind: Kind, charset: Charset) -> FieldValue {
    match kind {
        Kind::Char => {
            let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
            FieldValue::Text(charset.decode(&b[..end]))
        }
        Kind::Integer => le_i32(b, 0).map_or(FieldValue::Null, |v| FieldValue::Integer(v as i64)),
        Kind::SmallInt => le_i16(b, 0).map_or(FieldValue::Null, |v| FieldValue::Integer(v as i64)),
        Kind::LargeInt => {
            b.get(..8).and_then(|x| x.try_into().ok()).map_or(FieldValue::Null, |x| FieldValue::Integer(i64::from_le_bytes(x)))
        }
        Kind::Float => le_f64(b, 0).map_or(FieldValue::Null, FieldValue::Float),
        Kind::Decimal => {
            let s = String::from_utf8_lossy(b);
            s.trim().trim_end_matches('\0').parse().map_or(FieldValue::Null, FieldValue::Float)
        }
        Kind::Date => match (le_i16(b, 0), b.get(2), b.get(3)) {
            (Some(y), Some(&m), Some(&d)) if (y, m, d) != (0, 0, 0) => FieldValue::Date(format!("{y:04}-{m:02}-{d:02}")),
            _ => FieldValue::Null,
        },
        Kind::Time => match le_i32(b, 0) {
            Some(ms) if (0..=86_400_000).contains(&ms) => FieldValue::Text(hms(ms)),
            _ => FieldValue::Null,
        },
        Kind::DateTime => match (le_i16(b, 0), b.get(2), b.get(3), le_i32(b, 4)) {
            (Some(y), Some(&m), Some(&d), Some(ms)) if (y, m, d) != (0, 0, 0) => {
                FieldValue::DateTime(format!("{y:04}-{m:02}-{d:02}T{}", hms(ms.max(0))))
            }
            _ => FieldValue::Null,
        },
        Kind::Logical => match b.first() {
            Some(b'T' | b't' | b'Y' | b'y' | 1) => FieldValue::Boolean(true),
            Some(_) => FieldValue::Boolean(false),
            None => FieldValue::Null,
        },
    }
}

fn hms(ms: i32) -> String {
    let (h, m, s, f) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    if f > 0 { format!("{h:02}:{m:02}:{s:02}.{f:03}") } else { format!("{h:02}:{m:02}:{s:02}") }
}

/// The `.map` file: its header, and objects read by offset.
struct MapFile<'a> {
    b: &'a [u8],
    block: usize,
    quadrant: u8,
    scale: (f64, f64),
    displ: (f64, f64),
    precision: (f64, f64),
    /// Points, lines, regions, texts.
    counts: [i32; 4],
    proj: u8,
    datum: i16,
    units: u8,
    params: [f64; 6],
}

/// MapInfo unit codes and their `CoordSys` names.
const UNIT_NAMES: &[(u8, &str)] =
    &[(0, "mi"), (1, "km"), (2, "in"), (3, "ft"), (4, "yd"), (5, "mm"), (6, "cm"), (7, "m"), (8, "survey ft"), (9, "nmi"), (13, "degree")];

impl<'a> MapFile<'a> {
    fn new(b: &'a [u8]) -> Result<Self> {
        if le_i32(b, 0x100) != Some(42424242) {
            return Err(bad("not a .map file"));
        }
        let version = le_i16(b, 0x104).unwrap_or(0);
        let block = le_i16(b, 0x106).unwrap_or(512).max(512) as usize;
        let counts =
            [le_i32(b, 0x13c).unwrap_or(0), le_i32(b, 0x140).unwrap_or(0), le_i32(b, 0x144).unwrap_or(0), le_i32(b, 0x148).unwrap_or(0)];
        let coord_precision = b[0x160];
        let quadrant = b[0x161];
        let datum = if version >= 500 { le_i16(b, 0x16a).unwrap_or(0) } else { 0 };
        let (proj, units) = (b[0x16d], b[0x16f]);
        let f = |i: usize| le_f64(b, i).unwrap_or(0.0);
        let (mut scale, mut displ) = ((f(0x170), f(0x178)), (f(0x180), f(0x188)));
        if version <= 100 {
            let s = 10f64.powi(coord_precision as i32);
            scale = (s, s);
            displ = (0.0, 0.0);
        }
        if scale.0 == 0.0 || scale.1 == 0.0 {
            return Err(bad("null coordinate scale"));
        }
        let params = [f(0x190), f(0x198), f(0x1a0), f(0x1a8), f(0x1b0), f(0x1b8)];
        let precision = (10f64.powf(scale.0.log10().round()), 10f64.powf(scale.1.log10().round()));
        Ok(Self { b, block, quadrant, scale, displ, precision, counts, proj, datum, units, params })
    }

    /// The CRS, through the `CoordSys` clause the projection stands for.
    fn srs(&self) -> Option<crate::crs::SpatialRef> {
        if self.proj == 0 {
            return None;
        }
        let mut cs = format!("CoordSys Earth Projection {}, {}", self.proj, self.datum);
        if self.proj != 1 {
            let unit = UNIT_NAMES.iter().find(|u| u.0 == self.units)?.1;
            cs.push_str(&format!(", \"{unit}\""));
            for p in self.params {
                cs.push_str(&format!(", {p}"));
            }
        }
        mapinfo::coordsys_to_srs(&cs)
    }

    /// Integer coordinates to coordinates, as GDAL's `Int2Coordsys`.
    fn coord(&self, x: i32, y: i32) -> Coord {
        let q = self.quadrant;
        let mut dx =
            if matches!(q, 0 | 2 | 3) { -(x as f64 + self.displ.0) / self.scale.0 } else { (x as f64 - self.displ.0) / self.scale.0 };
        let mut dy =
            if matches!(q, 0 | 3 | 4) { -(y as f64 + self.displ.1) / self.scale.1 } else { (y as f64 - self.displ.1) / self.scale.1 };
        if self.precision.0 > 0.0 && self.precision.1 > 0.0 {
            dx = (dx * self.precision.0).round() / self.precision.0;
            dy = (dy * self.precision.1).round() / self.precision.1;
        }
        Coord::xy(dx, dy)
    }

    /// A cursor in the object block holding `off`, with the block's centre for compressed
    /// coordinates.
    fn object(&self, off: usize, expected_id: i32, charset: Charset) -> Result<Option<Geometry>> {
        let block = off - off % self.block;
        let center = (le_i32(self.b, block + 4).unwrap_or(0), le_i32(self.b, block + 8).unwrap_or(0));
        let mut c = Cursor { b: self.b, pos: off };
        let t = c.u8()?;
        let id = c.i32()?;
        if id != expected_id {
            return Err(bad(format!("object {expected_id} is stored as {id}")));
        }
        let compressed = t % 3 == 1;
        let pt = |c: &mut Cursor| -> Result<(i32, i32)> {
            if compressed { Ok((c.i16()? as i32 + center.0, c.i16()? as i32 + center.1)) } else { Ok((c.i32()?, c.i32()?)) }
        };
        let g = match t {
            0 => None,
            // Symbols, font symbols, custom symbols.
            0x01 | 0x02 => Some(Geometry::Point(self.at(pt(&mut c)?))),
            0x28 | 0x29 => {
                c.skip(12);
                Some(Geometry::Point(self.at(pt(&mut c)?)))
            }
            0x2b | 0x2c => {
                c.skip(2);
                Some(Geometry::Point(self.at(pt(&mut c)?)))
            }
            0x04 | 0x05 => Some(Geometry::LineString(vec![self.at(pt(&mut c)?), self.at(pt(&mut c)?)])),
            0x07 | 0x08 | 0x25 | 0x26 | 0x31 | 0x32 | 0x40 | 0x41 | 0x0d | 0x0e | 0x2e | 0x2f | 0x3d | 0x3e => {
                let ptr = c.i32()? as usize;
                let size = (c.i32()? & 0x7fff_ffff) as usize;
                let version = version(t);
                let sections = match t {
                    0x07 | 0x08 => 1,
                    0x3d | 0x3e | 0x40 | 0x41 => {
                        let n = c.i32()?;
                        c.skip(33);
                        n
                    }
                    _ => c.i16()? as i32,
                };
                let org = if compressed {
                    c.skip(4);
                    (c.i32()?, c.i32()?)
                } else {
                    (0, 0)
                };
                let mut s = CoordStream::new(self, ptr, org);
                Some(match t {
                    0x07 | 0x08 => {
                        let n = size / if compressed { 4 } else { 8 };
                        Geometry::LineString((0..n).map(|_| s.point(compressed).map(|p| self.at(p))).collect::<Result<_>>()?)
                    }
                    0x0d | 0x0e | 0x2e | 0x2f | 0x3d | 0x3e => self.region(&mut s, compressed, version, sections)?,
                    _ => self.multiline(&mut s, compressed, version, sections)?,
                })
            }
            0x0a | 0x0b => self.arc(&mut c, compressed, center)?,
            0x10 | 0x11 => self.text(&mut c, compressed, center, charset)?,
            0x13 | 0x14 | 0x16 | 0x17 => {
                let corner =
                    if t >= 0x16 { if compressed { (c.i16()? as i32, c.i16()? as i32) } else { (c.i32()?, c.i32()?) } } else { (0, 0) };
                let (a, b) = (self.at(pt(&mut c)?), self.at(pt(&mut c)?));
                let (x0, y0, x1, y1) = (a.x.min(b.x), a.y.min(b.y), a.x.max(b.x), a.y.max(b.y));
                let (rx, ry) = (corner.0 as f64 / self.scale.0 / 2.0, corner.1 as f64 / self.scale.1 / 2.0);
                let ring = if rx != 0.0 && ry != 0.0 {
                    let (rx, ry) = (rx.min((x1 - x0) / 2.0), ry.min((y1 - y0) / 2.0));
                    let pi = std::f64::consts::PI;
                    let mut r = Vec::new();
                    arc(&mut r, 45, x0 + rx, y0 + ry, rx, ry, pi, 1.5 * pi);
                    arc(&mut r, 45, x1 - rx, y0 + ry, rx, ry, 1.5 * pi, 2.0 * pi);
                    arc(&mut r, 45, x1 - rx, y1 - ry, rx, ry, 0.0, pi / 2.0);
                    arc(&mut r, 45, x0 + rx, y1 - ry, rx, ry, pi / 2.0, pi);
                    r
                } else {
                    vec![Coord::xy(x0, y0), Coord::xy(x1, y0), Coord::xy(x1, y1), Coord::xy(x0, y1)]
                };
                Some(polygon(ring))
            }
            0x19 | 0x1a => {
                let (a, b) = (self.at(pt(&mut c)?), self.at(pt(&mut c)?));
                let (cx, cy, rx, ry) = ((a.x + b.x) / 2.0, (a.y + b.y) / 2.0, ((b.x - a.x) / 2.0).abs(), ((b.y - a.y) / 2.0).abs());
                let mut r = Vec::new();
                arc(&mut r, 180, cx, cy, rx, ry, 0.0, 2.0 * std::f64::consts::PI);
                Some(polygon(r))
            }
            0x34 | 0x35 | 0x43 | 0x44 => {
                let ptr = c.i32()? as usize;
                let n = c.i32()? as usize;
                c.skip(15 + if t >= 0x43 { 33 } else { 0 } + 2);
                let org = if compressed {
                    c.skip(4);
                    (c.i32()?, c.i32()?)
                } else {
                    (0, 0)
                };
                let mut s = CoordStream::new(self, ptr, org);
                Some(Geometry::MultiPoint((0..n).map(|_| s.point(compressed).map(|p| self.at(p))).collect::<Result<_>>()?))
            }
            0x37 | 0x38 | 0x46 | 0x47 => self.collection(&mut c, t, compressed)?,
            _ => None,
        };
        Ok(g)
    }

    fn at(&self, (x, y): (i32, i32)) -> Coord {
        self.coord(x, y)
    }

    /// The section headers of a region or multiple polyline: `(vertices, holes, vertex offset)`.
    fn sections(&self, s: &mut CoordStream, compressed: bool, version: u32, n: i32) -> Result<(Vec<Section>, Vec<Coord>)> {
        let n = usize::try_from(n).map_err(|_| bad("section count"))?;
        let section_size = if version >= 450 { 28 } else { 24 };
        let total_hdr = section_size * n;
        let mut hdrs = Vec::with_capacity(n);
        let mut total = 0usize;
        for _ in 0..n {
            let vertices = if version >= 450 { s.i32()? } else { s.i16()? as i32 };
            let holes = if version >= 800 { s.i32()? } else { s.i16()? as i32 };
            s.point(compressed)?;
            s.point(compressed)?;
            let data = s.i32()? as usize;
            let (vertices, holes) =
                (usize::try_from(vertices).map_err(|_| bad("vertices"))?, usize::try_from(holes).map_err(|_| bad("holes"))?);
            let offset = data.checked_sub(total_hdr).ok_or_else(|| bad("section offset"))? / 8;
            hdrs.push((vertices, holes, offset));
            total += vertices;
        }
        let pts = (0..total).map(|_| s.point(compressed).map(|p| self.at(p))).collect::<Result<Vec<_>>>()?;
        for &(v, _, o) in &hdrs {
            if o + v > pts.len() {
                return Err(bad("section vertices out of range"));
            }
        }
        Ok((hdrs, pts))
    }

    fn region(&self, s: &mut CoordStream, compressed: bool, version: u32, n: i32) -> Result<Geometry> {
        let (hdrs, pts) = self.sections(s, compressed, version, n)?;
        let mut polygons: Vec<(Ring, Vec<Ring>)> = Vec::new();
        let mut holes_left = 0;
        for (v, holes, o) in hdrs {
            let mut ring = pts[o..o + v].to_vec();
            if ring.len() > 1 && ring.first().map(|c| (c.x, c.y)) == ring.last().map(|c| (c.x, c.y)) {
                ring.pop();
            }
            let ring = Ring::new(ring);
            if holes_left > 0 {
                holes_left -= 1;
                if let Some(p) = polygons.last_mut() {
                    p.1.push(ring);
                }
            } else {
                holes_left = holes;
                polygons.push((ring, Vec::new()));
            }
        }
        Ok(match polygons.len() {
            1 => {
                let (exterior, interiors) = polygons.remove(0);
                Geometry::Polygon { exterior, interiors }
            }
            _ => Geometry::MultiPolygon(polygons),
        })
    }

    fn multiline(&self, s: &mut CoordStream, compressed: bool, version: u32, n: i32) -> Result<Geometry> {
        let (hdrs, pts) = self.sections(s, compressed, version, n)?;
        let mut lines: Vec<Vec<Coord>> = hdrs.iter().map(|&(v, _, o)| pts[o..o + v].to_vec()).collect();
        Ok(if lines.len() == 1 { Geometry::LineString(lines.remove(0)) } else { Geometry::MultiLineString(lines) })
    }

    fn arc(&self, c: &mut Cursor, compressed: bool, center: (i32, i32)) -> Result<Option<Geometry>> {
        let (a0, a1) = (c.i16()? as f64 / 10.0, c.i16()? as f64 / 10.0);
        let pt = |c: &mut Cursor| -> Result<(i32, i32)> {
            if compressed { Ok((c.i16()? as i32 + center.0, c.i16()? as i32 + center.1)) } else { Ok((c.i32()?, c.i32()?)) }
        };
        let (e0, e1) = (self.at(pt(c)?), self.at(pt(c)?));
        let q = self.quadrant;
        let (mut start, mut end) = if matches!(q, 0 | 1 | 3) { (a0, a1) } else { (a1, a0) };
        if matches!(q, 0 | 2 | 3) {
            start = if start <= 180.0 { 180.0 - start } else { 540.0 - start };
            end = if end <= 180.0 { 180.0 - end } else { 540.0 - end };
        }
        if matches!(q, 0 | 3 | 4) {
            start = 360.0 - start;
            end = 360.0 - end;
        }
        let (cx, cy) = ((e0.x + e1.x) / 2.0, (e0.y + e1.y) / 2.0);
        let (rx, ry) = (((e1.x - e0.x) / 2.0).abs(), ((e1.y - e0.y) / 2.0).abs());
        let sweep = if end < start { end + 360.0 - start } else { end - start };
        let n = ((sweep / 2.0).abs() as usize + 1).max(2);
        let mut line = Vec::new();
        arc(&mut line, n, cx, cy, rx, ry, start.to_radians(), end.to_radians());
        Ok(Some(Geometry::LineString(line)))
    }

    fn text(&self, c: &mut Cursor, compressed: bool, center: (i32, i32), _charset: Charset) -> Result<Option<Geometry>> {
        c.skip(4 + 2 + 2); // string position and length, justification
        let angle = c.i16()? as f64 / 10.0;
        c.skip(2 + 6); // font style, colours
        let pt = |c: &mut Cursor| -> Result<(i32, i32)> {
            if compressed { Ok((c.i16()? as i32 + center.0, c.i16()? as i32 + center.1)) } else { Ok((c.i32()?, c.i32()?)) }
        };
        pt(c)?; // label line end
        let height = if compressed { c.i16()? as i32 } else { c.i32()? };
        let height = height as f64 / self.scale.1;
        c.skip(1);
        let (a, b) = (self.at(pt(c)?), self.at(pt(c)?));
        let (x0, y0, x1, y1) = (a.x.min(b.x), a.y.min(b.y), a.x.max(b.x), a.y.max(b.y));
        let (sin, cos) = angle.to_radians().sin_cos();
        let (x, y) = if sin > 0.0 && cos > 0.0 {
            (x0 + height * sin, y0)
        } else if sin > 0.0 && cos < 0.0 {
            (x1, y0 - height * cos)
        } else if sin < 0.0 && cos < 0.0 {
            (x1 + height * sin, y1)
        } else {
            (x0, y1 - height * cos)
        };
        Ok(Some(Geometry::Point(Coord::xy(x, y))))
    }

    fn collection(&self, c: &mut Cursor, t: u8, compressed: bool) -> Result<Option<Geometry>> {
        let version = version(t);
        let ptr = c.i32()? as usize;
        let n_points = c.i32()?;
        c.skip(8); // region and polyline data sizes
        let (n_reg, n_pl) = if version < 800 { (c.i16()? as i32, c.i16()? as i32) } else { (c.i32()?, c.i32()?) };
        if version >= 800 {
            c.skip(1);
        }
        c.skip(12 + 3 + 1 + 1 + 3);
        let org = if compressed { (c.i32()?, c.i32()?) } else { (0, 0) };
        let mut s = CoordStream::new(self, ptr, org);
        // Each part starts with a mini-header: its label point and bounds (relative to its own
        // compressed origin, read from the header too in V650 files).
        let mini = |s: &mut CoordStream| -> Result<()> {
            if version >= 800 {
                s.i32()?;
            }
            for _ in 0..3 {
                s.point(compressed)?;
            }
            Ok(())
        };
        let mut parts = Vec::new();
        let part_version = version.max(450);
        if n_reg > 0 {
            mini(&mut s)?;
            parts.push(self.region(&mut s, compressed, part_version, n_reg)?);
        }
        if n_pl > 0 {
            mini(&mut s)?;
            parts.push(self.multiline(&mut s, compressed, part_version, n_pl)?);
        }
        if n_points > 0 {
            mini(&mut s)?;
            parts.push(Geometry::MultiPoint((0..n_points).map(|_| s.point(compressed).map(|p| self.at(p))).collect::<Result<_>>()?));
        }
        Ok(Some(Geometry::GeometryCollection(parts)))
    }
}

/// A section header: vertex count, hole count, first vertex.
type Section = (usize, usize, usize);

/// The format version of an object type: 300, 450, 650 or 800.
fn version(t: u8) -> u32 {
    match t {
        ..0x2e => 300,
        0x2e..0x34 => 450,
        0x34..0x3a => 650,
        _ => 800,
    }
}

fn polygon(mut ring: Vec<Coord>) -> Geometry {
    if ring.len() > 1 && ring.first().map(|c| (c.x, c.y)) == ring.last().map(|c| (c.x, c.y)) {
        ring.pop();
    }
    Geometry::Polygon { exterior: Ring::new(ring), interiors: vec![] }
}

/// Points along an elliptical arc, as GDAL's `TABGenerateArc`.
#[allow(clippy::too_many_arguments)]
fn arc(out: &mut Vec<Coord>, n: usize, cx: f64, cy: f64, rx: f64, ry: f64, start: f64, mut end: f64) {
    if end < start {
        end += 2.0 * std::f64::consts::PI;
    }
    let step = (end - start) / (n as f64 - 1.0);
    let mut a = start;
    for i in 0..n {
        a = start + i as f64 * step;
        out.push(Coord::xy(cx + rx * a.cos(), cy + ry * a.sin()));
    }
    out.push(Coord::xy(cx + rx * a.cos(), cy + ry * a.sin()));
}

/// A little-endian cursor in the `.map` file.
struct Cursor<'a> {
    b: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N]> {
        let v = self.b.get(self.pos..self.pos + N).ok_or_else(|| bad("truncated object"))?;
        self.pos += N;
        Ok(v.try_into().unwrap_or([0; N]))
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take::<1>()?[0])
    }

    fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_le_bytes(self.take()?))
    }

    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take()?))
    }

    fn skip(&mut self, n: usize) {
        self.pos += n;
    }
}

/// Coordinate data, read across a chain of coordinate blocks (each with an 8-byte header: type,
/// bytes used, next block).
struct CoordStream<'a> {
    b: &'a [u8],
    block: usize,
    pos: usize,
    end: usize,
    next: usize,
    origin: (i32, i32),
}

impl<'a> CoordStream<'a> {
    fn new(m: &MapFile<'a>, ptr: usize, origin: (i32, i32)) -> Self {
        let mut s = Self { b: m.b, block: m.block, pos: ptr, end: 0, next: 0, origin };
        s.enter(ptr - ptr % m.block);
        s.pos = if ptr.is_multiple_of(m.block) { ptr + 8 } else { ptr };
        s
    }

    fn enter(&mut self, block: usize) {
        let used = le_i16(self.b, block + 2).unwrap_or(0).max(0) as usize;
        self.end = block + 8 + used;
        self.next = le_i32(self.b, block + 4).unwrap_or(0).max(0) as usize;
        self.pos = block + 8;
    }

    fn bytes<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut out = [0u8; N];
        for o in out.iter_mut() {
            if self.pos >= self.end && self.next > 0 && self.next < self.b.len() {
                let next = self.next;
                self.enter(next);
                let _ = self.block;
            }
            *o = *self.b.get(self.pos).ok_or_else(|| bad("truncated coordinates"))?;
            self.pos += 1;
        }
        Ok(out)
    }

    fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_le_bytes(self.bytes()?))
    }

    fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.bytes()?))
    }

    fn point(&mut self, compressed: bool) -> Result<(i32, i32)> {
        if compressed {
            Ok((self.i16()? as i32 + self.origin.0, self.i16()? as i32 + self.origin.1))
        } else {
            Ok((self.i32()?, self.i32()?))
        }
    }
}

/// Writes a layer as a TAB table: `path` names the `.tab` file; the `.dat`, `.map` and `.id`
/// files go next to it.
pub fn write(layer: &Layer, path: &str) -> Result<()> {
    let files = to_files(layer)?;
    let p = Path::new(path);
    std::fs::write(p, files.tab)?;
    std::fs::write(p.with_extension("dat"), files.dat)?;
    std::fs::write(p.with_extension("map"), files.map)?;
    std::fs::write(p.with_extension("id"), files.id)?;
    Ok(())
}

/// The four files of a TAB table.
pub struct TabFiles {
    /// The `.tab` header.
    pub tab: Vec<u8>,
    /// The attributes.
    pub dat: Vec<u8>,
    /// The objects.
    pub map: Vec<u8>,
    /// The object offsets.
    pub id: Vec<u8>,
}

const BLOCK: usize = 512;

/// MapInfo's object sizes by object type, stored at the start of the `.map` header.
const OBJ_LEN: [u8; 73] = [
    0x00, 0x0a, 0x0e, 0x15, 0x0e, 0x16, 0x1b, 0xa2, 0xa6, 0xab, 0x1a, 0x2a, 0x2f, 0xa5, 0xa9, 0xb5, 0xa7, 0xb5, 0xd9, 0x0f, 0x17, 0x23,
    0x13, 0x1f, 0x2b, 0x0f, 0x17, 0x23, 0x4f, 0x57, 0x63, 0x9c, 0xa4, 0xa9, 0xa0, 0xa8, 0xad, 0xa4, 0xa8, 0xad, 0x16, 0x1a, 0x39, 0x0d,
    0x11, 0x37, 0xa5, 0xa9, 0xb5, 0xa4, 0xa8, 0xad, 0xb2, 0xb6, 0xdc, 0xbd, 0xbd, 0xf4, 0x2b, 0x2f, 0x55, 0xc8, 0xcc, 0xd8, 0xc7, 0xcb,
    0xd0, 0xd3, 0xd7, 0xfd, 0xc2, 0xc2, 0xf9,
];

/// MapInfo datums: their ellipsoid and the datum shift GDAL writes with them.
const DATUM_ELLIPSOIDS: &[(i16, u8, [f64; 3])] = &[
    (104, 28, [0.0; 3]),
    (157, 54, [0.0; 3]),
    (74, 0, [0.0; 3]),
    (62, 7, [-8.0, 160.0, 176.0]),
    (79, 9, [375.0, -111.0, 431.0]),
    (28, 4, [-87.0, -98.0, -121.0]),
    (33, 0, [0.0; 3]),
    (115, 0, [0.0; 3]),
    (116, 0, [0.0; 3]),
];

/// The projection of a `CoordSys` clause: `(projection, datum, units, parameters)`; `None` for
/// non-earth coordinates.
fn projection(coordsys: &str) -> Option<(u8, i16, u8, Vec<f64>)> {
    let rest = coordsys.split_once("Projection")?.1;
    let mut parts = rest.split(',').map(str::trim);
    let proj: u8 = parts.next()?.parse().ok()?;
    let datum: i16 = parts.next()?.parse().ok()?;
    if proj == 1 {
        return Some((1, datum, 13, Vec::new()));
    }
    let unit = parts.next()?.trim_matches('"');
    let units = UNIT_NAMES.iter().find(|u| u.1 == unit)?.0;
    Some((proj, datum, units, parts.filter_map(|v| v.parse().ok()).collect()))
}

/// Encodes a layer as the files of a TAB table.
pub fn to_files(layer: &Layer) -> Result<TabFiles> {
    let srs = crate::crs::SpatialRef::from_layer(layer);
    let extent = crate::vector::layer_extent(layer);
    let coordsys = mapinfo::srs_to_coordsys(&srs, extent);
    let proj = projection(&coordsys);

    // Coordinate bounds: the world for longitude/latitude, as GDAL; the data's extent otherwise.
    let (x0, y0, x1, y1) = match (&proj, extent) {
        (Some((1, ..)), _) => (-1000.0, -1000.0, 1000.0, 1000.0),
        (_, Some((a, b, c, d))) => {
            let m = ((c - a).max(d - b) * 0.05).max(1.0);
            (a - m, b - m, c + m, d + m)
        }
        (_, None) => (-1000.0, -1000.0, 1000.0, 1000.0),
    };
    // A power of ten, so that readers rounding coordinates to the scale's precision (GDAL) get
    // them back exactly.
    let s = 10f64.powf((2e9 / (x1 - x0).max(y1 - y0)).log10().floor());
    let scale = (s, s);
    let displ = ((-s * (x1 + x0) / 2.0).round(), (-s * (y1 + y0) / 2.0).round());
    let to_int = |c: &Coord| -> (i32, i32) {
        ((c.x * scale.0 + displ.0).round().clamp(-2e9, 2e9) as i32, (c.y * scale.1 + displ.1).round().clamp(-2e9, 2e9) as i32)
    };

    // Fields.
    let fields = layer.schema.fields();
    let mut names: Vec<String> = Vec::new();
    let mut defs = Vec::new();
    for (i, f) in fields.iter().enumerate() {
        let mut name = mapinfo::column_name(&f.name);
        let mut n = 1;
        while names.iter().any(|x| x.eq_ignore_ascii_case(&name)) {
            name = format!("{}_{n}", mapinfo::column_name(&f.name).chars().take(28).collect::<String>());
            n += 1;
        }
        names.push(name.clone());
        let values = || layer.features.iter().filter_map(move |ft| ft.attributes.get(i));
        let (kind, width, decl) = match f.field_type {
            FieldType::Integer if values().filter_map(FieldValue::as_i64).all(|v| i32::try_from(v).is_ok()) => {
                (Kind::Integer, 4, "Integer".to_string())
            }
            FieldType::Integer => (Kind::LargeInt, 8, "LargeInt".into()),
            FieldType::Float => (Kind::Float, 8, "Float".into()),
            FieldType::Boolean => (Kind::Logical, 1, "Logical".into()),
            FieldType::Date => (Kind::Date, 4, "Date".into()),
            FieldType::DateTime => (Kind::DateTime, 8, "DateTime".into()),
            _ => {
                let longest = values().filter_map(FieldValue::as_str).map(str::len).max().unwrap_or(1);
                let w = if f.width > 0 { f.width.max(longest) } else { 254 }.clamp(1, 254);
                (Kind::Char, w, format!("Char ({w})"))
            }
        };
        defs.push((name, kind, width, decl));
    }

    // The .dat file.
    let record_len = 1 + defs.iter().map(|d| d.2).sum::<usize>();
    let header_len = 32 + 32 * defs.len() + 1;
    let mut dat = vec![0u8; header_len];
    dat[0] = 3;
    dat[1..4].copy_from_slice(&[126, 1, 1]);
    dat[4..8].copy_from_slice(&(layer.len() as u32).to_le_bytes());
    dat[8..10].copy_from_slice(&(header_len as u16).to_le_bytes());
    dat[10..12].copy_from_slice(&(record_len.min(65535) as u16).to_le_bytes());
    for (k, (name, kind, width, _)) in defs.iter().enumerate() {
        let d = &mut dat[32 + 32 * k..64 + 32 * k];
        for (j, b) in name.bytes().filter(u8::is_ascii).take(10).enumerate() {
            d[j] = b;
        }
        d[11] = match kind {
            Kind::Logical => b'L',
            Kind::Decimal => b'N',
            _ => b'C',
        };
        d[16] = *width as u8;
    }
    dat[header_len - 1] = 0x0d;
    for f in &layer.features {
        dat.push(b' ');
        for (k, (_, kind, width, _)) in defs.iter().enumerate() {
            let v = f.attributes.get(k).unwrap_or(&FieldValue::Null);
            let mut cell = vec![0u8; *width];
            encode_value(&mut cell, *kind, v);
            dat.extend_from_slice(&cell);
        }
    }
    dat.push(0x1a);

    // The .map and .id files.
    let mut map = MapWriter::new();
    let mut ids = Vec::with_capacity(layer.len() * 4);
    let mut version = 300;
    for (n, f) in layer.features.iter().enumerate() {
        let off = match &f.geometry {
            Some(g) => map.object(g, n as i32 + 1, &to_int, &mut version)?,
            None => 0,
        };
        ids.extend_from_slice(&(off as i32).to_le_bytes());
    }
    let map = map.finish(&proj, scale, displ);

    // The .tab header.
    let mut tab = format!(
        "!table\n!version {version}\n!charset Neutral\n\nDefinition Table\n  Type NATIVE Charset \"Neutral\"\n  Fields {}\n",
        defs.len()
    );
    for (name, _, _, decl) in &defs {
        tab.push_str(&format!("    {name} {decl} ;\n"));
    }
    Ok(TabFiles { tab: tab.into_bytes(), dat, map, id: ids })
}

fn encode_value(cell: &mut [u8], kind: Kind, v: &FieldValue) {
    match (kind, v) {
        (_, FieldValue::Null) => {
            if matches!(kind, Kind::Time) {
                cell.copy_from_slice(&(-1i32).to_le_bytes());
            }
        }
        (Kind::Integer, v) => cell.copy_from_slice(&(v.as_i64().unwrap_or(0) as i32).to_le_bytes()),
        (Kind::LargeInt, v) => cell.copy_from_slice(&v.as_i64().unwrap_or(0).to_le_bytes()),
        (Kind::Float, v) => cell.copy_from_slice(&v.as_f64().unwrap_or(0.0).to_le_bytes()),
        (Kind::Logical, FieldValue::Boolean(b)) => cell[0] = if *b { b'T' } else { b'F' },
        (Kind::Date | Kind::DateTime, FieldValue::Date(s) | FieldValue::DateTime(s) | FieldValue::Text(s)) => {
            let digits: Vec<u32> =
                s.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).filter_map(|p| p.parse().ok()).collect();
            if digits.len() >= 3 {
                cell[..2].copy_from_slice(&(digits[0] as i16).to_le_bytes());
                cell[2] = digits[1] as u8;
                cell[3] = digits[2] as u8;
                if kind == Kind::DateTime {
                    let ms =
                        digits.get(3).unwrap_or(&0) * 3_600_000 + digits.get(4).unwrap_or(&0) * 60_000 + digits.get(5).unwrap_or(&0) * 1000;
                    cell[4..8].copy_from_slice(&(ms as i32).to_le_bytes());
                }
            }
        }
        (_, v) => {
            let s = match v {
                FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => s.clone(),
                FieldValue::Integer(x) => x.to_string(),
                FieldValue::Float(x) => x.to_string(),
                FieldValue::Boolean(b) => (if *b { "T" } else { "F" }).to_string(),
                _ => String::new(),
            };
            // Truncated to the width, on a character boundary.
            let mut end = s.len().min(cell.len());
            while !s.is_char_boundary(end) {
                end -= 1;
            }
            cell[..end].copy_from_slice(&s.as_bytes()[..end]);
        }
    }
}

/// Builds a `.map` file: object blocks, coordinate blocks, then the spatial index.
struct MapWriter {
    blocks: Vec<Vec<u8>>,
    /// The current object block, its used bytes and the bounds of its objects.
    obj: Option<(usize, usize, [i32; 4])>,
    /// Finished object blocks: number and bounds.
    obj_blocks: Vec<(usize, [i32; 4])>,
    /// The current coordinate block and its used bytes.
    coord: Option<(usize, usize)>,
    bounds: Option<[i32; 4]>,
    counts: [i32; 4],
    max_coord: usize,
}

fn grow(b: &mut [i32; 4], o: &[i32; 4]) {
    b[0] = b[0].min(o[0]);
    b[1] = b[1].min(o[1]);
    b[2] = b[2].max(o[2]);
    b[3] = b[3].max(o[3]);
}

impl MapWriter {
    fn new() -> Self {
        // Block 0 is the header.
        Self { blocks: vec![vec![0; BLOCK]], obj: None, obj_blocks: Vec::new(), coord: None, bounds: None, counts: [0; 4], max_coord: 0 }
    }

    fn alloc(&mut self) -> usize {
        self.blocks.push(vec![0; BLOCK]);
        self.blocks.len() - 1
    }

    /// Appends coordinate data across chained coordinate blocks; returns where it starts.
    fn coords(&mut self, data: &[u8]) -> usize {
        self.max_coord = self.max_coord.max(data.len());
        let mut start = None;
        let mut rest = data;
        loop {
            let (b, used) = match self.coord {
                Some((b, used)) if used < BLOCK - 8 => (b, used),
                prev => {
                    let n = self.alloc();
                    self.blocks[n][0..2].copy_from_slice(&3i16.to_le_bytes());
                    if let Some((p, _)) = prev {
                        self.blocks[p][4..8].copy_from_slice(&((n * BLOCK) as i32).to_le_bytes());
                    }
                    self.coord = Some((n, 0));
                    (n, 0)
                }
            };
            if start.is_none() {
                start = Some(b * BLOCK + 8 + used);
            }
            let take = rest.len().min(BLOCK - 8 - used);
            self.blocks[b][8 + used..8 + used + take].copy_from_slice(&rest[..take]);
            let used = used + take;
            self.blocks[b][2..4].copy_from_slice(&(used as i16).to_le_bytes());
            self.coord = Some((b, used));
            rest = &rest[take..];
            if rest.is_empty() {
                return start.unwrap_or(0);
            }
        }
    }

    fn close_object_block(&mut self) {
        if let Some((b, used, mbr)) = self.obj.take() {
            let blk = &mut self.blocks[b];
            blk[0..2].copy_from_slice(&2i16.to_le_bytes());
            blk[2..4].copy_from_slice(&((used - 20) as i16).to_le_bytes());
            let cx = ((mbr[0] as i64 + mbr[2] as i64) / 2) as i32;
            let cy = ((mbr[1] as i64 + mbr[3] as i64) / 2) as i32;
            blk[4..8].copy_from_slice(&cx.to_le_bytes());
            blk[8..12].copy_from_slice(&cy.to_le_bytes());
            self.obj_blocks.push((b, mbr));
        }
    }

    /// Stores an object record; returns its offset.
    fn record(&mut self, rec: &[u8], mbr: [i32; 4]) -> usize {
        if self.obj.as_ref().is_some_and(|(_, used, _)| used + rec.len() > BLOCK) {
            self.close_object_block();
        }
        if self.obj.is_none() {
            let n = self.alloc();
            self.obj = Some((n, 20, mbr));
        }
        let Some((b, used, bmbr)) = self.obj.as_mut() else { return 0 };
        self.blocks[*b][*used..*used + rec.len()].copy_from_slice(rec);
        let off = *b * BLOCK + *used;
        *used += rec.len();
        grow(bmbr, &mbr);
        match &mut self.bounds {
            Some(all) => grow(all, &mbr),
            None => self.bounds = Some(mbr),
        }
        off
    }

    /// Writes a geometry's object (and coordinates); returns the object's offset, or 0 for
    /// geometries TAB cannot hold.
    fn object(&mut self, g: &Geometry, id: i32, to_int: &dyn Fn(&Coord) -> (i32, i32), version: &mut u32) -> Result<usize> {
        let mbr_of = |pts: &[(i32, i32)]| -> [i32; 4] {
            let mut m = [i32::MAX, i32::MAX, i32::MIN, i32::MIN];
            for &(x, y) in pts {
                grow(&mut m, &[x, y, x, y]);
            }
            m
        };
        let head = |t: u8| -> Vec<u8> {
            let mut r = vec![t];
            r.extend_from_slice(&id.to_le_bytes());
            r
        };
        let push_i32 = |r: &mut Vec<u8>, v: i32| r.extend_from_slice(&v.to_le_bytes());
        let label_mbr = |r: &mut Vec<u8>, m: &[i32; 4]| {
            r.extend_from_slice(&(((m[0] as i64 + m[2] as i64) / 2) as i32).to_le_bytes());
            r.extend_from_slice(&(((m[1] as i64 + m[3] as i64) / 2) as i32).to_le_bytes());
            for v in m {
                r.extend_from_slice(&v.to_le_bytes());
            }
        };
        Ok(match g {
            Geometry::Point(c) => {
                let (x, y) = to_int(c);
                let mut r = head(0x02);
                push_i32(&mut r, x);
                push_i32(&mut r, y);
                r.push(0);
                self.counts[0] += 1;
                self.record(&r, [x, y, x, y])
            }
            Geometry::LineString(cs) if !cs.is_empty() => {
                let pts: Vec<(i32, i32)> = cs.iter().map(to_int).collect();
                let mut data = Vec::with_capacity(pts.len() * 8);
                for (x, y) in &pts {
                    data.extend_from_slice(&x.to_le_bytes());
                    data.extend_from_slice(&y.to_le_bytes());
                }
                let ptr = self.coords(&data);
                let m = mbr_of(&pts);
                let mut r = head(0x08);
                push_i32(&mut r, ptr as i32);
                push_i32(&mut r, data.len() as i32);
                label_mbr(&mut r, &m);
                r.push(0);
                self.counts[1] += 1;
                self.record(&r, m)
            }
            Geometry::MultiLineString(parts) if !parts.is_empty() => {
                let sections: Vec<(Vec<(i32, i32)>, usize)> = parts.iter().map(|p| (p.iter().map(to_int).collect(), 0)).collect();
                *version = (*version).max(450);
                self.sections_object(0x32, id, &sections, None)?
            }
            Geometry::Polygon { exterior, interiors } => {
                *version = (*version).max(450);
                let sections = rings(exterior, interiors, to_int);
                self.sections_object(0x2f, id, &sections, Some(()))?
            }
            Geometry::MultiPolygon(polys) if !polys.is_empty() => {
                *version = (*version).max(450);
                let sections: Vec<_> = polys.iter().flat_map(|(e, h)| rings(e, h, to_int)).collect();
                self.sections_object(0x2f, id, &sections, Some(()))?
            }
            Geometry::MultiPoint(cs) if !cs.is_empty() => {
                *version = (*version).max(650);
                let pts: Vec<(i32, i32)> = cs.iter().map(to_int).collect();
                let mut data = Vec::with_capacity(pts.len() * 8);
                for (x, y) in &pts {
                    data.extend_from_slice(&x.to_le_bytes());
                    data.extend_from_slice(&y.to_le_bytes());
                }
                let ptr = self.coords(&data);
                let m = mbr_of(&pts);
                let mut r = head(0x35);
                push_i32(&mut r, ptr as i32);
                push_i32(&mut r, pts.len() as i32);
                r.extend_from_slice(&[0u8; 15]);
                r.extend_from_slice(&[0, 0]);
                label_mbr(&mut r, &m);
                self.counts[0] += 1;
                self.record(&r, m)
            }
            _ => 0,
        })
    }

    /// A multiple polyline or a region: section headers then vertices in the coordinate blocks.
    fn sections_object(&mut self, t: u8, id: i32, sections: &[(Vec<(i32, i32)>, usize)], region: Option<()>) -> Result<usize> {
        let n = sections.len();
        if n > 32767 {
            return Err(Error::Unsupported("MapInfo TAB: an object with more than 32767 parts".into()));
        }
        let mut data = Vec::new();
        let mut vertex = 0usize;
        let mut all = [i32::MAX, i32::MAX, i32::MIN, i32::MIN];
        for (pts, holes) in sections {
            let mut m = [i32::MAX, i32::MAX, i32::MIN, i32::MIN];
            for &(x, y) in pts {
                grow(&mut m, &[x, y, x, y]);
            }
            grow(&mut all, &m);
            data.extend_from_slice(&(pts.len() as i32).to_le_bytes());
            data.extend_from_slice(&(*holes as i16).to_le_bytes());
            for v in m {
                data.extend_from_slice(&v.to_le_bytes());
            }
            data.extend_from_slice(&((28 * n + 8 * vertex) as i32).to_le_bytes());
            vertex += pts.len();
        }
        for (pts, _) in sections {
            for (x, y) in pts {
                data.extend_from_slice(&x.to_le_bytes());
                data.extend_from_slice(&y.to_le_bytes());
            }
        }
        let ptr = self.coords(&data);
        let mut r = vec![t];
        r.extend_from_slice(&id.to_le_bytes());
        r.extend_from_slice(&(ptr as i32).to_le_bytes());
        r.extend_from_slice(&(data.len() as i32).to_le_bytes());
        r.extend_from_slice(&(n as i16).to_le_bytes());
        r.extend_from_slice(&(((all[0] as i64 + all[2] as i64) / 2) as i32).to_le_bytes());
        r.extend_from_slice(&(((all[1] as i64 + all[3] as i64) / 2) as i32).to_le_bytes());
        for v in all {
            r.extend_from_slice(&v.to_le_bytes());
        }
        r.push(0);
        if region.is_some() {
            r.push(0);
            self.counts[2] += 1;
        } else {
            self.counts[1] += 1;
        }
        Ok(self.record(&r, all))
    }

    /// Writes the spatial index and the header.
    fn finish(mut self, proj: &Option<(u8, i16, u8, Vec<f64>)>, scale: (f64, f64), displ: (f64, f64)) -> Vec<u8> {
        self.close_object_block();
        // The index: object blocks, grouped 25 to an index block, level by level up to one root.
        let per = (BLOCK - 4) / 20;
        let mut level: Vec<(usize, [i32; 4])> = std::mem::take(&mut self.obj_blocks);
        let mut depth = 0u8;
        let mut root = 0usize;
        while !level.is_empty() {
            depth += 1;
            let mut next = Vec::new();
            for group in level.chunks(per) {
                let n = self.alloc();
                let blk = &mut self.blocks[n];
                blk[0..2].copy_from_slice(&1i16.to_le_bytes());
                blk[2..4].copy_from_slice(&(group.len() as i16).to_le_bytes());
                let mut m = [i32::MAX, i32::MAX, i32::MIN, i32::MIN];
                for (k, (b, mbr)) in group.iter().enumerate() {
                    let at = 4 + 20 * k;
                    for (j, v) in mbr.iter().enumerate() {
                        blk[at + 4 * j..at + 4 * j + 4].copy_from_slice(&v.to_le_bytes());
                    }
                    blk[at + 16..at + 20].copy_from_slice(&((b * BLOCK) as i32).to_le_bytes());
                    grow(&mut m, mbr);
                }
                next.push((n, m));
            }
            if next.len() == 1 {
                root = next[0].0;
                break;
            }
            level = next;
        }

        let h = &mut self.blocks[0];
        h[..OBJ_LEN.len()].copy_from_slice(&OBJ_LEN);
        h[0x100..0x104].copy_from_slice(&42424242i32.to_le_bytes());
        h[0x104..0x106].copy_from_slice(&500i16.to_le_bytes());
        h[0x106..0x108].copy_from_slice(&(BLOCK as i16).to_le_bytes());
        h[0x108..0x110].copy_from_slice(&1.0f64.to_le_bytes());
        let b = self.bounds.unwrap_or([0; 4]);
        for (j, v) in b.iter().enumerate() {
            h[0x110 + 4 * j..0x114 + 4 * j].copy_from_slice(&v.to_le_bytes());
        }
        h[0x130..0x134].copy_from_slice(&((root * BLOCK) as i32).to_le_bytes());
        for (j, c) in self.counts.iter().enumerate() {
            h[0x13c + 4 * j..0x140 + 4 * j].copy_from_slice(&c.to_le_bytes());
        }
        h[0x14c..0x150].copy_from_slice(&(self.max_coord as i32).to_le_bytes());
        let (p, datum, units, params) = proj.clone().unwrap_or((0, 0, 7, Vec::new()));
        h[0x15e] = if p == 1 { 7 } else { units };
        h[0x15f] = if root == 0 { 0 } else { depth };
        h[0x160] = 3;
        h[0x161] = 1;
        h[0x163] = (OBJ_LEN.len() - 1) as u8;
        h[0x16a..0x16c].copy_from_slice(&datum.to_le_bytes());
        h[0x16d] = p;
        let (ellipsoid, shift) = DATUM_ELLIPSOIDS.iter().find(|d| d.0 == datum).map_or((0, [0.0; 3]), |d| (d.1, d.2));
        h[0x16e] = ellipsoid;
        h[0x16f] = units;
        for (j, v) in [scale.0, scale.1, displ.0, displ.1].iter().enumerate() {
            h[0x170 + 8 * j..0x178 + 8 * j].copy_from_slice(&v.to_le_bytes());
        }
        for (j, v) in params.iter().take(6).enumerate() {
            h[0x190 + 8 * j..0x198 + 8 * j].copy_from_slice(&v.to_le_bytes());
        }
        for (j, v) in shift.iter().enumerate() {
            h[0x1c0 + 8 * j..0x1c8 + 8 * j].copy_from_slice(&v.to_le_bytes());
        }
        self.blocks.concat()
    }
}

/// A polygon's rings as region sections: the exterior with its hole count, then the holes.
/// Rings are closed.
fn rings(exterior: &Ring, interiors: &[Ring], to_int: &dyn Fn(&Coord) -> (i32, i32)) -> Vec<(Vec<(i32, i32)>, usize)> {
    let close = |r: &Ring| -> Vec<(i32, i32)> {
        let mut pts: Vec<(i32, i32)> = r.0.iter().map(to_int).collect();
        if pts.len() > 1 && pts.first() != pts.last() {
            pts.push(pts[0]);
        }
        pts
    };
    let mut out = vec![(close(exterior), interiors.len())];
    out.extend(interiors.iter().map(|h| (close(h), 0)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let mut l = Layer::new("t").with_epsg(32633);
        l.add_field(FieldDef::new("name", FieldType::Text));
        l.add_field(FieldDef::new("n", FieldType::Integer));
        l.add_field(FieldDef::new("v", FieldType::Float));
        l.add_field(FieldDef::new("d", FieldType::Date));
        l.add_field(FieldDef::new("b", FieldType::Boolean));
        let sq = |x: f64, s: f64| Ring::new(vec![Coord::xy(x, 0.0), Coord::xy(x + s, 0.0), Coord::xy(x + s, s), Coord::xy(x, s)]);
        let geoms = [
            Some(Geometry::point(500000.5, 4000000.25)),
            Some(Geometry::LineString(vec![Coord::xy(500000.0, 0.0), Coord::xy(500100.0, 50.0)])),
            Some(Geometry::Polygon { exterior: sq(500000.0, 100.0), interiors: vec![sq(500010.0, 10.0)] }),
            Some(Geometry::MultiPolygon(vec![(sq(500000.0, 10.0), vec![]), (sq(500200.0, 10.0), vec![])])),
            Some(Geometry::MultiLineString(vec![vec![Coord::xy(500000.0, 0.0), Coord::xy(500001.0, 1.0)]; 2])),
            Some(Geometry::MultiPoint(vec![Coord::xy(500000.0, 0.0), Coord::xy(500002.0, 3.0)])),
            None,
        ];
        // Enough features for several object blocks and index levels.
        for i in 0..2000 {
            l.push(Feature {
                fid: i,
                geometry: geoms[i as usize % geoms.len()].clone(),
                attributes: vec![
                    FieldValue::Text(format!("feature é {i}")),
                    FieldValue::Integer(i as i64 - 1000),
                    FieldValue::Float(i as f64 / 4.0),
                    FieldValue::Date("2024-02-29".into()),
                    FieldValue::Boolean(i % 2 == 0),
                ],
            });
        }
        let f = to_files(&l).unwrap();
        let back = parse(&f.tab, &f.dat, Some(&f.map), Some(&f.id), "t").unwrap();
        assert_eq!(back.len(), 2000);
        assert_eq!(back.crs_epsg(), Some(32633));
        for (a, b) in l.features.iter().zip(&back.features) {
            assert_eq!(a.attributes, b.attributes);
            match (&a.geometry, &b.geometry) {
                (Some(x), Some(y)) => {
                    let (bx, by) = (x.bbox().unwrap(), y.bbox().unwrap());
                    assert!((bx.min_x - by.min_x).abs() < 1e-3 && (bx.max_y - by.max_y).abs() < 1e-3, "{x:?} {y:?}");
                    assert_eq!(x.geom_type(), y.geom_type());
                }
                (x, y) => assert_eq!(x, y),
            }
        }
        assert_eq!(back.features[1999].fid, 2000);
    }

    #[test]
    fn decodes_charsets() {
        assert_eq!(Charset::Cp1251.decode(&[0xcf, 0xf0, 0xe8]), "При");
        assert_eq!(Charset::Cp1252.decode(&[0x80, 0xe9]), "€é");
        assert_eq!(field_line("    Field6 Decimal (10, 2) Index 1 ;").unwrap(), ("Field6".into(), Kind::Decimal, 10, 2));
    }
}
