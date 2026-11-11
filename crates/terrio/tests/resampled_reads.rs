//! Reads into outputs of other sizes (`LazyRaster::read_resampled` and its masks) of the rasters
//! of `tests/data/resampled_reads`, against what GDAL 3.13.2 reads of them
//! (the RGB external, mixed-metadata internal/external and masked external cases use GDAL 3.13.3;
//! each case records `gdal_version`)
//! (`scripts/make_resampled_reads_fixtures.py`): pixels through `RasterIO` (the dataset's for
//! several bands, as rasterio reads them, the band's for one) and masks through `GetMaskBand()`,
//! or, for a level read alone, the pixels `gdal_translate -ovr` writes. Values are compared bit
//! for bit, any NaN matching any NaN, but for roundings of fused multiply-adds (see
//! [`difference`]). The masked external cubic 26×41 case uses a separate GDAL 3.13.3
//! control with equivalent first-image/mask flags 0/4: GDAL ignores that mask with 1/5.
//! `--mask-control` regenerates it without changing the historical raw references.

use std::io::Read;
use std::path::PathBuf;
use terrio::ResampledReadOptions;
use terrio::ops::Resampling;
use terrio::ops::overview::OverviewSelection;
use terrio::raster::RasterData;

/// A read of a fixture and what GDAL reads.
struct Case {
    file: String,
    options: ResampledReadOptions,
    dtype: String,
    /// The pixels, `(bands, rows, cols)`, as little-endian bytes.
    data: Vec<u8>,
    /// The masks, `(bands, rows, cols)`; `None` for a level read alone, whose masks are not
    /// compared.
    masks: Option<Vec<u8>>,
}

impl Case {
    fn path(&self) -> String {
        dir().join(&self.file).to_string_lossy().into_owned()
    }

    fn name(&self) -> String {
        let o = &self.options;
        format!("{} bands {:?} window {:?} size {:?} {} {:?}", self.file, o.bands, o.window, o.size, o.resampling.name(), o.overview)
    }
}

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/resampled_reads")
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

fn cases() -> Vec<Case> {
    let mut text = String::new();
    let file = std::fs::File::open(dir().join("expected.json.gz")).unwrap();
    flate2::read::GzDecoder::new(file).read_to_string(&mut text).unwrap();
    let expected: serde_json::Value = serde_json::from_str(&text).unwrap();
    text.clear();
    let file = std::fs::File::open(dir().join("expected_mask_control.json.gz")).unwrap();
    flate2::read::GzDecoder::new(file).read_to_string(&mut text).unwrap();
    let controls: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap();
    assert_eq!(controls.len(), 1, "one frozen reduced-first-image control is required");
    for control in &controls {
        let matches = expected
            .as_array()
            .unwrap()
            .iter()
            .filter(|raw| ["file", "bands", "window", "size", "resampling", "overview"].iter().all(|&key| control[key] == raw[key]))
            .count();
        assert_eq!(matches, 1, "mask control must match exactly one historical case: {control}");
    }
    let numbers = |v: &serde_json::Value| -> Vec<usize> { v.as_array().unwrap().iter().map(|n| n.as_u64().unwrap() as usize).collect() };
    expected
        .as_array()
        .unwrap()
        .iter()
        .map(|raw| {
            let c = controls
                .iter()
                .find(|control| ["file", "bands", "window", "size", "resampling", "overview"].iter().all(|&key| control[key] == raw[key]))
                .unwrap_or(raw);
            let window = c["window"].as_array().map(|w| {
                let w: Vec<f64> = w.iter().map(|v| v.as_f64().unwrap()).collect();
                (w[0], w[1], w[2], w[3])
            });
            let size = numbers(&c["size"]);
            Case {
                file: c["file"].as_str().unwrap().to_string(),
                options: ResampledReadOptions {
                    bands: numbers(&c["bands"]),
                    window,
                    size: (size[0], size[1]),
                    resampling: Resampling::parse(c["resampling"].as_str().unwrap()).unwrap(),
                    overview: OverviewSelection::parse(c["overview"].as_str().unwrap()).unwrap(),
                },
                dtype: c["dtype"].as_str().unwrap().to_string(),
                data: hex(c["data"].as_str().unwrap()),
                masks: c["masks"].as_str().map(hex),
            }
        })
        .collect()
}

