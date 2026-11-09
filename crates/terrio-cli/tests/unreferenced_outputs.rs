//! `terrio translate`, `convert` and `pipeline` write a source without a geotransform (an
//! unreferenced GeoTIFF) without one: no GeoJP2 box, no world file and no GeoTIFF tie point,
//! unless the command assigns bounds.

use std::path::Path;
use std::process::Command;
use terrio::{DataType, Raster, RasterConfig, RasterDataset, RasterWriteOptions};

/// The UUID of a GeoJP2 box.
const GEOJP2: [u8; 16] = [0xb1, 0x4b, 0xf8, 0xbd, 0x08, 0x3d, 0x4b, 0x43, 0xa5, 0xae, 0x8c, 0xd7, 0xd5, 0xa6, 0xce, 0x03];

fn run(dir: &Path, args: &[&str]) {
    let result = Command::new(env!("CARGO_BIN_EXE_terrio")).current_dir(dir).args(args).output().unwrap();
    assert!(result.status.success(), "{args:?}: {}", String::from_utf8_lossy(&result.stderr));
}

/// Writes `src.tif`: three Byte bands of 32 x 24 without a geotransform or a CRS.
fn source(dir: &Path) {
    let config = RasterConfig { cols: 32, rows: 24, bands: 3, data_type: DataType::U8, ..Default::default() };
    let mut ds = RasterDataset::new(Raster::from_data(config, (0..32 * 24 * 3).map(|i| (i % 251) as f64).collect()).unwrap(), false);
    ds.set_geo_transform([0.0, 1.0, 0.0, 24.0, 0.0, -1.0]);
    let options = RasterWriteOptions { creation_options: vec![("GEOREFERENCED".into(), "NO".into())], ..Default::default() };
    terrio::write_raster_dataset(&ds, dir.join("src.tif").to_str().unwrap(), &options).unwrap();
}

/// Whether a JPEG 2000 file holds a GeoJP2 box among its top-level boxes.
fn has_geojp2_box(path: &Path) -> bool {
    let bytes = std::fs::read(path).unwrap();
    let mut i = 0;
    while i + 8 <= bytes.len() {
        if &bytes[i + 4..i + 8] == b"uuid" && bytes[i + 8..].starts_with(&GEOJP2) {
            return true;
        }
        match u32::from_be_bytes(bytes[i..i + 4].try_into().unwrap()) as usize {
            size if size >= 8 => i += size,
            _ => break,
        }
    }
    false
}

/// Whether a written file is georeferenced: its GeoJP2 box, its world file, its GeoTIFF tie point.
fn georeferenced(dir: &Path, name: &str) -> bool {
    let path = dir.join(name);
    match path.extension().and_then(|e| e.to_str()) {
        Some("jp2") => has_geojp2_box(&path),
        Some("tif") => terrio::open_raster_lazy(path.to_str().unwrap()).unwrap().has_geo_transform(),
        _ => path.with_extension("wld").exists(),
    }
}

#[test]
fn an_unreferenced_source_gives_unreferenced_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    source(d);
    assert!(!georeferenced(d, "src.tif"));
    for command in ["translate", "convert", "pipeline"] {
        for ext in ["jp2", "png", "jpg", "webp", "tif"] {
            let out = format!("{command}-{ext}.{ext}");
            match command {
                "pipeline" => run(d, &["pipeline", "read", "src.tif", "!", "write", &out]),
                _ => run(d, &[command, "src.tif", &out]),
            }
            assert!(!georeferenced(d, &out), "{out}");
        }
    }
}

/// Assigned bounds georeference the output; an assigned CRS alone is written as before.
#[test]
fn assigned_bounds_or_a_crs_are_written() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    source(d);
    for ext in ["jp2", "png", "tif"] {
        let out = format!("ullr.{ext}");
        run(d, &["translate", "src.tif", &out, "--a-ullr", "500000", "4100000", "500960", "4099280"]);
        assert!(georeferenced(d, &out), "{out}");
    }
    for ext in ["jp2", "png"] {
        let out = format!("srs.{ext}");
        run(d, &["translate", "src.tif", &out, "--a-srs", "EPSG:32611"]);
        assert!(georeferenced(d, &out), "{out}");
        assert_eq!(terrio::open_raster(d.join(&out).to_str().unwrap()).unwrap().crs().epsg, Some(32611), "{out}");
    }
}
