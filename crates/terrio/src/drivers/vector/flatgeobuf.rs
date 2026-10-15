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
    /// The size of the spatial index following the header, in bytes.
    pub index_size: usize,
}

/// The length of the header (magic bytes and size-prefixed header) from the first 12 bytes of
/// a file.
pub(crate) fn header_len(start: &[u8]) -> Result<usize> {
    if start.len() < 12 || start[..3] != MAGIC[..3] || start[4..7] != MAGIC[4..7] {
        return Err(bad("not a FlatGeobuf file"));
    }
    Ok(12 + u32::from_le_bytes([start[8], start[9], start[10], start[11]]) as usize)
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
        }
        if c.precision() > 0 {
            def = def.precision(c.precision() as usize);
        }
        layer.add_field(def);
    }
    let count = header.features_count();
    let index_size =
        if header.index_node_size() > 0 && count > 0 { PackedRTree::index_size(count as usize, header.index_node_size()) } else { 0 };
    let meta = FgbHeader {
        geometry_type: header.geometry_type(),
        has_z: header.has_z(),
        has_m: header.has_m(),
        columns: columns.iter().map(|c| c.type_()).collect(),
        index_size,
    };
    Ok((layer, meta))
}

/// Decodes a feature: `fbuf` is its size-prefixed flatbuffer.
pub(crate) fn decode_feature(fbuf: &[u8], h: &FgbHeader, fid: u64) -> Result<Feature> {
    let f = flatgeobuf::size_prefixed_root_as_feature_with_opts(&verifier(), fbuf).map_err(|e| bad(&format!("feature {fid}: {e}")))?;
    let geometry = f.geometry().and_then(|g| to_geometry(&g, h.geometry_type, h.has_z, h.has_m));
    let mut attributes = vec![FieldValue::Null; h.columns.len()];
    if let Some(props) = f.properties() {
        read_properties(props.bytes(), &h.columns, &mut attributes).map_err(|e| bad(&format!("feature {fid}: {e}")))?;
    }
    Ok(Feature { fid, geometry, attributes })
}

/// Parses a FlatGeobuf file held in memory.
pub fn parse(buf: &[u8]) -> Result<Layer> {
    let (mut layer, h) = parse_header(buf)?;
    let mut pos = header_len(buf)? + h.index_size;
    let mut fid = 0u64;
    while pos + 4 <= buf.len() {
        let len = u32::from_le_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as usize;
        let fbuf = buf.get(pos..pos + 4 + len).ok_or_else(|| bad("truncated feature"))?;
        pos += 4 + len;
        layer.push(decode_feature(fbuf, &h, fid)?);
        fid += 1;
    }
    Ok(layer)
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

fn ring(mut cs: Vec<Coord>) -> Ring {
    if cs.len() > 1 && cs.first().map(|c| (c.x, c.y)) == cs.last().map(|c| (c.x, c.y)) {
        cs.pop();
    }
    Ring::new(cs)
}

fn polygon_rings(g: &FgbGeometry, has_z: bool, has_m: bool) -> Option<(Ring, Vec<Ring>)> {
    let mut rings = split(coords(g, has_z, has_m), g.ends()).into_iter().map(ring);
    let exterior = rings.next()?;
    Some((exterior, rings.collect()))
}

fn to_geometry(g: &FgbGeometry, header_type: FgbType, has_z: bool, has_m: bool) -> Option<Geometry> {
    let t = if header_type == FgbType::Unknown { g.type_() } else { header_type };
    Some(match t {
        FgbType::Point => Geometry::Point(coords(g, has_z, has_m).into_iter().next()?),
        FgbType::MultiPoint => Geometry::MultiPoint(coords(g, has_z, has_m)),
        FgbType::LineString => Geometry::LineString(coords(g, has_z, has_m)),
        FgbType::MultiLineString => Geometry::MultiLineString(split(coords(g, has_z, has_m), g.ends())),
        FgbType::Polygon => {
            let (exterior, interiors) = polygon_rings(g, has_z, has_m)?;
            Geometry::Polygon { exterior, interiors }
        }
        FgbType::MultiPolygon => match g.parts() {
            Some(parts) => Geometry::MultiPolygon(parts.iter().filter_map(|p| polygon_rings(&p, has_z, has_m)).collect()),
            // A multipolygon of one polygon may be stored without parts.
            None => Geometry::MultiPolygon(polygon_rings(g, has_z, has_m).into_iter().collect()),
        },
        FgbType::GeometryCollection => Geometry::GeometryCollection(
            g.parts().map(|ps| ps.iter().filter_map(|p| to_geometry(&p, p.type_(), has_z, has_m)).collect()).unwrap_or_default(),
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
    let profile = LayerProfile::of(layer);
    let enc = FgbEncoder::new(layer, &profile);
    let encoded = enc.encode(&layer.features, 0);
    let (features, mut nodes): (Vec<Vec<u8>>, Vec<NodeItem>) = encoded.into_iter().unzip();
    let index = !profile.geometry.missing_bbox && !features.is_empty();
    let extent = calc_extent(&nodes);
    let mut out = Vec::with_capacity(features.iter().map(Vec::len).sum::<usize>() + 4096);
    out.extend_from_slice(&enc.header(layer, features.len() as u64, &extent, index));
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

    /// Each feature as a size-prefixed flatbuffer, with its index node (bounds, and the
    /// feature's number from `first` in `offset`), encoded in parallel.
    pub(crate) fn encode(&self, features: &[Feature], first: u64) -> Vec<(Vec<u8>, NodeItem)> {
        let (geom_type, has_z, col_types) = (self.geom_type, self.has_z, &self.col_types);
        crate::par::map_range(features.len(), |i| {
            let f = &features[i];
            // Sized for the coordinates, so that the buffer is not regrown as it fills.
            let coords = f.geometry.as_ref().map_or(0, coord_count);
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
            if profile.fields.get(i).is_none_or(|p| p.no_int_beyond_int32) {
                ColumnType::Int
            } else {
                ColumnType::Long
            }
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

/// The number of coordinates of a geometry.
fn coord_count(g: &Geometry) -> usize {
    match g {
        Geometry::Point(_) => 1,
        Geometry::LineString(cs) | Geometry::MultiPoint(cs) => cs.len(),
        Geometry::Polygon { exterior, interiors } => exterior.0.len() + interiors.iter().map(|r| r.0.len()).sum::<usize>(),
        Geometry::MultiLineString(ls) => ls.iter().map(Vec::len).sum(),
        Geometry::MultiPolygon(ps) => ps.iter().map(|(e, h)| e.0.len() + h.iter().map(|r| r.0.len()).sum::<usize>()).sum(),
        Geometry::GeometryCollection(gs) => gs.iter().map(coord_count).sum(),
    }
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
