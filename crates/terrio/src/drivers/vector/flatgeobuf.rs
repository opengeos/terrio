// Written to match the behavior of:
//   GDAL v3.13.3:
//     ogr/ogrsf_frmts/flatgeobuf/ogrflatgeobuflayer.cpp
// No code is translated from these sources.

//! FlatGeobuf, read and written with the flatbuffers schema of the `flatgeobuf` crate.
//!
//! The whole file is held in memory. Writing sorts the features along a Hilbert curve and writes
//! the packed R-tree spatial index, as GDAL does.

use super::profile::LayerProfile;
use crate::crs::SpatialRef;
use crate::{Error, Result};
use flatbuffers::{FlatBufferBuilder, ForwardsUOffset, Vector, WIPOffset};
pub(crate) use flatgeobuf::packed_r_tree::{NodeItem, PackedRTree, calc_extent, hilbert_sort};
use flatgeobuf::{
    Column, ColumnArgs, ColumnType, Crs, CrsArgs, Feature as FgbFeature, FeatureArgs, Geometry as FgbGeometry, GeometryArgs,
    GeometryType as FgbType, Header, HeaderArgs,
};
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer, Ring};

const MAGIC: [u8; 8] = [b'f', b'g', b'b', 3, b'f', b'g', b'b', 0];
const NODE_SIZE: u16 = 16;

/// Reads a FlatGeobuf file.
pub fn read(path: &str) -> Result<Layer> {
    parse(&std::fs::read(path)?)
}

/// The Z and M flags a FlatGeobuf header declares, read from the header alone.
pub(crate) fn declared_dims_bytes(buf: &[u8]) -> Result<(bool, bool)> {
    let (_, header) = parse_header(buf)?;
    Ok((header.has_z, header.has_m))
}

/// Whether `buf` starts with the FlatGeobuf magic bytes. The version byte is not compared: GDAL
/// writes 1 where [`MAGIC`] has 0, and [`header_len`] reads such files too.
pub(crate) fn has_magic(buf: &[u8]) -> bool {
    header_len(buf).is_ok()
}

fn bad(what: &str) -> Error {
    Error::InvalidData(format!("FlatGeobuf: {what}"))
}

fn verifier() -> flatbuffers::VerifierOptions {
    flatbuffers::VerifierOptions { max_tables: usize::MAX, max_apparent_size: usize::MAX, ..Default::default() }
}

/// What decoding features needs of a file's header.
#[derive(Debug, Clone)]
pub(crate) struct FgbHeader {
    geometry_type: FgbType,
    has_z: bool,
    has_m: bool,
    columns: Vec<ColumnType>,
    /// The number of features (0 when the writer did not know it).
    pub(crate) features_count: u64,
    /// The node size of the packed R-tree spatial index, 0 without one.
    index_node_size: u16,
    /// The extent `[min_x, min_y, max_x, max_y]`, when the header has it.
    extent: Option<[f64; 4]>,
    /// The size of the spatial index following the header, in bytes.
    pub index_size: u64,
}

/// The length of the header (magic bytes and size-prefixed header) from the first 12 bytes of
/// a file.
pub(crate) fn header_len(start: &[u8]) -> Result<usize> {
    if start.len() < 12 || start[..3] != MAGIC[..3] || start[4..7] != MAGIC[4..7] {
        return Err(bad("not a FlatGeobuf file"));
    }
    usize::try_from(u32::from_le_bytes([start[8], start[9], start[10], start[11]]))
        .ok()
        .and_then(|n| n.checked_add(12))
        .ok_or_else(|| bad("header too large for this platform"))
}

/// Reads a file's header (`buf` holds at least its first [`header_len`] bytes): the layer
/// without its features, and what decoding them needs.
pub(crate) fn parse_header(buf: &[u8]) -> Result<(Layer, FgbHeader)> {
    let header_end = header_len(buf)?;
    let header_buf = buf.get(8..header_end).ok_or_else(|| bad("truncated header"))?;
    let header = flatgeobuf::size_prefixed_root_as_header_with_opts(&verifier(), header_buf).map_err(|e| bad(&format!("header: {e}")))?;

    let mut layer = Layer::new(header.name().unwrap_or_default());
    layer.geom_type = from_fgb_type(header.geometry_type());
    if let Some(crs) = header.crs() {
        let org = crs.org().unwrap_or("EPSG");
        if crs.code() > 0 && org.eq_ignore_ascii_case("EPSG") {
            layer.set_crs_epsg(Some(crs.code() as u32));
        }
        if let Some(wkt) = crs.wkt().filter(|w| !w.trim().is_empty()) {
            let s = SpatialRef::from_wkt(wkt);
            layer.set_crs_wkt(Some(wkt.to_string()));
            if layer.crs_epsg().is_none() {
                layer.set_crs_epsg(s.epsg);
            }
        }
    }
    let columns: Vec<Column> = header.columns().map(|c| c.iter().collect()).unwrap_or_default();
    for c in &columns {
        let mut def = FieldDef::new(c.name(), field_type(c.type_()));
        if c.width() > 0 {
            def = def.width(c.width() as usize);
        } else if matches!(c.type_(), ColumnType::Long | ColumnType::ULong | ColumnType::UInt) {
            // GDAL's Integer64 fields: the width keeps them 64-bit when written.
            def = def.width(crate::ops::vector_schema::INTEGER64_WIDTH);
        }
        if c.precision() > 0 {
            def = def.precision(c.precision() as usize);
        }
        layer.add_field(def);
    }
    let count = header.features_count();
    // More features than any file can hold: the index's size would overflow.
    if count > u64::MAX >> 8 {
        return Err(bad("implausible feature count"));
    }
    let meta = FgbHeader {
        geometry_type: header.geometry_type(),
        has_z: header.has_z(),
        has_m: header.has_m(),
        columns: columns.iter().map(|c| c.type_()).collect(),
        features_count: count,
        index_node_size: header.index_node_size(),
        extent: header.envelope().filter(|e| e.len() >= 4).map(|e| [e.get(0), e.get(1), e.get(2), e.get(3)]),
        index_size: index_size(count, header.index_node_size()),
    };
    Ok((layer, meta))
}

/// Decodes a feature: `fbuf` is its size-prefixed flatbuffer.
pub(crate) fn decode_feature(fbuf: &[u8], h: &FgbHeader, fid: u64) -> Result<Feature> {
    decode(fbuf, h, fid, false)
}

/// Decodes a feature [`FgbEncoder::encode`] encoded, keeping the closing coordinate of each ring
/// as it is stored, so that encoding it again gives the same bytes.
pub(crate) fn decode_feature_exact(fbuf: &[u8], h: &FgbHeader, fid: u64) -> Result<Feature> {
    decode(fbuf, h, fid, true)
}

fn decode(fbuf: &[u8], h: &FgbHeader, fid: u64, exact: bool) -> Result<Feature> {
    let f = flatgeobuf::size_prefixed_root_as_feature_with_opts(&verifier(), fbuf).map_err(|e| bad(&format!("feature {fid}: {e}")))?;
    if let Some(g) = f.geometry() {
        validate_geometry(&g, h)?;
    }
    let geometry = f.geometry().and_then(|g| to_geometry(&g, h.geometry_type, h.has_z, h.has_m, exact));
    let mut attributes = vec![FieldValue::Null; h.columns.len()];
    if let Some(props) = f.properties() {
        read_properties(props.bytes(), &h.columns, &mut attributes).map_err(|e| bad(&format!("feature {fid}: {e}")))?;
    }
    Ok(Feature { fid, geometry, attributes })
}

