// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

// Written from the published description of the format, `FeatureCollection.proto` in
// https://github.com/Esri/arcgis-pbf. GDAL has no such driver: its fields, values and geometries follow what
// `super::esrijson` reads from the same service as JSON.

//! The FeatureCollection protocol buffer of ArcGIS feature-service queries (`f=pbf`): one layer,
//! with what `f=json` gives. See [`super::featureservice`] for how fields, values and geometries
//! are read.
//!
//! Coordinates are integers to scale and translate as the result's `transform` says. X and Y are
//! differences from the previous vertex, starting over at each part; Z and M are each vertex's own,
//! and an M of 0 is none, as the same answer in JSON shows. An answer that holds a count, ids or
//! an extent, or a curve or shape-buffer geometry, is refused rather than read without its
//! features or arcs.

use super::featureservice::{self, Field};
use crate::{Error, Result};
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, Layer};

fn invalid(message: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("ESRIPBF: {message}"))
}

/// The head the tests give [`is_esripbf`]: room for a version of at most 32 bytes and the start of
/// the feature result. Format identification passes more.
#[cfg(test)]
const TEST_HEAD_BYTES: usize = 64;

/// A reader of protocol buffer fields, which refuses what runs past its bytes.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn done(&self) -> bool {
        self.at >= self.bytes.len()
    }

    fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    fn varint(&mut self) -> Result<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = *self.bytes.get(self.at).ok_or_else(|| invalid("the data ends inside a number"))?;
            self.at += 1;
            // The tenth byte holds the 64th bit alone.
            if shift == 63 && byte & 0x7e != 0 {
                return Err(invalid("a number is too long"));
            }
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(value);
            }
        }
        Err(invalid("a number is too long"))
    }

    /// A field's number and wire type.
    fn key(&mut self) -> Result<(u64, u8)> {
        let key = self.varint()?;
        Ok((key >> 3, (key & 7) as u8))
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(n).ok_or_else(|| invalid("a field's length is too large"))?;
        let taken = self.bytes.get(self.at..end).ok_or_else(|| invalid("the data ends inside a field"))?;
        self.at = end;
        Ok(taken)
    }

    /// A length-delimited field's bytes.
    fn message(&mut self) -> Result<&'a [u8]> {
        let n = usize::try_from(self.varint()?).map_err(|_| invalid("a field's length is too large"))?;
        self.take(n)
    }

    fn fixed<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?.try_into().map_err(|_| invalid("the data ends inside a field"))
    }

    fn double(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.fixed()?))
    }

    fn skip(&mut self, wire: u8) -> Result<()> {
        match wire {
            0 => self.varint().map(|_| ()),
            1 => self.take(8).map(|_| ()),
            2 => self.message().map(|_| ()),
            5 => self.take(4).map(|_| ()),
            w => Err(invalid(format!("unknown wire type {w}"))),
        }
    }
}

fn zigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

/// Whether the start of some bytes is such an answer with a feature result: an optional version,
/// then a query result holding a feature result, whose first field is one the format defines.
pub(crate) fn is_esripbf(bytes: &[u8]) -> bool {
    signs(bytes).unwrap_or(false)
}

/// Whether the start of some bytes is such an answer of any kind: a feature result, or a count, ids
/// or an extent, which [`page`] then refuses by name.
#[cfg_attr(not(remote), allow(dead_code))]
pub(crate) fn is_query_answer(bytes: &[u8]) -> bool {
    is_esripbf(bytes) || answer_kind(bytes).is_ok_and(|kind| matches!(kind, Some(2..=4)))
}

/// A reader at an answer's query result, past its optional version: `None` when the bytes start
/// otherwise. The query result's length is left to read.
fn at_query_result(bytes: &[u8]) -> Result<Option<Reader<'_>>> {
    let mut r = Reader::new(bytes);
    let mut key = r.key()?;
    if key == (1, 2) {
        let version = r.message()?;
        if version.len() > 32 || !version.iter().all(u8::is_ascii_graphic) {
            return Ok(None);
        }
        key = r.key()?;
    }
    Ok((key == (2, 2)).then_some(r))
}

/// The field number of the result an answer starts with, after its optional version.
#[cfg_attr(not(remote), allow(dead_code))]
fn answer_kind(bytes: &[u8]) -> Result<Option<u64>> {
    let Some(mut r) = at_query_result(bytes)? else { return Ok(None) };
    r.varint()?;
    Ok(match r.key()? {
        (kind, 2) => Some(kind),
        _ => None,
    })
}

fn signs(bytes: &[u8]) -> Result<bool> {
    let Some(mut r) = at_query_result(bytes)? else { return Ok(false) };
    // These lengths may pass the bytes given.
    let query = r.varint()?;
    if r.key()? != (1, 2) {
        return Ok(false);
    }
    let result = r.varint()?;
    if result >= query {
        return Ok(false);
    }
    let (field, wire) = r.key()?;
    Ok(match field {
        1..=6 | 8 | 12..=16 => wire == 2,
        7 | 9..=11 => wire == 0,
        _ => false,
    })
}

/// Reads such an answer from a file; the layer is named after it.
pub fn read(path: &str) -> Result<Layer> {
    let stem = std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("ESRIPBF");
    page(&std::fs::read(path)?, stem, path).map(|page| page.layer)
}

/// Parses a test answer into a layer named `name`.
#[cfg(test)]
fn parse(bytes: &[u8], name: &str) -> Result<Layer> {
    page(bytes, name, name).map(|page| page.layer)
}

