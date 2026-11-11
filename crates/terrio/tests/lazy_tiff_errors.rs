// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Lazy TIFF metadata must retain the cause of failed remote directory reads.
#![cfg(remote)]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

#[test]
fn remote_directory_errors_keep_their_cause() {
    for (response, expected) in [
        ("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", "500"),
        ("", "disconnected"),
        (
            "HTTP/1.1 302 Found\r\nLocation: http://localhost:1/elsewhere.tif\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            "a redirect to another origin",
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/image.tif", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for request in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                }
                if request == 0 {
                    // The TIFF header points to a directory beyond the first HTTP block.
                    let mut body = vec![0; 16384];
                    body[..8].copy_from_slice(b"II\x2a\0\0\0\x08\0");
                    let header = "HTTP/1.1 206 Partial Content\r\nContent-Length: 16384\r\nContent-Range: bytes 0-16383/524562\r\nConnection: close\r\n\r\n";
                    stream.write_all(header.as_bytes()).unwrap();
                    stream.write_all(&body).unwrap();
                } else {
                    stream.write_all(response.as_bytes()).unwrap();
                }
            }
        });
        terrio::config::push_scope(
            [("TERRIO_HTTP_FOLLOW_CROSS_ORIGIN".into(), Some("NO".into())), ("TERRIO_HTTP_MAX_RETRY".into(), Some("0".into()))].into(),
        );
        let result = terrio::LazyRaster::open_tiff(&url);
        terrio::config::pop_scope();
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("directory request must fail"),
        };
        assert!(matches!(error, terrio::Error::Io(_)), "{error:?}");
        assert!(error.to_string().contains(expected), "{error}");
        server.join().unwrap();
    }
}
