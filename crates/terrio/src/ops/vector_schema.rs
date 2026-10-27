//! Layer schemas as GDAL describes them: OGR field types and subtypes, the `OGR_SCHEMA` JSON
//! document (`gdal vector export-schema`), layers created from a schema (`gdal vector create`),
//! and field type changes with GDAL's value conversions (`gdal vector set-field-type`).
//!
//! terrio's data model holds fewer types than OGR: 64-bit integers, doubles, text, booleans,
//! blobs, dates, date-times and JSON. OGR types map onto them as follows: `Integer` and
//! `Integer64` are integers (an `Integer64` field declares a width of at least 10, as DBF and
//! GeoPackage readers record it); `Real` and `Float32` are doubles; `String`, `UUID` and `Time` are text (times as `HH:MM:SS[.sss]`);
//! `Int16` is an integer clamped to 16 bits; `Boolean` is a boolean; `JSON` is JSON text;
//! `Binary` is a blob. List types are not supported.

use crate::crs::SpatialRef;
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use wbvector::{FieldDef, FieldType, FieldValue, GeometryType, Layer};

/// The width an `Integer64` field declares when it has none of its own: DBF's width of GDAL's
/// 64-bit integer fields. Integer fields at least 10 wide are 64-bit, as in GDAL's DBF reader.
pub const INTEGER64_WIDTH: usize = 18;

/// An OGR field type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OgrFieldType {
    /// 32-bit integer.
    Integer,
    /// 64-bit integer.
    Integer64,
    /// Double-precision real.
    Real,
    /// UTF-8 string.
    String,
    /// Date (`YYYY-MM-DD`).
    Date,
    /// Time of day (`HH:MM:SS[.sss]`).
    Time,
    /// Date and time.
    DateTime,
    /// Raw bytes.
    Binary,
}

/// An OGR field subtype.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum OgrFieldSubType {
    /// No subtype.
    #[default]
    None,
    /// A boolean stored in an `Integer`.
    Boolean,
    /// A 16-bit integer stored in an `Integer`.
    Int16,
    /// A single-precision real stored in a `Real`.
    Float32,
    /// JSON text stored in a `String`.
    Json,
    /// A UUID stored in a `String`.
    Uuid,
}

impl OgrFieldType {
    /// GDAL's name of the type.
    pub fn name(self) -> &'static str {
        match self {
            Self::Integer => "Integer",
            Self::Integer64 => "Integer64",
            Self::Real => "Real",
            Self::String => "String",
            Self::Date => "Date",
            Self::Time => "Time",
            Self::DateTime => "DateTime",
            Self::Binary => "Binary",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "integer" => Self::Integer,
            "integer64" => Self::Integer64,
            "real" => Self::Real,
            "string" => Self::String,
            "date" => Self::Date,
            "time" => Self::Time,
            "datetime" => Self::DateTime,
            "binary" => Self::Binary,
            _ => return None,
        })
    }
}

impl OgrFieldSubType {
    /// GDAL's name of the subtype.
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Boolean => "Boolean",
            Self::Int16 => "Int16",
            Self::Float32 => "Float32",
            Self::Json => "JSON",
            Self::Uuid => "UUID",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_lowercase().as_str() {
            "none" => Self::None,
            "boolean" => Self::Boolean,
            "int16" => Self::Int16,
            "float32" => Self::Float32,
            "json" => Self::Json,
            "uuid" => Self::Uuid,
            _ => return None,
        })
    }

    /// The type a subtype goes with.
    fn base(self) -> Option<OgrFieldType> {
        match self {
            Self::None => None,
            Self::Boolean | Self::Int16 => Some(OgrFieldType::Integer),
            Self::Float32 => Some(OgrFieldType::Real),
            Self::Json | Self::Uuid => Some(OgrFieldType::String),
        }
    }
}

/// An OGR field type with its subtype.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OgrType {
    /// The type.
    pub field_type: OgrFieldType,
    /// The subtype.
    pub subtype: OgrFieldSubType,
}

impl OgrType {
    /// A type without subtype.
    pub fn new(field_type: OgrFieldType) -> Self {
        Self { field_type, subtype: OgrFieldSubType::None }
    }

    /// Parses a type (`Integer`, `Integer64`, `Real`, `String`, `Date`, `Time`, `DateTime`,
    /// `Binary`) or a subtype (`Boolean`, `Int16`, `Float32`, `JSON`, `UUID`), which implies its
    /// type, as `gdal vector set-field-type` does (case-insensitive). List types are refused.
    pub fn parse(s: &str) -> Result<Self> {
        let t = s.trim();
        if let Some(field_type) = OgrFieldType::parse(t) {
            return Ok(Self::new(field_type));
        }
        if let Some(subtype) = OgrFieldSubType::parse(t)
            && let Some(field_type) = subtype.base()
        {
            return Ok(Self { field_type, subtype });
        }
        if t.to_ascii_lowercase().ends_with("list") {
            return Err(Error::Unsupported(format!("field type '{t}': list types are not supported")));
        }
        Err(Error::InvalidArgument(format!(
            "unknown field type '{t}': expected Integer, Integer64, Real, String, Date, Time, DateTime, Binary, \
             or a subtype Boolean, Int16, Float32, JSON, UUID"
        )))
    }

    /// The type of a type name and an optional subtype name, as `OGR_SCHEMA` gives them.
    fn from_names(field_type: Option<&str>, subtype: Option<&str>) -> Result<Option<Self>> {
        let subtype = match subtype {
            Some(s) => Some(OgrFieldSubType::parse(s).ok_or_else(|| Error::InvalidArgument(format!("unknown field subtype '{s}'")))?),
            None => None,
        };
        match (field_type, subtype) {
            (Some(t), s) => {
                let field_type = OgrType::parse(t)?.field_type;
                Ok(Some(Self { field_type, subtype: s.unwrap_or_default() }))
            }
            (None, Some(s)) => Ok(s.base().map(|field_type| Self { field_type, subtype: s })),
            (None, None) => Ok(None),
        }
    }

    /// The terrio field type that holds values of this type.
    pub fn storage(self) -> FieldType {
        match (self.field_type, self.subtype) {
            (OgrFieldType::Integer, OgrFieldSubType::Boolean) => FieldType::Boolean,
            (OgrFieldType::Integer | OgrFieldType::Integer64, _) => FieldType::Integer,
            (OgrFieldType::Real, _) => FieldType::Float,
            (OgrFieldType::String, OgrFieldSubType::Json) => FieldType::Json,
            (OgrFieldType::String | OgrFieldType::Time, _) => FieldType::Text,
            (OgrFieldType::Date, _) => FieldType::Date,
            (OgrFieldType::DateTime, _) => FieldType::DateTime,
            (OgrFieldType::Binary, _) => FieldType::Blob,
        }
    }

    fn is_numeric(self) -> bool {
        matches!(self.field_type, OgrFieldType::Integer | OgrFieldType::Integer64 | OgrFieldType::Real)
    }
}

impl std::fmt::Display for OgrType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.subtype {
            OgrFieldSubType::None => f.write_str(self.field_type.name()),
            s => write!(f, "{}({})", self.field_type.name(), s.name()),
        }
    }
}