/// How a result's integers become coordinates.
struct Transform {
    /// The answer gave one: the format sends one with every result.
    given: bool,
    upper_left: bool,
    /// X, Y, M and Z, as the format orders them.
    scale: [f64; 4],
    translate: [f64; 4],
}

impl Default for Transform {
    fn default() -> Self {
        Self { given: false, upper_left: true, scale: [1.0; 4], translate: [0.0; 4] }
    }
}

/// The four numbers of a `Scale` or `Translate`; those absent keep the value given.
fn four(bytes: &[u8], absent: f64) -> Result<[f64; 4]> {
    let mut out = [absent; 4];
    let mut r = Reader::new(bytes);
    while !r.done() {
        match r.key()? {
            (field @ 1..=4, 1) => {
                let value = r.double()?;
                if let Some(slot) = (field as usize).checked_sub(1).and_then(|i| out.get_mut(i)) {
                    *slot = value;
                }
            }
            (_, wire) => r.skip(wire)?,
        }
    }
    Ok(out)
}

fn transform(bytes: &[u8]) -> Result<Transform> {
    let mut t = Transform { given: true, ..Transform::default() };
    let mut r = Reader::new(bytes);
    while !r.done() {
        match r.key()? {
            (1, 0) => t.upper_left = r.varint()? == 0,
            // A scale of zero is one the service left out.
            (2, 2) => t.scale = four(r.message()?, 1.0)?.map(|s| if s == 0.0 { 1.0 } else { s }),
            (3, 2) => t.translate = four(r.message()?, 0.0)?,
            (_, wire) => r.skip(wire)?,
        }
    }
    Ok(t)
}

/// The `esriFieldType…` name of the format's field type number.
fn field_type_name(number: u64) -> &'static str {
    match number {
        0 => "esriFieldTypeSmallInteger",
        1 => "esriFieldTypeInteger",
        2 => "esriFieldTypeSingle",
        3 => "esriFieldTypeDouble",
        4 => "esriFieldTypeString",
        5 => "esriFieldTypeDate",
        6 => "esriFieldTypeOID",
        7 => "esriFieldTypeGeometry",
        8 => "esriFieldTypeBlob",
        9 => "esriFieldTypeRaster",
        10 => "esriFieldTypeGUID",
        11 => "esriFieldTypeGlobalID",
        12 => "esriFieldTypeXML",
        13 => "esriFieldTypeBigInteger",
        14 => "esriFieldTypeDateOnly",
        15 => "esriFieldTypeTimeOnly",
        16 => "esriFieldTypeTimestampOffset",
        _ => "",
    }
}

/// The `esriGeometry…` name of the format's geometry type number.
fn geometry_type_name(number: u64) -> &'static str {
    match number {
        POINT => "esriGeometryPoint",
        MULTIPOINT => "esriGeometryMultipoint",
        POLYLINE => "esriGeometryPolyline",
        POLYGON => "esriGeometryPolygon",
        _ => "",
    }
}

const POINT: u64 = 0;
const MULTIPOINT: u64 = 1;
const POLYLINE: u64 = 2;
const POLYGON: u64 = 3;

/// A field's name and its type's `esriFieldType…` name.
fn field(bytes: &[u8]) -> Result<(String, &'static str)> {
    let (mut name, mut number) = (String::new(), 0);
    let mut r = Reader::new(bytes);
    while !r.done() {
        match r.key()? {
            (1, 2) => name = String::from_utf8_lossy(r.message()?).into_owned(),
            (2, 0) => number = r.varint()?,
            (_, wire) => r.skip(wire)?,
        }
    }
    Ok((name, field_type_name(number)))
}

fn spatial_reference(bytes: &[u8]) -> Result<Option<crate::crs::SpatialRef>> {
    let (mut wkid, mut latest, mut wkt) = (None, None, None);
    let mut r = Reader::new(bytes);
    while !r.done() {
        match r.key()? {
            (1, 0) => wkid = u32::try_from(r.varint()?).ok(),
            (2, 0) => latest = u32::try_from(r.varint()?).ok(),
            (5, 2) => wkt = Some(String::from_utf8_lossy(r.message()?).into_owned()),
            (_, wire) => r.skip(wire)?,
        }
    }
    Ok(featureservice::spatial_reference(latest, wkid, wkt.as_deref()))
}

/// Whether a page is JSON, as a service may answer whatever was asked for.
pub(crate) fn is_json(bytes: &[u8]) -> bool {
    bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes).trim_ascii_start().first() == Some(&b'{')
}

/// Finds a service error without materializing features that the allow-list refuses.
fn json_service_error(bytes: &[u8]) -> Option<Error> {
    use serde::de::{Error as _, MapAccess, Visitor};
    #[derive(Default, serde::Deserialize)]
    struct ServiceError {
        message: Option<String>,
        code: Option<i64>,
    }
    struct RootError<'a>(&'a mut Option<ServiceError>);
    impl<'de> Visitor<'de> for RootError<'_> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a JSON object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
            while let Some(key) = map.next_key::<String>()? {
                if key == "error" {
                    *self.0 = Some(map.next_value::<Option<ServiceError>>()?.unwrap_or_default());
                    return Err(A::Error::custom("service error found"));
                }
                map.next_value::<serde::de::IgnoredAny>()?;
            }
            Ok(())
        }
    }
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let mut found = None;
    let _ = serde::Deserializer::deserialize_map(&mut serde_json::Deserializer::from_slice(bytes), RootError(&mut found));
    found.map(|error| {
        let message = error.message.as_deref().unwrap_or("no message");
        let code = error.code.map(|c| format!(" ({c})")).unwrap_or_default();
        invalid(format!("the service answered with an error{code}: {message}"))
    })
}

