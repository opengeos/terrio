// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! OSM-binary storage and primitive blocks. Uses the parent driver's protobuf reader.
//! Schema: https://github.com/openstreetmap/OSM-binary/tree/master/osmpbf
//! Metadata, header bbox and dates are not exposed by terrio's OSM layers.

use super::{Member, Pb, Relation, pb_fields, pb_varint, unzigzag};
use crate::{Error, Result};
use std::collections::BTreeMap;
use std::sync::Arc;

pub(super) const MAX_BLOB: usize = 32 * 1024 * 1024;
pub(super) type Tags = BTreeMap<Arc<str>, Arc<str>>;
pub(super) struct Node {
    pub id: i64,
    pub decimicro_lon: i32,
    pub decimicro_lat: i32,
    pub tags: Tags,
}
pub(super) struct Way {
    pub id: i64,
    pub nodes: Vec<i64>,
    pub tags: Tags,
}
pub(super) enum Object {
    Node(Node),
    Way(Way),
    Relation(Relation),
}

fn bad() -> Error {
    Error::InvalidData("OSM PBF: malformed or truncated block".into())
}

/// Raw and zlib were the only encodings supported by the previous reader too.
/// Never trust raw_size to allocate; stop inflation at the specification's limit.
pub(super) fn payload(blob: &[u8]) -> Result<Option<std::borrow::Cow<'_, [u8]>>> {
    use std::io::Read;
    let mut data = None;
    let mut size = None;
    for (field, value) in pb_fields(blob).ok_or_else(bad)? {
        match (field, value) {
            (2, Pb::Int(n)) => size = Some(usize::try_from(n).map_err(|_| bad())?),
            (1 | 3, Pb::Bytes(bytes)) => {
                if data.replace((field, bytes)).is_some() {
                    return Err(bad());
                }
            }
            (4..=7, Pb::Bytes(_)) => {
                return Err(Error::InvalidData("OSM PBF: unsupported blob compression (expected raw or zlib)".into()));
            }
            _ => {}
        }
    }
    if blob.len() > MAX_BLOB || size.is_some_and(|n| n > MAX_BLOB) {
        return Err(bad());
    }
    let Some((encoding, bytes)) = data else { return Ok(None) };
    let decoded = if encoding == 1 {
        std::borrow::Cow::Borrowed(bytes)
    } else {
        let mut out = Vec::new();
        flate2::read::ZlibDecoder::new(bytes).take((MAX_BLOB + 1) as u64).read_to_end(&mut out)?;
        std::borrow::Cow::Owned(out)
    };
    if decoded.len() > MAX_BLOB || size.is_some_and(|n| n != decoded.len()) {
        return Err(bad());
    }
    Ok(Some(decoded))
}

pub(super) fn header_sorted(blob: &[u8]) -> Result<bool> {
    let Some(bytes) = payload(blob)? else { return Ok(false) };
    let mut sorted = false;
    for (field, value) in pb_fields(&bytes).ok_or_else(bad)? {
        match (field, value) {
            (4 | 5, Pb::Bytes(feature)) => sorted |= feature == b"Sort.Type_then_ID",
            // The bbox has no effect on the layers, but validate its message framing.
            (1, Pb::Bytes(bbox)) => {
                pb_fields(bbox).ok_or_else(bad)?;
            }
            _ => {}
        }
    }
    // Required/optional features were not used to reject files before. In particular,
    // HistoricalInformation was accepted, with visibility and other metadata ignored.
    Ok(sorted)
}