/// Checks coordinate pairs, declared dimensions and part ends before any coordinate slicing.
/// Nested parts obey the same bounds; undeclared and extra ordinate values are ignored.
fn validate_geometry(g: &FgbGeometry, h: &FgbHeader) -> Result<()> {
    let n = g.xy().map_or(0, |xy| xy.len());
    if !n.is_multiple_of(2) {
        return Err(bad("odd XY coordinate count"));
    }
    // Only the dimensions the header declares are read. Undeclared vectors and extra values
    // are ignored, as in GDAL; declared dimensions still need enough values for every XY pair.
    for (declared, values) in [(h.has_z, g.z()), (h.has_m, g.m())] {
        match values {
            _ if !declared => {}
            Some(values) if values.len() < n / 2 => return Err(bad("short coordinate dimension")),
            None if n != 0 => return Err(bad("missing coordinate dimension")),
            _ => {}
        }
    }
    if let Some(ends) = g.ends() {
        let mut previous = 0;
        for end in ends {
            let end = end as usize;
            if end < previous || end > n / 2 {
                return Err(bad("geometry part ends decrease or exceed the coordinate count"));
            }
            previous = end;
        }
    }
    if let Some(parts) = g.parts() {
        for p in parts {
            validate_geometry(&p, h)?;
        }
    }
    Ok(())
}

/// The size in bytes of the packed R-tree of `count` features, 0 without one. Computed in 64
/// bits, so that it does not overflow in 32-bit WebAssembly.
fn index_size(count: u64, node_size: u16) -> u64 {
    level_bounds(count, node_size).first().map_or(0, |l| l.1 * NODE_ITEM_LEN)
}

/// The size of an index node item: a bounding box and an offset.
const NODE_ITEM_LEN: u64 = 40;

/// The node ranges `[start, end)` of each level of a packed R-tree, the leaves first, as the
/// FlatGeobuf specification lays them out (the root first in the file).
fn level_bounds(count: u64, node_size: u16) -> Vec<(u64, u64)> {
    if count == 0 || node_size < 2 {
        return Vec::new();
    }
    let ns = node_size as u64;
    let mut n = count;
    let mut level_counts = vec![n];
    // At least two levels, as the reference implementation lays out one feature.
    loop {
        n = n.div_ceil(ns);
        level_counts.push(n);
        if n == 1 {
            break;
        }
    }
    let total: u64 = level_counts.iter().sum();
    // Levels from the root down start at the file's first node; the leaves are last.
    let mut out = Vec::with_capacity(level_counts.len());
    let mut end = total;
    for c in &level_counts {
        out.push((end - c, end));
        end -= c;
    }
    out
}

/// A complete size-prefixed feature and its next offset, or EOF at a record boundary.
fn next_feature(data: &[u8], pos: usize) -> Result<Option<(&[u8], usize)>> {
    if pos == data.len() {
        return Ok(None);
    }
    let prefix = data.get(pos..).and_then(|tail| tail.get(..4)).ok_or_else(|| bad("truncated feature size"))?;
    let len = u32::from_le_bytes(prefix.try_into().unwrap()) as usize;
    let end = pos.checked_add(4).and_then(|p| p.checked_add(len)).ok_or_else(|| bad("feature size overflow"))?;
    let feature = data.get(pos..end).ok_or_else(|| bad("truncated feature"))?;
    Ok(Some((feature, end)))
}

/// Parses a FlatGeobuf file held in memory.
pub fn parse(buf: &[u8]) -> Result<Layer> {
    let (mut layer, h) = parse_header(buf)?;
    let index_size = usize::try_from(h.index_size).map_err(|_| bad("an index too large for this platform"))?;
    let mut pos = header_len(buf)?.checked_add(index_size).filter(|&p| p <= buf.len()).ok_or_else(|| bad("truncated index"))?;
    let mut fid = 0u64;
    while let Some((fbuf, next)) = next_feature(buf, pos)? {
        pos = next;
        layer.push(decode_feature(fbuf, &h, fid)?);
        fid += 1;
    }
    check_count(fid, h.features_count)?;
    Ok(layer)
}

pub(crate) fn check_count(actual: u64, declared: u64) -> Result<()> {
    if declared != 0 && actual != declared {
        return Err(bad("feature count differs from header"));
    }
    Ok(())
}

/// What a FlatGeobuf header says about the file.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FlatGeobufInfo {
    /// The layer name.
    pub name: String,
    /// The number of features (0 when the writer did not know it).
    pub feature_count: u64,
    /// The extent `[min_x, min_y, max_x, max_y]`, when the header has it.
    pub extent: Option<[f64; 4]>,
    /// The node size of the packed R-tree spatial index, 0 without one.
    pub index_node_size: u16,
    /// The size of the header (magic bytes included).
    pub header_size: u64,
    /// The size of the spatial index.
    pub index_size: u64,
    /// Whether the header declares Z coordinates for every geometry.
    pub has_z: bool,
    /// Whether the header declares M coordinates for every geometry.
    pub has_m: bool,
}

/// Reads a FlatGeobuf file's header, and the features whose bounding box intersects `bbox`
/// `(min_x, min_y, max_x, max_y)`, found with the packed R-tree spatial index: only the index
/// nodes on the way and those features are read. All features without a filter (or without an
/// index: `index_size` is then 0, and the caller filters them, as it does the bounding-box
/// matches by their geometries). Feature ids are the features' positions in the file, as in GDAL.
///
/// `prefetch` is told the byte ranges `(offset, length)` about to be read: the index nodes of a
/// level, then the features (see [`crate::sparse`]). Offsets are 64-bit throughout, so that
/// files larger than 4 GB are read in 32-bit WebAssembly too.
pub fn read_filtered<R: std::io::Read + std::io::Seek>(
    r: &mut R,
    file_len: u64,
    bbox: Option<(f64, f64, f64, f64)>,
    prefetch: crate::sparse::Prefetch<'_>,
) -> Result<(FlatGeobufInfo, Layer)> {
    read_impl(r, file_len, bbox, false, prefetch, None)
}

/// Reads a FlatGeobuf file's header, and the layer it describes, without features.
pub fn read_header<R: std::io::Read + std::io::Seek>(r: &mut R, file_len: u64) -> Result<(FlatGeobufInfo, Layer)> {
    read_impl(r, file_len, None, true, &|_| Ok(()), None)
}

/// Parsed header state reused by an open indexed catalog.
pub(crate) struct PreparedIndex {
    pub info: FlatGeobufInfo,
    pub layer: Layer,
    pub metadata: Option<String>,
    header: FgbHeader,
    file_len: u64,
}
impl PreparedIndex {
    pub(crate) fn open<R: std::io::Read + std::io::Seek>(reader: &mut R, file_len: u64) -> Result<Self> {
        use std::io::SeekFrom;
        if file_len < 12 {
            return Err(bad("truncated header"));
        }
        reader.seek(SeekFrom::Start(0))?;
        let mut start = [0u8; 12];
        reader.read_exact(&mut start)?;
        let size = header_len(&start)?;
        if size as u64 > file_len {
            return Err(bad("truncated header"));
        }
        reader.seek(SeekFrom::Start(0))?;
        let mut bytes = vec![0u8; size];
        reader.read_exact(&mut bytes)?;
        let (layer, header) = parse_header(&bytes)?;
        let data_start = (size as u64).checked_add(header.index_size).filter(|v| *v <= file_len).ok_or_else(|| bad("truncated index"))?;
        let metadata = flatgeobuf::size_prefixed_root_as_header_with_opts(&verifier(), &bytes[8..])
            .ok()
            .and_then(|h| h.metadata().map(str::to_string));
        let info = FlatGeobufInfo {
            name: layer.name.clone(),
            feature_count: header.features_count,
            extent: header.extent,
            index_node_size: header.index_node_size,
            header_size: size as u64,
            index_size: data_start - size as u64,
            has_z: header.has_z,
            has_m: header.has_m,
        };
        Ok(Self { info, layer, metadata, header, file_len })
    }
    pub(crate) fn query<R: std::io::Read + std::io::Seek>(&self, reader: &mut R, bbox: Option<(f64, f64, f64, f64)>) -> Result<Layer> {
        read_impl(reader, self.file_len, bbox, false, &|_| Ok(()), Some(self)).map(|(_, layer)| layer)
    }
    pub(crate) fn first<R: std::io::Read + std::io::Seek>(&self, reader: &mut R) -> Result<Layer> {
        use std::io::SeekFrom;
        let start = self.info.header_size + self.info.index_size;
        let mut layer = self.layer.clone();
        if start == self.file_len {
            return Ok(layer);
        }
        reader.seek(SeekFrom::Start(start))?;
        let mut prefix = [0u8; 4];
        reader.read_exact(&mut prefix)?;
        let size = u32::from_le_bytes(prefix) as u64 + 4;
        if start.checked_add(size).is_none_or(|end| end > self.file_len) {
            return Err(bad("truncated feature"));
        }
        let mut bytes = vec![0u8; usize::try_from(size).map_err(|_| bad("feature too large"))?];
        bytes[..4].copy_from_slice(&prefix);
        reader.read_exact(&mut bytes[4..])?;
        layer.push(decode_feature(&bytes, &self.header, 0)?);
        Ok(layer)
    }
}

