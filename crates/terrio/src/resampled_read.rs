// SPDX-FileCopyrightText: Copyright (c) 1998, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2007-2014, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 1998, 2003, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2008-2013, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 1998, 2002, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2007-2015, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2015, Faza Mahamood
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.13.3:
//     gcore/rasterio.cpp
//     gcore/gdaldataset.cpp
//     apps/gdal_translate_lib.cpp
//     frmts/vrt/vrtsources.cpp
// Written to match the behavior of (no code is translated from these sources):
//   GDAL v3.13.3:
//     gcore/gdalrasterband.cpp (GDALRasterBand::GetMaskBand)
//     frmts/gtiff/gtiffrasterband.cpp (GTiffRasterBand::GetMaskBand, GetMaskFlags)
//     gcore/gdalnodatamaskband.cpp
//     gcore/gdalrescaledalphaband.cpp

//! Windows read into an output of another size, as GDAL's `RasterIO` reads them
//! ([`LazyRaster::read_resampled`]): from the stored resolution it chooses, by nearest neighbour
//! or with the kernels of `RasterIOResampled` ([`ResampledRead`]), invalid pixels weighted as
//! GDAL's mask bands weight them.

use crate::cog::read_bands;
use crate::dataset::fallible;
use crate::drivers::gdal_overview::{Method, ResampledRead, Sample, nbits_max};
use crate::ops::Resampling;
use crate::ops::overview::OverviewSelection;
use crate::ops::translate::round_if_close;
use crate::{Error, LazyRaster, RasterDataset, Result, Window, WindowOptions};
use std::ops::Range;
use wbraster::DataType;
use wbraster::raster::RasterData;

/// What [`LazyRaster::read_resampled`] reads: bands of a source window, into an output of a given
/// size, as GDAL's `RasterIO` reads them, which `gdal_translate -outsize` and rasterio's
/// `read(window=..., out_shape=..., resampling=...)` do.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResampledReadOptions {
    /// The bands, numbered from 1 as in GDAL. Empty means all bands.
    pub bands: Vec<usize>,
    /// The source window in full-resolution pixels, `(x_off, y_off, x_size, y_size)`, possibly
    /// fractional, within the raster; `None` for the whole raster. GDAL is given it with the
    /// integer window rasterio gives: its offsets and sizes truncated, sizes at least 1. It may
    /// end past the raster's right or bottom edge by less than an output pixel, as GDAL's virtual
    /// datasets request a window of which the source covers the last output pixels in part: its
    /// integer window is then clamped to the raster.
    pub window: Option<(f64, f64, f64, f64)>,
    /// The output `(width, height)`.
    pub size: (usize, usize),
    /// Nearest neighbour (the default), bilinear, cubic, cubic spline, Lanczos, average, RMS or
    /// mode. As in GDAL, a paletted band is read by nearest neighbour rather than with the
    /// bilinear, cubic, cubic spline or Lanczos kernels.
    pub resampling: Resampling,
    /// The stored resolution read, as `gdal_translate -ovr` takes it. `Auto`, the default, is
    /// `RasterIO`'s choice when the output is smaller than the window: the overview whose
    /// reduction is the largest below the least reduced axis' times a threshold, 1.2 for nearest
    /// neighbour and 1.0 otherwise (or `GDAL_OVERVIEW_OVERSAMPLING_THRESHOLD`), plus 0.1.
    /// `AutoOffset(n)` reads, alone, the level `n` finer than the first overview no wider than
    /// the output (`Auto` past full resolution), `None` full resolution and `Level(i)` overview
    /// `i` (the last one when there are fewer). A level read alone is read with the window scaled
    /// to it as `gdal_translate`'s source requests it: values within 0.001 of a whole pixel are
    /// rounded to it, and sizes are rounded rather than truncated.
    pub overview: OverviewSelection,
}

/// Where a band's validity comes from: GDAL's mask band of the band (`GetMaskFlags`), in the
/// order the GeoTIFF driver looks for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaskKind {
    /// The dataset's mask, internal or a `.msk` sidecar (`GMF_PER_DATASET`).
    PerDataset,
    /// The band's nodata value (`GMF_NODATA`), which masks nothing outside the band's type.
    Nodata,
    /// The alpha band (numbered from 1) of a two- or four-band raster (`GMF_ALPHA`).
    Alpha(usize),
    /// All valid (`GMF_ALL_VALID`).
    AllValid,
}

/// The mask band of a band at the level read, as `GDALDataset::IRasterIO` compares them to
/// resample bands together: they share the dataset's mask or an alpha band, or are all valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LevelMask {
    AllValid,
    Nodata,
    Dataset,
    Alpha(usize),
    /// A mask band of the band's own: the all-valid one of an internal overview without a mask,
    /// which keeps the per-dataset flags of full resolution.
    Own(usize),
}

/// A level a read resamples from, and the windows GDAL reads it with.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Request {
    /// `None` for full resolution, else the overview.
    level: Option<usize>,
    /// The level's size.
    size: (usize, usize),
    /// The integer window GDAL is given, in the level's pixels.
    window: (usize, usize, usize, usize),
    /// The floating-point window, in the level's pixels.
    fwindow: (f64, f64, f64, f64),
}

impl Request {
    /// A plain copy: the buffer of the window's size, at whole pixels (`bUseIntegerRequestCoords`).
    fn is_copy(&self, buf: (usize, usize)) -> bool {
        let (x, y, w, h) = self.window;
        buf == (w, h) && self.fwindow == (x as f64, y as f64, w as f64, h as f64)
    }

    /// Whether the buffer is smaller than the window on either axis.
    fn reduces(&self, buf: (usize, usize)) -> bool {
        buf.0 < self.window.2 || buf.1 < self.window.3
    }

    /// Whether the buffer is smaller than the window on both axes.
    fn reduces_both(&self, buf: (usize, usize)) -> bool {
        buf.0 < self.window.2 && buf.1 < self.window.3
    }
}

/// How some bands are read from a level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum How {
    /// The window copied (`RasterIO` into a buffer of its size).
    Copy,
    /// Nearest-neighbour sampling.
    Nearest,
    /// `RasterIOResampled`'s kernels, band by band or, `dataset`, together
    /// (`GDALDataset::RasterIOResampled`).
    Kernel { dataset: bool },
    /// Nothing read: a level read alone whose window starts at its last column or row once
    /// rounded, of which `VRTSimpleSource::GetSrcDstWindow` reads nothing. The output keeps the
    /// value it starts with, each band's nodata value or 0.
    Fill,
}

/// Some of the bands of a read (their positions among those asked for), read from a level one
/// way.
#[derive(Debug, Clone, PartialEq)]
struct Part {
    bands: Range<usize>,
    request: Request,
    how: How,
}

/// A read checked: the bands, the request of full resolution, the buffer, and the level read
/// alone (`gdal_translate -ovr`), `None` when `RasterIO` chooses.
#[derive(Debug, Clone)]
struct Read {
    bands: Vec<usize>,
    full: Request,
    buf: (usize, usize),
    resampling: Resampling,
    fixed: Option<Option<usize>>,
}

/// The source read into memory at once, at most: kernels resample rows of blocks of the buffer
/// from strips of the source of about this size.
const STRIP_BYTES: usize = 64 << 20;

/// The rows a source without blocks (a raster in memory) is sampled by nearest neighbour in.
const SAMPLE_ROWS: usize = 256;

