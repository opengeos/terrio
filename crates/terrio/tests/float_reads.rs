//! Floating-point window reads (`LazyRaster::read_window_f32` and `read_window_f64`, their
//! `RasterDataset` counterparts and, with the `ndarray` feature, their arrays) of the rasters of
//! `tests/data/float_reads`, against what GDAL 3.13.2 reads of them
//! (`scripts/make_float_reads_fixtures.py`): `ReadAsArray` into `Float32` and `Float64` buffers,
//! NaN where `GetMaskBand` is 0, unscaled values `value * scale + offset` computed in `Float64`.
//! Every value is compared bit for bit, any NaN matching any NaN.

use std::path::PathBuf;
use terrio::{FloatReadOptions, Window};

/// A read of a fixture and the values GDAL reads.
struct Case {
    file: String,
    options: FloatReadOptions,
    window: Window,
    /// Whether the raster has a mask of its own (internal or sidecar), which a dataset read into
    /// memory does not keep.
    per_dataset_mask: bool,
    f32: Vec<f32>,
    f64: Vec<f64>,
}

impl Case {
    /// The path terrio opens, a netCDF file's packed variable by its subdataset name.
    fn path(&self) -> String {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/float_reads").join(&self.file);
        let path = path.to_string_lossy().into_owned();
        if self.file.ends_with(".nc") { format!("NETCDF:\"{path}\":t") } else { path }
    }

    /// Whether this build reads the file.
    fn readable(&self) -> bool {
        cfg!(feature = "netcdf-hdf5") || !self.file.ends_with(".nc")
    }

    fn name(&self) -> String {
        let o = &self.options;
        format!("{} bands {:?} window {:?} overview {:?} unscale {}", self.file, o.bands, self.window, o.overview, o.unscale)
    }
}

fn cases() -> Vec<Case> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/float_reads");
    let text = std::fs::read_to_string(dir.join("expected.json")).unwrap();
    let expected: serde_json::Value = serde_json::from_str(&text).unwrap();
    let numbers = |v: &serde_json::Value| -> Vec<usize> { v.as_array().unwrap().iter().map(|n| n.as_u64().unwrap() as usize).collect() };
    // Text Rust's `f64::from_str` reads exactly, NaN and infinities included.
    let values =
        |v: &serde_json::Value| -> Vec<f64> { v.as_array().unwrap().iter().map(|s| s.as_str().unwrap().parse().unwrap()).collect() };
    expected
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let w = numbers(&c["window"]);
            Case {
                file: c["file"].as_str().unwrap().to_string(),
                options: FloatReadOptions {
                    bands: numbers(&c["bands"]),
                    overview: c["overview"].as_u64().map(|o| o as usize),
                    unscale: c["unscale"].as_bool().unwrap(),
                },
                window: Window::new(w[0], w[1], w[2], w[3]),
                per_dataset_mask: c["per_dataset_mask"].as_bool().unwrap(),
                f32: values(&c["f32"]).into_iter().map(|v| v as f32).collect(),
                f64: values(&c["f64"]),
            }
        })
        .collect()
}

/// The first value that differs, bit for bit, any NaN matching any NaN.
fn first_difference<T: Copy + Into<f64>>(read: &[T], expected: &[T]) -> Option<String> {
    let bits = |v: f64| if v.is_nan() { u64::MAX } else { v.to_bits() };
    if read.len() != expected.len() {
        return Some(format!("{} values, not {}", read.len(), expected.len()));
    }
    let i = read.iter().zip(expected).position(|(a, b)| bits((*a).into()) != bits((*b).into()))?;
    Some(format!("value {i} is {:?}, not {:?}", read[i].into(), expected[i].into()))
}

/// Every case read lazily, against GDAL 3.13.2's reads.
#[test]
fn lazy_reads_are_gdal_reads() {
    let cases = cases();
    assert_eq!(cases.len(), 38);
    for case in cases.iter().filter(|c| c.readable()) {
        let mut raster = terrio::open_raster_lazy(&case.path()).unwrap();
        let read = raster.read_window_f32(case.window, &case.options).unwrap();
        assert_eq!(first_difference(&read, &case.f32), None, "f32: {}", case.name());
        let read = raster.read_window_f64(case.window, &case.options).unwrap();
        assert_eq!(first_difference(&read, &case.f64), None, "f64: {}", case.name());
    }
}