fn read_impl<R: std::io::Read + std::io::Seek>(
    r: &mut R,
    file_len: u64,
    bbox: Option<(f64, f64, f64, f64)>,
    header_only: bool,
    prefetch: crate::sparse::Prefetch<'_>,
    prepared: Option<&PreparedIndex>,
) -> Result<(FlatGeobufInfo, Layer)> {
    use std::io::SeekFrom;
    let owned;
    let prepared = if let Some(prepared) = prepared {
        prepared
    } else {
        owned = PreparedIndex::open(r, file_len)?;
        &owned
    };
    let mut layer = prepared.layer.clone();
    let header = &prepared.header;
    let info = prepared.info.clone();
    // Lengths and offsets come from the file: they are checked against its size before anything
    // is allocated.
    let mut read_at = |offset: u64, len: u64| -> Result<Vec<u8>> {
        if offset.checked_add(len).is_none_or(|end| end > file_len) {
            return Err(bad("truncated file"));
        }
        r.seek(SeekFrom::Start(offset))?;
        let mut b = vec![0u8; usize::try_from(len).map_err(|_| bad("a read too large for this platform"))?];
        r.read_exact(&mut b)?;
        Ok(b)
    };
    let count = header.features_count;
    let node_size = header.index_node_size;
    let index_start = info.header_size;
    let isize = header.index_size;
    let data_start = index_start + isize;
    if header_only {
        return Ok((info, layer));
    }

    // The features to read: (fid, offset in the data section, end offset or None for the file's
    // end).
    let mut wanted: Vec<(u64, u64, Option<u64>)> = Vec::new();
    match bbox {
        Some((x0, y0, x1, y1)) if isize > 0 => {
            let levels = level_bounds(count, node_size);
            let (leaf_start, leaf_end) = levels[0];
            let ns = node_size as u64;
            // Nodes to visit at the current level: (first node, end), read a level at a time.
            let root = *levels.last().unwrap_or(&(0, 1));
            let mut current: Vec<(u64, u64)> = vec![root];
            for level in (0..levels.len()).rev() {
                let is_leaf = level == 0;
                // Leaves are read with the next item, whose offset ends the last feature found.
                let ranges: Vec<(u64, u64)> = current
                    .iter()
                    .map(|&(a, b)| {
                        let b = if is_leaf { (b + 1).min(leaf_end) } else { b };
                        (index_start + a * NODE_ITEM_LEN, (b - a) * NODE_ITEM_LEN)
                    })
                    .collect();
                prefetch(&ranges)?;
                let mut next = Vec::new();
                for (&(a, b), &(off, len)) in current.iter().zip(&ranges) {
                    let bytes = read_at(off, len)?;
                    let item = |k: usize| -> (f64, f64, f64, f64, u64) {
                        let c = &bytes[k * 40..k * 40 + 40];
                        let f = |i: usize| f64::from_le_bytes(c[i * 8..i * 8 + 8].try_into().unwrap_or_default());
                        (f(0), f(1), f(2), f(3), u64::from_le_bytes(c[32..40].try_into().unwrap_or_default()))
                    };
                    for k in 0..(b - a) as usize {
                        let (nx0, ny0, nx1, ny1, offset) = item(k);
                        if nx0 > x1 || ny0 > y1 || nx1 < x0 || ny1 < y0 {
                            continue;
                        }
                        if is_leaf {
                            let fid = a + k as u64 - leaf_start;
                            let end = ((k + 1) * 40 < bytes.len()).then(|| item(k + 1).4);
                            wanted.push((fid, offset, end));
                        } else {
                            // `offset` is the index of the child node's first item.
                            let child_level = levels[level - 1];
                            if offset < child_level.0 || offset >= child_level.1 {
                                return Err(bad("an index node points outside its level"));
                            }
                            next.push((offset, (offset + ns).min(child_level.1)));
                        }
                    }
                }
                // Merge adjacent nodes into runs, to read them together.
                next.sort_unstable();
                let mut merged: Vec<(u64, u64)> = Vec::new();
                for (a, b) in next {
                    match merged.last_mut() {
                        Some(last) if last.1 == a => last.1 = b,
                        _ => merged.push((a, b)),
                    }
                }
                current = merged;
                if current.is_empty() {
                    break;
                }
            }
            wanted.sort_unstable_by_key(|w| w.1);
        }
        _ => {
            // Every feature, in order: read the data section whole.
            let data = read_at(data_start, file_len.saturating_sub(data_start))?;
            let mut pos = 0usize;
            let mut fid = 0u64;
            while let Some((fbuf, next)) = next_feature(&data, pos)? {
                pos = next;
                layer.push(decode_feature(fbuf, header, fid)?);
                fid += 1;
            }
            check_count(fid, count)?;
            return Ok((info, layer));
        }
    }
    let ranges: Vec<(u64, u64)> = wanted
        .iter()
        .map(|&(_, off, end)| {
            let at = data_start.saturating_add(off);
            (at, end.map_or(file_len.saturating_sub(at), |e| e.saturating_sub(off)))
        })
        .collect();
    prefetch(&ranges)?;
    for (&(fid, ..), &(off, _)) in wanted.iter().zip(&ranges) {
        let size = read_at(off, 4)?;
        let len = u32::from_le_bytes([size[0], size[1], size[2], size[3]]) as u64;
        let fbuf = read_at(off, 4 + len)?;
        layer.push(decode_feature(&fbuf, header, fid)?);
    }
    Ok((info, layer))
}

fn from_fgb_type(t: FgbType) -> Option<GeometryType> {
    Some(match t {
        FgbType::Point => GeometryType::Point,
        FgbType::LineString => GeometryType::LineString,
        FgbType::Polygon => GeometryType::Polygon,
        FgbType::MultiPoint => GeometryType::MultiPoint,
        FgbType::MultiLineString => GeometryType::MultiLineString,
        FgbType::MultiPolygon => GeometryType::MultiPolygon,
        FgbType::GeometryCollection => GeometryType::GeometryCollection,
        _ => return None,
    })
}

fn field_type(t: ColumnType) -> FieldType {
    match t {
        ColumnType::Bool => FieldType::Boolean,
        ColumnType::Float | ColumnType::Double => FieldType::Float,
        ColumnType::String => FieldType::Text,
        ColumnType::Json => FieldType::Json,
        ColumnType::DateTime => FieldType::DateTime,
        ColumnType::Binary => FieldType::Blob,
        _ => FieldType::Integer,
    }
}

