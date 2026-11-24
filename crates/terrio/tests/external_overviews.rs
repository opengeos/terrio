//! External overviews: the images of a GDAL `.ovr` file next to a GeoTIFF without overviews of
//! its own are its overviews, as GDAL reads them.

use std::collections::HashMap;
use terrio::raster::RasterData;
use terrio::{DataType, RasterConfig, RasterDataset, RasterWriteOptions, ResampledReadOptions, Window, WindowOptions};

fn write(path: &std::path::Path, cols: usize, rows: usize, value: impl Fn(usize) -> u8, overviews: Vec<usize>) {
    let cfg = RasterConfig { cols, rows, bands: 1, data_type: DataType::U8, ..Default::default() };
    let r = terrio::Raster::from_data_native(cfg, RasterData::U8((0..cols * rows).map(value).collect())).unwrap();
    let options = RasterWriteOptions { overviews, ..Default::default() };
    terrio::write_raster_dataset(&RasterDataset::new(r, false), path.to_str().unwrap(), &options).unwrap();
}

fn overview(path: &std::path::Path, w: Window) -> Vec<f64> {
    let mut r = terrio::open_raster_lazy(path.to_str().unwrap()).unwrap();
    r.read_window(w, &WindowOptions { bands: vec![1], overview: Some(0) }).unwrap().raster.data.to_f64_vec()
}

#[test]
fn ovr_images_are_overviews() {
    let dir = tempfile::tempdir().unwrap();
    let tif = dir.path().join("img.tif");
    write(&tif, 70, 50, |i| (i % 7) as u8, Vec::new());
    assert!(terrio::open_raster_lazy(tif.to_str().unwrap()).unwrap().overviews().is_empty());
    // An .ovr of one 35 x 25 image, with values of its own.
    write(&dir.path().join("ovr.tif"), 35, 25, |i| (100 + i % 11) as u8, Vec::new());
    std::fs::rename(dir.path().join("ovr.tif"), dir.path().join("img.tif.ovr")).unwrap();
    let r = terrio::open_raster_lazy(tif.to_str().unwrap()).unwrap();
    assert_eq!(r.overviews(), [(35, 25)]);
    assert_eq!(r.level_geo_transform(Some(0)).unwrap()[1], 2.0);
    let got = overview(&tif, Window::new(3, 4, 5, 2));
    let want: Vec<f64> = [4, 5].iter().flat_map(|y| (3..8).map(move |x| (100 + (y * 35 + x) % 11) as f64)).collect();
    assert_eq!(got, want);
    // Full resolution is the TIFF's.
    let mut r = r;
    let full = r.read_window(Window::new(0, 0, 3, 1), &WindowOptions { bands: vec![1], overview: None }).unwrap();
    assert_eq!(full.raster.data.to_f64_vec(), [0.0, 1.0, 2.0]);
}

#[test]
fn internal_overviews_win_over_an_ovr() {
    let dir = tempfile::tempdir().unwrap();
    let tif = dir.path().join("img.tif");
    write(&tif, 70, 50, |i| (i % 7) as u8, vec![2]);
    write(&dir.path().join("ovr.tif"), 35, 25, |_| 200, Vec::new());
    std::fs::rename(dir.path().join("ovr.tif"), dir.path().join("img.tif.ovr")).unwrap();
    assert!(overview(&tif, Window::new(0, 0, 4, 4)).iter().all(|v| *v < 7.0));
}

#[test]
fn an_ovr_of_another_type_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let tif = dir.path().join("img.tif");
    write(&tif, 70, 50, |i| (i % 7) as u8, Vec::new());
    let cfg = RasterConfig { cols: 35, rows: 25, bands: 1, data_type: DataType::F32, ..Default::default() };
    let r = terrio::Raster::from_data_native(cfg, RasterData::F32(vec![1.0; 35 * 25])).unwrap();
    let ovr = dir.path().join("ovr.tif");
    terrio::write_raster_dataset(&RasterDataset::new(r, false), ovr.to_str().unwrap(), &RasterWriteOptions::default()).unwrap();
    std::fs::rename(&ovr, dir.path().join("img.tif.ovr")).unwrap();
    assert!(terrio::open_raster_lazy(tif.to_str().unwrap()).unwrap().overviews().is_empty());
}