/// The OGR type GDAL would give field `i` of a layer: integers declaring a width of at least
/// 10, or holding values beyond 32 bits, are `Integer64`; booleans are `Integer(Boolean)`; JSON
/// text is `String(JSON)`.
pub fn ogr_type(layer: &Layer, i: usize) -> OgrType {
    let Some(def) = layer.schema.fields().get(i) else { return OgrType::new(OgrFieldType::String) };
    match def.field_type {
        FieldType::Integer => {
            let wide = def.width >= 10
                || layer
                    .features
                    .iter()
                    .any(|f| f.attributes.get(i).and_then(FieldValue::as_i64).is_some_and(|v| i32::try_from(v).is_err()));
            OgrType::new(if wide { OgrFieldType::Integer64 } else { OgrFieldType::Integer })
        }
        FieldType::Float => OgrType::new(OgrFieldType::Real),
        FieldType::Text => OgrType::new(OgrFieldType::String),
        FieldType::Boolean => OgrType { field_type: OgrFieldType::Integer, subtype: OgrFieldSubType::Boolean },
        FieldType::Blob => OgrType::new(OgrFieldType::Binary),
        FieldType::Date => OgrType::new(OgrFieldType::Date),
        FieldType::DateTime => OgrType::new(OgrFieldType::DateTime),
        FieldType::Json => OgrType { field_type: OgrFieldType::String, subtype: OgrFieldSubType::Json },
    }
}

/// A terrio field definition of an OGR type: `Integer64` fields declare a width of at least
/// [`INTEGER64_WIDTH`] when they have none (so that writers keep them 64-bit), and `Integer`
/// fields at most 9 (so that they stay 32-bit).
pub fn field_def(name: &str, t: OgrType, width: usize, precision: usize) -> FieldDef {
    let mut def = FieldDef::new(name, t.storage());
    def.width = match t.field_type {
        OgrFieldType::Integer64 if width < 10 => INTEGER64_WIDTH,
        OgrFieldType::Integer if width >= 10 => 0,
        OgrFieldType::Integer | OgrFieldType::Integer64 | OgrFieldType::Real | OgrFieldType::String => width,
        _ => 0,
    };
    def.precision = if t.field_type == OgrFieldType::Real { precision } else { 0 };
    def
}

// ------------------------------------------------------------------------------------------------
// Value conversions
// ------------------------------------------------------------------------------------------------

/// Counts of the conversions that lost information, reported as warnings once per field.
#[derive(Default)]
struct Lossy {
    partial: usize,
    lossy: usize,
    overflow: usize,
    boolean: usize,
    dropped: usize,
}

/// The integer prefix of a string, as `strtol` reads it (leading spaces, a sign, digits), and
/// whether the whole string was read. Values beyond 64 bits saturate.
fn parse_int_prefix(s: &str) -> (i64, bool) {
    let t = s.trim_start();
    let b = t.as_bytes();
    let mut i = 0;
    let neg = matches!(b.first(), Some(b'-'));
    if matches!(b.first(), Some(b'-' | b'+')) {
        i = 1;
    }
    let start = i;
    let mut v: i128 = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        v = (v * 10 + i128::from(b[i] - b'0')).min(i128::from(i64::MAX) + 1);
        i += 1;
    }
    if i == start {
        return (0, s.is_empty());
    }
    let v = if neg { -v } else { v };
    (v.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64, i == b.len())
}

/// The real prefix of a string, as `strtod` reads it, and whether the whole string was read.
fn parse_real_prefix(s: &str) -> (f64, bool) {
    let t = s.trim_start();
    if t.is_empty() {
        return (0.0, s.is_empty());
    }
    // The longest prefix that parses (numbers are short: try from the longest).
    let candidates = t.char_indices().map(|(i, c)| i + c.len_utf8()).collect::<Vec<_>>();
    for &end in candidates.iter().rev() {
        let p = &t[..end];
        let lower = p.to_ascii_lowercase();
        if lower.ends_with('e') || lower.ends_with("e+") || lower.ends_with("e-") || lower == "nan" || lower.contains("inf") {
            continue;
        }
        if let Ok(v) = p.parse::<f64>() {
            return (v, end == t.len());
        }
    }
    (0.0, false)
}

/// Whether a string reads as a number, as `CPLGetValueType` tells integers and reals from
/// strings.
fn is_number(s: &str) -> bool {
    let t = s.trim();
    !t.is_empty() && t.parse::<f64>().is_ok() && !t.to_ascii_lowercase().contains("inf") && !t.to_ascii_lowercase().contains("nan")
}

/// `(year, month, day, hour, minute, second, fraction, time zone)` of a date, time or date-time
/// as `OGRParseDate` reads them: `YYYY-MM-DD` or `YYYY/MM/DD`, optionally followed by a time
/// after a space or `T`, or a time `HH:MM[:SS[.sss]]` alone (with the date left at 0).
#[derive(Debug, Clone, Default, PartialEq)]
struct DateParts {
    date: Option<(i32, u32, u32)>,
    time: Option<(u32, u32, u32, String)>,
    tz: String,
}

fn parse_datetime(s: &str) -> Option<DateParts> {
    let s = s.trim();
    let mut out = DateParts::default();
    let rest = if s.len() >= 10 && (s.as_bytes()[4] == b'-' || s.as_bytes()[4] == b'/') && s.as_bytes()[7] == s.as_bytes()[4] {
        let y: i32 = s.get(..4)?.parse().ok()?;
        let m: u32 = s.get(5..7)?.parse().ok()?;
        let d: u32 = s.get(8..10)?.parse().ok()?;
        if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
            return None;
        }
        out.date = Some((y, m, d));
        let r = &s[10..];
        match r.chars().next() {
            None => return Some(out),
            Some(' ' | 'T') => &r[1..],
            _ => return None,
        }
    } else {
        s
    };
    // HH:MM[:SS[.sss]][Z|+HH[:MM]|-HH[:MM]]
    let b = rest.as_bytes();
    if b.len() < 5 || b[2] != b':' {
        return out.date.is_some().then_some(out).filter(|_| rest.is_empty());
    }
    let h: u32 = rest.get(..2)?.parse().ok()?;
    let mi: u32 = rest.get(3..5)?.parse().ok()?;
    let mut i = 5;
    let mut sec = 0;
    let mut frac = String::new();
    if b.len() >= 8 && b[5] == b':' {
        sec = rest.get(6..8)?.parse().ok()?;
        i = 8;
        if b.len() > 8 && b[8] == b'.' {
            let digits: String = rest[9..].chars().take_while(char::is_ascii_digit).collect();
            i = 9 + digits.len();
            frac = digits;
        }
    }
    if h > 23 || mi > 59 || sec > 61 {
        return None;
    }
    out.tz = rest[i..].trim().to_string();
    out.time = Some((h, mi, sec, frac));
    Some(out)
}

fn fmt_time(t: &(u32, u32, u32, String)) -> String {
    let mut s = format!("{:02}:{:02}:{:02}", t.0, t.1, t.2);
    let ms = t.3.chars().chain(std::iter::repeat('0')).take(3).collect::<String>();
    if !t.3.is_empty() && ms != "000" {
        s.push('.');
        s.push_str(&ms);
    }
    s
}

/// GDAL's text of a value (`OGRFeature::GetFieldAsString`): dates as `YYYY/MM/DD`, reals with
/// 16 significant digits (or `precision` decimals when the field declares a width), blobs in
/// hexadecimal, booleans as 0 or 1.
fn value_text(v: &FieldValue, def: &FieldDef) -> Option<String> {
    Some(match v {
        FieldValue::Null => return None,
        FieldValue::Integer(i) => i.to_string(),
        FieldValue::Boolean(b) => if *b { "1" } else { "0" }.to_string(),
        FieldValue::Float(f) if def.width > 0 => format!("{f:.p$}", p = def.precision),
        FieldValue::Float(f) if *f == 0.0 => "0".into(),
        FieldValue::Float(f) => crate::drivers::common::fmt_g(*f, 16),
        FieldValue::Text(s) | FieldValue::DateTime(s) => s.clone(),
        FieldValue::Date(s) => s.replace('-', "/"),
        FieldValue::Blob(b) => b.iter().map(|x| format!("{x:02X}")).collect(),
    })
}