fn read_properties(mut b: &[u8], columns: &[ColumnType], out: &mut [FieldValue]) -> std::result::Result<(), String> {
    fn take<'a>(b: &mut &'a [u8], n: usize) -> std::result::Result<&'a [u8], String> {
        if b.len() < n {
            return Err("truncated properties".into());
        }
        let (head, tail) = b.split_at(n);
        *b = tail;
        Ok(head)
    }
    fn arr<const N: usize>(b: &mut &[u8]) -> std::result::Result<[u8; N], String> {
        Ok(take(b, N)?.try_into().unwrap_or([0; N]))
    }
    while !b.is_empty() {
        let i = u16::from_le_bytes(arr(&mut b)?) as usize;
        let col = columns.get(i).ok_or_else(|| format!("column {i} out of range"))?;
        let v = match *col {
            ColumnType::Byte => FieldValue::Integer(i8::from_le_bytes(arr(&mut b)?) as i64),
            ColumnType::UByte => FieldValue::Integer(u8::from_le_bytes(arr(&mut b)?) as i64),
            ColumnType::Bool => FieldValue::Boolean(u8::from_le_bytes(arr(&mut b)?) != 0),
            ColumnType::Short => FieldValue::Integer(i16::from_le_bytes(arr(&mut b)?) as i64),
            ColumnType::UShort => FieldValue::Integer(u16::from_le_bytes(arr(&mut b)?) as i64),
            ColumnType::Int => FieldValue::Integer(i32::from_le_bytes(arr(&mut b)?) as i64),
            ColumnType::UInt => FieldValue::Integer(u32::from_le_bytes(arr(&mut b)?) as i64),
            ColumnType::Long => FieldValue::Integer(i64::from_le_bytes(arr(&mut b)?)),
            ColumnType::ULong => FieldValue::Integer(u64::from_le_bytes(arr(&mut b)?) as i64),
            ColumnType::Float => FieldValue::Float(f32::from_le_bytes(arr(&mut b)?) as f64),
            ColumnType::Double => FieldValue::Float(f64::from_le_bytes(arr(&mut b)?)),
            t => {
                let n = u32::from_le_bytes(arr(&mut b)?) as usize;
                let bytes = take(&mut b, n)?;
                match t {
                    ColumnType::Binary => FieldValue::Blob(bytes.to_vec()),
                    ColumnType::DateTime => FieldValue::DateTime(String::from_utf8_lossy(bytes).into_owned()),
                    _ => FieldValue::Text(String::from_utf8_lossy(bytes).into_owned()),
                }
            }
        };
        if let Some(slot) = out.get_mut(i) {
            *slot = v;
        }
    }
    Ok(())
}

fn coords(g: &FgbGeometry, has_z: bool, has_m: bool) -> Vec<Coord> {
    let Some(xy) = g.xy() else { return Vec::new() };
    let z = if has_z { g.z() } else { None };
    let m = if has_m { g.m() } else { None };
    (0..xy.len() / 2).map(|i| Coord { x: xy.get(2 * i), y: xy.get(2 * i + 1), z: z.map(|z| z.get(i)), m: m.map(|m| m.get(i)) }).collect()
}

/// Splits coordinates at the `ends` offsets (one part when there are none).
fn split(cs: Vec<Coord>, ends: Option<Vector<u32>>) -> Vec<Vec<Coord>> {
    match ends {
        Some(ends) if ends.len() > 1 => {
            let mut parts = Vec::with_capacity(ends.len());
            let mut start = 0;
            for e in ends.iter() {
                let e = (e as usize).min(cs.len());
                parts.push(cs[start..e].to_vec());
                start = e;
            }
            parts
        }
        _ => vec![cs],
    }
}

/// A ring without its closing coordinate (with it, as stored, when `exact`).
fn ring(mut cs: Vec<Coord>, exact: bool) -> Ring {
    if !exact && cs.len() > 1 && cs.first().map(|c| (c.x, c.y)) == cs.last().map(|c| (c.x, c.y)) {
        cs.pop();
    }
    Ring::new(cs)
}

fn polygon_rings(g: &FgbGeometry, has_z: bool, has_m: bool, exact: bool) -> Option<(Ring, Vec<Ring>)> {
    let mut rings = split(coords(g, has_z, has_m), g.ends()).into_iter().map(|cs| ring(cs, exact));
    let exterior = rings.next()?;
    Some((exterior, rings.collect()))
}

fn to_geometry(g: &FgbGeometry, header_type: FgbType, has_z: bool, has_m: bool, exact: bool) -> Option<Geometry> {
    let t = if header_type == FgbType::Unknown { g.type_() } else { header_type };
    Some(match t {
        FgbType::Point => Geometry::Point(coords(g, has_z, has_m).into_iter().next()?),
        FgbType::MultiPoint => Geometry::MultiPoint(coords(g, has_z, has_m)),
        FgbType::LineString => Geometry::LineString(coords(g, has_z, has_m)),
        FgbType::MultiLineString => Geometry::MultiLineString(split(coords(g, has_z, has_m), g.ends())),
        FgbType::Polygon => {
            let (exterior, interiors) = polygon_rings(g, has_z, has_m, exact)?;
            Geometry::Polygon { exterior, interiors }
        }
        FgbType::MultiPolygon => match g.parts() {
            Some(parts) => Geometry::MultiPolygon(parts.iter().filter_map(|p| polygon_rings(&p, has_z, has_m, exact)).collect()),
            // A multipolygon of one polygon may be stored without parts.
            None => Geometry::MultiPolygon(polygon_rings(g, has_z, has_m, exact).into_iter().collect()),
        },
        FgbType::GeometryCollection => Geometry::GeometryCollection(
            g.parts().map(|ps| ps.iter().filter_map(|p| to_geometry(&p, p.type_(), has_z, has_m, exact)).collect()).unwrap_or_default(),
        ),
        _ => return None,
    })
}

/// Writes a layer as FlatGeobuf, with a spatial index when every feature has a geometry.
pub fn write(layer: &Layer, path: &str) -> Result<()> {
    std::fs::write(path, to_bytes(layer)?)?;
    Ok(())
}

/// Encodes a layer as a FlatGeobuf file.
pub fn to_bytes(layer: &Layer) -> Result<Vec<u8>> {
    to_bytes_with_metadata(layer, None)
}

/// Encodes layer metadata in the FlatGeobuf header (GTI indexes).
pub(crate) fn to_bytes_with_metadata(layer: &Layer, metadata: Option<&str>) -> Result<Vec<u8>> {
    let profile = LayerProfile::of(layer);
    let enc = FgbEncoder::new(layer, &profile);
    let encoded = enc.encode(&layer.features, 0);
    let (features, mut nodes): (Vec<Vec<u8>>, Vec<NodeItem>) = encoded.into_iter().unzip();
    let index = !profile.geometry.missing_bbox && !features.is_empty();
    let extent = calc_extent(&nodes);
    let mut out = Vec::with_capacity(features.iter().map(Vec::len).sum::<usize>() + 4096);
    out.extend_from_slice(&enc.header_with_metadata(layer, features.len() as u64, &extent, index, metadata));
    if index {
        let (tree, order) = sorted_index(&mut nodes, &extent, |i| features[i].len() as u64)?;
        tree.stream_write(&mut out)?;
        for i in order {
            out.extend_from_slice(&features[i as usize]);
        }
    } else {
        for f in &features {
            out.extend_from_slice(f);
        }
    }
    Ok(out)
}

/// Sorts index nodes (whose `offset` holds the feature number) along the Hilbert curve and
/// builds the packed R-tree over them, with the byte offsets of the features in that order
/// (`len` gives the size of feature `i`). Returns the tree and the feature numbers in order; the
/// nodes are left holding the byte offsets.
pub(crate) fn sorted_index(nodes: &mut [NodeItem], extent: &NodeItem, len: impl Fn(usize) -> u64) -> Result<(PackedRTree, Vec<u64>)> {
    hilbert_sort(nodes, extent);
    let order: Vec<u64> = nodes.iter().map(|n| n.offset).collect();
    let mut offset = 0u64;
    for n in nodes.iter_mut() {
        let size = len(n.offset as usize);
        n.offset = offset;
        offset += size;
    }
    let tree = PackedRTree::build(nodes, extent, NODE_SIZE).map_err(|e| Error::InvalidData(format!("FlatGeobuf index: {e}")))?;
    Ok((tree, order))
}

