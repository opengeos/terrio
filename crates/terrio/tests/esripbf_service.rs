// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Feature-service queries over HTTP as protocol buffers, from a service on a local port that
//! ignores ranges, as ArcGIS Server does.

#![cfg(all(not(target_family = "wasm"), feature = "remote"))]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use terrio::formats::VectorDriver;

const TOTAL: u64 = 5;

fn varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn number(field: u64, v: u64) -> Vec<u8> {
    let mut out = Vec::new();
    varint(&mut out, field << 3);
    varint(&mut out, v);
    out
}

fn message(field: u64, bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    varint(&mut out, (field << 3) | 2);
    varint(&mut out, bytes.len() as u64);
    out.extend_from_slice(bytes);
    out
}

/// A page of point features with ids from `offset + 1`, at most two of them.
fn page(offset: u64) -> Vec<u8> {
    let end = (offset + 2).min(TOTAL);
    let field = message(13, &[message(1, b"OBJECTID"), number(2, 6)].concat());
    let srs = message(8, &number(1, 4326));
    let mut result = [number(9, u64::from(end < TOTAL)), srs, field].concat();
    for i in offset..end {
        let mut coords = Vec::new();
        varint(&mut coords, i * 2);
        varint(&mut coords, i * 2);
        let feature = [message(1, &number(5, i + 1)), message(2, &message(3, &coords))].concat();
        result.extend(message(15, &feature));
    }
    message(2, &message(1, &result))
}

/// A service of [`TOTAL`] features at `/query`, with other paths for the other answers. Returns its
/// base URL and the requests it received.
fn serve() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests: Arc<Mutex<Vec<String>>> = Arc::default();
    let record = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            if reader.read_line(&mut first).unwrap_or(0) == 0 {
                continue;
            }
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 && !line.trim().is_empty() {
                line.clear();
            }
            let target = first.split_whitespace().nth(1).unwrap_or("/").to_string();
            record.lock().unwrap().push(target.clone());
            let offset = target
                .split_once('?')
                .and_then(|(_, q)| q.split('&').find_map(|p| p.strip_prefix("resultOffset=")))
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let body: Vec<u8> = match target.split('?').next().unwrap_or("/") {
                "/query" => page(offset),
                "/refused/query" => Vec::new(),
                "/denied/query" => br#"{"error":{"code":498,"message":"Invalid token."}}"#.to_vec(),
                "/count/query" => message(2, &message(2, &number(1, 5))),
                _ => b"not found".to_vec(),
            };
            let _ = stream.write_all(&[b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".as_slice(), &body].concat());
        }
    });
    (url, requests)
}

fn ids(ds: &terrio::VectorDataset) -> Vec<u64> {
    ds.layers[0].features.iter().map(|f| f.fid).collect()
}

#[test]
fn a_query_is_read_page_by_page() {
    let (base, requests) = serve();
    let url = format!("{base}/query?where=1%3D1&f=pbf");
    let ds = terrio::open_vector(&url, &Default::default()).unwrap();
    assert_eq!(ds.driver, VectorDriver::ESRIPBF);
    assert_eq!(ds.layers[0].name, "ESRIPBF");
    assert_eq!(ds.layers[0].crs_epsg(), Some(4326));
    assert_eq!(ids(&ds), [1, 2, 3, 4, 5]);
    let queries = requests.lock().unwrap().clone();
    assert_eq!(queries.len(), 3);
    assert!(queries.iter().all(|q| q.contains("f=pbf")), "{queries:?}");
    assert!(queries[1].contains("resultRecordCount=2") && queries[1].contains("resultOffset=2"), "{queries:?}");
}

#[test]
fn the_prefix_opens_and_the_paging_option_applies() {
    let (base, _) = serve();
    assert_eq!(ids(&terrio::open_vector(&format!("ESRIPBF:{base}/query"), &Default::default()).unwrap()), [1, 2, 3, 4, 5]);
    let mut options = terrio::VectorOpenOptions::default();
    options.set("FEATURE_SERVER_PAGING", "NO").unwrap();
    assert_eq!(ids(&terrio::open_vector(&format!("{base}/query?f=pbf"), &options).unwrap()), [1, 2]);
}

#[test]
fn it_is_opened_described_listed_and_identified() {
    let (base, _) = serve();
    let url = format!("{base}/query?f=pbf");
    assert!(matches!(terrio::open(&url), Ok(terrio::Dataset::Vector(ds)) if ds.driver == VectorDriver::ESRIPBF));
    let Ok(terrio::info::Info::Vector(info)) = terrio::info::info_path(&url, &Default::default()) else { panic!("not a vector dataset") };
    assert_eq!(info.driver, "ESRIPBF");
    assert_eq!(terrio::list_layers(&url).unwrap(), ["ESRIPBF"]);
    assert_eq!(terrio::inspection::identify(&url, false).unwrap().unwrap().driver, "ESRIPBF");
}

#[test]
fn a_refused_query_says_so() {
    let (base, _) = serve();
    let error = |path: &str| terrio::open_vector(&format!("{base}{path}"), &Default::default()).unwrap_err().to_string();
    assert!(error("/refused/query?f=pbf").contains("f=json shows why"));
    assert!(error("/denied/query?f=pbf").contains("Invalid token."));
}

#[test]
fn a_query_that_answers_no_features_is_not_read_as_a_layer() {
    let (base, _) = serve();
    assert!(terrio::open_vector(&format!("{base}/count/query?returnCountOnly=true&f=pbf"), &Default::default()).is_err());
    let named = terrio::open_vector(&format!("ESRIPBF:{base}/count/query?returnCountOnly=true&f=pbf"), &Default::default());
    assert!(named.unwrap_err().to_string().contains("count"));
}
