// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! CSV/TSV filenames select a default separator; explicit options take precedence.
//! GDAL 3.12.1 is the reference for CSV geometry and quoting, with SEPARATOR=TAB for TSV.

use terrio::{CsvOptions, Dataset, Layer, VectorOpenOptions, VectorWriteOptions};

fn layer(polygon: bool) -> Layer {
    let geometry = if polygon {
        serde_json::json!({"type":"Polygon","coordinates":[[[0,0],[2,0],[2,2],[0,0]]]})
    } else {
        serde_json::json!({"type":"Point","coordinates":[10,46]})
    };
    let document = serde_json::json!({"type":"FeatureCollection","features":[
        {"type":"Feature","properties":{"id":1,"name":"a, tab\tand \"quote\"\nnext line"},"geometry":geometry}
    ]});
    terrio::memory::parse_geojson_layer(document.to_string().as_bytes(), "shapes").unwrap()
}

#[test]
fn csv_and_tsv_writers_preserve_columns_geometry_and_explicit_separators() {
    let dir = tempfile::tempdir().unwrap();
    for polygon in [false, true] {
        let layer = layer(polygon);
        let src = dir.path().join("source.geojson");
        terrio::write_vector(std::slice::from_ref(&layer), src.to_str().unwrap(), &Default::default()).unwrap();
        for extension in ["csv", "tsv", "TSV"] {
            for separator in [None, Some(b','), Some(b';'), Some(b'\t'), Some(b'|')] {
                let options = VectorWriteOptions { csv: CsvOptions { separator, ..Default::default() }, ..Default::default() };
                let expected = separator.unwrap_or(if extension.eq_ignore_ascii_case("tsv") { b'\t' } else { b',' });
                let name = format!("shapes.{extension}");
                let dst = dir.path().join(&name);
                for surface in ["whole", "bytes", "stream"] {
                    let bytes = match surface {
                        "whole" => {
                            terrio::write_vector(std::slice::from_ref(&layer), dst.to_str().unwrap(), &options).unwrap();
                            std::fs::read(&dst).unwrap()
                        }
                        "bytes" => terrio::memory::write_vector_bytes(std::slice::from_ref(&layer), &name, &options).unwrap(),
                        _ => {
                            let options = terrio::stream::ConvertOptions { write: options.clone(), chunk_size: 1, ..Default::default() };
                            terrio::stream::convert(src.to_str().unwrap(), dst.to_str().unwrap(), &options).unwrap();
                            std::fs::read(&dst).unwrap()
                        }
                    };
                    let header = format!("WKT{}id{}name\n", expected as char, expected as char);
                    assert!(bytes.starts_with(header.as_bytes()), "{surface} {name} {separator:?}: {}", String::from_utf8_lossy(&bytes));
                    std::fs::write(&dst, &bytes).unwrap();
                    let open = VectorOpenOptions { csv: CsvOptions { separator, ..Default::default() }, ..Default::default() };
                    let back = terrio::open_vector(dst.to_str().unwrap(), &open).unwrap();
                    assert_eq!(back.layers[0].features[0].geometry, layer.features[0].geometry);
                    for (index, field) in layer.schema.fields().iter().enumerate() {
                        let actual = back.layers[0].schema.field_index(&field.name).unwrap();
                        assert_eq!(back.layers[0].features[0].attributes[actual], layer.features[0].attributes[index]);
                    }
                    if separator.is_none() {
                        let Dataset::Vector(memory) = terrio::memory::open_bytes(bytes, &name).unwrap() else { panic!() };
                        assert_eq!(memory.layers[0].features.len(), back.layers[0].features.len());
                        for (actual, expected) in memory.layers[0].features.iter().zip(&back.layers[0].features) {
                            assert_eq!(actual.geometry, expected.geometry);
                            assert_eq!(actual.attributes, expected.attributes);
                        }
                    }
                }
            }
        }
    }
}