/// Encodes the features of a layer, chunk by chunk, with the types its profile decides.
pub(crate) struct FgbEncoder {
    geom_type: FgbType,
    has_z: bool,
    col_types: Vec<ColumnType>,
}

impl FgbEncoder {
    /// An encoder for the features of `layer` (its fields; its features are not read), whose
    /// features `profile` describes.
    pub(crate) fn new(layer: &Layer, profile: &LayerProfile) -> Self {
        let fields = layer.schema.fields();
        Self {
            geom_type: layer_type(layer, profile),
            has_z: profile.geometry.has_z,
            col_types: fields.iter().enumerate().map(|(i, f)| column_type(profile, i, f)).collect(),
        }
    }

    /// Whether `other` encodes features as this encoder does (same geometry type, Z and column
    /// types).
    pub(crate) fn same_encoding(&self, other: &FgbEncoder) -> bool {
        (self.geom_type, self.has_z, &self.col_types) == (other.geom_type, other.has_z, &other.col_types)
    }

    /// What [`decode_feature`] needs to decode the features this encoder encodes.
    pub(crate) fn decoder(&self) -> FgbHeader {
        FgbHeader {
            geometry_type: self.geom_type,
            has_z: self.has_z,
            has_m: false,
            columns: self.col_types.clone(),
            features_count: 0,
            index_node_size: 0,
            extent: None,
            index_size: 0,
        }
    }

    /// Each feature as a size-prefixed flatbuffer, with its index node (bounds, and the
    /// feature's number from `first` in `offset`), encoded in parallel.
    pub(crate) fn encode(&self, features: &[Feature], first: u64) -> Vec<(Vec<u8>, NodeItem)> {
        let (geom_type, has_z, col_types) = (self.geom_type, self.has_z, &self.col_types);
        crate::par::map_range(features.len(), |i| {
            let f = &features[i];
            // Sized for the coordinates, so that the buffer is not regrown as it fills.
            let coords = f.geometry.as_ref().map_or(0, crate::vector::coord_count);
            let mut fbb = FlatBufferBuilder::with_capacity(1024 + coords * if has_z { 24 } else { 16 } + f.attributes.len() * 16);
            let mut node = NodeItem::create(first + i as u64);
            if let Some(b) = f.geometry.as_ref().and_then(|g| g.bbox()) {
                node.min_x = b.min_x;
                node.min_y = b.min_y;
                node.max_x = b.max_x;
                node.max_y = b.max_y;
            }
            let geometry = f.geometry.as_ref().map(|g| build_geometry(&mut fbb, g, geom_type == FgbType::Unknown, has_z));
            let mut props = Vec::new();
            for (ci, (v, t)) in f.attributes.iter().zip(col_types).enumerate() {
                write_property(&mut props, ci as u16, v, *t);
            }
            let properties = (!props.is_empty()).then(|| fbb.create_vector(&props));
            let feature = FgbFeature::create(&mut fbb, &FeatureArgs { geometry, properties, columns: None });
            fbb.finish_size_prefixed(feature, None);
            (fbb.finished_data().to_vec(), node)
        })
    }

    /// The magic bytes and header of a file of `count` features within `extent`.
    pub(crate) fn header(&self, layer: &Layer, count: u64, extent: &NodeItem, index: bool) -> Vec<u8> {
        self.header_with_metadata(layer, count, extent, index, None)
    }

    fn header_with_metadata(&self, layer: &Layer, count: u64, extent: &NodeItem, index: bool, metadata: Option<&str>) -> Vec<u8> {
        let fields = layer.schema.fields();
        let mut fbb = FlatBufferBuilder::new();
        let name = fbb.create_string(&layer.name);
        let columns: Vec<WIPOffset<Column>> = fields
            .iter()
            .zip(&self.col_types)
            .map(|(f, t)| {
                let name = fbb.create_string(&f.name);
                let width = if f.field_type == FieldType::Text { f.width as i32 } else { 0 };
                Column::create(&mut fbb, &ColumnArgs { name: Some(name), type_: *t, width, nullable: true, ..Default::default() })
            })
            .collect();
        let columns: Option<WIPOffset<Vector<ForwardsUOffset<Column>>>> = (!columns.is_empty()).then(|| fbb.create_vector(&columns));
        let srs = SpatialRef::from_layer(layer);
        let code = super::trusted_epsg(layer);
        let crs = (!srs.is_unknown()).then(|| {
            let wkt = super::output_wkt(layer).map(|w| fbb.create_string(&w));
            let org = code.map(|_| fbb.create_string("EPSG"));
            Crs::create(&mut fbb, &CrsArgs { org, code: code.unwrap_or(0) as i32, wkt, ..Default::default() })
        });
        let envelope = extent.min_x.is_finite().then(|| fbb.create_vector(&[extent.min_x, extent.min_y, extent.max_x, extent.max_y]));
        let metadata = metadata.map(|text| fbb.create_string(text));
        let header = Header::create(
            &mut fbb,
            &HeaderArgs {
                name: Some(name),
                envelope,
                geometry_type: self.geom_type,
                has_z: self.has_z,
                columns,
                features_count: count,
                index_node_size: if index { NODE_SIZE } else { 0 },
                crs,
                metadata,
                ..Default::default()
            },
        );
        fbb.finish_size_prefixed(header, None);
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(fbb.finished_data());
        out
    }
}

/// The geometry type to declare: the type every feature has, as GDAL declares it, and `Unknown`
/// (each geometry carrying its own type) otherwise. Without geometries, the declared type.
fn layer_type(layer: &Layer, profile: &LayerProfile) -> FgbType {
    let t = match profile.uniform_type() {
        Some(t) => Some(t),
        None if profile.no_geometry() => layer.geom_type,
        None => None,
    };
    match t {
        Some(GeometryType::Point) => FgbType::Point,
        Some(GeometryType::LineString) => FgbType::LineString,
        Some(GeometryType::Polygon) => FgbType::Polygon,
        Some(GeometryType::MultiPoint) => FgbType::MultiPoint,
        Some(GeometryType::MultiLineString) => FgbType::MultiLineString,
        Some(GeometryType::MultiPolygon) => FgbType::MultiPolygon,
        Some(GeometryType::GeometryCollection) => FgbType::GeometryCollection,
        None => FgbType::Unknown,
    }
}

fn column_type(profile: &LayerProfile, i: usize, f: &FieldDef) -> ColumnType {
    match f.field_type {
        FieldType::Integer => {
            // Integer fields at least 10 wide are 64-bit (GDAL's Integer64), whatever their values.
            if f.width < 10 && profile.fields.get(i).is_none_or(|p| p.no_int_beyond_int32) { ColumnType::Int } else { ColumnType::Long }
        }
        FieldType::Float => ColumnType::Double,
        FieldType::Boolean => ColumnType::Bool,
        FieldType::Blob => ColumnType::Binary,
        FieldType::Date | FieldType::DateTime => ColumnType::DateTime,
        FieldType::Json => ColumnType::Json,
        FieldType::Text => ColumnType::String,
    }
}

fn write_property(out: &mut Vec<u8>, i: u16, v: &FieldValue, t: ColumnType) {
    let mut put = |bytes: &[u8]| {
        out.extend_from_slice(&i.to_le_bytes());
        out.extend_from_slice(bytes);
    };
    let text = |s: &str| [&(s.len() as u32).to_le_bytes()[..], s.as_bytes()].concat();
    match (t, v) {
        (_, FieldValue::Null) => {}
        (ColumnType::Int, v) => put(&(v.as_i64().unwrap_or_default() as i32).to_le_bytes()),
        (ColumnType::Long, v) => put(&v.as_i64().unwrap_or_default().to_le_bytes()),
        (ColumnType::Double, v) => {
            if let Some(x) = v.as_f64() {
                put(&x.to_le_bytes());
            }
        }
        (ColumnType::Bool, v) => put(&[v.as_bool().unwrap_or_default() as u8]),
        (ColumnType::Binary, FieldValue::Blob(b)) => put(&[&(b.len() as u32).to_le_bytes()[..], b].concat()),
        (_, FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s)) => put(&text(s)),
        (_, FieldValue::Integer(x)) => put(&text(&x.to_string())),
        (_, FieldValue::Float(x)) => put(&text(&x.to_string())),
        (_, FieldValue::Boolean(x)) => put(&text(&x.to_string())),
        (_, FieldValue::Blob(_)) => {}
    }
}

