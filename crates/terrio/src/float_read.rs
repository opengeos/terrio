// Written to match the behavior of:
//   GDAL v3.13.2:
//     gcore/gdalrasterband.cpp (GDALRasterBand::GetMaskBand)
//     gcore/gdalnodatamaskband.cpp
//     gcore/rasterio.cpp (GDALCopyWords)
// No code is translated from these sources: nodata values are compared by the port of
// `GDALNoDataMaskBand` in `drivers::gdal_overview` (`Sample`).

//! Floating-point window reads: pixels as `f32` or `f64`, NaN where they are invalid, their
//! scales and offsets applied or not ([`LazyRaster::read_window_f32`]).

use crate::dataset::fallible;
use crate::drivers::gdal_overview::Sample;
use crate::pixel::with_data;
use crate::{Error, LazyRaster, RasterDataset, Result, Window, WindowOptions};
use wbraster::raster::RasterData;

/// What [`LazyRaster::read_window_f32`] and [`LazyRaster::read_window_f64`] read, and
/// [`RasterDataset::read_window_f32`] and [`RasterDataset::read_window_f64`]: bands and an
/// overview, as [`WindowOptions`] selects them, and whether values are unscaled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FloatReadOptions {
    /// The bands, numbered from 1 as in GDAL. Empty means all bands.
    pub bands: Vec<usize>,
    /// The overview to read, numbered from 0 (the largest) as in GDAL; `None` for full resolution.
    /// The window is in the overview's pixels.
    pub overview: Option<usize>,
    /// Whether to apply each band's scale and offset to its valid pixels (`value * scale +
    /// offset`), as `gdal raster unscale` does: the band's properties, or the packing of a netCDF
    /// variable (`scale_factor` and `add_offset`), as [`band_scale_offset`] reads them. Off by
    /// default: GDAL and rasterio read values as stored.
    ///
    /// [`band_scale_offset`]: crate::ops::pixel::band_scale_offset
    pub unscale: bool,
}

/// A sample type converted to floating point as GDAL's `GDALCopyWords` converts it: rounded once
/// to the nearest value (an `as` conversion), rather than through `f64`, which would round 64-bit
/// integers twice.
pub(crate) trait Cast: Sample {
    /// The sample as `f32`.
    fn to_f32(self) -> f32;
}

macro_rules! cast {
    ($($t:ty),*) => {$(
        impl Cast for $t {
            #[inline]
            fn to_f32(self) -> f32 {
                self as f32
            }
        }
    )*};
}
cast!(u8, i8, u16, i16, u32, i32, u64, i64, f32, f64);

/// A type of floating-point reads: `f32` or `f64`.
pub(crate) trait Float: Cast {
    /// The value of invalid pixels.
    const NAN: Self;
    /// A sample converted to this type.
    fn of<S: Cast>(sample: S) -> Self;
    /// A value computed in double precision, rounded to this type.
    fn of_f64(value: f64) -> Self;
    /// The values of a buffer of this type, leaving an empty one; `None` for another type.
    fn take(data: &mut RasterData) -> Option<Vec<Self>>;
}

impl Float for f32 {
    const NAN: Self = f32::NAN;
    #[inline]
    fn of<S: Cast>(sample: S) -> Self {
        sample.to_f32()
    }
    #[inline]
    fn of_f64(value: f64) -> Self {
        value as f32
    }
    fn take(data: &mut RasterData) -> Option<Vec<Self>> {
        match data {
            RasterData::F32(values) => Some(std::mem::take(values)),
            _ => None,
        }
    }
}

impl Float for f64 {
    const NAN: Self = f64::NAN;
    #[inline]
    fn of<S: Cast>(sample: S) -> Self {
        sample.to_f64()
    }
    #[inline]
    fn of_f64(value: f64) -> Self {
        value
    }
    fn take(data: &mut RasterData) -> Option<Vec<Self>> {
        match data {
            RasterData::F64(values) => Some(std::mem::take(values)),
            _ => None,
        }
    }
}

/// What the errors of the converted buffer name.
const WHAT: &str = "the floating-point window";

