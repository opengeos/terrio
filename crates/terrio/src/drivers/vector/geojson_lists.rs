// SPDX-FileCopyrightText: Copyright (c) 2007, Mateusz Loskot
// SPDX-FileCopyrightText: Copyright (c) 2008-2024, Even Rouault <even.rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 1999, Les Technologies SoftMap Inc.
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.13.1 (the source the port follows; its fixtures in tests/data/geojson_lists
//     were validated against GDAL 3.13.2):
//     ogr/ogrlibjsonutils.cpp (GeoJSONPropertyToFieldType)
//     ogr/ogrsf_frmts/geojson/ogrgeojsonreader.cpp (OGRGeoJSONReaderAddOrUpdateField,
//       OGRGeoJSONReaderSetField)
//     ogr/ogrfeature.cpp (OGRFeature::SetField of a string into a StringList field)

//! The list fields of GeoJSON features, typed as GDAL's GeoJSON reader types them: integer,
//! 64-bit integer, real, string and boolean lists, which terrio holds as JSON arrays
//! ([`wbvector::FieldType::Json`] text) and records the OGR types of
//! ([`crate::VectorDataset::ogr_type`]).
//!
//! GDAL types a field from its values in feature order: each value has a type of its own
//! (`GeoJSONPropertyToFieldType`: an array of integers is an `IntegerList`, of integers beyond 32
//! bits an `Integer64List`, of numbers with a real a `RealList`, of strings a `StringList`, of
//! booleans an `IntegerList` of the `Boolean` subtype; an empty array, an array with a null or
//! mixing these, and an object are JSON), and updates the field's type with it (`OGRGeoJSONReaderAddOrUpdateField`): a
//! scalar and a list of a type it widens to make a list, an empty array leaves a list as it is,
//! and other mixtures make JSON. The updates depend on the order of the values (an empty array
//! first makes the field JSON for good), so a scan is summed up as what it makes of each type a
//! field can have before it ([`Typing`]), which scans of consecutive features combine exactly.
//!
//! The values of a list field are converted as GDAL sets them (`OGRGeoJSONReaderSetField`): array
//! elements to the list's type (booleans to 0 or 1 except in boolean lists, numbers to strings in
//! string lists), a scalar to a list of one, and strings to a string list as `OGRFeature::SetField`
//! parses them (`(n:a,b)` lists and JSON arrays); an empty array, `[]` or `(0:)` is an empty
//! string list, which GDAL holds as set, not null.

use crate::ops::vector_schema::{OgrFieldSubType, OgrFieldType, OgrType};
use serde_json::Value;
use wbvector::FieldValue;

/// The type of a GeoJSON field as GDAL's reader holds it while it reads the features.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum State {
    /// No value but nulls yet.
    Undetermined,
    Integer,
    /// `Integer` of the `Boolean` subtype.
    Boolean,
    Integer64,
    Real,
    String,
    /// `String` of the `JSON` subtype.
    Json,
    Date,
    Time,
    DateTime,
    IntegerList,
    /// `IntegerList` of the `Boolean` subtype.
    BooleanList,
    Integer64List,
    RealList,
    StringList,
}

const N: usize = 15;

const STATES: [State; N] = [
    State::Undetermined,
    State::Integer,
    State::Boolean,
    State::Integer64,
    State::Real,
    State::String,
    State::Json,
    State::Date,
    State::Time,
    State::DateTime,
    State::IntegerList,
    State::BooleanList,
    State::Integer64List,
    State::RealList,
    State::StringList,
];

impl State {
    /// The OGR type of a list field.
    pub(crate) fn list_type(self) -> Option<OgrType> {
        let t = |field_type| OgrType::new(field_type);
        Some(match self {
            State::IntegerList => t(OgrFieldType::IntegerList),
            State::BooleanList => OgrType { field_type: OgrFieldType::IntegerList, subtype: OgrFieldSubType::Boolean },
            State::Integer64List => t(OgrFieldType::Integer64List),
            State::RealList => t(OgrFieldType::RealList),
            State::StringList => t(OgrFieldType::StringList),
            _ => return None,
        })
    }
}