#[test]
fn external_overview_dimensions_follow_gdal_acceptance() {
    // GDAL 3.12.2: no aspect-ratio, reduction-factor or smaller-than-base gate.
    for (width, height) in [(30, 10), (150, 40), (100, 80), (200, 160)] {
        let dir = tempfile::tempdir().unwrap();
        let tif = dir.path().join("img.tif");
        write(&tif, 100, 80, |_| 1, Vec::new());
        write(&dir.path().join("ovr.tif"), width, height, |_| 7, Vec::new());
        std::fs::rename(dir.path().join("ovr.tif"), dir.path().join("img.tif.ovr")).unwrap();
        let r = terrio::open_raster_lazy(tif.to_str().unwrap()).unwrap();
        assert_eq!(r.overviews(), [(width, height)]);
        assert_eq!(overview(&tif, Window::new(0, 0, 2, 1)), [7.0, 7.0]);
    }
}

#[test]
fn external_overviews_keep_their_nodata_and_internal_mask() {
    // GDAL 3.12.2: GetOverview(0) reports its own nodata and per-dataset mask.
    let dir = tempfile::tempdir().unwrap();
    let tif = dir.path().join("img.tif");
    write(&tif, 8, 6, |_| 9, Vec::new());
    let cfg = RasterConfig { cols: 4, rows: 3, bands: 1, data_type: DataType::U8, nodata: 7.0, ..Default::default() };
    let raster = terrio::Raster::from_data_native(cfg, RasterData::U8(vec![7; 12])).unwrap();
    let mask: Vec<u8> = (0..12).map(|i| if i % 3 == 0 { 0 } else { 255 }).collect();
    let ovr = dir.path().join("ovr.tif");
    terrio::write_raster_dataset(
        &RasterDataset::new(raster, true),
        ovr.to_str().unwrap(),
        &RasterWriteOptions { mask: Some(mask.clone()), ..Default::default() },
    )
    .unwrap();
    std::fs::rename(&ovr, dir.path().join("img.tif.ovr")).unwrap();
    let mut r = terrio::open_raster_lazy(tif.to_str().unwrap()).unwrap();
    assert_eq!(r.nodata(), None);
    let window = Window::full(4, 3);
    let data = r.read_window(window, &WindowOptions { overview: Some(0), ..Default::default() }).unwrap();
    assert_eq!(data.nodata(), Some(7.0));
    assert_eq!(r.read_mask(window, Some(0)).unwrap(), Some(mask));
    assert!(r.read_mask(Window::full(8, 6), None).unwrap().is_none());
}

#[test]
fn external_mask_policy_is_independent_of_the_base_tiff() {
    // GDAL 3.12.2: an unmasked external overview is all-valid even when
    // the base has an internal mask. An `.ovr.msk` applies to the overview.
    let dir = tempfile::tempdir().unwrap();
    let tif = dir.path().join("img.tif");
    let cfg = RasterConfig { cols: 8, rows: 6, bands: 1, data_type: DataType::U8, ..Default::default() };
    let raster = terrio::Raster::from_data_native(cfg, RasterData::U8(vec![1; 48])).unwrap();
    terrio::write_raster_dataset(
        &RasterDataset::new(raster, false),
        tif.to_str().unwrap(),
        &RasterWriteOptions { mask: Some(vec![0; 48]), ..Default::default() },
    )
    .unwrap();
    write(&dir.path().join("ovr.tif"), 4, 3, |_| 1, Vec::new());
    std::fs::rename(dir.path().join("ovr.tif"), dir.path().join("img.tif.ovr")).unwrap();
    let mut r = terrio::open_raster_lazy(tif.to_str().unwrap()).unwrap();
    assert_eq!(r.read_mask(Window::full(8, 6), None).unwrap(), Some(vec![0; 48]));
    assert!(r.read_mask(Window::full(4, 3), Some(0)).unwrap().is_none());
    let mask = dir.path().join("mask.tif");
    let cfg = RasterConfig {
        cols: 4,
        rows: 3,
        bands: 1,
        data_type: DataType::U8,
        metadata: vec![("INTERNAL_MASK_FLAGS_1".into(), "2".into())],
        ..Default::default()
    };
    let values: Vec<u8> = (0..12).map(|i| if i % 3 == 0 { 0 } else { 255 }).collect();
    let raster = terrio::Raster::from_data_native(cfg, RasterData::U8(values)).unwrap();
    terrio::write_raster_dataset(&RasterDataset::new(raster, false), mask.to_str().unwrap(), &RasterWriteOptions::default()).unwrap();
    std::fs::rename(mask, dir.path().join("img.tif.ovr.msk")).unwrap();
    let mut r = terrio::open_raster_lazy(tif.to_str().unwrap()).unwrap();
    let expected: Vec<u8> = (0..12).map(|i| if i % 3 == 0 { 0 } else { 255 }).collect();
    assert_eq!(r.read_mask(Window::full(4, 3), Some(0)).unwrap(), Some(expected));
    assert_eq!(r.read_mask(Window::full(8, 6), None).unwrap(), Some(vec![0; 48]));
    assert_eq!(
        r.file_list().unwrap(),
        ["img.tif", "img.tif.ovr", "img.tif.ovr.msk"].map(|name| dir.path().join(name).to_string_lossy().into_owned())
    );
}

