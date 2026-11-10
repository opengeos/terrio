//! A raster without a geotransform (`GEOREFERENCED=NO`) and without a CRS is written without
//! georeferencing: no GeoJP2 box in a JPEG 2000 file, no world file beside a PNG, JPEG or WebP
//! one, and no `.aux.xml` sidecar an earlier write left. A CRS alone is still written, as before,
//! with the raster's unit-pixel grid.

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
fn has_geojp2_box(bytes: &[u8]) -> bool {
    boxes(bytes).iter().any(|(kind, contents)| kind == b"uuid" && contents.starts_with(&GEOJP2))
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

/// An image written over one whose `.aux.xml` sidecar holds a CRS, without a geotransform or a CRS
/// of its own, leaves no sidecar: the earlier image's CRS would be read as the new one's.
#[test]
fn a_crs_sidecar_of_an_earlier_write_is_removed() {
    let mut readings = Vec::new();
    for ext in ["png", "jpg", "webp"] {
        let (dir, path) = write(&raster(Some(32611)), ext, &RasterWriteOptions::default());
        let sidecar = format!("a.{ext}.aux.xml");
        assert_eq!(files(dir.path()), [format!("a.{ext}"), sidecar.clone(), "a.wld".into()], "{ext}");
        assert!(std::fs::read_to_string(dir.path().join(&sidecar)).unwrap().contains("<SRS"), "{ext}");
        terrio::write_raster_dataset(&raster(None), &path, &unreferenced()).unwrap();
        let back = terrio::open_raster(&path).unwrap().crs();
        readings.push((ext, back.epsg, back.is_unknown(), files(dir.path())));
    }
    let unreferenced = |ext: &'static str| (ext, None, true, vec![format!("a.{ext}")]);
    assert_eq!(readings, [unreferenced("png"), unreferenced("jpg"), unreferenced("webp")]);
}

/// So does a JPEG 2000 file written over one whose sidecar holds a nodata value, without one of
/// its own: the earlier value would be read as the new image's.
#[test]
fn a_nodata_sidecar_of_an_earlier_write_is_removed() {
    let mut with_nodata = raster(None);
    with_nodata.raster.nodata = 7.0;
    with_nodata.has_nodata = true;
    let (dir, path) = write(&with_nodata, "jp2", &RasterWriteOptions::default());
    assert_eq!(files(dir.path()), ["a.jp2", "a.jp2.aux.xml"]);
    assert_eq!(terrio::open_raster(&path).unwrap().nodata(), Some(7.0));
    terrio::write_raster_dataset(&raster(None), &path, &RasterWriteOptions::default()).unwrap();
    assert_eq!(terrio::open_raster(&path).unwrap().nodata(), None);
    assert_eq!(files(dir.path()), ["a.jp2"]);
}

/// A sidecar that cannot be removed (here a directory of that name) fails the write, rather than
/// being read as the new image's.
#[test]
fn a_sidecar_that_cannot_be_removed_fails_the_write() {
    for ext in ["png", "jpg", "webp", "jp2"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(format!("a.{ext}.aux.xml"))).unwrap();
        let path = dir.path().join(format!("a.{ext}"));
        let written = terrio::write_raster_dataset(&raster(None), path.to_str().unwrap(), &unreferenced());
        assert!(matches!(written, Err(terrio::Error::Io(_))), "{ext}: {written:?}");
        assert!(!path.exists(), "{ext}: failed cleanup left the new image readable");
    }
}

/// A world file that cannot be removed reports failure after the image is written.
#[test]
fn a_world_file_that_cannot_be_removed_reports_failure() {
    for ext in ["png", "jpg", "webp"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("a.wld")).unwrap();
        let path = dir.path().join(format!("a.{ext}"));
        let written = terrio::write_raster_dataset(&raster(None), path.to_str().unwrap(), &unreferenced());
        assert!(matches!(written, Err(terrio::Error::Io(_))), "{ext}: {written:?}");
        assert_eq!(files(dir.path()), [format!("a.{ext}"), "a.wld".into()], "{ext}");
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

/// Failure to write the image must not remove an existing shared world file.
#[test]
fn a_failed_image_write_preserves_the_world_file() {
    for ext in ["png", "jpg", "webp"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("a.{ext}"));
        std::fs::create_dir(&path).unwrap();
        let world = dir.path().join("a.wld");
        std::fs::write(&world, "existing georeferencing").unwrap();
        assert!(terrio::write_raster_dataset(&raster(None), path.to_str().unwrap(), &unreferenced()).is_err());
        assert_eq!(std::fs::read_to_string(world).unwrap(), "existing georeferencing", "{ext}");
    }
}

