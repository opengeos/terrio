//! Zarr stores read in place over HTTP: v2 and v3 stores served from a local port (chunks read
//! only for the windows asked, consolidated metadata, sharding with range requests, missing
//! chunks, float16, the `scale_offset` and `cast_value` codecs, GDAL's `ZARR:` names).
//!
//! The tests on public cloud stores are ignored by default: run them with
//! `cargo test --test remote_zarr -- --ignored`. Their expected values come from GDAL 3.12's Zarr
//! driver (`gdallocationinfo` through `/vsis3/` and `/vsigs/`).

#![cfg(not(target_arch = "wasm32"))]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use terrio::{DataType, Raster, RasterConfig, RasterDataset, RasterWriteOptions, Window, WindowOptions};

/// What a test server has sent: requests and body bytes.
#[derive(Clone, Default)]
struct Counter {
    requests: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
    /// While set, every request is answered 401 (as a server failing or denying access).
    fail: Arc<std::sync::atomic::AtomicBool>,
}

impl Counter {
    fn take(&self) -> (u64, u64) {
        (self.requests.swap(0, Ordering::SeqCst), self.bytes.swap(0, Ordering::SeqCst))
    }
}

/// Serves the files of `root` on a local port, with range requests (`a-b`, `a-` and `-n`) and
/// 404 for missing files. Returns the base URL and the counter.
fn serve(root: PathBuf) -> (String, Counter) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let counter = Counter::default();
    let c = counter.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let root = root.clone();
            let c = c.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                if reader.read_line(&mut first).unwrap_or(0) == 0 {
                    return;
                }
                let mut range: Option<(Option<u64>, Option<u64>)> = None;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("range: bytes=") {
                        let (a, b) = v.trim().split_once('-').unwrap();
                        range = Some((a.parse().ok(), b.parse().ok()));
                    }
                }
                c.requests.fetch_add(1, Ordering::SeqCst);
                if c.fail.load(Ordering::SeqCst) {
                    let _ = stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    return;
                }
                let path = first.split_whitespace().nth(1).unwrap_or("/").split('?').next().unwrap().trim_start_matches('/').to_string();
                let file = root.join(&path);
                let Ok(data) = std::fs::read(&file).map_err(|_| ()).and_then(|d| if file.is_file() { Ok(d) } else { Err(()) }) else {
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    return;
                };
                let len = data.len() as u64;
                let (status, a, b) = match range {
                    None => ("200 OK", 0, len.saturating_sub(1)),
                    Some((Some(a), b)) => ("206 Partial Content", a, b.unwrap_or(len - 1).min(len - 1)),
                    Some((None, Some(n))) => ("206 Partial Content", len.saturating_sub(n), len - 1),
                    Some((None, None)) => ("200 OK", 0, len.saturating_sub(1)),
                };
                let body = if len == 0 { Vec::new() } else { data[a as usize..=b as usize].to_vec() };
                c.bytes.fetch_add(body.len() as u64, Ordering::SeqCst);
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Range: bytes {a}-{b}/{len}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(&[head.into_bytes(), body].concat());
            });
        }
    });
    (url, counter)
}

/// A UInt16 raster of `cols` x `rows` x `bands` with distinct values, in EPSG:32633.
fn raster(cols: usize, rows: usize, bands: usize) -> RasterDataset {
    let cfg = RasterConfig {
        cols,
        rows,
        bands,
        x_min: 500000.0,
        y_min: 4600000.0 - rows as f64 * 10.0,
        cell_size: 10.0,
        nodata: 0.0,
        data_type: DataType::U16,
        crs: wbraster::CrsInfo::from_epsg(32633),
        ..Default::default()
    };
    let values: Vec<f64> = (0..cols * rows * bands).map(|i| (1 + i % 60000) as f64).collect();
    let data = wbraster::raster::RasterData::from_f64_vec(DataType::U16, values);
    RasterDataset::new(Raster::from_data_native(cfg, data).unwrap(), true)
}

