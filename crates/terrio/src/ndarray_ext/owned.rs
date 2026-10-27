//! Owned transfers: a dataset's pixel buffer moved into an owned array, and an owned array's buffer
//! moved into a dataset, without copying either.
//!
//! The stride handling in `into_buffer` and `rebuild` depends on how `ndarray` lays out owned arrays:
//! the offset of their elements in their buffer, and the strides of axes of length 1. The tests in
//! `tests/ndarray.rs` cover it; a patch release of `ndarray` that changes it would show there.

// The errors give the input back, a dataset as large as those the moves return on success: boxing
// it would allocate on every failure.
#![allow(clippy::result_large_err)]

use super::{RasterArrayExt, RasterElement};
use crate::{Error, RasterDataset};
use ndarray::{Array, Array1, Array2, Array3, ArrayD, Axis, Dimension, Ix2, IxDyn, ShapeBuilder, Slice, s};
use std::fmt;

/// A failed move of pixels between a dataset and an owned array: why, with the input given back.
///
/// [`ArrayError::into_input`] gives the input back as it was, its pixel buffer neither copied nor
/// dropped. The error converts into [`Error`], dropping the input, so that `?` returns it from
/// functions returning [`Result`](crate::Result).
pub struct ArrayError<I> {
    error: Error,
    input: I,
}

impl<I> ArrayError<I> {
    fn new(error: Error, input: I) -> Self {
        Self { error, input }
    }

    /// Why the move failed.
    pub fn error(&self) -> &Error {
        &self.error
    }

    /// The input, as it was given.
    pub fn into_input(self) -> I {
        self.input
    }

    /// The error and the input.
    pub fn into_parts(self) -> (Error, I) {
        (self.error, self.input)
    }
}

impl<I> fmt::Debug for ArrayError<I> {
    // Without the input, which can be millions of pixels.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArrayError").field("error", &self.error).finish_non_exhaustive()
    }
}

impl<I> fmt::Display for ArrayError<I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.error, f)
    }
}

impl<I> std::error::Error for ArrayError<I> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.source()
    }
}

impl<I> From<ArrayError<I>> for Error {
    fn from(e: ArrayError<I>) -> Self {
        e.error
    }
}

#[cfg_attr(docsrs, doc(cfg(feature = "ndarray")))]
impl RasterDataset {
    /// Moves the pixels into an owned array of shape `(bands, rows, cols)`, without copying them,
    /// and returns it with the rest of the dataset: a header, the dataset without pixels (its buffer
    /// empty) as [`LazyRaster::header`](crate::LazyRaster::header) returns one, which
    /// [`RasterDataset::from_array3`] takes back.
    ///
    /// The array takes over the pixel buffer as it is, capacity included: nothing is copied,
    /// converted, reordered or allocated. Its pixels are those [`RasterArrayExt::view3`] views:
    /// nodata values as stored, the component bands of a complex dataset. The header keeps the
    /// georeferencing, the CRS, the nodata values ([`RasterDataset::band_nodata`]), the band
    /// metadata, the GCPs and the rest of the metadata.
    ///
    /// # Errors
    ///
    /// When `T` is not the type of the pixels, or the buffer does not hold `bands × rows × cols`
    /// values of the raster's data type (as for [`RasterArrayExt::view3`]). The error gives the
    /// dataset back.
    ///
    /// ```
    /// use terrio::raster::RasterData;
    /// use terrio::{DataType, Raster, RasterConfig, RasterDataset};
    ///
    /// let config = RasterConfig { cols: 3, rows: 2, bands: 1, data_type: DataType::I16, nodata: -1.0, ..Default::default() };
    /// let raster = Raster::from_data_native(config, RasterData::I16(vec![1, 2, -1, 4, 5, 6]))?;
    /// let (mut pixels, header) = RasterDataset::new(raster, true).into_array3::<i16>()?;
    /// assert_eq!(pixels.dim(), (1, 2, 3));
    /// pixels.mapv_inplace(|v| if v == -1 { v } else { v * 10 }); // nodata stays nodata
    /// let ds = RasterDataset::from_array3(header, pixels)?;
    /// assert_eq!(ds.raster.data.as_i16_slice(), Some(&[10, 20, -1, 40, 50, 60][..]));
    /// assert_eq!(ds.nodata(), Some(-1.0));
    /// # Ok::<(), terrio::Error>(())
    /// ```
    pub fn into_array3<T: RasterElement>(self) -> Result<(Array3<T>, RasterDataset), ArrayError<RasterDataset>> {
        let shape = self.raster.view3::<T>().map(|pixels| pixels.raw_dim());
        into_array(self, shape)
    }