/// Converts a value of a field of type `from` to type `to`, as GDAL's `OGRFeature::SetFrom`
/// does. `Err(())` when GDAL refuses the conversion: the feature is then left out.
fn convert(v: &FieldValue, def: &FieldDef, from: OgrType, to: OgrType, lossy: &mut Lossy) -> std::result::Result<FieldValue, ()> {
    use OgrFieldSubType as S;
    use OgrFieldType as T;
    if v.is_null() {
        return Ok(FieldValue::Null);
    }
    let temporal = matches!(from.field_type, T::Date | T::DateTime | T::Time);
    // Dates and blobs do not become numbers (nor dates blobs): GDAL drops the feature.
    if (temporal && (to.is_numeric() || to.field_type == T::Binary))
        || (from.field_type == T::Binary && !matches!(to.field_type, T::Binary | T::String))
    {
        return Err(());
    }
    let integer = |x: i64, lossy: &mut Lossy| -> FieldValue {
        match (to.field_type, to.subtype) {
            (T::Integer64, _) => FieldValue::Integer(x),
            (T::Integer, s) => {
                let mut y = x;
                if from.field_type != T::Integer {
                    if i32::try_from(y).is_err() {
                        lossy.overflow += 1;
                    }
                    y = y.clamp(i64::from(i32::MIN), i64::from(i32::MAX));
                }
                match s {
                    S::Boolean => {
                        if y != 0 && y != 1 && from.field_type != T::Integer {
                            lossy.boolean += 1;
                        }
                        FieldValue::Boolean(y != 0)
                    }
                    S::Int16 if from.field_type != T::Integer || from.subtype != S::None => {
                        FieldValue::Integer(y.clamp(i64::from(i16::MIN), i64::from(i16::MAX)))
                    }
                    _ => FieldValue::Integer(y),
                }
            }
            _ => FieldValue::Integer(x),
        }
    };
    // GDAL stores Float32 values as given (only printing them with single precision).
    let real = FieldValue::Float;
    Ok(match to.field_type {
        T::Integer | T::Integer64 => match v {
            FieldValue::Integer(i) => integer(*i, lossy),
            FieldValue::Boolean(b) => integer(i64::from(*b), lossy),
            FieldValue::Float(f) => {
                if f.fract() != 0.0
                    || (to.field_type == T::Integer && (*f < f64::from(i32::MIN) || *f > f64::from(i32::MAX)))
                    || !f.is_finite()
                {
                    lossy.lossy += 1;
                }
                let t = if f.is_nan() { 0 } else { f.trunc().clamp(i64::MIN as f64, i64::MAX as f64) as i64 };
                // Clamped to 32 bits first, without another warning.
                let t = if to.field_type == T::Integer { t.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) } else { t };
                let saved = lossy.overflow;
                let out = integer(t, lossy);
                lossy.overflow = saved;
                out
            }
            FieldValue::Text(s) | FieldValue::DateTime(s) | FieldValue::Date(s) => {
                if to.subtype == S::Boolean {
                    let l = s.trim().to_ascii_lowercase();
                    match l.as_str() {
                        "1" | "true" | "yes" | "on" => FieldValue::Boolean(true),
                        "0" | "false" | "no" | "off" => FieldValue::Boolean(false),
                        _ => {
                            lossy.boolean += 1;
                            FieldValue::Boolean(is_number(s) && parse_real_prefix(s).0 != 0.0)
                        }
                    }
                } else {
                    let (x, whole) = parse_int_prefix(s);
                    if !whole && to.field_type == T::Integer {
                        lossy.partial += 1;
                    }
                    let x = if to.field_type == T::Integer { x.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) } else { x };
                    let saved = lossy.overflow;
                    let out = integer(x, lossy);
                    lossy.overflow = saved;
                    out
                }
            }
            FieldValue::Blob(_) | FieldValue::Null => FieldValue::Null,
        },
        T::Real => match v {
            FieldValue::Integer(i) => real(*i as f64),
            FieldValue::Boolean(b) => real(f64::from(u8::from(*b))),
            FieldValue::Float(f) => real(*f),
            FieldValue::Text(s) | FieldValue::DateTime(s) | FieldValue::Date(s) => {
                let (x, whole) = parse_real_prefix(s);
                if !whole {
                    lossy.partial += 1;
                }
                real(x)
            }
            FieldValue::Blob(_) | FieldValue::Null => FieldValue::Null,
        },
        T::String => {
            let text = match (from.field_type, v) {
                (T::DateTime, FieldValue::DateTime(s) | FieldValue::Text(s)) => s.clone(),
                _ => value_text(v, def).unwrap_or_default(),
            };
            FieldValue::Text(text)
        }
        T::Binary => match v {
            FieldValue::Blob(b) => FieldValue::Blob(b.clone()),
            _ => FieldValue::Null,
        },
        T::Date | T::DateTime | T::Time => {
            let parts = match v {
                FieldValue::Date(s) | FieldValue::DateTime(s) | FieldValue::Text(s) if !from.is_numeric() => parse_datetime(s),
                _ => None,
            };
            let Some(p) = parts else { return Ok(FieldValue::Null) };
            match to.field_type {
                T::Date => match p.date {
                    Some((y, m, d)) => FieldValue::Date(format!("{y:04}-{m:02}-{d:02}")),
                    None => FieldValue::Null,
                },
                T::DateTime => match p.date {
                    Some((y, m, d)) => {
                        let t = p.time.clone().unwrap_or((0, 0, 0, String::new()));
                        FieldValue::DateTime(format!("{y:04}-{m:02}-{d:02}T{}{}", fmt_time(&t), p.tz))
                    }
                    None => FieldValue::Null,
                },
                _ => match (&p.time, p.date) {
                    (Some(t), _) => FieldValue::Text(fmt_time(t)),
                    (None, Some(_)) => FieldValue::Text("00:00:00".into()),
                    (None, None) => FieldValue::Null,
                },
            }
        }
    })
}

/// A value converted from type `from` to type `to` as GDAL converts it, without warnings; null
/// when GDAL refuses the conversion.
pub(crate) fn convert_value(v: &FieldValue, def: &FieldDef, from: OgrType, to: OgrType) -> FieldValue {
    if from == to {
        return v.clone();
    }
    convert(v, def, from, to, &mut Lossy::default()).unwrap_or(FieldValue::Null)
}

/// The OGR type two fields merged into one take, as GDAL's union layers merge them: reals over
/// integers, 64-bit over 32-bit integers, strings for other differences.
pub(crate) fn merge_types(a: OgrType, b: OgrType) -> OgrType {
    use OgrFieldType as T;
    if a.field_type == b.field_type {
        // The first field's subtype is kept.
        return a;
    }
    let int = |t: T| matches!(t, T::Integer | T::Integer64);
    OgrType::new(match (a.field_type, b.field_type) {
        (T::Real, t) | (t, T::Real) if int(t) => T::Real,
        (T::Integer64, T::Integer) | (T::Integer, T::Integer64) => T::Integer64,
        _ => T::String,
    })
}

