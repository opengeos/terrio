// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! A process-wide allow-list of ESRIJSON alone, for queries that ask for the protocol buffer: one
//! test in a binary of its own, because a list once set cannot be lifted (see `allow_list.rs`). What
//! each page holds decides.

#![cfg(all(not(target_family = "wasm"), feature = "remote"))]

use std::io::{BufRead, BufReader, Write};
use terrio::formats::{Kind, set_allowed_drivers};

const FEATURES: &[u8] = br#"{"features":[{"attributes":{"A":1}}]}"#;
const MORE: &[u8] = br#"{"exceededTransferLimit":true,"features":[{"attributes":{"A":1}}]}"#;
const DENIED: &[u8] = br#"{"error":{"code":498,"message":"Invalid token."}}"#;

/// A query result holding one field name and no features.
fn binary() -> Vec<u8> {
    let result = [&[(1u8 << 3) | 2, 1][..], b"A"].concat();
    let query = [&[(1u8 << 3) | 2, result.len() as u8][..], &result].concat();
    [&[(2u8 << 3) | 2, query.len() as u8][..], &query].concat()
}

fn answer(request: &str) -> Vec<u8> {
    let next = request.contains("resultOffset=");
    if request.contains("/features/") {
        FEATURES.to_vec()
    } else if request.contains("/denied/") {
        DENIED.to_vec()
    } else if request.contains("/mixed/") && !next {
        MORE.to_vec()
    } else {
        binary()
    }
}

fn serve() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            let _ = reader.read_line(&mut request);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 && !line.trim().is_empty() {
                line.clear();
            }
            let _ = stream.write_all(&[b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".as_slice(), &answer(&request)].concat());
        }
    });
    base
}

#[test]
fn an_allow_list_of_esrijson_judges_each_page_by_what_it_holds() {
    set_allowed_drivers(Some(&["ESRIJSON"]), Some(Kind::Vector)).unwrap();
    let base = serve();
    let open = |path: &str| terrio::open_vector(&format!("{base}/{path}/query?f=pbf"), &Default::default());
    assert_eq!(open("features").unwrap().layers[0].len(), 1);
    let e = open("denied").unwrap_err().to_string();
    assert!(e.contains("Invalid token."), "{e}");
    for path in ["binary", "mixed"] {
        let e = open(path).unwrap_err().to_string();
        assert!(e.contains("ESRIPBF") && e.contains("not allowed"), "{path}: {e}");
    }
}