/// Under `TERRIO_DISABLE_READDIR_ON_OPEN=EMPTY_DIR`, or `GDAL_DISABLE_READDIR_ON_OPEN=EMPTY_DIR`,
/// nothing beside the GeoTIFF is read: its `.ovr` gives it no overviews, a reduced read takes the
/// GeoTIFF's own pixels, and the GeoTIFF is the only file listed.
#[test]
fn an_ovr_is_not_read_under_empty_dir() {
    let dir = tempfile::tempdir().unwrap();
    let tif = dir.path().join("img.tif");
    write(&tif, 70, 50, |_| 1, Vec::new());
    write(&dir.path().join("ovr.tif"), 35, 25, |_| 2, Vec::new());
    std::fs::rename(dir.path().join("ovr.tif"), dir.path().join("img.tif.ovr")).unwrap();
    let name = tif.to_str().unwrap();
    let reduced = ResampledReadOptions { size: (7, 5), ..Default::default() };
    // Without the option, the `.ovr` is the GeoTIFF's overview and serves the reduced read.
    let mut r = terrio::open_raster_lazy(name).unwrap();
    assert_eq!(r.overviews(), [(35, 25)]);
    assert_eq!(r.read_resampled(&reduced).unwrap().raster.data.to_f64_vec(), [2.0; 35]);
    for key in ["TERRIO_DISABLE_READDIR_ON_OPEN", "GDAL_DISABLE_READDIR_ON_OPEN"] {
        terrio::config::push_scope(HashMap::from([(key.to_string(), Some("EMPTY_DIR".to_string()))]));
        let read = terrio::open_raster_lazy(name).and_then(|mut r| Ok((r.overviews(), r.file_list()?, r.read_resampled(&reduced)?)));
        terrio::config::pop_scope();
        let (overviews, files, small) = read.unwrap();
        assert!(overviews.is_empty(), "{key}: {overviews:?}");
        assert_eq!(files, [name], "{key}");
        assert_eq!(small.raster.data.to_f64_vec(), [1.0; 35], "{key}");
    }
}

