//! GeoJSON, read and written as GDAL does.
//!
//! Reading: fields in the order the properties first appear, typed from their JSON values
//! (integers, reals, booleans, ISO 8601 dates and date-times, strings; arrays and objects become
//! JSON text), string feature ids as a leading `id` field and integer ids as feature ids, and the
//! CRS from a GeoJSON 2008 `crs` member (EPSG:4326 otherwise).
//!
//! Writing keeps the layer's coordinates and names its CRS in a `crs` member when it is not
//! WGS 84, as GDAL does by default; [`GeoJsonOptions::rfc7946`] reprojects to WGS 84 instead and
//! orients polygon rings counter-clockwise, like GDAL's `RFC7946=YES`.

use crate::crs::SpatialRef;
use crate::{Error, Result};
use serde_json::{Map, Value, json};
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, Layer, Ring};

/// Options for writing GeoJSON and GeoJSON text sequences.
#[derive(Debug, Clone, Default)]
pub struct GeoJsonOptions {
    /// Write RFC 7946 GeoJSON: coordinates reprojected to WGS 84, no `crs` member,
    /// counter-clockwise exterior rings (GDAL's `RFC7946=YES`).
    pub rfc7946: bool,
    /// The field written as each feature's `id` member instead of a property (GDAL's
    /// `ID_FIELD`). A layer without that field is written without `id` members, as by GDAL.
    pub id_field: Option<String>,
    /// The JSON type of the `id` member written from [`Self::id_field`] (GDAL's `ID_TYPE`). By
    /// default (`AUTO`), a number for integer and boolean fields and a string otherwise.
    pub id_type: Option<GeoJsonIdType>,
}

/// The JSON type of a feature's `id` member (GDAL's `ID_TYPE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeoJsonIdType {
    /// A string; `""` for a null value.
    String,
    /// An integer: text read as C's `atoi` reads it, reals truncated, `0` for a null value.
    Integer,
}

