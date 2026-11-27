// SPDX-FileCopyrightText: Copyright (c) 2002, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2008-2013, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2002, Frank Warmerdam <warmerdam@pobox.com>
// SPDX-FileCopyrightText: Copyright (c) 2009-2013, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2005, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2010-2014, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 1998, Daniel Morissette
// SPDX-FileCopyrightText: Copyright (c) 2006, Andrey Kiselev
// SPDX-FileCopyrightText: Copyright (c) 2008-2012, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.13.2:
//     ogr/ogrsf_frmts/gmlutils/gmlpropertydefn.cpp (GMLPropertyDefn::AnalysePropertyValue)
//     ogr/ogrsf_frmts/gml/ogrgmllayer.cpp (OGRGMLLayer::GetNextFeature, setting list fields)
//     ogr/ogrsf_frmts/gmlutils/parsexsd.cpp (GetSimpleTypeProperties, GetListTypeFromSingleType,
//       the element types of GMLParseFeatureType)
//     port/cpl_string.cpp (CPLGetValueType)
//     port/cpl_strtod.cpp (CPLStrtodDelim)

//! The list fields of GML features, typed and read as GDAL's GML reader types and reads them:
//! integer, 64-bit integer, real, string and boolean lists, which terrio holds as JSON arrays
//! ([`wbvector::FieldType::Json`] text) and records the OGR types of
//! ([`crate::VectorDataset::ogr_type`]).
//!
//! A property is a list when the application schema declares it with a `maxOccurs` other than
//! `1` and a type GDAL lists ([`schema_list_type`]), or, without a schema, when GDAL's inference
//! makes it one ([`Inference`]): a property repeated in a feature becomes a list of the type of its
//! values (`GMLPropertyDefn::AnalysePropertyValue`). The values of a list are converted as C
//! converts them (`atoi`, `atoll`, `CPLAtof`; booleans `true` and `1`), each element of the
//! property one value ([`list_value`]).
//!
//! GDAL reads the elements of a property as its handler collects them ([`Sub`]): an empty element
//! is left out, an element with `xsi:nil="true"` is a null marker (null when it comes first, the
//! marker's text otherwise), and other text loses its leading spaces.

use super::super::xml::Element;
use crate::ops::vector_schema::{OgrFieldSubType, OgrFieldType, OgrType};
use wbvector::FieldValue;

/// The text GDAL keeps for an element with `xsi:nil="true"` (`OGR_GML_NULL`).
const NULL_MARKER: &str = "___OGR_GML_NULL___";

/// An element of a property, as GDAL's handler collects it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sub<'a> {
    /// `xsi:nil="true"`.
    Nil,
    /// The text of the element, which is not empty, with its leading spaces.
    Text(&'a str),
}

impl<'a> Sub<'a> {
    /// The element of a property element: `None` for an empty element, which GDAL leaves out.
    pub(crate) fn of(property: &'a Element) -> Option<Self> {
        let nil = property.attrs.iter().find(|(k, _)| k == "xsi:nil");
        match nil {
            Some((_, v)) if v.eq_ignore_ascii_case("true") => Some(Sub::Nil),
            _ if property.text.is_empty() => None,
            _ => Some(Sub::Text(&property.text)),
        }
    }

    /// The value GDAL holds: the text without its leading spaces, or the null marker.
    fn value(self) -> &'a str {
        match self {
            Sub::Nil => NULL_MARKER,
            Sub::Text(t) => t.trim_start_matches([' ', '\n', '\r', '\t']),
        }
    }
}

/// The type of a GML property as GDAL's reader holds it (`GMLPropertyType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum State {
    Untyped,
    String,
    Integer,
    Real,
    Integer64,
    Boolean,
    Date,
    Time,
    DateTime,
    StringList,
    IntegerList,
    Integer64List,
    RealList,
    BooleanList,
}

const N: usize = 14;

const STATES: [State; N] = [
    State::Untyped,
    State::String,
    State::Integer,
    State::Real,
    State::Integer64,
    State::Boolean,
    State::Date,
    State::Time,
    State::DateTime,
    State::StringList,
    State::IntegerList,
    State::Integer64List,
    State::RealList,
    State::BooleanList,
];