impl LazyRaster {
    /// Reads a window of some bands into an output of another size, as GDAL's `RasterIO` reads
    /// it ([`ResampledReadOptions`]): `(bands, rows, cols)` band-major in the raster's type, as a
    /// dataset georeferenced to the output (the window's extent at the output's size, GCPs
    /// scaled with it).
    ///
    /// A window of the output's size at whole pixels is read as [`LazyRaster::read_window`]
    /// reads it. Otherwise the stored resolution is chosen, the window rounded to it as GDAL
    /// rounds it, and the level resampled:
    /// - by nearest neighbour, from the pixel centers GDAL samples, reading only the blocks
    ///   holding them, a row of blocks at a time (or the chunks, through
    ///   [`LazyRaster::read_sampled`]);
    /// - with `RasterIOResampled`'s kernels otherwise, band by band or, for bands sharing a mask
    ///   with bilinear, cubic, cubic spline or Lanczos kernels, together as
    ///   `GDALDataset::RasterIOResampled` does, a strip of the source at a time. Invalid pixels
    ///   (masked out by the level's mask, their band's nodata value or an alpha band) carry no
    ///   weight. As in GDAL, an overview without a mask of its own is all valid.
    ///
    /// # Errors
    ///
    /// When a band does not exist, the output is empty, the window is empty or not within the
    /// raster (reads past its edges are not supported, but for less than an output pixel past its
    /// right or bottom edge), the method is one only warps use (min, max, median, quartiles, sum),
    /// the bands are complex and the method is not nearest neighbour, average and RMS reads of a
    /// paletted band at full resolution (GDAL averages its colors), or the read fails.
    ///
    /// ```no_run
    /// use terrio::ResampledReadOptions;
    /// use terrio::ops::Resampling;
    ///
    /// // As gdal_translate -outsize 512 512 -r average: from the overview RasterIO chooses.
    /// let mut cog = terrio::open_raster_lazy("https://example.com/cog.tif")?;
    /// let options = ResampledReadOptions { size: (512, 512), resampling: Resampling::Average, ..Default::default() };
    /// let small = cog.read_resampled(&options)?;
    /// assert_eq!((small.raster.cols, small.raster.rows), (512, 512));
    /// # Ok::<(), terrio::Error>(())
    /// ```
    pub fn read_resampled(&mut self, options: &ResampledReadOptions) -> Result<RasterDataset> {
        let read = self.check_resampled(options)?;
        let kinds = self.mask_kinds(&read.bands);
        let data = self.resample_bands(&read, &kinds)?;
        self.resampled_dataset(&read, data)
    }

    /// Reads the mask bands of the bands of [`LazyRaster::read_resampled`] the same way, as
    /// GDAL's `GetMaskBand()->RasterIO()` reads them: `(bands, rows, cols)`, 0 for invalid pixels
    /// and 255 for valid ones, or values between where a kernel resampled them.
    ///
    /// - The dataset's mask (internal or a `.msk` sidecar) is resampled as a band of its own,
    ///   all valid, from its own overviews (the levels with a mask of their own, the sidecar's)
    ///   chosen as for pixels, else from full resolution.
    /// - A nodata value masks the pixels [`LazyRaster::read_resampled`] reads that are that value
    ///   (NaN for NaN), none when it is not one of the band's type's values.
    /// - The alpha band of a two- or four-band raster is the other bands' mask: a Byte one read as
    ///   a band, a UInt16 one rescaled to 0–255 as GDAL's `GDALRescaledAlphaBand` does, from full
    ///   resolution.
    /// - Bands without one are all valid.
    ///
    /// # Errors
    ///
    /// As [`LazyRaster::read_resampled`].
    pub fn read_resampled_masks(&mut self, options: &ResampledReadOptions) -> Result<Vec<u8>> {
        let read = self.check_resampled(options)?;
        let kinds = self.read_mask_kinds(&read);
        // A nodata mask band reads its band band by band, as a read of that band alone does.
        let mut nodata = Vec::with_capacity(read.bands.len());
        for (k, &b) in read.bands.iter().enumerate() {
            nodata.push(match kinds[k] {
                MaskKind::Nodata => Some(self.resample_bands(&Read { bands: vec![b], ..read.clone() }, &kinds[k..k + 1])?),
                _ => None,
            });
        }
        let n = read.buf.0 * read.buf.1;
        self.resampled_masks(&read, &kinds, |k| nodata[k].as_ref().map(|d| (d, 0)), n)
    }

    /// Reads [`LazyRaster::read_resampled`] and [`LazyRaster::read_resampled_masks`] at once:
    /// nodata masks are computed from the pixels read rather than from a second read.
    ///
    /// # Errors
    ///
    /// As [`LazyRaster::read_resampled`].
    pub fn read_resampled_with_masks(&mut self, options: &ResampledReadOptions) -> Result<(RasterDataset, Vec<u8>)> {
        let read = self.check_resampled(options)?;
        let kinds = self.read_mask_kinds(&read);
        let data = self.resample_bands(&read, &kinds)?;
        let n = read.buf.0 * read.buf.1;
        // GDAL reads bands with a nodata mask band by band, as the read did.
        let masks = self.resampled_masks(&read, &kinds, |k| Some((&data, k)), n)?;
        Ok((self.resampled_dataset(&read, data)?, masks))
    }

    /// Reads as [`LazyRaster::read_resampled`] does, each band through its own `RasterIO`, as
    /// GDAL's virtual datasets read bands that are not the dataset's first ones in order: never
    /// resampled together.
    pub(crate) fn read_resampled_by_band(&mut self, options: &ResampledReadOptions) -> Result<RasterDataset> {
        let read = self.check_resampled(options)?;
        let kinds = self.mask_kinds(&read.bands);
        let n = read.buf.0 * read.buf.1;
        let mut data: Option<RasterData> = None;
        for (k, &b) in read.bands.iter().enumerate() {
            let band = self.resample_bands(&Read { bands: vec![b], ..read.clone() }, &kinds[k..k + 1])?;
            // The band of a read of one band is the read: kept rather than copied.
            if data.is_none() && band.len() == read.bands.len() * n {
                data = Some(band);
                continue;
            }
            let data = data.get_or_insert_with(|| RasterData::new_filled(band.data_type(), read.bands.len() * n, 0.0));
            place(data, &band, k * n);
        }
        let data = data.ok_or_else(|| Error::InvalidArgument("no band to read".into()))?;
        self.resampled_dataset(&read, data)
    }

    /// Checks a read and resolves its bands and windows.
    fn check_resampled(&self, o: &ResampledReadOptions) -> Result<Read> {
        let bands = read_bands(&o.bands, self.band_count())?;
        let (bw, bh) = o.size;
        if bw == 0 || bh == 0 {
            return Err(Error::InvalidArgument(format!("the output size {bw}x{bh} is empty")));
        }
        // An output no allocator can hold is refused before any buffer is sized from it.
        let what = "a resampled read";
        let sample_bytes = bands.iter().map(|&b| self.band_data_type(b).size_bytes()).sum::<usize>();
        let bytes = fallible::len(&[sample_bytes.max(1), bw, bh], what)?;
        fallible::reserve(&mut Vec::<u8>::new(), bytes, what)?;
        match o.resampling {
            Resampling::Nearest
            | Resampling::Bilinear
            | Resampling::Cubic
            | Resampling::CubicSpline
            | Resampling::Lanczos
            | Resampling::Average
            | Resampling::Rms
            | Resampling::Mode => {}
            m => return Err(Error::Unsupported(format!("'{}' resampling applies to warps, not to reads", m.name()))),
        }
        if self.is_complex() && o.resampling != Resampling::Nearest {
            return Err(Error::Unsupported("complex bands are read into another size by nearest neighbour only".into()));
        }
        let (w, h) = (self.width(), self.height());
        let fwindow = o.window.unwrap_or((0.0, 0.0, w as f64, h as f64));
        let (x, y, xs, ys) = fwindow;
        // Past the right or bottom edge by less than an output pixel at most.
        let ends = |off: f64, len: f64, n: usize, buf: usize| off + len <= n as f64 || off + len - (n as f64) < len / buf as f64;
        let inside = [x, y, xs, ys].iter().all(|v| v.is_finite())
            && x >= 0.0
            && y >= 0.0
            && xs > 0.0
            && ys > 0.0
            && x < w as f64
            && y < h as f64
            && ends(x, xs, w, bw)
            && ends(y, ys, h, bh);
        if !inside {
            return Err(Error::InvalidArgument(format!("the window {fwindow:?} is empty or not within the {w}x{h} raster")));
        }
        // The integer window rasterio gives GDAL with the floating-point one, within the raster.
        let (xi, yi) = (x as usize, y as usize);
        let window = (xi, yi, (xs.max(1.0) as usize).min(w - xi), (ys.max(1.0) as usize).min(h - yi));
        let full = Request { level: None, size: (w, h), window, fwindow };
        let fixed = fixed_level(o.overview, &self.overviews(), bw);
        Ok(Read { bands, full, buf: o.size, resampling: o.resampling, fixed })
    }

    /// GDAL's mask band of each band (see [`MaskKind`]).
    fn mask_kinds(&mut self, bands: &[usize]) -> Vec<MaskKind> {
        if self.has_mask() {
            return vec![MaskKind::PerDataset; bands.len()];
        }
        let alpha = self.mask_alpha();
        bands
            .iter()
            .map(|&b| match (self.band_nodata(b), alpha) {
                (Some(_), _) => MaskKind::Nodata,
                (None, Some(a)) if a != b => MaskKind::Alpha(a),
                _ => MaskKind::AllValid,
            })
            .collect()
    }

