// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Scalar fields, physical FIDs and WKB for scripts/compare_shapefile_deleted.py.
use serde_json::json;
use terrio::{FieldValue, VectorOpenOptions};

/// Reads one live-feature page without rewriting its physical feature IDs.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err("usage: shapefile_rows FILE SKIP MAX (or - for no maximum)".into());
    }
    let options = VectorOpenOptions {
        skip_features: args[2].parse()?,
        max_features: if args[3] == "-" { None } else { Some(args[3].parse()?) },
        ..Default::default()
    };
    let data = terrio::open_vector(&args[1], &options)?;
    let mut rows = Vec::new();
    for layer in &data.layers {
        for feature in &layer.features {
            let mut attributes = Vec::new();
            for value in &feature.attributes {
                attributes.push(match value {
                    FieldValue::Null => json!(null),
                    FieldValue::Integer(value) => json!(value),
                    FieldValue::Float(value) => json!(value),
                    FieldValue::Text(value) | FieldValue::Date(value) | FieldValue::DateTime(value) => json!(value),
                    FieldValue::Boolean(value) => json!(value),
                    _ => return Err("the comparison expects scalar fields".into()),
                });
            }
            let wkb = feature.geometry.as_ref().map(|geometry| geometry.to_wkb().iter().map(|b| format!("{b:02x}")).collect::<String>());
            rows.push(json!({"fid":feature.fid, "wkb":wkb, "attributes":attributes}));
        }
    }
    serde_json::to_writer(std::io::stdout().lock(), &rows)?;
    Ok(())
}