/// Rewrites one classic TIFF directory with a NewSubfileType value, keeping its image data.
fn mark_image(path: &std::path::Path, index: usize, subfile: u32) {
    let mut bytes = std::fs::read(path).unwrap();
    assert_eq!(&bytes[..4], b"II*\0");
    let u16_at = |b: &[u8], i: usize| u16::from_le_bytes(b[i..i + 2].try_into().unwrap()) as usize;
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap()) as usize;
    let mut pointer = 4;
    for _ in 0..index {
        let offset = u32_at(&bytes, pointer);
        pointer = offset + 2 + 12 * u16_at(&bytes, offset);
    }
    let offset = u32_at(&bytes, pointer);
    let count = u16_at(&bytes, offset);
    let mut entries: Vec<Vec<u8>> = bytes[offset + 2..offset + 2 + 12 * count].as_chunks::<12>().0.iter().map(|e| e.to_vec()).collect();
    let next = bytes[offset + 2 + 12 * count..offset + 6 + 12 * count].to_vec();
    entries.retain(|entry| u16_at(entry, 0) != 254);
    let mut tag = vec![254, 0, 4, 0, 1, 0, 0, 0];
    tag.extend(subfile.to_le_bytes());
    entries.push(tag);
    entries.sort_by_key(|entry| u16_at(entry, 0));
    if !bytes.len().is_multiple_of(2) {
        bytes.push(0);
    }
    let rewritten = (bytes.len() as u32).to_le_bytes();
    bytes[pointer..pointer + 4].copy_from_slice(&rewritten);
    bytes.extend((entries.len() as u16).to_le_bytes());
    for entry in entries {
        bytes.extend(entry);
    }
    bytes.extend(next);
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn reduced_first_images_keep_their_masks_and_fallbacks() {
    // TIFF 6.0: mask association follows the image's own reduced-resolution bit.
    // GDAL 3.13.3 controls use ordinary first-image flags (see the Python comparison).
    for (reduced, mask_reduced) in [(false, false), (true, true), (true, false)] {
        for own_overview_mask in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let base = dir.path().join("image.tif");
            write(&base, 64, 48, |_| 9, Vec::new());
            let level = dir.path().join("image.tif.ovr");
            let cfg = RasterConfig { cols: 32, rows: 24, bands: 1, data_type: DataType::U8, ..Default::default() };
            let data = terrio::Raster::from_data_native(cfg, RasterData::U8(vec![7; 32 * 24])).unwrap();
            let mask: Vec<u8> = (0..32 * 24).map(|i| if i % 32 < 12 { 0 } else { 255 }).collect();
            let options = RasterWriteOptions {
                driver: Some(terrio::formats::RasterDriver::GTiff),
                mask: Some(mask.clone()),
                overviews: vec![2],
                ..Default::default()
            };
            terrio::write_raster_dataset(&RasterDataset::new(data, false), level.to_str().unwrap(), &options).unwrap();
            mark_image(&level, 0, u32::from(reduced));
            mark_image(&level, 1, 4 | u32::from(mask_reduced));
            if !own_overview_mask {
                mark_image(&level, 3, 0);
            }
            let mut standalone = terrio::open_raster_lazy(level.to_str().unwrap()).unwrap();
            assert!(standalone.has_mask());
            assert_eq!(standalone.read_mask(Window::new(0, 0, 32, 24), None).unwrap().unwrap(), mask);
            assert_eq!(standalone.level_has_own_mask(0), own_overview_mask);
            assert_eq!(standalone.read_level_mask(Window::new(0, 0, 16, 12), Some(0)).unwrap().is_some(), own_overview_mask);
            let fallback = standalone.read_mask(Window::new(0, 0, 16, 12), Some(0)).unwrap().unwrap();
            assert_eq!(fallback.iter().filter(|&&v| v == 0).count(), 6 * 12);
            let mut attached = terrio::open_raster_lazy(base.to_str().unwrap()).unwrap();
            assert!(attached.level_has_own_mask(0));
            assert_eq!(attached.read_mask(Window::new(0, 0, 32, 24), Some(0)).unwrap().unwrap(), mask);
            assert_eq!(attached.level_has_own_mask(1), own_overview_mask);
        }
    }
}

#[test]
fn unreduced_first_images_ignore_reduced_masks_and_keep_nodata() {
    use terrio::ops::overview::OverviewSelection;
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("image.tif");
    write(&base, 64, 48, |_| 9, Vec::new());
    let level = dir.path().join("image.tif.ovr");
    let cfg = RasterConfig { cols: 32, rows: 24, bands: 1, data_type: DataType::U8, nodata: 255.0, ..Default::default() };
    let values: Vec<u8> = (0..32 * 24).map(|i| if i % 32 < 12 { 255 } else { 7 }).collect();
    let raster = terrio::Raster::from_data_native(cfg, RasterData::U8(values)).unwrap();
    let options =
        RasterWriteOptions { driver: Some(terrio::formats::RasterDriver::GTiff), mask: Some(vec![0; 32 * 24]), ..Default::default() };
    terrio::write_raster_dataset(&RasterDataset::new(raster, true), level.to_str().unwrap(), &options).unwrap();
    mark_image(&level, 0, 0);
    mark_image(&level, 1, 5);
    let expected: Vec<u8> = (0..32 * 24).map(|i| if i % 32 < 12 { 0 } else { 255 }).collect();
    for (path, overview) in [(&level, None), (&base, Some(0))] {
        let mut r = terrio::open_raster_lazy(path.to_str().unwrap()).unwrap();
        assert!(!r.has_mask());
        assert!(r.read_mask(Window::full(32, 24), overview).unwrap().is_none());
        if let Some(level) = overview {
            assert!(!r.level_has_own_mask(level));
        }
        let options = ResampledReadOptions {
            size: (32, 24),
            overview: overview.map_or(OverviewSelection::None, OverviewSelection::Level),
            ..Default::default()
        };
        assert_eq!(r.read_resampled_masks(&options).unwrap(), expected);
    }
}