    /// The mask kinds of a read's bands (see [`MaskKind`]): those of the level read alone, a
    /// dataset of its own (`gdal_translate -ovr`), else of full resolution.
    fn read_mask_kinds(&mut self, read: &Read) -> Vec<MaskKind> {
        let kinds = self.mask_kinds(&read.bands);
        let level = read.fixed.flatten();
        kinds.iter().zip(&read.bands).map(|(&k, &b)| self.kind_at(k, b, level)).collect()
    }

    /// The alpha band GDAL takes as the other bands' mask band: band 2 of two or band 4 of four,
    /// when its color interpretation is alpha and its type Byte or UInt16.
    fn mask_alpha(&mut self) -> Option<usize> {
        let count = self.band_count();
        if !matches!(count, 2 | 4) {
            return None;
        }
        let alpha = crate::ops::overview::alpha_band(self)? + 1;
        (alpha == count && matches!(self.band_data_type(alpha), DataType::U8 | DataType::U16)).then_some(alpha)
    }

    /// The pixels of a read, `(bands, rows, cols)`.
    fn resample_bands(&mut self, read: &Read, kinds: &[MaskKind]) -> Result<RasterData> {
        let parts = self.plan(read, kinds);
        let n = read.buf.0 * read.buf.1;
        let mut out: Option<RasterData> = None;
        for part in &parts {
            let mut data = self.read_part(read, kinds, part)?;
            // A part of every band is the read: kept rather than copied.
            if out.is_none() && data.len() == read.bands.len() * n {
                out = Some(data);
                continue;
            }
            let out = out.get_or_insert_with(|| RasterData::new_filled(data.data_type(), read.bands.len() * n, 0.0));
            if data.data_type() != out.data_type() {
                // Bands of several types: in the type of the first read.
                data = RasterData::from_f64_vec(out.data_type(), data.to_f64_vec());
            }
            place(out, &data, part.bands.start * n);
        }
        out.ok_or_else(|| Error::InvalidArgument("no band to read".into()))
    }

    /// The dataset of a read's pixels, georeferenced to the output.
    fn resampled_dataset(&self, read: &Read, data: RasterData) -> Result<RasterDataset> {
        let gt = self.geo_transform();
        let (x, y, xs, ys) = read.full.fwindow;
        let (bw, bh) = (read.buf.0 as f64, read.buf.1 as f64);
        let (rx, ry) = (xs / bw, ys / bh);
        let geo_transform = [gt[0] + x * gt[1] + y * gt[2], gt[1] * rx, gt[2] * ry, gt[3] + x * gt[4] + y * gt[5], gt[4] * rx, gt[5] * ry];
        let gcp = |pixel: f64, line: f64| ((pixel - x) * bw / xs, (line - y) * bh / ys);
        // The whole image at full resolution: not an overview read alone at the raster's size.
        let whole = read.full.is_copy(read.buf) && read.buf == read.full.size && read.fixed.is_none_or(|level| level.is_none());
        // The nodata values of the dataset read: a level read alone is one of its own
        // (`gdal_translate -ovr`), whose bands an external overview's tags describe.
        let nodata = self.level_band_nodata(&read.bands, read.fixed.flatten());
        self.window_dataset(&read.bands, nodata, data, read.buf, geo_transform, gcp, whole)
    }

    /// How the bands of a read are read, as GDAL's `RasterIO` reads them: from which level, and
    /// band by band or together.
    fn plan(&mut self, read: &Read, kinds: &[MaskKind]) -> Vec<Part> {
        let all = 0..read.bands.len();
        let overviews = self.overviews();
        // `GDALDataset::IRasterIO` resamples bands together with these kernels only.
        let together = read.bands.len() > 1 && interpolates(read.resampling);
        if let Some(level) = read.fixed {
            let size = level.map_or(read.full.size, |i| overviews[i]);
            let request = fixed_request(read.full, level, size);
            if fixed_is_empty(read.full, size) {
                return vec![Part { bands: all, request, how: How::Fill }];
            }
            return self.at_level(read, kinds, all, request, together);
        }
        if read.full.is_copy(read.buf) {
            return vec![Part { bands: all, request: read.full, how: How::Copy }];
        }
        let request = self.auto_request(read, &overviews);
        if together && !overviews.is_empty() && read.full.reduces(read.buf) {
            // The dataset reads an overview when the buffer is smaller on both axes; otherwise,
            // or when it keeps full resolution, its bands (which have overviews) are read band by
            // band, each choosing its level as `GDALRasterBand::IRasterIO` does.
            if read.full.reduces_both(read.buf) && request.level.is_some() {
                return self.at_level(read, kinds, all, request, true);
            }
            return vec![Part { bands: all, request, how: self.band_how(read, request) }];
        }
        if together {
            return self.at_level(read, kinds, all, read.full, true);
        }
        vec![Part { bands: all, request, how: self.band_how(read, request) }]
    }

    /// The parts of a read of some bands from a level, through `GDALDataset::IRasterIO` there
    /// when bands may be resampled `together`: the first bands sharing a mask together, the
    /// others band by band.
    fn at_level(&mut self, read: &Read, kinds: &[MaskKind], bands: Range<usize>, request: Request, together: bool) -> Vec<Part> {
        let how = self.band_how(read, request);
        if !together || read.buf == (request.window.2, request.window.3) {
            return vec![Part { bands, request, how }];
        }
        let shared = self.shared_bands(read, kinds, request.level);
        let mut parts = Vec::new();
        if shared > 0 {
            parts.push(Part { bands: bands.start..bands.start + shared, request, how: How::Kernel { dataset: true } });
        }
        if bands.start + shared < bands.end {
            parts.push(Part { bands: bands.start + shared..bands.end, request, how });
        }
        parts
    }

    /// How `GDALRasterBand::IRasterIO` reads a band from a level: a copy, by nearest neighbour
    /// (for nearest neighbour, and a paletted band with an interpolating kernel at full
    /// resolution: GDAL's GeoTIFF overviews report no color table), or resampled.
    fn band_how(&self, read: &Read, request: Request) -> How {
        if request.is_copy(read.buf) {
            How::Copy
        } else if read.resampling == Resampling::Nearest || (interpolates(read.resampling) && self.paletted_at(request.level)) {
            How::Nearest
        } else {
            How::Kernel { dataset: false }
        }
    }

    /// The number of leading bands `GDALDataset::IRasterIO` resamples together at a level: of
    /// one type, without a color table, sharing a mask band (the dataset's mask, an alpha band)
    /// or all valid, a nodata mask ending them.
    fn shared_bands(&mut self, read: &Read, kinds: &[MaskKind], level: Option<usize>) -> usize {
        let paletted = self.paletted_at(level);
        let mut first: Option<(LevelMask, DataType)> = None;
        let mut count = 0;
        for (k, &b) in read.bands.iter().enumerate() {
            let mask = self.level_mask(self.kind_at(kinds[k], b, level), b, level);
            let data_type = self.band_data_type(b);
            if (paletted && b == 1) || mask == LevelMask::Nodata {
                break;
            }
            match first {
                None => first = Some((mask, data_type)),
                Some((m, t)) => {
                    let shared = (m == LevelMask::AllValid && mask == LevelMask::AllValid)
                        || (m == mask && matches!(mask, LevelMask::Dataset | LevelMask::Alpha(_)));
                    if t != data_type || !shared {
                        break;
                    }
                }
            }
            count += 1;
        }
        count
    }

    /// Whether the first band has a color table at a level: at full resolution, GDAL's GeoTIFF
    /// overviews reporting none.
    fn paletted_at(&self, level: Option<usize>) -> bool {
        level.is_none() && self.has_color_table()
    }

    /// Where a band's validity comes from at a level whose band has mask band `kind` at full
    /// resolution. An external overview is a dataset of its own (`GetMaskFlags` of its band): its
    /// own mask, else its own nodata value, else the alpha band; an internal one shares full
    /// resolution's.
    fn kind_at(&self, kind: MaskKind, band: usize, level: Option<usize>) -> MaskKind {
        let Some(i) = level.filter(|&i| !self.level_keeps_mask_flags(i)) else { return kind };
        if self.level_has_own_mask(i) {
            MaskKind::PerDataset
        } else if self.level_band_nodata(&[band], level)[0].is_some() {
            MaskKind::Nodata
        } else if let MaskKind::Alpha(a) = kind {
            MaskKind::Alpha(a)
        } else {
            MaskKind::AllValid
        }
    }

