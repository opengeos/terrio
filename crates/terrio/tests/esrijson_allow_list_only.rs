// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! A process-wide allow-list of ESRIJSON alone: one test in a binary of its own, because a list
//! once set cannot be lifted (see `allow_list.rs`).

use terrio::formats::{Kind, VectorDriver, set_allowed_drivers};

#[test]
fn an_allow_list_of_esrijson_opens_it_and_not_geojson() {
    set_allowed_drivers(Some(&["ESRIJSON"]), Some(Kind::Vector)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let options = terrio::VectorOpenOptions::default();
    let page = r#"{"geometryType":"esriGeometryPoint","fields":[{"name":"A","type":"esriFieldTypeString"}],"features":[{"attributes":{"A":"x"},"geometry":{"x":1,"y":2}}]}"#;
    let path = dir.path().join("a.json");
    std::fs::write(&path, page).unwrap();
    assert_eq!(terrio::open_vector(path.to_str().unwrap(), &options).unwrap().driver, VectorDriver::ESRIJSON);

    let geojson = r#"{"type":"FeatureCollection","features":[]}"#;
    let path = dir.path().join("b.json");
    std::fs::write(&path, geojson).unwrap();
    assert!(terrio::open_vector(path.to_str().unwrap(), &options).is_err_and(|e| e.to_string().contains("not allowed")));
}