/// The values of a buffer of a type as `f64`, from their little-endian bytes.
fn values(bytes: &[u8], dtype: &str) -> Vec<f64> {
    match dtype {
        "uint8" => bytes.iter().map(|&b| f64::from(b)).collect(),
        "uint16" => bytes.chunks(2).map(|c| f64::from(u16::from_le_bytes([c[0], c[1]]))).collect(),
        "int16" => bytes.chunks(2).map(|c| f64::from(i16::from_le_bytes([c[0], c[1]]))).collect(),
        "float32" => bytes.chunks(4).map(|c| f64::from(f32::from_le_bytes(c.try_into().unwrap()))).collect(),
        other => panic!("unexpected type {other}"),
    }
}

fn read_values(data: &RasterData) -> Vec<f64> {
    match data {
        RasterData::U8(v) => v.iter().map(|&x| f64::from(x)).collect(),
        RasterData::U16(v) => v.iter().map(|&x| f64::from(x)).collect(),
        RasterData::I16(v) => v.iter().map(|&x| f64::from(x)).collect(),
        RasterData::F32(v) => v.iter().map(|&x| f64::from(x)).collect(),
        other => panic!("unexpected type {:?}", other.data_type()),
    }
}

/// A read's values against GDAL's: equal (any NaN matching any NaN), else rounded otherwise by a
/// fused multiply-add. GDAL's arm64 builds, which wrote the fixtures, contract the multiply-adds
/// of the average and RMS kernels, and of floating-point kernels, into fused ones; x86-64 builds,
/// whose arithmetic terrio's kernels follow, do not. A mean exactly between two integers may
/// then round the other way, and a floating-point value differ in its last bit. Such
/// differences are allowed in at most 1% of a case's values, or in one value of a case of fewer
/// than 200 values; any other is an error.
fn difference(read: &[f64], expected: &[f64], float: bool, case: &Case) -> Option<String> {
    if read.len() != expected.len() {
        return Some(format!("{} values, not {}", read.len(), expected.len()));
    }
    let averaged = matches!(case.options.resampling, Resampling::Average | Resampling::Rms);
    let fused = |a: f64, b: f64| {
        if float {
            let (a, b) = (a as f32, b as f32);
            case.options.resampling != Resampling::Nearest && a.signum() == b.signum() && a.to_bits().abs_diff(b.to_bits()) == 1
        } else {
            averaged && (a - b).abs() == 1.0
        }
    };
    let (mut rounded, mut first) = (0, None);
    for (i, (&a, &b)) in read.iter().zip(expected).enumerate() {
        if a == b || (a.is_nan() && b.is_nan()) {
            continue;
        }
        if fused(a, b) {
            rounded += 1;
        } else {
            return Some(format!("value {i} is {a}, not {b}"));
        }
        first.get_or_insert(i);
    }
    (rounded > (read.len() / 100).max(1)).then(|| format!("{rounded} of {} values rounded otherwise (first {first:?})", read.len()))
}

/// Every case against its recorded reference: original cases 3.13.2, added cases 3.13.3,
/// with GDAL 3.13 behavior selected (masked convolution chunks skip NaN values).
#[test]
fn resampled_reads_are_gdal_reads() {
    terrio::config::push_scope([("TERRIO_GDAL_COMPAT".to_string(), Some("3.13".to_string()))].into());
    let cases = cases();
    assert_eq!(cases.len(), 258);
    let mut failures = Vec::new();
    for case in &cases {
        let mut raster = terrio::open_raster_lazy(&case.path()).unwrap();
        let (read, masks) = if case.masks.is_some() {
            let (read, masks) = raster.read_resampled_with_masks(&case.options).unwrap();
            (read, Some(masks))
        } else {
            (raster.read_resampled(&case.options).unwrap(), None)
        };
        let float = case.dtype == "float32";
        if let Some(d) = difference(&read_values(&read.raster.data), &values(&case.data, &case.dtype), float, case) {
            failures.push(format!("{}: pixels: {d}", case.name()));
        }
        if let (Some(masks), Some(expected)) = (&masks, &case.masks) {
            // The reads alone are the read together.
            let bits = |v: Vec<f64>| v.into_iter().map(|x| if x.is_nan() { u64::MAX } else { x.to_bits() }).collect::<Vec<_>>();
            let alone = read_values(&raster.read_resampled(&case.options).unwrap().raster.data);
            assert_eq!(bits(alone), bits(read_values(&read.raster.data)), "{}", case.name());
            assert_eq!(&raster.read_resampled_masks(&case.options).unwrap(), masks, "{}", case.name());
            let masks: Vec<f64> = masks.iter().map(|&m| f64::from(m)).collect();
            if let Some(d) = difference(&masks, &values(expected, "uint8"), false, case) {
                failures.push(format!("{}: masks: {d}", case.name()));
            }
        }
    }
    terrio::config::pop_scope();
    assert!(failures.is_empty(), "{} of {} cases differ from GDAL:\n{}", failures.len(), cases.len(), failures.join("\n"));
}

