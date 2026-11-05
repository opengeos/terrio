// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! A local HTTP server with range requests that counts what it serves.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Serves the files of `root` on a local port, with range requests. Returns the base URL, the
/// requests answered and the body bytes sent.
pub fn serve(root: PathBuf) -> (String, Arc<AtomicU64>, Arc<AtomicU64>) {
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