    /// A band's mask band at a level. An internal overview without a mask of its own has an
    /// all-valid mask band of its own band's while reporting the per-dataset flags of full
    /// resolution; an external one, a dataset of its own, reports none.
    fn level_mask(&self, kind: MaskKind, band: usize, level: Option<usize>) -> LevelMask {
        match (kind, level) {
            (MaskKind::AllValid, _) => LevelMask::AllValid,
            (MaskKind::Nodata, _) => LevelMask::Nodata,
            (MaskKind::Alpha(a), _) => LevelMask::Alpha(a),
            (MaskKind::PerDataset, None) => LevelMask::Dataset,
            (MaskKind::PerDataset, Some(i)) if self.level_has_own_mask(i) => LevelMask::Dataset,
            (MaskKind::PerDataset, Some(i)) if self.level_keeps_mask_flags(i) => LevelMask::Own(band),
            (MaskKind::PerDataset, Some(_)) => LevelMask::AllValid,
        }
    }

    /// The request `GDALRasterBand::IRasterIO` makes of a read: of the overview
    /// `GDALBandGetBestOverviewLevel2` chooses when the buffer is smaller than the window on
    /// either axis, else of full resolution.
    fn auto_request(&mut self, read: &Read, overviews: &[(usize, usize)]) -> Request {
        if overviews.is_empty() || !read.full.reduces(read.buf) {
            return read.full;
        }
        // GDAL ignores overviews of AVERAGE_BIT2GRAYSCALE resampling, as GeoTIFFs record it.
        if self.overview_resampling().is_some_and(|r| r.to_ascii_uppercase().starts_with("AVERAGE_BIT2")) {
            return read.full;
        }
        let threshold = oversampling_threshold(read.resampling);
        match best_overview(read.full.size, overviews, read.full.window, read.buf, threshold) {
            Some(i) => overview_request(read.full, i, overviews[i]),
            None => read.full,
        }
    }

    /// Reads a part of a read.
    fn read_part(&mut self, read: &Read, kinds: &[MaskKind], part: &Part) -> Result<RasterData> {
        let bands: Vec<usize> = read.bands[part.bands.clone()].to_vec();
        let request = part.request;
        let options = WindowOptions { bands: bands.clone(), overview: request.level };
        match part.how {
            How::Fill => {
                let n = read.buf.0 * read.buf.1;
                let nodata = self.level_band_nodata(&bands, request.level);
                let values = nodata.iter().flat_map(|nd| std::iter::repeat_n(nd.unwrap_or(0.0), n)).collect();
                Ok(RasterData::from_f64_vec(self.band_data_type(bands[0]), values))
            }
            How::Copy => {
                let (x, y, w, h) = request.window;
                Ok(self.read_window(Window::new(x, y, w, h), &options)?.raster.data)
            }
            How::Nearest => {
                let (cols, rows) = nearest_pixels(request.fwindow, read.buf, request.size);
                if let Some(data) = self.read_sampled(&cols, &rows, &options)? {
                    return Ok(data);
                }
                let block = self.level_block_size(request.level).unwrap_or((request.size.0, SAMPLE_ROWS));
                sample_by_blocks(&cols, &rows, block, &mut |w| Ok(self.read_window(w, &options)?.raster.data))
            }
            How::Kernel { dataset } => {
                if matches!(read.resampling, Resampling::Average | Resampling::Rms) && bands.contains(&1) && self.paletted_at(request.level)
                {
                    return Err(Error::Unsupported(
                        "average and RMS reads of a paletted band are not supported (GDAL averages its colors): use nearest or mode".into(),
                    ));
                }
                let resampled = self.resampled_read(read, request, dataset);
                let nodata = self.level_band_nodata(&bands, request.level);
                let kinds: Vec<MaskKind> =
                    kinds[part.bands.clone()].iter().zip(&bands).map(|(&k, &b)| self.kind_at(k, b, request.level)).collect();
                let kinds = &kinds;
                let sample_bytes = self.band_data_type(bands[0]).size_bytes();
                resample(&resampled, &nodata, sample_bytes, &mut |w| {
                    let data = self.read_window(w, &options)?.raster.data;
                    let masks = self.weights(w, request.level, &bands, kinds, &nodata, &data)?;
                    Ok((data, masks))
                })
            }
        }
    }

    /// The geometry of `RasterIOResampled` for a request of pixels.
    fn resampled_read(&self, read: &Read, request: Request, dataset: bool) -> ResampledRead {
        let block_width = self.level_block_size(request.level).map_or(request.size.0, |b| b.0);
        // The clamp of the level read, whose band's `NBITS` GDAL's `RasterIOResampled` takes.
        let max = self.level_sample_bits(request.level).and_then(|bits| nbits_max(u32::from(bits), self.data_type()));
        resampled_read(read, request, dataset, block_width, max)
    }

    /// The weights of some bands' pixels `data` over a window of a level, as their mask bands
    /// there give them: 0 for invalid pixels, or alpha values; `None` for all valid.
    fn weights(
        &mut self,
        window: Window,
        level: Option<usize>,
        bands: &[usize],
        kinds: &[MaskKind],
        nodata: &[Option<f64>],
        data: &RasterData,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        let n = window.width * window.height;
        let mut dataset: Option<Option<Vec<u8>>> = None;
        let mut alpha: Option<Vec<u8>> = None;
        let mut out = Vec::with_capacity(bands.len());
        for (k, kind) in kinds.iter().enumerate() {
            out.push(match *kind {
                MaskKind::AllValid => None,
                MaskKind::Nodata => nodata[k].and_then(|nd| nodata_mask(data, k, n, nd)),
                MaskKind::PerDataset => {
                    if dataset.is_none() {
                        dataset = Some(self.read_level_mask(window, level)?);
                    }
                    dataset.clone().flatten()
                }
                MaskKind::Alpha(a) => {
                    if alpha.is_none() {
                        let values = match bands.iter().position(|&b| b == a) {
                            Some(j) => band_of(data, j, n),
                            None => self.read_window(window, &WindowOptions { bands: vec![a], overview: level })?.raster.data,
                        };
                        alpha = Some(alpha_mask(&values));
                    }
                    alpha.clone()
                }
            });
        }
        Ok(out)
    }