impl GeoJsonOptions {
    /// Sets an option by its GDAL name: `RFC7946` (YES/NO), `ID_FIELD` (a field name) or
    /// `ID_TYPE` (`AUTO`, `String` or `Integer`). Returns `false` for other names.
    pub fn set(&mut self, key: &str, value: &str) -> Result<bool> {
        match key.to_ascii_uppercase().as_str() {
            "RFC7946" => self.rfc7946 = matches!(value.to_ascii_uppercase().as_str(), "YES" | "TRUE" | "ON" | "1"),
            "ID_FIELD" => self.id_field = (!value.is_empty()).then(|| value.to_string()),
            "ID_TYPE" => {
                self.id_type = match value.to_ascii_uppercase().as_str() {
                    "AUTO" => None,
                    "STRING" => Some(GeoJsonIdType::String),
                    "INTEGER" => Some(GeoJsonIdType::Integer),
                    _ => return Err(Error::InvalidArgument(format!("ID_TYPE must be AUTO, String or Integer, not '{value}'"))),
                }
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}

/// The field written as each feature's `id` member: its index and whether the member is an
/// integer.
#[derive(Debug, Clone, Copy)]
struct IdColumn {
    index: usize,
    integer: bool,
}

impl IdColumn {
    /// The `id` column `options` asks for, when `fields` has it.
    fn of(fields: &[FieldDef], options: &GeoJsonOptions) -> Option<Self> {
        let name = options.id_field.as_deref()?;
        let index = fields.iter().position(|f| f.name == name)?;
        let integer = match options.id_type {
            Some(GeoJsonIdType::Integer) => true,
            Some(GeoJsonIdType::String) => false,
            None => matches!(fields[index].field_type, FieldType::Integer | FieldType::Boolean),
        };
        Some(Self { index, integer })
    }

    /// The `id` member written for a value of the field, as GDAL writes it.
    fn value(self, v: &FieldValue) -> Value {
        if self.integer {
            json!(match v {
                FieldValue::Integer(x) => *x,
                FieldValue::Boolean(b) => i64::from(*b),
                FieldValue::Float(x) if x.is_finite() => x.trunc() as i64,
                FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => atoi(s),
                _ => 0,
            })
        } else {
            json!(match v {
                FieldValue::Null => String::new(),
                FieldValue::Integer(x) => x.to_string(),
                FieldValue::Float(x) => x.to_string(),
                FieldValue::Boolean(b) => u8::from(*b).to_string(),
                FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => s.clone(),
                FieldValue::Blob(b) => b.iter().map(|x| format!("{x:02X}")).collect(),
            })
        }
    }
}

/// The integer at the start of `s`, as C's `atoi` reads it: `0` when there is none. As GDAL's
/// 64-bit parse does, a magnitude too large for `i64` saturates.
fn atoi(s: &str) -> i64 {
    // C's `isspace` characters, not Unicode's whitespace.
    let s = s.trim_start_matches([' ', '\t', '\n', '\x0B', '\x0C', '\r']);
    let (sign, digits) = match s.as_bytes().first() {
        Some(b'-') => (-1, &s[1..]),
        Some(b'+') => (1, &s[1..]),
        _ => (1, s),
    };
    let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
    let magnitude = &digits[..end];
    match magnitude.parse::<i64>() {
        Ok(n) => sign * n,
        // No digits at all, which `parse` rejects too.
        Err(_) if magnitude.is_empty() => 0,
        Err(_) if sign < 0 => i64::MIN,
        Err(_) => i64::MAX,
    }
}

/// Reads a GeoJSON file.
pub fn read(path: &str) -> Result<Layer> {
    let stem = std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("layer");
    parse(&std::fs::read(path)?, stem)
}

/// Parses GeoJSON. `default_name` names a layer whose collection has no `name`.
pub fn parse(bytes: &[u8], default_name: &str) -> Result<Layer> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let doc: Value = serde_json::from_slice(bytes).map_err(|e| Error::InvalidData(format!("GeoJSON: {e}")))?;
    let features: Vec<&Value> = match doc["type"].as_str() {
        Some("FeatureCollection") => doc["features"].as_array().map(|a| a.iter().collect()).unwrap_or_default(),
        Some("Feature") => vec![&doc],
        Some(_) => vec![&doc],
        None => return Err(Error::InvalidData("GeoJSON: no type".into())),
    };
    let mut schema = SchemaScan::default();
    for f in &features {
        schema.observe(f);
    }
    let crs = doc["crs"]["properties"]["name"].as_str().and_then(|n| SpatialRef::parse(n).ok());
    let mut layer = schema.layer(doc["name"].as_str().unwrap_or(default_name), crs);
    let reader = FeatureDecoder::new(&layer, &schema);
    for (n, f) in features.iter().enumerate() {
        layer.push(reader.feature(n as u64, f));
    }
    Ok(layer)
}

/// The fields of GeoJSON features, inferred as GDAL infers them: a scan of every feature.
#[derive(Debug, Default, Clone)]
pub(crate) struct SchemaScan {
    /// Some feature has a string `id`: ids become a leading `id` field.
    string_ids: bool,
    /// The first feature is a STAC item: foreign members become fields.
    stac: Option<bool>,
    /// The fields in the order they first appear, with their merged kinds.
    kinds: Vec<(String, Option<Kind>)>,
}

/// What the schema scan needs of a feature, without decoding its geometry.
#[derive(serde::Deserialize)]
struct Probe {
    #[serde(default)]
    id: Value,
    #[serde(default)]
    properties: Value,
    #[serde(default)]
    stac_version: Option<serde::de::IgnoredAny>,
}

impl SchemaScan {
    /// Adds a feature (a `Feature` object or a bare geometry).
    pub(crate) fn observe(&mut self, f: &Value) {
        let stac = *self.stac.get_or_insert_with(|| f.get("stac_version").is_some());
        self.string_ids |= f["id"].is_string();
        for (k, v) in attributes_of(f, stac) {
            self.add_kind(&k, v);
        }
    }

    /// Adds a feature given by its JSON text, decoding only its id and properties unless it is
    /// a STAC item.
    pub(crate) fn observe_raw(&mut self, raw: &[u8]) -> Result<()> {
        let err = |e: serde_json::Error| Error::InvalidData(format!("GeoJSON: {e}"));
        if self.stac.is_none() || self.stac == Some(true) {
            let v: Value = serde_json::from_slice(raw).map_err(err)?;
            self.observe(&v);
            return Ok(());
        }
        let p: Probe = serde_json::from_slice(raw).map_err(err)?;
        let _ = p.stac_version;
        self.string_ids |= p.id.is_string();
        if let Value::Object(props) = &p.properties {
            for (k, v) in props {
                self.add_kind(k, v);
            }
        }
        Ok(())
    }

    /// Whether a feature has been observed.
    pub(crate) fn started(&self) -> bool {
        self.stac.is_some()
    }

    /// An empty scan that decodes features as this one does (STAC items or not).
    pub(crate) fn empty_like(&self) -> Self {
        Self { string_ids: false, stac: self.stac, kinds: Vec::new() }
    }

    /// Adds the scan of the features that follow those of this scan.
    pub(crate) fn merge(&mut self, other: SchemaScan) {
        self.string_ids |= other.string_ids;
        if self.stac.is_none() {
            self.stac = other.stac;
        }
        for (k, t) in other.kinds {
            match self.kinds.iter_mut().find(|(n, _)| *n == k) {
                Some((_, prev)) => *prev = merge(*prev, t),
                None => self.kinds.push((k, t)),
            }
        }
    }

    fn add_kind(&mut self, k: &str, v: &Value) {
        let t = kind(v);
        match self.kinds.iter_mut().find(|(n, _)| n == k) {
            Some((_, prev)) => *prev = merge(*prev, t),
            None => self.kinds.push((k.to_string(), t)),
        }
    }

    /// The layer the features make, without its features.
    pub(crate) fn layer(&self, name: &str, crs: Option<SpatialRef>) -> Layer {
        let mut layer = Layer::new(name);
        if self.string_ids {
            layer.add_field(FieldDef::new("id", FieldType::Text));
        }
        for (k, t) in &self.kinds {
            if layer.schema.field_index(k).is_none() {
                layer.add_field(FieldDef::new(k, t.map_or(FieldType::Text, Kind::field_type)));
            }
        }
        match crs {
            Some(s) => s.apply_to_layer(&mut layer),
            None => layer.set_crs_epsg(Some(4326)),
        }
        layer
    }
}

/// Decodes features into the fields of a layer.
pub(crate) struct FeatureDecoder {
    names: Vec<String>,
    types: Vec<FieldType>,
    string_ids: bool,
    stac: bool,
}

impl FeatureDecoder {
    pub(crate) fn new(layer: &Layer, scan: &SchemaScan) -> Self {
        Self {
            names: layer.schema.fields().iter().map(|f| f.name.clone()).collect(),
            types: layer.schema.fields().iter().map(|f| f.field_type).collect(),
            string_ids: scan.string_ids,
            stac: scan.stac.unwrap_or(false),
        }
    }

    /// The `n`-th feature (numbered from 0) of the collection.
    pub(crate) fn feature(&self, n: u64, f: &Value) -> Feature {
        let mut attributes = vec![FieldValue::Null; self.types.len()];
        if self.string_ids {
            attributes[0] = match &f["id"] {
                Value::Null => FieldValue::Null,
                Value::String(s) => FieldValue::Text(s.clone()),
                v => FieldValue::Text(v.to_string()),
            };
        }
        for (k, v) in attributes_of(f, self.stac) {
            if let Some(i) = self.names.iter().position(|n| *n == k).filter(|&i| !(self.string_ids && i == 0)) {
                attributes[i] = value(v, self.types[i]);
            }
        }
        let fid = if self.string_ids { n } else { f["id"].as_u64().unwrap_or(n) };
        let geometry = if f["type"] == "Feature" { geometry(&f["geometry"]) } else { geometry(f) };
        Feature { fid, geometry, attributes }
    }

    /// Decodes features given by their JSON text, numbered from `first`, in parallel.
    pub(crate) fn features_raw(&self, first: u64, raw: &[Vec<u8>]) -> Result<Vec<Feature>> {
        crate::par::map_range(raw.len(), |i| {
            let v: Value = serde_json::from_slice(&raw[i]).map_err(|e| Error::InvalidData(format!("GeoJSON: {e}")))?;
            Ok(self.feature(first + i as u64, &v))
        })
        .into_iter()
        .collect()
    }
}

/// A feature's attributes: its properties and, for STAC items, its foreign members (all but
/// `type`, `id`, `geometry`, `bbox` and `properties`), each asset's members flattened to
/// `assets.<asset>.<member>`, as GDAL's `FOREIGN_MEMBERS=AUTO` reads them.
fn attributes_of(f: &Value, stac: bool) -> Vec<(String, &Value)> {
    let mut out: Vec<(String, &Value)> = f["properties"].as_object().into_iter().flatten().map(|(k, v)| (k.clone(), v)).collect();
    if !stac {
        return out;
    }
    for (k, v) in f.as_object().into_iter().flatten() {
        match k.as_str() {
            "type" | "id" | "geometry" | "bbox" | "properties" => {}
            "assets" if v.is_object() => {
                for (name, asset) in v.as_object().into_iter().flatten() {
                    for (m, mv) in asset.as_object().into_iter().flatten() {
                        out.push((format!("assets.{name}.{m}"), mv));
                    }
                }
            }
            _ => out.push((k.clone(), v)),
        }
    }
    out
}

/// The type of a property value, as GDAL infers it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Integer,
    Real,
    Boolean,
    Date,
    DateTime,
    String,
    Json,
}

impl Kind {
    fn field_type(self) -> FieldType {
        match self {
            Kind::Integer => FieldType::Integer,
            Kind::Real => FieldType::Float,
            Kind::Boolean => FieldType::Boolean,
            Kind::Date => FieldType::Date,
            Kind::DateTime => FieldType::DateTime,
            Kind::String => FieldType::Text,
            Kind::Json => FieldType::Json,
        }
    }
}

fn is_date(s: &str) -> bool {
    is_date_bytes(s.as_bytes())
}

fn is_date_bytes(b: &[u8]) -> bool {
    b.len() == 10 && b[4] == b'-' && b[7] == b'-' && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

fn is_datetime(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 19
        && is_date_bytes(&b[..10])
        && (b[10] == b'T' || b[10] == b' ')
        && b[13] == b':'
        && b[16] == b':'
        && [11, 12, 14, 15, 17, 18].iter().all(|&i| b[i].is_ascii_digit())
}

fn kind(v: &Value) -> Option<Kind> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(_) => Kind::Boolean,
        Value::Number(n) if n.is_i64() || n.is_u64() => Kind::Integer,
        Value::Number(_) => Kind::Real,
        Value::String(s) if is_datetime(s) => Kind::DateTime,
        Value::String(s) if is_date(s) => Kind::Date,
        Value::String(_) => Kind::String,
        _ => Kind::Json,
    })
}