/// What GDAL's reader sees of a non-null value: its type (`GeoJSONPropertyToFieldType`, where a
/// string is a `String` and an object or an array that is no list is JSON), the type of a string
/// as dates and times refine it, and whether it is an empty array.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Class {
    of: State,
    refined: State,
    empty: bool,
}

/// An integer at the ends of the 64-bit range, which GDAL reads as clamped, as a real.
fn clamped(n: &serde_json::Number) -> bool {
    n.as_i64().is_some_and(|i| i == i64::MIN || i == i64::MAX)
}

/// The class of a value; `string` is the type of a string value (`Date`, `Time`, `DateTime` or
/// `String`). `None` for null.
pub(crate) fn class(v: &Value, string: State) -> Option<Class> {
    let of = match v {
        Value::Null => return None,
        Value::Bool(_) => State::Boolean,
        Value::Number(n) if clamped(n) => State::Real,
        Value::Number(n) => match n.as_i64() {
            Some(i) if i32::try_from(i).is_ok() => State::Integer,
            Some(_) => State::Integer64,
            None => State::Real,
        },
        Value::String(_) => return Some(Class { of: State::String, refined: string, empty: false }),
        Value::Object(_) => State::Json,
        Value::Array(a) if a.is_empty() => return Some(Class { of: State::Json, refined: State::Json, empty: true }),
        Value::Array(a) => array_type(a),
    };
    Some(Class { of, refined: of, empty: false })
}

/// The type of a non-empty array, as `GeoJSONPropertyToFieldType` reads its elements.
fn array_type(a: &[Value]) -> State {
    use State as S;
    let (mut t, mut boolean) = (S::IntegerList, false);
    for (i, e) in a.iter().enumerate() {
        match e {
            Value::String(_) if i == 0 || t == S::StringList => t = S::StringList,
            Value::Number(n) if !n.is_i64() || clamped(n) => {
                if boolean || !(i == 0 || matches!(t, S::RealList | S::IntegerList | S::Integer64List)) {
                    return S::Json;
                }
                t = S::RealList;
            }
            Value::Number(n) => {
                if !boolean && t == S::IntegerList {
                    if n.as_i64().is_some_and(|v| i32::try_from(v).is_err()) {
                        t = S::Integer64List;
                    }
                } else if boolean || !matches!(t, S::Integer64List | S::RealList) {
                    return S::Json;
                }
            }
            Value::Bool(_) if i == 0 || (t == S::IntegerList && boolean) => boolean = true,
            _ => return S::Json,
        }
    }
    if boolean { S::BooleanList } else { t }
}

