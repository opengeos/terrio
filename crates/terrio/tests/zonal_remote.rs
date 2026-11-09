// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Zonal statistics over a remote COG read only the blocks under zones, once per strip for all
//! requested bands: the requests and bytes it takes, from a local port.

#![cfg(all(not(target_family = "wasm"), feature = "remote"))]

#[path = "common/serve.rs"]
mod serve;
use serve::serve;
use std::sync::atomic::Ordering;
use terrio::ops::zonal::{Statistic, ZonalBand, ZonalOptions, ZonalSource, zonal_stats};
use terrio::vector::Coord;
use terrio::{DataType, Feature, FieldValue, Geometry, Layer, Raster, RasterConfig, RasterDataset, Window, WindowOptions};

/// A 4-band 2048 × 2048 COG of noise in 256-pixel tiles, and its size.
fn cog(dir: &std::path::Path) -> u64 {
    let (cols, rows, bands) = (2048, 2048, 4);
    let values: Vec<f64> = (0..(cols * rows * bands) as u64).map(|i| ((i.wrapping_mul(2_654_435_761) >> 9) % 4000) as f64).collect();
    let cfg = RasterConfig { cols, rows, bands, data_type: DataType::U16, ..Default::default() };
    let mut ds = RasterDataset::new(Raster::from_data(cfg, values).unwrap(), false);
    ds.set_geo_transform([0.0, 1.0, 0.0, rows as f64, 0.0, -1.0]);
    let options = terrio::RasterWriteOptions {
        driver: Some(terrio::formats::RasterDriver::Cog),
        tile_size: Some(256),
        compression: Some("DEFLATE".into()),
        ..Default::default()
    };
    let path = dir.join("cog.tif");
    terrio::write_raster_dataset(&ds, path.to_str().unwrap(), &options).unwrap();
    std::fs::metadata(path).unwrap().len()
}

fn rectangle(x0: f64, y0: f64, x1: f64, y1: f64) -> Geometry {
    Geometry::polygon(vec![Coord::xy(x0, y0), Coord::xy(x1, y0), Coord::xy(x1, y1), Coord::xy(x0, y1), Coord::xy(x0, y0)], vec![])
}

fn zones(geometries: Vec<Geometry>) -> Layer {
    let mut l = Layer::new("zones");
    for (i, g) in geometries.into_iter().enumerate() {
        l.features.push(Feature { fid: i as u64, geometry: Some(g), attributes: vec![] });
    }
    l
}

fn source(url: &str, bands: &[usize]) -> ZonalSource {
    source_of(url, "cog.tif", bands)
}

fn source_of(url: &str, file: &str, bands: &[usize]) -> ZonalSource {
    ZonalSource {
        raster: terrio::open_raster_lazy(&format!("{url}/{file}")).unwrap().into(),
        bands: bands.iter().map(|&b| ZonalBand::new(b, format!("b{b}"))).collect(),
    }
}

#[test]
fn zonal_reads_each_strip_once_per_source() {
    let dir = tempfile::tempdir().unwrap();
    cog(dir.path());
    let (url, requests, sent) = serve(dir.path().to_path_buf());
    // The top-left block: rows 0–255, columns 0–255.
    let l = zones(vec![rectangle(0., 1792., 256., 2048.)]);
    let o = ZonalOptions { stats: vec![Statistic::Mean], ..Default::default() };
    let mut direct = terrio::open_raster_lazy(&format!("{url}/cog.tif")).unwrap();
    direct.read_window(Window::new(0, 0, 256, 256), &WindowOptions { bands: vec![1, 2, 3, 4], overview: None }).unwrap();
    let (direct_requests, direct_bytes) = (requests.swap(0, Ordering::SeqCst), sent.swap(0, Ordering::SeqCst));
    let out = zonal_stats(&l, &mut [source(&url, &[1, 2, 3, 4])], &o).unwrap();
    assert!(out.features[0].attributes.iter().all(|v| matches!(v, FieldValue::Float(_))));
    let (zonal_requests, zonal_bytes) = (requests.swap(0, Ordering::SeqCst), sent.swap(0, Ordering::SeqCst));
    assert!(zonal_bytes <= direct_bytes * 11 / 10, "zonal {zonal_bytes} bytes, direct read {direct_bytes}");
    assert!(zonal_requests <= direct_requests + 1, "zonal {zonal_requests} requests, direct read {direct_requests}");
}

#[test]
fn zonal_skips_strips_without_zones_and_keeps_empty_zone_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let size = cog(dir.path());
    let (url, _, sent) = serve(dir.path().to_path_buf());
    let l = zones(vec![rectangle(3., 1800., 200., 2040.), rectangle(1900., 10., 2040., 200.), rectangle(5000., 5000., 5010., 5010.)]);
    let stats = ["count", "mean", "variety", "quantile_50"].iter().map(|s| Statistic::parse(s).unwrap()).collect();
    let o = ZonalOptions { stats, ..Default::default() };
    let out = zonal_stats(&l, &mut [source(&url, &[1, 2, 3, 4])], &o).unwrap();
    let bytes = sent.load(Ordering::SeqCst);
    assert!(bytes < size / 8, "{bytes} of {size} bytes read");
    let outside = &out.features[2].attributes;
    for b in 0..4 {
        assert_eq!(outside[b * 4..b * 4 + 4], [FieldValue::Integer(0), FieldValue::Null, FieldValue::Integer(0), FieldValue::Null]);
    }
    assert!(matches!(out.features[0].attributes[1], FieldValue::Float(_)));
    assert!(matches!(out.features[1].attributes[1], FieldValue::Float(_)));
}

#[test]
fn zonal_reads_only_strips_with_zone_pixels_and_no_source_without_bands() {
    let dir = tempfile::tempdir().unwrap();
    let size = cog(dir.path());
    std::fs::copy(dir.path().join("cog.tif"), dir.path().join("unused.tif")).unwrap();
    let (url, _, sent) = serve(dir.path().to_path_buf());
    let ring = |x0, y0, x1, y1| vec![Coord::xy(x0, y0), Coord::xy(x1, y0), Coord::xy(x1, y1), Coord::xy(x0, y1), Coord::xy(x0, y0)];
    // One zone of two parts, in the top-left and bottom-left blocks: the six blocks between them
    // are not read, nor anything of a source without bands.
    let l = zones(vec![Geometry::multi_polygon(vec![(ring(3., 1800., 200., 2040.), vec![]), (ring(3., 10., 200., 200.), vec![])])]);
    let mut sources = [source_of(&url, "unused.tif", &[]), source(&url, &[1, 2, 3, 4])];
    sent.store(0, Ordering::SeqCst);
    let o = ZonalOptions { stats: vec![Statistic::Count], ..Default::default() };
    let out = zonal_stats(&l, &mut sources, &o).unwrap();
    let bytes = sent.load(Ordering::SeqCst);
    // Two of the 64 blocks, with the rounding of HTTP reads: the gap would add six, the unused source two.
    assert!(bytes < size * 4 / 64, "{bytes} of {size} bytes read");
    assert!(matches!(out.features[0].attributes[0], FieldValue::Integer(n) if n > 0));
}
