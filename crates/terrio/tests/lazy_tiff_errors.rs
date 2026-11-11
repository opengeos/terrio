// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Lazy TIFF metadata must retain the cause of failed remote directory reads.

#[test]
fn malformed_uploaded_tiff_metadata_is_invalid_data() {
    for bytes in [b"II*\0".as_slice(), b"II*\0broken"] {
        let error = match terrio::LazyRaster::from_tiff_bytes(bytes.to_vec()) {
            Err(error) => error,
            Ok(_) => panic!("malformed metadata must fail"),
        };
        assert!(matches!(error, terrio::Error::InvalidData(_)), "{error:?}");
        assert!(error.to_string().contains("TIFF:"), "{error}");
    }
    // BigTIFF's declared directory count overflows its next-directory offset.
    let mut overflow = b"II+\0\x08\0\0\0".to_vec();
    overflow.extend(16u64.to_le_bytes());
    overflow.extend(u64::MAX.to_le_bytes());
    overflow.resize(24 + 4096 * 20, 0);
    let error = match terrio::LazyRaster::from_tiff_bytes(overflow.clone()) {
        Err(error) => error,
        Ok(_) => panic!("overflowed directory must fail"),
    };
    assert!(matches!(error, terrio::Error::InvalidData(_)), "{error:?}");
    assert!(error.to_string().contains("overflow"), "{error}");
    #[cfg(fs)]
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overflow.tif");
        std::fs::write(&path, overflow).unwrap();
        let error = match terrio::LazyRaster::open_tiff(path.to_str().unwrap()) {
            Err(error) => error,
            Ok(_) => panic!("overflowed file directory must fail"),
        };
        assert!(matches!(error, terrio::Error::InvalidData(_)), "{error:?}");
        assert!(error.to_string().contains("overflow"), "{error}");
    }
}

#[cfg(fs)]
#[test]
fn short_non_tiff_inputs_are_not_matches() {
    let dir = tempfile::tempdir().unwrap();
    for bytes in [b"".as_slice(), b"abc"] {
        let path = dir.path().join(format!("short-{}.tif", bytes.len()));
        std::fs::write(&path, bytes).unwrap();
        assert!(matches!(terrio::LazyRaster::open_tiff(path.to_str().unwrap()), Ok(None)));
        assert!(matches!(terrio::LazyRaster::from_tiff_bytes(bytes.to_vec()), Ok(None)));
    }
}

#[test]
#[cfg(remote)]
fn remote_directory_errors_keep_their_cause() {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    for (response, expected) in [
        ("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", Some("500")),
        ("", None),
        (
            "HTTP/1.1 302 Found\r\nLocation: http://localhost:1/elsewhere.tif\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            Some("a redirect to another origin"),
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/image.tif", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for request in 0..2 {
                let deadline = Instant::now() + Duration::from_secs(10);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        other => panic!("expected HTTP request {request}: {other:?}"),
                    }
                };
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
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
        if let Some(expected) = expected {
            assert!(error.to_string().contains(expected), "{error}");
        } else if let terrio::Error::Io(ref io) = error {
            assert!(
                matches!(io.get_ref().and_then(|source| source.downcast_ref::<terrio::Error>()), Some(terrio::Error::Http(_))),
                "{error:?}"
            );
        }
        server.join().unwrap();
    }
}
