// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Deterministic strip-heavy write workload. No external data or GDAL required.
use terrio::raster::RasterData;
use terrio::{DataType, Raster, RasterConfig, RasterDataset, RasterWriteOptions};
fn main() {
    let args: Vec<_> = std::env::args().collect();
    let dense = args.get(2).is_some_and(|s| s == "dense");
    let cols = 16384;
    let rows = 2048;
    let cfg = RasterConfig { cols, rows, bands: 1, data_type: DataType::F32, nodata: -9999., ..Default::default() };
    let mut state = 0x12345678u32;
    let data = (0..cols * rows)
        .map(|i| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            if !dense && (i / cols) % 23 < 17 { -9999. } else { (state % 10000) as f32 / 100. }
        })
        .collect();
    let ds = RasterDataset::new(Raster::from_data_native(cfg, RasterData::F32(data)).unwrap(), true);
    let o = RasterWriteOptions {
        compression: Some("LZW".into()),
        creation_options: vec![("BLOCKYSIZE".into(), "1".into())],
        ..Default::default()
    };
    terrio::write_raster_dataset(&ds, &args[1], &o).unwrap();
}