fn write_zarr(dir: &Path, name: &str, ds: &RasterDataset, options: &[(&str, &str)]) -> PathBuf {
    let p = dir.join(name);
    let opts = RasterWriteOptions {
        creation_options: options.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        ..Default::default()
    };
    terrio::write_raster_dataset(ds, p.to_str().unwrap(), &opts).unwrap();
    p
}

#[test]
fn remote_stores_are_read_by_window() {
    let dir = tempfile::tempdir().unwrap();
    let ds = raster(700, 500, 3);
    // Chunks of one band and 256 x 256 pixels.
    write_zarr(dir.path(), "v2.zarr", &ds, &[]);
    write_zarr(dir.path(), "v3.zarr", &ds, &[("FORMAT", "ZARR_V3")]);
    let (url, counter) = serve(dir.path().to_path_buf());
    for name in ["v2.zarr", "v3.zarr"] {
        let local = terrio::open_raster(dir.path().join(name).to_str().unwrap()).unwrap();
        let remote = format!("{url}/{name}");
        let mut r = terrio::open_raster_lazy(&remote).unwrap();
        assert!(r.is_lazy());
        assert_eq!((r.width(), r.height(), r.band_count()), (700, 500, 3));
        assert_eq!(r.geo_transform(), local.geo_transform());
        assert_eq!(r.crs().epsg, Some(32633));
        counter.take();
        // A window inside one chunk of band 2: one chunk read.
        let w = r.read_window(Window::new(110, 20, 50, 40), &WindowOptions { bands: vec![2], overview: None }).unwrap();
        let (requests, bytes) = counter.take();
        assert_eq!(requests, 1, "{name}");
        assert!(bytes <= 256 * 256 * 2 + 1024, "{name}: {bytes} bytes");
        for (r0, c0) in [(0, 0), (39, 49), (17, 3)] {
            assert_eq!(w.raster.get(0, r0, c0), local.raster.get(1, 20 + r0, 110 + c0), "{name}");
        }
        // The whole raster, through the in-memory API too.
        let all = r.read().unwrap();
        assert_eq!(all.raster.data.to_f64_vec(), local.raster.data.to_f64_vec(), "{name}");
        let Ok(terrio::Dataset::Raster(whole)) = terrio::open(&remote) else { panic!("{name}") };
        assert_eq!(whole.raster.data.to_f64_vec(), local.raster.data.to_f64_vec(), "{name}");
        // GDAL's names.
        let gdal_name = format!("ZARR:\"/vsicurl/{url}/{name}\"");
        assert_eq!(terrio::open_raster_lazy(&gdal_name).unwrap().width(), 700);
    }
}

/// A metadata document that cannot be read is an error naming the failure (not "no Zarr
/// metadata"), and is not remembered as missing: the store opens once the server answers.
#[test]
fn metadata_read_failures_are_errors() {
    let dir = tempfile::tempdir().unwrap();
    write_zarr(dir.path(), "v2.zarr", &raster(30, 20, 1), &[]);
    let (url, counter) = serve(dir.path().to_path_buf());
    let remote = format!("{url}/v2.zarr");
    counter.fail.store(true, Ordering::SeqCst);
    let Err(err) = terrio::open_raster_lazy(&remote) else { panic!("the server fails") };
    let err = err.to_string();
    assert!(err.contains("401"), "{err}");
    counter.fail.store(false, Ordering::SeqCst);
    assert_eq!(terrio::open_raster_lazy(&remote).unwrap().width(), 30);
    // An array document that is not JSON (as a truncated or error body sent with 200) is
    // reported as such, not taken for a missing array.
    let store = dir.path().join("v2.zarr");
    let _ = std::fs::remove_file(store.join(".zmetadata"));
    // The data variable is named after the store.
    std::fs::write(store.join("v2/.zarray"), b"{\"zarr_format\": 2, \"shape\"").unwrap();
    let Err(err) = terrio::open_raster_lazy(&format!("{remote}/v2")) else { panic!("a broken .zarray") };
    let err = err.to_string();
    assert!(err.contains("not valid JSON"), "{err}");
}

