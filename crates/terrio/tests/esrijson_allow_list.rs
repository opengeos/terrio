// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! ESRI JSON under a process-wide allow-list of GeoJSON alone: one test in a binary of its own,
//! because a list once set cannot be lifted (see `allow_list.rs`).

use terrio::formats::{Kind, set_allowed_drivers};

const PAGE: &str = r#"{"geometryType":"esriGeometryPoint","fields":[{"name":"A","type":"esriFieldTypeString"}],"features":[{"attributes":{"A":"x"},"geometry":{"x":1,"y":2}}]}"#;

#[test]
fn an_allow_list_of_geojson_refuses_esrijson() {
    set_allowed_drivers(Some(&["GeoJSON"]), Some(Kind::Vector)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let options = terrio::VectorOpenOptions::default();
    for name in ["a.json", "query"] {
        let path = dir.path().join(name);
        std::fs::write(&path, PAGE).unwrap();
        let refused = |r: terrio::Result<terrio::VectorDataset>| r.is_err_and(|e| e.to_string().contains("not allowed"));
        assert!(refused(terrio::open_vector(path.to_str().unwrap(), &options)), "{name}");
        assert!(refused(terrio::memory::open_vector_bytes(PAGE.as_bytes().to_vec(), name, &options)), "{name} in memory");
    }
}
