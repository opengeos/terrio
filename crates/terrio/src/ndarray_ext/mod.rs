//! Raster pixels as `ndarray` arrays (the `ndarray` feature), without copying them.
//!
//! A raster's pixels are one typed buffer ([`Raster::data`]), band after band, each band row after
//! row from the top: the standard (row-major) layout of an array of shape `(bands, rows, cols)`.
//! [`RasterArrayExt`] views that buffer, whole or one band, borrowed or mutable; `owned` moves it
//! into owned arrays and back ([`RasterDataset::into_array3`]); `lazy` adapts the masks of lazy
//! reads. None of them copies, converts, reorders or reallocates pixels: a view points into the
//! raster's buffer, and an owned array is that buffer. Reading and decoding pixels or masks, which
//! allocates, is not part of this: these adapt buffers already read.
//!
//! [`RasterDataset::into_array3`]: crate::RasterDataset::into_array3

mod lazy;
mod owned;

pub use owned::ArrayError;

use crate::{Error, Raster, Result};
use ndarray::{ArrayView2, ArrayView3, ArrayViewMut2, ArrayViewMut3, Axis};
use wbraster::DataType;
use wbraster::raster::RasterData;

mod sealed {
    use wbraster::raster::RasterData;

    /// The variant of `RasterData` holding a type.
    pub trait Sealed: Sized {
        fn slice(data: &RasterData) -> Option<&[Self]>;
        fn slice_mut(data: &mut RasterData) -> Option<&mut [Self]>;
        /// The buffer, leaving an empty one (`Vec::new`, which allocates nothing) of the same type.
        fn take(data: &mut RasterData) -> Option<Vec<Self>>;
        fn wrap(values: Vec<Self>) -> RasterData;
    }

    /// The types `RasterArrayExt` is implemented for.
    pub trait Viewed {}

    impl Viewed for wbraster::Raster {}
}

/// A type of raster pixels, and of the arrays viewing them: `u8`, `i8`, `u16`, `i16`, `u32`, `i32`,
/// `u64`, `i64`, `f32` or `f64`, the types [`RasterData`]'s variants hold.
///
/// Implemented for these types only.
pub trait RasterElement: sealed::Sealed + Copy + Send + Sync + 'static {
    /// The data type of rasters holding this type: [`DataType::F32`] for `f32`.
    const DATA_TYPE: DataType;
}

macro_rules! element {
    ($($t:ty: $variant:ident, $slice:ident, $slice_mut:ident;)*) => {$(
        impl sealed::Sealed for $t {
            fn slice(data: &RasterData) -> Option<&[Self]> {
                data.$slice()
            }
            fn slice_mut(data: &mut RasterData) -> Option<&mut [Self]> {
                data.$slice_mut()
            }
            fn take(data: &mut RasterData) -> Option<Vec<Self>> {
                match data {
                    RasterData::$variant(values) => Some(std::mem::take(values)),
                    _ => None,
                }
            }
            fn wrap(values: Vec<Self>) -> RasterData {
                RasterData::$variant(values)
            }
        }

        impl RasterElement for $t {
            const DATA_TYPE: DataType = DataType::$variant;
        }
    )*};
}

element! {
    u8: U8, as_u8_slice, as_u8_slice_mut;
    i8: I8, as_i8_slice, as_i8_slice_mut;
    u16: U16, as_u16_slice, as_u16_slice_mut;
    i16: I16, as_i16_slice, as_i16_slice_mut;
    u32: U32, as_u32_slice, as_u32_slice_mut;
    i32: I32, as_i32_slice, as_i32_slice_mut;
    u64: U64, as_u64_slice, as_u64_slice_mut;
    i64: I64, as_i64_slice, as_i64_slice_mut;
    f32: F32, as_f32_slice, as_f32_slice_mut;
    f64: F64, as_f64_slice, as_f64_slice_mut;
}

