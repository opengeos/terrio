// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! A feature-service query and a WFS GetFeature request under a process-wide allow-list: one test
//! in a binary of its own, because a list once set can only be narrowed (see `allow_list.rs`).

#![cfg(all(not(target_family = "wasm"), feature = "remote"))]

use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use terrio::formats::{Kind, set_allowed_drivers};

const PAGE: &str = r#"{"geometryType":"esriGeometryPoint","fields":[{"name":"A","type":"esriFieldTypeString"}],"features":[{"attributes":{"A":"x"},"geometry":{"x":1,"y":2}}]}"#;

/// A local service that answers every request with one page of ESRI JSON, and the number of
/// requests it has been sent.
fn serve() -> (String, Arc<AtomicUsize>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            counter.fetch_add(1, Ordering::SeqCst);
            while reader.read_line(&mut line).unwrap_or(0) > 0 && !line.trim().is_empty() {
                line.clear();
            }
            let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", PAGE.len());
            let _ = stream.write_all(&[head.as_bytes(), PAGE.as_bytes()].concat());
        }
    });
    (base, requests)
}

/// Only the ESRIJSON, ESRIPBF, GeoJSON and GML drivers read a query's answer as vector data. With
/// none of them allowed, a vector open refuses the URL before any request, and so does an open of
/// any kind once no raster driver is allowed either, as it reads a raster answer as a raster. With
/// ESRI JSON allowed, the URL is requested and read. Each URL is new, so that no answer comes from
/// a cache.
#[test]
fn a_query_no_allowed_driver_could_read_is_not_requested() {
    let (base, requests) = serve();
    let query = |name: &str| format!("{base}/{name}/FeatureServer/0/query?where=1%3D1&f=json");
    let wfs = |name: &str| format!("{base}/{name}/wfs?service=WFS&request=GetFeature&typeNames=x");
    let refused = |call: &str, url: &str, result: terrio::Result<()>| {
        let error = result.unwrap_err().to_string();
        assert!(error.contains("not allowed"), "{call}({url}): {error}");
    };
    set_allowed_drivers(Some(&["ESRIJSON"]), Some(Kind::Vector)).unwrap();
    let read = terrio::open_vector(&query("allowed"), &Default::default()).unwrap();
    assert_eq!(read.layers[0].features.len(), 1);
    assert!(requests.load(Ordering::SeqCst) > 0);
    let none: &[&str] = &[];
    set_allowed_drivers(Some(none), Some(Kind::Vector)).unwrap();
    let before = requests.load(Ordering::SeqCst);
    for url in [query("vector"), wfs("vector")] {
        refused("open_vector", &url, terrio::open_vector(&url, &Default::default()).map(|_| ()));
        refused("list_layers", &url, terrio::list_layers(&url).map(|_| ()));
    }
    assert_eq!(requests.load(Ordering::SeqCst), before, "requests of vector opens");
    let url = query("raster");
    refused("info_path", &url, terrio::info::info_path(&url, &Default::default()).map(|_| ()));
    assert!(requests.load(Ordering::SeqCst) > before, "an open of any kind requests it while the raster drivers are allowed");
    set_allowed_drivers(Some(none), Some(Kind::Raster)).unwrap();
    let before = requests.load(Ordering::SeqCst);
    for url in [query("none"), wfs("none")] {
        refused("open_vector", &url, terrio::open_vector(&url, &Default::default()).map(|_| ()));
        refused("open", &url, terrio::open(&url).map(|_| ()));
        refused("list_layers", &url, terrio::list_layers(&url).map(|_| ()));
        refused("info_path", &url, terrio::info::info_path(&url, &Default::default()).map(|_| ()));
    }
    assert_eq!(requests.load(Ordering::SeqCst), before, "requests sent while no driver could read the answer");
}