/// Borrow the blob from one complete fileblock, checking framing before slicing.
pub(super) fn decode(span: &[u8], emit: &mut dyn FnMut(Object) -> Result<()>) -> Result<()> {
    let prefix: [u8; 4] = span.get(..4).ok_or_else(bad)?.try_into().map_err(|_| bad())?;
    let n = u32::from_be_bytes(prefix) as usize;
    if n > 64 * 1024 {
        return Err(bad());
    }
    let end = 4usize.checked_add(n).ok_or_else(bad)?;
    let mut size = None;
    let mut kind = None;
    for (field, value) in pb_fields(span.get(4..end).ok_or_else(bad)?).ok_or_else(bad)? {
        match (field, value) {
            (1, Pb::Bytes(s)) => kind = Some(s),
            (3, Pb::Int(s)) => size = Some(usize::try_from(s).map_err(|_| bad())?),
            _ => {}
        }
    }
    let size = size.filter(|&n| n <= MAX_BLOB).ok_or_else(bad)?;
    if kind != Some(b"OSMData".as_slice()) || end.checked_add(size) != Some(span.len()) {
        return Err(bad());
    }
    let bytes = payload(span.get(end..).ok_or_else(bad)?)?.ok_or_else(bad)?;
    primitives(&bytes, emit)
}

// Repeated integer fields accept both packed and unpacked encodings, including
// multiple packed segments. Lengths come from validated slices, never a claimed count.
fn numbers(bytes: &[u8], field: u64) -> Result<Vec<u64>> {
    let mut out = Vec::new();
    for (n, value) in pb_fields(bytes).ok_or_else(bad)? {
        if n != field {
            continue;
        }
        match value {
            Pb::Int(v) => out.push(v),
            Pb::Bytes(packed) => {
                let mut i = 0;
                while i < packed.len() {
                    out.push(pb_varint(packed, &mut i).ok_or_else(bad)?);
                }
            }
        }
    }
    Ok(out)
}

fn integer(bytes: &[u8], field: u64) -> Result<Option<u64>> {
    Ok(pb_fields(bytes)
        .ok_or_else(bad)?
        .filter_map(|(n, v)| match (n == field, v) {
            (true, Pb::Int(v)) => Some(v),
            _ => None,
        })
        .last())
}

fn deltas(v: Vec<u64>) -> Result<Vec<i64>> {
    let mut sum = 0i64;
    v.into_iter()
        .map(|v| {
            sum = sum.checked_add(unzigzag(v)).ok_or_else(bad)?;
            Ok(sum)
        })
        .collect()
}

struct Context {
    strings: Vec<Arc<str>>,
    granularity: i64,
    lat_offset: i64,
    lon_offset: i64,
}
impl Context {
    fn string(&self, index: u64) -> Result<Arc<str>> {
        let index = usize::try_from(index).map_err(|_| bad())?;
        self.strings.get(index).cloned().ok_or_else(bad)
    }
    fn tags(&self, bytes: &[u8]) -> Result<Tags> {
        let keys = numbers(bytes, 2)?;
        let values = numbers(bytes, 3)?;
        if keys.len() != values.len() {
            return Err(bad());
        }
        let mut tags = Tags::new();
        // The previous simple-object reader kept the first duplicate key; dense
        // nodes inserted sequentially and kept the last.
        for (k, v) in keys.into_iter().zip(values) {
            tags.entry(self.string(k)?).or_insert(self.string(v)?);
        }
        Ok(tags)
    }
    fn coordinate(&self, v: i64, offset: i64) -> Result<i32> {
        let nano = v.checked_mul(self.granularity).and_then(|v| v.checked_add(offset)).ok_or_else(bad)?;
        // Preserve the old reader's integer division to decimicrodegrees.
        i32::try_from(nano / 100).map_err(|_| bad())
    }
    fn node(&self, id: i64, lat: i64, lon: i64, tags: Tags) -> Result<Object> {
        Ok(Object::Node(Node {
            id,
            decimicro_lat: self.coordinate(lat, self.lat_offset)?,
            decimicro_lon: self.coordinate(lon, self.lon_offset)?,
            tags,
        }))
    }
    fn dense(&self, bytes: &[u8], emit: &mut dyn FnMut(Object) -> Result<()>) -> Result<()> {
        let ids = deltas(numbers(bytes, 1)?)?;
        let lat = deltas(numbers(bytes, 8)?)?;
        let lon = deltas(numbers(bytes, 9)?)?;
        if ids.len() != lat.len() || ids.len() != lon.len() {
            return Err(bad());
        }
        let kv = numbers(bytes, 10)?;
        let mut tags = kv.iter().copied();
        for ((id, lat), lon) in ids.into_iter().zip(lat).zip(lon) {
            let mut node_tags = Tags::new();
            if !kv.is_empty() {
                loop {
                    let key = tags.next().ok_or_else(bad)?;
                    if key == 0 {
                        break;
                    }
                    let value = tags.next().ok_or_else(bad)?;
                    node_tags.insert(self.string(key)?, self.string(value)?);
                }
            }
            emit(self.node(id, lat, lon, node_tags)?)?;
        }
        if tags.next().is_some() {
            return Err(bad());
        }
        Ok(())
    }
    fn object(&self, kind: u64, bytes: &[u8]) -> Result<Object> {
        let id = integer(bytes, 1)?.ok_or_else(bad)?;
        let tags = self.tags(bytes)?;
        match kind {
            1 => {
                self.node(unzigzag(id), unzigzag(integer(bytes, 8)?.ok_or_else(bad)?), unzigzag(integer(bytes, 9)?.ok_or_else(bad)?), tags)
            }
            3 => Ok(Object::Way(Way { id: id as i64, nodes: deltas(numbers(bytes, 8)?)?, tags })),
            4 => {
                let roles = numbers(bytes, 8)?;
                let ids = deltas(numbers(bytes, 9)?)?;
                let types = numbers(bytes, 10)?;
                if roles.len() != ids.len() || ids.len() != types.len() {
                    return Err(bad());
                }
                let mut members = Vec::new();
                for ((id, kind), role) in ids.into_iter().zip(types).zip(roles) {
                    let member = match kind {
                        0 => Member::Node(id),
                        1 => Member::Way(id),
                        2 => Member::Relation(id),
                        _ => continue,
                    };
                    members.push((member, self.string(role)?));
                }
                Ok(Object::Relation(Relation { id: id as i64, members, tags: tags.into_iter().collect() }))
            }
            _ => Err(bad()),
        }
    }
}