    /// Moves the pixels of a raster of one band into an owned array of shape `(rows, cols)`, as
    /// [`RasterDataset::into_array3`] does; [`RasterDataset::from_array2`] takes the header back.
    ///
    /// # Errors
    ///
    /// As [`RasterDataset::into_array3`], and when the raster does not have one band (a complex
    /// band is two). The error gives the dataset back.
    pub fn into_array2<T: RasterElement>(self) -> Result<(Array2<T>, RasterDataset), ArrayError<RasterDataset>> {
        let shape = self.raster.view3::<T>().and_then(|pixels| match pixels.dim() {
            (1, rows, cols) => Ok(Ix2(rows, cols)),
            (bands, ..) => Err(Error::InvalidArgument(format!("the raster has {bands} bands, not one"))),
        });
        into_array(self, shape)
    }

    /// A dataset of the metadata of `header` and the pixels of an owned array of shape `(bands,
    /// rows, cols)`, whose buffer it takes over without copying it: the inverse of
    /// [`RasterDataset::into_array3`].
    ///
    /// `header` is the dataset whose pixels the array's replace: any pixels it holds are dropped.
    /// It is normally one without pixels: the header `into_array3` returns, or
    /// [`LazyRaster::header`](crate::LazyRaster::header). Its bands, rows, columns and data type
    /// must be the array's. Nothing else in it is checked or changed: its nodata values, for one,
    /// still describe the pixels.
    ///
    /// The array must be in standard (row-major) layout and own exactly its elements: arrays made
    /// by `from_shape_vec`, `zeros` or `into_array3`, for example, but not transposed ones, whose
    /// elements would be reordered, nor slices of a larger array, which would be copied. Its buffer
    /// becomes the raster's as it is, capacity included.
    ///
    /// `ndarray` tells whether an owned array's elements are the whole of its buffer only by giving
    /// the buffer up, and builds owned arrays only from whole buffers: an array is given back
    /// exactly by rebuilding it on its buffer. Its axes of length 1, whose strides address nothing,
    /// must therefore have strides that can be rebuilt: 0 (those of a slice), plus or minus 1 (of
    /// `insert_axis`), the array's length or the stride of one of its other axes (of construction,
    /// reshaping or permuted axes). Arrays with other strides there are refused before anything
    /// is taken apart; slicing such an axis (`slice_axis_inplace`) sets its stride to 0.
    /// Empty arrays must also fit an extra axis of length 2 for rebuilding: the product of their
    /// nonzero axis lengths must be at most `isize::MAX / 2`.
    ///
    /// # Errors
    ///
    /// When the shapes, types or layouts differ. The error gives the header and the array back as
    /// they were: the array on the same buffer, with the same first element, shape and strides.
    pub fn from_array3<T: RasterElement>(
        header: RasterDataset,
        array: Array3<T>,
    ) -> Result<RasterDataset, ArrayError<(RasterDataset, Array3<T>)>> {
        let (bands, rows, cols) = array.dim();
        with_buffer(header, array, (bands, rows, cols))
    }

    /// A dataset of the metadata of `header` and the pixels of an owned array of shape `(rows,
    /// cols)`, as [`RasterDataset::from_array3`] makes one: `header` has one band.
    ///
    /// # Errors
    ///
    /// As [`RasterDataset::from_array3`]. The error gives the header and the array back.
    pub fn from_array2<T: RasterElement>(
        header: RasterDataset,
        array: Array2<T>,
    ) -> Result<RasterDataset, ArrayError<(RasterDataset, Array2<T>)>> {
        let (rows, cols) = array.dim();
        with_buffer(header, array, (1, rows, cols))
    }
}

