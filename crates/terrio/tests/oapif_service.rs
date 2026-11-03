// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Native OAPIF discovery and paging, including observed HTTP request counts.
#![cfg(all(feature = "remote", not(target_family = "wasm")))]

use terrio::{FieldType, FieldValue, VectorOpenOptions};
#[path = "common/oapif.rs"]
mod fixture;
use fixture::Server;

#[test]
fn native_reader_follows_redirect_base_and_stops_at_requested_range() {
    let server = Server::new();
    let mut o = VectorOpenOptions { layers: vec!["places".into()], skip_features: 1, max_features: Some(4), ..Default::default() };
    o.oapif.page_size = 2;
    let ds = terrio::open_vector(&format!("OAPIF:{}/alias", server.url), &o).unwrap();
    assert_eq!(ds.driver, terrio::formats::VectorDriver::OAPIF);
    let l = &ds.layers[0];
    assert_eq!(l.len(), 4);
    assert_eq!(l.crs_epsg(), Some(4326));
    assert_eq!(l.features.iter().map(|f| f.fid).collect::<Vec<_>>(), [1, 2, 3, 4]);
    assert_eq!(l.features[0].get(&l.schema, "name").unwrap(), &FieldValue::Text("p1".into()));
    assert_eq!(l.schema.fields().iter().find(|f| f.name == "missing").unwrap().field_type, FieldType::Integer);
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 7, "{requests:?}");
    let pages: Vec<_> = requests.iter().filter(|(url, _)| url.contains("/items")).collect();
    assert_eq!(pages.len(), 3);
    assert!(pages.iter().all(|(_, accept)| accept.contains("application/geo+json")));
    assert!(requests.iter().any(|(u, _)| u == "/api/collections"), "relative link must use redirected /api/ base");
}

#[test]
fn native_metadata_calls_never_fetch_features_or_schema() {
    let server = Server::new();
    let url = format!("OAPIF:{}/alias", server.url);
    assert_eq!(terrio::list_layers(&url).unwrap(), ["places"]);
    let info = terrio::info::info_path(&url, &Default::default()).unwrap();
    let value = serde_json::to_value(info).unwrap();
    assert_eq!(value["driver"], "OAPIF");
    assert_eq!(value["metadata_only"], true);
    assert_eq!(value["layers"][0]["feature_count"], serde_json::Value::Null);
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 6);
    assert!(!requests.iter().any(|(url, _)| url.contains("items") || url.contains("schema")));
    assert!(requests.iter().all(|(_, accept)| accept == "application/json"), "{requests:?}");
}