fn merge(a: Option<Kind>, b: Option<Kind>) -> Option<Kind> {
    use Kind as K;
    Some(match (a, b) {
        (a, None) => return a,
        (None, b) => return b,
        (Some(a), Some(b)) if a == b => a,
        (Some(K::Integer | K::Real), Some(K::Integer | K::Real)) => K::Real,
        (Some(K::Date | K::DateTime), Some(K::Date | K::DateTime)) => K::DateTime,
        _ => K::String,
    })
}

fn value(v: &Value, t: FieldType) -> FieldValue {
    match (t, v) {
        (_, Value::Null) => FieldValue::Null,
        (FieldType::Integer, v) => v.as_i64().map_or(FieldValue::Null, FieldValue::Integer),
        (FieldType::Float, v) => v.as_f64().map_or(FieldValue::Null, FieldValue::Float),
        (FieldType::Boolean, Value::Bool(b)) => FieldValue::Boolean(*b),
        (FieldType::Date, Value::String(s)) => FieldValue::Date(s.clone()),
        (FieldType::DateTime, Value::String(s)) => FieldValue::DateTime(s.clone()),
        (_, Value::String(s)) => FieldValue::Text(s.clone()),
        (_, v) => FieldValue::Text(v.to_string()),
    }
}

fn position(v: &Value) -> Option<Coord> {
    let a = v.as_array()?;
    Some(Coord { x: a.first()?.as_f64()?, y: a.get(1)?.as_f64()?, z: a.get(2).and_then(Value::as_f64), m: None })
}