/// Builds empty layers from a parsed schema or explicit fields with consistent naming rules.
pub fn created_layers(
    schema: Option<&OgrSchema>,
    name: Option<&str>,
    fields: &[String],
    geometry: Option<&str>,
    crs: Option<&SpatialRef>,
    fid: Option<&str>,
) -> Result<Vec<CreatedLayer>> {
    let mut out = match schema {
        Some(schema) => schema.layers.iter().map(layer_from_schema).collect::<Result<Vec<_>>>()?,
        None => {
            let mut layer = Layer::new(name.ok_or_else(|| Error::InvalidArgument("give the output layer name".into()))?);
            for field in fields {
                layer.add_field(parse_field_spec(field)?);
            }
            let (kind, z, m) = geometry.map(parse_geometry_type).transpose()?.unwrap_or((None, false, false));
            layer.geom_type = kind;
            if let Some(crs) = crs {
                crs.apply_to_layer(&mut layer);
            }
            vec![CreatedLayer { layer, spatial: geometry.is_some() || crs.is_some(), z, m, fid: fid.map(str::to_owned) }]
        }
    };
    if out.is_empty() {
        return Err(Error::InvalidArgument("the template has no layer".into()));
    }
    if let (Some(name), Some(_)) = (name, schema) {
        if out.len() != 1 {
            return Err(Error::InvalidArgument("output layer naming needs a single template layer: choose one with layers".into()));
        }
        out[0].layer.name = name.into();
    }
    Ok(out)
}

/// Converts fields across selected layers, validating the active layer and first target field.
/// Layers without a named field are retained unchanged after the first target validates.
pub fn set_field_type_in(layers: &[Layer], selector: &FieldSelector, to: OgrType, active: Option<&str>) -> Result<Vec<Layer>> {
    let first = match active {
        Some(name) => {
            layers.iter().find(|l| l.name == name).ok_or_else(|| Error::InvalidArgument(format!("cannot find layer '{name}'")))?
        }
        None => layers.first().ok_or_else(|| Error::InvalidArgument("no input vector layer".into()))?,
    };
    let has = |l: &Layer, name: &str| l.schema.fields().iter().any(|f| f.name.eq_ignore_ascii_case(name));
    if let FieldSelector::Name(name) = selector
        && !has(first, name)
    {
        return Err(Error::InvalidArgument(format!("cannot find field '{name}' in layer '{}'", first.name)));
    }
    layers
        .iter()
        .map(|l| {
            if active.is_some_and(|name| l.name != name) || matches!(selector, FieldSelector::Name(name) if !has(l, name)) {
                Ok(l.clone())
            } else {
                set_field_type(l, selector, to)
            }
        })
        .collect()
}

/// Which fields `set_field_type` changes: one by name, or all of a type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldSelector {
    /// The field with this name.
    Name(String),
    /// Every field of this OGR type (and subtype).
    Type(OgrType),
}

/// Changes the type of fields of a layer, as `gdal vector set-field-type` does: the field keeps
/// its name, width and precision, and its values are converted as GDAL converts them (strings
/// read as numbers by their numeric prefix, reals truncated and clamped to integers, 32-bit
/// overflows clamped, dates reformatted). Features whose value GDAL cannot convert (a date or
/// blob to a number, a date to a blob) are left out, with a warning. Lossy conversions are
/// reported as warnings.
pub fn set_field_type(layer: &Layer, selector: &FieldSelector, to: OgrType) -> Result<Layer> {
    let fields = layer.schema.fields();
    let targets: Vec<usize> = match selector {
        FieldSelector::Name(name) => {
            let i = layer
                .schema
                .field_index(name)
                .or_else(|| fields.iter().position(|f| f.name.eq_ignore_ascii_case(name)))
                .ok_or_else(|| Error::InvalidArgument(format!("cannot find field '{name}' in layer '{}'", layer.name)))?;
            vec![i]
        }
        FieldSelector::Type(t) => (0..fields.len()).filter(|&i| ogr_type(layer, i) == *t).collect(),
    };
    if targets.is_empty() {
        return Ok(layer.clone());
    }
    let from: Vec<OgrType> = targets.iter().map(|&i| ogr_type(layer, i)).collect();
    let mut out = layer.clone();
    out.schema = wbvector::Schema::new();
    for (i, f) in fields.iter().enumerate() {
        if targets.contains(&i) {
            let mut def = field_def(&f.name, to, f.width, f.precision);
            def.nullable = f.nullable;
            out.schema.add_field(def);
        } else {
            out.schema.add_field(f.clone());
        }
    }
    let mut lossy: Vec<Lossy> = targets.iter().map(|_| Lossy::default()).collect();
    let mut kept = Vec::with_capacity(layer.len());
    'features: for feature in &layer.features {
        let mut f = feature.clone();
        for (k, &i) in targets.iter().enumerate() {
            let Some(v) = feature.attributes.get(i) else { continue };
            match convert(v, &fields[i], from[k], to, &mut lossy[k]) {
                Ok(nv) => f.attributes[i] = nv,
                Err(()) => {
                    lossy[k].dropped += 1;
                    continue 'features;
                }
            }
        }
        kept.push(f);
    }
    out.features = kept;
    out.extent = None;
    for (k, &i) in targets.iter().enumerate() {
        let l = &lossy[k];
        let name = &fields[i].name;
        let lname = &layer.name;
        if l.dropped > 0 {
            crate::warnings::warn(format!(
                "{lname}.{name}: {} feature(s) left out, their {} values cannot be converted to {to}",
                l.dropped, from[k]
            ));
        }
        if l.partial > 0 {
            crate::warnings::warn(format!("{lname}.{name}: {} value(s) parsed incompletely to {to}", l.partial));
        }
        if l.lossy > 0 {
            crate::warnings::warn(format!("{lname}.{name}: lossy conversion of {} real value(s) to {to}", l.lossy));
        }
        if l.overflow > 0 {
            crate::warnings::warn(format!("{lname}.{name}: integer overflow of {} value(s) set as 32-bit integers", l.overflow));
        }
        if l.boolean > 0 {
            crate::warnings::warn(format!("{lname}.{name}: {} value(s) other than 0 or 1 taken as booleans", l.boolean));
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------------------------------------
// OGR_SCHEMA
// ------------------------------------------------------------------------------------------------

/// An `OGR_SCHEMA` document: GDAL's description of the layers of a dataset
/// (`ogr/data/ogr_fields_override.schema.json`), as `gdal vector export-schema` writes it and
/// `gdal vector create --schema` reads it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OgrSchema {
    /// The layers.
    pub layers: Vec<SchemaLayer>,
}

/// A layer of an `OGR_SCHEMA` document.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SchemaLayer {
    /// The layer name.
    pub name: String,
    /// `Full` or `Patch`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_type: Option<String>,
    /// The geometry fields (none for a layer without geometries).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub geometry_fields: Vec<SchemaGeometryField>,
    /// The name of the feature id column.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fid_column_name: Option<String>,
    /// The attribute fields.
    #[serde(default)]
    pub fields: Vec<SchemaField>,
}

/// An attribute field of an `OGR_SCHEMA` layer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SchemaField {
    /// The field name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The type: `Integer`, `Integer64`, `Real`, `String`, `Date`, `Time`, `DateTime`, `Binary`.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub field_type: Option<String>,
    /// The subtype: `Boolean`, `Int16`, `Float32`, `JSON`, `UUID`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sub_type: Option<String>,
    /// The width.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width: Option<usize>,
    /// The precision.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub precision: Option<usize>,
    /// Whether the field may be null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nullable: Option<bool>,
    /// Whether values are unique.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unique_constraint: Option<bool>,
    /// The default value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_value: Option<String>,
    /// An alternative name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// A comment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    /// A field domain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain_name: Option<String>,
    /// The time zone of date-time fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timezone: Option<String>,
    /// For patches: the type of the fields to change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src_type: Option<String>,
    /// For patches: the subtype of the fields to change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub src_sub_type: Option<String>,
    /// For patches: the field's new name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_name: Option<String>,
}