/// Little-endian bytes of the CRC32C checksum (Castagnoli) of `data`.
fn crc32c(data: &[u8]) -> [u8; 4] {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0x82F6_3B78 } else { crc >> 1 };
        }
    }
    (!crc).to_le_bytes()
}

/// Writes a Zarr v3 array of `shape` with shards of `shard` and inner chunks of `inner`, the
/// inner chunks' `codecs` encoding the values that `encode` gives (bytes of one inner chunk, in C
/// order; `None` leaves the inner chunk out).
fn write_sharded(
    dir: &Path,
    shape: [usize; 2],
    shard: [usize; 2],
    inner: [usize; 2],
    meta: serde_json::Value,
    encode: impl Fn(usize, usize) -> Option<Vec<u8>>,
) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("zarr.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
    let grid = [shape[0].div_ceil(shard[0]), shape[1].div_ceil(shard[1])];
    let per = [shard[0] / inner[0], shard[1] / inner[1]];
    for sy in 0..grid[0] {
        for sx in 0..grid[1] {
            let mut body = Vec::new();
            let mut index = Vec::new();
            for iy in 0..per[0] {
                for ix in 0..per[1] {
                    match encode(sy * per[0] + iy, sx * per[1] + ix) {
                        Some(chunk) => {
                            index.extend((body.len() as u64).to_le_bytes());
                            index.extend((chunk.len() as u64).to_le_bytes());
                            body.extend(chunk);
                        }
                        None => index.extend([0xffu8; 16]),
                    }
                }
            }
            let crc = crc32c(&index);
            body.extend(index);
            body.extend(crc);
            std::fs::create_dir_all(dir.join(format!("c/{sy}"))).unwrap();
            std::fs::write(dir.join(format!("c/{sy}/{sx}")), body).unwrap();
        }
    }
}

#[test]
fn sharded_stores_read_only_the_inner_chunks_needed() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("s.zarr");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(store.join("zarr.json"), r#"{"zarr_format": 3, "node_type": "group", "attributes": {}}"#).unwrap();
    let (shape, shard, inner) = ([200usize, 300usize], [128usize, 128usize], [16usize, 16usize]);
    // UInt16 values row * 1000 + col, an inner chunk left out (fill value 7).
    let value = |r: usize, c: usize| (r * 1000 + c) as u16;
    let missing = (3, 5);
    let meta = serde_json::json!({
        "zarr_format": 3, "node_type": "array", "shape": shape, "data_type": "uint16",
        "chunk_grid": {"name": "regular", "configuration": {"chunk_shape": shard}},
        "chunk_key_encoding": {"name": "default", "configuration": {"separator": "/"}},
        "fill_value": 7, "dimension_names": ["y", "x"],
        "attributes": {"spatial:dimensions": ["y", "x"], "spatial:transform": [10.0, 0.0, 500000.0, 0.0, -10.0, 4600000.0], "proj:code": "EPSG:32633"},
        "codecs": [{"name": "sharding_indexed", "configuration": {
            "chunk_shape": inner,
            "codecs": [{"name": "bytes", "configuration": {"endian": "little"}}, {"name": "gzip", "configuration": {"level": 1}}],
            "index_codecs": [{"name": "bytes", "configuration": {"endian": "little"}}, {"name": "crc32c"}],
            "index_location": "end"}}]
    });
    write_sharded(&store.join("a"), shape, shard, inner, meta, |iy, ix| {
        if (iy, ix) == missing {
            return None;
        }
        let mut raw = Vec::new();
        for r in iy * 16..iy * 16 + 16 {
            for c in ix * 16..ix * 16 + 16 {
                raw.extend(value(r, c).to_le_bytes());
            }
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&raw).unwrap();
        Some(gz.finish().unwrap())
    });
    let (url, counter) = serve(dir.path().to_path_buf());
    let mut r = terrio::open_raster_lazy(&format!("ZARR:\"{url}/s.zarr\":/a")).unwrap();
    assert_eq!((r.width(), r.height()), (300, 200));
    assert_eq!(r.geo_transform(), [500000.0, 10.0, 0.0, 4600000.0, 0.0, -10.0]);
    assert_eq!(r.crs().epsg, Some(32633));
    counter.take();
    // A window over four inner chunks of one shard: the index, then one request for the chunks.
    let w = r.read_window(Window::new(40, 30, 20, 10), &WindowOptions::default()).unwrap();
    let (requests, bytes) = counter.take();
    assert_eq!(requests, 2);
    let shard_size = std::fs::metadata(store.join("a/c/0/0")).unwrap().len();
    assert!(bytes < shard_size / 4, "{bytes} of {shard_size}");
    assert_eq!(w.raster.get(0, 0, 0), value(30, 40) as f64);
    assert_eq!(w.raster.get(0, 9, 19), value(39, 59) as f64);
    // The whole array, the missing inner chunk filled.
    let all = r.read().unwrap();
    for (row, col) in [(0, 0), (199, 299), (150, 290), (48, 80), (63, 95)] {
        let expected = if (row / 16, col / 16) == missing { 7.0 } else { value(row, col) as f64 };
        assert_eq!(all.raster.get(0, row as isize, col as isize), expected, "{row} {col}");
    }
    // The same store read locally.
    let local = terrio::open_raster(store.join("a").to_str().unwrap()).unwrap();
    assert_eq!(local.raster.data.to_f64_vec(), all.raster.data.to_f64_vec());
}