fn positions(v: &Value) -> Vec<Coord> {
    v.as_array().into_iter().flatten().filter_map(position).collect()
}

fn ring(v: &Value) -> Ring {
    let mut cs = positions(v);
    if cs.len() > 1 && cs.first().map(|c| (c.x, c.y)) == cs.last().map(|c| (c.x, c.y)) {
        cs.pop();
    }
    Ring::new(cs)
}

fn polygon(v: &Value) -> Option<(Ring, Vec<Ring>)> {
    let mut rings = v.as_array()?.iter().map(ring);
    let exterior = rings.next()?;
    Some((exterior, rings.collect()))
}

/// Parses a GeoJSON geometry object.
pub fn geometry(g: &Value) -> Option<Geometry> {
    let c = &g["coordinates"];
    let list = || c.as_array().into_iter().flatten();
    Some(match g["type"].as_str()? {
        "Point" => Geometry::Point(position(c)?),
        "MultiPoint" => Geometry::MultiPoint(positions(c)),
        "LineString" => Geometry::LineString(positions(c)),
        "MultiLineString" => Geometry::MultiLineString(list().map(positions).collect()),
        "Polygon" => {
            let (exterior, interiors) = polygon(c)?;
            Geometry::Polygon { exterior, interiors }
        }
        "MultiPolygon" => Geometry::MultiPolygon(list().filter_map(polygon).collect()),
        "GeometryCollection" => {
            Geometry::GeometryCollection(g["geometries"].as_array().into_iter().flatten().filter_map(geometry).collect())
        }
        _ => return None,
    })
}