/// A geometry field of an `OGR_SCHEMA` layer.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaGeometryField {
    /// The geometry column name (empty for formats without one).
    pub name: String,
    /// The geometry type, such as `MultiPolygon` or `PointZ` (`Geometry` for any).
    #[serde(rename = "type")]
    pub geometry_type: String,
    /// Whether geometries may be null.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nullable: Option<bool>,
    /// The CRS: `{"authid": "EPSG:4326"}`, or `{"wkt": ...}`; `null` for none.
    #[serde(default)]
    pub coordinate_system: Option<serde_json::Value>,
    /// Other members, kept as given.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl OgrSchema {
    /// Parses an `OGR_SCHEMA` document.
    pub fn from_json(text: &str) -> Result<Self> {
        let text = text.trim_start_matches('\u{feff}');
        serde_json::from_str(text).map_err(|e| Error::InvalidArgument(format!("invalid OGR_SCHEMA: {e}")))
    }

    /// The document, as `gdal vector export-schema` prints it (indented by two spaces).
    pub fn to_json(&self) -> String {
        let mut buf = Vec::new();
        let fmt = serde_json::ser::PrettyFormatter::with_indent(b"  ");
        let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
        // Serializing these types to memory does not fail.
        let _ = self.serialize(&mut ser);
        String::from_utf8(buf).unwrap_or_default()
    }
}

/// GDAL's camel-case name of a geometry type with its dimensions (`MultiPolygon`, `PointZ`,
/// `Geometry` for any type).
fn geometry_type_name(t: Option<GeometryType>, z: bool, m: bool) -> String {
    let base = t.map_or("Geometry", GeometryType::as_str);
    format!("{base}{}{}", if z { "Z" } else { "" }, if m { "M" } else { "" })
}

/// A geometry type name, as GDAL's `--geometry-type` options give it (case-insensitive, with
/// an optional `Z`, `M` or `ZM` suffix, with or without a space): the type (`None` for
/// `GEOMETRY`/`UNKNOWN`), Z and M.
pub fn parse_geometry_type(s: &str) -> Result<(Option<GeometryType>, bool, bool)> {
    let u = s.trim().to_ascii_uppercase().replace(' ', "");
    let (base, z, m) = if let Some(b) = u.strip_suffix("ZM") {
        (b, true, true)
    } else if let Some(b) = u.strip_suffix('Z') {
        (b, true, false)
    } else if let Some(b) = u.strip_suffix('M').filter(|b| !b.is_empty()) {
        (b, false, true)
    } else {
        (u.as_str(), false, false)
    };
    let t = match base {
        "GEOMETRY" | "UNKNOWN" => None,
        "POINT" => Some(GeometryType::Point),
        "LINESTRING" => Some(GeometryType::LineString),
        "POLYGON" => Some(GeometryType::Polygon),
        "MULTIPOINT" => Some(GeometryType::MultiPoint),
        "MULTILINESTRING" => Some(GeometryType::MultiLineString),
        "MULTIPOLYGON" => Some(GeometryType::MultiPolygon),
        "GEOMETRYCOLLECTION" => Some(GeometryType::GeometryCollection),
        "CURVE" | "CIRCULARSTRING" | "COMPOUNDCURVE" | "SURFACE" | "CURVEPOLYGON" | "MULTICURVE" | "MULTISURFACE" | "POLYHEDRALSURFACE"
        | "TIN" | "TRIANGLE" => {
            return Err(Error::Unsupported(format!("geometry type '{s}': curve and surface types are not supported")));
        }
        _ => return Err(Error::InvalidArgument(format!("unknown geometry type '{s}'"))),
    };
    Ok((t, z, m))
}

/// The geometry column and FID column names GDAL reports for layers of a driver.
fn driver_columns(driver: Option<crate::formats::VectorDriver>) -> (&'static str, Option<&'static str>) {
    use crate::formats::VectorDriver as D;
    match driver {
        Some(D::GPKG) => ("geom", Some("fid")),
        Some(D::Parquet) => ("geometry", None),
        Some(D::MapInfo) => ("", None),
        _ => ("", None),
    }
}

/// The `OGR_SCHEMA` of layers read by `driver` (which tells the geometry and FID column names
/// GDAL reports): GDAL's field types (see [`ogr_type`]), widths, precisions and nullability, the
/// geometry type and CRS (`authid` when the CRS has an authority code, otherwise its WKT).
pub fn export_schema(layers: &[Layer], driver: Option<crate::formats::VectorDriver>) -> OgrSchema {
    let (geom_name, fid) = driver_columns(driver);
    let layers = layers
        .iter()
        .map(|l| {
            let fields = (0..l.schema.len())
                .map(|i| {
                    let f = &l.schema.fields()[i];
                    let t = ogr_type(l, i);
                    let has_width =
                        matches!(t.field_type, OgrFieldType::Integer | OgrFieldType::Integer64 | OgrFieldType::Real | OgrFieldType::String);
                    SchemaField {
                        name: Some(f.name.clone()),
                        field_type: Some(t.field_type.name().into()),
                        sub_type: (t.subtype != OgrFieldSubType::None).then(|| t.subtype.name().into()),
                        width: (has_width && f.width > 0).then_some(f.width),
                        precision: (t.field_type == OgrFieldType::Real && f.precision > 0).then_some(f.precision),
                        nullable: Some(f.nullable),
                        unique_constraint: Some(false),
                        ..Default::default()
                    }
                })
                .collect();
            let has_geometry =
                l.geom_type.is_some() || l.features.iter().any(|f| f.geometry.is_some()) || !SpatialRef::from_layer(l).is_unknown();
            let geometry_fields = if has_geometry {
                let declared = l.geom_type.or_else(|| crate::drivers::vector::uniform_type(l));
                let (z, m) = l.features.iter().filter_map(|f| f.geometry.as_ref()).take(1000).fold((false, false), |(z, m), g| {
                    let (gz, gm) = crate::vector::geometry_dims(g);
                    (z || gz, m || gm)
                });
                vec![SchemaGeometryField {
                    name: geom_name.into(),
                    geometry_type: geometry_type_name(declared, z, m),
                    nullable: Some(true),
                    coordinate_system: Some(crs_json(&SpatialRef::from_layer(l))),
                    extra: Default::default(),
                }]
            } else {
                Vec::new()
            };
            SchemaLayer {
                name: l.name.clone(),
                schema_type: Some("Full".into()),
                geometry_fields,
                fid_column_name: fid.map(str::to_string),
                fields,
            }
        })
        .collect();
    OgrSchema { layers }
}

/// The `OGR_SCHEMA` of a dataset (the layers named by `layers`; all when empty), as
/// `gdal vector export-schema` exports it. Local GeoPackages are described from their tables'
/// declarations (exact column types, `NOT NULL` and `UNIQUE` constraints and defaults, the
/// geometry column and its Z/M flags); other datasets are read and described by
/// [`export_schema`].
pub fn export_schema_path(path: &str, layers: &[String]) -> Result<OgrSchema> {
    #[cfg(feature = "sqlite")]
    if crate::formats::VectorDriver::for_extension(path) == Some(crate::formats::VectorDriver::GPKG)
        && crate::vfs::Location::parse(path).is_local()
        && std::path::Path::new(path).is_file()
    {
        let declared = crate::drivers::vector::gpkg_read::declared_layers(path, layers)?;
        return Ok(OgrSchema { layers: declared.iter().map(declared_schema).collect() });
    }
    let options = crate::VectorOpenOptions { layers: layers.to_vec(), ..Default::default() };
    let ds = crate::open_vector(path, &options)?;
    Ok(export_schema(&ds.layers, Some(ds.driver)))
}

