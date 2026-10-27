//! Raw-layout rasters (SRTM tiles, EHdr, ENVI and SAGA grids...) read by window: a window reads
//! only the rows it covers, and a summary reads no pixels. Remote SRTM tiles are read with range
//! requests, from a local port.

#![cfg(all(not(target_family = "wasm"), feature = "remote"))]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use terrio::{Window, WindowOptions};

/// Serves the files of `root` on a local port, with range requests (`a-b`). Returns the base URL
/// and the body bytes sent.
fn serve(root: PathBuf) -> (String, Arc<AtomicU64>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let sent = Arc::new(AtomicU64::new(0));
    let counter = sent.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let (root, counter) = (root.clone(), counter.clone());
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
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
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    return;
                };
                let len = data.len() as u64;
                let (status, a, b) = match range {
                    Some((a, b)) => ("206 Partial Content", a, b.unwrap_or(len - 1).min(len - 1)),
                    None => ("200 OK", 0, len - 1),
                };
                let body = data[a as usize..=b as usize].to_vec();
                counter.fetch_add(body.len() as u64, Ordering::SeqCst);
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Range: bytes {a}-{b}/{len}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&[head.into_bytes(), body].concat());
            });
        }
    });
    (url, sent)
}

/// A 1" tile (3601 x 3601 Int16 samples, 25.9 MB) whose samples are their row minus their column.
fn tile_bytes() -> Vec<u8> {
    let mut b = Vec::with_capacity(3601 * 3601 * 2);
    for row in 0..3601i32 {
        for col in 0..3601i32 {
            b.extend(((row - col) as i16).to_be_bytes());
        }
    }
    b
}

#[test]
fn remote_tiles_are_read_by_range() {
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("N37W120.hgt");
    std::fs::write(&local, tile_bytes()).unwrap();
    let whole = terrio::open_raster(local.to_str().unwrap()).unwrap();
    let (base, sent) = serve(dir.path().to_path_buf());
    let url = format!("{base}/N37W120.hgt");

    // A summary fetches the first block, not the tile.
    let terrio::info::Info::Raster(info) = terrio::info::info_path(&url, &Default::default()).unwrap() else { panic!() };
    assert_eq!((info.driver.as_str(), info.size, info.bands[0].nodata), ("SRTMHGT", [3601, 3601], Some(-32768.0)));
    assert_eq!(info.geo_transform, whole.geo_transform());
    let summary = sent.swap(0, Ordering::SeqCst);
    assert!(summary <= 64 * 1024, "{summary} bytes for a summary");

    // A window of 20 rows fetches the blocks those rows are in (each row is 7,202 bytes).
    let mut lazy = terrio::open_raster_lazy(&url).unwrap();
    let window = lazy.read_window(Window::new(1000, 2000, 300, 20), &WindowOptions::default()).unwrap();
    let fetched = sent.swap(0, Ordering::SeqCst);
    assert!(fetched <= 4 * 64 * 1024, "{fetched} bytes for a window");
    for row in 0..20 {
        for col in 0..300 {
            assert_eq!(window.raster.get(0, row, col), whole.raster.get(0, 2000 + row, 1000 + col));
        }
    }
    assert_eq!(window.nodata(), Some(-32768.0));

    // Read whole by range, as read eagerly.
    let all = lazy.read().unwrap();
    assert_eq!(all.raster.data.to_f64_vec(), whole.raster.data.to_f64_vec());
    assert_eq!(all.geo_transform(), whole.geo_transform());

    // Files named as tiles that are not tiles are left to the other drivers.
    std::fs::write(dir.path().join("N38W120.hgt"), [0u8; 100]).unwrap();
    assert!(terrio::open_raster_lazy(&format!("{base}/N38W120.hgt")).is_err());
}

#[test]
fn labelled_grids_are_read_by_window() {
    // EHdr, ENVI, SAGA, ERS, Idrisi and Surfer grids written by terrio, read back by window.
    let cfg = wbraster::RasterConfig {
        cols: 50,
        rows: 40,
        bands: 1,
        x_min: 500000.0,
        y_min: 4100000.0,
        cell_size: 30.0,
        nodata: -9999.0,
        data_type: terrio::DataType::F32,
        crs: wbraster::CrsInfo::from_epsg(32611),
        ..Default::default()
    };
    let values: Vec<f64> = (0..2000).map(|i| if i == 7 { -9999.0 } else { i as f64 * 0.5 }).collect();
    let data = wbraster::raster::RasterData::from_f64_vec(terrio::DataType::F32, values);
    let ds = terrio::RasterDataset::new(wbraster::Raster::from_data_native(cfg, data).unwrap(), true);
    let dir = tempfile::tempdir().unwrap();
    for (name, driver) in [("g.bil", "EHdr"), ("g.envi", "ENVI"), ("g.sdat", "SAGA"), ("g.ers", "ERS"), ("g.rst", "RST"), ("g.grd", "GSBG")]
    {
        let path = dir.path().join(name);
        let path = path.to_str().unwrap();
        terrio::write_raster_dataset(&ds, path, &Default::default()).unwrap();
        let whole = terrio::open_raster(path).unwrap();
        let mut lazy = terrio::open_raster_lazy(path).unwrap();
        assert_eq!(lazy.driver().name(), driver);
        assert!(lazy.dataset().is_none(), "{name} is read whole");
        let window = lazy.read_window(Window::new(10, 5, 20, 7), &WindowOptions::default()).unwrap();
        for row in 0..7 {
            for col in 0..20 {
                assert_eq!(window.raster.get(0, row, col).to_bits(), whole.raster.get(0, 5 + row, 10 + col).to_bits(), "{name}");
            }
        }
        assert_eq!((window.nodata(), window.raster.data_type), (whole.nodata(), whole.raster.data_type), "{name}");
        let header = lazy.header();
        assert_eq!(header.geo_transform(), whole.geo_transform(), "{name}");
        assert_eq!(header.raster.crs, whole.raster.crs, "{name}");
        assert_eq!(header.raster.metadata, whole.raster.metadata, "{name}");
        assert_eq!(lazy.read().unwrap().raster.metadata, whole.raster.metadata, "{name}");
        assert_eq!(lazy.shared().read().unwrap().raster.metadata, whole.raster.metadata, "{name}");
        // The files read, as GDAL lists them: the data file first, then its header.
        assert!(lazy.file_list().unwrap().iter().any(|f| f.ends_with(name) || f.ends_with(&name.replace(".rst", ".rdc"))), "{name}");
        let summary = terrio::info::info_path(path, &terrio::info::InfoOptions { statistics: true, ..Default::default() }).unwrap();
        let eager =
            terrio::info::info(&terrio::Dataset::Raster(whole), &terrio::info::InfoOptions { statistics: true, ..Default::default() });
        assert_eq!(serde_json::to_value(&summary).unwrap(), serde_json::to_value(&eager).unwrap(), "{name}");
    }
}