/// The type a field of type `s` has after a value of class `c` (`OGRGeoJSONReaderAddOrUpdateField`).
fn step(s: State, c: Class) -> State {
    use State::*;
    let kept_list = |list| if c.empty { list } else { Json };
    match s {
        Undetermined => c.refined,
        Integer => match c.of {
            Integer | Boolean => Integer,
            Integer64 | Real | Integer64List | RealList | StringList => c.of,
            IntegerList | BooleanList => IntegerList,
            _ => Json,
        },
        Boolean => match c.of {
            Integer | Boolean | IntegerList | BooleanList => c.of,
            Integer64 | Real | Integer64List | RealList | StringList => c.of,
            _ => Json,
        },
        Integer64 => match c.of {
            Integer | Boolean | Integer64 => Integer64,
            Real | RealList | StringList => c.of,
            IntegerList | BooleanList | Integer64List => Integer64List,
            _ => Json,
        },
        Real => match c.of {
            Integer | Boolean | Integer64 | Real => Real,
            IntegerList | BooleanList | Integer64List | RealList => RealList,
            StringList => StringList,
            _ => Json,
        },
        // Only a string list changes a string field; anything but a string makes it JSON.
        String => match c.of {
            StringList => StringList,
            String | Json => String,
            _ => Json,
        },
        Json => Json,
        Date | Time | DateTime => match c.of {
            Json => Json,
            String => match c.refined {
                r if r == s => s,
                String => String,
                DateTime if s == Date => DateTime,
                Date if s == DateTime => DateTime,
                _ => Json,
            },
            _ => Json,
        },
        IntegerList => match c.of {
            String | Json => kept_list(IntegerList),
            Integer | Boolean | IntegerList | BooleanList => IntegerList,
            Integer64 => Integer64List,
            Real => RealList,
            Integer64List | RealList | StringList => c.of,
            _ => Json,
        },
        BooleanList => match c.of {
            String | Json => kept_list(BooleanList),
            Boolean | BooleanList => BooleanList,
            Integer | IntegerList => IntegerList,
            Integer64 => Integer64List,
            Real => RealList,
            Integer64List | RealList | StringList => c.of,
            _ => Json,
        },
        Integer64List => match c.of {
            String | Json => kept_list(Integer64List),
            Integer | Boolean | Integer64 | IntegerList | BooleanList | Integer64List => Integer64List,
            Real | RealList => RealList,
            StringList => StringList,
            _ => Json,
        },
        RealList => match c.of {
            String | Json => kept_list(RealList),
            StringList => StringList,
            Integer | Boolean | Integer64 | Real | IntegerList | BooleanList | Integer64List | RealList => RealList,
            _ => Json,
        },
        // Only JSON (an object or an array that is no list) other than an empty array changes it.
        StringList => match c.of {
            Json => kept_list(StringList),
            _ => StringList,
        },
    }
}

/// What a run of values makes of a field of each type it can have before them: their summary,
/// which combines with the summary of the values that follow ([`Typing::then`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Typing([State; N]);

impl Default for Typing {
    fn default() -> Self {
        Self(STATES)
    }
}

impl Typing {
    /// Adds a value.
    pub(crate) fn add(&mut self, c: Class) {
        for s in &mut self.0 {
            *s = step(*s, c);
        }
    }

    /// The summary of these values followed by those of `next`.
    pub(crate) fn then(&mut self, next: &Typing) {
        for s in &mut self.0 {
            *s = next.0[*s as usize];
        }
    }

    /// The type of a field with these values from the first feature.
    pub(crate) fn state(&self) -> State {
        self.0[State::Undetermined as usize]
    }
}

/// `json_object_get_int64` of an element: integers, reals truncated and clamped, booleans as 0 or
/// 1, strings by their leading integer, and anything else 0.
fn get_int64(v: &Value) -> i64 {
    match v {
        Value::Bool(b) => i64::from(*b),
        Value::Number(n) => n.as_i64().unwrap_or_else(|| n.as_f64().map_or(0, |f| f as i64)),
        Value::String(s) => leading_int(s),
        _ => 0,
    }
}

/// The integer at the start of a string, as `strtoll` reads it, clamped to 64 bits.
fn leading_int(s: &str) -> i64 {
    let t = s.trim_start();
    let (sign, digits) = match t.as_bytes().first() {
        Some(b'-') => (-1i128, &t[1..]),
        Some(b'+') => (1, &t[1..]),
        _ => (1, t),
    };
    let mut v: i128 = 0;
    for c in digits.bytes().take_while(u8::is_ascii_digit) {
        v = (v * 10 + i128::from(c - b'0')).min(i128::from(i64::MAX) + 1);
    }
    (sign * v).clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// `json_object_get_double` of an element.
fn get_double(v: &Value) -> f64 {
    match v {
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::Number(n) => n.as_f64().unwrap_or(0.0),
        Value::String(s) => s.trim().parse().unwrap_or(0.0),
        _ => 0.0,
    }
}

/// `json_object_get_string` of an element: a string itself, other values as JSON.
fn get_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        v => v.to_string(),
    }
}