fn primitives(bytes: &[u8], emit: &mut dyn FnMut(Object) -> Result<()>) -> Result<()> {
    let mut c = Context { strings: Vec::new(), granularity: 100, lat_offset: 0, lon_offset: 0 };
    let mut has_table = false;
    for (field, value) in pb_fields(bytes).ok_or_else(bad)? {
        match (field, value) {
            (1, Pb::Bytes(table)) => {
                has_table = true;
                for (n, value) in pb_fields(table).ok_or_else(bad)? {
                    if let (1, Pb::Bytes(s)) = (n, value) {
                        c.strings.push(Arc::from(String::from_utf8_lossy(s).as_ref()));
                    }
                }
            }
            (17, Pb::Int(g)) => c.granularity = i64::from(g as i32),
            (19, Pb::Int(o)) => c.lat_offset = o as i64,
            (20, Pb::Int(o)) => c.lon_offset = o as i64,
            _ => {}
        }
    }
    if !has_table {
        return Err(bad());
    }
    for (field, value) in pb_fields(bytes).ok_or_else(bad)? {
        if let (2, Pb::Bytes(group)) = (field, value) {
            // The previous reader visited simple nodes, dense nodes, ways, then
            // relations within each group, irrespective of protobuf field order.
            for kind in [1, 2, 3, 4] {
                if kind == 2 {
                    let mut dense: Option<std::borrow::Cow<'_, [u8]>> = None;
                    for (n, value) in pb_fields(group).ok_or_else(bad)? {
                        if let (2, Pb::Bytes(bytes)) = (n, value) {
                            if let Some(merged) = &mut dense {
                                merged.to_mut().extend_from_slice(bytes);
                            } else {
                                dense = Some(std::borrow::Cow::Borrowed(bytes));
                            }
                        }
                    }
                    if let Some(dense) = dense {
                        c.dense(&dense, emit)?;
                    }
                    continue;
                }
                for (n, value) in pb_fields(group).ok_or_else(bad)? {
                    if let Pb::Bytes(object) = value {
                        if n != kind {
                            continue;
                        }
                        emit(c.object(kind, object)?)?;
                    }
                }
            }
        }
    }
    Ok(())
}