#[test]
fn an_unreadable_first_image_subfile_type_keeps_ordinary_decoding() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("image.tif");
    write(&path, 8, 6, |_| 7, Vec::new());
    mark_image(&path, 0, 0);
    let mut bytes = std::fs::read(&path).unwrap();
    let ifd = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
    let count = u16::from_le_bytes(bytes[ifd..ifd + 2].try_into().unwrap()) as usize;
    let tag = (0..count).map(|i| ifd + 2 + 12 * i).find(|&i| bytes[i..i + 2] == 254u16.to_le_bytes()).unwrap();
    // An ASCII tag is a valid TIFF field but cannot be read as an unsigned flag.
    bytes[tag + 2..tag + 4].copy_from_slice(&2u16.to_le_bytes());
    bytes[tag + 4..tag + 8].copy_from_slice(&2u32.to_le_bytes());
    bytes[tag + 8..tag + 12].copy_from_slice(b"x\0\0\0");
    std::fs::write(&path, bytes).unwrap();
    let mut raster = terrio::open_raster_lazy(path.to_str().unwrap()).unwrap();
    let read = raster.read_window(Window::full(8, 6), &WindowOptions::default()).unwrap();
    assert_eq!(read.raster.data.to_f64_vec(), vec![7.0; 48]);
}

/// A level of the GDAL-built fixture of `scripts/make_msk_overviews_fixtures.py`: its size, the
/// mask GDAL reads of it (255 valid) and its data.
struct MskLevel {
    size: (usize, usize),
    mask: Vec<u8>,
    data: Vec<f64>,
}

fn msk_fixture_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/msk_overviews")
}

/// The levels GDAL 3.13.2 builds for `masked.tif` (`gdaladdo -ro -r average masked.tif 2 3`),
/// largest first, and the files it lists for the dataset.
fn msk_expected() -> (Vec<MskLevel>, Vec<String>) {
    let text = std::fs::read_to_string(msk_fixture_dir().join("expected.json")).unwrap();
    let expected: serde_json::Value = serde_json::from_str(&text).unwrap();
    let levels = expected["levels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            let size = l["size"].as_array().unwrap();
            MskLevel {
                size: (size[0].as_u64().unwrap() as usize, size[1].as_u64().unwrap() as usize),
                mask: l["mask"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|row| row.as_str().unwrap().bytes().map(|b| (b - b'0') * 255))
                    .collect(),
                data: l["data"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect(),
            }
        })
        .collect();
    let files = expected["files"].as_array().unwrap().iter().map(|f| f.as_str().unwrap().to_string()).collect();
    (levels, files)
}

/// Copies the fixture's GeoTIFF and `.msk` into `dir`, with GDAL's `.ovr` and `.msk.ovr` when
/// `sidecars`; returns the GeoTIFF's path.
fn copy_msk_fixture(dir: &std::path::Path, sidecars: bool) -> String {
    let names: &[&str] = if sidecars { &["", ".msk", ".ovr", ".msk.ovr"] } else { &["", ".msk"] };
    for suffix in names {
        std::fs::copy(msk_fixture_dir().join(format!("masked.tif{suffix}")), dir.join(format!("masked.tif{suffix}"))).unwrap();
    }
    dir.join("masked.tif").to_string_lossy().into_owned()
}

/// Asserts that every overview of `path` has GDAL's mask and data, the mask its own.
fn assert_msk_levels(path: &str, expected: &[MskLevel]) {
    let mut r = terrio::LazyRaster::open(path).unwrap();
    assert_eq!(r.overviews(), expected.iter().map(|l| l.size).collect::<Vec<_>>());
    for (i, level) in expected.iter().enumerate() {
        let window = Window::full(level.size.0, level.size.1);
        assert!(r.level_has_own_mask(i), "level {i}");
        assert_eq!(r.read_level_mask(window, Some(i)).unwrap().as_deref(), Some(&level.mask[..]), "level {i}");
        assert_eq!(r.read_mask(window, Some(i)).unwrap().as_deref(), Some(&level.mask[..]), "level {i}");
        let data = r.read_window(window, &WindowOptions { bands: vec![1], overview: Some(i) }).unwrap();
        assert_eq!(data.raster.data.to_f64_vec(), level.data, "level {i}");
    }
}

