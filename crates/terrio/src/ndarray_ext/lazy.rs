//! Lazy reads as arrays: the masks of [`LazyRaster::read_mask`]. Windows ([`LazyRaster::read_window`])
//! are datasets, which the views and owned transfers take as they are.

use crate::{Error, LazyRaster, Result, Window};
use ndarray::Array2;

#[cfg_attr(docsrs, doc(cfg(feature = "ndarray")))]
impl LazyRaster {
    /// Reads a window of the per-dataset mask, at full resolution or of an overview, as an array of
    /// shape `(rows, cols)`: 255 for valid pixels, 0 for invalid ones. `None` when the raster has no
    /// mask.
    ///
    /// The array is the buffer [`LazyRaster::read_mask`] reads, taken over without copying it.
    /// The mask stays separate from the pixels: a [`LazyRaster::read_window`] of the same window
    /// holds the pixels as stored, and moves into an array without copying them with
    /// [`RasterDataset::into_array3`](crate::RasterDataset::into_array3).
    ///
    /// ```no_run
    /// use terrio::{Window, WindowOptions};
    ///
    /// let mut cog = terrio::open_raster_lazy("https://example.com/cog.tif")?;
    /// let window = Window::new(0, 0, 512, 512);
    /// let (pixels, header) = cog.read_window(window, &WindowOptions::default())?.into_array3::<u16>()?;
    /// if let Some(mask) = cog.read_mask_array2(window, None)? {
    ///     assert_eq!(mask.dim(), (512, 512));
    /// }
    /// # Ok::<(), terrio::Error>(())
    /// ```
    pub fn read_mask_array2(&mut self, window: Window, overview: Option<usize>) -> Result<Option<Array2<u8>>> {
        let Some(mask) = self.read_mask(window, overview)? else { return Ok(None) };
        let shape = (window.height, window.width);
        let array = Array2::from_shape_vec(shape, mask);
        array.map(Some).map_err(|e| Error::InvalidData(format!("the mask of a {} x {} window: {e}", window.width, window.height)))
    }
}