    /// The masks of a read's bands (see [`LazyRaster::read_resampled_masks`]); `nodata(k)` gives
    /// the pixels band `k` was read into, as a buffer and the band's position in it.
    fn resampled_masks<'d>(
        &mut self,
        read: &Read,
        kinds: &[MaskKind],
        nodata: impl Fn(usize) -> Option<(&'d RasterData, usize)>,
        n: usize,
    ) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(kinds.len() * n);
        let mut dataset: Option<Vec<u8>> = None;
        let mut alpha: Option<Vec<u8>> = None;
        for (k, &b) in read.bands.iter().enumerate() {
            match kinds[k] {
                MaskKind::AllValid => out.resize(out.len() + n, 255),
                MaskKind::Nodata => {
                    let nd = self.level_band_nodata(&[b], read.fixed.flatten())[0]
                        .ok_or_else(|| Error::InvalidData(format!("band {b} has a nodata mask but no nodata value")))?;
                    let (data, j) = nodata(k).ok_or_else(|| Error::InvalidData("the pixels of a nodata mask were not read".into()))?;
                    out.extend(nodata_mask(data, j, n, nd).unwrap_or_else(|| vec![255; n]));
                }
                MaskKind::PerDataset => {
                    if dataset.is_none() {
                        dataset = Some(self.resampled_mask_band(read)?);
                    }
                    out.extend_from_slice(dataset.as_deref().unwrap_or_default());
                }
                MaskKind::Alpha(a) => {
                    if alpha.is_none() {
                        alpha = Some(self.resampled_alpha(read, a)?);
                    }
                    out.extend_from_slice(alpha.as_deref().unwrap_or_default());
                }
            }
        }
        Ok(out)
    }

    /// The dataset's mask band read as GDAL reads it into another size: a band of its own, all
    /// valid, from its own overviews chosen as for pixels.
    fn resampled_mask_band(&mut self, read: &Read) -> Result<Vec<u8>> {
        let mask_overviews = self.mask_band_overviews();
        let request = match read.fixed {
            // A level of which nothing is read: no valid pixel.
            Some(level) if fixed_is_empty(read.full, level.map_or(read.full.size, |i| self.overviews()[i])) => {
                return Ok(vec![0; read.buf.0 * read.buf.1]);
            }
            // A level read alone: its own mask, if any, else (in GDAL, an all-valid mask band)
            // nothing masked.
            Some(Some(i)) => {
                let size = self.overviews()[i];
                match mask_overviews.iter().position(|s| *s == size) {
                    Some(j) => fixed_request(read.full, Some(j), size),
                    None => return Ok(vec![255; read.buf.0 * read.buf.1]),
                }
            }
            Some(None) => fixed_request(read.full, None, read.full.size),
            None if mask_overviews.is_empty() || !read.full.reduces(read.buf) => read.full,
            None => {
                let threshold = oversampling_threshold(read.resampling);
                match best_overview(read.full.size, &mask_overviews, read.full.window, read.buf, threshold) {
                    Some(j) => overview_request(read.full, j, mask_overviews[j]),
                    None => read.full,
                }
            }
        };
        let (x, y, w, h) = request.window;
        if request.is_copy(read.buf) {
            return self.read_mask_band(Window::new(x, y, w, h), request.level);
        }
        // The blocks of the level of the mask's size.
        let level = request.level.and_then(|j| self.overviews().iter().position(|s| *s == mask_overviews[j]));
        let block = self.level_block_size(level).unwrap_or((request.size.0, SAMPLE_ROWS));
        let data = if read.resampling == Resampling::Nearest {
            let (cols, rows) = nearest_pixels(request.fwindow, read.buf, request.size);
            sample_by_blocks(&cols, &rows, block, &mut |w| Ok(RasterData::U8(self.read_mask_band(w, request.level)?)))?
        } else {
            let max = self.mask_band_bits().and_then(|bits| nbits_max(u32::from(bits), DataType::U8));
            let resampled = resampled_read(read, request, false, block.0, max);
            resample(&resampled, &[None], 1, &mut |w| Ok((RasterData::U8(self.read_mask_band(w, request.level)?), vec![None])))?
        };
        match data {
            RasterData::U8(v) => Ok(v),
            other => Ok(other.to_f64_vec().into_iter().map(|v| v as u8).collect()),
        }
    }

    /// The alpha band `alpha` read as the mask band GDAL makes of it: a Byte band read as the
    /// other bands are, band by band, all valid; a UInt16 one rescaled to 0–255 as
    /// `GDALRescaledAlphaBand` does, which has no overviews: from full resolution.
    fn resampled_alpha(&mut self, read: &Read, alpha: usize) -> Result<Vec<u8>> {
        let single = Read { bands: vec![alpha], ..read.clone() };
        if self.band_data_type(alpha) == DataType::U8 {
            let kinds = self.mask_kinds(&single.bands);
            return match self.resample_bands(&single, &kinds)? {
                RasterData::U8(v) => Ok(v),
                other => Ok(alpha_mask(&other)),
            };
        }
        let full = read.full;
        let options = WindowOptions { bands: vec![alpha], overview: None };
        let rescaled = |data: RasterData| RasterData::U8(alpha_mask(&data));
        let (x, y, w, h) = full.window;
        let data = if full.is_copy(read.buf) {
            rescaled(self.read_window(Window::new(x, y, w, h), &options)?.raster.data)
        } else if read.resampling == Resampling::Nearest {
            let (cols, rows) = nearest_pixels(full.fwindow, read.buf, full.size);
            let block = self.level_block_size(None).unwrap_or((full.size.0, SAMPLE_ROWS));
            sample_by_blocks(&cols, &rows, block, &mut |w| Ok(rescaled(self.read_window(w, &options)?.raster.data)))?
        } else {
            let block_width = self.level_block_size(None).map_or(full.size.0, |b| b.0);
            let resampled = resampled_read(read, full, false, block_width, None);
            resample(&resampled, &[None], 1, &mut |w| Ok((rescaled(self.read_window(w, &options)?.raster.data), vec![None])))?
        };
        Ok(alpha_mask(&data))
    }
}

/// The geometry of `RasterIOResampled` for a request of a band of blocks `block_width` pixels
/// wide, whose `NBITS`-bit samples are at most `max`.
fn resampled_read(read: &Read, request: Request, dataset: bool, block_width: usize, max: Option<f64>) -> ResampledRead {
    let (x, y, w, h) = request.window;
    ResampledRead {
        method: kernel(read.resampling),
        raster: request.size,
        window: (x as i64, y as i64, w as i64, h as i64),
        fwindow: request.fwindow,
        buf: read.buf,
        block_width,
        dataset,
        max,
        nan_aware: true,
    }
}

/// Whether a method is one of the interpolating kernels, which `GDALDataset::IRasterIO`
/// resamples bands together with and paletted bands are not resampled with.
fn interpolates(m: Resampling) -> bool {
    matches!(m, Resampling::Bilinear | Resampling::Cubic | Resampling::CubicSpline | Resampling::Lanczos)
}

/// The kernel of `RasterIOResampled` for a method other than nearest neighbour.
fn kernel(m: Resampling) -> Method {
    match m {
        Resampling::Bilinear => Method::Bilinear,
        Resampling::Cubic => Method::Cubic,
        Resampling::CubicSpline => Method::CubicSpline,
        Resampling::Lanczos => Method::Lanczos,
        Resampling::Rms => Method::Rms,
        Resampling::Mode => Method::Mode,
        _ => Method::Average,
    }
}

/// The threshold above which `RasterIO` does not read an overview less reduced than the request:
/// `GDAL_OVERVIEW_OVERSAMPLING_THRESHOLD`, else 1.2 for nearest neighbour and 1.0 otherwise.
fn oversampling_threshold(m: Resampling) -> f64 {
    match crate::config::get("GDAL_OVERVIEW_OVERSAMPLING_THRESHOLD") {
        Some(v) => crate::ops::dem::atof(&v),
        None if m == Resampling::Nearest => 1.2,
        None => 1.0,
    }
}

/// The overview `RasterIO` reads a request of an integer `window` into `buf` from
/// (`GDALBandGetBestOverviewLevel2`, `GDALBandGetBestOverviewLevel`): the desired reduction is
/// that of the least reduced axis (the columns' for a buffer of one row), and full resolution
/// (a reduction of 1) then each overview (by its least reduced axis) is a candidate when below
/// that times `threshold` plus 0.1 and more reduced than the best so far, the search stopping at
/// one within 0.1 of it. `None` for full resolution.
pub(crate) fn best_overview(
    (w, h): (usize, usize),
    overviews: &[(usize, usize)],
    window: (usize, usize, usize, usize),
    (bw, bh): (usize, usize),
    threshold: f64,
) -> Option<usize> {
    const EPSILON: f64 = 0.1;
    let (xr, yr) = (window.2 as f64 / bw as f64, window.3 as f64 / bh as f64);
    let desired = if xr < yr || bh == 1 { xr } else { yr };
    let mut best = None;
    let mut best_ratio = 0.0;
    let candidates = std::iter::once((None, 1.0)).chain(
        overviews
            .iter()
            .enumerate()
            .filter(|(_, (ow, oh))| *ow <= w && *oh <= h)
            .map(|(i, (ow, oh))| (Some(i), (w as f64 / *ow as f64).min(h as f64 / *oh as f64))),
    );
    for (level, ratio) in candidates {
        if ratio >= desired * threshold + EPSILON || ratio <= best_ratio {
            continue;
        }
        best = level;
        best_ratio = ratio;
        if (desired - ratio).abs() < EPSILON {
            break;
        }
    }
    best
}

/// The request `GDALBandGetBestOverviewLevel2` makes of overview `level` of size `(ow, oh)`: the
/// integer window rounded to it and clamped, the floating-point window scaled.
fn overview_request(full: Request, level: usize, (ow, oh): (usize, usize)) -> Request {
    let (w, h) = full.size;
    let (xf, yf) = (w as f64 / ow as f64, h as f64 / oh as f64);
    let (x, y, xs, ys) = full.window;
    let ox = ((x as f64 / xf + 0.5) as usize).min(ow - 1);
    let oy = ((y as f64 / yf + 0.5) as usize).min(oh - 1);
    let oxs = ((xs as f64 / xf + 0.5) as usize).max(1).min(ow - ox);
    let oys = ((ys as f64 / yf + 0.5) as usize).max(1).min(oh - oy);
    let (fx, fy, fxs, fys) = full.fwindow;
    Request { level: Some(level), size: (ow, oh), window: (ox, oy, oxs, oys), fwindow: (fx / xf, fy / yf, fxs / xf, fys / yf) }
}