impl State {
    /// The OGR type of a list.
    pub(crate) fn list_type(self) -> Option<OgrType> {
        Some(match self {
            State::StringList => OgrType::new(OgrFieldType::StringList),
            State::IntegerList => OgrType::new(OgrFieldType::IntegerList),
            State::Integer64List => OgrType::new(OgrFieldType::Integer64List),
            State::RealList => OgrType::new(OgrFieldType::RealList),
            State::BooleanList => OgrType { field_type: OgrFieldType::IntegerList, subtype: OgrFieldSubType::Boolean },
            _ => return None,
        })
    }

    fn is_list(self) -> bool {
        self.list_type().is_some()
    }

    /// A list type of numbers, which GDAL sets into a number field when it has one value.
    fn is_number_list(self) -> bool {
        matches!(self, State::IntegerList | State::Integer64List | State::RealList | State::BooleanList)
    }

    /// The list a property of this type becomes when it holds several values.
    fn as_list(self) -> Self {
        match self {
            State::Integer => State::IntegerList,
            State::Integer64 => State::Integer64List,
            State::Real => State::RealList,
            State::String => State::StringList,
            State::Boolean => State::BooleanList,
            s => s,
        }
    }
}

/// `CPLGetValueType`'s answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueType {
    String,
    Integer,
    Real,
}

/// C's `isspace` in the C locale.
fn is_c_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `CPLGetValueType`: whether a string is an integer, a real or other text.
fn value_type(s: &str) -> ValueType {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && is_c_space(b[i]) {
        i += 1;
    }
    if i == b.len() {
        return ValueType::String;
    }
    if b[i] == b'+' || b[i] == b'-' {
        i += 1;
    }
    let at = |k: usize| b.get(k).copied().unwrap_or(0);
    if at(i) == b'0' && at(i + 1) != 0 && at(i + 1) != b'.' {
        return ValueType::String;
    }
    let (mut dot, mut exponent, mut last_exponent, mut real, mut mantissa) = (false, false, false, false, false);
    let mut after_exponent = None;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_digit() {
            last_exponent = false;
            mantissa = true;
        } else if is_c_space(c) {
            if b[i..].iter().all(|&c| is_c_space(c)) {
                break;
            }
            return ValueType::String;
        } else if c == b'-' || c == b'+' {
            if !last_exponent {
                return ValueType::String;
            }
            last_exponent = false;
        } else if c == b'.' {
            real = true;
            if dot || last_exponent {
                return ValueType::String;
            }
            dot = true;
            last_exponent = false;
        } else if matches!(c, b'D' | b'd' | b'E' | b'e') {
            if !mantissa || !(matches!(at(i + 1), b'+' | b'-') || at(i + 1).is_ascii_digit()) {
                return ValueType::String;
            }
            real = true;
            if exponent {
                return ValueType::String;
            }
            exponent = true;
            after_exponent = Some(i + 1);
            last_exponent = true;
        } else {
            return ValueType::String;
        }
        i += 1;
    }
    if real && after_exponent.is_some_and(|k| b.len() - k > 3) && atof(s).is_infinite() {
        return ValueType::String;
    }
    if real { ValueType::Real } else { ValueType::Integer }
}