/// An element as its list keeps it: a 32-bit integer (`json_object_get_int`), a boolean, a 64-bit
/// integer or a real.
fn element(v: &Value, s: State) -> Value {
    match s {
        State::IntegerList => Value::from(get_int64(v).clamp(i64::from(i32::MIN), i64::from(i32::MAX))),
        State::BooleanList => Value::from(get_int64(v) != 0),
        State::Integer64List => Value::from(get_int64(v)),
        _ => serde_json::Number::from_f64(get_double(v)).map_or(Value::Null, Value::Number),
    }
}

/// The JSON text of a list.
fn list(values: Vec<Value>) -> FieldValue {
    FieldValue::Text(Value::Array(values).to_string())
}

/// A string set into a string list field (`OGRFeature::SetField`): an OGR list `(n:a,b)` of `n`
/// values, a JSON array (nulls as empty strings), or one value; nothing for an empty string.
fn string_list(s: &str) -> FieldValue {
    if s.is_empty() {
        return FieldValue::Null;
    }
    if s.starts_with('(') && s.ends_with(')') && s.contains(':') {
        let tokens: Vec<&str> = s.split([',', ':', '(', ')']).filter(|t| !t.is_empty()).collect();
        let n = tokens.first().map_or(0, |t| leading_int(t));
        if !tokens.is_empty() && usize::try_from(n).is_ok_and(|n| n == tokens.len() - 1) {
            return list(tokens[1..].iter().map(|t| Value::from(*t)).collect());
        }
        return FieldValue::Null;
    }
    if s.starts_with('[')
        && s.ends_with(']')
        && let Ok(Value::Array(a)) = serde_json::from_str::<Value>(s)
    {
        return list(a.iter().map(|e| Value::from(if e.is_null() { String::new() } else { get_string(e) })).collect());
    }
    list(vec![Value::from(s)])
}

