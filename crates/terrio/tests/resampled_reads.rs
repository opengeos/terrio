//! Reads into outputs of other sizes (`LazyRaster::read_resampled` and its masks) of the rasters
//! of `tests/data/resampled_reads`, against what GDAL 3.13.2 reads of them
//! (`scripts/make_resampled_reads_fixtures.py`): pixels through `RasterIO` (the dataset's for
//! several bands, as rasterio reads them, the band's for one) and masks through `GetMaskBand()`,
//! or, for a level read alone, the pixels `gdal_translate -ovr` writes. Values are compared bit
//! for bit, any NaN matching any NaN, but for roundings of fused multiply-adds (see
//! [`difference`]).

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
    let numbers = |v: &serde_json::Value| -> Vec<usize> { v.as_array().unwrap().iter().map(|n| n.as_u64().unwrap() as usize).collect() };
    expected
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
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
/// differences are allowed in at most 1% of a case's values; any other is an error.
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

/// Every case, against GDAL 3.13.2's reads.
#[test]
fn resampled_reads_are_gdal_reads() {
    let cases = cases();
    assert_eq!(cases.len(), 238);
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