/// The file names `path`'s dataset lists.
fn file_names(path: &str) -> Vec<String> {
    let r = terrio::LazyRaster::open(path).unwrap();
    r.file_list().unwrap().iter().map(|f| std::path::Path::new(f).file_name().unwrap().to_string_lossy().into_owned()).collect()
}

/// GDAL 3.13.2: the levels of an `.ovr` beside a GeoTIFF with a `.msk` sidecar take the `.msk`'s
/// overviews (its `.msk.ovr`) of their sizes as their masks, at every level.
#[test]
fn ovr_levels_take_the_msk_ovr_images_as_masks() {
    let (expected, files) = msk_expected();
    let path = msk_fixture_dir().join("masked.tif").to_string_lossy().into_owned();
    assert_msk_levels(&path, &expected);
    // As GDAL lists them: the `.msk.ovr` follows the `.ovr`.
    assert_eq!(file_names(&path), files);
}

/// GDAL 3.13.2: with a `.msk` on the base, an `.ovr` level without an internal mask takes the
/// `.msk`'s overview of its size, else none (all valid): it never reads an `.ovr.msk`.
#[test]
fn msk_overviews_win_over_an_ovr_msk() {
    let (expected, _) = msk_expected();
    let dir = tempfile::tempdir().unwrap();
    let path = copy_msk_fixture(dir.path(), true);
    let (w, h) = expected[0].size;
    let cfg = RasterConfig {
        cols: w,
        rows: h,
        bands: 1,
        data_type: DataType::U8,
        metadata: vec![("INTERNAL_MASK_FLAGS_1".into(), "2".into())],
        ..Default::default()
    };
    let raster = terrio::Raster::from_data_native(cfg, RasterData::U8(vec![0; w * h])).unwrap();
    let mask = dir.path().join("mask.tif");
    terrio::write_raster_dataset(&RasterDataset::new(raster, false), mask.to_str().unwrap(), &RasterWriteOptions::default()).unwrap();
    std::fs::rename(mask, format!("{path}.ovr.msk")).unwrap();
    assert_msk_levels(&path, &expected);
    std::fs::remove_file(format!("{path}.msk.ovr")).unwrap();
    let mut r = terrio::LazyRaster::open(&path).unwrap();
    for (i, level) in expected.iter().enumerate() {
        let window = Window::full(level.size.0, level.size.1);
        assert!(!r.level_has_own_mask(i), "level {i}");
        assert_eq!(r.read_level_mask(window, Some(i)).unwrap(), None, "level {i}");
        assert_eq!(r.read_mask(window, Some(i)).unwrap(), None, "level {i}");
    }
}

/// External overviews of a read-only GeoTIFF with a `.msk` sidecar write the mask's overviews to
/// its `.msk.ovr`, as GDAL 3.13.2 does (`gdaladdo -ro`): the levels' masks and data are GDAL's,
/// and the GeoTIFF and its `.msk` are left as they were.
#[test]
fn external_overviews_of_a_msk_sidecar_write_its_msk_ovr() {
    use terrio::ops::Resampling;
    use terrio::ops::overview::{OverviewOptions, add_overviews};
    let (expected, files) = msk_expected();
    let dir = tempfile::tempdir().unwrap();
    let path = copy_msk_fixture(dir.path(), false);
    let originals: Vec<Vec<u8>> = ["", ".msk"].iter().map(|s| std::fs::read(format!("{path}{s}")).unwrap()).collect();
    let read_only = |yes: bool| {
        for s in ["", ".msk"] {
            let file = format!("{path}{s}");
            let mut permissions = std::fs::metadata(&file).unwrap().permissions();
            permissions.set_readonly(yes);
            std::fs::set_permissions(&file, permissions).unwrap();
        }
    };
    read_only(true);
    let o = OverviewOptions { external: true, levels: vec![2, 3], resampling: Resampling::Average, ..Default::default() };
    let built = add_overviews(&path, &o);
    read_only(false);
    assert_eq!(built.unwrap(), expected.iter().map(|l| l.size).collect::<Vec<_>>());
    assert!(std::path::Path::new(&format!("{path}.msk.ovr")).is_file());
    assert_eq!(["", ".msk"].iter().map(|s| std::fs::read(format!("{path}{s}")).unwrap()).collect::<Vec<_>>(), originals);
    assert_msk_levels(&path, &expected);
    assert_eq!(file_names(&path), files);
    // Built again, the sidecars are rebuilt in place, with the same levels.
    add_overviews(&path, &o).unwrap();
    assert_msk_levels(&path, &expected);
}