/// A value of a field of list type `s`, as GDAL sets it (`OGRGeoJSONReaderSetField`): the JSON
/// text of the list, or null.
pub(crate) fn value(v: &Value, s: State) -> FieldValue {
    match (s, v) {
        (_, Value::Null) => FieldValue::Null,
        (State::StringList, Value::Array(a)) => list(a.iter().take_while(|e| !e.is_null()).map(|e| Value::from(get_string(e))).collect()),
        (State::StringList, v) => string_list(&get_string(v)),
        (_, Value::Array(a)) => list(a.iter().map(|e| element(e, s)).collect()),
        (State::IntegerList | State::BooleanList | State::Integer64List, Value::Bool(_)) => list(vec![element(v, s)]),
        (State::IntegerList | State::BooleanList | State::Integer64List, Value::Number(n)) if n.is_i64() => list(vec![element(v, s)]),
        (State::RealList, Value::Bool(_) | Value::Number(_)) => list(vec![element(v, s)]),
        // GDAL leaves the field unset.
        _ => FieldValue::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn typed(values: &[Value]) -> State {
        let mut t = Typing::default();
        for v in values {
            if let Some(c) = class(v, State::String) {
                t.add(c);
            }
        }
        t.state()
    }

    #[test]
    fn arrays_are_typed_as_gdal_types_them() {
        assert_eq!(typed(&[json!([1, 2])]), State::IntegerList);
        assert_eq!(typed(&[json!([9007199254740993i64, 1])]), State::Integer64List);
        assert_eq!(typed(&[json!([1, 2.5])]), State::RealList);
        assert_eq!(typed(&[json!(["a", "b"])]), State::StringList);
        assert_eq!(typed(&[json!([true, false])]), State::BooleanList);
        // Mixtures, nulls, nested values are JSON.
        for v in [json!([1, "a"]), json!([true, 1]), json!([1, true]), json!([null, 1]), json!([1, null]), json!([[1]]), json!([{}])] {
            assert_eq!(typed(std::slice::from_ref(&v)), State::Json, "{v}");
        }
        assert_eq!(typed(&[json!([])]), State::Json);
    }

    #[test]
    fn fields_are_typed_in_feature_order() {
        // A list keeps its type over empty arrays; an empty array first makes the field JSON.
        assert_eq!(typed(&[json!([1]), json!([])]), State::IntegerList);
        assert_eq!(typed(&[json!([]), json!([1])]), State::Json);
        // Scalars and lists widen together.
        assert_eq!(typed(&[json!(3), json!([1, 2])]), State::IntegerList);
        assert_eq!(typed(&[json!([1]), json!(2.5)]), State::RealList);
        assert_eq!(typed(&[json!(5000000000i64), json!([1])]), State::Integer64List);
        assert_eq!(typed(&[json!("a"), json!(["b"])]), State::StringList);
        assert_eq!(typed(&[json!([1]), json!("a")]), State::Json);
        assert_eq!(typed(&[json!(["a"]), json!(1)]), State::StringList);
        assert_eq!(typed(&[json!([true]), json!([1])]), State::IntegerList);
        assert_eq!(typed(&[json!(null), json!([1])]), State::IntegerList);
    }

    #[test]
    fn summaries_of_consecutive_values_combine_exactly() {
        let values = [json!(null), json!([]), json!(3), json!([1]), json!(2.5), json!(["a"]), json!({}), json!([true]), json!("x")];
        let class_of = |v: &Value| class(v, State::String);
        // Every split of every sequence of up to four values: the halves' summaries combined
        // equal the whole's.
        let mut seqs: Vec<Vec<&Value>> = vec![Vec::new()];
        for _ in 0..4 {
            seqs = seqs.iter().flat_map(|s| values.iter().map(move |v| [s.clone(), vec![v]].concat())).collect();
            for s in &seqs {
                let mut whole = Typing::default();
                s.iter().filter_map(|v| class_of(v)).for_each(|c| whole.add(c));
                for split in 0..=s.len() {
                    let (mut a, mut b) = (Typing::default(), Typing::default());
                    s[..split].iter().filter_map(|v| class_of(v)).for_each(|c| a.add(c));
                    s[split..].iter().filter_map(|v| class_of(v)).for_each(|c| b.add(c));
                    a.then(&b);
                    assert_eq!(a, whole);
                }
            }
        }
    }

    #[test]
    fn values_are_set_as_gdal_sets_them() {
        let text = |v: FieldValue| match v {
            FieldValue::Text(s) => s,
            other => format!("{other:?}"),
        };
        assert_eq!(text(value(&json!([1, true]), State::IntegerList)), "[1,1]");
        assert_eq!(text(value(&json!(3), State::IntegerList)), "[3]");
        assert_eq!(text(value(&json!(false), State::BooleanList)), "[false]");
        assert_eq!(text(value(&json!([9007199254740993i64]), State::Integer64List)), "[9007199254740993]");
        assert_eq!(text(value(&json!([1, 2.5]), State::RealList)), "[1.0,2.5]");
        // GDAL holds an empty string list as set, not null (`(0:)`), as pyogrio reads it.
        assert_eq!(text(value(&json!([]), State::StringList)), "[]");
        assert_eq!(text(value(&json!("[]"), State::StringList)), "[]");
        assert_eq!(text(value(&json!("(0:)"), State::StringList)), "[]");
        assert_eq!(text(value(&json!([1, true]), State::StringList)), "[\"1\",\"true\"]");
        assert_eq!(text(value(&json!("x"), State::StringList)), "[\"x\"]");
        assert_eq!(text(value(&json!("(2:a,b)"), State::StringList)), "[\"a\",\"b\"]");
        // Malformed OGR lists with no values, or a count that does not match, are null.
        assert_eq!(value(&json!("(:)"), State::StringList), FieldValue::Null);
        assert_eq!(value(&json!("(1:)"), State::StringList), FieldValue::Null);
        assert_eq!(text(value(&json!([]), State::RealList)), "[]");
        assert_eq!(value(&json!(null), State::RealList), FieldValue::Null);
        assert_eq!(value(&json!(""), State::StringList), FieldValue::Null);
    }
}
