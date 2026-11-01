// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Feature-service queries over HTTP, paged as GDAL 3.13.3's ESRIJSON driver pages them, from a
//! service on a local port.

#![cfg(all(not(target_family = "wasm"), feature = "remote"))]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use terrio::formats::VectorDriver;

const TOTAL: usize = 5;

fn page(offset: usize, count: usize) -> String {
    let end = (offset + count).min(TOTAL);
    let features: Vec<String> = (offset..end)
        .map(|i| format!(r#"{{"attributes":{{"OBJECTID":{},"NAME":"n{i}"}},"geometry":{{"x":{i},"y":{i}}}}}"#, i + 1))
        .collect();
    format!(
        r#"{{"objectIdFieldName":"OBJECTID","geometryType":"esriGeometryPoint","spatialReference":{{"wkid":4326}},
            "fields":[{{"name":"OBJECTID","type":"esriFieldTypeOID"}},{{"name":"NAME","type":"esriFieldTypeString"}}],
            "features":[{}],"exceededTransferLimit":{}}}"#,
        features.join(","),
        end < TOTAL
    )
}

/// A page like [`page`]'s without an object id field.
fn no_object_ids(offset: usize, count: usize) -> String {
    let end = (offset + count).min(TOTAL);
    let features: Vec<String> =
        (offset..end).map(|i| format!(r#"{{"attributes":{{"NAME":"n{i}"}},"geometry":{{"x":{i},"y":{i}}}}}"#)).collect();
    format!(
        r#"{{"geometryType":"esriGeometryPoint","fields":[{{"name":"NAME","type":"esriFieldTypeString"}}],"features":[{}],"exceededTransferLimit":{}}}"#,
        features.join(","),
        end < TOTAL
    )
}

/// The path of a query whose server ignores ranges.
const IGNORES_RANGES: &str = "/ignores-ranges/query";

const LARGE_FEATURES: usize = 4000;

/// A page larger than ten times the first range a lazy open asks for.
fn large_page() -> String {
    let features: Vec<String> = (0..LARGE_FEATURES)
        .map(|i| format!(r#"{{"attributes":{{"OBJECTID":{},"NAME":"{}"}},"geometry":{{"x":1,"y":2}}}}"#, i + 1, "n".repeat(50)))
        .collect();
    format!(
        r#"{{"objectIdFieldName":"OBJECTID","geometryType":"esriGeometryPoint","fields":[{{"name":"OBJECTID","type":"esriFieldTypeOID"}},{{"name":"NAME","type":"esriFieldTypeString"}}],"features":[{}]}}"#,
        features.join(",")
    )
}

const BIG_GEOJSON_BYTES: usize = 300_000;

fn big_geojson() -> String {
    format!(
        r#"{{"type":"FeatureCollection","features":[{{"type":"Feature","properties":{{"a":"{}"}},"geometry":{{"type":"Point","coordinates":[1,2]}}}}]}}"#,
        "x".repeat(BIG_GEOJSON_BYTES)
    )
}

/// A service of [`TOTAL`] features at `/FeatureServer/0/query`, at most two per page, with other
/// paths for the error cases. Returns its base URL and the requests it received.
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
            let mut range = None;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                    let (a, b) = v.trim().split_once('-').unwrap();
                    range = Some((a.parse::<usize>().unwrap(), b.parse::<usize>().ok()));
                }
            }
            let target = first.split_whitespace().nth(1).unwrap_or("/").to_string();
            record.lock().unwrap().push(target.clone());
            let param = |key: &str| {
                target.split_once('?').and_then(|(_, q)| {
                    q.split('&').find_map(|p| p.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v.parse::<usize>().unwrap()))
                })
            };
            let offset = param("resultOffset").unwrap_or(0);
            let count = param("resultRecordCount").unwrap_or(2).min(2);
            let path = target.split('?').next().unwrap_or("/");
            let body = match path {
                "/FeatureServer/0/query" => page(offset, count),
                "/stalls/query" => {
                    r#"{"geometryType":"esriGeometryPoint","fields":[{"name":"A","type":"esriFieldTypeString"}],
                        "features":[],"exceededTransferLimit":true}"#
                        .to_string()
                }
                "/fails/query" if offset > 0 => r#"{"error":{"code":500,"message":"Server is busy"}}"#.to_string(),
                "/fails/query" => page(offset, count),
                "/other/query" if offset > 0 => {
                    r#"{"geometryType":"esriGeometryPoint","fields":[{"name":"B","type":"esriFieldTypeString"}],
                        "features":[{"attributes":{"B":"x"},"geometry":{"x":1,"y":2}}]}"#
                        .to_string()
                }
                "/other/query" => page(offset, count),
                "/denied/query" => r#"{"error":{"code":498,"message":"Invalid token."}}"#.to_string(),
                "/repeats/query" => page(0, 2),
                "/noid/query" => no_object_ids(offset, count),
                "/emptylast/query" if offset > 0 => r#"{"features":[]}"#.to_string(),
                "/emptylast/query" => r#"{"features":[{"attributes":{"A":1},"geometry":{"x":1,"y":2}}],"exceededTransferLimit":true}"#.to_string(),
                "/FeatureServer/0" => {
                    r#"{"id":0,"type":"Feature Layer","geometryType":"esriGeometryPoint","fields":[{"name":"A","type":"esriFieldTypeString"}]}"#
                        .to_string()
                }
                IGNORES_RANGES => large_page(),
                "/ogc/big.geojson" => big_geojson(),
                _ => r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"a":1},"geometry":{"type":"Point","coordinates":[1,2]}}]}"#.to_string(),
            };
            let data = body.into_bytes();
            let len = data.len();
            if path == IGNORES_RANGES {
                // As ArcGIS Server answers: the whole answer, with neither its length nor a range.
                let _ = stream.write_all(&[b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".as_slice(), &data].concat());
                continue;
            }
            let (status, a, b) = match range {
                Some((a, b)) => ("206 Partial Content", a, b.unwrap_or(len - 1).min(len - 1)),
                None => ("200 OK", 0, len - 1),
            };
            let part = &data[a..=b];
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Range: bytes {a}-{b}/{len}\r\nConnection: close\r\n\r\n",
                part.len()
            );
            let _ = stream.write_all(&[head.as_bytes(), part].concat());
        }
    });
    (url, requests)
}