/// The level `gdal_translate -ovr` reads alone, `Some(None)` for full resolution; `None` when
/// `RasterIO` chooses (`AUTO`, and `AUTO-n` past full resolution). `AUTO-n` is `n` levels finer
/// than the first overview no wider than the output (`buf_width`), or the last.
pub(crate) fn fixed_level(selection: OverviewSelection, overviews: &[(usize, usize)], buf_width: usize) -> Option<Option<usize>> {
    let n = overviews.len();
    match selection {
        OverviewSelection::Auto => None,
        OverviewSelection::None => Some(None),
        OverviewSelection::Level(i) if i < n => Some(Some(i)),
        OverviewSelection::Level(i) if n > 0 => {
            crate::warnings::warn(format!("cannot get overview level {i}; defaulting to level {}", n - 1));
            Some(Some(n - 1))
        }
        OverviewSelection::Level(i) => {
            crate::warnings::warn(format!("cannot get overview level {i}; defaulting to full resolution"));
            None
        }
        OverviewSelection::AutoOffset(_) if n == 0 => None,
        OverviewSelection::AutoOffset(offset) => {
            let first = (0..n - 1).find(|&i| overviews[i].0 <= buf_width).unwrap_or(n - 1);
            first.checked_sub(offset).map(Some)
        }
    }
}

/// The request of a level read alone (`gdal_translate -ovr`): the window scaled to the level, as
/// `gdal_translate`'s VRT source requests it of that level (`VRTSimpleSource::GetSrcDstWindow`):
/// offsets truncated, or rounded up within 0.001 of the next pixel, sizes rounded (at least 1)
/// and clamped to the level, and the floating-point window's values within 0.001 of a whole
/// pixel rounded to it (`RoundIfCloseToInt`).
fn fixed_request(full: Request, level: Option<usize>, (lw, lh): (usize, usize)) -> Request {
    let (w, h) = full.size;
    let (xf, yf) = (w as f64 / lw as f64, h as f64 / lh as f64);
    let (fx, fy, fxs, fys) = full.fwindow;
    let axis = |off: f64, len: f64, n: usize| {
        let (mut off, mut len) = (off, len);
        let mut start = off as usize;
        if off - start as f64 > 1.0 - 1e-3 {
            start += 1;
            off = start as f64;
        }
        let mut size = ((len + 0.5).floor() as usize).max(1);
        if start + size > n {
            size = n.saturating_sub(start).max(1);
        }
        if off + len > n as f64 {
            len = n as f64 - off;
        }
        (start, size, round_if_close(off), round_if_close(len))
    };
    let (x, xs, fx, fxs) = axis(fx / xf, fxs / xf, lw);
    let (y, ys, fy, fys) = axis(fy / yf, fys / yf, lh);
    Request { level, size: (lw, lh), window: (x.min(lw - 1), y.min(lh - 1), xs, ys), fwindow: (fx, fy, fxs, fys) }
}

/// Whether a level read alone has nothing to read: its window, scaled to the level of size
/// `(lw, lh)`, starts at the level's last column or row once rounded up (within 0.001 of the next
/// pixel), where `VRTSimpleSource::GetSrcDstWindow` reads nothing.
fn fixed_is_empty(full: Request, (lw, lh): (usize, usize)) -> bool {
    let (w, h) = full.size;
    let (fx, fy, _, _) = full.fwindow;
    let start = |off: f64| {
        let start = off as usize;
        if off - start as f64 > 1.0 - 1e-3 { start + 1 } else { start }
    };
    // Scaled as `fixed_request` scales it.
    start(fx / (w as f64 / lw as f64)) >= lw || start(fy / (h as f64 / lh as f64)) >= lh
}

/// The columns and rows GDAL's `RasterIO` samples by nearest neighbour (`GDALRasterBand::
/// IRasterIO`): the buffer's pixel centers in the floating-point window, plus 1e-10, clamped to
/// the level, columns stepped by repeated addition as GDAL steps them.
fn nearest_pixels(
    (fx, fy, fxs, fys): (f64, f64, f64, f64),
    (bw, bh): (usize, usize),
    (lw, lh): (usize, usize),
) -> (Vec<usize>, Vec<usize>) {
    const EPS: f64 = 1e-10;
    let clamp = |v: f64, n: usize| v.max(0.0).min((n - 1) as f64) as usize;
    let (x_inc, y_inc) = (fxs / bw as f64, fys / bh as f64);
    let mut cols = Vec::with_capacity(bw);
    let mut x = 0.5 * x_inc + fx + EPS;
    for _ in 0..bw {
        cols.push(clamp(x, lw));
        x += x_inc;
    }
    let rows = (0..bh).map(|r| clamp((r as f64 + 0.5) * y_inc + fy + EPS, lh)).collect();
    (cols, rows)
}

/// The pixels at columns `cols` of rows `rows` (each ascending), read through `read` a row of
/// blocks of size `block` at a time, from the runs of blocks holding them alone:
/// `(bands, rows, cols)`.
fn sample_by_blocks(
    cols: &[usize],
    rows: &[usize],
    (block_w, block_h): (usize, usize),
    read: &mut dyn FnMut(Window) -> Result<RasterData>,
) -> Result<RasterData> {
    let (block_w, block_h) = (block_w.max(1), block_h.max(1));
    // Runs of output columns whose blocks are adjacent.
    let mut spans: Vec<Range<usize>> = Vec::new();
    for (c, &col) in cols.iter().enumerate() {
        match spans.last_mut() {
            Some(span) if col / block_w <= cols[c - 1] / block_w + 1 => span.end = c + 1,
            _ => spans.push(c..c + 1),
        }
    }
    let (bw, bh) = (cols.len(), rows.len());
    let mut out: Option<RasterData> = None;
    let mut r0 = 0;
    while r0 < bh {
        // The output rows whose source rows are in the same row of blocks.
        let mut r1 = r0 + 1;
        while r1 < bh && rows[r1] / block_h == rows[r0] / block_h {
            r1 += 1;
        }
        let (y0, y1) = (rows[r0], rows[r1 - 1]);
        for span in &spans {
            let (x0, x1) = (cols[span.start], cols[span.end - 1]);
            let window = Window::new(x0, y0, x1 - x0 + 1, y1 - y0 + 1);
            let part = read(window)?;
            let n = window.width * window.height;
            let bands = part.len() / n.max(1);
            let out = out.get_or_insert_with(|| RasterData::new_filled(part.data_type(), bands * bw * bh, 0.0));
            pick(out, &part, (bw, bh), window, (span.clone(), r0..r1), cols, rows)?;
        }
        r0 = r1;
    }
    out.ok_or_else(|| Error::InvalidArgument("nothing to sample".into()))
}

/// The pixels of some bands over a window, band after band, and each band's weights there (see
/// [`ResampledRead::read`]).
type Weighted = (RasterData, Vec<Option<Vec<u8>>>);

/// Resamples with `resampled`, reading the source through `read` a strip of rows of blocks at a
/// time: `(bands, rows, cols)`. Bands read band by band take their own nodata value from
/// `nodata`.
fn resample(
    resampled: &ResampledRead,
    nodata: &[Option<f64>],
    sample_bytes: usize,
    read: &mut dyn FnMut(Window) -> Result<Weighted>,
) -> Result<RasterData> {
    let bands = nodata.len();
    let (x, _, w, _) = resampled.region();
    let (bw, bh) = resampled.buf;
    let blocks = resampled.row_blocks();
    let mut out: Option<RasterData> = None;
    let mut i = 0;
    while i < blocks.len() {
        // Rows of blocks reading a strip of the source of at most STRIP_BYTES, at least one.
        let (y, mut end) = (blocks[i].1.0, blocks[i].1.0 + blocks[i].1.1);
        let mut j = i + 1;
        while j < blocks.len() {
            let next = end.max(blocks[j].1.0 + blocks[j].1.1);
            if (next - y) * w * bands * sample_bytes > STRIP_BYTES {
                break;
            }
            end = next;
            j += 1;
        }
        let rows = blocks[i].0.start..blocks[j - 1].0.end;
        let window = Window::new(x, y, w, end - y);
        let region = (x, y, w, end - y);
        let (src, masks) = read(window)?;
        let part = if resampled.dataset {
            resampled.read_rows(&src, region, bands, &masks, None, rows.clone())
        } else {
            let n = w * (end - y);
            let parts: Vec<RasterData> = (0..bands)
                .map(|b| resampled.read_rows(&band_of(&src, b, n), region, 1, &masks[b..b + 1], nodata[b], rows.clone()))
                .collect();
            concat(parts)
        };
        let out = out.get_or_insert_with(|| RasterData::new_filled(part.data_type(), bands * bw * bh, 0.0));
        // The part's rows, band after band, into the buffer's.
        let m = rows.len() * bw;
        for b in 0..bands {
            copy_into(out, b * bw * bh + rows.start * bw, &part, b * m, m);
        }
        i = j;
    }
    out.ok_or_else(|| Error::InvalidArgument("nothing to resample".into()))
}