/// C's `atoll` (`strtoll`): the leading integer, clamped to 64 bits, 0 when there is none.
fn atoll(s: &str) -> i64 {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && is_c_space(b[i]) {
        i += 1;
    }
    let negative = b.get(i) == Some(&b'-');
    if matches!(b.get(i), Some(b'-' | b'+')) {
        i += 1;
    }
    let mut v: i128 = 0;
    while let Some(d) = b.get(i).filter(|d| d.is_ascii_digit()) {
        v = (v * 10 + i128::from(d - b'0')).min(i128::from(i64::MAX) + 1);
        i += 1;
    }
    (if negative { -v } else { v }).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// C's `atoi` where `long` has 64 bits (Linux and macOS, as `(int)strtol`): the leading integer,
/// clamped to 64 bits and truncated to 32.
fn atoi(s: &str) -> i32 {
    atoll(s) as i32
}

/// The longest prefix of `s` that `fast_float` parses as a number (an optional `-`, digits with
/// an optional `.`, at least one digit, and an optional exponent), parsed.
fn float_prefix(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut i = usize::from(b.first() == Some(&b'-'));
    let digits = |i: &mut usize| {
        let start = *i;
        while b.get(*i).is_some_and(u8::is_ascii_digit) {
            *i += 1;
        }
        *i - start
    };
    let mut n = digits(&mut i);
    if b.get(i) == Some(&b'.') {
        i += 1;
        n += digits(&mut i);
    }
    if n == 0 {
        return None;
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let mut k = i + 1;
        if matches!(b.get(k), Some(b'+' | b'-')) {
            k += 1;
        }
        if digits(&mut k) > 0 {
            i = k;
        }
    }
    s[..i].parse().ok()
}

/// `CPLAtof` (`CPLStrtodDelim` with GDAL's `fast_float`): the leading number of a string, 0 when
/// there is none.
fn atof(s: &str) -> f64 {
    let s = s.trim_start_matches([' ', '\r', '\n', '\t']);
    let starts_ci = |s: &str, p: &str| s.len() >= p.len() && s.as_bytes()[..p.len()].eq_ignore_ascii_case(p.as_bytes());
    if s.starts_with("-1.#QNAN") || s.starts_with("-1.#IND") || s.starts_with("1.#QNAN") || s.starts_with("1.#SNAN") {
        return f64::NAN;
    }
    if starts_ci(s, "-1.#INF") {
        return f64::NEG_INFINITY;
    }
    if starts_ci(s, "1.#INF") {
        return f64::INFINITY;
    }
    let s = s.strip_prefix('+').unwrap_or(s);
    let end = s.bytes().position(|b| !(b.is_ascii_digit() || matches!(b, b'.' | b'+' | b'-' | b'e' | b'E'))).unwrap_or(s.len());
    match float_prefix(&s[..end]) {
        Some(v) => v,
        None if starts_ci(s, "-inf") => f64::NEG_INFINITY,
        None if starts_ci(s, "inf") => f64::INFINITY,
        None if starts_ci(s, "nan") => f64::NAN,
        None => 0.0,
    }
}

/// What `AnalysePropertyValue` reads of a value: `None` for an empty value, which only makes a
/// property of several values a list.
#[derive(Debug, Clone, Copy)]
struct Class {
    value_type: ValueType,
    /// `true` or `false`.
    boolean: bool,
    /// Digits as `YYYY-MM-DD` at the start.
    date: bool,
    /// Followed by `THH:MM:SS` and the end, a fraction or a time zone.
    date_time: bool,
    /// Nothing after the date.
    date_only: bool,
    /// `HH:MM:SS` and the end or a fraction.
    time: bool,
    /// The value fits in 32 bits (`CPLAtoGIntBig`).
    fits: bool,
}

fn class(v: &str) -> Option<Class> {
    if v.is_empty() {
        return None;
    }
    let b = v.as_bytes();
    let at = |k: usize| b.get(k).copied().unwrap_or(0);
    let digit = |k: usize, max: u8| (b'0'..=b'0' + max).contains(&at(k));
    let date = digit(0, 9)
        && digit(1, 9)
        && digit(2, 9)
        && digit(3, 9)
        && at(4) == b'-'
        && digit(5, 1)
        && digit(6, 9)
        && at(7) == b'-'
        && digit(8, 3)
        && digit(9, 9);
    let date_time = at(10) == b'T'
        && digit(11, 2)
        && digit(12, 9)
        && at(13) == b':'
        && digit(14, 5)
        && digit(15, 9)
        && at(16) == b':'
        && digit(17, 6)
        && digit(18, 9)
        && matches!(at(19), 0 | b'.' | b'Z' | b'+' | b'-');
    let time = digit(0, 2)
        && digit(1, 9)
        && at(2) == b':'
        && digit(3, 5)
        && digit(4, 9)
        && at(5) == b':'
        && digit(6, 6)
        && digit(7, 9)
        && matches!(at(8), 0 | b'.');
    let value_type = value_type(v);
    Some(Class {
        value_type,
        boolean: v == "true" || v == "false",
        date,
        date_time,
        date_only: at(10) == 0,
        time,
        fits: value_type == ValueType::Integer && i32::try_from(atoll(v)).is_ok(),
    })
}

/// The type of a property of type `s` after its value at index `j` (`AnalysePropertyValue`'s loop).
fn step(s: State, c: Option<Class>, j: usize) -> State {
    use State as S;
    let s = if j > 0 { s.as_list() } else { s };
    let Some(c) = c else { return s };
    if c.value_type == ValueType::String && s != S::String && s != S::StringList {
        return match s {
            S::Untyped | S::Boolean if c.boolean => S::Boolean,
            S::BooleanList if c.boolean => S::BooleanList,
            S::BooleanList | S::IntegerList | S::Integer64List | S::RealList => S::StringList,
            S::Untyped | S::DateTime | S::Date if c.date => {
                if c.date_time {
                    S::DateTime
                } else if c.date_only {
                    if s == S::DateTime { s } else { S::Date }
                } else {
                    S::String
                }
            }
            S::Untyped | S::Time if c.time => S::Time,
            _ => S::String,
        };
    }
    let real = c.value_type == ValueType::Real;
    match s {
        S::Untyped | S::Integer | S::Integer64 if real => S::Real,
        S::Untyped | S::Integer if c.fits => S::Integer,
        S::Untyped | S::Integer => S::Integer64,
        S::IntegerList | S::Integer64List if real => S::RealList,
        S::IntegerList if c.value_type == ValueType::Integer && !c.fits => S::Integer64List,
        s => s,
    }
}

/// GDAL's inference of the types of a layer's properties from their values, feature by feature, in
/// document order. GDAL analyses a property's values anew each time it adds one to a feature, from
/// the type the property has then; the values the feature has so far are summed up as what they
/// make of each type ([`Self::add`]), so that each value costs the same however many the feature
/// has (and a property of one value, one step).
///
/// GDAL reads the features twice: it infers the types (its prescan), then reads the features,
/// still inferring, from the types the prescan gave. Where the second pass makes a scalar a list,
/// the field keeps the prescan's type and GDAL sets the list into it ([`scalar_read`]).
pub(crate) struct Inference {
    states: Vec<State>,
    /// For each property, what the values of the current feature make of each type, from its
    /// second value.
    prefix: Vec<[State; N]>,
    /// For each property, its first value in the current feature.
    first: Vec<Option<Class>>,
    /// For each property, its number of values in the current feature.
    count: Vec<usize>,
    /// For each property, the feature it last had a value in, plus one.
    feature: Vec<usize>,
}

impl Inference {
    /// The inference of `n` properties, untyped.
    pub(crate) fn new(n: usize) -> Self {
        Self::from_states(vec![State::Untyped; n])
    }

    /// The inference of properties of these types (GDAL's read after its prescan).
    pub(crate) fn from_states(states: Vec<State>) -> Self {
        let n = states.len();
        Self { states, prefix: vec![STATES; n], first: vec![None; n], count: vec![0; n], feature: vec![0; n] }
    }

    /// Adds a value of property `i` in feature `feature` (`GMLReader::SetFeaturePropertyDirectly`):
    /// GDAL analyses the property's values in the feature, except when the value added is null.
    pub(crate) fn add(&mut self, feature: usize, i: usize, sub: Sub<'_>) {
        if self.feature[i] != feature + 1 {
            self.feature[i] = feature + 1;
            self.count[i] = 0;
        }
        let c = class(sub.value());
        let j = self.count[i];
        self.count[i] += 1;
        if j == 0 {
            self.first[i] = c;
            if sub != Sub::Nil {
                self.states[i] = step(self.states[i], c, 0);
            }
            return;
        }
        if j == 1 {
            let first = self.first[i];
            self.prefix[i] = STATES.map(|s| step(s, first, 0));
        }
        for s in &mut self.prefix[i] {
            *s = step(*s, c, j);
        }
        if sub != Sub::Nil {
            self.states[i] = self.prefix[i][self.states[i] as usize];
        }
    }

    /// The type of property `i`.
    pub(crate) fn state(&self, i: usize) -> State {
        self.states[i]
    }

    /// The types of the properties.
    pub(crate) fn into_states(self) -> Vec<State> {
        self.states
    }
}

/// What GDAL reads of a scalar property of a feature that its reading made a list of type
/// `read`: nothing (GDAL leaves the field unset) unless the property is a number with one value,
/// which GDAL sets as it is. `None` when the property is read as its type.
pub(crate) fn scalar_read(field: State, read: State, values: usize) -> Option<bool> {
    (!field.is_list() && read.is_list())
        .then(|| matches!(field, State::Integer | State::Integer64 | State::Real | State::Boolean) && read.is_number_list() && values == 1)
}

/// The value of a list property of type `s` with these elements, as GDAL sets it: null when the
/// first element is null, otherwise the JSON text of the elements converted to the list's type.
/// Reals that are not finite are null.
pub(crate) fn list_value(subs: &[Sub<'_>], s: State) -> FieldValue {
    use std::io::Write as _;
    if subs.first().is_none_or(|sub| *sub == Sub::Nil) {
        return FieldValue::Null;
    }
    // Written as serde_json writes a `Value` array, without building one.
    let mut out = Vec::new();
    out.push(b'[');
    for (k, sub) in subs.iter().enumerate() {
        if k > 0 {
            out.push(b',');
        }
        let v = sub.value();
        let _ = match s {
            State::IntegerList => write!(out, "{}", atoi(v)),
            State::Integer64List => write!(out, "{}", atoll(v)),
            State::RealList => match serde_json::Number::from_f64(atof(v)) {
                Some(n) => write!(out, "{n}"),
                None => out.write_all(b"null"),
            },
            State::BooleanList => out.write_all(if v == "true" || v == "1" { b"true" } else { b"false" }),
            _ => serde_json::to_writer(&mut out, v).map_err(std::io::Error::other),
        };
    }
    out.push(b']');
    FieldValue::Text(String::from_utf8(out).unwrap_or_default())
}

/// The type of a property GDAL reads from an application schema (`GMLParseFeatureType` and
/// `GetSimpleTypeProperties`), for the types it lists: from the element's `type`, from a
/// top-level `simpleType` it names, or from the element's own `simpleType`, by the (unprefixed)
/// name of the type or of the base of the simple type's restriction.
fn schema_type(element: &Element, schema: &Element) -> Option<State> {
    let strip = |name: &str| name.split_once(':').map_or(name, |(_, local)| local).to_string();
    let restriction = |simple: &Element| -> Option<State> {
        let base = strip(simple.child("restriction")?.attr("base").unwrap_or(""));
        Some(match base.to_ascii_lowercase().as_str() {
            "decimal" | "float" | "double" => State::Real,
            "integer" | "short" => State::Integer,
            "long" | "unsignedlong" => State::Integer64,
            "string" => State::String,
            "boolean" => State::Boolean,
            _ => return None,
        })
    };
    match element.attr("type") {
        Some(t) => {
            let name = strip(t);
            Some(match name.to_ascii_lowercase().as_str() {
                "string" | "character" | "hexbinary" => State::String,
                "real" | "double" | "decimal" | "float" => State::Real,
                "int" | "integer" | "short" => State::Integer,
                "long" | "unsignedlong" => State::Integer64,
                "boolean" => State::Boolean,
                "date" | "time" | "datetime" | "timeinstanttype" => return None,
                _ if t.starts_with("gml:") => return None,
                _ => {
                    let simple = schema
                        .children
                        .iter()
                        .find(|c| c.name == "simpleType" && c.attr("name").is_some_and(|n| n.eq_ignore_ascii_case(&name)))?;
                    return restriction(simple);
                }
            })
        }
        None => restriction(element.child("simpleType")?),
    }
}

/// The list type GDAL gives a property of an application schema: a property declared with a
/// `maxOccurs` other than `1`, of a type GDAL lists (`GetListTypeFromSingleType`): strings,
/// integers, 64-bit integers, reals and booleans. Dates and times are not listed.
pub(crate) fn schema_list_type(element: &Element, schema: &Element) -> Option<State> {
    let max = element.attr("maxOccurs")?;
    if max == "1" {
        return None;
    }
    schema_type(element, schema).map(State::as_list)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(features: &[&[&str]]) -> State {
        let mut inference = Inference::new(1);
        for (f, values) in features.iter().enumerate() {
            for v in *values {
                inference.add(f, 0, if *v == "nil" { Sub::Nil } else { Sub::Text(v) });
            }
        }
        inference.state(0)
    }

    #[test]
    fn values_are_typed_as_cpl_get_value_type_types_them() {
        for (v, t) in [
            ("1", ValueType::Integer),
            (" -25 ", ValueType::Integer),
            ("+", ValueType::Integer),
            ("0", ValueType::Integer),
            ("007", ValueType::String),
            ("0.5", ValueType::Real),
            (".5", ValueType::Real),
            ("1e3", ValueType::Real),
            ("25.e+3", ValueType::Real),
            ("1d2", ValueType::Real),
            ("1e", ValueType::String),
            ("e5", ValueType::String),
            ("2-5", ValueType::String),
            ("1 2", ValueType::String),
            ("1e9999", ValueType::String),
            ("1e999", ValueType::Real),
            ("", ValueType::String),
            ("abc", ValueType::String),
        ] {
            assert_eq!(value_type(v), t, "{v:?}");
        }
    }

    #[test]
    fn numbers_are_converted_as_c_converts_them() {
        assert_eq!(atoi(" 7"), 7);
        assert_eq!(atoi("8x"), 8);
        assert_eq!(atoi(""), 0);
        assert_eq!(atoi("5000000000"), 705_032_704);
        assert_eq!(atoll("-99999999999999999999"), i64::MIN);
        assert_eq!(atof(" 2.5e1"), 25.0);
        assert_eq!(atof("x"), 0.0);
        assert_eq!(atof("+1.5x"), 1.5);
        assert_eq!(atof("1e"), 1.0);
        assert_eq!(atof("-.5"), -0.5);
        assert_eq!(atof("INF"), f64::INFINITY);
        assert!(atof("nan").is_nan());
    }

    #[test]
    fn properties_are_typed_as_gdal_types_them() {
        assert_eq!(typed(&[&["1", "2"]]), State::IntegerList);
        assert_eq!(typed(&[&["3"], &["1", "2"]]), State::IntegerList);
        assert_eq!(typed(&[&["1", "2"], &["2.5"]]), State::RealList);
        assert_eq!(typed(&[&["1", "2"], &["5000000000"]]), State::Integer64List);
        assert_eq!(typed(&[&["1", "2"], &["x"]]), State::StringList);
        assert_eq!(typed(&[&["true", "false"], &["1"]]), State::BooleanList);
        // Dates, times and empty values do not make a property a list by themselves.
        assert_eq!(typed(&[&["2020-01-01", "2020-01-02"]]), State::Date);
        assert_eq!(typed(&[&["2020-01-01", "abc"]]), State::String);
        assert_eq!(typed(&[&["", "5"]]), State::Integer);
        // A null first is text: it makes the next value a string list.
        assert_eq!(typed(&[&["nil", "1"]]), State::StringList);
        assert_eq!(typed(&[&["1", "nil"]]), State::Integer);
        assert_eq!(typed(&[&["1", "nil", "2"]]), State::StringList);
    }

    #[test]
    fn lists_are_set_as_gdal_sets_them() {
        let text = |v: FieldValue| match v {
            FieldValue::Text(s) => s,
            other => format!("{other:?}"),
        };
        let subs = [Sub::Text(" 7"), Sub::Text("8x"), Sub::Text("  "), Sub::Nil];
        assert_eq!(text(list_value(&subs, State::IntegerList)), "[7,8,0,0]");
        assert_eq!(text(list_value(&subs, State::RealList)), "[7.0,8.0,0.0,0.0]");
        assert_eq!(text(list_value(&subs, State::StringList)), r#"["7","8x","","___OGR_GML_NULL___"]"#);
        assert_eq!(text(list_value(&[Sub::Text("true"), Sub::Text("1"), Sub::Text("yes")], State::BooleanList)), "[true,true,false]");
        assert_eq!(list_value(&[Sub::Nil, Sub::Text("1")], State::IntegerList), FieldValue::Null);
    }
}