/// Datasets read into memory: those without a mask of their own, at full resolution. A virtual
/// raster of bands of several types holds them in one type there, which their nodata values are
/// compared in (a lazy read compares each in its own type).
#[test]
fn in_memory_reads_are_gdal_reads() {
    let cases = cases();
    let in_memory: Vec<&Case> = cases
        .iter()
        .filter(|c| c.readable() && !c.per_dataset_mask && c.options.overview.is_none() && c.file != "mixed_types.vrt")
        .collect();
    assert_eq!(in_memory.len(), if cfg!(feature = "netcdf-hdf5") { 25 } else { 22 });
    for case in in_memory {
        let ds = terrio::open_raster(&case.path()).unwrap();
        let read = ds.read_window_f32(case.window, &case.options).unwrap();
        assert_eq!(first_difference(&read, &case.f32), None, "f32: {}", case.name());
        let read = ds.read_window_f64(case.window, &case.options).unwrap();
        assert_eq!(first_difference(&read, &case.f64), None, "f64: {}", case.name());
    }
}

/// The arrays of the reads, bands first and last, and of one band.
#[cfg(feature = "ndarray")]
#[test]
fn arrays_are_gdal_reads() {
    use terrio::BandLayout;
    use terrio::ndarray::Array3;

    for case in cases().iter().filter(|c| c.readable()) {
        let mut raster = terrio::open_raster_lazy(&case.path()).unwrap();
        let (rows, cols) = (case.window.height, case.window.width);
        let bands = case.f32.len() / (rows * cols);
        let expected = Array3::from_shape_vec((bands, rows, cols), case.f32.clone()).unwrap();
        let first = raster.read_window_array3::<f32>(case.window, &case.options, BandLayout::BandsFirst).unwrap();
        assert_eq!(first.dim(), (bands, rows, cols));
        assert_eq!(first_difference(first.as_slice().unwrap(), &case.f32), None, "bands first: {}", case.name());
        let last = raster.read_window_array3::<f32>(case.window, &case.options, BandLayout::BandsLast).unwrap();
        assert_eq!(last.dim(), (rows, cols, bands));
        // Written in standard layout, pixel after pixel: the values of the bands-first array,
        // its axes permuted.
        let permuted = expected.view().permuted_axes([1, 2, 0]).iter().copied().collect::<Vec<_>>();
        assert_eq!(first_difference(last.as_slice().unwrap(), &permuted), None, "bands last: {}", case.name());
        let doubles = raster.read_window_array3::<f64>(case.window, &case.options, BandLayout::BandsLast).unwrap();
        let expected = Array3::from_shape_vec((bands, rows, cols), case.f64.clone()).unwrap();
        let permuted = expected.view().permuted_axes([1, 2, 0]).iter().copied().collect::<Vec<_>>();
        assert_eq!(first_difference(doubles.as_slice().unwrap(), &permuted), None, "f64 bands last: {}", case.name());
        if bands == 1 {
            let band = raster.read_window_array2::<f64>(case.window, &case.options).unwrap();
            assert_eq!(band.dim(), (rows, cols));
            assert_eq!(first_difference(band.as_slice().unwrap(), &case.f64), None, "one band: {}", case.name());
        } else {
            assert!(raster.read_window_array2::<f32>(case.window, &case.options).is_err());
        }
        if !case.per_dataset_mask && case.options.overview.is_none() && case.file != "mixed_types.vrt" {
            let ds = terrio::open_raster(&case.path()).unwrap();
            let last = ds.read_window_array3::<f64>(case.window, &case.options, BandLayout::BandsLast).unwrap();
            assert_eq!(first_difference(last.as_slice().unwrap(), &permuted), None, "in memory, bands last: {}", case.name());
            if bands == 1 {
                let band = ds.read_window_array2::<f32>(case.window, &case.options).unwrap();
                assert_eq!(first_difference(band.as_slice().unwrap(), &case.f32), None, "in memory, one band: {}", case.name());
            }
        }
    }
}
