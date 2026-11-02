// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! A process-wide allow-list of ESRIPBF alone: one test in a binary of its own, because a list once
//! set cannot be lifted (see `allow_list.rs`). `.pbf` names both drivers, so what a file holds
//! decides.

use terrio::formats::{Kind, VectorDriver, set_allowed_drivers};

#[test]
fn an_allow_list_of_esripbf_opens_it_and_not_openstreetmap() {
    set_allowed_drivers(Some(&["ESRIPBF"]), Some(Kind::Vector)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let options = terrio::VectorOpenOptions::default();
    // A query result holding one field name and no features.
    let result = [&[(1u8 << 3) | 2, 1][..], b"A"].concat();
    let query = [&[(1u8 << 3) | 2, result.len() as u8][..], &result].concat();
    let page = [&[(2u8 << 3) | 2, query.len() as u8][..], &query].concat();
    let path = dir.path().join("a.pbf");
    std::fs::write(&path, &page).unwrap();
    assert_eq!(terrio::open_vector(path.to_str().unwrap(), &options).unwrap().driver, VectorDriver::ESRIPBF);

    let osm = b"\x00\x00\x00\x0d\x0a\x09OSMHeader\x18\x00";
    for name in ["b.pbf", "b.osm.pbf"] {
        let path = dir.path().join(name);
        std::fs::write(&path, osm).unwrap();
        let e = terrio::open_vector(path.to_str().unwrap(), &options).unwrap_err().to_string();
        assert!(e.contains("not allowed"), "{name}: {e}");
    }

    // OpenStreetMap data without its header block, which no content check names: the OpenStreetMap
    // reader would read it, so the list judges that reader.
    let block = [0x0a, 0x00]; // A primitive block with an empty string table.
    let blob = [&[0x0a, block.len() as u8][..], &block].concat(); // Its raw data.
    let header = [&[0x0a, 7][..], b"OSMData", &[0x18, blob.len() as u8]].concat();
    let data = [&(header.len() as u32).to_be_bytes()[..], &header, &blob].concat();
    let path = dir.path().join("c.pbf");
    std::fs::write(&path, &data).unwrap();
    let e = terrio::open_vector(path.to_str().unwrap(), &options).unwrap_err().to_string();
    assert!(e.contains("not allowed"), "{e}");

    // An answer in JSON: its features are ESRI JSON, which the list leaves out; a service's error is
    // still reported.
    let path = dir.path().join("d.json");
    let named = format!("ESRIPBF:{}", path.to_str().unwrap());
    std::fs::write(&path, br#"{"features":"invalid"}"#).unwrap();
    let e = terrio::open_vector(&named, &options).unwrap_err().to_string();
    assert!(e.contains("ESRIJSON") && e.contains("not allowed") && e.contains(path.to_str().unwrap()), "{e}");
    std::fs::write(&path, br#"{"unrelated":[1,2,3],"error":{"code":498,"message":"Invalid token.","details":[{"unrelated":[1,2,3]}]}}"#)
        .unwrap();
    let e = terrio::open_vector(&named, &options).unwrap_err().to_string();
    assert!(e.contains("Invalid token."), "{e}");
    #[cfg(feature = "remote")]
    {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for stream in listener.incoming().take(2) {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                reader.read_line(&mut request).unwrap();
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 && !line.trim().is_empty() {
                    line.clear();
                }
                let body = if request.contains("/features/") {
                    br#"{"features":"invalid"}"#.as_slice()
                } else {
                    br#"{"unrelated":[1,2],"error":{"code":498,"message":"Invalid token.","details":[{"nested":[1,2,3]}]}}"#.as_slice()
                };
                stream.write_all(&[b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".as_slice(), body].concat()).unwrap();
            }
        });
        let source = format!("{base}/features/query");
        let denied = terrio::open_vector(&format!("ESRIPBF:{source}?f=pbf&token=secret"), &options).unwrap_err().to_string();
        assert!(denied.contains("ESRIJSON") && denied.contains("not allowed") && denied.contains(&source), "{denied}");
        assert!(!denied.contains("secret"), "{denied}");
        let error = terrio::open_vector(&format!("ESRIPBF:{base}/error/query?f=pbf&token=secret"), &options).unwrap_err().to_string();
        assert!(error.contains("ESRIPBF") && error.contains("498") && error.contains("Invalid token."), "{error}");
        server.join().unwrap();
    }
}