/// Writes a layer as GeoJSON.
pub fn write(layer: &Layer, path: &str, options: &GeoJsonOptions) -> Result<()> {
    std::fs::write(path, to_string(layer, options)?)?;
    Ok(())
}

/// Encodes a layer as a GeoJSON FeatureCollection.
pub fn to_string(layer: &Layer, options: &GeoJsonOptions) -> Result<String> {
    let mut w = CollectionEncoder::new(layer, options)?;
    let mut out = w.head();
    w.features(layer, &mut out)?;
    out.push_str(CollectionEncoder::TAIL);
    Ok(out)
}

/// Encodes a FeatureCollection piece by piece: its head, its features (in as many chunks as
/// needed), its tail. The text is the same as [`to_string`] gives.
pub(crate) struct CollectionEncoder {
    head: String,
    rfc7946: bool,
    id: Option<IdColumn>,
    reproject: Option<SpatialRef>,
    first: bool,
}

impl CollectionEncoder {
    /// The end of the collection.
    pub(crate) const TAIL: &str = "]}\n";

    /// An encoder for the features of `layer` (its fields and CRS; its features are not read).
    pub(crate) fn new(layer: &Layer, options: &GeoJsonOptions) -> Result<Self> {
        let srs = SpatialRef::from_layer(layer);
        let reproject =
            (options.rfc7946 && !srs.is_unknown() && srs.epsg != Some(4326)).then(|| SpatialRef::from_epsg(4326)).transpose()?;
        let mut doc = Map::new();
        doc.insert("type".into(), json!("FeatureCollection"));
        doc.insert("name".into(), json!(layer.name));
        if !options.rfc7946
            && !srs.is_unknown()
            && srs.epsg != Some(4326)
            && let Some(code) = super::naming_epsg(layer)
        {
            doc.insert("crs".into(), json!({"type": "name", "properties": {"name": format!("urn:ogc:def:crs:EPSG::{code}")}}));
        }
        doc.insert("features".into(), Value::Array(Vec::new()));
        let text = Value::Object(doc).to_string();
        let head = text.strip_suffix("]}").unwrap_or(&text).to_string();
        Ok(Self { head, rfc7946: options.rfc7946, id: IdColumn::of(layer.schema.fields(), options), reproject, first: true })
    }

    /// The head of the collection, up to the opening of its `features` array.
    pub(crate) fn head(&self) -> String {
        self.head.clone()
    }

    /// Appends the features of a chunk (a layer with the fields given to [`Self::new`]).
    pub(crate) fn features(&mut self, chunk: &Layer, out: &mut String) -> Result<()> {
        let reprojected;
        let chunk = match &self.reproject {
            Some(dst) => {
                reprojected = crate::vector::reproject_layer(chunk, dst)?;
                &reprojected
            }
            None => chunk,
        };
        let fields = chunk.schema.fields();
        let (rfc, id) = (self.rfc7946, self.id);
        let texts = crate::par::map_range(chunk.features.len(), |i| feature_json(&chunk.features[i], fields, rfc, id).to_string());
        for t in texts {
            if !self.first {
                out.push(',');
            }
            self.first = false;
            // One feature per line, as GDAL writes it.
            out.push_str(&t.replace("{\"type\":\"Feature\",", "\n{\"type\":\"Feature\","));
        }
        Ok(())
    }
}

/// A feature as a GeoJSON `Feature` object, with the `id` member `id` gives.
fn feature_json(f: &Feature, fields: &[FieldDef], rfc7946: bool, id: Option<IdColumn>) -> Value {
    let props: Map<String, Value> = fields
        .iter()
        .zip(&f.attributes)
        .enumerate()
        .filter(|&(i, _)| id.is_none_or(|c| c.index != i))
        .map(|(_, (d, v))| {
            let v = match v {
                FieldValue::Null => Value::Null,
                FieldValue::Integer(x) => json!(x),
                FieldValue::Float(x) if x.is_finite() => json!(x),
                FieldValue::Float(_) => Value::Null,
                FieldValue::Boolean(b) => json!(b),
                FieldValue::Text(s) if d.field_type == FieldType::Json => serde_json::from_str(s).unwrap_or_else(|_| json!(s)),
                FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => json!(s),
                FieldValue::Blob(b) => json!(b.iter().map(|x| format!("{x:02X}")).collect::<String>()),
            };
            (d.name.clone(), v)
        })
        .collect();
    let mut feature = Map::new();
    feature.insert("type".into(), json!("Feature"));
    if let Some(c) = id {
        feature.insert("id".into(), c.value(f.attributes.get(c.index).unwrap_or(&FieldValue::Null)));
    }
    feature.insert("properties".into(), Value::Object(props));
    feature.insert("geometry".into(), f.geometry.as_ref().map_or(Value::Null, |g| encode(g, rfc7946)));
    Value::Object(feature)
}

