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

/// Reads the features `skip..skip + max` of a shapefile (`max` `None`: to the end), numbered as
/// [`read`] numbers them, reading only their records: DBF records by position (they have a fixed
/// size), and SHP records at the offsets the `.shx` index gives (through a scan of the record
/// headers, without decoding the others, when there is no `.shx`). Memory follows the range,
/// not the file.
pub fn read_range(path: &str, skip: usize, max: Option<usize>) -> Result<Layer> {
    use std::io::{Read, Seek, SeekFrom};
    let not_a_shapefile = || Error::InvalidData(format!("'{path}' is not a shapefile"));
    let base = Path::new(path).with_extension("");
    let name = base.file_name().and_then(|s| s.to_str()).unwrap_or("layer");
    let mut shp = std::io::BufReader::new(std::fs::File::open(path)?);
    let shp_len = shp.get_ref().metadata()?.len();
    let mut head = [0u8; 100];
    shp.read_exact(&mut head).map_err(|_| not_a_shapefile())?;
    let mut layer = header_layer(&head, name).map_err(|_| not_a_shapefile())?;
    let prj = sidecar(&base, "prj").and_then(|p| std::fs::read_to_string(p).ok());
    if prj.is_some() {
        crate::crs::prefetch();
    }
    let cpg = sidecar(&base, "cpg").and_then(|p| std::fs::read_to_string(p).ok());

    // Where each record of the .shp starts: from the .shx when it indexes the file, else a scan
    // of the record headers.
    let shx = sidecar(&base, "shx").map(std::fs::read).transpose()?.filter(|x| x.len() >= 100 && (x.len() - 100) % 8 == 0);
    let offsets: Vec<u64> = match &shx {
        Some(x) => x[100..].as_chunks::<8>().0.iter().map(|e| 2 * u64::from(u32::from_be_bytes([e[0], e[1], e[2], e[3]]))).collect(),
        None => {
            let mut found = Vec::new();
            let mut pos = 100u64;
            while pos + 8 <= shp_len {
                found.push(pos);
                shp.seek(SeekFrom::Start(pos + 4))?;
                let mut w = [0u8; 4];
                shp.read_exact(&mut w)?;
                pos = (pos + 8 + 2 * u64::from(u32::from_be_bytes(w))).min(shp_len);
            }
            found
        }
    };

    let (fields, dbf_total, mut dbf) = match sidecar(&base, "dbf") {
        Some(p) => {
            let mut f = std::io::BufReader::new(std::fs::File::open(p)?);
            let len = f.get_ref().metadata()?.len();
            let mut first = [0u8; 32];
            f.read_exact(&mut first).map_err(|_| Error::InvalidData("truncated .dbf file".into()))?;
            let header_len = dbf_header_len(&first)?;
            let mut header = first.to_vec();
            header.resize(header_len.max(32).min(len as usize), 0);
            f.read_exact(&mut header[32..]).map_err(|_| Error::InvalidData("truncated .dbf file".into()))?;
            let l = dbf_layout(&header, cpg.as_deref())?;
            let held = (len as usize).saturating_sub(l.header_len) / l.record_len.max(1);
            let total = if l.record_len == 0 { 0 } else { l.count.min(held) };
            (l.fields.clone(), total, Some((f, l)))
        }
        None => (Vec::new(), 0, None),
    };
    for f in fields {
        layer.add_field(f);
    }
    let n = if dbf_total == 0 { offsets.len() } else { dbf_total.max(offsets.len()) };
    let start = skip.min(n);
    let end = max.map_or(n, |m| start.saturating_add(m).min(n));

    // The DBF records of the range, read at their position.
    let mut records: Vec<Vec<FieldValue>> = Vec::new();
    if let Some((f, l)) = dbf.as_mut() {
        let last = end.min(dbf_total);
        if start < last {
            let bytes = (last - start) * l.record_len;
            let mut buf: Vec<u8> = Vec::new();
            buf.try_reserve_exact(bytes).map_err(|_| Error::InvalidData(format!("cannot allocate {bytes} bytes for .dbf records")))?;
            buf.resize(bytes, 0);
            f.seek(SeekFrom::Start((l.header_len + start * l.record_len) as u64))?;
            f.read_exact(&mut buf)?;
            records.try_reserve_exact(last - start).map_err(|_| Error::InvalidData("cannot allocate .dbf records".into()))?;
            records.extend(buf.chunks_exact(l.record_len).map(|r| l.record(r)));
        }
    }

    let width = layer.schema.len();
    layer.features.try_reserve_exact(end - start).map_err(|_| Error::InvalidData("cannot allocate features".into()))?;
    let mut content = Vec::new();
    let mut records = records.into_iter();
    for fid in start..end {
        let geometry = match offsets.get(fid) {
            Some(&at) if at + 8 <= shp_len => {
                shp.seek(SeekFrom::Start(at + 4))?;
                let mut w = [0u8; 4];
                shp.read_exact(&mut w)?;
                let len = (2 * u64::from(u32::from_be_bytes(w))).min(shp_len - at - 8) as usize;
                content.clear();
                content.resize(len, 0);
                shp.read_exact(&mut content)?;
                parse_shape(&content)
            }
            _ => None,
        };
        let attributes = records.next().unwrap_or_else(|| vec![FieldValue::Null; width]);
        layer.push(Feature { fid: fid as u64, geometry, attributes });
    }
    apply_prj(&mut layer, prj.as_deref());
    Ok(layer)
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
    let count = |o: usize| i32_at(o).and_then(|v| usize::try_from(v).ok());
    let (num_parts, num_points, parts_at) = if multipoint { (1, count(36)?, 40) } else { (count(36)?, count(40)?, 44) };
    // Counts the record cannot hold are corrupt (GDAL reads no geometry either).
    if num_parts > rec.len() / 4 || num_points > rec.len() / 16 {
        return None;
    }
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

/// Whether the self-touching rings of records of several parts are split into an exterior and
/// holes, as GDAL 3.13 splits them (#315). GDAL 3.12.1, which terrio reads as, keeps them as
/// stored: off, measured on the 18 cases of `scripts/compare_shapefile_rings_with_gdal.py`.
const SPLIT_SELF_TOUCHING_RINGS: bool = false;

/// Groups rings into polygons: clockwise rings are exteriors, and each counter-clockwise ring is
/// a hole of the smallest exterior containing it.
fn organize_rings(parts: Vec<Vec<Coord>>) -> Geometry {
    // GDAL organizes, and since 3.13 splits self-touching rings, only in records of several parts.
    let several = parts.len() > 1 && SPLIT_SELF_TOUCHING_RINGS;
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

/// The encoding of a `.dbf` file's text, decided as GDAL's Shapefile driver decides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DbfEncoding {
    Utf8,
    /// A single-byte code page: the characters of bytes 0x80 to 0xFF.
    Table(&'static [char; 128]),
    /// A multi-byte code page: 932, 936, 949 or 950.
    Cjk(u16),
    /// GDAL does not recode the text (no `.cpg`, no language driver id, or an encoding it cannot
    /// recode from): its bytes reach the caller as they are, and pyogrio decodes them as
    /// ISO-8859-1.
    Raw,
}

#[path = "shapefile_codepages.rs"]
mod codepages;

#[path = "shapefile_cjk.rs"]
mod cjk;

impl DbfEncoding {
    /// The encoding of a `.dbf` whose `.cpg` holds `cpg` (its first line counts) and whose header
    /// has the language driver id `ldid` (byte 29), as GDAL decides it: the `.cpg`, else the id,
    /// else the `SHAPE_ENCODING` option.
    pub(crate) fn detect(cpg: Option<&str>, ldid: u8) -> Self {
        let cpg = cpg.map(|c| c.split(['\n', '\r']).next().unwrap_or_default()).filter(|c| !c.is_empty());
        let named = match cpg {
            Some(c) => Some(Self::from_code_page(c)),
            None if ldid != 0 => Some(Self::from_ldid(ldid)),
            None => None,
        };
        match named {
            Some(e) => e,
            None => crate::config::get("SHAPE_ENCODING").map_or(Self::Raw, |e| Self::from_code_page(&e)),
        }
    }

    /// GDAL's `OGRShapeLayer::ConvertCodePage` of a `.cpg` value: code page numbers (as `atoi`
    /// reads them) of 437 to 950 and 1250 to 1258 are `CP<n>`, `8859...` is `ISO-8859-...`,
    /// `UTF-8...` is UTF-8, and other values go to iconv as they are.
    fn from_code_page(c: &str) -> Self {
        // ESRI writes `ANSI 1252`, which GDAL reads as the code page number.
        let number = c.strip_prefix("ANSI ").unwrap_or(c);
        let digits: String = number.trim_start().chars().take_while(char::is_ascii_digit).take(9).collect();
        let n: u32 = digits.parse().unwrap_or(0);
        let upper = c.to_ascii_uppercase();
        if (437..=950).contains(&n) || (1250..=1258).contains(&n) {
            return Self::from_iconv_name(&format!("CP{n}"));
        }
        if let Some(rest) = upper.strip_prefix("8859") {
            return Self::from_iconv_name(&format!("ISO-8859-{}", rest.strip_prefix('-').unwrap_or(rest)));
        }
        if upper.starts_with("UTF-8") {
            return Self::Utf8;
        }
        Self::from_iconv_name(upper.trim())
    }

    /// The encoding iconv knows by `name` (upper case), for the encodings terrio decodes;
    /// [`DbfEncoding::Raw`] for the others, which GDAL then leaves unrecoded too when iconv does
    /// not know them.
    fn from_iconv_name(name: &str) -> Self {
        let iso = |n: &str| format!("ISO-8859-{n}");
        let canonical = match name {
            "UTF-8" | "UTF8" => return Self::Utf8,
            // The names glibc's iconv gives these code pages' tables (measured: its SJIS, EUC-KR
            // and GB18030 are other tables; WINDOWS-932, -949 and -950 are unknown to it).
            "CP932" | "WINDOWS-31J" | "MS932" => return Self::Cjk(932),
            "CP936" | "GBK" | "MS936" | "WINDOWS-936" => return Self::Cjk(936),
            "CP949" | "UHC" => return Self::Cjk(949),
            "CP950" | "BIG5" => return Self::Cjk(950),
            "LATIN1" | "L1" | "ISO88591" | "ISO-IR-100" | "CP819" | "IBM819" => iso("1"),
            "LATIN2" | "L2" => iso("2"),
            "LATIN3" | "L3" => iso("3"),
            "LATIN4" | "L4" => iso("4"),
            "CYRILLIC" => iso("5"),
            "ARABIC" => iso("6"),
            "GREEK" => iso("7"),
            "HEBREW" => iso("8"),
            "LATIN5" | "L5" => iso("9"),
            "LATIN6" | "L6" => iso("10"),
            "LATIN7" | "L7" => iso("13"),
            "LATIN8" | "L8" => iso("14"),
            "LATIN9" | "L9" => iso("15"),
            "LATIN10" | "L10" => iso("16"),
            "MS-ANSI" => "CP1252".into(),
            "MS-EE" => "CP1250".into(),
            "MS-CYRL" => "CP1251".into(),
            "MS-GREEK" => "CP1253".into(),
            "MS-TURK" => "CP1254".into(),
            "MS-HEBR" => "CP1255".into(),
            "MS-ARAB" => "CP1256".into(),
            "WINBALTRIM" => "CP1257".into(),
            n => {
                if let Some(cp) = n.strip_prefix("WINDOWS-").or_else(|| n.strip_prefix("IBM")) {
                    format!("CP{cp}")
                } else if let Some(rest) = n.strip_prefix("ISO8859-").or_else(|| n.strip_prefix("ISO_8859-")) {
                    iso(rest)
                } else {
                    n.to_string()
                }
            }
        };
        codepages::table(&canonical).map_or(Self::Raw, Self::Table)
    }

    /// GDAL's code page of a DBF language driver id (`LDID/<n>`).
    fn from_ldid(ldid: u8) -> Self {
        let cp = match ldid {
            87 => return Self::from_iconv_name("ISO-8859-1"),
            1 | 11 | 13 | 15 | 18 | 21 | 24 | 25 | 27 => 437,
            2 | 10 | 14 | 16 | 20 | 22 | 26 | 29 | 37 | 55 => 850,
            3 | 88 | 89 => 1252,
            8 | 17 | 23 | 102 => 865,
            19 | 123 => 932,
            28 | 108 => 863,
            31 | 34 | 35 | 64 | 100 | 135 => 852,
            36 => 860,
            38 | 101 => 866,
            77 | 122 => 936,
            78 | 121 => 949,
            79 | 120 => 950,
            80 | 124 => 874,
            103 => 861,
            106 | 134 => 737,
            107 | 136 => 857,
            200 => 1250,
            201 => 1251,
            202 => 1254,
            203 => 1253,
            204 => 1257,
            _ => return Self::Raw,
        };
        Self::from_iconv_name(&format!("CP{cp}"))
    }

    /// Decodes text in this encoding. Invalid UTF-8 is replaced; bytes a code page leaves
    /// undefined are skipped, as iconv's recoding in GDAL skips them.
    pub(crate) fn decode(self, bytes: &[u8]) -> String {
        match self {
            Self::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
            Self::Raw => bytes.iter().map(|&b| b as char).collect(),
            Self::Table(t) => bytes
                .iter()
                .map(|&b| if b < 0x80 { b as char } else { t[usize::from(b - 0x80)] })
                .filter(|&c| c != codepages::UNDEFINED)
                .collect(),
            Self::Cjk(cp) => decode_cjk(cp, bytes),
        }
    }
}

/// Decodes a multi-byte code page as GDAL's recoding (glibc's iconv) does: `encoding_rs`'s
/// decoder where the two agree, glibc's differences from [`cjk`], and iconv's error handling in
/// GDAL: a byte that starts no character is skipped, and a character cut by the end of the text
/// ends it.
fn decode_cjk(cp: u16, bytes: &[u8]) -> String {
    use std::cmp::Ordering;
    let (encoding, invalid, chars, swallowed) = match cp {
        932 => (encoding_rs::SHIFT_JIS, cjk::INVALID_932, cjk::CHARS_932, cjk::SWALLOWED_932),
        936 => (encoding_rs::GBK, cjk::INVALID_936, cjk::CHARS_936, cjk::SWALLOWED_936),
        949 => (encoding_rs::EUC_KR, cjk::INVALID_949, cjk::CHARS_949, cjk::SWALLOWED_949),
        _ => (encoding_rs::BIG5, cjk::INVALID_950, cjk::CHARS_950, cjk::SWALLOWED_950),
    };
    // The character glibc decodes from `seq` (one byte, or a lead byte and a trail byte), whose
    // code is `code`.
    let glibc = |code: u16, seq: &[u8]| -> Option<char> {
        let rejected = invalid.binary_search_by(|&(a, b)| {
            if code < a {
                Ordering::Greater
            } else if code > b {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        });
        if rejected.is_ok() {
            return None;
        }
        if let Ok(i) = chars.binary_search_by_key(&code, |&(c, _)| c) {
            return Some(chars[i].1);
        }
        let text = encoding.decode_without_bom_handling_and_without_replacement(seq)?;
        let mut it = text.chars();
        match (it.next(), it.next()) {
            (Some(c), None) => Some(c),
            _ => None,
        }
    };
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b < 0x80 {
            out.push(char::from(b));
            i += 1;
        } else if let Some(c) = glibc(u16::from(b), &bytes[i..=i]) {
            out.push(c);
            i += 1;
        } else if let Some(&t) = bytes.get(i + 1) {
            let code = u16::from_be_bytes([b, t]);
            if swallowed.contains(&code) {
                // glibc consumes the two bytes, writes nothing and reports an error: GDAL skips
                // one more byte.
                i += 3;
                continue;
            }
            match glibc(code, &bytes[i..i + 2]) {
                Some(c) => {
                    out.push(c);
                    i += 2;
                }
                None => i += 1,
            }
        } else {
            break;
        }
    }
    out
}

/// The fields and record layout of a `.dbf` file.
pub(crate) struct DbfLayout {
    /// The fields.
    pub fields: Vec<FieldDef>,
    layout: Vec<(usize, usize, u8)>,
    encoding: DbfEncoding,
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
    let encoding = DbfEncoding::detect(cpg, d[29]);
    let count = u32::from_le_bytes([d[4], d[5], d[6], d[7]]) as usize;
    let record_len = u16::from_le_bytes([d[10], d[11]]) as usize;
    let mut fields = Vec::new();
    let mut layout = Vec::new();
    let mut offset = 1;
    let mut pos = 32;
    while pos + 32 <= d.len().min(header_len) && d[pos] != 0x0D {
        let raw = &d[pos..pos + 11];
        let name = encoding.decode(&raw[..raw.iter().position(|&b| b == 0).unwrap_or(11)]).trim().to_string();
        let kind = d[pos + 11];
        let width = d[pos + 16] as usize;
        let decimals = d[pos + 17] as usize;
        // As shapelib and GDAL type them: numbers without decimals are integers below 19 digits.
        let t = match kind {
            b'N' | b'F' if decimals == 0 && width < 19 => FieldType::Integer,
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
    Ok(DbfLayout { fields, layout, encoding, count, header_len, record_len })
}

impl DbfLayout {
    /// Decodes a record, with shapelib's rules for null values and blanks as GDAL builds it
    /// (`TRIM_DBF_WHITESPACE`: text loses its leading and trailing spaces).
    pub(crate) fn record(&self, rec: &[u8]) -> Vec<FieldValue> {
        let encoding = self.encoding;
        self.layout
            .iter()
            .zip(&self.fields)
            .map(|(&(off, width, kind), def)| {
                let raw = rec.get(off..(off + width).min(rec.len())).unwrap_or_default();
                if kind == b'I' && raw.len() == 4 {
                    return FieldValue::Integer(i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as i64);
                }
                let raw = raw.split(|&b| b == 0).next().unwrap_or_default();
                let start = raw.iter().position(|&b| b != b' ').unwrap_or(raw.len());
                let end = raw.iter().rposition(|&b| b != b' ').map_or(start, |e| e + 1);
                let v = &raw[start..end];
                let null = match kind {
                    b'N' | b'F' => v.first() == Some(&b'*') || v.is_empty(),
                    b'D' => v.is_empty() || v.starts_with(b"00000000") || v == b"0",
                    b'L' => v.first() == Some(&b'?'),
                    _ => v.is_empty(),
                };
                if null {
                    return FieldValue::Null;
                }
                let ascii = || std::str::from_utf8(v).unwrap_or_default();
                match def.field_type {
                    FieldType::Integer => ascii().parse().map(FieldValue::Integer).unwrap_or(FieldValue::Null),
                    FieldType::Float => ascii().parse().map(FieldValue::Float).unwrap_or(FieldValue::Null),
                    FieldType::Boolean => FieldValue::Boolean(matches!(v.first(), Some(b'T' | b't' | b'Y' | b'y'))),
                    FieldType::Date if v.len() == 8 && v.iter().all(u8::is_ascii_digit) => {
                        let v = ascii();
                        FieldValue::Date(format!("{}-{}-{}", &v[..4], &v[4..6], &v[6..]))
                    }
                    FieldType::Date => FieldValue::Null,
                    _ => FieldValue::Text(encoding.decode(v)),
                }
            })
            .collect()
    }
}

/// Parses a `.dbf` file into field definitions and records.
fn parse_dbf(d: &[u8], cpg: Option<&str>) -> Result<(Vec<FieldDef>, Vec<Vec<FieldValue>>)> {
    let l = dbf_layout(d, cpg)?;
    // The header's record count is not trusted: only the records the file holds are read, and
    // their list is allocated fallibly.
    let held = d.len().saturating_sub(l.header_len) / l.record_len.max(1);
    let count = if l.record_len == 0 { 0 } else { l.count.min(held) };
    let mut records: Vec<Vec<FieldValue>> = Vec::new();
    records
        .try_reserve_exact(count)
        .map_err(|_| Error::InvalidData(format!("the .dbf file declares {count} records, more than memory holds")))?;
    // Every record below `count` is whole in the file; they are decoded in parallel into the
    // list reserved above.
    let record = |i: usize| {
        let start = l.header_len + i * l.record_len;
        l.record(&d[start..start + l.record_len])
    };
    #[cfg(not(target_arch = "wasm32"))]
    crate::par::install(|| {
        use rayon::prelude::*;
        #[allow(clippy::disallowed_methods)] // On terrio's pool, inside `par::install`.
        records.par_extend((0..count).into_par_iter().map(record));
    });
    #[cfg(target_arch = "wasm32")]
    records.extend((0..count).map(record));
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

    /// GDAL 3.12.1 keeps a self-touching Esri ring as stored, in a record of several parts too
    /// (the first case of `compare_shapefile_rings_with_gdal.py`, read by GDAL 3.12.1).
    #[test]
    fn keeps_self_touching_rings_as_gdal_3_12_does() {
        let ring = [(0.0, 0.0), (0.0, 10.0), (10.0, 10.0), (10.0, 0.0), (5.0, 0.0), (6.0, 2.0), (4.0, 2.0), (5.0, 0.0)];
        let hole = [(2.0, 6.0), (3.0, 6.0), (3.0, 7.0), (2.0, 7.0)];
        assert_eq!(polygon_rings(&organize_rings(vec![closed(&ring), closed(&hole)])), [vec![ring.to_vec(), hole.to_vec()]]);
    }

    #[test]
    #[ignore = "GDAL 3.13's split (#315); GDAL 3.12.1, the reference, keeps the rings as stored (SPLIT_SELF_TOUCHING_RINGS)"]
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

    /// A `.dbf` of one text field holding `cell`, its header declaring `count` records and the
    /// language driver id `ldid`.
    fn dbf(cell: &[u8], count: u32, ldid: u8) -> Vec<u8> {
        let width = cell.len() as u8;
        let mut d = vec![3u8, 125, 1, 1];
        d.extend_from_slice(&count.to_le_bytes());
        d.extend_from_slice(&65u16.to_le_bytes());
        d.extend_from_slice(&(1 + u16::from(width)).to_le_bytes());
        d.extend_from_slice(&[0; 17]);
        d.extend_from_slice(&[ldid, 0, 0]);
        d.extend_from_slice(b"NAME\0\0\0\0\0\0\0C\0\0\0\0");
        d.extend_from_slice(&[width, 0]);
        d.extend_from_slice(&[0; 14]);
        d.push(0x0D);
        d.push(b' ');
        d.extend_from_slice(cell);
        d.push(0x1A);
        d
    }

    fn text(d: &[u8], cpg: Option<&str>) -> String {
        let (_, records) = parse_dbf(d, cpg).unwrap();
        match &records[0][0] {
            FieldValue::Text(s) => s.clone(),
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn the_cpg_then_the_language_driver_decide_the_encoding() {
        // As pyogrio 0.13.0 (GDAL 3.12.4) reads them.
        let cp1252 = b"Z\xfcrich \x80";
        assert_eq!(text(&dbf(cp1252, 1, 0), Some("1252")), "Zürich €");
        assert_eq!(text(&dbf(cp1252, 1, 0), Some("CP1252\r\n")), "Zürich €");
        assert_eq!(text(&dbf(cp1252, 1, 0), Some("LATIN1")), "Zürich \u{80}");
        assert_eq!(text(&dbf("Zürich €".as_bytes(), 1, 0x57), Some("UTF-8")), "Zürich €");
        assert_eq!(text(&dbf(cp1252, 1, 0x03), None), "Zürich €");
        assert_eq!(text(&dbf(cp1252, 1, 0x57), None), "Zürich \u{80}");
        // Nothing names an encoding: GDAL does not recode, and pyogrio reads ISO-8859-1.
        assert_eq!(text(&dbf("Zürich".as_bytes(), 1, 0), None), "ZÃ¼rich");
    }

    #[test]
    fn decodes_the_code_pages_gdal_recodes_from() {
        // .cpg values as GDAL's ConvertCodePage reads them, then language driver ids.
        let privet = "Привет";
        assert_eq!(text(&dbf(b"\xcf\xf0\xe8\xe2\xe5\xf2", 1, 0), Some("1251")), privet);
        assert_eq!(text(&dbf(b"\xcf\xf0\xe8\xe2\xe5\xf2", 1, 0), Some("WINDOWS-1251")), privet);
        assert_eq!(text(&dbf(b"\xf0\xd2\xc9\xd7\xc5\xd4", 1, 0), Some("KOI8-R")), privet);
        assert_eq!(text(&dbf(b"\x8f\xe0\xa8\xa2\xa5\xe2", 1, 0), Some("866")), privet);
        assert_eq!(text(&dbf(b"\xa3\xf3d\xbc", 1, 0), Some("88592")), "Łódź");
        assert_eq!(text(&dbf(b"\xa3\xf3d\xbc", 1, 0), Some("ISO-8859-2")), "Łódź");
        assert_eq!(text(&dbf(b"\xc3\xe5\xe9\xdc", 1, 0), Some("1253")), "Γειά");
        assert_eq!(text(&dbf(b"\xa5and\xa3", 1, 0x01), None), "Ñandú");
        assert_eq!(text(&dbf(b"\xa5and\xa3", 1, 0x02), None), "Ñandú");
        assert_eq!(text(&dbf(b"\x8f\xe0\xa8\xa2\xa5\xe2", 1, 0x26), None), privet);
        assert_eq!(text(&dbf(b"\xcf\xf0\xe8\xe2\xe5\xf2", 1, 0xC9), None), privet);
        assert_eq!(text(&dbf(b"\xcf", 1, 0), Some("ANSI 1251")), "П");
        // Undefined bytes are skipped; a name iconv does not know leaves the text unrecoded.
        assert_eq!(text(&dbf(b"a\x81b", 1, 0), Some("1252")), "ab");
        assert_eq!(text(&dbf(b"\xcf", 1, 0), Some("65001")), "\u{cf}");
    }

    #[test]
    fn decodes_multi_byte_code_pages_as_gdal_recodes_them() {
        // As glibc's iconv recodes them in GDAL: undefined bytes skipped, a cut character ends
        // the text, and glibc's tables where they differ from the WHATWG ones.
        assert_eq!(text(&dbf(b"\x80\x93\xfa\x96\x7b", 1, 0), Some("932")), "日本");
        assert_eq!(text(&dbf(b"\xd6\xd0\xce\xc4", 1, 0), Some("936")), "中文");
        assert_eq!(text(&dbf(b"\xd6\xd0\xce", 1, 0), Some("CP936")), "中");
        assert_eq!(text(&dbf(b"\xaa\xa1x", 1, 0), Some("GBK")), "x");
        assert_eq!(text(&dbf(b"\xc7\xd1\xb1\xb9", 1, 0), Some("949")), "한국");
        assert_eq!(text(&dbf(b"\xa2\xe8\xea\xbf", 1, 0), Some("949")), "");
        assert_eq!(text(&dbf(b"\xa2\xe8xyz", 1, 0), Some("UHC")), "yz");
        assert_eq!(text(&dbf(b"\xa4\xa4\xa4\xe5\x80", 1, 0x78), None), "中文\u{80}");
        assert_eq!(text(&dbf(b"\x88\x62", 1, 0), Some("950")), "b");
        assert_eq!(text(&dbf(b"\xc6\xa1", 1, 0), Some("950")), "\u{f6b1}");
    }

    #[test]
    fn the_record_count_of_the_header_is_not_trusted() {
        // 4,294,967,295 records declared, one held: one is read, and nothing is allocated for
        // the others.
        let (_, records) = parse_dbf(&dbf(b"a", u32::MAX, 0), None).unwrap();
        assert_eq!(records.len(), 1);
        assert!(records.capacity() < 1024, "{} records allocated", records.capacity());
    }

    #[test]
    fn corrupt_counts_give_no_geometry() {
        let mut rec = 5i32.to_le_bytes().to_vec();
        rec.extend_from_slice(&[0; 32]);
        rec.extend_from_slice(&(-1i32).to_le_bytes());
        rec.extend_from_slice(&i32::MAX.to_le_bytes());
        assert_eq!(parse_shape(&rec), None);
    }

    #[test]
    fn a_range_reads_the_features_a_whole_read_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.shp");
        let path = path.to_str().unwrap();
        let mut l = Layer::new("r");
        l.add_field(FieldDef::new("id", FieldType::Integer));
        l.add_field(FieldDef::new("name", FieldType::Text));
        for i in 0..50i64 {
            let g = (i % 7 != 3).then(|| Geometry::point(i as f64, -(i as f64)));
            l.push(Feature { fid: i as u64, geometry: g, attributes: vec![FieldValue::Integer(i), FieldValue::Text(format!("f{i}"))] });
        }
        write(&l, path).unwrap();
        let whole = read(path).unwrap();
        let check = |skip: usize, max: Option<usize>| {
            let part = read_range(path, skip, max).unwrap();
            let start = skip.min(whole.features.len());
            let end = max.map_or(whole.features.len(), |m| (start + m).min(whole.features.len()));
            let key = |f: &Feature| (f.fid, f.geometry.clone(), f.attributes.clone());
            let got: Vec<_> = part.features.iter().map(key).collect();
            let want: Vec<_> = whole.features[start..end].iter().map(key).collect();
            assert_eq!(got, want, "{skip} {max:?}");
            let fields = |l: &Layer| l.schema.fields().iter().map(|f| (f.name.clone(), f.field_type, f.width)).collect::<Vec<_>>();
            assert_eq!(fields(&part), fields(&whole));
            assert_eq!(part.crs_epsg(), whole.crs_epsg());
        };
        for (skip, max) in [(0, Some(10)), (3, Some(4)), (49, Some(1)), (45, Some(20)), (60, Some(5)), (10, None), (0, None)] {
            check(skip, max);
        }
        // Without the .shx, the record headers are scanned instead.
        std::fs::remove_file(dir.path().join("r.shx")).unwrap();
        for (skip, max) in [(3, Some(4)), (49, Some(1)), (10, None)] {
            check(skip, max);
        }
    }
}