fn fgb_type(g: &Geometry) -> FgbType {
    match g {
        Geometry::Point(_) => FgbType::Point,
        Geometry::LineString(_) => FgbType::LineString,
        Geometry::Polygon { .. } => FgbType::Polygon,
        Geometry::MultiPoint(_) => FgbType::MultiPoint,
        Geometry::MultiLineString(_) => FgbType::MultiLineString,
        Geometry::MultiPolygon(_) => FgbType::MultiPolygon,
        Geometry::GeometryCollection(_) => FgbType::GeometryCollection,
    }
}

/// Builds a geometry table. `typed` stores the type in the geometry (layers of mixed types and
/// collection parts).
fn build_geometry<'a>(fbb: &mut FlatBufferBuilder<'a>, g: &Geometry, typed: bool, has_z: bool) -> WIPOffset<FgbGeometry<'a>> {
    let mut xy = Vec::new();
    let mut z = Vec::new();
    let mut ends = Vec::new();
    let push = |cs: &[Coord], close: bool, xy: &mut Vec<f64>, z: &mut Vec<f64>| {
        for c in cs {
            xy.extend_from_slice(&[c.x, c.y]);
            z.push(c.z.unwrap_or(0.0));
        }
        if close && cs.len() > 1 && (cs[0].x, cs[0].y) != (cs[cs.len() - 1].x, cs[cs.len() - 1].y) {
            xy.extend_from_slice(&[cs[0].x, cs[0].y]);
            z.push(cs[0].z.unwrap_or(0.0));
        }
    };
    let mut parts = Vec::new();
    match g {
        Geometry::Point(c) => push(std::slice::from_ref(c), false, &mut xy, &mut z),
        Geometry::LineString(cs) | Geometry::MultiPoint(cs) => push(cs, false, &mut xy, &mut z),
        Geometry::MultiLineString(ls) => {
            for l in ls {
                push(l, false, &mut xy, &mut z);
                ends.push((xy.len() / 2) as u32);
            }
        }
        Geometry::Polygon { exterior, interiors } => {
            for r in std::iter::once(exterior).chain(interiors) {
                push(&r.0, true, &mut xy, &mut z);
                ends.push((xy.len() / 2) as u32);
            }
        }
        Geometry::MultiPolygon(ps) => {
            for (exterior, interiors) in ps {
                parts.push(build_polygon(fbb, exterior, interiors, has_z));
            }
        }
        Geometry::GeometryCollection(gs) => {
            for p in gs {
                parts.push(build_geometry(fbb, p, true, has_z));
            }
        }
    }
    // A single part needs no ends.
    if ends.len() <= 1 {
        ends.clear();
    }
    let args = GeometryArgs {
        ends: (!ends.is_empty()).then(|| fbb.create_vector(&ends)),
        xy: (!xy.is_empty()).then(|| fbb.create_vector(&xy)),
        z: (has_z && !z.is_empty()).then(|| fbb.create_vector(&z)),
        parts: (!parts.is_empty()).then(|| fbb.create_vector(&parts)),
        type_: if typed { fgb_type(g) } else { FgbType::Unknown },
        ..Default::default()
    };
    FgbGeometry::create(fbb, &args)
}