/// The OGR type GDAL's GeoPackage driver gives a declared column type.
#[cfg(feature = "sqlite")]
fn declared_type(decl: &str) -> (OgrType, usize) {
    use OgrFieldSubType as S;
    use OgrFieldType as T;
    let upper = decl.trim().to_ascii_uppercase();
    let base = upper.split('(').next().unwrap_or_default().trim().to_string();
    let width = upper.split('(').nth(1).and_then(|w| w.trim_end_matches(')').trim().parse().ok()).unwrap_or(0);
    let (t, s) = match base.as_str() {
        "MEDIUMINT" => (T::Integer, S::None),
        "INTEGER" | "INT" | "BIGINT" | "INT8" => (T::Integer64, S::None),
        "SMALLINT" | "TINYINT" | "INT2" => (T::Integer, S::Int16),
        "BOOLEAN" => (T::Integer, S::Boolean),
        "FLOAT" => (T::Real, S::Float32),
        "REAL" | "DOUBLE" | "NUMERIC" | "DECIMAL" => (T::Real, S::None),
        "DATE" => (T::Date, S::None),
        "DATETIME" | "TIMESTAMP" => (T::DateTime, S::None),
        "BLOB" => (T::Binary, S::None),
        _ => (T::String, S::None),
    };
    (OgrType { field_type: t, subtype: s }, if t == T::String { width } else { 0 })
}

#[cfg(feature = "sqlite")]
fn declared_schema(d: &crate::drivers::vector::gpkg_read::DeclaredLayer) -> SchemaLayer {
    let fields = d
        .columns
        .iter()
        .map(|c| {
            let (t, width) = declared_type(&c.decl);
            SchemaField {
                name: Some(c.name.clone()),
                field_type: Some(t.field_type.name().into()),
                sub_type: (t.subtype != OgrFieldSubType::None).then(|| t.subtype.name().into()),
                width: (width > 0).then_some(width),
                nullable: Some(!c.not_null),
                unique_constraint: Some(c.unique),
                default_value: c.default.clone(),
                ..Default::default()
            }
        })
        .collect();
    let geometry_fields = d
        .geometry
        .iter()
        .map(|(name, type_name, z, m, not_null)| {
            let t = parse_geometry_type(type_name).map(|(t, ..)| t).unwrap_or(None);
            SchemaGeometryField {
                name: name.clone(),
                geometry_type: geometry_type_name(t, matches!(z, 1 | 2), matches!(m, 1 | 2)),
                nullable: Some(!not_null),
                coordinate_system: Some(crs_json(&SpatialRef::from_layer(&d.layer))),
                extra: Default::default(),
            }
        })
        .collect();
    SchemaLayer { name: d.layer.name.clone(), schema_type: Some("Full".into()), geometry_fields, fid_column_name: d.fid.clone(), fields }
}

/// The `coordinateSystem` member of a geometry field.
pub(crate) fn crs_json(s: &SpatialRef) -> serde_json::Value {
    if s.is_unknown() {
        return serde_json::Value::Null;
    }
    if let Some(code) = s.authority_code() {
        return serde_json::json!({ "authid": code });
    }
    let mut o = serde_json::Map::new();
    if let Some(w) = s.to_wkt() {
        o.insert("wkt".into(), w.into());
    }
    o.insert("dataAxisToSRSAxisMapping".into(), serde_json::json!([1, 2]));
    serde_json::Value::Object(o)
}

/// The CRS of a `coordinateSystem` member: `authid`, `wkt` or `projjson`.
fn crs_from_json(v: &serde_json::Value) -> Result<Option<SpatialRef>> {
    match v {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(s) => SpatialRef::parse(s).map(Some),
        serde_json::Value::Object(o) => {
            if let Some(a) = o.get("authid").and_then(|a| a.as_str()) {
                return SpatialRef::parse(a).map(Some);
            }
            if let Some(w) = o.get("wkt").and_then(|a| a.as_str()) {
                return SpatialRef::parse(w).map(Some);
            }
            if let Some(p) = o.get("projjson") {
                return SpatialRef::from_projjson(&p.to_string()).map(Some);
            }
            Ok(None)
        }
        _ => Err(Error::InvalidArgument("coordinateSystem: expected an object".into())),
    }
}

/// An empty layer described by an `OGR_SCHEMA` layer: its fields (the types mapped as
/// [`field_def`] maps them), geometry type and CRS. Also returns whether the geometry type has
/// Z and M (which an empty terrio layer cannot hold), and the FID column name.
pub fn layer_from_schema(s: &SchemaLayer) -> Result<CreatedLayer> {
    let mut layer = Layer::new(s.name.clone());
    // GDAL's schema/template creation traverses field definitions by name.
    // Explicit --field construction retains the caller's order.
    let mut fields: Vec<_> = s.fields.iter().collect();
    fields.sort_by(|a, b| a.name.cmp(&b.name));
    for f in fields {
        let name = f.name.as_deref().ok_or_else(|| Error::InvalidArgument(format!("layer '{}': a field has no name", s.name)))?;
        let t = OgrType::from_names(f.field_type.as_deref(), f.sub_type.as_deref())?
            .ok_or_else(|| Error::InvalidArgument(format!("field '{name}' has no type")))?;
        let mut def = field_def(name, t, f.width.unwrap_or(0), f.precision.unwrap_or(0));
        def.nullable = f.nullable.unwrap_or(true);
        layer.add_field(def);
    }
    let mut z = false;
    let mut m = false;
    match s.geometry_fields.as_slice() {
        [] => {}
        [g, rest @ ..] => {
            if !rest.is_empty() {
                crate::warnings::warn(format!(
                    "layer '{}': only the first of {} geometry fields is created",
                    s.name,
                    s.geometry_fields.len()
                ));
            }
            let (t, gz, gm) = parse_geometry_type(&g.geometry_type)?;
            layer.geom_type = t;
            z = gz;
            m = gm;
            if let Some(cs) = &g.coordinate_system
                && let Some(srs) = crs_from_json(cs)?
            {
                srs.apply_to_layer(&mut layer);
            }
        }
    }
    let spatial = !s.geometry_fields.is_empty();
    Ok(CreatedLayer { layer, spatial, z, m, fid: s.fid_column_name.clone() })
}

/// A layer made from a schema, with what an empty layer cannot hold.
#[derive(Debug, Clone)]
pub struct CreatedLayer {
    /// The empty layer.
    pub layer: Layer,
    /// Whether it has a geometry field.
    pub spatial: bool,
    /// Whether its geometry type has Z.
    pub z: bool,
    /// Whether its geometry type has M.
    pub m: bool,
    /// The FID column name.
    pub fid: Option<String>,
}

/// Options of [`create`].
#[derive(Debug, Clone, Default)]
pub struct CreateOptions {
    /// How to write the dataset (its driver: by default, from the path's extension).
    pub write: crate::VectorWriteOptions,
    /// Add the layers to an existing dataset (a GeoPackage), rather than creating it.
    pub update: bool,
    /// Replace an existing dataset.
    pub overwrite: bool,
}