/// A cancelled rebuild, while the data's levels or the mask's are computed, or at the last
/// callback, leaves both the `.ovr` and the `.msk.ovr` as they were.
#[test]
fn a_cancelled_msk_rebuild_keeps_both_sidecars() {
    use terrio::ops::Resampling;
    use terrio::ops::overview::{OverviewOptions, add_overviews, add_overviews_with_progress};
    let dir = tempfile::tempdir().unwrap();
    let path = copy_msk_fixture(dir.path(), false);
    let o = OverviewOptions { external: true, levels: vec![2], resampling: Resampling::Average, ..Default::default() };
    add_overviews(&path, &o).unwrap();
    let sidecars = || ["ovr", "msk.ovr"].map(|s| std::fs::read(format!("{path}.{s}")).unwrap());
    let before = sidecars();
    let more = OverviewOptions { levels: vec![2, 3], ..o.clone() };
    // The data takes the first half of the work (one band), the mask the second.
    for stop in [0.25, 0.75, 1.0] {
        let mut calls = Vec::new();
        let err = add_overviews_with_progress(&path, &more, &mut |f| {
            calls.push(f);
            f < stop
        })
        .unwrap_err();
        assert!(matches!(err, terrio::Error::Cancelled), "{err}");
        assert!(calls.last().is_some_and(|f| *f >= stop), "{calls:?}");
        assert_eq!(sidecars(), before, "stopped at {stop}");
    }
    let mut calls = Vec::new();
    add_overviews_with_progress(&path, &more, &mut |f| {
        calls.push(f);
        true
    })
    .unwrap();
    assert!(calls.windows(2).all(|p| p[0] <= p[1]) && calls.last() == Some(&1.0), "{calls:?}");
    assert!(calls.iter().any(|f| *f > 0.5 && *f < 1.0), "{calls:?}");
    assert_eq!(terrio::LazyRaster::open(&path).unwrap().overviews().len(), 2);
}

/// A `.msk` with internal overviews of its own cannot take external ones, as in GDAL: refused
/// before anything is written.
#[test]
fn a_msk_with_internal_overviews_is_refused() {
    use terrio::ops::overview::{OverviewOptions, add_overviews};
    let dir = tempfile::tempdir().unwrap();
    let tif = dir.path().join("img.tif");
    write(&tif, 40, 30, |i| (i % 9) as u8, Vec::new());
    let cfg = RasterConfig {
        cols: 40,
        rows: 30,
        bands: 1,
        data_type: DataType::U8,
        metadata: vec![("INTERNAL_MASK_FLAGS_1".into(), "2".into())],
        ..Default::default()
    };
    let raster =
        terrio::Raster::from_data_native(cfg, RasterData::U8((0..1200).map(|i| if i % 3 == 0 { 0 } else { 255 }).collect())).unwrap();
    let options = RasterWriteOptions { overviews: vec![2], ..Default::default() };
    let mask = dir.path().join("mask.tif");
    terrio::write_raster_dataset(&RasterDataset::new(raster, false), mask.to_str().unwrap(), &options).unwrap();
    std::fs::rename(mask, dir.path().join("img.tif.msk")).unwrap();
    let path = tif.to_string_lossy().into_owned();
    assert!(terrio::LazyRaster::open(&path).unwrap().has_mask());
    let err = add_overviews(&path, &OverviewOptions { external: true, levels: vec![2], ..Default::default() }).unwrap_err();
    assert!(matches!(&err, terrio::Error::InvalidArgument(m) if m.contains("internal overviews")), "{err}");
    assert!(!std::path::Path::new(&format!("{path}.ovr")).exists());
    assert!(!std::path::Path::new(&format!("{path}.msk.ovr")).exists());
}
