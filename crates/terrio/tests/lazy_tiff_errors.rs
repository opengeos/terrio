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
        #[cfg(fs)]
        {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("truncated.tif");
            std::fs::write(&path, bytes).unwrap();
            let error = match terrio::LazyRaster::open_tiff(path.to_str().unwrap()) {
                Err(error) => error,
                Ok(_) => panic!("truncated live metadata must fail"),
            };
            assert!(matches!(error, terrio::Error::Io(error) if error.kind() == std::io::ErrorKind::UnexpectedEof));
        }
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
fn inventory_tag_length_overflow_is_invalid_data() {
    let mut bytes = b"II+\0\x08\0\0\0".to_vec();
    bytes.extend(16u64.to_le_bytes());
    bytes.extend(1u64.to_le_bytes());
    bytes.extend(33550u16.to_le_bytes());
    bytes.extend(12u16.to_le_bytes());
    bytes.extend(u64::MAX.to_le_bytes());
    bytes.extend(0u64.to_le_bytes());
    bytes.extend(0u64.to_le_bytes());
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tag-overflow.tif");
    std::fs::write(&path, bytes).unwrap();
    let error = terrio::dataset_files::file_list(path.to_str().unwrap(), Some("GTiff")).unwrap_err();
    assert!(matches!(error, terrio::Error::InvalidData(_)), "{error:?}");
    assert!(error.to_string().contains("tag length overflow"), "{error}");
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
fn remote_metadata_errors_keep_their_cause() {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    let failures = [
        ("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", Some("500"), false),
        ("", None, false),
        (
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 0\r\nContent-Range: bytes 524288-524561/524562\r\nConnection: close\r\n\r\n",
            Some("unexpected response (status 206, 0 bytes)"),
            false,
        ),
        (
            "HTTP/1.1 302 Found\r\nLocation: http://localhost:1/elsewhere.tif\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            Some("a redirect to another origin"),
            false,
        ),
        ("HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n", Some("500"), true),
        ("", None, true),
        (
            "HTTP/1.1 206 Partial Content\r\nContent-Length: 0\r\nContent-Range: bytes 524288-524561/524562\r\nConnection: close\r\n\r\n",
            Some("unexpected response (status 206, 0 bytes)"),
            true,
        ),
        (
            "HTTP/1.1 302 Found\r\nLocation: http://localhost:1/elsewhere.tif\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            Some("a redirect to another origin"),
            true,
        ),
    ];
    for (decoder_setup, (response, expected, translate)) in
        [false, true].into_iter().flat_map(|decoder_setup| failures.into_iter().map(move |failure| (decoder_setup, failure)))
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/image.tif", listener.local_addr().unwrap());
        let finished = Arc::new(AtomicBool::new(false));
        let client_finished = finished.clone();
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
                    // Fail either the directory walk or the JPEG tables loaded by Decoder::new.
                    let mut body = vec![0; 16384];
                    if decoder_setup {
                        body[..8].copy_from_slice(b"II\x2a\0\x08\0\0\0");
                        let entries = [
                            (256u16, 4u16, 1u32, 1u32),
                            (257, 4, 1, 1),
                            (258, 3, 1, 8),
                            (259, 3, 1, 7),
                            (262, 3, 1, 1),
                            (273, 4, 1, 1024),
                            (277, 3, 1, 1),
                            (278, 4, 1, 1),
                            (279, 4, 1, 8),
                            (347, 7, 8, 524288),
                        ];
                        body[8..10].copy_from_slice(&(entries.len() as u16).to_le_bytes());
                        for (i, (tag, typ, count, value)) in entries.into_iter().enumerate() {
                            let at = 10 + i * 12;
                            body[at..at + 2].copy_from_slice(&tag.to_le_bytes());
                            body[at + 2..at + 4].copy_from_slice(&typ.to_le_bytes());
                            body[at + 4..at + 8].copy_from_slice(&count.to_le_bytes());
                            body[at + 8..at + 12].copy_from_slice(&value.to_le_bytes());
                        }
                    } else {
                        body[..8].copy_from_slice(b"II\x2a\0\0\0\x08\0");
                    }
                    let header = "HTTP/1.1 206 Partial Content\r\nContent-Length: 16384\r\nContent-Range: bytes 0-16383/524562\r\nConnection: close\r\n\r\n";
                    stream.write_all(header.as_bytes()).unwrap();
                    stream.write_all(&body).unwrap();
                } else {
                    stream.write_all(response.as_bytes()).unwrap();
                }
            }
            // Count all requests until the caller returns, including a whole-object fallback.
            let deadline = Instant::now() + Duration::from_secs(10);
            while !client_finished.load(Ordering::Relaxed) && Instant::now() < deadline {
                match listener.accept() {
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
                    other => panic!("unexpected request after the directory failure: {other:?}"),
                }
            }
            assert!(client_finished.load(Ordering::Relaxed), "client did not finish after the directory failure");
        });
        terrio::config::push_scope(
            [("TERRIO_HTTP_FOLLOW_CROSS_ORIGIN".into(), Some("NO".into())), ("TERRIO_HTTP_MAX_RETRY".into(), Some("0".into()))].into(),
        );
        let result = if translate {
            terrio::ops::translate::translate_path(
                &url,
                &terrio::ops::translate::TranslateOptions::default(),
                terrio::ops::overview::OverviewSelection::Auto,
            )
            .map(|_| ())
        } else {
            terrio::LazyRaster::open_tiff(&url).map(|_| ())
        };
        finished.store(true, Ordering::Relaxed);
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
