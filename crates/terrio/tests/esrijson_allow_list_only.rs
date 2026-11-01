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
    query_from_a_server_that_ignores_ranges_opens(&options);
}

/// A query judged by what a plain request answers, as a first range isn't served: the whole answer,
/// larger than a lazy open accepts, with neither its length nor a range.
#[cfg(all(not(target_family = "wasm"), feature = "remote"))]
fn query_from_a_server_that_ignores_ranges_opens(options: &terrio::VectorOpenOptions) {
    use std::io::{BufRead, BufReader, Write};
    let features = vec![r#"{"attributes":{"A":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"},"geometry":{"x":1,"y":2}}"#; 4000];
    let page = format!(
        r#"{{"geometryType":"esriGeometryPoint","fields":[{{"name":"A","type":"esriFieldTypeString"}}],"features":[{}]}}"#,
        features.join(",")
    );
    assert!(page.len() > 160 * 1024);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/FeatureServer/0/query?f=json", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 && !line.trim().is_empty() {
                line.clear();
            }
            let _ = stream.write_all(&[b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".as_slice(), page.as_bytes()].concat());
        }
    });
    let ds = terrio::open_vector(&url, options).unwrap();
    assert_eq!(ds.driver, VectorDriver::ESRIJSON);
    assert_eq!(ds.layers[0].features.len(), 4000);
}

#[cfg(not(all(not(target_family = "wasm"), feature = "remote")))]
fn query_from_a_server_that_ignores_ranges_opens(_: &terrio::VectorOpenOptions) {}