fn ids(ds: &terrio::VectorDataset) -> Vec<u64> {
    ds.layers[0].features.iter().map(|f| f.fid).collect()
}

fn offsets(requests: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    requests
        .lock()
        .unwrap()
        .iter()
        .map(|r| r.split_once('?').map_or("", |(_, q)| q).split('&').filter(|p| p.starts_with("result")).collect::<Vec<_>>().join("&"))
        .collect()
}

#[test]
fn a_query_is_read_page_by_page() {
    let (base, requests) = serve();
    let url = format!("{base}/FeatureServer/0/query?where=1%3D1&f=json");
    let ds = terrio::open_vector(&url, &Default::default()).unwrap();
    assert_eq!(ds.driver, VectorDriver::ESRIJSON);
    assert_eq!(ds.layers[0].name, "ESRIJSON");
    assert_eq!(ds.layers[0].crs_epsg(), Some(4326));
    assert_eq!(ids(&ds), [1, 2, 3, 4, 5]);
    // The first page's size becomes the page size, as in GDAL.
    assert_eq!(offsets(&requests), ["", "resultRecordCount=2&resultOffset=2", "resultRecordCount=2&resultOffset=4"]);

    let Ok(terrio::Dataset::Vector(any)) = terrio::open(&url) else { panic!("not a vector dataset") };
    assert_eq!(ids(&any), [1, 2, 3, 4, 5]);
}

#[test]
fn url_page_size_and_prefix_are_kept() {
    let (base, requests) = serve();
    let url = format!("ESRIJSON:{base}/FeatureServer/0/query?resultRecordCount=2");
    let ds = terrio::open_vector(&url, &Default::default()).unwrap();
    assert_eq!(ids(&ds), [1, 2, 3, 4, 5]);
    assert_eq!(offsets(&requests)[1], "resultRecordCount=2&resultOffset=2");
}