/// GEOREFERENCED keeps the existing GeoTIFF last-match precedence across output formats.
#[test]
fn the_last_georeferenced_option_wins() {
    for ext in ["jp2", "png", "jpg", "webp", "tif"] {
        for (first, second, expected) in [("NO", "YES", true), ("YES", "NO", false)] {
            let options = RasterWriteOptions {
                creation_options: vec![("GEOREFERENCED".into(), first.into()), ("georeferenced".into(), second.into())],
                ..Default::default()
            };
            let (dir, path) = write(&raster(None), ext, &options);
            let present = match ext {
                "jp2" => has_geojp2_box(&std::fs::read(&path).unwrap()),
                "tif" => terrio::open_raster_lazy(&path).unwrap().has_geo_transform(),
                _ => dir.path().join("a.wld").exists(),
            };
            assert_eq!(present, expected, "{ext}: {first}, {second}");
        }
    }
}

/// GDAL 3.13.3 whole reads retain absence separately from a legitimate unit-pixel transform.
#[cfg(feature = "gdal")]
#[test]
fn whole_read_translation_preserves_an_absent_geotransform() {
    use terrio::ops::overview::OverviewSelection;
    use terrio::ops::translate::{TranslateOptions, translate_path_to_file};
    let dir = tempfile::tempdir().unwrap();
    for referenced in [false, true] {
        let src = dir.path().join(format!("source-{referenced}.tif"));
        let options = if referenced { RasterWriteOptions::default() } else { unreferenced() };
        terrio::write_raster_dataset(&raster(None), src.to_str().unwrap(), &options).unwrap();
        let source = format!("GDAL:{}", src.display());
        for ext in ["jp2", "png", "jpg", "webp", "tif"] {
            let path = dir.path().join(format!("out-{referenced}.{ext}"));
            translate_path_to_file(
                &source,
                &TranslateOptions::default(),
                OverviewSelection::Auto,
                path.to_str().unwrap(),
                &Default::default(),
            )
            .unwrap();
            let present = match ext {
                "jp2" => has_geojp2_box(&std::fs::read(&path).unwrap()),
                "tif" => terrio::open_raster_lazy(path.to_str().unwrap()).unwrap().has_geo_transform(),
                _ => path.with_extension("wld").exists(),
            };
            assert_eq!(present, referenced, "{ext}: referenced={referenced}");
        }
    }
}

#[test]
fn eager_jp2_reads_preserve_transform_presence_including_a_real_unit_grid() {
    for referenced in [false, true] {
        let options = if referenced { RasterWriteOptions::default() } else { unreferenced() };
        let (_dir, path) = write(&raster(None), "jp2", &options);
        assert_eq!(terrio::open_raster(&path).unwrap().geotransform_present, Some(referenced));
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(terrio::memory::open_raster_bytes(bytes, "a.jp2").unwrap().geotransform_present, Some(referenced));
    }
}

#[test]
fn xyz_rewrite_removes_stale_pam_crs() {
    let config = RasterConfig { cols: 2, rows: 2, bands: 1, data_type: DataType::F64, ..Default::default() };
    let mut ds = RasterDataset::new(Raster::from_data(config, vec![1., 2., 3., 4.]).unwrap(), false);
    ds.raster.crs.epsg = Some(4326);
    let (_dir, path) = write(&ds, "xyz", &RasterWriteOptions::default());
    assert!(std::path::Path::new(&format!("{path}.aux.xml")).exists());
    ds.raster.crs = Default::default();
    terrio::write_raster_dataset(&ds, &path, &RasterWriteOptions::default()).unwrap();
    assert!(!std::path::Path::new(&format!("{path}.aux.xml")).exists());
    assert_eq!(terrio::open_raster(&path).unwrap().raster.crs.epsg, None);
}