/// Encodes GeoJSON text sequences (RFC 8142, GDAL's GeoJSONSeq): one feature per line, in
/// WGS 84 with counter-clockwise exterior rings, as GDAL writes them.
pub(crate) struct SeqEncoder {
    reproject: Option<SpatialRef>,
    id: Option<IdColumn>,
}

impl SeqEncoder {
    /// An encoder for the features of `layer`.
    pub(crate) fn new(layer: &Layer, options: &GeoJsonOptions) -> Result<Self> {
        let srs = SpatialRef::from_layer(layer);
        let reproject = (!srs.is_unknown() && srs.epsg != Some(4326)).then(|| SpatialRef::from_epsg(4326)).transpose()?;
        Ok(Self { reproject, id: IdColumn::of(layer.schema.fields(), options) })
    }

    /// Appends the features of a chunk, a line each.
    pub(crate) fn features(&self, chunk: &Layer, out: &mut String) -> Result<()> {
        let reprojected;
        let chunk = match &self.reproject {
            Some(dst) => {
                reprojected = crate::vector::reproject_layer(chunk, dst)?;
                &reprojected
            }
            None => chunk,
        };
        let fields = chunk.schema.fields();
        let id = self.id;
        for t in crate::par::map_range(chunk.features.len(), |i| feature_json(&chunk.features[i], fields, true, id).to_string()) {
            out.push_str(&t);
            out.push('\n');
        }
        Ok(())
    }
}

/// Encodes a layer as a GeoJSON text sequence.
pub fn to_seq_string(layer: &Layer, options: &GeoJsonOptions) -> Result<String> {
    let mut out = String::new();
    SeqEncoder::new(layer, options)?.features(layer, &mut out)?;
    Ok(out)
}

/// Parses a GeoJSON text sequence (one feature per line, optionally preceded by the record
/// separator). `name` names the layer; the CRS is WGS 84.
pub fn parse_seq(bytes: &[u8], name: &str) -> Result<Layer> {
    let mut scan = SchemaScan::default();
    let lines: Vec<&[u8]> = bytes.split(|&b| b == b'\n').map(seq_record).filter(|l| !l.is_empty()).collect();
    for l in &lines {
        scan.observe_raw(l)?;
    }
    let mut layer = scan.layer(name, None);
    let decoder = FeatureDecoder::new(&layer, &scan);
    let raw: Vec<Vec<u8>> = lines.iter().map(|l| l.to_vec()).collect();
    layer.features = decoder.features_raw(0, &raw)?;
    Ok(layer)
}

/// A line of a text sequence without its record separators and surrounding white space.
pub(crate) fn seq_record(line: &[u8]) -> &[u8] {
    let is_space = |b: &u8| b.is_ascii_whitespace() || *b == 0x1e;
    let start = line.iter().position(|b| !is_space(b)).unwrap_or(line.len());
    let end = line.iter().rposition(|b| !is_space(b)).map_or(start, |e| e + 1);
    &line[start..end]
}

fn pos(c: &Coord) -> Value {
    match c.z {
        Some(z) => json!([c.x, c.y, z]),
        None => json!([c.x, c.y]),
    }
}

/// A closed ring's positions; with `ccw`, exterior rings counter-clockwise and holes clockwise.
fn ring_json(r: &Ring, exterior: bool, ccw: bool) -> Value {
    let mut cs = r.0.clone();
    if ccw && (r.signed_area() > 0.0) != exterior {
        cs.reverse();
    }
    if let Some(first) = cs.first().cloned()
        && cs.len() > 1
        && cs.last().map(|l| (l.x, l.y)) != Some((first.x, first.y))
    {
        cs.push(first);
    }
    Value::Array(cs.iter().map(pos).collect())
}