/// A page of a query's results: its layer, and whether the service has more.
pub(crate) fn page(bytes: &[u8], name: &str, source: &str) -> Result<featureservice::Page> {
    if bytes.is_empty() {
        return Err(invalid("the answer is empty"));
    }
    // A service may answer in JSON whatever was asked for. Its error is reported as this driver's;
    // its features are ESRI JSON, which an allow-list may leave out.
    if is_json(bytes) {
        if let Err(denied) = crate::formats::check_driver_allowed("ESRIJSON", source, crate::formats::Kind::Vector) {
            return Err(json_service_error(bytes).unwrap_or(denied));
        }
        let relabel = |m: String| m.strip_prefix("ESRIJSON: ").map_or_else(|| m.clone(), |rest| format!("ESRIPBF: {rest}"));
        let page = super::esrijson::page(bytes, name).map_err(|e| match e {
            Error::InvalidData(m) => Error::InvalidData(relabel(m)),
            Error::InvalidArgument(m) => Error::InvalidArgument(relabel(m)),
            e => e,
        })?;
        return Ok(page);
    }
    super::geojson::reserve_parse_memory(bytes.len(), "ESRIPBF")?;
    let result = feature_result(bytes)?;

    let (mut more, mut has_z, mut has_m) = (false, false, false);
    let (mut geometry_type, mut crs) = (None, None);
    let mut quantized = Transform::default();
    let mut declared = Vec::new();
    let mut features = Vec::new();
    let mut r = Reader::new(result);
    while !r.done() {
        match r.key()? {
            (7, 0) => geometry_type = Some(r.varint()?),
            (8, 2) => crs = spatial_reference(r.message()?)?,
            (9, 0) => more = r.varint()? != 0,
            (10, 0) => has_z = r.varint()? != 0,
            (11, 0) => has_m = r.varint()? != 0,
            (12, 2) => quantized = transform(r.message()?)?,
            (13, 2) => declared.push(field(r.message()?)?),
            (15, 2) => features.push(r.message()?),
            (_, wire) => r.skip(wire)?,
        }
    }

    // Values are by position: a repeated name keeps its first column, as the layer does.
    let mut names = std::collections::HashSet::new();
    let columns: Vec<bool> = declared.iter().map(|(name, _)| names.insert(name.clone())).collect();
    let fields: Vec<Field> =
        declared.iter().zip(&columns).filter(|(_, keep)| **keep).map(|((name, esri), _)| featureservice::field(name, esri, None)).collect();
    featureservice::check_values("ESRIPBF", features.len(), fields.len(), bytes.len())?;

    let mut layer = Layer::new(name);
    for f in &fields {
        layer.add_field(f.def.clone());
    }
    if let Some(crs) = crs {
        crs.apply_to_layer(&mut layer);
    }
    let object_id = fields.iter().position(|f| f.object_id);
    // A point layer's type is the format's default, so it may be absent: as it is without geometries.
    let kind = geometry_type.unwrap_or(POINT);
    let shape = Shape { kind, transform: &quantized, has_z, has_m };
    let mut any_geometry = false;
    let mut by_position = Vec::new();
    for (n, bytes) in features.into_iter().enumerate() {
        let (values, geometry) = feature(bytes, &fields, &columns, &shape).map_err(|e| match e {
            Error::InvalidData(m) => invalid(format!("feature {n}: {}", m.strip_prefix("ESRIPBF: ").unwrap_or(&m))),
            e => e,
        })?;
        any_geometry |= geometry.is_some();
        let fid = object_id.and_then(|i| match values.get(i) {
            Some(FieldValue::Integer(id)) => u64::try_from(*id).ok(),
            _ => None,
        });
        if fid.is_none() {
            by_position.push(n);
        }
        layer.push(Feature { fid: fid.unwrap_or(n as u64), geometry, attributes: values });
    }
    if geometry_type.is_some() || any_geometry {
        layer.geom_type = featureservice::layer_geometry_type(geometry_type_name(kind));
    }
    featureservice::number_by_position(&mut layer, &by_position);
    Ok(featureservice::Page { layer, more, by_position })
}

/// The bytes of an answer's feature result.
fn feature_result(bytes: &[u8]) -> Result<&[u8]> {
    let mut result = None;
    let mut top = Reader::new(bytes);
    while !top.done() {
        match top.key()? {
            (2, 2) => {
                let mut query = Reader::new(top.message()?);
                while !query.done() {
                    match query.key()? {
                        (1, 2) => result = Some(query.message()?),
                        (2..=4, 2) => return Err(invalid("the answer holds a count, ids or an extent, not features")),
                        (_, wire) => query.skip(wire)?,
                    }
                }
            }
            (_, wire) => top.skip(wire)?,
        }
    }
    result.ok_or_else(|| invalid("the answer holds no feature result"))
}

/// What a feature's values and geometry are read with.
struct Shape<'a> {
    kind: u64,
    transform: &'a Transform,
    has_z: bool,
    has_m: bool,
}