impl LazyRaster {
    /// Reads a window of some bands, at full resolution or from an overview, as `f32` values band
    /// after band, each band row after row from the top (an array of shape `(bands, rows,
    /// cols)`), NaN where pixels are invalid: the values of GDAL's `ReadAsArray` into a `Float32`
    /// buffer, with the pixels its `GetMaskBand` masks set to NaN (as GDAL 3.13 reads them).
    ///
    /// Which pixels are invalid is decided as GDAL's mask band decides it, alpha bands aside:
    ///
    /// - The per-dataset mask of the level read, when it has one ([`LazyRaster::read_mask`]: a
    ///   GeoTIFF's internal mask or a `.msk` sidecar; an overview's own mask, else the
    ///   full-resolution mask sampled by nearest neighbour): pixels are invalid where it is 0.
    ///   It replaces the bands' nodata values, as in GDAL: a pixel holding the nodata value that
    ///   the mask marks valid is read as a number.
    /// - Otherwise each band's nodata value ([`RasterDataset::band_nodata`]), compared in the
    ///   band's own type before anything is converted, as GDAL's nodata mask band compares it:
    ///   floating-point values within GDAL's relative tolerance (`ARE_REAL_EQUAL`; for `Float32`
    ///   bands, the nodata value rounded to `f32` first), NaN for a NaN nodata value, integers
    ///   equal to the nodata value cast to their type (a fractional value is truncated, 1.5 being
    ///   1 for a `Byte` band). A nodata value outside the type's range (-9999 for a `Byte` band,
    ///   1e39 for a `Float32` one) masks nothing.
    ///
    /// Alpha bands are read as bands, not taken for a mask (GDAL's `GMF_ALPHA`). Pixels already
    /// NaN stay NaN and infinite values are kept.
    ///
    /// Valid pixels are converted to `f32` as GDAL converts them: rounded once to the nearest
    /// `f32` (`Float64` values beyond `f32`'s range become infinite). With
    /// [`FloatReadOptions::unscale`], `value * scale + offset` is computed in `f64` and rounded to
    /// `f32` once, as `gdal raster unscale` computes it.
    ///
    /// # Precision
    ///
    /// `f32` holds every integer up to 2^24 exactly: `UInt32`, `Int32`, `UInt64` and `Int64`
    /// values beyond it are rounded, and so are `Float64` values. [`LazyRaster::read_window_f64`]
    /// holds integers up to 2^53 exactly, `UInt64` and `Int64` values beyond rounded. Nodata
    /// values are compared before rounding: `Int32` -2147483646 stays valid next to a nodata value
    /// of -2147483647, although both read as -2147483648 in `f32`. Unscaling converts values to
    /// `f64` first, rounding `UInt64` and `Int64` values beyond 2^53.
    ///
    /// # Errors
    ///
    /// When the window is not within the level read, a band or overview does not exist, the bands
    /// are complex ([`Error::Unsupported`]), the converted values would not fit in memory, or the
    /// read fails.
    ///
    /// ```no_run
    /// use terrio::{FloatReadOptions, Window};
    ///
    /// let mut cog = terrio::open_raster_lazy("https://example.com/cog.tif")?;
    /// let options = FloatReadOptions { bands: vec![1, 2], unscale: true, ..Default::default() };
    /// let values = cog.read_window_f32(Window::new(0, 0, 512, 512), &options)?;
    /// assert_eq!(values.len(), 2 * 512 * 512); // (bands, rows, cols)
    /// let valid = values.iter().filter(|v| !v.is_nan()).count();
    /// # Ok::<(), terrio::Error>(())
    /// ```
    pub fn read_window_f32(&mut self, window: Window, options: &FloatReadOptions) -> Result<Vec<f32>> {
        self.read_float(window, options, false)
    }

    /// Reads a window of some bands as `f64` values: see [`LazyRaster::read_window_f32`].
    ///
    /// # Errors
    ///
    /// As [`LazyRaster::read_window_f32`].
    pub fn read_window_f64(&mut self, window: Window, options: &FloatReadOptions) -> Result<Vec<f64>> {
        self.read_float(window, options, false)
    }

