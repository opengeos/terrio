// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Drawing a remote raster without overviews reads it a band of blocks at a time, not a sampled
//! row at a time: the requests and bytes it takes, from a local port.

#![cfg(all(not(target_family = "wasm"), feature = "remote"))]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Serves the files of `root` on a local port, with range requests. Returns the base URL, the
/// requests answered and the body bytes sent.
fn serve(root: PathBuf) -> (String, Arc<AtomicU64>, Arc<AtomicU64>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (requests, sent) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let (request_counter, byte_counter) = (requests.clone(), sent.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let (root, requests, sent) = (root.clone(), request_counter.clone(), byte_counter.clone());
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut first = String::new();
                    if reader.read_line(&mut first).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut range = None;
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                            break;
                        }
                        if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                            let (a, b) = v.trim().split_once('-').unwrap();
                            range = Some((a.parse::<u64>().unwrap(), b.parse::<u64>().ok()));
                        }
                    }
                    let name = first.split_whitespace().nth(1).unwrap_or("/").trim_start_matches('/').to_string();
                    let Ok(data) = std::fs::read(root.join(name)) else {
                        let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                        continue;
                    };
                    let len = data.len() as u64;
                    let (status, a, b) = match range {
                        Some((a, b)) => ("206 Partial Content", a, b.unwrap_or(len - 1).min(len - 1)),
                        None => ("200 OK", 0, len - 1),
                    };
                    let body = &data[a as usize..=b as usize];
                    requests.fetch_add(1, Ordering::SeqCst);
                    sent.fetch_add(body.len() as u64, Ordering::SeqCst);
                    let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Range: bytes {a}-{b}/{len}\r\n\r\n", body.len());
                    if stream.write_all(&[head.into_bytes(), body.to_vec()].concat()).is_err() {
                        return;
                    }
                }
            });
        }
    });
    (url, requests, sent)
}

#[test]
fn a_remote_raster_without_overviews_is_drawn_from_bands_of_blocks() {
    // 4000 x 4000 pixels (over the 4 Mpx read whole) of noise, uncompressed in tiles of 256:
    // 16 rows of tiles.
    let (cols, rows) = (4000, 4000);
    let values: Vec<f64> = (0..(rows * cols) as u64).map(|i| (i.wrapping_mul(2_654_435_761) >> 7 & 255) as f64).collect();
    let cfg = wbraster::RasterConfig { cols, rows, bands: 1, data_type: terrio::DataType::U8, ..Default::default() };
    let data = wbraster::raster::RasterData::from_f64_vec(terrio::DataType::U8, values);
    let ds = terrio::RasterDataset::new(wbraster::Raster::from_data_native(cfg, data).unwrap(), false);
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("big.tif");
    let options = terrio::RasterWriteOptions {
        creation_options: [("TILED", "YES"), ("BLOCKXSIZE", "256"), ("BLOCKYSIZE", "256")]
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        ..Default::default()
    };
    terrio::write_raster_dataset(&ds, local.to_str().unwrap(), &options).unwrap();
    let file_size = std::fs::metadata(&local).unwrap().len();
    let (base, requests, sent) = serve(dir.path().to_path_buf());

    let draw = terrio::ops::render::RenderOptions { width: Some(240), ..Default::default() };
    let mut remote = terrio::open_raster_lazy(&format!("{base}/big.tif")).unwrap();
    assert!(remote.overviews().is_empty());
    let (opened, opened_bytes) = (requests.swap(0, Ordering::SeqCst), sent.swap(0, Ordering::SeqCst));
    let image = terrio::ops::render::render_raster(&mut remote, &draw).unwrap();
    let (gets, bytes) = (requests.load(Ordering::SeqCst), sent.load(Ordering::SeqCst));
    eprintln!("open: {opened} GETs, {opened_bytes} bytes; draw: {gets} GETs, {bytes} bytes; file {file_size} bytes");
    // A request or so a row of tiles, each tile fetched once: not one a sampled row.
    assert!(gets <= 20, "{gets} GETs to draw 240 rows from 16 rows of tiles");
    assert!(bytes <= file_size, "{bytes} bytes fetched from a {file_size}-byte file");

    // The same picture as drawn from the local file.
    let mut local = terrio::open_raster_lazy(local.to_str().unwrap()).unwrap();
    let start = std::time::Instant::now();
    assert_eq!(terrio::ops::render::render_raster(&mut local, &draw).unwrap(), image);
    eprintln!("local draw: {:?}", start.elapsed());
}