#[test]
fn float16_and_value_codecs() {
    let dir = tempfile::tempdir().unwrap();
    // Zarr v2 float16, as NOAA's HRRR stores are: read as Float32.
    let a = dir.path().join("h.zarr/t");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::write(dir.path().join("h.zarr/.zgroup"), r#"{"zarr_format": 2}"#).unwrap();
    std::fs::write(
        a.join(".zarray"),
        r#"{"zarr_format": 2, "shape": [3, 4], "chunks": [2, 4], "dtype": "<f2", "compressor": null, "fill_value": -10000.0, "filters": null, "order": "C"}"#,
    )
    .unwrap();
    std::fs::write(a.join(".zattrs"), r#"{"_ARRAY_DIMENSIONS": ["y", "x"]}"#).unwrap();
    let halves: Vec<f32> = vec![292.75, -1.5, 0.0, 65504.0, 1.0, 2.0, 3.0, 4.0];
    let bytes: Vec<u8> = halves.iter().flat_map(|v| half::f16::from_f32(*v).to_le_bytes()).collect();
    std::fs::write(a.join("0.0"), bytes).unwrap();
    // The second chunk is missing: fill value.
    let ds = terrio::open_raster(a.to_str().unwrap()).unwrap();
    assert_eq!(ds.raster.data_type, DataType::F32);
    assert_eq!(&ds.raster.data_f32().unwrap()[..9], &[292.75, -1.5, 0.0, 65504.0, 1.0, 2.0, 3.0, 4.0, -10000.0]);

    // Zarr v3 scale_offset and cast_value, as the EOPF GeoZarr samples store reflectances.
    let b = dir.path().join("g.zarr/b");
    std::fs::create_dir_all(b.join("c/0")).unwrap();
    std::fs::write(dir.path().join("g.zarr/zarr.json"), r#"{"zarr_format": 3, "node_type": "group", "attributes": {}}"#).unwrap();
    let meta = serde_json::json!({
        "zarr_format": 3, "node_type": "array", "shape": [1, 4], "data_type": "float32",
        "chunk_grid": {"name": "regular", "configuration": {"chunk_shape": [1, 4]}},
        "chunk_key_encoding": {"name": "default", "configuration": {"separator": "/"}},
        "fill_value": "NaN", "dimension_names": ["y", "x"],
        "codecs": [
            {"name": "scale_offset", "configuration": {"offset": -0.1, "scale": 10000.0}},
            {"name": "cast_value", "configuration": {"data_type": "uint16", "scalar_map": {"encode": [["NaN", 0]], "decode": [[0, "NaN"]]}}},
            {"name": "bytes", "configuration": {"endian": "little"}}]
    });
    std::fs::write(b.join("zarr.json"), serde_json::to_vec(&meta).unwrap()).unwrap();
    let stored: Vec<u8> = [0u16, 1000, 4279, 65535].iter().flat_map(|v| v.to_le_bytes()).collect();
    std::fs::write(b.join("c/0/0"), stored).unwrap();
    let ds = terrio::open_raster(b.to_str().unwrap()).unwrap();
    let v = ds.raster.data_f32().unwrap();
    // Decoded in float32 as zarr-python decodes them: stored / scale + offset.
    assert!(v[0].is_nan());
    let expected = |s: u16| (s as f32) / 10000.0f32 + (-0.1f32);
    assert_eq!(&v[1..], &[expected(1000), expected(4279), expected(65535)]);
}

#[test]
#[ignore = "reads NOAA's HRRR Zarr store on AWS S3"]
fn hrrr_on_s3_as_gdal_reads_it() {
    // SAFETY: set before any thread reads the environment in this test binary's other tests; the
    // value only turns request signing off.
    unsafe { std::env::set_var("AWS_NO_SIGN_REQUEST", "YES") };
    let name = "ZARR:\"/vsis3/hrrrzarr/sfc/20240101/20240101_00z_anl.zarr/2m_above_ground/TMP/2m_above_ground/TMP\"";
    let mut r = terrio::open_raster_lazy(name).unwrap();
    assert_eq!((r.width(), r.height()), (1799, 1059));
    // GDAL: origin (-2699020.1425, -1588806.1526), pixel size (3000, 3000): south up.
    let gt = r.geo_transform();
    assert!((gt[0] + 2_699_020.142_521_93).abs() < 1e-6 && (gt[3] - 1588193.847443336).abs() < 1e-6, "{gt:?}");
    assert_eq!(r.nodata(), Some(-10000.0));
    // gdallocationinfo -valonly ... 900 500 → 272; 0 0 → 292.75 (rows counted from the south).
    let w = r.read_window(Window::new(900, 1058 - 500, 1, 1), &WindowOptions::default()).unwrap();
    assert_eq!(w.raster.get(0, 0, 0), 272.0);
    let w = r.read_window(Window::new(0, 1058, 1, 1), &WindowOptions::default()).unwrap();
    assert_eq!(w.raster.get(0, 0, 0), 292.75);
}

#[test]
#[ignore = "reads a CMIP6 Zarr store on Google Cloud Storage"]
fn cmip6_on_gcs_as_gdal_reads_it() {
    let name = "ZARR:\"gs://cmip6/CMIP6/CMIP/NOAA-GFDL/GFDL-CM4/historical/r1i1p1f1/Amon/tas/gr1/v20180701\":/tas";
    let mut r = terrio::open_raster_lazy(name).unwrap();
    assert_eq!((r.width(), r.height(), r.band_count()), (288, 180, 1980));
    assert_eq!(r.geo_transform(), [0.0, 1.25, 0.0, 90.0, 0.0, -1.0]);
    // gdallocationinfo -valonly -b 1980 ... 100 50 → 286.898071289062 (rows from the south).
    let w = r.read_window(Window::new(100, 179 - 50, 1, 1), &WindowOptions { bands: vec![1980], overview: None }).unwrap();
    assert_eq!(w.raster.get(0, 0, 0), 286.898_07_f32 as f64);
}
