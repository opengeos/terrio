//! Reads of a window into a buffer of another size, resampled as GDAL's `RasterIO` resamples
//! them with a method other than nearest neighbour (`GDALRasterBand::RasterIOResampled` and
//! `GDALDataset::RasterIOResampled`, gcore/rasterio.cpp), pixel for pixel: the same kernels as
//! GDAL's overviews (`crate::drivers::gdal_overview`), over floating-point windows.

use crate::drivers::gdal_overview::{self, Method, ResampledRead};
use crate::{Error, Result};
use wbraster::raster::RasterData;

/// A resampled read: the level read, its windows and the buffer.
#[derive(Debug, Clone)]
pub struct ResampledWindow {
    /// The resampling, as GDAL names it: `bilinear`, `cubic`, `cubicspline`, `lanczos`,
    /// `average`, `rms`, `mode` or `gauss`.
    pub resampling: String,
    /// The size of the level read (full resolution or an overview).
    pub raster: (usize, usize),
    /// The integer window GDAL is given: `(x, y, width, height)`.
    pub window: (i64, i64, i64, i64),
    /// The floating-point window, `(x, y, width, height)`.
    pub fwindow: (f64, f64, f64, f64),
    /// The buffer's `(width, height)`.
    pub buf: (usize, usize),
    /// The band's block width, which GDAL's blocks of the buffer depend on.
    pub block_width: usize,
    /// Resample as `GDALDataset::RasterIOResampled` (several bands sharing a mask, with
    /// bilinear, cubic, cubic spline or Lanczos kernels) rather than band by band.
    pub dataset: bool,
}

impl ResampledWindow {
    fn read(&self) -> Result<ResampledRead> {
        let method = Method::parse(&self.resampling)
            .filter(|m| *m != Method::Nearest)
            .ok_or_else(|| Error::Unsupported(format!("'{}' resampling is not supported here", self.resampling)))?;
        if self.buf.0 == 0 || self.buf.1 == 0 || self.raster.0 == 0 || self.raster.1 == 0 {
            return Err(Error::InvalidArgument("empty window or buffer".into()));
        }
        Ok(ResampledRead {
            method,
            raster: self.raster,
            window: self.window,
            fwindow: self.fwindow,
            buf: self.buf,
            block_width: self.block_width,
            dataset: self.dataset,
        })
    }

    /// The source region the read needs, `(x, y, width, height)` in the level's pixels.
    pub fn region(&self) -> Result<(usize, usize, usize, usize)> {
        Ok(self.read()?.region())
    }

    /// Resamples `bands` bands of the source `region` ([`ResampledWindow::region`], band after
    /// band) into the buffer, band after band. `masks` holds each band's mask over the region
    /// (0 for invalid pixels, or alpha values), or `None`: the band's nodata mask when it has a
    /// nodata value, else all valid.
    pub fn resample(
        &self,
        src: &RasterData,
        region: (usize, usize, usize, usize),
        bands: usize,
        masks: Vec<Option<Vec<u8>>>,
        nodata: Option<f64>,
    ) -> Result<RasterData> {
        let r = self.read()?;
        let n = region.2 * region.3;
        if src.len() != bands * n || masks.iter().flatten().any(|m| m.len() != n) {
            return Err(Error::InvalidArgument("the source does not match the region".into()));
        }
        let mut masks = masks;
        masks.resize(bands, None);
        for (b, m) in masks.iter_mut().enumerate() {
            if m.is_none()
                && let Some(nd) = nodata
            {
                let band = crate::pixel::map_data!(src, v => v[b * n..(b + 1) * n].to_vec());
                *m = gdal_overview::nodata_mask(&band, nd);
            }
        }
        Ok(r.read(src, region, bands, &masks, nodata))
    }
}
