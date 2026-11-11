//! `terrio translate` and `terrio convert` with `--co ZLEVEL=` and `--co ZSTD_LEVEL=`: the level
//! reaches the PNG and GeoTIFF writers, changing the bytes written but not the pixels read back,
//! and a level out of range is refused.

use std::path::Path;
use std::process::{Command, Output};
use terrio::{DataType, Raster, RasterConfig, RasterDataset};

fn terrio(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_terrio")).current_dir(dir).args(args).output().unwrap()
}

/// Writes `src.tif`: three Byte bands of 128 x 96, a gradient with noise.
fn source(dir: &Path) -> Vec<f64> {
    let (cols, rows) = (128, 96);
    let mut x = 5u64;
    let values: Vec<f64> = (0..cols * rows * 3)
        .map(|i| {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            f64::from(((i % cols) * 2 + (i / cols % rows) + (x >> 62) as usize) as u8)
        })
        .collect();
    let config = RasterConfig { cols, rows, bands: 3, data_type: DataType::U8, ..Default::default() };
    let ds = RasterDataset::new(Raster::from_data(config, values.clone()).unwrap(), false);
    terrio::write_raster_dataset(&ds, dir.join("src.tif").to_str().unwrap(), &Default::default()).unwrap();
    values
}

#[test]
fn translate_and_convert_pass_the_compression_level_to_the_writer() {
    let dir = tempfile::tempdir().unwrap();
    let values = source(dir.path());
    // `convert` writes a GeoTIFF from a GeoTIFF by strips, and a PNG whole.
    for command in ["translate", "convert"] {
        // (output, creation options before the level, the level's option)
        for (out, extra, key) in [("out.png", &[][..], "ZLEVEL"), ("out.tif", &["--co", "COMPRESS=DEFLATE"][..], "ZLEVEL")] {
            let write = |level: Option<&str>| {
                let option = level.map(|l| format!("{key}={l}"));
                let mut args = vec![command, "src.tif", out];
                args.extend_from_slice(extra);
                if let Some(o) = &option {
                    args.extend(["--co", o.as_str()]);
                }
                let result = terrio(dir.path(), &args);
                assert!(result.status.success(), "{args:?}: {}", String::from_utf8_lossy(&result.stderr));
                let back = terrio::open_raster(dir.path().join(out).to_str().unwrap()).unwrap();
                assert_eq!(back.raster.data.to_f64_vec(), values, "{args:?}");
                std::fs::read(dir.path().join(out)).unwrap()
            };
            let (fastest, default, six, smallest) = (write(Some("1")), write(None), write(Some("6")), write(Some("9")));
            assert_eq!(default, six, "{command} {out}");
            assert_ne!(fastest, smallest, "{command} {out}");
            assert!(smallest.len() <= fastest.len(), "{command} {out}: {} > {}", smallest.len(), fastest.len());
        }
        for (out, option) in [("out.png", "ZLEVEL=10"), ("out.tif", "ZLEVEL=0"), ("out.tif", "ZSTD_LEVEL=23")] {
            let result = terrio(dir.path(), &[command, "src.tif", out, "--co", option]);
            let stderr = String::from_utf8_lossy(&result.stderr);
            let (key, value) = option.split_once('=').unwrap();
            assert!(!result.status.success() && stderr.contains(&format!("invalid {key} '{value}'")), "{command} {option}: {stderr}");
        }
    }
}
