// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Scalar fields, physical FIDs and WKB for scripts/compare_shapefile_deleted.py.
use serde_json::json;
use terrio::{FieldValue, VectorOpenOptions};

/// Reads one live-feature page without rewriting its physical feature IDs.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if !(4..=5).contains(&args.len()) || (args.len() == 5 && args[4] != "--stream") {
        return Err("usage: shapefile_rows FILE SKIP MAX (or - for no maximum) [--stream]".into());
    }
    let options = VectorOpenOptions {
        skip_features: args[2].parse()?,
        max_features: if args[3] == "-" { None } else { Some(args[3].parse()?) },
        ..Default::default()
    };
    let layers = if args.len() == 5 {
        if options.skip_features != 0 || options.max_features.is_some() {
            return Err("the stream comparison reads the whole live result set".into());
        }
        terrio::stream::open_layers(&args[1], &options, 2)?.into_iter().map(|r| r.read_all()).collect::<terrio::Result<Vec<_>>>()?
    } else {
        terrio::open_vector(&args[1], &options)?.layers
    };
    let mut rows = Vec::new();
    for layer in &layers {
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