fn polygon_json(e: &Ring, hs: &[Ring], ccw: bool) -> Value {
    Value::Array(std::iter::once(ring_json(e, true, ccw)).chain(hs.iter().map(|h| ring_json(h, false, ccw))).collect())
}

fn encode(g: &Geometry, ccw: bool) -> Value {
    match g {
        Geometry::Point(c) => json!({"type": "Point", "coordinates": pos(c)}),
        Geometry::MultiPoint(cs) => json!({"type": "MultiPoint", "coordinates": cs.iter().map(pos).collect::<Vec<_>>()}),
        Geometry::LineString(cs) => json!({"type": "LineString", "coordinates": cs.iter().map(pos).collect::<Vec<_>>()}),
        Geometry::MultiLineString(ls) => json!({
            "type": "MultiLineString",
            "coordinates": ls.iter().map(|l| l.iter().map(pos).collect::<Vec<_>>()).collect::<Vec<_>>(),
        }),
        Geometry::Polygon { exterior, interiors } => json!({"type": "Polygon", "coordinates": polygon_json(exterior, interiors, ccw)}),
        Geometry::MultiPolygon(ps) => {
            json!({"type": "MultiPolygon", "coordinates": ps.iter().map(|(e, h)| polygon_json(e, h, ccw)).collect::<Vec<_>>()})
        }
        Geometry::GeometryCollection(gs) => {
            json!({"type": "GeometryCollection", "geometries": gs.iter().map(|g| encode(g, ccw)).collect::<Vec<_>>()})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_field_is_written_as_each_features_id_as_gdal_writes_it() {
        let mixed = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","id":"pole-17","properties":{"name":"a"},"geometry":{"type":"Point","coordinates":[30,-2]}},
            {"type":"Feature","id":42,"properties":{"name":"b"},"geometry":{"type":"Point","coordinates":[30.1,-2]}},
            {"type":"Feature","id":3.5,"properties":{"name":"c"},"geometry":{"type":"Point","coordinates":[30.2,-2]}},
            {"type":"Feature","properties":{"name":"d"},"geometry":{"type":"Point","coordinates":[30.3,-2]}}]}"#;
        let typed = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{"num":5,"real":2.5,"flag":true},"geometry":{"type":"Point","coordinates":[30,-2]}},
            {"type":"Feature","properties":{"num":null,"real":null,"flag":false},"geometry":{"type":"Point","coordinates":[30.1,-2]}},
            {"type":"Feature","properties":{"num":12,"real":-7.25,"flag":null},"geometry":{"type":"Point","coordinates":[30.2,-2]}}]}"#;
        // The `id` members GDAL 3.12.2 writes, to GeoJSON and GeoJSONSeq, with
        // -lco ID_FIELD=<field> -lco ID_TYPE=<type>.
        let cases = [
            (mixed, "id", "AUTO", r#"["pole-17","42","3.5",""]"#),
            (mixed, "id", "String", r#"["pole-17","42","3.5",""]"#),
            (mixed, "id", "Integer", "[0,42,3,0]"),
            (typed, "num", "AUTO", "[5,0,12]"),
            (typed, "num", "String", r#"["5","","12"]"#),
            (typed, "num", "Integer", "[5,0,12]"),
            (typed, "real", "AUTO", r#"["2.5","","-7.25"]"#),
            (typed, "real", "String", r#"["2.5","","-7.25"]"#),
            (typed, "real", "Integer", "[2,0,-7]"),
            (typed, "flag", "AUTO", "[1,0,0]"),
            (typed, "flag", "String", r#"["1","0",""]"#),
            (typed, "flag", "Integer", "[1,0,0]"),
        ];
        for (source, field, id_type, ids) in cases {
            let layer = parse(source.as_bytes(), "l").unwrap();
            let mut options = GeoJsonOptions::default();
            assert!(options.set("ID_FIELD", field).unwrap() && options.set("id_type", id_type).unwrap());
            let expected: Value = serde_json::from_str(ids).unwrap();
            let seq = to_seq_string(&layer, &options).unwrap();
            let seq = format!("{{\"features\":[{}]}}", seq.trim_end().replace('\n', ","));
            for text in [to_string(&layer, &options).unwrap(), seq] {
                let doc: Value = serde_json::from_str(&text).unwrap();
                let features = doc["features"].as_array().unwrap();
                let got = Value::Array(features.iter().map(|f| f["id"].clone()).collect());
                assert_eq!(got, expected, "{field} {id_type}");
                assert!(features.iter().all(|f| f["properties"].get(field).is_none()), "{field} {id_type}");
            }
        }
        // The member follows `type`, as GDAL writes it; a field the layer lacks writes no `id`.
        let layer = parse(mixed.as_bytes(), "l").unwrap();
        let options = GeoJsonOptions { id_field: Some("id".into()), ..Default::default() };
        assert!(to_string(&layer, &options).unwrap().contains(r#"{"type":"Feature","id":"pole-17","properties":{"name":"a"}"#));
        let options = GeoJsonOptions { id_field: Some("nope".into()), ..Default::default() };
        assert!(to_string(&layer, &options).unwrap().contains(r#"{"type":"Feature","properties":{"id":"pole-17","name":"a"}"#));
        assert!(GeoJsonOptions::default().set("ID_TYPE", "Float").is_err());
    }

    #[test]
    fn atoi_reads_text_ids_as_c_does() {
        assert_eq!(atoi("42"), 42);
        assert_eq!(atoi(" \t\n\x0B\x0C\r-7x"), -7);
        assert_eq!(atoi("\u{a0}42"), 0);
        assert_eq!(atoi("+"), 0);
        assert_eq!(atoi("x1"), 0);
        assert_eq!(atoi("-9223372036854775808"), i64::MIN);
        assert_eq!(atoi("-99999999999999999999"), i64::MIN);
        assert_eq!(atoi("9223372036854775807"), i64::MAX);
        assert_eq!(atoi("9223372036854775808"), i64::MAX);
    }

    #[test]
    fn reads_long_multibyte_text() {
        // Byte 10 of these names falls inside a character; the date check must not slice there (#159).
        let text = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{"name":"東京都千代田区"},"geometry":{"type":"Point","coordinates":[139.7,35.7]}},
            {"type":"Feature","properties":{"name":"2020-01-0千代田区一丁目"},"geometry":null}]}"#;
        let l = parse(text.as_bytes(), "file").unwrap();
        assert_eq!(l.schema.fields()[0].field_type, FieldType::Text);
        assert_eq!(l.features[0].attributes[0], FieldValue::Text("東京都千代田区".into()));
    }

    #[test]
    fn reads_like_gdal() {
        let text = r#"{"type":"FeatureCollection","name":"places","crs":{"type":"name","properties":{"name":"urn:ogc:def:crs:EPSG::32633"}},
            "features":[
            {"type":"Feature","id":"a","properties":{"name":"Côte d'Ivoire","n":3.0,"k":1,"when":"2020-01-02","tags":[1,2]},"geometry":{"type":"Point","coordinates":[500000,10]}},
            {"type":"Feature","id":"b","properties":{"name":null,"n":2,"k":2,"when":"2020-01-03T10:00:00Z","flag":true},"geometry":null}]}"#;
        let l = parse(text.as_bytes(), "file").unwrap();
        assert_eq!((l.name.as_str(), l.crs_epsg()), ("places", Some(32633)));
        let t: Vec<(&str, FieldType)> = l.schema.fields().iter().map(|f| (f.name.as_str(), f.field_type)).collect();
        assert_eq!(
            t,
            [
                ("id", FieldType::Text),
                ("name", FieldType::Text),
                ("n", FieldType::Float),
                ("k", FieldType::Integer),
                ("when", FieldType::DateTime),
                ("tags", FieldType::Json),
                ("flag", FieldType::Boolean)
            ]
        );
        assert_eq!(l.features[0].attributes[1], FieldValue::Text("Côte d'Ivoire".into()));
        assert_eq!(l.features[0].attributes[2], FieldValue::Float(3.0));

        // GDAL's default keeps the coordinates and names the CRS.
        let out = to_string(&l, &GeoJsonOptions::default()).unwrap();
        assert!(out.contains("urn:ogc:def:crs:EPSG::32633"), "{out}");
        let back = parse(out.as_bytes(), "file").unwrap();
        assert_eq!(back.crs_epsg(), Some(32633));
        assert_eq!(back.features[0].geometry, Some(Geometry::point(500000.0, 10.0)));
        assert_eq!(back.features[0].attributes, l.features[0].attributes);
        // RFC 7946: WGS 84.
        let out = to_string(&l, &GeoJsonOptions { rfc7946: true, ..Default::default() }).unwrap();
        let back = parse(out.as_bytes(), "file").unwrap();
        assert!(!out.contains("\"crs\""));
        let Some(Geometry::Point(c)) = &back.features[0].geometry else { panic!("no point") };
        assert!((c.x - 15.0).abs() < 1e-6, "{}", c.x);
    }
}