/// Runs `$body` with `$a` and `$b` the values of two buffers of one type.
macro_rules! with_pair {
    ($a:expr, $b:expr, $x:ident, $y:ident => $body:expr) => {
        match ($a, $b) {
            (RasterData::U8($x), RasterData::U8($y)) => $body,
            (RasterData::I8($x), RasterData::I8($y)) => $body,
            (RasterData::U16($x), RasterData::U16($y)) => $body,
            (RasterData::I16($x), RasterData::I16($y)) => $body,
            (RasterData::U32($x), RasterData::U32($y)) => $body,
            (RasterData::I32($x), RasterData::I32($y)) => $body,
            (RasterData::U64($x), RasterData::U64($y)) => $body,
            (RasterData::I64($x), RasterData::I64($y)) => $body,
            (RasterData::F32($x), RasterData::F32($y)) => $body,
            (RasterData::F64($x), RasterData::F64($y)) => $body,
            _ => unreachable!("buffers of one type"),
        }
    };
}

/// Copies `len` values of `src` from `from` into `dst` at `at`; `src` in `dst`'s type.
fn copy_into(dst: &mut RasterData, at: usize, src: &RasterData, from: usize, len: usize) {
    let converted;
    let src = if src.data_type() == dst.data_type() {
        src
    } else {
        converted = RasterData::from_f64_vec(dst.data_type(), src.to_f64_vec());
        &converted
    };
    with_pair!(dst, src, d, s => d[at..at + len].copy_from_slice(&s[from..from + len]));
}

/// Places all of `src` into `dst` at `at`.
fn place(dst: &mut RasterData, src: &RasterData, at: usize) {
    copy_into(dst, at, src, 0, src.len());
}

/// Picks the samples of the output columns `span` and rows `out_rows` from `part`, a read
/// `window` of every band, into `out` (`(bands, rows, cols)` of size `(bw, bh)`).
fn pick(
    out: &mut RasterData,
    part: &RasterData,
    (bw, bh): (usize, usize),
    window: Window,
    (span, out_rows): (Range<usize>, Range<usize>),
    cols: &[usize],
    rows: &[usize],
) -> Result<()> {
    let n = window.width * window.height;
    let bands = part.len() / n.max(1);
    let converted;
    let part = if part.data_type() == out.data_type() {
        part
    } else {
        converted = RasterData::from_f64_vec(out.data_type(), part.to_f64_vec());
        &converted
    };
    with_pair!(out, part, o, p => {
        for b in 0..bands {
            for r in out_rows.clone() {
                let src_row = &p[b * n + (rows[r] - window.y) * window.width..][..window.width];
                let dst_row = &mut o[b * bw * bh + r * bw..][..bw];
                for c in span.clone() {
                    dst_row[c] = src_row[cols[c] - window.x];
                }
            }
        }
    });
    Ok(())
}

/// Band `band` of a buffer of bands of `n` values each.
fn band_of(data: &RasterData, band: usize, n: usize) -> RasterData {
    crate::pixel::map_data!(data, v => v[band * n..(band + 1) * n].to_vec())
}

/// Buffers one after the other.
fn concat(parts: Vec<RasterData>) -> RasterData {
    let Some(first) = parts.first() else { return RasterData::U8(Vec::new()) };
    // Allocated once, each part copied in at its offset.
    let mut out = RasterData::new_filled(first.data_type(), parts.iter().map(RasterData::len).sum(), 0.0);
    let mut at = 0;
    for part in &parts {
        place(&mut out, part, at);
        at += part.len();
    }
    out
}

/// The mask GDAL's `GDALNoDataMaskBand` gives band `band` (of `n` values) of a buffer: 0 where
/// the value is the nodata value, else 255; `None` when the nodata value is not one of the
/// type's values (the band is then all valid).
fn nodata_mask(data: &RasterData, band: usize, n: usize, nodata: f64) -> Option<Vec<u8>> {
    fn mask<T: Sample>(v: &[T], nd: f64) -> Option<Vec<u8>> {
        T::nodata_in_range(nd).then(|| v.iter().map(|x| if x.is_nodata(nd) { 0 } else { 255 }).collect())
    }
    crate::pixel::with_data!(data, v => mask(&v[band * n..(band + 1) * n], nodata))
}