/// Rasters read into memory first, without overviews or a mask of their own, read the same way.
#[test]
fn in_memory_reads_are_gdal_reads() {
    let cases = cases();
    let in_memory: Vec<&Case> = cases.iter().filter(|c| matches!(c.file.as_str(), "i16_nodata.tif" | "valid.tif")).collect();
    assert_eq!(in_memory.len(), 36);
    for case in in_memory {
        let mut raster = terrio::LazyRaster::from(terrio::open_raster(&case.path()).unwrap());
        assert!(!raster.is_lazy());
        let (read, masks) = raster.read_resampled_with_masks(&case.options).unwrap();
        let d = difference(&read_values(&read.raster.data), &values(&case.data, &case.dtype), false, case);
        assert_eq!(d, None, "{}", case.name());
        let masks: Vec<f64> = masks.iter().map(|&m| f64::from(m)).collect();
        assert_eq!(difference(&masks, &values(case.masks.as_ref().unwrap(), "uint8"), false, case), None, "{}", case.name());
    }
}

/// A window a level read alone starts at the level's last column of, once rounded, reads nothing,
/// as `VRTSimpleSource::GetSrcDstWindow` does: the bands' nodata values or 0, masks of 0.
#[test]
fn level_reads_past_the_last_pixel_are_empty() {
    let methods = [Resampling::Nearest, Resampling::Bilinear, Resampling::Cubic, Resampling::Average, Resampling::Rms, Resampling::Mode];
    let mut raster = terrio::open_raster_lazy(dir().join("rgb_mask.tif").to_str().unwrap()).unwrap();
    for resampling in methods {
        for overview in [OverviewSelection::Level(0), OverviewSelection::None] {
            let o =
                ResampledReadOptions { window: Some((63.9995, 0.0, 1.0, 48.0)), size: (1, 4), resampling, overview, ..Default::default() };
            let (read, masks) = raster.read_resampled_with_masks(&o).unwrap();
            assert_eq!(read_values(&read.raster.data), [0.0; 12], "{resampling:?} {overview:?}");
            assert_eq!(masks, [0; 12], "{resampling:?} {overview:?}");
        }
    }
    let mut raster = terrio::open_raster_lazy(dir().join("i16_nodata.tif").to_str().unwrap()).unwrap();
    let o = ResampledReadOptions {
        window: Some((49.9995, 0.0, 1.0, 40.0)),
        size: (1, 3),
        resampling: Resampling::Average,
        overview: OverviewSelection::None,
        ..Default::default()
    };
    let (read, masks) = raster.read_resampled_with_masks(&o).unwrap();
    assert_eq!(read_values(&read.raster.data), [-32768.0; 6]);
    assert_eq!(masks, [0; 6]);
}

/// An external overview is a dataset of its own: its nodata value masks its pixels, though full
/// resolution has none, for bands resampled together or band by band; a level read alone keeps
/// its nodata value.
#[test]
fn external_overview_nodata_masks_its_pixels() {
    let dir = tempfile::tempdir().unwrap();
    let base = external_overview_with_nodata(dir.path());
    let mut raster = terrio::open_raster_lazy(base.to_str().unwrap()).unwrap();
    assert_eq!(raster.overviews(), [(32, 24)]);
    assert_eq!(raster.band_nodata(1), None);
    // Not a fixture case (those are GDAL 3.13.2's reads): GDAL 3.12.1's `ReadRaster` and
    // `gdal_translate -ovr 0` were checked to read these values of the same files. (Its cubic and
    // Lanczos kernels give nodata where the pixel nearest an output pixel's center is invalid,
    // here every one.)
    for resampling in [Resampling::Bilinear, Resampling::Average, Resampling::Rms] {
        for (overview, nodata) in [(OverviewSelection::Auto, None), (OverviewSelection::Level(0), Some(7.0))] {
            let o = ResampledReadOptions { size: (16, 12), resampling, overview, ..Default::default() };
            let expected: Vec<f64> = [100.0, 150.0, 200.0].iter().flat_map(|&v| [v; 16 * 12]).collect();
            let read = raster.read_resampled(&o).unwrap();
            assert_eq!(read_values(&read.raster.data), expected, "{resampling:?} {overview:?}");
            assert_eq!(read.band_nodata(1), nodata, "{resampling:?} {overview:?}");
            for band in 1..=3 {
                let alone = raster.read_resampled(&ResampledReadOptions { bands: vec![band], ..o.clone() }).unwrap();
                let expected = &expected[(band - 1) * 192..band * 192];
                assert_eq!(read_values(&alone.raster.data), expected, "{resampling:?} {overview:?} band {band}");
            }
        }
    }
}