/// Builds the geometry table of a polygon part of a multipolygon, as [`build_geometry`] builds an
/// untyped polygon.
fn build_polygon<'a>(fbb: &mut FlatBufferBuilder<'a>, exterior: &Ring, interiors: &[Ring], has_z: bool) -> WIPOffset<FgbGeometry<'a>> {
    let (mut xy, mut z, mut ends) = (Vec::new(), Vec::new(), Vec::new());
    for r in std::iter::once(exterior).chain(interiors) {
        let cs = &r.0;
        for c in cs {
            xy.extend_from_slice(&[c.x, c.y]);
            z.push(c.z.unwrap_or(0.0));
        }
        if cs.len() > 1 && (cs[0].x, cs[0].y) != (cs[cs.len() - 1].x, cs[cs.len() - 1].y) {
            xy.extend_from_slice(&[cs[0].x, cs[0].y]);
            z.push(cs[0].z.unwrap_or(0.0));
        }
        ends.push((xy.len() / 2) as u32);
    }
    // A single part needs no ends.
    if ends.len() <= 1 {
        ends.clear();
    }
    let args = GeometryArgs {
        ends: (!ends.is_empty()).then(|| fbb.create_vector(&ends)),
        xy: (!xy.is_empty()).then(|| fbb.create_vector(&xy)),
        z: (has_z && !z.is_empty()).then(|| fbb.create_vector(&z)),
        type_: FgbType::Unknown,
        ..Default::default()
    };
    FgbGeometry::create(fbb, &args)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests decoder admission directly, without the convenience API's optional GDAL fallback.
    fn accepts(bytes: &[u8]) -> bool {
        parse(bytes).is_ok()
    }

    /// A point file with independently declared dimensions and optional ordinate vectors.
    fn dimension_fixture(has_z: bool, has_m: bool, z: Option<&[f64]>, m: Option<&[f64]>) -> Vec<u8> {
        let mut b = FlatBufferBuilder::new();
        let h = Header::create(
            &mut b,
            &HeaderArgs { features_count: 1, index_node_size: 0, has_z, has_m, geometry_type: FgbType::Point, ..Default::default() },
        );
        b.finish_size_prefixed(h, None);
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(b.finished_data());
        let mut b = FlatBufferBuilder::new();
        let xy = b.create_vector(&[1.0, 2.0]);
        let z = z.map(|z| b.create_vector(z));
        let m = m.map(|m| b.create_vector(m));
        let g = FgbGeometry::create(&mut b, &GeometryArgs { xy: Some(xy), z, m, ..Default::default() });
        let f = FgbFeature::create(&mut b, &FeatureArgs { geometry: Some(g), ..Default::default() });
        b.finish_size_prefixed(f, None);
        bytes.extend_from_slice(b.finished_data());
        bytes
    }

    /// GDAL 3.13.3 ignores undeclared/extra ordinates and refuses missing/short declared ones.
    #[test]
    fn undeclared_z_is_ignored_and_declared_z_required() {
        // The reference TypeScript writer stores z without setting `has_z`; GDAL reads it as 2D.
        for z in [Some(&[5.0][..]), Some(&[][..])] {
            let bytes = dimension_fixture(false, false, z, Some(&[]));
            assert!(accepts(&bytes));
            assert_eq!(parse(&bytes).unwrap().features[0].geometry.as_ref().unwrap().all_coords()[0].z, None);
        }
        assert_eq!(
            parse(&dimension_fixture(true, false, Some(&[5.0]), None)).unwrap().features[0].geometry.as_ref().unwrap().all_coords()[0].z,
            Some(5.0)
        );
        assert!(!accepts(&dimension_fixture(true, false, None, None)));
        assert!(!accepts(&dimension_fixture(true, false, Some(&[]), None)));
        assert!(!accepts(&dimension_fixture(false, true, None, None)));
        assert!(!accepts(&dimension_fixture(false, true, None, Some(&[]))));
        let extra = parse(&dimension_fixture(true, false, Some(&[5.0, 6.0]), None)).unwrap();
        assert_eq!(extra.features[0].geometry.as_ref().unwrap().all_coords()[0].z, Some(5.0));
    }

    /// A point file with the declared dimensions and no features, so no geometry shows them.
    fn empty_dimension_fixture(has_z: bool, has_m: bool) -> Vec<u8> {
        empty_typed_dimension_fixture(FgbType::Point, has_z, has_m)
    }

    /// An empty layer of `geometry_type` with no CRS, declaring Z and M as given.
    fn empty_typed_dimension_fixture(geometry_type: FgbType, has_z: bool, has_m: bool) -> Vec<u8> {
        let mut b = FlatBufferBuilder::new();
        let h = Header::create(
            &mut b,
            &HeaderArgs { features_count: 0, index_node_size: 0, has_z, has_m, geometry_type, ..Default::default() },
        );
        b.finish_size_prefixed(h, None);
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(b.finished_data());
        bytes
    }

    /// The geometry type `export_schema_bytes` reports for the one geometry field of a file.
    fn exported_geometry_type(bytes: &[u8]) -> String {
        let schema = crate::ops::vector_schema::export_schema_bytes(bytes.to_vec(), "fixture.fgb", None).unwrap();
        schema.layers[0].geometry_fields[0].geometry_type.clone()
    }

    #[test]
    fn declared_dimensions_show_without_features() {
        assert_eq!(exported_geometry_type(&empty_dimension_fixture(true, false)), "PointZ");
        assert_eq!(exported_geometry_type(&empty_dimension_fixture(false, false)), "Point");
    }

    #[test]
    fn declared_z_gives_a_geometry_field_for_an_unknown_type_without_crs() {
        // No geometry type, no features and no CRS: only the declared Z says there is a geometry.
        let bytes = empty_typed_dimension_fixture(FgbType::Unknown, true, false);
        assert_eq!(exported_geometry_type(&bytes), "GeometryZ");
        let undeclared = empty_typed_dimension_fixture(FgbType::Unknown, false, false);
        let schema = crate::ops::vector_schema::export_schema_bytes(undeclared, "fixture.fgb", None).unwrap();
        assert!(schema.layers[0].geometry_fields.is_empty());
    }

    #[test]
    fn declared_dimensions_show_without_features_from_a_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.fgb");
        std::fs::write(&path, empty_dimension_fixture(true, true)).unwrap();
        let schema = crate::ops::vector_schema::export_schema_path(path.to_str().unwrap(), &[]).unwrap();
        assert_eq!(schema.layers[0].geometry_fields[0].geometry_type, "PointZM");
    }

    #[cfg(remote)]
    #[test]
    fn remote_flatgeobuf_reports_declared_z_without_features() {
        // The file is downloaded once; its header declares Z, and no feature can show it.
        let bytes = empty_dimension_fixture(true, false);
        let (base, opened) = crate::http::test_server(bytes.clone());
        crate::open_vector(&format!("{base}/empty.fgb"), &Default::default()).unwrap();
        let (base, exported) = crate::http::test_server(bytes);
        let schema = crate::ops::vector_schema::export_schema_path(&format!("{base}/empty.fgb"), &[]).unwrap();
        assert_eq!(schema.layers[0].geometry_fields[0].geometry_type, "PointZ");
        // Requests as opening the file makes them: the schema route does not download it again.
        assert!(!opened.lock().unwrap().is_empty());
        assert_eq!(exported.lock().unwrap().len(), opened.lock().unwrap().len());
    }

    /// A zip archive at a temporary path, holding one member `name` with `bytes`.
    fn zip_with_member(name: &str, bytes: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            zip.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(&mut zip, bytes).unwrap();
            zip.finish().unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("archive.zip");
        std::fs::write(&path, buf.into_inner()).unwrap();
        (dir, path)
    }

    #[test]
    fn declared_z_survives_a_zip_archive() {
        // The archive's member is read, not the archive: its declared Z shows with no features.
        let member = empty_typed_dimension_fixture(FgbType::Unknown, true, false);
        let (_dir, path) = zip_with_member("empty.fgb", &member);
        let schema = crate::ops::vector_schema::export_schema_path(path.to_str().unwrap(), &[]).unwrap();
        assert_eq!(schema.layers[0].geometry_fields[0].geometry_type, "GeometryZ");
        // The layer is named as opening the archive names it.
        let opened = crate::open_vector(path.to_str().unwrap(), &Default::default()).unwrap();
        let names: Vec<_> = schema.layers.iter().map(|l| l.name.clone()).collect();
        assert_eq!(names, opened.layers.iter().map(|l| l.name.clone()).collect::<Vec<_>>());
    }

    #[cfg(remote)]
    #[test]
    fn remote_flatgeobuf_without_extension_keeps_the_general_route() {
        // Known limit: a remote FlatGeobuf is recognised by its URL's file name extension. Without
        // one, the download is named without an extension and no driver recognises it, for schema
        // export as for open_vector.
        let (base, _) = crate::http::test_server(empty_dimension_fixture(true, false));
        let url = format!("{base}/layer?format=fgb");
        let opened = crate::open_vector(&url, &Default::default()).unwrap_err().to_string();
        let exported = crate::ops::vector_schema::export_schema_path(&url, &[]).unwrap_err().to_string();
        assert!(exported.contains("format not recognized"), "{exported}");
        assert!(opened.contains("format not recognized"), "{opened}");
    }

    #[cfg(remote)]
    #[test]
    fn export_schema_path_honours_the_asset_read_policy() {
        // Under a remote STAC document, a cross-origin FlatGeobuf read with ambient credentials is
        // refused by open_vector; the schema route must refuse it the same way, before any download.
        let (base, requests) = crate::http::test_server(empty_typed_dimension_fixture(FgbType::Unknown, true, false));
        let url = format!("{base}/empty.fgb");
        crate::config::with_options(
            &[("GDAL_HTTP_HEADERS", "Authorization: Bearer test-token"), ("TERRIO_STAC_ALLOW_CREDENTIALED_ASSETS", "NO")],
            || {
                let _scope = crate::stac::AssetReadPolicy::for_tests("https://example.invalid/item.json", false).enter().unwrap();
                let opened = crate::open_vector(&url, &Default::default()).unwrap_err().to_string();
                let exported = crate::ops::vector_schema::export_schema_path(&url, &[]).unwrap_err().to_string();
                assert_eq!(exported, opened);
            },
        );
        assert!(requests.lock().unwrap().is_empty(), "the refused file was requested");
    }

    #[test]
    fn disallowed_remote_flatgeobuf_is_refused_before_download() {
        // Refused by the allow-list before any request: the file is not downloaded.
        let (base, requests) = crate::http::test_server(empty_dimension_fixture(true, false));
        crate::config::push_scope(std::collections::HashMap::from([("TERRIO_ALLOWED_DRIVERS".to_string(), Some("GTiff".to_string()))]));
        let result = crate::ops::vector_schema::export_schema_path(&format!("{base}/empty.fgb"), &[]);
        crate::config::pop_scope();
        let err = result.unwrap_err().to_string();
        assert!(err.to_lowercase().contains("flatgeobuf driver is not allowed"), "{err}");
        assert!(requests.lock().unwrap().is_empty(), "the disallowed file was requested");
    }

    #[cfg(remote)]
    #[test]
    fn allowed_remote_flatgeobuf_keeps_declared_z() {
        // Guard: with FlatGeobuf allowed, the remote declared Z still shows with no features.
        let (base, requests) = crate::http::test_server(empty_dimension_fixture(true, false));
        crate::config::push_scope(std::collections::HashMap::from([(
            "TERRIO_ALLOWED_DRIVERS".to_string(),
            Some("FlatGeobuf".to_string()),
        )]));
        let result = crate::ops::vector_schema::export_schema_path(&format!("{base}/empty.fgb"), &[]);
        crate::config::pop_scope();
        let schema = result.unwrap();
        assert_eq!(schema.layers[0].geometry_fields[0].geometry_type, "PointZ");
        assert!(!requests.lock().unwrap().is_empty());
    }

    #[cfg(remote)]
    #[test]
    fn remote_flatgeobuf_with_query_reports_declared_z() {
        // A signed URL keeps its extension before the query: the file is still read as FlatGeobuf.
        let bytes = empty_dimension_fixture(true, false);
        let (base, opened) = crate::http::test_server(bytes.clone());
        crate::open_vector(&format!("{base}/empty.fgb?sig=abc"), &Default::default()).unwrap();
        let (base, exported) = crate::http::test_server(bytes);
        let schema = crate::ops::vector_schema::export_schema_path(&format!("{base}/empty.fgb?sig=abc"), &[]).unwrap();
        assert_eq!(schema.layers[0].geometry_fields[0].geometry_type, "PointZ");
        assert!(!opened.lock().unwrap().is_empty());
        assert_eq!(exported.lock().unwrap().len(), opened.lock().unwrap().len());
    }

    #[test]
    fn declared_dimensions_show_for_gdal_magic_bytes() {
        // GDAL writes the magic's version byte as 1 (terrio's MAGIC has 0): the bytes route reads its header too.
        let mut bytes = empty_dimension_fixture(true, false);
        bytes[7] = 1;
        assert!(has_magic(&bytes));
        assert_eq!(exported_geometry_type(&bytes), "PointZ");
    }

    #[test]
    fn header_reports_declared_dimensions() {
        for (has_z, has_m) in [(false, false), (true, false), (false, true), (true, true)] {
            let bytes = empty_dimension_fixture(has_z, has_m);
            let (info, _) = read_header(&mut std::io::Cursor::new(&bytes), bytes.len() as u64).unwrap();
            assert_eq!((info.has_z, info.has_m), (has_z, has_m));
            assert_eq!(declared_dims_bytes(&bytes).unwrap(), (has_z, has_m));
        }
    }

    #[test]
    fn short_z_vectors_are_rejected_before_coordinate_reads() {
        let mut b = FlatBufferBuilder::new();
        let xy = b.create_vector(&[1.0, 2.0, 3.0, 4.0]);
        let z = b.create_vector(&[5.0]);
        let g = FgbGeometry::create(&mut b, &GeometryArgs { xy: Some(xy), z: Some(z), ..Default::default() });
        let f = FgbFeature::create(&mut b, &FeatureArgs { geometry: Some(g), ..Default::default() });
        b.finish_size_prefixed(f, None);
        let mut bytes = dimension_fixture(true, false, None, None);
        let header_end = header_len(&bytes).unwrap();
        bytes.truncate(header_end);
        bytes.extend_from_slice(b.finished_data());
        assert!(!accepts(&bytes));
    }

    #[test]
    fn geometry_validation_rejects_invalid_part_ends() {
        let (_, header) = parse_header(&dimension_fixture(false, false, None, None)).unwrap();
        for (ends, valid) in [(&[4_u32, 2][..], false), (&[2, 5][..], false), (&[2, 4][..], true)] {
            let mut b = FlatBufferBuilder::new();
            let xy = b.create_vector(&[0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 1.0, 0.0]);
            let ends = b.create_vector(ends);
            let g = FgbGeometry::create(&mut b, &GeometryArgs { xy: Some(xy), ends: Some(ends), ..Default::default() });
            let f = FgbFeature::create(&mut b, &FeatureArgs { geometry: Some(g), ..Default::default() });
            b.finish_size_prefixed(f, None);
            let feature = flatgeobuf::size_prefixed_root_as_feature(b.finished_data()).unwrap();
            assert_eq!(validate_geometry(&feature.geometry().unwrap(), &header).is_ok(), valid);
        }
    }

    #[test]
    fn indexed_headers_reject_counts_above_the_arithmetic_bound() {
        // parse_header checks this limit before index_size/level_bounds, including in release.
        for count in [u64::MAX, 1 << 58, (u64::MAX >> 8) + 1] {
            for index_node_size in [2, 16, u16::MAX] {
                let mut b = FlatBufferBuilder::new();
                let h = Header::create(&mut b, &HeaderArgs { features_count: count, index_node_size, ..Default::default() });
                b.finish_size_prefixed(h, None);
                let mut bytes = MAGIC.to_vec();
                bytes.extend_from_slice(b.finished_data());
                let error = parse_header(&bytes).unwrap_err();
                assert!(error.to_string().contains("implausible feature count"), "{error}");
                assert!(!accepts(&bytes));
            }
        }
    }

    #[test]
    fn largest_admitted_index_count_fits_u64() {
        let count = u64::MAX >> 8;
        for node_size in [2, 16, u16::MAX] {
            // Independent wider-arithmetic oracle for the level sum and byte count.
            let mut n = u128::from(count);
            let mut total = n;
            loop {
                n = n.div_ceil(u128::from(node_size));
                total += n;
                if n == 1 {
                    break;
                }
            }
            let expected = u64::try_from(total * 40).unwrap();
            assert_eq!(index_size(count, node_size), expected);
        }
    }

    fn sample() -> Layer {
        let mut l = Layer::new("sample").with_epsg(4326);
        l.add_field(FieldDef::new("name", FieldType::Text));
        l.add_field(FieldDef::new("count", FieldType::Integer));
        l.add_field(FieldDef::new("big", FieldType::Integer));
        l.add_field(FieldDef::new("value", FieldType::Float));
        l.add_field(FieldDef::new("flag", FieldType::Boolean));
        let square = |x: f64| vec![Coord::xy(x, 0.0), Coord::xy(x + 1.0, 0.0), Coord::xy(x + 1.0, 1.0), Coord::xy(x, 1.0)];
        let geoms = [
            Geometry::polygon(square(0.0), vec![]),
            Geometry::multi_polygon(vec![(square(5.0), vec![]), (square(10.0), vec![square(10.2)])]),
        ];
        for (i, g) in geoms.into_iter().enumerate() {
            let attrs = vec![
                if i == 0 { FieldValue::Text("Zürich".into()) } else { FieldValue::Null },
                FieldValue::Integer(i as i64),
                FieldValue::Integer(1 << 40),
                FieldValue::Float(0.5 + i as f64),
                FieldValue::Boolean(i == 1),
            ];
            l.push(Feature { fid: i as u64, geometry: Some(g), attributes: attrs });
        }
        l
    }

    #[test]
    fn round_trips() {
        let l = sample();
        let back = parse(&to_bytes(&l).unwrap()).unwrap();
        assert_eq!(back.name, "sample");
        assert_eq!(back.crs_epsg(), Some(4326));
        // A polygon and a multipolygon: declared Unknown, each keeping its type, as GDAL writes them.
        assert_eq!(back.geom_type, None);
        assert_eq!(back.len(), 2);
        let mut feats: Vec<&Feature> = back.features.iter().collect();
        feats.sort_by_key(|f| f.attributes[1].as_i64());
        assert_eq!(feats[0].attributes[0], FieldValue::Text("Zürich".into()));
        assert_eq!(feats[1].attributes[0], FieldValue::Null);
        assert_eq!(feats[1].attributes[2], FieldValue::Integer(1 << 40));
        assert_eq!(feats[1].attributes[4], FieldValue::Boolean(true));
        let Some(Geometry::MultiPolygon(ps)) = &feats[1].geometry else { panic!("not a multipolygon") };
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[1].1.len(), 1);
        assert_eq!(ps[1].0.len(), 4);
        assert!(matches!(&feats[0].geometry, Some(Geometry::Polygon { .. })));
    }

    #[test]
    fn round_trips_points_with_z() {
        let mut l = Layer::new("pts");
        for i in 0..40 {
            l.push(Feature { fid: i, geometry: Some(Geometry::point_z(i as f64, -(i as f64), 100.0 + i as f64)), attributes: vec![] });
        }
        let back = parse(&to_bytes(&l).unwrap()).unwrap();
        assert_eq!(back.len(), 40);
        let z: f64 = back.features.iter().map(|f| f.geometry.as_ref().unwrap().all_coords()[0].z.unwrap()).sum();
        assert_eq!(z, (0..40).map(|i| 100.0 + i as f64).sum::<f64>());
    }
}
