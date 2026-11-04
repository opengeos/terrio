// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Drawing a remote raster without overviews reads it a band of blocks at a time, not a sampled
//! row at a time: the requests and bytes it takes, from a local port.

#![cfg(all(not(target_family = "wasm"), feature = "remote"))]

#[path = "common/serve.rs"]
mod serve;
use serve::serve;
use std::sync::atomic::Ordering;

#[test]
fn a_remote_raster_without_overviews_is_drawn_from_bands_of_blocks() {
    // 4000 x 4000 pixels (over the 4 Mpx read whole) of noise, uncompressed in tiles of 256:
    // 16 rows of tiles.
    let (cols, rows) = (4000, 4000);
    let values: Vec<u8> = (0..(rows * cols) as u64).map(|i| ((i.wrapping_mul(2_654_435_761) >> 7) & 255) as u8).collect();
    let cfg = wbraster::RasterConfig { cols, rows, bands: 1, data_type: terrio::DataType::U8, ..Default::default() };
    let data = wbraster::raster::RasterData::U8(values);
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
    // A request or so a row of tiles, each tile fetched once: not one a sampled row. This bounds
    // the current behavior; it held before block reads too, as fetched bytes are cached. The
    // unit test `sampled_rows_are_read_a_row_of_blocks_at_a_time` guards the block grouping.
    assert!(gets <= 20, "{gets} GETs to draw 240 rows from 16 rows of tiles");
    assert!(bytes <= file_size, "{bytes} bytes fetched from a {file_size}-byte file");

    // The same picture as drawn from the local file.
    let mut local = terrio::open_raster_lazy(local.to_str().unwrap()).unwrap();
    let start = std::time::Instant::now();
    assert_eq!(terrio::ops::render::render_raster(&mut local, &draw).unwrap(), image);
    eprintln!("local draw: {:?}", start.elapsed());
}