    /// The values of [`LazyRaster::read_window_f32`] as `T`, band after band, or with
    /// `bands_last` pixel after pixel (an array of shape `(rows, cols, bands)`).
    pub(crate) fn read_float<T: Float>(&mut self, window: Window, options: &FloatReadOptions, bands_last: bool) -> Result<Vec<T>> {
        let bands = float_bands(&options.bands, self.band_count(), self.is_complex())?;
        let read = |bands: Vec<usize>| WindowOptions { bands, overview: options.overview };
        // A source of bands of several types reads them in one type wide enough for all, which
        // nodata values are not compared in: each band is then read on its own, in its own type.
        let mixed = bands.iter().any(|&b| self.band_data_type(b) != self.band_data_type(bands[0]));
        let mut reads = if mixed {
            bands.iter().map(|&b| self.read_window(window, &read(vec![b]))).collect::<Result<Vec<_>>>()?
        } else {
            vec![self.read_window(window, &read(bands.clone()))?]
        };
        // GDAL's mask band: the level's per-dataset mask when it has one, else the bands' nodata.
        let mask = self.read_mask(window, options.overview)?;
        let n = window.width * window.height;
        if mask.as_ref().is_some_and(|m| m.len() != n) {
            return Err(Error::InvalidData(format!(
                "the mask of a {} x {} window is not one value per pixel",
                window.width, window.height
            )));
        }
        let header = options.unscale.then(|| self.header());
        let scales: Vec<_> = bands.iter().map(|&b| header.as_ref().and_then(|h| scale_offset(h, b))).collect();
        // Band `k`: the read holding it, its number there and where its pixels start.
        let at = |k: usize| if mixed { (k, 1, 0) } else { (0, k + 1, k * n) };
        let nodata: Vec<_> = (0..bands.len())
            .map(|k| {
                let (read, band, _) = at(k);
                if mask.is_some() { None } else { reads[read].band_nodata(band) }
            })
            .collect();
        let size = (window.width, window.height);
        if let [one] = reads.as_mut_slice()
            && !bands_last
            && one.raster.data.data_type() == T::DT
        {
            // Pixels read as `T`: converted where they are, without a second buffer.
            let nodata: Vec<_> = nodata.iter().map(|nd| nd.filter(|nd| T::nodata_in_range(*nd))).collect();
            let mut values = T::take(&mut one.raster.data).expect("pixels of the type checked");
            convert_in_place(&mut values, &nodata, &scales, size, mask.as_deref());
            return Ok(values);
        }
        let sources: Vec<_> = (0..bands.len())
            .map(|k| {
                let (read, _, start) = at(k);
                Band::new(&reads[read].raster.data, start, window.width, nodata[k], scales[k])
            })
            .collect();
        convert(&sources, size, mask.as_deref(), bands_last)
    }
}

impl RasterDataset {
    /// Reads a window of some bands as `f32` values band after band, NaN where pixels are
    /// invalid, as [`LazyRaster::read_window_f32`] reads them from a raster without a mask: NaN
    /// where pixels are their band's nodata value ([`RasterDataset::band_nodata`]). The window is
    /// read from the dataset's pixels, which are not copied.
    ///
    /// A dataset holds its bands in one type, which their nodata values are compared in: a virtual
    /// raster of bands of several types, read whole, holds them in a type wide enough for all,
    /// where a lazy read compares each band in its own.
    ///
    /// # Errors
    ///
    /// When the window is not within the raster, a band does not exist, an overview is asked for
    /// (a dataset in memory has none), the bands are complex ([`Error::Unsupported`]), the
    /// raster's buffer does not hold `bands × rows × cols` values ([`Error::InvalidData`]), or the
    /// converted values would not fit in memory.
    ///
    /// ```no_run
    /// use terrio::{FloatReadOptions, Window};
    ///
    /// let dem = terrio::open_raster("dem.tif")?;
    /// let window = Window::full(dem.raster.cols, dem.raster.rows);
    /// let heights = dem.read_window_f64(window, &FloatReadOptions::default())?;
    /// let highest = heights.iter().copied().filter(|z| !z.is_nan()).fold(f64::NEG_INFINITY, f64::max);
    /// # Ok::<(), terrio::Error>(())
    /// ```
    pub fn read_window_f32(&self, window: Window, options: &FloatReadOptions) -> Result<Vec<f32>> {
        self.read_float(window, options, false)
    }

    /// Reads a window of some bands as `f64` values: see [`RasterDataset::read_window_f32`].
    ///
    /// # Errors
    ///
    /// As [`RasterDataset::read_window_f32`].
    pub fn read_window_f64(&self, window: Window, options: &FloatReadOptions) -> Result<Vec<f64>> {
        self.read_float(window, options, false)
    }