/// A feature's values, one per field, and its geometry.
fn feature(bytes: &[u8], fields: &[Field], columns: &[bool], shape: &Shape) -> Result<(Vec<FieldValue>, Option<Geometry>)> {
    let mut values = Vec::with_capacity(fields.len());
    let mut column = 0;
    let mut geometry = None;
    let mut r = Reader::new(bytes);
    while !r.done() {
        match r.key()? {
            (1, 2) => {
                let cell = cell(r.message()?)?;
                let keep = *columns.get(column).ok_or_else(|| invalid("it has more values than the result has fields"))?;
                column += 1;
                if keep && let Some(field) = fields.get(values.len()) {
                    values.push(value(&cell, &field.def));
                }
            }
            (2, 2) => geometry = read_geometry(r.message()?, shape)?,
            (3, 2) => return Err(invalid("shape-buffer geometries are not read")),
            (7, 2) => return Err(invalid("curve geometries are not read")),
            (_, wire) => r.skip(wire)?,
        }
    }
    values.resize(fields.len(), FieldValue::Null);
    Ok((values, geometry))
}

/// A `Value`, as the format encodes it.
enum Cell<'a> {
    Null,
    Text(&'a [u8]),
    Int(i64),
    UInt(u64),
    Real(f64),
    Bool(bool),
}

fn cell(bytes: &[u8]) -> Result<Cell<'_>> {
    let mut out = Cell::Null;
    let mut r = Reader::new(bytes);
    while !r.done() {
        match r.key()? {
            (1, 2) => out = Cell::Text(r.message()?),
            // Through its shortest text: 0.1, not 0.10000000149011612.
            (2, 5) => out = Cell::Real(f32::from_le_bytes(r.fixed()?).to_string().parse().unwrap_or(f64::NAN)),
            (3, 1) => out = Cell::Real(r.double()?),
            (4 | 8, 0) => out = Cell::Int(zigzag(r.varint()?)),
            (5 | 7, 0) => out = Cell::UInt(r.varint()?),
            (6, 0) => out = Cell::Int(r.varint()? as i64),
            (9, 0) => out = Cell::Bool(r.varint()? != 0),
            (10, 0) => {
                r.varint()?;
                out = Cell::Null;
            }
            (_, wire) => r.skip(wire)?,
        }
    }
    Ok(out)
}

impl Cell<'_> {
    fn text(&self) -> Option<std::borrow::Cow<'_, str>> {
        match self {
            Cell::Text(bytes) => Some(String::from_utf8_lossy(bytes)),
            _ => None,
        }
    }

    fn as_i64(&self) -> Option<i64> {
        match self {
            Cell::Int(i) => Some(*i),
            Cell::UInt(u) => i64::try_from(*u).ok(),
            // A whole number past 64 bits is none, not the nearest that fits, as in ESRIJSON.
            Cell::Real(f) => {
                (f.is_finite() && (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(f)).then_some(*f as i64)
            }
            _ => None,
        }
    }

    fn as_f64(&self) -> Option<f64> {
        match self {
            Cell::Int(i) => Some(*i as f64),
            Cell::UInt(u) => Some(*u as f64),
            Cell::Real(f) => Some(*f),
            _ => None,
        }
    }
}

/// A value, as the field's type reads it: as [`super::esrijson`] reads the same value in JSON.
fn value(cell: &Cell, field: &FieldDef) -> FieldValue {
    if matches!(cell, Cell::Null) {
        return FieldValue::Null;
    }
    let text = cell.text();
    match field.field_type {
        FieldType::Integer => {
            cell.as_i64().or_else(|| text.and_then(|s| s.trim().parse().ok())).map_or(FieldValue::Null, FieldValue::Integer)
        }
        FieldType::Float => cell
            .as_f64()
            .or_else(|| text.and_then(|s| s.trim().parse().ok()))
            .filter(|f: &f64| f.is_finite())
            .map_or(FieldValue::Null, FieldValue::Float),
        FieldType::DateTime => match cell.as_i64() {
            Some(ms) => featureservice::date_time(ms),
            None => text.map_or(FieldValue::Null, |s| FieldValue::DateTime(s.into_owned())),
        },
        FieldType::Date => match cell.as_i64() {
            Some(ms) => FieldValue::Date(super::datetime::format_date(ms.div_euclid(86_400_000))),
            None => text.map_or(FieldValue::Null, |s| FieldValue::Date(s.into_owned())),
        },
        _ => FieldValue::Text(match cell {
            Cell::Text(bytes) => String::from_utf8_lossy(bytes).into_owned(),
            Cell::Int(i) => i.to_string(),
            Cell::UInt(u) => u.to_string(),
            Cell::Real(f) => f.to_string(),
            Cell::Bool(b) => b.to_string(),
            Cell::Null => String::new(),
        }),
    }
}

/// The vertices of a part: `count` of them, or those left without one.
fn part(c: &mut Reader, count: Option<usize>, shape: &Shape) -> Result<Vec<Coord>> {
    let dimensions = 2 + usize::from(shape.has_z) + usize::from(shape.has_m);
    let t = shape.transform;
    let mut vertices = Vec::with_capacity(count.unwrap_or(0).min(c.remaining() / dimensions));
    let mut sums = [0i64; 2];
    while count.map_or(!c.done(), |n| vertices.len() < n) {
        for sum in &mut sums {
            *sum = sum.checked_add(zigzag(c.varint()?)).ok_or_else(|| invalid("a coordinate is out of range"))?;
        }
        // After X and Y, Z when it has Z, then M: each vertex's own, not differences.
        let mut own = [0i64; 2];
        for value in own.iter_mut().take(dimensions - 2) {
            *value = zigzag(c.varint()?);
        }
        let ([a, b], [third, fourth]) = (sums, own);
        let dy = t.scale[1] * b as f64;
        let x = t.translate[0] + t.scale[0] * a as f64;
        let y = if t.upper_left { t.translate[1] - dy } else { t.translate[1] + dy };
        let z = shape.has_z.then(|| t.translate[3] + t.scale[3] * third as f64);
        // A service writes 0 for a vertex without M, which JSON gives as null.
        let quantized_m = if shape.has_z { fourth } else { third };
        let m = (shape.has_m && quantized_m != 0).then(|| t.translate[2] + t.scale[2] * quantized_m as f64);
        if !(x.is_finite() && y.is_finite() && z.is_none_or(f64::is_finite) && m.is_none_or(f64::is_finite)) {
            return Err(invalid("a coordinate is not finite"));
        }
        vertices.push(Coord { x, y, z, m });
    }
    Ok(vertices)
}