/// An alpha band's values as the mask band GDAL makes of it: a Byte band's as they are, a UInt16
/// one's rescaled as `GDALRescaledAlphaBand` rescales them (1 to 256 give 1, the others times
/// 255 / 65535).
fn alpha_mask(values: &RasterData) -> Vec<u8> {
    match values {
        RasterData::U8(v) => v.clone(),
        RasterData::U16(v) => v.iter().map(|&a| if a > 0 && a < 257 { 1 } else { (u32::from(a) * 255 / 65535) as u8 }).collect(),
        other => other.to_f64_vec().into_iter().map(|a| a.clamp(0.0, 255.0) as u8).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dataset::Gcp;

    /// A raster in memory of `bands` bands of `cols` x `rows` pixels holding `data`.
    fn raster(data: RasterData, cols: usize, rows: usize, bands: usize, nodata: Option<f64>) -> LazyRaster {
        let cfg = wbraster::RasterConfig {
            cols,
            rows,
            bands,
            data_type: data.data_type(),
            nodata: nodata.unwrap_or(-9999.0),
            ..Default::default()
        };
        let mut ds = RasterDataset::new(crate::pixel::raster_from_data(cfg, data).unwrap(), nodata.is_some());
        ds.set_geo_transform([1000.0, 10.0, 0.0, 2000.0, 0.0, -10.0]);
        ds.gcps = vec![Gcp { pixel: 4.0, line: 6.0, x: 1.0, y: 2.0, ..Default::default() }];
        LazyRaster::from(ds)
    }

    /// The levels GDAL 3.13.2 reads from (found by reading rasters whose levels hold markers):
    /// full resolution is a candidate, and the search stops within 0.1 of the desired reduction.
    #[test]
    fn overview_choices_are_gdals() {
        let ovr = [(500, 400), (250, 200), (125, 100), (63, 50)];
        let full = (0, 0, 1000, 800);
        // A desired reduction of 3.88: overview 1 (4) is too reduced for interpolating kernels
        // (below 3.88 + 0.1), not for nearest neighbour (1.2 x 3.88 + 0.1), and not within 0.1.
        assert_eq!(best_overview((1000, 800), &ovr, full, (257, 206), 1.0), Some(0));
        assert_eq!(best_overview((1000, 800), &ovr, full, (257, 206), 1.2), Some(1));
        // 1.89: overview 0 (2) is past 1.89 + 0.1, full resolution is read.
        assert_eq!(best_overview((1000, 800), &ovr, full, (530, 424), 1.0), None);
        // The least reduced axis: 1000 / 400 = 2.5 against 800 / 100 = 8 (overview 1 is past
        // 1.2 x 2.5 + 0.1).
        assert_eq!(best_overview((1000, 800), &ovr, full, (400, 100), 1.2), Some(0));
        // A buffer of one row: the columns' reduction, overviews no more reduced than the row.
        assert_eq!(best_overview((1001, 1), &[(501, 1), (251, 1)], (584, 0, 156, 1), (134, 1), 1.2), None);
        // GDAL_OVERVIEW_OVERSAMPLING_THRESHOLD=2 on 997 x 613: full resolution within 0.1 of
        // 1.0994; overview 0 (1.9967) within 0.1 of 2.0962; overview 1 (2.99) below 1.89 x 2 + 0.1.
        let ovr = [(499, 307), (333, 205), (143, 88), (63, 39)];
        let window = (223, 16, 763, 556);
        assert_eq!(best_overview((997, 613), &ovr, window, (694, 235), 2.0), None);
        assert_eq!(best_overview((997, 613), &ovr, window, (364, 235), 2.0), Some(0));
        assert_eq!(best_overview((997, 613), &ovr, window, (403, 235), 2.0), Some(1));
    }

    /// The levels `gdal_translate -ovr` reads alone.
    #[test]
    fn fixed_levels_are_gdal_translates() {
        let ovr = [(32, 24), (16, 12), (8, 6)];
        assert_eq!(fixed_level(OverviewSelection::Auto, &ovr, 20), None);
        assert_eq!(fixed_level(OverviewSelection::None, &ovr, 20), Some(None));
        assert_eq!(fixed_level(OverviewSelection::Level(1), &ovr, 20), Some(Some(1)));
        // A level past the last: the last, with a warning; none without overviews.
        assert_eq!(fixed_level(OverviewSelection::Level(9), &ovr, 20), Some(Some(2)));
        assert_eq!(fixed_level(OverviewSelection::Level(0), &[], 20), None);
        // AUTO-n: the first overview no wider than the output (16 for 20), n levels finer, or
        // RasterIO's choice past full resolution.
        assert_eq!(fixed_level(OverviewSelection::AutoOffset(1), &ovr, 20), Some(Some(0)));
        assert_eq!(fixed_level(OverviewSelection::AutoOffset(2), &ovr, 20), None);
        assert_eq!(fixed_level(OverviewSelection::AutoOffset(1), &ovr, 4), Some(Some(1)));
        assert_eq!(fixed_level(OverviewSelection::AutoOffset(0), &ovr, 100), Some(Some(0)));
    }

    /// A level read alone takes the window as `gdal_translate`'s source requests it.
    #[test]
    fn fixed_requests_round_as_vrt_sources_do() {
        let full = Request { level: None, size: (64, 48), window: (3, 5, 54, 38), fwindow: (3.0, 5.0, 54.0, 38.0) };
        let r = fixed_request(full, Some(1), (16, 12));
        assert_eq!(r.window, (0, 1, 14, 10));
        assert_eq!(r.fwindow, (0.75, 1.25, 13.5, 9.5));
        // Within 0.001 of whole pixels: whole.
        let full = Request { level: None, size: (997, 613), window: (0, 0, 997, 613), fwindow: (0.0, 0.0, 997.0, 613.0) };
        let r = fixed_request(full, Some(0), (499, 307));
        assert_eq!((r.window, r.fwindow), ((0, 0, 499, 307), (0.0, 0.0, 499.0, 307.0)));
    }

    /// GDAL samples columns by repeated addition, rows by multiplication.
    #[test]
    fn nearest_pixels_are_gdals() {
        let (cols, rows) = nearest_pixels((0.75, 1.25, 13.5, 9.5), (13, 9), (16, 12));
        assert_eq!(cols, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
        assert_eq!(rows, [1, 2, 3, 4, 6, 7, 8, 9, 10]);
        // Clamped to the level.
        assert_eq!(nearest_pixels((0.0, 0.0, 4.0, 4.0), (8, 8), (4, 4)).0, [0, 0, 1, 1, 2, 2, 3, 3]);
    }

    /// The output covers the window at its size, its GCPs scaled with it; bands and the nodata
    /// value are kept.
    #[test]
    fn outputs_are_georeferenced_to_the_window() {
        let mut r = raster(RasterData::U8((0..200).map(|v| v as u8).collect()), 10, 10, 2, Some(7.0));
        let o = ResampledReadOptions { bands: vec![2], window: Some((2.0, 4.0, 6.0, 4.0)), size: (3, 2), ..Default::default() };
        let ds = r.read_resampled(&o).unwrap();
        assert_eq!((ds.raster.cols, ds.raster.rows, ds.raster.bands), (3, 2, 1));
        assert_eq!(ds.geo_transform(), [1020.0, 20.0, 0.0, 1960.0, 0.0, -20.0]);
        assert_eq!((ds.gcps[0].pixel, ds.gcps[0].line), (1.0, 1.0));
        assert_eq!(ds.band_nodata(1), Some(7.0));
        // Nearest neighbour: columns 3, 5, 7 of rows 5 and 7 of band 2.
        assert_eq!(read_values_u8(&ds.raster.data), [153, 155, 157, 173, 175, 177]);
        // A window of the output's size at whole pixels is a copy.
        let o = ResampledReadOptions { window: Some((2.0, 4.0, 3.0, 2.0)), size: (3, 2), ..Default::default() };
        let copy = r.read_window(Window::new(2, 4, 3, 2), &WindowOptions::default()).unwrap();
        assert_eq!(read_values_u8(&r.read_resampled(&o).unwrap().raster.data), read_values_u8(&copy.raster.data));
    }

    /// Nodata masks: a nodata value outside the type's range masks nothing, NaN masks NaN.
    #[test]
    fn nodata_masks_are_gdals() {
        let mut r = raster(RasterData::U8(vec![7, 1, 7, 2]), 2, 2, 1, Some(7.0));
        let o = ResampledReadOptions { size: (4, 4), ..Default::default() };
        let (ds, masks) = r.read_resampled_with_masks(&o).unwrap();
        assert_eq!(masks, r.read_resampled_masks(&o).unwrap());
        let expected: Vec<u8> = read_values_u8(&ds.raster.data).iter().map(|&v| if v == 7 { 0 } else { 255 }).collect();
        assert_eq!(masks, expected);
        let mut r = raster(RasterData::U8(vec![7, 1, 7, 2]), 2, 2, 1, Some(-1.0));
        assert!(r.read_resampled_masks(&o).unwrap().iter().all(|&m| m == 255));
        let mut r = raster(RasterData::F32(vec![f32::NAN, 1.0, 2.0, 3.0]), 2, 2, 1, Some(f64::NAN));
        let masks = r.read_resampled_masks(&ResampledReadOptions { size: (2, 2), ..Default::default() }).unwrap();
        assert_eq!(masks, [0, 255, 255, 255]);
    }

    fn read_values_u8(data: &RasterData) -> Vec<u8> {
        match data {
            RasterData::U8(v) => v.clone(),
            other => panic!("not Byte: {:?}", other.data_type()),
        }
    }

    #[test]
    fn refusals() {
        let mut r = raster(RasterData::U8(vec![0; 16]), 4, 4, 1, None);
        let read = |r: &mut LazyRaster, o: ResampledReadOptions| r.read_resampled(&o).unwrap_err().to_string();
        let o = ResampledReadOptions { size: (2, 2), ..Default::default() };
        assert!(read(&mut r, ResampledReadOptions { size: (0, 2), ..o.clone() }).contains("empty"));
        assert!(read(&mut r, ResampledReadOptions { bands: vec![2], ..o.clone() }).contains("no band 2"));
        // Windows past the raster, and empty ones.
        for window in [(3.0, 0.0, 2.0, 2.0), (-0.5, 0.0, 2.0, 2.0), (0.0, 0.0, 0.0, 2.0), (0.0, 0.0, f64::NAN, 1.0)] {
            assert!(read(&mut r, ResampledReadOptions { window: Some(window), ..o.clone() }).contains("not within"), "{window:?}");
        }
        assert!(read(&mut r, ResampledReadOptions { resampling: Resampling::Max, ..o.clone() }).contains("warps"));
        // Past the edge by less than an output pixel (half the window's width here): read, as
        // GDAL's virtual datasets request it; by a whole one, refused.
        assert!(r.read_resampled(&ResampledReadOptions { window: Some((0.0, 0.0, 7.5, 4.0)), ..o.clone() }).is_ok());
        assert!(read(&mut r, ResampledReadOptions { window: Some((0.0, 0.0, 8.0, 4.0)), ..o.clone() }).contains("not within"));
        // GDAL averages the colors of a paletted band at full resolution.
        let mut ds = RasterDataset::new(
            crate::pixel::raster_from_data(
                wbraster::RasterConfig { cols: 4, rows: 4, bands: 1, data_type: DataType::U8, ..Default::default() },
                RasterData::U8(vec![0; 16]),
            )
            .unwrap(),
            false,
        );
        ds.color_table = vec![[0, 0, 0, 255], [255, 255, 255, 255]];
        let mut paletted = LazyRaster::from(ds);
        let err = read(&mut paletted, ResampledReadOptions { resampling: Resampling::Average, ..o.clone() });
        assert!(err.contains("paletted"), "{err}");
        // Interpolating kernels read a paletted band by nearest neighbour.
        assert!(paletted.read_resampled(&ResampledReadOptions { resampling: Resampling::Cubic, ..o }).is_ok());
    }
}