/// The masks of an external overview read alone are those of its nodata value, which the output
/// takes, though full resolution has none.
#[test]
fn external_overview_masks_follow_its_nodata() {
    let dir = tempfile::tempdir().unwrap();
    let base = external_overview_with_nodata(dir.path());
    let mut raster = terrio::open_raster_lazy(base.to_str().unwrap()).unwrap();
    let o = ResampledReadOptions { size: (32, 24), overview: OverviewSelection::Level(0), ..Default::default() };
    let (read, masks) = raster.read_resampled_with_masks(&o).unwrap();
    assert_eq!(read.band_nodata(1), Some(7.0));
    let expected: Vec<u8> = read_values(&read.raster.data).iter().map(|&v| if v == 7.0 { 0 } else { 255 }).collect();
    assert!(expected.contains(&0));
    assert_eq!(masks, expected);
    assert_eq!(raster.read_resampled_masks(&o).unwrap(), expected);
}

/// A raster of 64 x 48 pixels, three Byte bands without nodata, whose external overview of
/// 32 x 24 pixels has nodata 7: each band one value (100, 150, 200) but for nodata pixels at even
/// rows and columns. Returns the raster's path.
fn external_overview_with_nodata(dir: &std::path::Path) -> PathBuf {
    use terrio::{DataType, Raster, RasterConfig, RasterDataset};
    let dataset = |cols: usize, rows: usize, values: Vec<u8>, nodata: Option<f64>| {
        let config = RasterConfig { cols, rows, bands: 3, data_type: DataType::U8, nodata: nodata.unwrap_or(-1.0), ..Default::default() };
        let mut ds = RasterDataset::new(Raster::from_data_native(config, RasterData::U8(values)).unwrap(), nodata.is_some());
        ds.set_geo_transform([0.0, 64.0 / cols as f64, 0.0, 48.0, 0.0, -48.0 / rows as f64]);
        ds
    };
    let base = dir.join("base.tif");
    terrio::write_raster_dataset(&dataset(64, 48, vec![1; 3 * 64 * 48], None), base.to_str().unwrap(), &Default::default()).unwrap();
    let ovr: Vec<u8> = (0..3)
        .flat_map(|b| (0..24 * 32).map(move |i| if (i / 32) % 2 == 0 && (i % 32) % 2 == 0 { 7 } else { 100 + 50 * b as u8 }))
        .collect();
    let ovr_tif = dir.join("ovr.tif");
    terrio::write_raster_dataset(&dataset(32, 24, ovr, Some(7.0)), ovr_tif.to_str().unwrap(), &Default::default()).unwrap();
    std::fs::rename(&ovr_tif, dir.join("base.tif.ovr")).unwrap();
    base
}

/// A shared handle reads as the raster it shares: an overview without a mask of its own is all
/// valid, rather than sampling the full-resolution mask.
#[test]
fn shared_handles_weight_overviews_by_their_own_masks() {
    let path = dir().join("rgb_mask_plain_overviews.tif");
    let mut direct = terrio::open_raster_lazy(path.to_str().unwrap()).unwrap();
    let mut shared = terrio::open_raster_lazy(path.to_str().unwrap()).unwrap().shared();
    for resampling in [Resampling::Nearest, Resampling::Bilinear, Resampling::Cubic, Resampling::Average] {
        for overview in [OverviewSelection::Auto, OverviewSelection::Level(0), OverviewSelection::Level(1)] {
            let o = ResampledReadOptions { size: (13, 9), resampling, overview, ..Default::default() };
            let (a, a_masks) = direct.read_resampled_with_masks(&o).unwrap();
            let (b, b_masks) = shared.read_resampled_with_masks(&o).unwrap();
            assert_eq!(read_values(&a.raster.data), read_values(&b.raster.data), "{resampling:?} {overview:?}");
            assert_eq!(a_masks, b_masks, "{resampling:?} {overview:?}");
        }
    }
}

