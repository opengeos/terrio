// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Extensionless GetFeature responses, using GDAL 3.13.3 as the format reference.
#![cfg(all(feature = "remote", not(target_family = "wasm")))]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

const GEOJSON: &[u8] = br#"{"type":"FeatureCollection","crs":{"type":"name","properties":{"name":"EPSG:25832"}},"features":[{"type":"Feature","properties":{"name":"road"},"geometry":{"type":"LineString","coordinates":[[700000,5100000],[700001,5100001]]}}]}"#;
const GML: &[u8] = br#"<wfs:FeatureCollection xmlns:wfs="http://www.opengis.net/wfs/2.0" xmlns:gml="http://www.opengis.net/gml/3.2" xmlns:app="urn:test"><wfs:member><app:road gml:id="road.1"><app:name>road</app:name><app:geom><gml:LineString srsName="urn:ogc:def:crs:EPSG::25832"><gml:posList srsDimension="2">700000 5100000 700001 5100001</gml:posList></gml:LineString></app:geom></app:road></wfs:member></wfs:FeatureCollection>"#;

fn serve(body: Vec<u8>) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests: Arc<Mutex<Vec<String>>> = Arc::default();
    let record = requests.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            if reader.read_line(&mut first).unwrap_or(0) == 0 {
                continue;
            }
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 && !line.trim().is_empty() {
                line.clear();
            }
            record.lock().unwrap().push(first.trim().to_string());
            let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
            let _ = stream.write_all(response.as_bytes());
            if !first.starts_with("HEAD ") {
                let _ = stream.write_all(&body);
            }
        }
    });
    (url, requests)
}

fn check_response(body: &'static [u8], extension: &str, expected: &str) {
    let (base, requests) = serve(body.to_vec());
    let named = terrio::open_vector(&format!("{base}/roads.{extension}"), &Default::default()).unwrap();
    assert_eq!(named.layers[0].features.len(), 1);
    let query = format!("{base}/ows?service=WFS&request=GetFeature&count=1");
    let ds = terrio::open_vector(&query, &Default::default()).unwrap();
    assert_eq!(ds.driver.name(), expected);
    assert_eq!(ds.layers[0].features.len(), 1);
    assert_eq!(ds.layers[0].crs_epsg(), Some(25832));
    assert!(matches!(terrio::open(&query), Ok(terrio::Dataset::Vector(_))));
    let terrio::info::Info::Vector(info) = terrio::info::info_path(&query, &Default::default()).unwrap() else { panic!("not vector") };
    assert_eq!(info.layers[0].feature_count, 1);
    assert_eq!(terrio::list_layers(&query).unwrap().len(), 1);
    let opaque = format!("{base}/data");
    assert_eq!(terrio::open_vector(&opaque, &Default::default()).unwrap().layers[0].features.len(), 1);
    assert!(matches!(terrio::info::info_path(&opaque, &Default::default()), Ok(terrio::info::Info::Vector(_))));
    let requests = requests.lock().unwrap();
    assert!(requests.iter().any(|request| request.contains("request=GetFeature&count=1")));
    // One named document, four streamed GetFeature opens, and one cached opaque
    // document. The final opaque info reuses that fetch: six is the I/O target.
    assert_eq!(requests.len(), 6, "{requests:?}");
}

#[test]
fn extensionless_geojson_response_opens_by_content() {
    check_response(GEOJSON, "geojson", "GeoJSON");
}

#[test]
fn extensionless_gml_response_opens_by_content() {
    check_response(GML, "gml", "GML");
}

#[test]
fn large_getfeature_responses_do_not_require_range_support() {
    let mut document: serde_json::Value = serde_json::from_slice(GEOJSON).unwrap();
    document["padding"] = serde_json::Value::String("x".repeat(200_000));
    let (base, requests) = serve(serde_json::to_vec(&document).unwrap());
    let url = format!("{base}/ows?SERVICE=WFS&REQUEST=Get%46eature");
    let ds = terrio::open_vector(&url, &Default::default()).unwrap();
    assert_eq!(ds.layers[0].features.len(), 1);
    assert_eq!(requests.lock().unwrap().len(), 1);
}

#[test]
fn service_responses_obey_the_actual_driver_allow_list() {
    const CHILD: &str = "TERRIO_VECTOR_RESPONSE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        for driver in ["GeoJSON", "GML"] {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "service_responses_obey_the_actual_driver_allow_list"])
                .env(CHILD, driver)
                .env("TERRIO_ALLOWED_DRIVERS", driver)
                .output()
                .unwrap();
            assert!(result.status.success(), "{}{}", String::from_utf8_lossy(&result.stdout), String::from_utf8_lossy(&result.stderr));
        }
        return;
    }
    let allowed = std::env::var(CHILD).unwrap();
    for (body, driver) in [(GEOJSON, "GeoJSON"), (GML, "GML")] {
        let (base, requests) = serve(body.to_vec());
        let result = terrio::open_vector(&format!("{base}/ows?SERVICE=WFS&REQUEST=Get%46eature"), &Default::default());
        if driver == allowed {
            assert_eq!(result.unwrap().layers[0].features.len(), 1);
        } else {
            let error = result.unwrap_err().to_string();
            assert!(error.contains("not allowed"), "{error}");
            assert_eq!(requests.lock().unwrap().len(), 1, "rejected at the identification probe");
        }
    }
}
