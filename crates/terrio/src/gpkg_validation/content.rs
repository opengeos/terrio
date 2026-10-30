// SPDX-FileCopyrightText: Copyright (c) 2017, Even Rouault <even.rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.13.3:
//     swig/python/gdal-utils/osgeo_utils/samples/validate_gpkg.py

//! Advisory user-value checks, corresponding to GDAL's optional extra checks.
use super::{schema::Column, valid_timestamp};
use crate::drivers::sqlite::Value;

pub(super) fn valid_data_type(kind: &str) -> bool {
    matches!(
        kind,
        "BOOLEAN"
            | "TINYINT"
            | "SMALLINT"
            | "MEDIUMINT"
            | "INT"
            | "INTEGER"
            | "FLOAT"
            | "DOUBLE"
            | "REAL"
            | "TEXT"
            | "BLOB"
            | "DATE"
            | "DATETIME"
    ) || limit(kind, "TEXT").is_some()
        || limit(kind, "BLOB").is_some()
}
fn limit(kind: &str, prefix: &str) -> Option<u64> {
    kind.strip_prefix(prefix)?.strip_prefix('(')?.strip_suffix(')')?.trim().parse().ok()
}
fn geometry_type(kind: &str) -> bool {
    matches!(
        kind,
        "GEOMETRY"
            | "POINT"
            | "LINESTRING"
            | "POLYGON"
            | "MULTIPOINT"
            | "MULTILINESTRING"
            | "MULTIPOLYGON"
            | "GEOMETRYCOLLECTION"
            | "CIRCULARSTRING"
            | "COMPOUNDCURVE"
            | "CURVEPOLYGON"
            | "MULTICURVE"
            | "MULTISURFACE"
            | "CURVE"
            | "SURFACE"
    )
}
fn valid_date(text: &str) -> bool {
    text.len() == 10 && valid_timestamp(&format!("{text}T00:00:00.000Z"))
}
fn valid_datetime(text: &str) -> bool {
    // GeoPackage's UTC forms; unlike GDAL's prefix regex, require a complete
    // calendar-valid value. The stricter advisory diagnostics are tested below.
    match text.strip_suffix('Z') {
        Some(value) if value.len() == 16 => valid_timestamp(&format!("{value}:00.000Z")),
        Some(value) if value.len() == 19 => valid_timestamp(&format!("{value}.000Z")),
        Some(value) if value.len() == 23 => valid_timestamp(text),
        _ => false,
    }
}
/// At most one diagnostic per column is retained by the streaming caller.
pub(super) fn warning(column: &Column, value: &Value) -> Option<String> {
    let kind = column.data_type.as_str();
    let problem = match (kind, value) {
        (_, Value::Null) => return None,
        ("BOOLEAN", Value::Int(value)) if !matches!(*value, 0 | 1) => "expected integer 0 or 1".into(),
        ("TINYINT", Value::Int(value)) if !(-128..=127).contains(value) => "expected integer in [-128,127]".into(),
        ("SMALLINT", Value::Int(value)) if !(-32768..=32767).contains(value) => "expected integer in [-32768,32767]".into(),
        ("MEDIUMINT", Value::Int(value)) if !(-2147483648..=2147483647).contains(value) => {
            "expected integer in [-2147483648,2147483647]".into()
        }
        ("BOOLEAN" | "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "INTEGER", Value::Int(_)) => return None,
        ("FLOAT", Value::Real(value)) if !value.is_nan() && (*value as f32) as f64 != *value => {
            "value cannot be represented as IEEE Float32".into()
        }
        ("FLOAT", Value::Int(value)) if ((*value as f64) as f32) as f64 != *value as f64 => {
            "value cannot be represented as IEEE Float32".into()
        }
        // SQLite stores integral REAL values using integer serial types on disk,
        // but SELECT/typeof exposes REAL affinity as REAL (datatype3.html).
        ("FLOAT" | "DOUBLE" | "REAL", Value::Real(_) | Value::Int(_)) => return None,
        ("TEXT", Value::Text(_)) | ("BLOB", Value::Blob(_)) => return None,
        ("DATE", Value::Text(text)) if valid_date(text) => return None,
        ("DATETIME", Value::Text(text)) if valid_datetime(text) => return None,
        ("DATE" | "DATETIME", Value::Text(_)) => format!("invalid {kind} value"),
        (_, Value::Blob(_)) if geometry_type(kind) => return None,
        (_, Value::Text(text)) if limit(kind, "TEXT").is_some() => {
            let max = limit(kind, "TEXT").expect("checked limit");
            // Count Unicode characters, including embedded NULs, not encoded bytes.
            if text.chars().count() as u64 <= max {
                return None;
            }
            format!("text exceeds {max} characters")
        }
        (_, Value::Blob(bytes)) if limit(kind, "BLOB").is_some() => {
            let max = limit(kind, "BLOB").expect("checked limit");
            if bytes.len() as u64 <= max {
                return None;
            }
            format!("blob exceeds {max} bytes")
        }
        _ => format!("stored value does not match declared {kind}"),
    };
    Some(format!("column {}: {problem}", column.name))
}

#[cfg(test)]
mod tests {
    use super::super::schema::Schema;
    use super::*;
    fn column(kind: &str) -> Column {
        Schema::parse(&format!("CREATE TABLE t(v {kind})")).unwrap().columns.remove(0)
    }
    #[test]
    fn declarations_and_value_ranges() {
        for kind in ["BOOLEAN", "INT", "INTEGER", "FLOAT", "DOUBLE", "REAL", "DATE", "DATETIME", "TEXT", "BLOB", "TEXT(3)", "BLOB(0)"] {
            assert!(valid_data_type(kind), "{kind}");
            assert!(warning(&column(kind), &Value::Null).is_none());
        }
        for kind in ["VARCHAR", "POINT", "TEXT(-1)", "BLOB(1,2)", "TEXT(x)", ""] {
            assert!(!valid_data_type(kind), "{kind}");
        }
        for (kind, min, max) in
            [("BOOLEAN", 0, 1), ("TINYINT", -128, 127), ("SMALLINT", -32768, 32767), ("MEDIUMINT", -2147483648, 2147483647)]
        {
            let column = column(kind);
            for value in [min, max] {
                assert!(warning(&column, &Value::Int(value)).is_none());
            }
            for value in [min - 1, max + 1] {
                assert!(warning(&column, &Value::Int(value)).is_some());
            }
            assert!(warning(&column, &Value::Text("not integer".into())).is_some());
        }
        assert!(warning(&column("REAL"), &Value::Int(1)).is_none());
        assert!(warning(&column("FLOAT"), &Value::Real(1.5)).is_none());
        assert!(warning(&column("FLOAT"), &Value::Real(1.1)).is_some());
        assert!(warning(&column("FLOAT"), &Value::Real(1e40)).is_some());
        assert!(warning(&column("INTEGER"), &Value::Real(1.5)).is_some());
        assert!(warning(&column("TEXT(3)"), &Value::Text("é中🙂".into())).is_none());
        assert!(warning(&column("TEXT(3)"), &Value::Text("a\0bc".into())).is_some());
        assert!(warning(&column("BLOB(2)"), &Value::Blob(vec![0; 3])).is_some());
        assert!(warning(&column("POINT"), &Value::Blob(vec![])).is_none());
    }
    #[test]
    fn dates_require_complete_calendar_valid_utc_values() {
        assert!(valid_date("2024-02-29"));
        for value in ["1900-02-29", "2024-02-31", "2024-19-31", "2024-01-01tail"] {
            assert!(!valid_date(value));
        }
        for value in ["2024-02-29T23:59Z", "2024-02-29T23:59:59Z", "2024-02-29T23:59:59.123Z"] {
            assert!(valid_datetime(value), "{value}");
        }
        for value in ["2024-02-29T24:00Z", "2024-02-29T23:59:59X123Z", "2024-02-29T23:59:59.123Ztail", "2024-02-29T23:59:59+00:00"] {
            assert!(!valid_datetime(value), "{value}");
        }
    }
}