/// Moves the pixel buffer of `ds` into an array of `shape`, the shape of a view of the same pixels.
fn into_array<T: RasterElement, D: Dimension>(
    mut ds: RasterDataset,
    shape: crate::Result<D>,
) -> Result<(Array<T, D>, RasterDataset), ArrayError<RasterDataset>> {
    let shape = match shape {
        Ok(shape) => shape,
        Err(error) => return Err(ArrayError::new(error, ds)),
    };
    let values = T::take(&mut ds.raster.data).expect("the type of a view");
    let array = Array::from_shape_vec(shape, values).expect("the shape of a view");
    Ok((array, ds))
}

/// `header` with the buffer of an array of `(bands, rows, cols)` pixels.
#[allow(clippy::type_complexity)]
fn with_buffer<T: RasterElement, D: Dimension>(
    mut header: RasterDataset,
    array: Array<T, D>,
    shape: (usize, usize, usize),
) -> Result<RasterDataset, ArrayError<(RasterDataset, Array<T, D>)>> {
    let (r, (bands, rows, cols)) = (&header.raster, shape);
    let name = crate::info::gdal_type_name;
    let mismatch = if (r.bands, r.rows, r.cols) != shape {
        Some(format!(
            "the array's {bands} band(s) of {cols} x {rows} pixels are not the header's {} band(s) of {} x {}",
            r.bands, r.cols, r.rows
        ))
    } else if r.data_type != T::DATA_TYPE {
        Some(format!("the array's {} values are not the header's {} pixels", name(T::DATA_TYPE), name(r.data_type)))
    } else {
        None
    };
    if let Some(error) = mismatch {
        return Err(ArrayError::new(Error::InvalidArgument(error), (header, array)));
    }
    match into_buffer(array) {
        Ok(values) => {
            header.raster.data = T::wrap(values);
            Ok(header)
        }
        Err((error, array)) => Err(ArrayError::new(error, (header, array))),
    }
}

/// The buffer of an array in standard layout that owns exactly its elements. Otherwise the array as
/// it was (its buffer, first element and strides), with why its buffer cannot be moved without
/// copying.
///
/// `ndarray` tells where an owned array's elements are in its buffer only by giving the buffer up,
/// and builds owned arrays only from whole buffers: an array is taken apart only when [`rebuild`]
/// can build it again exactly, which leaves out non-empty arrays with a stride along an axis of
/// length 1 that it does not restore, and empty arrays whose shapes cannot hold an extra axis of
/// length 2.
fn into_buffer<T: RasterElement, D: Dimension>(array: Array<T, D>) -> Result<Vec<T>, (Error, Array<T, D>)> {
    if !array.is_standard_layout() {
        let error = "the array is not in standard (row-major) layout: moving its buffer would reorder its elements";
        return Err((Error::InvalidArgument(error.into()), array));
    }
    if let Some((axis, stride)) = unrestorable_stride(&array) {
        let error = format!(
            "the array's axis {axis}, of length 1, has stride {stride}: moved arrays have 0, ±1, ±{} or the stride of another axis \
             there, so that a failed move gives them back unchanged",
            array.len()
        );
        return Err((Error::InvalidArgument(error), array));
    }
    if array.is_empty() {
        // Rebuilding needs an extra axis of length 2; ndarray bounds the product of nonzero axis
        // lengths even when an array is empty.
        let size = array.shape().iter().try_fold(2usize, |n, &d| n.checked_mul(d.max(1)));
        if size.is_none_or(|n| n > isize::MAX as usize) {
            let error = "the empty array is too large to rebuild with an extra axis of length 2";
            return Err((Error::InvalidArgument(error.into()), array));
        }
    }
    let (first, len, shape) = (array.as_ptr(), array.len(), array.raw_dim());
    let mut strides = shape.clone();
    for (axis, &stride) in array.strides().iter().enumerate() {
        strides[axis] = stride as usize;
    }
    let (values, _) = array.into_raw_vec_and_offset();
    // In elements, for empty arrays too, whose offset `into_raw_vec_and_offset` does not tell.
    let offset = (first as usize - values.as_ptr() as usize) / std::mem::size_of::<T>();
    if offset == 0 && values.len() == len {
        return Ok(values);
    }
    let error =
        format!("the array is {len} of the {} elements of its buffer, from element {offset}: moving it would copy them", values.len());
    let array = rebuild(values, offset, shape.clone(), strides.clone());
    debug_assert!(
        array.as_ptr() == first && array.raw_dim() == shape && (0..shape.ndim()).all(|k| array.strides()[k] as usize == strides[k])
    );
    Err((Error::InvalidArgument(error), array))
}