/// A geometry. One without coordinates is no geometry.
fn read_geometry(bytes: &[u8], shape: &Shape) -> Result<Option<Geometry>> {
    let mut lengths = Vec::new();
    // Packed, possibly in several blocks, or one number a field. One block, the usual, is not copied.
    let mut coords: std::borrow::Cow<[u8]> = std::borrow::Cow::Borrowed(&[]);
    let mut r = Reader::new(bytes);
    while !r.done() {
        match r.key()? {
            (2, 2) => {
                let mut packed = Reader::new(r.message()?);
                while !packed.done() {
                    lengths.push(packed.varint()?);
                }
            }
            (2, 0) => lengths.push(r.varint()?),
            (3, 2) => {
                let block = r.message()?;
                if coords.is_empty() {
                    coords = std::borrow::Cow::Borrowed(block);
                } else {
                    coords.to_mut().extend_from_slice(block);
                }
            }
            (3, 0) => put_varint(coords.to_mut(), r.varint()?),
            (_, wire) => r.skip(wire)?,
        }
    }
    if coords.is_empty() {
        if lengths.iter().any(|&n| n > 0) {
            return Err(invalid("a part has vertices, but the geometry has no coordinates"));
        }
        return Ok(None);
    }
    let mut c = Reader::new(&coords);
    let mut parts = Vec::with_capacity(lengths.len().min(coords.len()));
    if lengths.is_empty() {
        parts.push(part(&mut c, None, shape)?);
    }
    for length in lengths {
        let count = usize::try_from(length).map_err(|_| invalid("a part's length is too large"))?;
        parts.push(part(&mut c, Some(count), shape)?);
    }
    if !c.done() {
        return Err(invalid("it has more coordinates than its parts count"));
    }
    let geometry = match shape.kind {
        POINT => {
            let mut vertices = parts.into_iter().flatten();
            let point = vertices.next();
            if vertices.next().is_some() {
                return Err(invalid("a point has more than one vertex"));
            }
            point.map(Geometry::Point)
        }
        MULTIPOINT => Some(Geometry::MultiPoint(parts.into_iter().flatten().collect())),
        POLYLINE => featureservice::polyline(parts),
        POLYGON => super::polygons_from_rings(parts),
        kind => return Err(invalid(format!("geometries of type {kind} are not read"))),
    };
    // Without a transform, its integers are no coordinates of the layer's.
    if geometry.is_some() && !shape.transform.given {
        return Err(invalid("it has coordinates, but the answer has no transform for them"));
    }
    Ok(geometry)
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push((v as u8 & 0x7f) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    fn number(field: u64, v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        varint(&mut out, field << 3);
        varint(&mut out, v);
        out
    }

    fn message(field: u64, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        varint(&mut out, (field << 3) | 2);
        varint(&mut out, bytes.len() as u64);
        out.extend_from_slice(bytes);
        out
    }

    fn double(field: u64, v: f64) -> Vec<u8> {
        let mut out = Vec::new();
        varint(&mut out, (field << 3) | 1);
        out.extend_from_slice(&v.to_le_bytes());
        out
    }

    fn sint(v: i64) -> u64 {
        ((v << 1) ^ (v >> 63)) as u64
    }

    fn packed(field: u64, values: &[u64]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for v in values {
            varint(&mut bytes, *v);
        }
        message(field, &bytes)
    }

    fn field_of(name: &str, number_of_type: u64) -> Vec<u8> {
        message(13, &[message(1, name.as_bytes()), number(2, number_of_type)].concat())
    }

    fn geometry(lengths: &[u64], coords: &[i64]) -> Vec<u8> {
        let coords: Vec<u64> = coords.iter().map(|c| sint(*c)).collect();
        message(2, &[packed(2, lengths), packed(3, &coords)].concat())
    }

    /// A transform of scale 1 with Y going down from 100, so that a vertex is easy to read.
    fn quantized(origin: u64) -> Vec<u8> {
        let scale = message(2, &[double(1, 1.0), double(2, 1.0)].concat());
        let translate = message(3, &[double(1, 10.0), double(2, 100.0)].concat());
        message(12, &[number(1, origin), scale, translate].concat())
    }

    fn answer(result: &[Vec<u8>]) -> Vec<u8> {
        [message(1, b"2.0"), message(2, &message(1, &result.concat()))].concat()
    }

    fn value_field(bytes: Vec<u8>) -> Vec<u8> {
        message(1, &bytes)
    }

    fn cells(l: &Layer, row: usize) -> &[FieldValue] {
        &l.features[row].attributes
    }

    #[test]
    fn reads_fields_values_ids_and_the_crs() {
        let bytes = answer(&[
            number(7, POLYLINE),
            message(8, &[number(1, 102100), number(2, 3857)].concat()),
            number(9, 1),
            quantized(0),
            field_of("OBJECTID", 6),
            field_of("NAME", 4),
            field_of("AREA", 3),
            field_of("BUILT", 5),
            field_of("DAY", 14),
            message(
                15,
                &[
                    value_field(number(5, 7)),
                    value_field(message(1, b"Lake")),
                    value_field(double(3, 1.5)),
                    value_field(number(8, sint(0))),
                    value_field(message(1, b"2020-01-31")),
                    geometry(&[2], &[1, 2, 3, 4]),
                ]
                .concat(),
            ),
            message(15, &[value_field(number(5, 8)), value_field(number(10, 1))].concat()),
        ]);
        let page = page(&bytes, "lakes", "lakes").unwrap();
        assert!(page.more);
        assert!(page.by_position.is_empty());
        let l = page.layer;
        assert_eq!(l.name, "lakes");
        assert_eq!((l.features[0].fid, l.features[1].fid), (7, 8));
        assert_eq!(l.crs_epsg(), Some(3857));
        assert_eq!(l.geom_type, Some(wbvector::GeometryType::LineString));
        assert_eq!(
            cells(&l, 0),
            [
                FieldValue::Integer(7),
                FieldValue::Text("Lake".into()),
                FieldValue::Float(1.5),
                FieldValue::DateTime("1970-01-01T00:00:00Z".into()),
                FieldValue::Date("2020-01-31".into()),
            ]
        );
        assert_eq!(cells(&l, 1).len(), 5, "values left out are null");
        assert_eq!(cells(&l, 1)[1], FieldValue::Null);
        let Some(Geometry::LineString(line)) = &l.features[0].geometry else { panic!("not a line") };
        assert_eq!((line[0].x, line[0].y), (11.0, 98.0));
        assert_eq!((line[1].x, line[1].y), (14.0, 94.0));
        assert!(l.features[1].geometry.is_none());
    }

    #[test]
    fn deltas_start_over_at_each_part_and_the_origin_sets_the_direction_of_y() {
        for (origin, y0, y1) in [(0, 99.0, 90.0), (1, 101.0, 110.0)] {
            let bytes = answer(&[number(7, POLYLINE), quantized(origin), message(15, &geometry(&[1, 1], &[1, 1, 5, 10]))]);
            let l = parse(&bytes, "x").unwrap();
            let Some(Geometry::MultiLineString(lines)) = &l.features[0].geometry else { panic!("not a multi line") };
            assert_eq!((lines[0][0].x, lines[0][0].y), (11.0, y0));
            assert_eq!((lines[1][0].x, lines[1][0].y), (15.0, y1));
        }
    }

    #[test]
    fn z_and_m_follow_x_and_y_with_their_own_scales() {
        let scale = message(2, &[double(1, 1.0), double(2, 1.0), double(3, 0.5), double(4, 2.0)].concat());
        let translate = message(3, &[double(3, 1.0), double(4, 100.0)].concat());
        let t = message(12, &[scale, translate].concat());
        let both = answer(&[number(7, MULTIPOINT), number(10, 1), number(11, 1), t.clone(), message(15, &geometry(&[1], &[1, 1, 3, 4]))]);
        let l = parse(&both, "x").unwrap();
        let Some(Geometry::MultiPoint(p)) = &l.features[0].geometry else { panic!("not a multi point") };
        assert_eq!((p[0].z, p[0].m), (Some(106.0), Some(3.0)));

        let only_m = answer(&[number(7, MULTIPOINT), number(11, 1), t, message(15, &geometry(&[1], &[1, 1, 4]))]);
        let l = parse(&only_m, "x").unwrap();
        let Some(Geometry::MultiPoint(p)) = &l.features[0].geometry else { panic!("not a multi point") };
        assert_eq!((p[0].z, p[0].m), (None, Some(3.0)));
    }

    #[test]
    fn z_and_m_are_each_vertex_s_own_and_an_m_of_0_is_none() {
        // As a service's answer has them: Z from 1,527 down to -2,600 along a line, and M 8 then
        // none. X and Y are differences; Z and M are not.
        let scale = message(2, &[double(1, 1.0), double(2, 1.0), double(3, 0.0001), double(4, 0.0001)].concat());
        let translate = message(3, &[double(3, -100_000.0), double(4, -100_000.0)].concat());
        let t = message(12, &[scale, translate].concat());
        let bytes = answer(&[
            number(7, POLYLINE),
            number(10, 1),
            number(11, 1),
            t,
            message(15, &geometry(&[3], &[1, 1, 1_015_270_000, 1_000_080_000, 1, 1, 974_000_000, 0, 1, 1, 1_000_000_000, 1_000_100_000])),
        ]);
        let l = parse(&bytes, "x").unwrap();
        let Some(Geometry::LineString(line)) = &l.features[0].geometry else { panic!("not a line") };
        let zm: Vec<_> = line.iter().map(|c| (c.z.map(|z| z.round()), c.m.map(|m| m.round()))).collect();
        assert_eq!(zm, [(Some(1527.0), Some(8.0)), (Some(-2600.0), None), (Some(0.0), Some(10.0))]);
        assert_eq!((line[2].x, line[2].y), (3.0, -3.0), "X and Y still add up");
    }

    #[test]
    fn coordinates_in_several_blocks_or_unpacked_are_one_stream() {
        let line =
            |coords: Vec<u8>| answer(&[number(7, POLYLINE), quantized(0), message(15, &message(2, &[packed(2, &[2]), coords].concat()))]);
        let split = line([packed(3, &[sint(1), sint(1)]), packed(3, &[sint(2), sint(2)])].concat());
        let unpacked = line([number(3, sint(1)), number(3, sint(1)), number(3, sint(2)), number(3, sint(2))].concat());
        for bytes in [split, unpacked] {
            let l = parse(&bytes, "x").unwrap();
            let Some(Geometry::LineString(p)) = &l.features[0].geometry else { panic!("not a line") };
            assert_eq!(p.iter().map(|c| (c.x, c.y)).collect::<Vec<_>>(), [(11.0, 99.0), (13.0, 97.0)]);
        }
    }

    #[test]
    fn numbers_and_coordinates_past_64_bits_are_refused_or_none() {
        // A tenth byte with more than the 64th bit, and a sum of differences past the range.
        let too_long = answer(&[message(15, &message(2, &[0x1a, 10, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02]))]);
        assert!(parse(&too_long, "x").unwrap_err().to_string().contains("too long"));
        let wraps = answer(&[number(7, POLYLINE), quantized(0), message(15, &geometry(&[2], &[i64::MAX, 0, 1, 0]))]);
        assert!(parse(&wraps, "x").unwrap_err().to_string().contains("out of range"));
        // A real past 64 bits in an integer field is null, as in ESRIJSON.
        let big = answer(&[field_of("A", 1), message(15, &value_field(double(3, 1e19)))]);
        assert_eq!(parse(&big, "x").unwrap().features[0].attributes, [FieldValue::Null]);
    }

    #[test]
    fn coordinates_without_a_transform_are_refused() {
        let bytes = answer(&[number(7, POLYLINE), message(15, &geometry(&[2], &[1, 1, 2, 2]))]);
        assert!(parse(&bytes, "x").unwrap_err().to_string().contains("no transform"));
    }

    #[test]
    fn a_z_of_0_is_the_z_translate() {
        // Unlike M, no answer seen gives 0 for a vertex without Z: it stays the translate's value.
        let scale = message(2, &[double(1, 1.0), double(2, 1.0), double(4, 0.0001)].concat());
        let translate = message(3, &double(4, -100_000.0));
        let t = message(12, &[scale, translate].concat());
        let bytes = answer(&[number(7, MULTIPOINT), number(10, 1), t, message(15, &geometry(&[1], &[1, 1, 0]))]);
        let l = parse(&bytes, "x").unwrap();
        let Some(Geometry::MultiPoint(p)) = &l.features[0].geometry else { panic!("not a multi point") };
        assert_eq!(p[0].z, Some(-100_000.0));
    }

    #[test]
    fn a_point_layer_may_leave_its_type_out() {
        let bytes = answer(&[quantized(0), message(15, &geometry(&[], &[1, 2]))]);
        let l = parse(&bytes, "x").unwrap();
        assert!(matches!(&l.features[0].geometry, Some(Geometry::Point(c)) if (c.x, c.y) == (11.0, 98.0)));
        assert_eq!(l.geom_type, Some(wbvector::GeometryType::Point));
    }

    #[test]
    fn without_geometries_no_type_is_declared() {
        let bytes = answer(&[field_of("A", 4), message(15, &value_field(message(1, b"x")))]);
        let l = parse(&bytes, "x").unwrap();
        assert_eq!(l.geom_type, None);
        assert!(l.features[0].geometry.is_none());
    }

    #[test]
    fn rings_become_polygons_by_containment() {
        let exterior: [i64; 10] = [0, 0, 4, 0, 0, -4, -4, 0, 0, 4];
        let hole: [i64; 10] = [1, -1, 1, 0, 0, -1, -1, 0, 0, 1];
        let bytes = answer(&[number(7, POLYGON), quantized(0), message(15, &geometry(&[5, 5], &[exterior, hole].concat()))]);
        let l = parse(&bytes, "x").unwrap();
        let Some(Geometry::Polygon { interiors, .. }) = &l.features[0].geometry else { panic!("not a polygon") };
        assert_eq!(interiors.len(), 1);
    }

    #[test]
    fn values_read_as_their_field_types_read_them() {
        let bytes = answer(&[
            field_of("I", 1),
            field_of("F", 2),
            field_of("D", 3),
            field_of("T", 4),
            field_of("O", 15),
            message(
                15,
                &[
                    value_field(message(1, b" 3 ")),
                    {
                        let mut single = Vec::new();
                        varint(&mut single, (2 << 3) | 5);
                        single.extend_from_slice(&0.1f32.to_le_bytes());
                        value_field(single)
                    },
                    value_field(message(1, b"NaN")),
                    value_field(number(4, sint(-5))),
                    value_field(number(9, 1)),
                ]
                .concat(),
            ),
        ]);
        let l = parse(&bytes, "x").unwrap();
        assert_eq!(
            cells(&l, 0),
            [
                FieldValue::Integer(3),
                FieldValue::Float(0.1),
                FieldValue::Null,
                FieldValue::Text("-5".into()),
                FieldValue::Text("true".into()),
            ]
        );
    }

    #[test]
    fn a_repeated_field_name_does_not_shift_the_values() {
        let bytes = answer(&[
            field_of("A", 4),
            field_of("A", 4),
            field_of("B", 4),
            message(15, &[value_field(message(1, b"a")), value_field(message(1, b"dup")), value_field(message(1, b"b"))].concat()),
        ]);
        let l = parse(&bytes, "x").unwrap();
        assert_eq!(cells(&l, 0), [FieldValue::Text("a".into()), FieldValue::Text("b".into())]);
    }

    #[test]
    fn features_without_an_object_id_say_so() {
        let bytes = answer(&[field_of("A", 4), message(15, &value_field(message(1, b"x")))]);
        assert_eq!(page(&bytes, "x", "x").unwrap().by_position, [0]);
        // With an object id field, those that leave it out.
        let bytes = answer(&[field_of("OBJECTID", 6), message(15, &value_field(number(5, 7))), message(15, &value_field(number(10, 1)))]);
        assert_eq!(page(&bytes, "x", "x").unwrap().by_position, [1]);
    }

    #[test]
    fn errors_name_what_is_wrong() {
        let count = [message(2, &message(2, &number(1, 5)))].concat();
        let curve = answer(&[message(15, &message(7, &number(1, 3)))]);
        let shape = answer(&[message(15, &message(3, &message(1, b"x")))]);
        let more_values = answer(&[message(15, &value_field(message(1, b"x")))]);
        let short = answer(&[number(7, POLYLINE), message(15, &geometry(&[2], &[1, 1]))]);
        let long = answer(&[number(7, POLYLINE), message(15, &geometry(&[1], &[1, 1, 2, 2]))]);
        let huge = answer(&[number(7, POLYLINE), message(15, &geometry(&[u64::MAX / 2], &[1, 1]))]);
        let mesh = answer(&[number(7, 4), message(15, &geometry(&[1], &[1, 1]))]);
        let two_points = answer(&[number(7, POINT), message(15, &geometry(&[2], &[1, 1, 1, 1]))]);
        let z_scale = message(12, &message(2, &[double(1, 1.0), double(2, 1.0), double(4, f64::NAN)].concat()));
        let nan_z = answer(&[number(10, 1), z_scale, message(15, &geometry(&[], &[1, 1, 1]))]);
        for (bytes, says) in [
            (&two_points, "more than one vertex"),
            (&nan_z, "not finite"),
            (&count, "count"),
            (&curve, "curve"),
            (&shape, "shape-buffer"),
            (&more_values, "more values"),
            (&short, "ends inside"),
            (&long, "more coordinates"),
            (&huge, "ends inside"),
            (&mesh, "not read"),
        ] {
            let e = parse(bytes, "x").unwrap_err().to_string();
            assert!(e.contains(says), "{says}: {e}");
        }
        assert!(parse(&[], "x").unwrap_err().to_string().contains("the answer is empty"));
        assert!(parse(&message(1, b"2.0"), "x").unwrap_err().to_string().contains("no feature result"));
        let json = b"\xEF\xBB\xBF \n{\"error\":{\"code\":400,\"message\":\"Invalid query\"}}";
        let e = parse(json, "x").unwrap_err().to_string();
        assert!(e.contains("Invalid query") && e.starts_with("ESRIPBF: "), "{e}");
        // Without a list, features in JSON are read.
        assert_eq!(parse(br#"{"features":[{"attributes":{"A":1}}]}"#, "x").unwrap().len(), 1);
    }

    #[test]
    fn a_bad_feature_is_named_under_one_prefix() {
        let short = answer(&[number(7, POLYLINE), message(15, &geometry(&[2], &[1, 1]))]);
        let e = parse(&short, "x").unwrap_err().to_string();
        assert!(e.starts_with("ESRIPBF: feature 0: ") && e.matches("ESRIPBF:").count() == 1, "{e}");
    }

    #[test]
    fn parts_with_vertices_need_coordinates() {
        let part = |lengths: &[u64]| answer(&[number(7, POLYLINE), message(15, &message(2, &packed(2, lengths)))]);
        assert!(parse(&part(&[2]), "x").unwrap_err().to_string().contains("no coordinates"));
        assert!(parse(&part(&[0]), "x").unwrap().features[0].geometry.is_none());
    }

    #[test]
    fn junk_and_cut_data_never_panic() {
        let bytes = answer(&[
            number(7, POLYGON),
            quantized(0),
            field_of("A", 4),
            message(15, &[value_field(message(1, b"x")), geometry(&[5], &[0, 0, 4, 0, 0, -4, -4, 0, 0, 4])].concat()),
        ]);
        for end in 0..bytes.len() {
            let _ = parse(&bytes[..end], "x");
            let _ = is_esripbf(&bytes[..end]);
        }
        for at in 0..bytes.len() {
            let mut junk = bytes.clone();
            junk[at] ^= 0xff;
            let _ = parse(&junk, "x");
        }
    }

    #[test]
    fn its_first_bytes_show_it() {
        let whole = answer(&[number(7, POLYGON), field_of("A", 4)]);
        assert!(is_esripbf(&whole));
        let without_version = message(2, &message(1, &[message(1, b"OBJECTID"), number(7, POLYGON)].concat()));
        assert!(is_esripbf(&without_version));
        // Whatever the lengths promise past the bytes given.
        let result = [message(1, b"OBJECTID"), vec![0; 1000]].concat();
        let long = message(2, &message(1, &result));
        assert!(is_esripbf(long.get(..TEST_HEAD_BYTES).unwrap()));

        assert!(!is_esripbf(&[]));
        assert!(!is_esripbf(b"{\"features\":[]}"));
        // OpenStreetMap PBF: a big-endian header length.
        assert!(!is_esripbf(b"\x00\x00\x00\x0d\x0a\x09OSMHeader"));
        // A vector tile: its layers are field 3.
        assert!(!is_esripbf(&message(3, &message(1, b"layer"))));
        // A count result: an answer, without features.
        let count = message(2, &message(2, &number(1, 5)));
        assert!(!is_esripbf(&count));
        assert!(is_query_answer(&count) && is_query_answer(&whole));
        assert!(!is_query_answer(&message(3, &message(1, b"layer"))));
        // A version that is not one.
        assert!(!is_esripbf(&[message(1, &[0, 1, 2]), message(2, &message(1, &number(7, 3)))].concat()));
    }
}