    /// The values of [`RasterDataset::read_window_f32`] as `T`, band after band, or with
    /// `bands_last` pixel after pixel.
    pub(crate) fn read_float<T: Float>(&self, window: Window, options: &FloatReadOptions, bands_last: bool) -> Result<Vec<T>> {
        let r = &self.raster;
        let bands = float_bands(&options.bands, r.bands, self.complex)?;
        if let Some(i) = options.overview {
            return Err(Error::InvalidArgument(format!("no overview {i}: the raster has 0 overview(s)")));
        }
        window.check(r.cols, r.rows)?;
        let n = r.cols * r.rows;
        if r.bands.checked_mul(n) != Some(r.data.len()) {
            return Err(Error::InvalidData(format!(
                "the raster holds {} values for {} band(s) of {} x {} pixels",
                r.data.len(),
                r.bands,
                r.cols,
                r.rows
            )));
        }
        let first = window.y * r.cols + window.x;
        let sources: Vec<_> = bands
            .iter()
            .map(|&b| {
                let scale = if options.unscale { scale_offset(self, b) } else { None };
                Band::new(&r.data, (b - 1) * n + first, r.cols, self.band_nodata(b), scale)
            })
            .collect();
        convert(&sources, (window.width, window.height), None, bands_last)
    }
}

/// The bands `bands` selects of a raster of `count` bands (all for none), or the error of a band it
/// does not have, or of complex bands, which are not read as floating-point values.
pub(crate) fn float_bands(bands: &[usize], count: usize, complex: bool) -> Result<Vec<usize>> {
    if complex {
        return Err(Error::Unsupported("reading complex bands as floating-point values".into()));
    }
    crate::cog::read_bands(bands, count)
}

/// The scale and offset of a band (from 1) of `ds`, `None` for 1 and 0, which leave values as
/// they are.
fn scale_offset(ds: &RasterDataset, band: usize) -> Option<(f64, f64)> {
    Some(crate::ops::pixel::band_scale_offset(ds, band)).filter(|&scale| scale != (1.0, 0.0))
}

/// A band of a window, read: its pixels in their own type, and what is done to them.
struct Band<'a> {
    /// The pixels read, of this band and maybe others.
    data: &'a RasterData,
    /// The index in `data` of the band's first pixel of the window.
    start: usize,
    /// The distance in `data` from a row of the window to the next.
    stride: usize,
    /// The nodata value the pixels are compared with, `None` for none (or one outside their type's
    /// range).
    nodata: Option<f64>,
    /// The scale and offset applied to valid pixels, `None` for none.
    scale: Option<(f64, f64)>,
}

impl<'a> Band<'a> {
    fn new(data: &'a RasterData, start: usize, stride: usize, nodata: Option<f64>, scale: Option<(f64, f64)>) -> Self {
        // GDAL's `IsNoDataInRange`: a nodata value the type cannot hold masks nothing.
        let nodata = nodata.filter(|&nd| with_data!(data, v => in_range(v, nd)));
        Self { data, start, stride, nodata, scale }
    }

    /// Converts row `row` of the window, `width` pixels, into every `step`-th value of `out` from
    /// the first.
    fn row<T: Float>(&self, row: usize, width: usize, mask: Option<&[u8]>, out: &mut [T], step: usize) {
        let at = self.start + row * self.stride;
        with_data!(self.data, v => convert_row(&v[at..at + width], mask, self.nodata, self.scale, out, step))
    }
}

/// Whether a type of `S` samples holds `nodata`.
fn in_range<S: Sample>(_: &[S], nodata: f64) -> bool {
    S::nodata_in_range(nodata)
}