/// Views of a raster's pixels as `ndarray` arrays, without copying them.
///
/// A raster's pixels are one typed buffer ([`Raster::data`]), band after band, each band row after
/// row from the top: the standard (row-major) layout of an array of shape `(bands, rows, cols)`,
/// which [`view3`](Self::view3) borrows and [`view3_mut`](Self::view3_mut) borrows mutably, and of
/// which [`band_view2`](Self::band_view2) views one band, of shape `(rows, cols)`. The views point
/// into the buffer: nothing is copied, converted, reordered or allocated, and a write through a
/// mutable view is a write to the raster. [`split_bands_mut`](Self::split_bands_mut) splits the
/// bands into views that do not overlap, to update them in parallel.
///
/// - The type viewed must be the type of the pixels (`f32` for [`DataType::F32`]): views of
///   another type fail ([`Error::InvalidArgument`]) rather than convert.
/// - The buffer must hold `bands × rows × cols` values of the raster's data type
///   ([`Raster::data_type`]). The fields of [`Raster`] are public and can disagree; views of a
///   raster whose fields do not match its buffer fail ([`Error::InvalidData`]) rather than panic,
///   and so do its moves into arrays, which could not move back.
/// - Bands are numbered from 0, as along the first axis of [`view3`](Self::view3) and as
///   [`Raster::band_view`] numbers them, rather than from 1 as [`WindowOptions::bands`] and GDAL do.
/// - Nodata pixels hold the nodata value, as stored (no NaN is substituted): see
///   [`RasterDataset::band_nodata`]. Masks are separate arrays
///   ([`LazyRaster::read_mask_array2`](crate::LazyRaster::read_mask_array2)).
/// - A complex dataset ([`RasterDataset::complex`]) is viewed as stored: twice as many bands, each
///   complex band's real parts then its imaginary parts, so that band `k` of the dataset (from 0)
///   is bands `2k` and `2k + 1` of the views.
///
/// Owned arrays are moved in and out of datasets by [`RasterDataset::into_array3`] and
/// [`RasterDataset::from_array3`], and their two-dimensional counterparts.
///
/// ```
/// use terrio::raster::RasterData;
/// use terrio::{DataType, Raster, RasterArrayExt, RasterConfig};
///
/// let config = RasterConfig { cols: 4, rows: 3, bands: 2, data_type: DataType::F32, ..Default::default() };
/// let mut raster = Raster::from_data_native(config, RasterData::F32((0..24).map(|v| v as f32).collect()))?;
/// let pixels = raster.view3::<f32>()?;
/// assert_eq!(pixels.dim(), (2, 3, 4)); // bands, rows, columns
/// assert_eq!(pixels[[1, 2, 3]], 23.0); // the last pixel of the second band
/// assert!(raster.view3::<f64>().is_err()); // not the type of the pixels
///
/// // Each band on its own (with rayon: `bands.into_par_iter().for_each(...)`).
/// let bands = raster.split_bands_mut::<f32>()?;
/// for mut band in bands {
///     band.mapv_inplace(|v| v * 2.0);
/// }
/// assert_eq!(raster.band_view2::<f32>(1)?[[0, 0]], 24.0);
/// # Ok::<(), terrio::Error>(())
/// ```
///
/// [`RasterDataset::band_nodata`]: crate::RasterDataset::band_nodata
/// [`RasterDataset::complex`]: crate::RasterDataset::complex
/// [`RasterDataset::into_array3`]: crate::RasterDataset::into_array3
/// [`RasterDataset::from_array3`]: crate::RasterDataset::from_array3
/// [`WindowOptions::bands`]: crate::WindowOptions::bands
pub trait RasterArrayExt: sealed::Viewed {
    /// The pixels as an array of shape `(bands, rows, cols)`, borrowed.
    fn view3<T: RasterElement>(&self) -> Result<ArrayView3<'_, T>>;

    /// The pixels as an array of shape `(bands, rows, cols)`, borrowed mutably.
    fn view3_mut<T: RasterElement>(&mut self) -> Result<ArrayViewMut3<'_, T>>;

    /// A band, numbered from 0, as an array of shape `(rows, cols)`, borrowed.
    fn band_view2<T: RasterElement>(&self, band: usize) -> Result<ArrayView2<'_, T>>;

    /// A band, numbered from 0, as an array of shape `(rows, cols)`, borrowed mutably.
    fn band_view2_mut<T: RasterElement>(&mut self, band: usize) -> Result<ArrayViewMut2<'_, T>>;

