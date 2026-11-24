// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Exact baseline file regressions, also run in fresh processes with one and three workers.
use terrio::raster::RasterData;
use terrio::{DataType, Raster, RasterConfig, RasterDataset, RasterWriteOptions};
#[test]
fn lzw_baseline_files() {
    let mut state = 37u32;
    let values = (0..37 * 19 * 2)
        .map(|i| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            match i % 31 {
                0 => f32::from_bits(0x7fc01234),
                1 => -0.,
                2 => f32::INFINITY,
                _ => f32::from_bits(state),
            }
        })
        .collect();
    let cfg = RasterConfig { cols: 37, rows: 19, bands: 2, data_type: DataType::F32, ..Default::default() };
    let ds = RasterDataset::new(Raster::from_data_native(cfg, RasterData::F32(values)).unwrap(), true);
    for cog in [false, true] {
        for tiled in [false, true] {
            for planar in [false, true] {
                for predictor in [1, 2, 3] {
                    for big in [false, true] {
                        let name = format!("lzw-{cog}-{tiled}-{planar}-{predictor}-{big}.tif");
                        let o = RasterWriteOptions {
                            driver: Some(if cog { terrio::formats::RasterDriver::Cog } else { terrio::formats::RasterDriver::GTiff }),
                            compression: Some("LZW".into()),
                            bigtiff: big,
                            creation_options: vec![
                                ("TILED".into(), if tiled { "YES" } else { "NO" }.into()),
                                ("BLOCKXSIZE".into(), "16".into()),
                                ("BLOCKYSIZE".into(), if tiled { "16" } else { "7" }.into()),
                                ("INTERLEAVE".into(), if planar { "BAND" } else { "PIXEL" }.into()),
                                ("PREDICTOR".into(), predictor.to_string()),
                            ],
                            ..Default::default()
                        };
                        let actual = terrio::memory::write_raster_bytes(&ds, "x.tif", &o).unwrap();
                        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lzw").join(name);
                        assert_eq!(actual, std::fs::read(&path).unwrap(), "{}", path.display());
                    }
                }
            }
        }
    }
}
#[test]
fn lzw_one_and_many_threads() {
    for n in ["1", "3"] {
        assert!(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "lzw_baseline_files"])
                .env("RAYON_NUM_THREADS", n)
                .status()
                .unwrap()
                .success()
        );
    }
}

#[test]
fn concurrent_writes_remain_byte_identical() {
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(lzw_baseline_files);
        }
    });
}