/// Creates a dataset holding the (empty) layers, as `gdal vector create` does, or adds them to
/// an existing GeoPackage with `update`. A layer's FID column name is used for new
/// GeoPackages. Z and M flags of empty layers are not recorded (terrio's writers take them
/// from the features), with a warning.
pub fn create(layers: &[CreatedLayer], path: &str, o: &CreateOptions) -> Result<()> {
    use crate::formats::VectorDriver;
    if layers.is_empty() {
        return Err(Error::InvalidArgument("no layer to create".into()));
    }
    let exists = std::path::Path::new(path).exists();
    if exists && !o.update && !o.overwrite {
        return Err(Error::InvalidArgument(format!("'{path}' already exists: give overwrite (to replace it) or update (to add layers)")));
    }
    for c in layers {
        if c.z || c.m {
            crate::warnings::warn(format!(
                "layer '{}': the Z/M dimensions of its geometry type are not recorded in an empty layer",
                c.layer.name
            ));
        }
    }
    let plain: Vec<Layer> = layers.iter().map(|c| c.layer.clone()).collect();
    let driver = o.write.driver.or_else(|| VectorDriver::for_extension(path));
    if exists && o.update {
        let existing = crate::list_layers(path)?;
        if let Some(l) = plain.iter().find(|l| existing.iter().any(|e| e.eq_ignore_ascii_case(&l.name))) {
            return Err(Error::InvalidArgument(format!("'{path}' already has a layer named '{}'", l.name)));
        }
        if driver != Some(VectorDriver::GPKG) {
            return Err(Error::Unsupported("layers can only be added to an existing GeoPackage".into()));
        }
        if layers.iter().any(|c| c.fid.as_deref().is_some_and(|fid| !fid.eq_ignore_ascii_case("fid"))) {
            crate::warnings::warn("custom FID names are not applied when adding layers to an existing GeoPackage".into());
        }
        return crate::ops::vector_filter::write_vector_with_mode(&plain, path, &o.write, crate::ops::vector_filter::WriteMode::Append);
    }
    #[cfg(feature = "sqlite")]
    if driver == Some(VectorDriver::GPKG) && layers.iter().any(|c| c.fid.is_some()) {
        return Ok(std::fs::write(path, create_bytes(layers, path, &o.write)?)?);
    }
    let fid = layers.iter().find_map(|c| c.fid.clone()).filter(|f| !f.eq_ignore_ascii_case("fid"));
    match (driver, fid) {
        #[cfg(feature = "sqlite")]
        (Some(VectorDriver::GPKG), Some(fid)) => crate::drivers::vector::geopackage::write_with_fid(&plain, path, &fid),
        (_, Some(fid)) => {
            crate::warnings::warn(format!("the FID column name '{fid}' is only used for GeoPackage outputs"));
            crate::write_vector(&plain, path, &o.write)
        }
        _ => crate::write_vector(&plain, path, &o.write),
    }
}

/// Creates an empty vector dataset in memory, retaining custom per-layer GeoPackage FID names.
/// Other formats use the ordinary in-memory writer and its documented capabilities.
pub fn create_bytes(layers: &[CreatedLayer], name: &str, write: &crate::VectorWriteOptions) -> Result<Vec<u8>> {
    if layers.is_empty() {
        return Err(Error::InvalidArgument("no layer to create".into()));
    }
    let driver = write.driver.or_else(|| crate::formats::VectorDriver::for_extension(name));
    #[cfg(feature = "sqlite")]
    if driver == Some(crate::formats::VectorDriver::GPKG) {
        let mut builder = crate::drivers::vector::geopackage::Builder::new();
        for created in layers {
            builder.set_fid_column(created.fid.as_deref().unwrap_or("fid"));
            builder.add_layer(&created.layer)?;
        }
        return builder.finish();
    }
    if layers.iter().any(|l| l.fid.as_deref().is_some_and(|f| !f.eq_ignore_ascii_case("fid"))) {
        return Err(Error::Unsupported("custom FID names require GeoPackage output with sqlite enabled".into()));
    }
    let _ = driver;
    let plain: Vec<Layer> = layers.iter().map(|l| l.layer.clone()).collect();
    crate::memory::write_vector_bytes(&plain, name, write)
}

