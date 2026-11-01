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
    // The prefix is refused by its name, before any request.
    let named = terrio::open_vector("ESRIJSON:http://127.0.0.1:9/query?f=json", &options);
    assert!(named.is_err_and(|e| e.to_string().contains("not allowed")));
    remote_query_is_refused(&options);
}

/// A query answered by a local service: refused for what it answers, whatever its first bytes showed.
#[cfg(all(not(target_family = "wasm"), feature = "remote"))]
fn remote_query_is_refused(options: &terrio::VectorOpenOptions) {
    use std::io::{BufRead, BufReader, Write};
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
            let len = PAGE.len();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {len}\r\nContent-Range: bytes 0-{}/{len}\r\nConnection: close\r\n\r\n",
                len - 1
            );
            let _ = stream.write_all(&[head.as_bytes(), PAGE.as_bytes()].concat());
        }
    });
    let e = terrio::open_vector(&url, options).unwrap_err().to_string();
    assert!(e.contains("not allowed"), "{e}");
}

#[cfg(not(all(not(target_family = "wasm"), feature = "remote")))]
fn remote_query_is_refused(_: &terrio::VectorOpenOptions) {}