    /// Every band, in order, as an array of shape `(rows, cols)`, borrowed mutably: views that do
    /// not overlap, which can be sent to other threads (rayon's `into_par_iter` takes the vector).
    /// The vector of views is allocated, not the pixels.
    fn split_bands_mut<T: RasterElement>(&mut self) -> Result<Vec<ArrayViewMut2<'_, T>>>;
}

impl RasterArrayExt for Raster {
    fn view3<T: RasterElement>(&self) -> Result<ArrayView3<'_, T>> {
        let shape = check::<T>(self)?;
        let values = T::slice(&self.data).ok_or_else(|| type_mismatch::<T>(self.data.data_type()))?;
        ArrayView3::from_shape(shape, values).map_err(|e| bad_shape(shape, e))
    }

    fn view3_mut<T: RasterElement>(&mut self) -> Result<ArrayViewMut3<'_, T>> {
        let (shape, stored) = (check::<T>(self)?, self.data.data_type());
        let values = T::slice_mut(&mut self.data).ok_or_else(|| type_mismatch::<T>(stored))?;
        ArrayViewMut3::from_shape(shape, values).map_err(|e| bad_shape(shape, e))
    }

    fn band_view2<T: RasterElement>(&self, band: usize) -> Result<ArrayView2<'_, T>> {
        let pixels = self.view3::<T>()?;
        check_band(band, self.bands)?;
        Ok(pixels.index_axis_move(Axis(0), band))
    }

    fn band_view2_mut<T: RasterElement>(&mut self, band: usize) -> Result<ArrayViewMut2<'_, T>> {
        let bands = self.bands;
        let pixels = self.view3_mut::<T>()?;
        check_band(band, bands)?;
        Ok(pixels.index_axis_move(Axis(0), band))
    }

    fn split_bands_mut<T: RasterElement>(&mut self) -> Result<Vec<ArrayViewMut2<'_, T>>> {
        let pixels = self.view3_mut::<T>()?;
        let mut bands = Vec::with_capacity(pixels.len_of(Axis(0)));
        bands.extend(pixels.into_outer_iter_mut());
        Ok(bands)
    }
}

/// The `(bands, rows, cols)` shape of a raster whose buffer holds values of its data type, which is
/// `T`, and as many as the shape has pixels.
fn check<T: RasterElement>(raster: &Raster) -> Result<(usize, usize, usize)> {
    let (declared, stored) = (raster.data_type, raster.data.data_type());
    if declared != stored {
        let name = crate::info::gdal_type_name;
        return Err(Error::InvalidData(format!("the raster's data type is {}, but its pixels are {}", name(declared), name(stored))));
    }
    if stored != T::DATA_TYPE {
        return Err(type_mismatch::<T>(stored));
    }
    let (bands, rows, cols, len) = (raster.bands, raster.rows, raster.cols, raster.data.len());
    if bands.checked_mul(rows).and_then(|n| n.checked_mul(cols)) != Some(len) {
        return Err(Error::InvalidData(format!("the raster holds {len} values for {bands} band(s) of {cols} x {rows} pixels")));
    }
    Ok((bands, rows, cols))
}

/// The error of a view of `T` of pixels of another type.
fn type_mismatch<T: RasterElement>(stored: DataType) -> Error {
    let name = crate::info::gdal_type_name;
    Error::InvalidArgument(format!("the raster's pixels are {}, not {}", name(stored), name(T::DATA_TYPE)))
}

/// The error of a shape `ndarray` does not index: no pixels, but other dimensions whose product
/// exceeds `isize::MAX`.
fn bad_shape((bands, rows, cols): (usize, usize, usize), e: ndarray::ShapeError) -> Error {
    Error::InvalidData(format!("{bands} band(s) of {cols} x {rows} pixels: {e}"))
}

fn check_band(band: usize, bands: usize) -> Result<()> {
    if band < bands {
        return Ok(());
    }
    Err(Error::InvalidArgument(format!("no band {band}: the raster has {bands} band(s), numbered from 0")))
}