#[test]
fn an_offset_in_the_url_means_that_page_unless_paging_is_asked_for() {
    let (base, _) = serve();
    let url = format!("{base}/FeatureServer/0/query?f=json&resultOffset=1");
    assert_eq!(ids(&terrio::open_vector(&url, &Default::default()).unwrap()), [2, 3]);

    let mut options = terrio::VectorOpenOptions::default();
    assert!(options.set("FEATURE_SERVER_PAGING", "YES").unwrap());
    assert_eq!(ids(&terrio::open_vector(&url, &options).unwrap()), [2, 3, 4, 5]);

    let mut options = terrio::VectorOpenOptions::default();
    options.set("FEATURE_SERVER_PAGING", "NO").unwrap();
    let first = format!("{base}/FeatureServer/0/query?f=json");
    assert_eq!(ids(&terrio::open_vector(&first, &options).unwrap()), [1, 2]);
    assert!(options.set("FEATURE_SERVER_PAGING", "maybe").is_err());
}

#[test]
fn a_feature_range_stops_the_paging_early() {
    let (base, requests) = serve();
    let url = format!("{base}/FeatureServer/0/query?f=json");
    let options = terrio::VectorOpenOptions { skip_features: 1, max_features: Some(2), ..Default::default() };
    assert_eq!(ids(&terrio::open_vector(&url, &options).unwrap()), [2, 3]);
    assert_eq!(requests.lock().unwrap().len(), 2, "three features need two pages of two");
}

#[test]
fn listing_layers_reads_one_page() {
    let (base, requests) = serve();
    let url = format!("{base}/FeatureServer/0/query?f=json");
    assert_eq!(terrio::list_layers(&url).unwrap(), ["ESRIJSON"]);
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[test]
fn a_service_s_errors_and_stalls_are_errors() {
    let (base, _) = serve();
    let error = |path: &str| terrio::open_vector(&format!("{base}{path}"), &Default::default()).unwrap_err().to_string();
    assert!(error("/denied/query?f=json").contains("Invalid token."));
    assert!(error("/fails/query?f=json").contains("Server is busy"));
    assert!(error("/other/query?f=json").contains("other fields"));
    assert!(error("/stalls/query?f=json").contains("has none"));
    assert!(error("/repeats/query?f=json").contains("same as the one before"));
    assert!(error("/FeatureServer/0?f=json").contains("no features"));
}

#[test]
fn features_without_object_ids_are_numbered_across_pages() {
    let (base, _) = serve();
    let ds = terrio::open_vector(&format!("{base}/noid/query?f=json"), &Default::default()).unwrap();
    assert_eq!(ids(&ds), [0, 1, 2, 3, 4]);
}

#[test]
fn an_empty_last_page_without_fields_ends_the_reading() {
    let (base, _) = serve();
    let ds = terrio::open_vector(&format!("{base}/emptylast/query?f=json"), &Default::default()).unwrap();
    assert_eq!(ds.layers[0].features.len(), 1);
}

#[test]
fn a_server_that_ignores_ranges_is_read_and_described() {
    let (base, requests) = serve();
    let url = format!("{base}{IGNORES_RANGES}?f=json");
    assert!(large_page().len() > 160 * 1024, "larger than a lazy open accepts");
    assert_eq!(terrio::open_vector(&url, &Default::default()).unwrap().layers[0].features.len(), LARGE_FEATURES);
    assert_eq!(requests.lock().unwrap().len(), 1, "one request");

    let Ok(terrio::info::Info::Vector(info)) = terrio::info::info_path(&url, &Default::default()) else { panic!("not a vector dataset") };
    assert_eq!(info.driver, "ESRIJSON");
    assert_eq!(terrio::inspection::identify(&url, false).unwrap().unwrap().driver, "ESRIJSON");
}

#[test]
fn other_json_at_such_a_url_is_read_as_before() {
    let (base, _) = serve();
    let ds = terrio::open_vector(&format!("{base}/ogc/data.geojson?f=json"), &Default::default()).unwrap();
    assert_eq!(ds.driver, VectorDriver::GeoJSON);
    assert_eq!(ds.layers[0].features.len(), 1);
}

#[test]
fn other_json_at_such_a_url_is_not_downloaded_twice() {
    let (base, _) = serve();
    let ds = terrio::open_vector(&format!("{base}/ogc/big.geojson?f=json"), &Default::default()).unwrap();
    assert_eq!(ds.driver, VectorDriver::GeoJSON);
    let host = base.trim_start_matches("http://");
    let stats = terrio::http::fetch_stats_by_host();
    let (_, fetched) = stats.iter().find(|(h, _)| h == host).unwrap();
    let whole = big_geojson().len() as u64;
    assert!(fetched.bytes < whole + whole / 4, "{} bytes fetched of a document of {whole}", fetched.bytes);
}