/// A shared handle chooses again among an external `.ovr`'s other images when `RasterIO`
/// chooses that file's image, as the reader it shares does: the reads GDAL makes from the
/// `.ovr`'s second image.
#[test]
fn shared_handles_choose_among_external_overviews_twice() {
    for file in ["rgb_external_overviews.tif", "mixed_external_overviews.tif", "masked_external_overviews.tif"] {
        let path = dir().join(file);
        let mut direct = terrio::open_raster_lazy(path.to_str().unwrap()).unwrap();
        let mut shared = terrio::open_raster_lazy(path.to_str().unwrap()).unwrap().shared();
        let expected: Vec<Case> = cases().into_iter().filter(|c| c.file == file).collect();
        assert!(!expected.is_empty());
        for case in &expected {
            let a = read_values(&direct.read_resampled(&case.options).unwrap().raster.data);
            let b = read_values(&shared.read_resampled(&case.options).unwrap().raster.data);
            assert_eq!(a, b, "{}", case.name());
            assert_eq!(difference(&b, &values(&case.data, &case.dtype), false, case), None, "{}", case.name());
        }
    }
}

/// An overview read alone at the raster's size is not the full-resolution image: its band
/// statistics, which describe full resolution, are dropped.
#[test]
fn overview_reads_drop_full_resolution_statistics() {
    use terrio::{DataType, Raster, RasterConfig, RasterDataset};
    let config = RasterConfig { cols: 64, rows: 64, bands: 1, data_type: DataType::U8, ..Default::default() };
    let mut ds = RasterDataset::new(
        Raster::from_data_native(config, RasterData::U8((0..64 * 64).map(|i| (i % 251) as u8).collect())).unwrap(),
        false,
    );
    ds.set_geo_transform([0.0, 1.0, 0.0, 64.0, 0.0, -1.0]);
    ds.band_metadata = vec![terrio::BandMetadata { items: vec![("STATISTICS_MEAN".into(), "1".into())], ..Default::default() }];
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cog.tif");
    let options =
        terrio::RasterWriteOptions { driver: Some(terrio::formats::RasterDriver::Cog), tile_size: Some(16), ..Default::default() };
    terrio::write_raster_dataset(&ds, path.to_str().unwrap(), &options).unwrap();
    let mut raster = terrio::open_raster_lazy(path.to_str().unwrap()).unwrap();
    assert!(!raster.overviews().is_empty());
    let stats = |ds: &terrio::RasterDataset| ds.band_metadata.iter().any(|b| b.items.iter().any(|(k, _)| k.starts_with("STATISTICS_")));
    let full = ResampledReadOptions { size: (64, 64), ..Default::default() };
    assert!(stats(&raster.read_resampled(&full).unwrap()));
    let level = ResampledReadOptions { overview: OverviewSelection::Level(0), ..full };
    assert!(!stats(&raster.read_resampled(&level).unwrap()));
}

/// GDAL 3.12 (the default) sums the NaN values of a masked floating-point chunk, so an output
/// pixel whose footprint holds one is NaN; GDAL 3.13 (`TERRIO_GDAL_COMPAT=3.13`) skips them.
#[test]
fn masked_nan_follows_the_gdal_compat_version() {
    let path = format!("{}/tests/data/resampled_reads/f32_nan.tif", env!("CARGO_MANIFEST_DIR"));
    let options = ResampledReadOptions { size: (23, 17), resampling: Resampling::Bilinear, ..Default::default() };
    let read = || read_values(&terrio::open_raster_lazy(&path).unwrap().read_resampled(&options).unwrap().raster.data);
    let nans = |v: &[f64]| v.iter().filter(|x| x.is_nan()).count();
    // The default, whatever `TERRIO_GDAL_COMPAT` the process has: a scoped `None` unsets it.
    terrio::config::push_scope([("TERRIO_GDAL_COMPAT".to_string(), None)].into());
    let default = read();
    terrio::config::pop_scope();
    terrio::config::push_scope([("TERRIO_GDAL_COMPAT".to_string(), Some("3.12".to_string()))].into());
    let gdal312 = read();
    terrio::config::pop_scope();
    terrio::config::push_scope([("TERRIO_GDAL_COMPAT".to_string(), Some("3.13".to_string()))].into());
    let gdal313 = read();
    terrio::config::pop_scope();
    let bits = |v: &[f64]| v.iter().map(|x| if x.is_nan() { u64::MAX } else { x.to_bits() }).collect::<Vec<_>>();
    assert_eq!(bits(&default), bits(&gdal312));
    assert!(nans(&gdal312) > nans(&gdal313), "{} NaN with 3.12, {} with 3.13", nans(&gdal312), nans(&gdal313));
    // Where 3.13 skips the NaN values, 3.12's pixel is NaN.
    assert!(gdal313.iter().zip(&gdal312).any(|(a, b)| a.is_finite() && b.is_nan()));
}