/// An axis of length 1 of a non-empty array, with its stride, that [`rebuild`] does not restore: it
/// restores 0, and plus or minus 1, the array's length and the strides of the axes longer than 1.
fn unrestorable_stride<T, D: Dimension>(array: &Array<T, D>) -> Option<(usize, isize)> {
    if array.is_empty() {
        return None;
    }
    let (shape, strides, len) = (array.shape(), array.strides(), array.len());
    let restored = |s: usize| s <= 1 || s == len || shape.iter().zip(strides).any(|(&n, &t)| n > 1 && t.unsigned_abs() == s);
    let axes = shape.iter().zip(strides).enumerate();
    axes.filter(|&(_, (&n, &s))| n == 1 && !restored(s.unsigned_abs())).map(|(axis, (_, &s))| (axis, s)).next()
}

/// The array of `shape` and `strides` whose first element is element `offset` of `values`: one that
/// [`into_buffer`] took apart.
fn rebuild<T, D: Dimension>(values: Vec<T>, offset: usize, shape: D, strides: D) -> Array<T, D> {
    let n = shape.ndim();
    let array = if shape.size() == 0 {
        // An empty array addresses nothing: ndarray takes it with any strides, and with a first axis
        // whose stride moves its first element there from where ndarray puts it, past the elements
        // its negative strides reach.
        let negative = |k: usize| (strides[k] as isize).is_negative() && shape[k] > 1;
        let reached: usize = (0..n).filter(|&k| negative(k)).map(|k| (shape[k] - 1) * (strides[k] as isize).unsigned_abs()).sum();
        let dims: Vec<usize> = std::iter::once(2).chain((0..n).map(|k| shape[k])).collect();
        let steps: Vec<usize> = std::iter::once(offset - reached).chain((0..n).map(|k| strides[k])).collect();
        let array = ArrayD::from_shape_vec(IxDyn(&dims).strides(IxDyn(&steps)), values);
        array.expect("an empty array within its buffer").index_axis_move(Axis(0), 1)
    } else {
        // Standard layout: the axes longer than 1 have the strides of the shape. An axis of length 1
        // takes the stride of where it is placed among them when the elements are shaped (the
        // product of the lengths that follow), then is moved back, and inverted or sliced for a
        // negative stride or 0.
        let long: Vec<usize> = (0..n).filter(|&k| shape[k] > 1).collect();
        let place = |k: usize| match (strides[k] as isize).unsigned_abs() {
            s if s == shape.size() => 0,
            s => long.iter().position(|&a| strides[a] == s).map_or(long.len(), |i| i + 1),
        };
        let mut order = Vec::with_capacity(n);
        for i in 0..=long.len() {
            order.extend((0..n).filter(|&k| shape[k] == 1 && place(k) == i));
            order.extend(long.get(i));
        }
        let mut axes = vec![0; n];
        for (i, &k) in order.iter().enumerate() {
            axes[k] = i;
        }
        let dims: Vec<usize> = order.iter().map(|&k| shape[k]).collect();
        let elements = Array1::from_vec(values).slice_move(s![offset..offset + shape.size()]);
        let mut array = elements.into_shape_with_order(IxDyn(&dims)).expect("contiguous elements").permuted_axes(axes);
        for k in (0..n).filter(|&k| shape[k] == 1) {
            match strides[k] as isize {
                0 => array.slice_axis_inplace(Axis(k), Slice::from(..)),
                s if s < 0 => array.invert_axis(Axis(k)),
                _ => {}
            }
        }
        array
    };
    array.into_dimensionality().expect("the same number of axes")
}