/// A field given as `NAME:TYPE[(WIDTH[,PRECISION])]`, as `gdal vector create --field` reads
/// it: the type is one of `INTEGER`, `INTEGER64`, `REAL`, `STRING`, `DATE`, `TIME`,
/// `DATETIME`, `BINARY` (or a subtype), case-insensitive.
pub fn parse_field_spec(spec: &str) -> Result<FieldDef> {
    let (name, rest) = spec
        .rsplit_once(':')
        .filter(|(n, _)| !n.is_empty())
        .ok_or_else(|| Error::InvalidArgument(format!("field '{spec}': expected NAME:TYPE[(WIDTH[,PRECISION])]")))?;
    let (t, args) = match rest.find('(') {
        Some(i) => {
            let inner =
                rest[i + 1..].strip_suffix(')').ok_or_else(|| Error::InvalidArgument(format!("field '{spec}': unbalanced parenthesis")))?;
            (&rest[..i], Some(inner))
        }
        None => (rest, None),
    };
    let t = OgrType::parse(t)?;
    let (mut width, mut precision) = (0, 0);
    if let Some(a) = args {
        let mut parts = a.split(',').map(str::trim);
        let num = |p: Option<&str>| -> Result<usize> {
            p.filter(|s| !s.is_empty())
                .map(|s| {
                    s.parse::<usize>().map_err(|_| Error::InvalidArgument(format!("field '{spec}': invalid width or precision '{s}'")))
                })
                .transpose()
                .map(|v| v.unwrap_or(0))
        };
        width = num(parts.next())?;
        precision = num(parts.next())?;
        if !matches!(t.field_type, OgrFieldType::Integer | OgrFieldType::Integer64 | OgrFieldType::Real | OgrFieldType::String) {
            return Err(Error::InvalidArgument(format!(
                "field '{spec}': width and precision apply to INTEGER, INTEGER64, REAL and STRING fields"
            )));
        }
    }
    Ok(field_def(name, t, width, precision))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbvector::{Feature, Geometry};

    fn layer(defs: &[(&str, FieldType)], rows: Vec<Vec<FieldValue>>) -> Layer {
        let mut l = Layer::new("t");
        for (n, t) in defs {
            l.add_field(FieldDef::new(*n, *t));
        }
        for (i, r) in rows.into_iter().enumerate() {
            l.push(Feature { fid: i as u64 + 1, geometry: Some(Geometry::point(i as f64, 0.0)), attributes: r });
        }
        l
    }

    fn column(l: &Layer, i: usize) -> Vec<FieldValue> {
        l.features.iter().map(|f| f.attributes[i].clone()).collect()
    }

    fn text(v: &[&str]) -> Vec<Vec<FieldValue>> {
        v.iter().map(|s| vec![FieldValue::Text((*s).into())]).collect()
    }

    #[test]
    fn parses_types_and_subtypes() {
        assert_eq!(OgrType::parse("integer64").unwrap(), OgrType::new(OgrFieldType::Integer64));
        let b = OgrType::parse("Boolean").unwrap();
        assert_eq!((b.field_type, b.subtype), (OgrFieldType::Integer, OgrFieldSubType::Boolean));
        assert_eq!(OgrType::parse("JSON").unwrap().to_string(), "String(JSON)");
        assert!(matches!(OgrType::parse("IntegerList"), Err(Error::Unsupported(_))));
        assert!(OgrType::parse("Number").is_err());
    }

    #[test]
    fn strings_to_numbers_as_gdal() {
        // gdal vector set-field-type of a String field (GDAL 3.12.2).
        let l = layer(&[("s", FieldType::Text)], text(&["12", "abc", "12abc", "3.9", "2021-05-06", "", "1e3"]));
        let i = set_field_type(&l, &FieldSelector::Name("s".into()), OgrType::parse("Integer").unwrap()).unwrap();
        assert_eq!(column(&i, 0), [12, 0, 12, 3, 2021, 0, 1].map(FieldValue::Integer));
        let r = set_field_type(&l, &FieldSelector::Name("s".into()), OgrType::parse("Real").unwrap()).unwrap();
        assert_eq!(column(&r, 0), [12.0, 0.0, 12.0, 3.9, 2021.0, 0.0, 1000.0].map(FieldValue::Float));
        let b = set_field_type(&l, &FieldSelector::Name("s".into()), OgrType::parse("Boolean").unwrap()).unwrap();
        assert_eq!(column(&b, 0), [true, false, false, true, false, false, true].map(FieldValue::Boolean));
        let d = set_field_type(&l, &FieldSelector::Name("s".into()), OgrType::parse("Date").unwrap()).unwrap();
        assert_eq!(column(&d, 0)[4], FieldValue::Date("2021-05-06".into()));
        assert_eq!(column(&d, 0)[0], FieldValue::Null);
    }

    #[test]
    fn reals_truncate_and_clamp() {
        let rows = [1.7, -1.7, 3e10, 1e300].iter().map(|v| vec![FieldValue::Float(*v)]).collect();
        let l = layer(&[("r", FieldType::Float)], rows);
        let i = set_field_type(&l, &FieldSelector::Name("r".into()), OgrType::parse("Integer").unwrap()).unwrap();
        assert_eq!(column(&i, 0), [1, -1, 2147483647, 2147483647].map(FieldValue::Integer));
        let i = set_field_type(&l, &FieldSelector::Name("r".into()), OgrType::parse("Integer64").unwrap()).unwrap();
        assert_eq!(column(&i, 0), [1, -1, 30000000000, i64::MAX].map(FieldValue::Integer));
        assert!(i.schema.fields()[0].width >= 10, "Integer64 fields declare a 64-bit width");
        let i = set_field_type(&l, &FieldSelector::Name("r".into()), OgrType::parse("Int16").unwrap()).unwrap();
        assert_eq!(column(&i, 0), [1, -1, 32767, 32767].map(FieldValue::Integer));
        let s = set_field_type(&l, &FieldSelector::Name("r".into()), OgrType::parse("String").unwrap()).unwrap();
        assert_eq!(column(&s, 0), ["1.7", "-1.7", "30000000000", "1e+300"].map(|s| FieldValue::Text(s.into())));
    }

    #[test]
    fn dates_to_numbers_drop_features() {
        let rows = vec![vec![FieldValue::Date("2020-01-02".into())], vec![FieldValue::Null], vec![FieldValue::Date("1999-12-31".into())]];
        let l = layer(&[("d", FieldType::Date)], rows);
        let i = set_field_type(&l, &FieldSelector::Name("d".into()), OgrType::parse("Integer").unwrap()).unwrap();
        assert_eq!(i.len(), 1);
        let s = set_field_type(&l, &FieldSelector::Name("d".into()), OgrType::parse("String").unwrap()).unwrap();
        assert_eq!(column(&s, 0)[0], FieldValue::Text("2020/01/02".into()));
        let dt = set_field_type(&l, &FieldSelector::Name("d".into()), OgrType::parse("DateTime").unwrap()).unwrap();
        assert_eq!(column(&dt, 0)[2], FieldValue::DateTime("1999-12-31T00:00:00".into()));
        let t = set_field_type(&l, &FieldSelector::Name("d".into()), OgrType::parse("Time").unwrap()).unwrap();
        assert_eq!(column(&t, 0)[0], FieldValue::Text("00:00:00".into()));
    }

    #[test]
    fn selects_fields_by_type() {
        let l = layer(
            &[("a", FieldType::Integer), ("b", FieldType::Integer), ("c", FieldType::Text)],
            vec![vec![FieldValue::Integer(1), FieldValue::Integer(5_000_000_000), FieldValue::Text("x".into())]],
        );
        let r = set_field_type(&l, &FieldSelector::Type(OgrType::parse("Integer").unwrap()), OgrType::parse("Real").unwrap()).unwrap();
        assert_eq!(
            r.schema.fields().iter().map(|f| f.field_type).collect::<Vec<_>>(),
            [FieldType::Float, FieldType::Integer, FieldType::Text]
        );
        assert!(set_field_type(&l, &FieldSelector::Name("missing".into()), OgrType::parse("Real").unwrap()).is_err());
    }

    #[test]
    fn schema_round_trips() {
        let mut l = layer(&[("name", FieldType::Text), ("n", FieldType::Integer)], vec![]);
        l.schema = wbvector::Schema::new();
        l.add_field(FieldDef::new("name", FieldType::Text).width(20));
        l.add_field(field_def("n", OgrType::parse("Integer64").unwrap(), 0, 0));
        l.add_field(field_def("ok", OgrType::parse("Boolean").unwrap(), 0, 0));
        l.geom_type = Some(GeometryType::Point);
        l.set_crs_epsg(Some(4326));
        let schema = export_schema(&[l], Some(crate::formats::VectorDriver::GPKG));
        let json = schema.to_json();
        assert!(json.contains("\"schemaType\": \"Full\""), "{json}");
        assert!(json.contains("\"authid\": \"EPSG:4326\""), "{json}");
        let parsed = OgrSchema::from_json(&json).unwrap();
        assert_eq!(parsed, schema);
        let created = layer_from_schema(&parsed.layers[0]).unwrap();
        assert_eq!(created.layer.geom_type, Some(GeometryType::Point));
        assert_eq!(created.layer.crs_epsg(), Some(4326));
        assert_eq!(created.layer.schema.fields().iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["n", "name", "ok"]);
        assert_eq!(ogr_type(&created.layer, 0).field_type, OgrFieldType::Integer64);
        assert_eq!(created.layer.schema.fields()[2].field_type, FieldType::Boolean);
        assert_eq!(created.fid.as_deref(), Some("fid"));
    }

    #[test]
    fn parses_field_specs_and_geometry_types() {
        // 32-bit integers stay narrower than 10 digits, which marks 64-bit integers.
        let f = parse_field_spec("population:INTEGER(10,5)").unwrap();
        assert_eq!((f.field_type, f.width), (FieldType::Integer, 0));
        let f = parse_field_spec("population:INTEGER64(12)").unwrap();
        assert_eq!((f.field_type, f.width), (FieldType::Integer, 12));
        let f = parse_field_spec("name:string(255)").unwrap();
        assert_eq!((f.field_type, f.width), (FieldType::Text, 255));
        assert!(parse_field_spec("nameonly").is_err());
        assert!(parse_field_spec("d:DATE(10)").is_err());
        assert_eq!(parse_geometry_type("point z").unwrap(), (Some(GeometryType::Point), true, false));
        assert_eq!(parse_geometry_type("MULTIPOLYGONZM").unwrap(), (Some(GeometryType::MultiPolygon), true, true));
        assert_eq!(parse_geometry_type("geometry").unwrap(), (None, false, false));
        assert!(parse_geometry_type("TIN").is_err());
    }
    #[cfg(feature = "sqlite")]
    #[test]
    fn byte_creation_preserves_each_layers_custom_fid() {
        let layers: Vec<_> = [("a", "object_id"), ("b", "record_id")]
            .into_iter()
            .map(|(name, fid)| CreatedLayer { layer: Layer::new(name), spatial: false, z: false, m: false, fid: Some(fid.into()) })
            .collect();
        let bytes = create_bytes(&layers, "created.gpkg", &Default::default()).unwrap();
        let db = crate::drivers::sqlite::Db::new(bytes).unwrap();
        assert!(db.table_sql("a").unwrap().contains("object_id"));
        assert!(db.table_sql("b").unwrap().contains("record_id"));
        assert!(!db.table_sql("b").unwrap().contains("object_id"));
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("created.gpkg");
        create(&layers, path.to_str().unwrap(), &Default::default()).unwrap();
        let db = crate::drivers::sqlite::Db::new(std::fs::read(path).unwrap()).unwrap();
        assert!(db.table_sql("a").unwrap().contains("object_id"));
        assert!(db.table_sql("b").unwrap().contains("record_id"));
    }
}
