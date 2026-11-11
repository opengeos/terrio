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

/// The top-level boxes of JPEG 2000 bytes, `(type, contents)`. A box of size 0 runs to the end of
/// the bytes, and one of size 1 gives its size in the 8 bytes after its type. Panics unless the
/// boxes end exactly at the end of the bytes, so that a walk cut short fails the test.
fn boxes(bytes: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        assert!(i + 8 <= bytes.len(), "a box header is cut short at byte {i} of {}", bytes.len());
        let kind: [u8; 4] = bytes[i + 4..i + 8].try_into().unwrap();
        let (size, header) = match u32::from_be_bytes(bytes[i..i + 4].try_into().unwrap()) {
            0 => (bytes.len() - i, 8),
            1 => {
                assert!(i + 16 <= bytes.len(), "a box size is cut short at byte {i} of {}", bytes.len());
                (u64::from_be_bytes(bytes[i + 8..i + 16].try_into().unwrap()) as usize, 16)
            }
            size => (size as usize, 8),
        };
        assert!(size >= header && size <= bytes.len() - i, "box {kind:?} at byte {i} of {}: size {size}", bytes.len());
        out.push((kind, &bytes[i + header..i + size]));
        i += size;
    }
    assert_eq!(i, bytes.len(), "the boxes end at byte {i} of {}", bytes.len());
    out
}

/// Whether JPEG 2000 bytes hold a GeoJP2 box among their top-level boxes.
fn holds_geojp2_box(bytes: &[u8]) -> bool {
    boxes(bytes).iter().any(|(kind, contents)| kind == b"uuid" && contents.starts_with(&GEOJP2))
}

/// Whether a JPEG 2000 file holds a GeoJP2 box among its top-level boxes.
fn has_geojp2_box(path: &Path) -> bool {
    holds_geojp2_box(&std::fs::read(path).unwrap())
}

/// The box walker reads a box with a 64-bit size and one running to the end of the file, and
/// refuses a file whose last box is cut short.
#[test]
fn the_box_walker_reads_every_box_or_fails() {
    let geojp2 = |size: u32| [&size.to_be_bytes()[..], b"uuid", &GEOJP2, b"payload"].concat();
    let extended = [&1u32.to_be_bytes()[..], b"free", &24u64.to_be_bytes(), &[0; 8]].concat();
    let signature = [&12u32.to_be_bytes()[..], b"jP  ", b"\r\n\x87\n"].concat();
    assert!(holds_geojp2_box(&[signature.clone(), extended.clone(), geojp2(31)].concat()));
    assert!(holds_geojp2_box(&[signature.clone(), extended.clone(), geojp2(0)].concat()));
    let cut = [signature, extended, geojp2(31)].concat();
    assert!(std::panic::catch_unwind(|| holds_geojp2_box(&cut[..cut.len() - 1])).is_err());
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
        let out = format!("{command}-cog.tif");
        match command {
            "pipeline" => run(d, &["pipeline", "read", "src.tif", "!", "write", &out, "--format", "COG"]),
            _ => run(d, &[command, "src.tif", &out, "-f", "COG"]),
        }
        assert!(!georeferenced(d, &out), "{out}");
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
    // A GeoTIFF keeps the CRS in its GeoKeys, without a tie point.
    run(d, &["translate", "src.tif", "srs.tif", "--a-srs", "EPSG:32611"]);
    assert!(!georeferenced(d, "srs.tif"));
    assert_eq!(terrio::open_raster(d.join("srs.tif").to_str().unwrap()).unwrap().crs().epsg, Some(32611));
}

#[test]
fn unreferenced_images_remain_unreferenced_through_each_command() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    source(d);
    for ext in ["png", "jpg", "webp", "jp2"] {
        let src = format!("src.{ext}");
        run(d, &["translate", "src.tif", &src]);
        for command in ["translate", "convert", "pipeline"] {
            let out = format!("{command}-{ext}.tif");
            match command {
                "pipeline" => run(d, &["pipeline", "read", &src, "!", "write", &out]),
                _ => run(d, &[command, &src, &out]),
            }
            assert!(!georeferenced(d, &out), "{src} via {command}");
        }
    }
}
