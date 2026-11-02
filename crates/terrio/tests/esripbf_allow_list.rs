// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! The protocol buffer of feature-service queries under a process-wide allow-list of OpenStreetMap
//! alone: one test in a binary of its own, because a list once set cannot be lifted (see
//! `allow_list.rs`).

use terrio::formats::{Kind, set_allowed_drivers};

#[test]
fn an_allow_list_of_osm_refuses_esripbf_under_its_extension() {
    set_allowed_drivers(Some(&["OSM"]), Some(Kind::Vector)).unwrap();
    // A query result holding one field name, enough to show the format.
    let result = [&[(1u8 << 3) | 2, 1][..], b"A"].concat();
    let query = [&[(1u8 << 3) | 2, result.len() as u8][..], &result].concat();
    let page = [&[(2u8 << 3) | 2, query.len() as u8][..], &query].concat();
    let dir = tempfile::tempdir().unwrap();
    let options = terrio::VectorOpenOptions::default();
    let path = dir.path().join("a.pbf");
    std::fs::write(&path, &page).unwrap();
    let refused = |r: terrio::Result<terrio::VectorDataset>| r.is_err_and(|e| e.to_string().contains("not allowed"));
    assert!(refused(terrio::open_vector(path.to_str().unwrap(), &options)));
    assert!(refused(terrio::memory::open_vector_bytes(page.clone(), "a.pbf", &options)));
    // The prefix is refused by its name, before any request.
    assert!(refused(terrio::open_vector("ESRIPBF:http://127.0.0.1:9/query?f=pbf", &options)));
    remote_query_is_refused(page, &options);
}

/// A query answered by a local service: refused for what it answers.
#[cfg(all(not(target_family = "wasm"), feature = "remote"))]
fn remote_query_is_refused(page: Vec<u8>, options: &terrio::VectorOpenOptions) {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/FeatureServer/0/query?f=pbf", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 && !line.trim().is_empty() {
                line.clear();
            }
            let _ = stream.write_all(&[b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".as_slice(), &page].concat());
        }
    });
    let e = terrio::open_vector(&url, options).unwrap_err().to_string();
    assert!(e.contains("not allowed"), "{e}");
}

#[cfg(not(all(not(target_family = "wasm"), feature = "remote")))]
fn remote_query_is_refused(_: Vec<u8>, _: &terrio::VectorOpenOptions) {}
