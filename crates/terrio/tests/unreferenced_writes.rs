//! A raster without a geotransform (`GEOREFERENCED=NO`) and without a CRS is written without
//! georeferencing: no GeoJP2 box in a JPEG 2000 file, no world file beside a PNG, JPEG or WebP
//! one. A CRS alone is still written, as before, with the raster's unit-pixel grid.

use terrio::{DataType, Raster, RasterConfig, RasterDataset, RasterWriteOptions};

/// The UUID of a GeoJP2 box.
const GEOJP2: [u8; 16] = [0xb1, 0x4b, 0xf8, 0xbd, 0x08, 0x3d, 0x4b, 0x43, 0xa5, 0xae, 0x8c, 0xd7, 0xd5, 0xa6, 0xce, 0x03];
const EXTENSIONS: [&str; 4] = ["jp2", "png", "jpg", "webp"];

/// Three Byte bands of 32 x 24 on the unit-pixel grid of a raster without a geotransform, with
/// an EPSG CRS or none.
fn raster(epsg: Option<u32>) -> RasterDataset {
    let (cols, rows) = (32, 24);
    let config = RasterConfig { cols, rows, bands: 3, data_type: DataType::U8, ..Default::default() };
    let data = (0..cols * rows * 3).map(|i| (i % 251) as f64).collect();
    let mut ds = RasterDataset::new(Raster::from_data(config, data).unwrap(), false);
    ds.set_geo_transform([0.0, 1.0, 0.0, rows as f64, 0.0, -1.0]);
    ds.raster.crs.epsg = epsg;
    ds
}

fn unreferenced() -> RasterWriteOptions {
    RasterWriteOptions { creation_options: vec![("GEOREFERENCED".into(), "NO".into())], ..Default::default() }
}

/// Whether JPEG 2000 bytes hold a GeoJP2 box among their top-level boxes.
fn has_geojp2_box(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i + 8 <= bytes.len() {
        let size = u32::from_be_bytes(bytes[i..i + 4].try_into().unwrap()) as usize;
        if &bytes[i + 4..i + 8] == b"uuid" && bytes[i + 8..].starts_with(&GEOJP2) {
            return true;
        }
        if size < 8 {
            break;
        }
        i += size;
    }
    false
}

/// The files in a directory, sorted.
fn files(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    names.sort();
    names
}

/// Writes `ds` to `a.<ext>` in a new directory: the directory and the path.
fn write(ds: &RasterDataset, ext: &str, options: &RasterWriteOptions) -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(format!("a.{ext}")).to_str().unwrap().to_string();
    terrio::write_raster_dataset(ds, &path, options).unwrap();
    (dir, path)
}

#[test]
fn a_raster_without_a_geotransform_or_a_crs_is_written_without_georeferencing() {
    for ext in EXTENSIONS {
        let (dir, path) = write(&raster(None), ext, &unreferenced());
        assert_eq!(files(dir.path()), [format!("a.{ext}")], "{ext}");
        if ext == "jp2" {
            assert!(!has_geojp2_box(&std::fs::read(&path).unwrap()));
        }
        if ext != "jpg" {
            let back = terrio::open_raster(&path).unwrap();
            assert_eq!(back.raster.data.to_f64_vec(), raster(None).raster.data.to_f64_vec(), "{ext}");
        }
    }
}

#[test]
fn a_crs_without_a_geotransform_and_a_georeferenced_raster_are_written_as_before() {
    for ext in EXTENSIONS {
        for (ds, options) in [(raster(Some(32611)), unreferenced()), (raster(None), RasterWriteOptions::default())] {
            let (dir, path) = write(&ds, ext, &options);
            let names = files(dir.path());
            if ext == "jp2" {
                assert!(has_geojp2_box(&std::fs::read(&path).unwrap()), "{ext}");
            } else {
                assert!(names.contains(&"a.wld".to_string()), "{ext}: {names:?}");
            }
            assert_eq!(terrio::open_raster(&path).unwrap().crs().epsg, ds.raster.crs.epsg, "{ext}");
        }
    }
}

/// An image written over one with a world file, without a geotransform or a CRS, leaves none:
/// it would georeference the new image.
#[test]
fn a_world_file_of_an_earlier_write_is_removed() {
    for ext in ["png", "jpg", "webp"] {
        let (dir, path) = write(&raster(None), ext, &RasterWriteOptions::default());
        assert!(dir.path().join("a.wld").exists(), "{ext}");
        terrio::write_raster_dataset(&raster(None), &path, &unreferenced()).unwrap();
        assert_eq!(files(dir.path()), [format!("a.{ext}")], "{ext}");
    }
}

/// Written to bytes, an unreferenced PNG is the PNG alone (no world file zipped with it), and a
/// JPEG 2000 file holds no GeoJP2 box.
#[test]
fn bytes_of_an_unreferenced_raster_hold_the_image_alone() {
    let png = terrio::memory::write_raster_bytes(&raster(None), "a.png", &unreferenced()).unwrap();
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
    let zipped = terrio::memory::write_raster_bytes(&raster(None), "a.png", &RasterWriteOptions::default()).unwrap();
    assert!(zipped.starts_with(b"PK"));
    let jp2 = terrio::memory::write_raster_bytes(&raster(None), "a.jp2", &unreferenced()).unwrap();
    assert!(jp2.starts_with(b"\x00\x00\x00\x0cjP  \r\n\x87\n") && !has_geojp2_box(&jp2));
    let jp2 = terrio::memory::write_raster_bytes(&raster(None), "a.jp2", &RasterWriteOptions::default()).unwrap();
    assert!(has_geojp2_box(&jp2));
}
