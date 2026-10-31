//! Floating-point window reads as owned arrays: the values of [`LazyRaster::read_window_f32`] and
//! [`RasterDataset::read_window_f32`], `f32` or `f64`, NaN where pixels are invalid, in arrays of
//! shape `(bands, rows, cols)` or `(rows, cols, bands)` ([`BandLayout`]), or `(rows, cols)` for one
//! band. The values are converted in the layout asked for as they are read.

use super::{RasterElement, sealed};
use crate::float_read::float_bands;
use crate::{Error, FloatReadOptions, LazyRaster, RasterDataset, Result, Window};
use ndarray::{Array2, Array3, Axis};
use wbraster::DataType;
use wbraster::raster::RasterData;

/// Where the bands are in the three-dimensional arrays of floating-point window reads
/// ([`LazyRaster::read_window_array3`], [`RasterDataset::read_window_array3`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum BandLayout {
    /// Shape `(bands, rows, cols)`: band after band, as terrio stores pixels and
    /// [`RasterArrayExt::view3`](super::RasterArrayExt::view3) views them.
    #[default]
    BandsFirst,
    /// Shape `(rows, cols, bands)`: the bands of each pixel together, as image processing and
    /// machine learning libraries often take them.
    BandsLast,
}

/// A floating-point type of window reads as arrays ([`LazyRaster::read_window_array3`]): `f32` or
/// `f64`.
///
/// Implemented for these types only.
pub trait FloatElement: RasterElement + sealed::Floating {}

impl FloatElement for f32 {}
impl FloatElement for f64 {}

#[cfg_attr(docsrs, doc(cfg(feature = "ndarray")))]
impl LazyRaster {
    /// Reads a window of some bands, at full resolution or from an overview, as an array of `f32`
    /// or `f64` values, NaN where pixels are invalid: the values of
    /// [`LazyRaster::read_window_f32`] (which tells which pixels are invalid and how values are
    /// converted), of shape `(bands, rows, cols)` or `(rows, cols, bands)`.
    ///
    /// The values are written in the layout asked for as they are converted: bands last takes no
    /// second pass over them. Bands first, pixels read as the type asked for (`f32` for a
    /// `Float32` raster) are converted where they are read, and the array takes that buffer over.
    ///
    /// # Errors
    ///
    /// As [`LazyRaster::read_window_f32`].
    ///
    /// ```no_run
    /// use terrio::{BandLayout, FloatReadOptions, Window};
    ///
    /// let mut image = terrio::open_raster_lazy("https://example.com/cog.tif")?;
    /// let options = FloatReadOptions { bands: vec![1, 2, 3], ..Default::default() };
    /// let window = Window::new(0, 0, 256, 256);
    /// let pixels = image.read_window_array3::<f32>(window, &options, BandLayout::BandsLast)?;
    /// assert_eq!(pixels.dim(), (256, 256, 3)); // rows, columns, bands
    /// # Ok::<(), terrio::Error>(())
    /// ```
    pub fn read_window_array3<T: FloatElement>(
        &mut self,
        window: Window,
        options: &FloatReadOptions,
        layout: BandLayout,
    ) -> Result<Array3<T>> {
        let last = layout == BandLayout::BandsLast;
        let data = match T::DATA_TYPE {
            DataType::F32 => RasterData::F32(self.read_float(window, options, last)?),
            _ => RasterData::F64(self.read_float(window, options, last)?),
        };
        array3(data, window, layout)
    }

    /// Reads a window of one band, at full resolution or from an overview, as an array of shape
    /// `(rows, cols)` of `f32` or `f64` values, NaN where pixels are invalid: the values of
    /// [`LazyRaster::read_window_f32`]. `options` selects one band, or the raster has one.
    ///
    /// # Errors
    ///
    /// As [`LazyRaster::read_window_f32`], and when `options` selects more than one band.
    pub fn read_window_array2<T: FloatElement>(&mut self, window: Window, options: &FloatReadOptions) -> Result<Array2<T>> {
        one_band(float_bands(&options.bands, self.band_count(), self.is_complex())?)?;
        Ok(self.read_window_array3::<T>(window, options, BandLayout::BandsFirst)?.index_axis_move(Axis(0), 0))
    }
}

#[cfg_attr(docsrs, doc(cfg(feature = "ndarray")))]
impl RasterDataset {
    /// Reads a window of some bands as an array of `f32` or `f64` values, NaN where pixels are
    /// their band's nodata value: the values of [`RasterDataset::read_window_f32`], of shape
    /// `(bands, rows, cols)` or `(rows, cols, bands)`, written in that layout as they are
    /// converted.
    ///
    /// # Errors
    ///
    /// As [`RasterDataset::read_window_f32`].
    ///
    /// ```no_run
    /// use terrio::{BandLayout, FloatReadOptions, Window};
    ///
    /// let image = terrio::open_raster("image.tif")?;
    /// let window = Window::full(image.raster.cols, image.raster.rows);
    /// let pixels = image.read_window_array3::<f64>(window, &FloatReadOptions::default(), BandLayout::BandsLast)?;
    /// assert_eq!(pixels.dim(), (image.raster.rows, image.raster.cols, image.raster.bands));
    /// # Ok::<(), terrio::Error>(())
    /// ```
    pub fn read_window_array3<T: FloatElement>(&self, window: Window, options: &FloatReadOptions, layout: BandLayout) -> Result<Array3<T>> {
        let last = layout == BandLayout::BandsLast;
        let data = match T::DATA_TYPE {
            DataType::F32 => RasterData::F32(self.read_float(window, options, last)?),
            _ => RasterData::F64(self.read_float(window, options, last)?),
        };
        array3(data, window, layout)
    }

    /// Reads a window of one band as an array of shape `(rows, cols)` of `f32` or `f64` values,
    /// NaN where pixels are the band's nodata value: the values of
    /// [`RasterDataset::read_window_f32`]. `options` selects one band, or the raster has one.
    ///
    /// # Errors
    ///
    /// As [`RasterDataset::read_window_f32`], and when `options` selects more than one band.
    pub fn read_window_array2<T: FloatElement>(&self, window: Window, options: &FloatReadOptions) -> Result<Array2<T>> {
        one_band(float_bands(&options.bands, self.raster.bands, self.complex)?)?;
        Ok(self.read_window_array3::<T>(window, options, BandLayout::BandsFirst)?.index_axis_move(Axis(0), 0))
    }
}

/// The error of a selection of other than one band, for a two-dimensional array.
fn one_band(bands: Vec<usize>) -> Result<()> {
    match bands.len() {
        1 => Ok(()),
        n => Err(Error::InvalidArgument(format!("{n} bands are selected: a two-dimensional array holds one"))),
    }
}

/// The array of the `T` values `data` holds, of a window of `values.len() / (rows × cols)` bands
/// in `layout`: their buffer, moved rather than copied.
fn array3<T: FloatElement>(mut data: RasterData, window: Window, layout: BandLayout) -> Result<Array3<T>> {
    let values = T::take(&mut data).expect("f32 or f64 values, read as the type asked for");
    let (rows, cols) = (window.height, window.width);
    let bands = values.len() / (rows * cols).max(1);
    let shape = match layout {
        BandLayout::BandsFirst => (bands, rows, cols),
        BandLayout::BandsLast => (rows, cols, bands),
    };
    Array3::from_shape_vec(shape, values).map_err(|e| Error::InvalidData(format!("{bands} band(s) of a {cols} x {rows} window: {e}")))
}