/// The pixels of `bands` of a `width` x `height` window as `T`, NaN where they are invalid (where
/// `mask` is 0, or their band's nodata value), band after band or, with `bands_last`, pixel after
/// pixel.
fn convert<T: Float>(bands: &[Band<'_>], (width, height): (usize, usize), mask: Option<&[u8]>, bands_last: bool) -> Result<Vec<T>> {
    let count = bands.len();
    let len = fallible::len(&[count, height, width], WHAT)?;
    fallible::check_usable(len.checked_mul(size_of::<T>()), || format!("{WHAT} ({width} x {height} x {count})"))
        .map_err(|message| Error::Io(std::io::Error::new(std::io::ErrorKind::OutOfMemory, message)))?;
    let mut out = fallible::filled(len, T::default(), WHAT)?;
    if len == 0 {
        return Ok(out);
    }
    // Chunks of whole rows of output: a row of a band, or a row of the pixels of every band.
    let row_len = if bands_last { width * count } else { width };
    let rows = crate::par::chunk_len(len / row_len, (1 << 14) / row_len);
    crate::par::chunks_mut(&mut out, rows * row_len, |i, chunk| {
        for (j, out) in chunk.chunks_mut(row_len).enumerate() {
            let row = i * rows + j;
            if bands_last {
                let mask = mask.map(|m| &m[row * width..][..width]);
                for (k, band) in bands.iter().enumerate() {
                    band.row(row, width, mask, &mut out[k..], count);
                }
            } else {
                let (band, row) = (row / height, row % height);
                bands[band].row(row, width, mask.map(|m| &m[row * width..][..width]), out, 1);
            }
        }
    });
    Ok(out)
}

/// [`convert`] of pixels read as `T` band after band, where they are: each band's nodata value (in
/// the type's range) and scale and offset.
fn convert_in_place<T: Float>(
    values: &mut [T],
    nodata: &[Option<f64>],
    scales: &[Option<(f64, f64)>],
    (width, height): (usize, usize),
    mask: Option<&[u8]>,
) {
    if values.is_empty() {
        return;
    }
    let rows = crate::par::chunk_len(values.len() / width, (1 << 14) / width);
    crate::par::chunks_mut(values, rows * width, |i, chunk| {
        for (j, values) in chunk.chunks_mut(width).enumerate() {
            let (band, row) = ((i * rows + j) / height, (i * rows + j) % height);
            let mask = mask.map(|m| &m[row * width..][..width]);
            for (c, v) in values.iter_mut().enumerate() {
                *v = value(*v, mask.is_none_or(|m| m[c] != 0), nodata[band], scales[band]);
            }
        }
    });
}

/// Converts a row of a band's samples into every `step`-th value of `out` from the first.
#[inline]
fn convert_row<S: Cast, T: Float>(
    src: &[S],
    mask: Option<&[u8]>,
    nodata: Option<f64>,
    scale: Option<(f64, f64)>,
    out: &mut [T],
    step: usize,
) {
    for (c, &x) in src.iter().enumerate() {
        out[c * step] = value(x, mask.is_none_or(|m| m[c] != 0), nodata, scale);
    }
}

/// A sample as `T`: NaN when the mask marks it invalid (`unmasked` false) or it is the nodata
/// value, else converted, its scale and offset applied in `f64` when given.
#[inline]
fn value<S: Cast, T: Float>(x: S, unmasked: bool, nodata: Option<f64>, scale: Option<(f64, f64)>) -> T {
    if !unmasked || nodata.is_some_and(|nd| x.is_nodata(nd)) {
        T::NAN
    } else if let Some((scale, offset)) = scale {
        T::of_f64(x.to_f64() * scale + offset)
    } else {
        T::of(x)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::vrt::VirtualRaster;
    use std::collections::HashMap;
    use std::path::Path;

    /// A dataset of `bands` bands of `cols` x `rows` pixels holding `data`, its nodata value
    /// `nodata`.
    fn dataset(data: RasterData, cols: usize, rows: usize, bands: usize, nodata: Option<f64>) -> RasterDataset {
        let cfg = wbraster::RasterConfig {
            cols,
            rows,
            bands,
            data_type: data.data_type(),
            nodata: nodata.unwrap_or(-9999.0),
            ..Default::default()
        };
        RasterDataset::new(crate::pixel::raster_from_data(cfg, data).unwrap(), nodata.is_some())
    }

    /// A row of pixels, of one band.
    fn row(data: RasterData, nodata: Option<f64>) -> RasterDataset {
        let cols = data.len();
        dataset(data, cols, 1, 1, nodata)
    }

    fn whole(ds: &RasterDataset) -> Window {
        Window::full(ds.raster.cols, ds.raster.rows)
    }

    /// Whether values are the same, bit for bit, any NaN matching any NaN.
    fn same<T: Copy + Into<f64>>(a: &[T], b: &[T]) -> bool {
        let bits = |v: f64| if v.is_nan() { u64::MAX } else { v.to_bits() };
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| bits((*x).into()) == bits((*y).into()))
    }

    fn f32s(ds: &RasterDataset) -> Vec<f32> {
        ds.read_window_f32(whole(ds), &FloatReadOptions::default()).unwrap()
    }

    fn f64s(ds: &RasterDataset) -> Vec<f64> {
        ds.read_window_f64(whole(ds), &FloatReadOptions::default()).unwrap()
    }

    /// Integers are compared with their nodata value in their own type, then rounded once, as
    /// GDAL 3.13's nodata mask band compares them and its `GDALCopyWords` converts them.
    #[test]
    fn integers_are_compared_with_nodata_before_rounding() {
        // -2147483646 and -2147483648 round to the nodata value's f32, -2147483648.
        let int32 = row(RasterData::I32(vec![-2_147_483_647, -2_147_483_646, i32::MIN, 16_777_217]), Some(-2_147_483_647.0));
        assert!(same(&f32s(&int32), &[f32::NAN, -2_147_483_648.0, -2_147_483_648.0, 16_777_216.0]));
        assert!(same(&f64s(&int32), &[f64::NAN, -2_147_483_646.0, -2_147_483_648.0, 16_777_217.0]));
        // 64-bit integers are compared exactly, and rounded once: 2^62 + 2^38 + 1 is 2^62 + 2^39
        // in f32, as GDAL converts it, not 2^62 as through f64.
        let big = (1i64 << 62) + (1 << 38) + 1;
        let int64 = row(RasterData::I64(vec![(1 << 53) + 1, 1 << 53, big, i64::MIN]), Some(9_007_199_254_740_992.0));
        assert!(same(&f32s(&int64), &[9_007_199_254_740_992.0, f32::NAN, 4_611_686_568_183_201_792.0, -9_223_372_036_854_775_808.0]));
        assert!(same(&f64s(&int64), &[9_007_199_254_740_992.0, f64::NAN, 4_611_686_293_305_294_848.0, -9_223_372_036_854_775_808.0]));
        // u64::MAX's nodata value is 2^64 as f64; u64::MAX - 1 is not it.
        let uint64 = row(RasterData::U64(vec![u64::MAX, u64::MAX - 1, 0]), Some(u64::MAX as f64));
        assert!(same(&f64s(&uint64), &[f64::NAN, 18_446_744_073_709_551_616.0, 0.0]));
    }

    /// The pixels GDAL 3.13's nodata mask band (`GDALNoDataMaskBand`) masks.
    #[test]
    fn nodata_values_follow_gdal_mask_bands() {
        let bytes = || RasterData::U8(vec![0, 1, 2, 255]);
        // A fractional nodata value is truncated to the type, as GDAL casts it.
        assert!(same(&f32s(&row(bytes(), Some(1.5))), &[0.0, f32::NAN, 2.0, 255.0]));
        // Values the type cannot hold mask nothing.
        for nodata in [-9999.0, 256.0, f64::NAN] {
            assert!(same(&f32s(&row(bytes(), Some(nodata))), &[0.0, 1.0, 2.0, 255.0]), "{nodata}");
        }
        assert!(same(&f32s(&row(RasterData::I8(vec![-2, -1, 0, 1]), Some(-1.5))), &[-2.0, f32::NAN, 0.0, 1.0]));
        // A Float32 nodata value is compared as f32, within GDAL's tolerance: -3.4e38 given as f64
        // is the pixels' -3.4e38 (and f32::MIN, as GDAL's float arithmetic overflows). NaN pixels
        // stay NaN and infinities are kept.
        let floats = || RasterData::F32(vec![-3.4e38, f32::MIN, f32::INFINITY, f32::NEG_INFINITY, f32::NAN, 1.5]);
        let read = f32s(&row(floats(), Some(-3.4e38)));
        assert!(same(&read, &[f32::NAN, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, f32::NAN, 1.5]));
        assert!(same(
            &f64s(&row(floats(), Some(f64::NAN))),
            &[-3.4e38f32 as f64, f32::MIN as f64, f64::INFINITY, f64::NEG_INFINITY, f64::NAN, 1.5]
        ));
        // Beyond f32's range: no Float32 pixel is the nodata value, infinities included.
        let read = f32s(&row(floats(), Some(1e39)));
        assert!(same(&read, &[-3.4e38, f32::MIN, f32::INFINITY, f32::NEG_INFINITY, f32::NAN, 1.5]));
        // Float64 values close to the nodata value are it; beyond f32's range, values read as f32
        // are infinite.
        let doubles = row(RasterData::F64(vec![-9999.0, -9999.0000001, -9998.99, 1e39, -1e39]), Some(-9999.0));
        assert!(same(&f64s(&doubles), &[f64::NAN, f64::NAN, -9998.99, 1e39, -1e39]));
        assert!(same(&f32s(&doubles), &[f32::NAN, f32::NAN, -9998.99, f32::INFINITY, f32::NEG_INFINITY]));
    }

    #[test]
    fn bands_have_their_own_nodata_values_and_layouts() {
        let mut ds = dataset(RasterData::U16(vec![0, 1, 2, 3, 10, 11, 12, 13]), 2, 2, 2, Some(1.0));
        ds.band_nodata = vec![Some(1.0), Some(12.0)];
        let options = FloatReadOptions::default();
        let nan = f32::NAN;
        assert!(same(&f32s(&ds), &[0.0, nan, 2.0, 3.0, 10.0, 11.0, nan, 13.0]));
        let last: Vec<f32> = ds.read_float(whole(&ds), &options, true).unwrap();
        assert!(same(&last, &[0.0, 10.0, nan, 11.0, 2.0, nan, 3.0, 13.0]));
        // A column of the second band, then the bands in another order, read from a window.
        let second = FloatReadOptions { bands: vec![2], ..Default::default() };
        assert!(same(&ds.read_window_f32(Window::new(1, 0, 1, 2), &second).unwrap(), &[11.0, 13.0]));
        let swapped = FloatReadOptions { bands: vec![2, 1], ..Default::default() };
        let read: Vec<f32> = ds.read_float(Window::new(0, 1, 2, 1), &swapped, true).unwrap();
        assert!(same(&read, &[nan, 2.0, 13.0, 3.0]));
    }

    /// `value * scale + offset` on valid pixels, as GDAL 3.13's `gdal raster unscale` computes it.
    #[test]
    fn values_are_unscaled_on_request() {
        let mut ds = row(RasterData::U16(vec![100, 0, 65535]), Some(0.0));
        ds.band_metadata = vec![crate::BandMetadata { scale: Some(0.01), offset: Some(-1.0), ..Default::default() }];
        let unscale = FloatReadOptions { unscale: true, ..Default::default() };
        let values = |v: f64| v * 0.01 - 1.0;
        let read = ds.read_window_f64(whole(&ds), &unscale).unwrap();
        assert!(same(&read, &[values(100.0), f64::NAN, values(65535.0)]));
        // Computed in f64, rounded to f32 once.
        let read = ds.read_window_f32(whole(&ds), &unscale).unwrap();
        assert!(same(&read, &[values(100.0) as f32, f32::NAN, values(65535.0) as f32]));
        assert!(same(&f64s(&ds), &[100.0, f64::NAN, 65535.0]));
        // A netCDF variable's packing.
        let mut packed = row(RasterData::I16(vec![-32767, 2, 3]), Some(-32767.0));
        packed.raster.metadata.push((format!("{}BAND_1_SCALE", crate::info::INTERNAL), "0.5".into()));
        packed.raster.metadata.push((format!("{}BAND_1_OFFSET", crate::info::INTERNAL), "273.15".into()));
        let read = packed.read_window_f64(whole(&packed), &unscale).unwrap();
        assert!(same(&read, &[f64::NAN, 2.0 * 0.5 + 273.15, 3.0 * 0.5 + 273.15]));
    }

    #[test]
    fn reads_are_checked() {
        let ds = dataset(RasterData::U8(vec![1; 8]), 2, 2, 2, None);
        let read = |window: Window, options: FloatReadOptions| ds.read_window_f32(window, &options).unwrap_err().to_string();
        assert!(read(Window::new(1, 0, 2, 1), FloatReadOptions::default()).contains("outside the 2x2 image"));
        assert!(read(Window::new(0, 0, 0, 1), FloatReadOptions::default()).contains("outside"));
        for band in [0, 3] {
            assert!(read(whole(&ds), FloatReadOptions { bands: vec![band], ..Default::default() }).contains(&format!("no band {band}")));
        }
        assert!(read(whole(&ds), FloatReadOptions { overview: Some(0), ..Default::default() }).contains("no overview 0"));
        let mut complex = ds.clone();
        complex.complex = true;
        let error = complex.read_window_f64(whole(&ds), &FloatReadOptions::default()).unwrap_err();
        assert!(matches!(error, Error::Unsupported(_)));
        assert!(matches!(LazyRaster::from(complex).read_window_f32(whole(&ds), &FloatReadOptions::default()), Err(Error::Unsupported(_))));
        // The raster's fields describe more pixels than its buffer holds.
        let mut short = ds.clone();
        short.raster.bands = 3;
        assert!(matches!(short.read_window_f32(whole(&ds), &FloatReadOptions::default()), Err(Error::InvalidData(_))));
    }

    #[test]
    fn lazy_reads_are_those_of_datasets_in_memory() {
        let values = vec![1.0, -9999.0, f32::NAN, 4.0, 5.0, 6.0, -9999.0, 8.0, 9.0, 10.0, 11.0, 12.0];
        let ds = dataset(RasterData::F32(values), 3, 2, 2, Some(-9999.0));
        let mut lazy = LazyRaster::from(ds.clone());
        let window = Window::new(1, 0, 2, 2);
        for bands in [vec![], vec![2], vec![2, 1]] {
            let options = FloatReadOptions { bands, ..Default::default() };
            // f32 pixels read as f32 band after band are converted in place.
            assert!(same(&lazy.read_window_f32(window, &options).unwrap(), &ds.read_window_f32(window, &options).unwrap()));
            assert!(same(&lazy.read_window_f64(window, &options).unwrap(), &ds.read_window_f64(window, &options).unwrap()));
            let last: Vec<f32> = lazy.read_float(window, &options, true).unwrap();
            assert!(same(&last, &ds.read_float::<f32>(window, &options, true).unwrap()));
        }
    }

    /// A per-dataset mask replaces the bands' nodata values, as in GDAL 3.13's `GetMaskBand`.
    #[test]
    fn per_dataset_masks_replace_nodata_values() {
        let ds = dataset(RasterData::U8(vec![0, 1, 2, 3, 0, 5, 6, 7]), 2, 2, 2, Some(0.0));
        let mut masked = LazyRaster::from_masked_dataset(ds.clone(), vec![255, 0, 255, 255]).unwrap();
        let read = masked.read_window_f32(whole(&ds), &FloatReadOptions::default()).unwrap();
        // The pixels of the nodata value that the mask marks valid are numbers.
        let nan = f32::NAN;
        assert!(same(&read, &[0.0, nan, 2.0, 3.0, 0.0, nan, 6.0, 7.0]));
        let last: Vec<f32> = masked.read_float(Window::new(0, 0, 2, 1), &FloatReadOptions::default(), true).unwrap();
        assert!(same(&last, &[0.0, 0.0, nan, nan]));
        assert!(same(&f32s(&ds), &[nan, 1.0, 2.0, 3.0, nan, 5.0, 6.0, 7.0]));
    }

    /// Each band of a virtual raster of several types has its nodata value compared in its own
    /// type, as GDAL 3.13's mask band of each band compares it.
    #[test]
    fn bands_of_several_types_are_compared_in_their_own() {
        // Read together, a UInt32 and a Float32 band are Float64, where 4294967294 is within
        // GDAL's tolerance of the nodata value 4294967295.
        let source = dataset(RasterData::F64(vec![4_294_967_295.0, 4_294_967_294.0, 1.5, 2.0]), 2, 1, 2, None);
        let band = |n: usize, dt: &str, nodata: &str| {
            format!(
                "<VRTRasterBand dataType=\"{dt}\" band=\"{n}\"><NoDataValue>{nodata}</NoDataValue><SimpleSource>\
                 <SourceFilename>source.tif</SourceFilename><SourceBand>{n}</SourceBand></SimpleSource></VRTRasterBand>"
            )
        };
        let xml = format!(
            "<VRTDataset rasterXSize=\"2\" rasterYSize=\"1\"><GeoTransform>0,1,0,1,0,-1</GeoTransform>{}{}</VRTDataset>",
            band(1, "UInt32", "4294967295"),
            band(2, "Float32", "1.5")
        );
        let sources = HashMap::from([("source.tif".to_string(), LazyRaster::from(source))]);
        let mut vrt = VirtualRaster::from_xml(&xml).unwrap().into_lazy(Path::new(""), sources).unwrap();
        let together = vrt.read_window(Window::full(2, 1), &WindowOptions::default()).unwrap();
        assert_eq!(together.raster.data.data_type(), wbraster::DataType::F64);
        let read = vrt.read_window_f64(Window::full(2, 1), &FloatReadOptions::default()).unwrap();
        assert!(same(&read, &[f64::NAN, 4_294_967_294.0, f64::NAN, 2.0]));
        let last: Vec<f32> = vrt.read_float(Window::full(2, 1), &FloatReadOptions::default(), true).unwrap();
        assert!(same(&last, &[f32::NAN, f32::NAN, 4_294_967_294.0, 2.0]));
    }
}
