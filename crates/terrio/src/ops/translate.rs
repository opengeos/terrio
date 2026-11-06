// SPDX-FileCopyrightText: Copyright (c) 1998, 2002, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2007-2015, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2015, Faza Mahamood
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.13.3:
//     apps/gdal_translate_lib.cpp
//     frmts/vrt/vrtsources.cpp (VRTSimpleSource::GetSrcDstWindow)

//! Subsetting, resizing, rescaling and type conversion: an equivalent of `gdal_translate`.
//!
//! As in GDAL, the output is a window of the source (`srcwin`, or `projwin` in georeferenced
//! coordinates) read into an output of the same or another size, band by band, through the
//! same windowing as VRT sources. Values are then optionally rescaled and converted to the output
//! type with GDAL's rounding and clamping.

use super::Ct as Transformer;
use super::overview::OverviewSelection;
use super::vrt::SourceRead;
use super::{Grid, Resampling, convert_value, type_range};
use crate::crs::SpatialRef;
use crate::{Error, LazyRaster, RasterDataset, ResampledReadOptions, Result};
use wbraster::DataType;
use wbraster::raster::RasterData;

/// An output dimension: pixels or a percentage of the source window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Size {
    /// A number of pixels; 0 keeps the aspect ratio of the other dimension.
    Pixels(usize),
    /// A percentage of the source window.
    Percent(f64),
}

impl Size {
    /// Parses `512` or `50%`.
    pub fn parse(s: &str) -> Result<Self> {
        let bad = || Error::InvalidArgument(format!("invalid size '{s}'"));
        match s.strip_suffix('%') {
            Some(p) => Ok(Self::Percent(p.trim().parse().map_err(|_| bad())?)),
            None => Ok(Self::Pixels(s.trim().parse().map_err(|_| bad())?)),
        }
    }

    fn is_zero(&self) -> bool {
        matches!(self, Self::Pixels(0)) || matches!(self, Self::Percent(p) if *p == 0.0)
    }
}

/// Linear rescaling: `[src_min, src_max]` maps to `[dst_min, dst_max]`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Scale {
    /// The source range; the band's minimum and maximum when `None`.
    pub src: Option<(f64, f64)>,
    /// The output range; the output type's range (0–1 for floating-point types) when `None`.
    pub dst: Option<(f64, f64)>,
}

/// Options for [`translate`], named after `gdal_translate`'s.
#[derive(Debug, Clone, Default)]
pub struct TranslateOptions {
    /// The bands to keep, from 1 (`-b`). All when empty.
    pub bands: Vec<usize>,
    /// The output data type (`-ot`).
    pub output_type: Option<DataType>,
    /// The source window `(x_off, y_off, x_size, y_size)` in pixels (`-srcwin`).
    pub src_win: Option<(f64, f64, f64, f64)>,
    /// The source window `(ulx, uly, lrx, lry)` in georeferenced coordinates (`-projwin`).
    pub proj_win: Option<(f64, f64, f64, f64)>,
    /// The CRS of [`TranslateOptions::proj_win`] (`-projwin_srs`).
    pub proj_win_srs: Option<SpatialRef>,
    /// The output size (`-outsize`).
    pub out_size: Option<(Size, Size)>,
    /// The output resolution `(x, y)` (`-tr`).
    pub resolution: Option<(f64, f64)>,
    /// The resampling method when the size changes (`-r`). Nearest by default.
    pub resampling: Option<Resampling>,
    /// Linear rescaling (`-scale`), applied to all bands.
    pub scale: Option<Scale>,
    /// Apply a power function after scaling: `dst_min + (dst_max - dst_min) * t^exponent`, with
    /// `t` the clipped position in the source range (`-exponent`).
    pub exponent: Option<f64>,
    /// The nodata value to assign (`-a_nodata`): `Some(None)` removes it.
    pub assign_nodata: Option<Option<f64>>,
    /// The CRS to assign (`-a_srs`).
    pub assign_srs: Option<SpatialRef>,
    /// The bounds to assign `(ulx, uly, lrx, lry)` (`-a_ullr`).
    pub assign_ullr: Option<(f64, f64, f64, f64)>,
}

/// The geometry of a translation: the source window, the output size and geotransform, and the
/// source and destination rectangles once the window is clamped to the raster
/// (`FixSrcDstWindow`), `None` when it does not intersect it.
#[derive(Debug, Clone, Copy)]
struct Geometry {
    win: (f64, f64, f64, f64),
    size: (usize, usize),
    gt: [f64; 6],
    rects: Option<([f64; 4], [f64; 4])>,
}

/// The geometry `gdal_translate` gives a translation of `ds` (whose pixels it does not read).
fn geometry(ds: &RasterDataset, o: &TranslateOptions) -> Result<Geometry> {
    let r = &ds.raster;
    let (w, h) = (r.cols as f64, r.rows as f64);
    let src_gt = ds.geo_transform();
    let nearest = o.resampling.is_none_or(|m| m == Resampling::Nearest);

    // The source window.
    let mut win = o.src_win.unwrap_or((0.0, 0.0, w, h));
    if let Some((mut ulx, mut uly, mut lrx, mut lry)) = o.proj_win {
        if let Some(srs) = &o.proj_win_srs {
            let ds_srs = ds.crs();
            if !ds_srs.is_unknown() && !srs.same_as(&ds_srs) {
                let b = super::transform_bounds(&Transformer::new(srs, &ds_srs)?, (ulx, lry, lrx, uly), 21)?;
                (ulx, lry, lrx, uly) = b;
            }
        }
        let mut xoff = (ulx - src_gt[0]) / src_gt[1];
        let mut yoff = (uly - src_gt[3]) / src_gt[5];
        if nearest {
            xoff = (xoff + 0.001).floor();
            yoff = (yoff + 0.001).floor();
            ulx = xoff * src_gt[1] + src_gt[0];
            uly = yoff * src_gt[5] + src_gt[3];
        }
        let mut xs = (lrx - ulx) / src_gt[1];
        let mut ys = (lry - uly) / src_gt[5];
        if nearest {
            xs = (xs - 0.001).ceil();
            ys = (ys - 0.001).ceil();
        }
        win = (xoff, yoff, xs, ys);
    }
    if win.2 <= 0.0 || win.3 <= 0.0 {
        return Err(Error::InvalidArgument(format!("the source window {win:?} is empty")));
    }
    if win.0 + win.2 <= 0.0 || win.1 + win.3 <= 0.0 || win.0 >= w || win.1 >= h {
        return Err(Error::InvalidArgument(format!("the source window {win:?} falls completely outside the raster")));
    }

    // The output size.
    let (ow, oh) = if let Some((xr, yr)) = o.resolution {
        ((win.2 / xr * src_gt[1] + 0.5) as usize, (win.3 / yr * src_gt[5].abs() + 0.5) as usize)
    } else if let Some((sx, sy)) = o.out_size {
        let dim = |s: Size, len: f64| match s {
            Size::Pixels(p) => p,
            Size::Percent(p) => (p / 100.0 * len) as usize,
        };
        let (mut ow, mut oh) = (dim(sx, win.2), dim(sy, win.3));
        if sx.is_zero() {
            ow = (oh as f64 * win.2 / win.3 + 0.5) as usize;
        } else if sy.is_zero() {
            oh = (ow as f64 * win.3 / win.2 + 0.5) as usize;
        }
        (ow, oh)
    } else {
        ((win.2 - 0.001).ceil() as usize, (win.3 - 0.001).ceil() as usize)
    };
    if ow == 0 || oh == 0 {
        return Err(Error::InvalidArgument(format!("the output size {ow}x{oh} is empty")));
    }

    // The output geotransform.
    let gt = if let Some((ulx, uly, lrx, lry)) = o.assign_ullr {
        [ulx, (lrx - ulx) / ow as f64, 0.0, uly, 0.0, (lry - uly) / oh as f64]
    } else {
        let mut g = src_gt;
        g[0] += win.0 * g[1];
        g[3] += win.1 * g[5];
        g[1] *= win.2 / ow as f64;
        g[5] *= win.3 / oh as f64;
        if let Some((xr, yr)) = o.resolution {
            g[1] = xr;
            g[5] = if g[5] > 0.0 { yr } else { -yr };
        }
        g
    };

    // The destination window, after clamping the source window to the raster (FixSrcDstWindow).
    let (mut s, mut d) = ([win.0, win.1, win.2, win.3], [0.0, 0.0, ow as f64, oh as f64]);
    if o.resolution.is_some() && !nearest {
        d[2] = win.2 * src_gt[1] / gt[1];
        d[3] = win.3 * (src_gt[5] / gt[5]).abs();
    }
    let rects = fix_windows(&mut s, &mut d, w, h).then_some((s, d));
    Ok(Geometry { win, size: (ow, oh), gt, rects })
}

/// Copies a window of a raster to a new one, optionally resized, rescaled, retyped and with new
/// georeferencing, as `gdal_translate` does.
pub fn translate(ds: &RasterDataset, o: &TranslateOptions) -> Result<RasterDataset> {
    translate_pixels(ds, o, None)
}

/// [`translate`] of a dataset it takes: an output that copies the whole of it, every band in
/// order, in its type, by nearest neighbour and not rescaled, has its pixels converted where they
/// are instead of copied.
fn translate_owned(mut ds: RasterDataset, o: &TranslateOptions) -> Result<RasterDataset> {
    let r = &ds.raster;
    let whole = [0.0, 0.0, r.cols as f64, r.rows as f64];
    let copies = o.scale.is_none()
        && o.exponent.is_none()
        && o.resampling.is_none_or(|m| m == Resampling::Nearest)
        && o.output_type.is_none_or(|t| t == r.data_type)
        && (o.bands.is_empty() || o.bands.iter().copied().eq(1..=r.bands))
        && geometry(&ds, o).is_ok_and(|g| g.size == (r.cols, r.rows) && g.rects == Some((whole, whole)));
    if !copies {
        return translate(&ds, o);
    }
    let empty = RasterData::new_filled(r.data_type, 0, 0.0);
    let pixels = std::mem::replace(&mut ds.raster.data, empty);
    translate_pixels(&ds, o, Some(pixels))
}

/// [`translate`], with the pixels of `ds` taken out as `pixels` when the output copies them all
/// ([`translate_owned`]).
fn translate_pixels(ds: &RasterDataset, o: &TranslateOptions, pixels: Option<RasterData>) -> Result<RasterDataset> {
    let r = &ds.raster;
    let (w, h) = (r.cols as f64, r.rows as f64);
    let nearest = o.resampling.is_none_or(|m| m == Resampling::Nearest);
    let Geometry { win, size: (ow, oh), gt, rects } = geometry(ds, o)?;

    let bands: Vec<usize> = if o.bands.is_empty() { (1..=r.bands).collect() } else { o.bands.clone() };
    if let Some(b) = bands.iter().find(|b| **b == 0 || **b > r.bands) {
        return Err(Error::InvalidArgument(format!("band {b} does not exist (the raster has {})", r.bands)));
    }
    let out_type = o.output_type.unwrap_or(r.data_type);
    let out_nodata: Vec<Option<f64>> = bands.iter().map(|b| o.assign_nodata.unwrap_or_else(|| ds.band_nodata(*b))).collect();
    let Some((s, d)) = rects else {
        return Err(Error::InvalidArgument("the source window does not intersect the raster".into()));
    };

    let complex = o.scale.is_some() || o.exponent.is_some();
    let n = ow * oh;
    let in_place = pixels.is_some();
    let mut out = pixels.unwrap_or_else(|| RasterData::new_filled(out_type, n * bands.len(), 0.0));
    for (k, &b) in bands.iter().enumerate() {
        if in_place {
            // Converted as the copy below converts them.
            crate::pixel::with_data!(&mut out, v => {
                crate::par::chunks_mut(&mut v[k * n..(k + 1) * n], crate::par::chunk_len(n, 1 << 16), |_, chunk| {
                    chunk.iter_mut().for_each(|x| *x = convert_value(crate::pixel::Pixel::to_f64(*x), out_type) as _);
                });
            });
            continue;
        }
        let src_nodata = ds.band_nodata(b);
        let values = super::BandView::of(r, b - 1);
        // Rescaling and resampling skip invalid pixels; a nearest-neighbour read copies them.
        let valid: Option<Vec<bool>> = if complex || !nearest { src_nodata.map(|nd| values.valid(nd)) } else { None };
        let read = SourceRead {
            values,
            valid: valid.as_deref(),
            width: r.cols,
            height: r.rows,
            src_type: r.data_type,
            src_rect: s,
            dst_rect: d,
            resampling: o.resampling.unwrap_or_default(),
            working_float: complex,
            invalid_value: if complex { None } else { src_nodata },
        };
        // Scaling parameters, as gdal_translate derives them.
        let scaling = if complex {
            let sc = o.scale.unwrap_or_default();
            let (smin, mut smax) = match sc.src {
                Some(r) => r,
                None => min_max(values, valid.as_deref()).unwrap_or((0.0, 0.0)),
            };
            if smax == smin {
                smax += 0.1;
            }
            let (dmin, dmax) = sc.dst.unwrap_or_else(|| match out_type {
                DataType::F32 | DataType::F64 => (0.0, 1.0),
                DataType::U64 => (0.0, 18446744073709549568.0),
                DataType::I64 => (-9223372036854774784.0, 9223372036854773760.0),
                t => type_range(t),
            });
            Some((smin, smax, dmin, dmax))
        } else {
            None
        };
        let float_working = matches!(r.data_type, DataType::F64 | DataType::I32 | DataType::U32 | DataType::I64 | DataType::U64);
        let fill = convert_value(out_nodata[k].unwrap_or(0.0), out_type);
        let scaled = |v: f64| match scaling {
            None => v,
            Some((smin, smax, dmin, dmax)) => {
                let s = match o.exponent {
                    Some(e) if e != 1.0 => {
                        let t = ((v - smin) / (smax - smin)).clamp(0.0, 1.0);
                        (dmax - dmin) * t.powf(e) + dmin
                    }
                    _ => {
                        let ratio = (dmax - dmin) / (smax - smin);
                        v * ratio + (dmin - smin * ratio)
                    }
                };
                if float_working { s } else { s as f32 as f64 }
            }
        };
        // Written in the output's type, converted as they are stored.
        crate::pixel::with_data!(&mut out, v => {
            let band = &mut v[k * n..(k + 1) * n];
            band.iter_mut().for_each(|x| *x = fill as _);
            read.read(ow, oh, band, |x| convert_value(scaled(x), out_type) as _);
        });
    }
    let grid = Grid { width: ow, height: oh, geo_transform: gt };
    let srs = o.assign_srs.clone().unwrap_or_else(|| ds.crs());
    let mut res = super::build_raster_data(grid, out, bands.len(), out_nodata.first().copied().flatten(), &srs)?;
    res.raster.metadata = ds.raster.metadata.iter().filter(|(k, _)| k == "AREA_OR_POINT").cloned().collect();
    res.band_metadata = bands.iter().map(|&b| ds.band_metadata.get(b - 1).cloned().unwrap_or_default()).collect();
    if win != (0., 0., w, h)
        || ow != r.cols
        || oh != r.rows
        || out_type != r.data_type
        || o.scale.is_some()
        || o.exponent.is_some()
        || o.assign_nodata.is_some()
    {
        for band in &mut res.band_metadata {
            band.items.retain(|(key, _)| !key.starts_with("STATISTICS_"));
        }
    }

    if o.assign_ullr.is_none() {
        res.gcps = ds
            .gcps
            .iter()
            .cloned()
            .map(|mut g| {
                g.pixel = (g.pixel - win.0) * ow as f64 / win.2;
                g.line = (g.line - win.1) * oh as f64 / win.3;
                g
            })
            .collect();
    }
    res.band_nodata = out_nodata;
    res.driver = ds.driver;
    Ok(res)
}

/// [`translate`] of a raster read on demand, as `gdal_translate` reads it: the window into the
/// output's size, from the stored resolution `overview` selects as `-ovr` does
/// ([`LazyRaster::read_resampled`]: by default the overview `RasterIO` chooses; `AUTO-n`, `NONE`
/// or a level, that level alone), reading only what the output needs. Bands are read through
/// the dataset when they are its first ones in order, as `gdal_translate`'s virtual dataset
/// reads them, else band by band.
///
/// Rescaling, an output type other than the source's with resampling other than nearest
/// neighbour (which GDAL resamples in the output type), a window past the raster's edges, a
/// destination window of a fraction of a pixel (`-tr` with resampling other than nearest
/// neighbour) and a raster in memory are translated as [`translate`] translates them, from the
/// window's level read whole.
///
/// # Errors
///
/// As [`translate`].
pub fn translate_lazy(src: &mut LazyRaster, o: &TranslateOptions, overview: OverviewSelection) -> Result<RasterDataset> {
    match plan_lazy(src, o, overview)? {
        Planned::Translated(ds) => Ok(ds),
        Planned::Read(read) => read.rows(src, 0, read.read.size.1),
    }
}

/// [`translate_lazy`], written to `path` as [`crate::write_raster_dataset`] writes its output
/// with `options`, the same file. An output read by window and written as a GeoTIFF or a COG is
/// read, converted and written a strip of rows at a time, so that neither it nor the source is
/// held whole (`gdal_translate` of GDAL 3.13.3 copies its output in chunks too), from a source
/// whose windows read only what they cover ([`LazyRaster::reads_by_window`]), when it copies a window of the
/// source or is resampled by nearest neighbour or a bilinear, cubic or cubic spline kernel from
/// the overview `RasterIO` chooses (`OverviewSelection::Auto`). Other outputs (resampled by
/// average, RMS, mode or Lanczos, from a level read alone, of complex bands or of another
/// format), those that [`translate_lazy`] translates from the source read whole, and outputs
/// that replace one of the source's files are written from the output translated whole.
///
/// # Errors
///
/// As [`translate_lazy`] and [`crate::write_raster_dataset`].
pub fn translate_lazy_to_file(
    src: &mut LazyRaster,
    o: &TranslateOptions,
    overview: OverviewSelection,
    path: &str,
    options: &crate::RasterWriteOptions,
) -> Result<()> {
    write_lazy(src, o, overview, path, options, None)
}

/// [`translate_lazy_to_file`], in strips of `rows_per_strip` rows (by default, about 64 MB).
fn write_lazy(
    src: &mut LazyRaster,
    o: &TranslateOptions,
    overview: OverviewSelection,
    path: &str,
    options: &crate::RasterWriteOptions,
    rows_per_strip: Option<usize>,
) -> Result<()> {
    let read = match plan_lazy(src, o, overview)? {
        Planned::Translated(ds) => return crate::write_raster_dataset(&ds, path, options),
        Planned::Read(read) => read,
    };
    let (ow, oh) = read.read.size;
    let driver = options.driver.or_else(|| crate::formats::RasterDriver::for_extension(path));
    let streams = matches!(driver, Some(crate::formats::RasterDriver::GTiff | crate::formats::RasterDriver::Cog))
        && !src.is_complex()
        && src.input_paths().is_ok_and(|inputs| !super::strips::overwrites(path, &inputs));
    if !streams || !read.by_strips {
        return crate::write_raster_dataset(&read.rows(src, 0, oh)?, path, options);
    }
    // About 64 MB of values per strip, counted as the 8-byte values sources such as VRTs read in;
    // a copy's strips are rows of the source's blocks (a VRT's sources'), so that each is read
    // once, when the window starts on one and a row of blocks holds at most 2 GB of values (512 rows
    // of a 98304-wide RGB mosaic are 1.2 GB); taller rows of blocks are split.
    let start = read.read.window.map_or(0.0, |w| w.1);
    let block = if read.copy { src.row_alignment().filter(|h| start % *h as f64 == 0.0) } else { None };
    let block = super::strips::bounded_alignment(ow, read.read.bands.len(), 8, block);
    let rows = rows_per_strip.unwrap_or_else(|| super::strips::strip_rows(ow, read.read.bands.len(), 8, block)).clamp(1, oh);
    let mut first = read.rows(src, 0, rows)?;
    let data = std::mem::replace(&mut first.raster.data, RasterData::new_filled(first.raster.data_type, 0, 0.0));
    let header = read.header(src, first, oh);
    let mut out = super::strips::StripWriter::create(&header, path, options)?;
    out.write(data, rows)?;
    let rest = (oh - rows).div_ceil(rows);
    super::strips::pipelined(
        rest,
        |i| {
            let y = (i + 1) * rows;
            let h = rows.min(oh - y);
            Ok((read.rows(src, y, h)?.raster.data, h))
        },
        |_, (data, h)| out.write(data, h),
    )?;
    out.finish()
}

/// How [`translate_lazy`] makes its output.
enum Planned {
    /// Translated from the source (or the level) read whole.
    Translated(RasterDataset),
    /// Read by window.
    Read(LazyRead),
}

/// The output of [`translate_lazy`] read by window: resampled from the source, then converted.
struct LazyRead {
    /// The whole output's read.
    read: ResampledReadOptions,
    /// Whether the bands are read together (see [`translate_lazy`]).
    together: bool,
    /// The output's geotransform, before `-a_ullr`.
    gt: [f64; 6],
    /// The source's GCPs on the output's grid.
    gcps: Vec<crate::Gcp>,
    /// Whether the read is the whole image at full resolution, which keeps band statistics.
    whole: bool,
    /// Whether a strip of the output's rows read alone holds the values the whole read gives
    /// them, from a source whose windows are its whole read's ([`LazyRaster::reads_by_window`]):
    /// an output that copies a window of the source, or one resampled by nearest
    /// neighbour or a bilinear, cubic or cubic spline kernel from the overview `RasterIO`
    /// chooses. Other resamplings (average, RMS, mode, Lanczos) of a strip starting within a
    /// source row differ from the whole read's, as may a level read alone.
    by_strips: bool,
    /// Whether the output copies a window of the source, pixel for pixel.
    copy: bool,
    /// The conversion of the values read: type, nodata and georeferencing.
    values: TranslateOptions,
}

impl LazyRead {
    /// Rows `y..y + h` of the output, as the whole output holds them: the window of the source
    /// they cover read into them, as GDAL reads a chunk of a virtual dataset's rows. The rows of
    /// the whole output are its whole read.
    fn rows(&self, src: &mut LazyRaster, y: usize, h: usize) -> Result<RasterDataset> {
        let (ow, oh) = self.read.size;
        let read = if (y, h) == (0, oh) {
            self.read.clone()
        } else {
            let window = self.read.window.map(|(wx, wy, wxs, wys)| {
                let ry = wys / oh as f64;
                (wx, wy + y as f64 * ry, wxs, h as f64 * ry)
            });
            ResampledReadOptions { window, size: (ow, h), ..self.read.clone() }
        };
        let mut resized = if self.together { src.read_resampled(&read)? } else { src.read_resampled_by_band(&read)? };
        let mut gt = self.gt;
        gt[0] += y as f64 * gt[2];
        gt[3] += y as f64 * gt[5];
        resized.set_geo_transform(gt);
        resized.gcps = self
            .gcps
            .iter()
            .cloned()
            .map(|mut g| {
                g.line -= y as f64;
                g
            })
            .collect();
        translate_owned(resized, &self.values)
    }

    /// The header of the whole output, of `oh` rows, from `first`, its first rows (their pixels
    /// taken out): their description, with the whole output's georeferencing, GCPs and band
    /// statistics.
    fn header(&self, src: &LazyRaster, first: RasterDataset, oh: usize) -> RasterDataset {
        let mut header = first;
        header.raster.rows = oh;
        let ow = self.read.size.0;
        let gt = match self.values.assign_ullr {
            Some((ulx, uly, lrx, lry)) => [ulx, (lrx - ulx) / ow as f64, 0.0, uly, 0.0, (lry - uly) / oh as f64],
            None => self.gt,
        };
        header.set_geo_transform(gt);
        // `-a_ullr` drops the GCPs, as `translate` does.
        if self.values.assign_ullr.is_none() {
            header.gcps = self.gcps.clone();
        }
        // A whole read keeps the source's band statistics, unless the values change.
        if self.whole {
            header.band_metadata = src.bands_metadata(&self.read.bands);
            if self.values.output_type.is_some_and(|t| t != src.data_type()) || self.values.assign_nodata.is_some() {
                for band in &mut header.band_metadata {
                    band.items.retain(|(key, _)| !key.starts_with("STATISTICS_"));
                }
            }
        }
        header
    }
}

/// Plans [`translate_lazy`]: the output translated from the source read whole, or its read.
fn plan_lazy(src: &mut LazyRaster, o: &TranslateOptions, overview: OverviewSelection) -> Result<Planned> {
    let header = src.header();
    let full = geometry(&header, o)?;
    let count = header.raster.bands;
    let bands: Vec<usize> = if o.bands.is_empty() { (1..=count).collect() } else { o.bands.clone() };
    if let Some(b) = bands.iter().find(|b| **b == 0 || **b > count) {
        return Err(Error::InvalidArgument(format!("band {b} does not exist (the raster has {count})")));
    }
    let overviews = src.overviews();
    let fixed = crate::resampled_read::fixed_level(overview, &overviews, full.size.0);
    let (selection, level) = match fixed {
        Some(Some(i)) => (OverviewSelection::Level(i), Some(i)),
        Some(None) => (OverviewSelection::None, None),
        None => (OverviewSelection::Auto, None),
    };
    // A level read alone (`-ovr`): the window scaled to it, the output's geotransform from its
    // own, and, without an output size, the output at its resolution.
    let (level_header, level_options) = match level {
        Some(i) => {
            let (lw, lh) = overviews[i];
            let (w, h) = (header.raster.cols as f64, header.raster.rows as f64);
            let (rx, ry) = (w / lw as f64, h / lh as f64);
            let mut lh_ds = header.clone();
            (lh_ds.raster.cols, lh_ds.raster.rows) = (lw, lh);
            lh_ds.set_geo_transform(src.level_geo_transform(Some(i))?);
            for g in &mut lh_ds.gcps {
                (g.pixel, g.line) = (g.pixel * lw as f64 / w, g.line * lh as f64 / h);
            }
            let (ow, oh) = if o.out_size.is_some() || o.resolution.is_some() {
                full.size
            } else {
                let at = |n: usize, r: f64| ((n as f64 / r - 0.001).ceil() as usize).max(1);
                (at(full.size.0, rx), at(full.size.1, ry))
            };
            let (x, y, xs, ys) = full.win;
            let lo = TranslateOptions {
                src_win: Some((x / rx, y / ry, xs / rx, ys / ry)),
                proj_win: None,
                proj_win_srs: None,
                out_size: Some((Size::Pixels(ow), Size::Pixels(oh))),
                ..o.clone()
            };
            (lh_ds, lo)
        }
        None => (header, o.clone()),
    };
    let resampling = o.resampling.unwrap_or_default();
    let nearest = resampling == Resampling::Nearest;
    let geom = match level {
        Some(_) => {
            // The output's size, set above; `-tr` sets the output's resolution.
            let mut g = geometry(&level_header, &TranslateOptions { resolution: None, ..level_options.clone() })?;
            if let Some((xr, yr)) = o.resolution {
                g.gt[1] = xr;
                g.gt[5] = if g.gt[5] > 0.0 { yr } else { -yr };
                if !nearest && let Some((_, d)) = &mut g.rects {
                    // With resampling, GDAL maps the window to the output at the resolution.
                    let src_gt = level_header.geo_transform();
                    d[2] = g.win.2 * src_gt[1] / xr;
                    d[3] = g.win.3 * (src_gt[5] / g.gt[5]).abs();
                }
            }
            g
        }
        None => full,
    };
    let (ow, oh) = geom.size;
    let (x, y, xs, ys) = geom.win;
    // The window within the raster, mapped onto the whole output (or, with `-tr` and resampling,
    // most of its last row or column): the window of the output requested from the source, as
    // `gdal_translate`'s VRT source requests it (`VRTSimpleSource::GetSrcDstWindow`), possibly
    // past the raster's edge by less than an output pixel.
    // `s` and `d` are the window and its destination clamped to the raster (`fix_windows`): a
    // window past the raster's edge maps onto part of the output. A level read alone (`-ovr`,
    // including `NONE`) scales and clamps its request to the level again (`fixed_request`), so a
    // window past its edge is translated from the level read whole.
    let request = geom.rects.and_then(|(s, d)| {
        if fixed.is_some() && s != [x, y, xs, ys] {
            return None;
        }
        if d == [0.0, 0.0, ow as f64, oh as f64] {
            return Some((s[0], s[1], s[2], s[3]));
        }
        // The output's pixels the destination reaches into, as GDAL counts them (to within
        // 0.001): all of them, the window then requested past its end in proportion.
        let reaches = |end: f64, n: usize| (end - 1e-3).ceil() >= n as f64;
        let covers = d[0] <= 0.0 && d[1] <= 0.0 && reaches(d[0] + d[2], ow) && reaches(d[1] + d[3], oh);
        let (rx, ry) = (s[2] / d[2], s[3] / d[3]);
        covers.then(|| (s[0] - d[0] * rx, s[1] - d[1] * ry, ow as f64 * rx, oh as f64 * ry))
    });
    let source_type = src.data_type();
    let lazy = src.is_lazy()
        && request.is_some()
        && o.scale.is_none()
        && o.exponent.is_none()
        && (nearest || o.output_type.is_none_or(|t| t == source_type))
        && (nearest || !src.is_complex())
        && matches!(
            resampling,
            Resampling::Nearest
                | Resampling::Bilinear
                | Resampling::Cubic
                | Resampling::CubicSpline
                | Resampling::Lanczos
                | Resampling::Average
                | Resampling::Rms
                | Resampling::Mode
        );
    if !lazy {
        return match level {
            Some(i) => {
                let (lw, lh) = overviews[i];
                let ds = src.read_window(crate::Window::full(lw, lh), &crate::WindowOptions { bands: Vec::new(), overview: Some(i) })?;
                translate_owned(ds, &level_options)
            }
            None => match src.dataset() {
                Some(ds) => translate(ds, o),
                None => translate_owned(src.read()?, o),
            },
        }
        .map(Planned::Translated);
    }
    // The request in full-resolution pixels, which `read_resampled` scales to a level read
    // alone as `gdal_translate -ovr` does, rounding it there as the VRT source does.
    let window = request.map(|r| match level {
        Some(_) if r == geom.win => full.win,
        Some(i) => {
            let (lw, lh) = overviews[i];
            let (sx, sy) = (src.width() as f64 / lw as f64, src.height() as f64 / lh as f64);
            (r.0 * sx, r.1 * sy, r.2 * sx, r.3 * sy)
        }
        None => (round_if_close(r.0), round_if_close(r.1), round_if_close(r.2), round_if_close(r.3)),
    });
    let read = ResampledReadOptions { bands: bands.clone(), window, size: geom.size, resampling, overview: selection };
    // gdal_translate's virtual dataset reads its bands together when they are the source's
    // first ones in order (`VRTDataset::CheckCompatibleForDatasetIO`).
    let together = bands.iter().enumerate().all(|(i, &b)| b == i + 1);
    let gcps = level_header
        .gcps
        .iter()
        .cloned()
        .map(|mut g| {
            g.pixel = (g.pixel - x) * ow as f64 / xs;
            g.line = (g.line - y) * oh as f64 / ys;
            g
        })
        .collect();
    // As `LazyRaster::resampled_dataset` tells a read of the whole image at full resolution.
    let (w, h) = (src.width() as f64, src.height() as f64);
    let whole = level.is_none() && read.window == Some((0.0, 0.0, w, h)) && (ow as f64, oh as f64) == (w, h);
    let copy = level.is_none()
        && read
            .window
            .is_some_and(|(wx, wy, wxs, wys)| [wx, wy, wxs, wys].iter().all(|v| v.fract() == 0.0) && (wxs, wys) == (ow as f64, oh as f64));
    // A level read alone (`--overview NONE` or a level) rounds each request's window to whole
    // pixels within 0.001 (`fixed_request`): a strip's own origin may round off the whole
    // output's sampling grid.
    // A source whose windows are not its whole read's (a VRT of resampled sources) is read whole.
    let by_strips = src.reads_by_window()
        && (copy
            || (fixed.is_none()
                && matches!(resampling, Resampling::Nearest | Resampling::Bilinear | Resampling::Cubic | Resampling::CubicSpline)));
    let values = TranslateOptions {
        output_type: o.output_type,
        assign_nodata: o.assign_nodata,
        assign_srs: o.assign_srs.clone(),
        assign_ullr: o.assign_ullr,
        ..Default::default()
    };
    Ok(Planned::Read(LazyRead { read, together, gt: geom.gt, gcps, whole, by_strips, copy, values }))
}

/// A value within 0.001 of a whole number, rounded to it, as `gdal_translate`'s VRT source rounds
/// its request (`RoundIfCloseToInt`).
pub(crate) fn round_if_close(v: f64) -> f64 {
    let closest = (v + 0.5).floor();
    if (v - closest).abs() < 1e-3 { closest } else { v }
}

/// The smallest and largest valid values.
fn min_max(values: super::BandView, valid: Option<&[bool]>) -> Option<(f64, f64)> {
    let mut it = (0..values.len()).map(|i| (i, values.get(i))).filter(|(i, v)| !v.is_nan() && valid.is_none_or(|m| m[*i])).map(|(_, v)| v);
    let first = it.next()?;
    Some(it.fold((first, first), |(a, b), v| (a.min(v), b.max(v))))
}

/// GDAL's `FixSrcDstWindow()`: clamps the source window to the raster and shrinks the destination
/// window in proportion.
fn fix_windows(s: &mut [f64; 4], d: &mut [f64; 4], w: f64, h: f64) -> bool {
    let (mut modified_x, mut modified_y) = (false, false);
    let (mut sxo, mut syo, mut sxs, mut sys) = (s[0], s[1], s[2], s[3]);
    if sxo < 0.0 {
        sxs += sxo;
        sxo = 0.0;
        modified_x = true;
    }
    if syo < 0.0 {
        sys += syo;
        syo = 0.0;
        modified_y = true;
    }
    if sxo + sxs > w {
        sxs = w - sxo;
        modified_x = true;
    }
    if syo + sys > h {
        sys = h - syo;
        modified_y = true;
    }
    if !modified_x && !modified_y {
        return true;
    }
    if sxs <= 0.0 || sys <= 0.0 {
        return false;
    }
    let (dxo, dyo, dxs, dys) = (d[0], d[1], d[2], d[3]);
    if modified_x {
        let ratio = dxs / s[2];
        d[0] = dxo + (sxo - s[0]) * ratio;
        d[2] = sxs * ratio;
    }
    if modified_y {
        let ratio = dys / s[3];
        d[1] = dyo + (syo - s[1]) * ratio;
        d[3] = sys * ratio;
    }
    *s = [sxo, syo, sxs, sys];
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbraster::{Raster, RasterConfig};

    fn ds(cols: usize, rows: usize) -> RasterDataset {
        let cfg = RasterConfig {
            cols,
            rows,
            bands: 2,
            x_min: 100.0,
            y_min: 200.0 - rows as f64 * 10.0,
            cell_size: 10.0,
            cell_size_y: Some(10.0),
            nodata: -1.0,
            data_type: DataType::F32,
            crs: wbraster::CrsInfo::from_epsg(32610),
            metadata: vec![],
        };
        let n = cols * rows;
        let data = (0..2 * n).map(|i| (i % n) as f64 + (i / n) as f64 * 1000.0).collect();
        RasterDataset::new(Raster::from_data(cfg, data).unwrap(), false)
    }

    /// A translate written a few rows at a time writes the file the whole translate writes:
    /// strips resampled from a window, of a level, retyped, with new nodata or bounds, of some
    /// bands, from a GeoTIFF (with overviews and a mask, or with band statistics) and a VRT.
    #[test]
    fn translate_written_by_strips_is_the_whole_translate() {
        use crate::formats::RasterDriver;
        let dir = tempfile::tempdir().unwrap();
        let at = |name: &str| dir.path().join(name).to_str().unwrap().to_string();
        let rgb = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/resampled_reads/rgb_mask.tif").to_string();
        let mut stats = ds(37, 29);
        for band in &mut stats.band_metadata {
            band.items = vec![("STATISTICS_MAXIMUM".into(), "1072".into()), ("STATISTICS_MINIMUM".into(), "0".into())];
        }
        let stats_tif = at("stats.tif");
        crate::write_raster_dataset(&stats, &stats_tif, &crate::RasterWriteOptions::default()).unwrap();
        let vrt = at("rgb.vrt");
        let band = |b: usize| {
            format!(
                "<VRTRasterBand dataType=\"Byte\" band=\"{b}\"><SimpleSource><SourceFilename relativeToVRT=\"0\">{rgb}</SourceFilename>\
                 <SourceBand>{b}</SourceBand><SrcRect xOff=\"0\" yOff=\"0\" xSize=\"64\" ySize=\"48\"/>\
                 <DstRect xOff=\"0\" yOff=\"0\" xSize=\"64\" ySize=\"48\"/></SimpleSource></VRTRasterBand>"
            )
        };
        std::fs::write(
            &vrt,
            format!(
                "<VRTDataset rasterXSize=\"64\" rasterYSize=\"48\"><GeoTransform>500000, 30, 0, 4100000, 0, -30</GeoTransform>{}</VRTDataset>",
                (1..=3).map(band).collect::<String>()
            ),
        )
        .unwrap();
        // A VRT downsampling its source by Lanczos: its windows are not its whole read's.
        let lanczos = at("lanczos.vrt");
        std::fs::write(
            &lanczos,
            format!(
                "<VRTDataset rasterXSize=\"20\" rasterYSize=\"15\"><VRTRasterBand dataType=\"Byte\" band=\"1\">\
                 <ComplexSource resampling=\"lanczos\"><SourceFilename relativeToVRT=\"0\">{rgb}</SourceFilename>\
                 <SourceBand>1</SourceBand><SrcRect xOff=\"0\" yOff=\"0\" xSize=\"64\" ySize=\"48\"/>\
                 <DstRect xOff=\"0\" yOff=\"0\" xSize=\"20\" ySize=\"15\"/></ComplexSource></VRTRasterBand></VRTDataset>"
            ),
        )
        .unwrap();
        let resized =
            |w, h, m| TranslateOptions { out_size: Some((Size::Pixels(w), Size::Pixels(h))), resampling: Some(m), ..Default::default() };
        let cases: Vec<(&str, TranslateOptions, OverviewSelection)> = vec![
            (&rgb, TranslateOptions::default(), OverviewSelection::Auto),
            (&vrt, TranslateOptions::default(), OverviewSelection::Auto),
            (&stats_tif, TranslateOptions::default(), OverviewSelection::Auto),
            (&stats_tif, TranslateOptions { output_type: Some(DataType::I16), ..Default::default() }, OverviewSelection::Auto),
            (
                &stats_tif,
                TranslateOptions { assign_nodata: Some(Some(5.0)), bands: vec![2], ..Default::default() },
                OverviewSelection::Auto,
            ),
            (
                &rgb,
                TranslateOptions { src_win: Some((3.0, 5.0, 54.0, 38.0)), ..resized(13, 9, Resampling::Bilinear) },
                OverviewSelection::Auto,
            ),
            (
                &vrt,
                TranslateOptions { src_win: Some((3.0, 5.0, 54.0, 38.0)), ..resized(25, 17, Resampling::Cubic) },
                OverviewSelection::Auto,
            ),
            // Read whole: an average of strips starting within source rows is not the whole's.
            (&rgb, resized(41, 31, Resampling::Average), OverviewSelection::None),
            (&rgb, resized(13, 9, Resampling::Lanczos), OverviewSelection::Auto),
            (&rgb, resized(13, 9, Resampling::Bilinear), OverviewSelection::Auto),
            (&rgb, resized(20, 15, Resampling::CubicSpline), OverviewSelection::Auto),
            (&rgb, resized(41, 31, Resampling::Nearest), OverviewSelection::None),
            (&rgb, TranslateOptions { src_win: Some((8.0, 4.0, 40.0, 32.0)), ..Default::default() }, OverviewSelection::Level(1)),
            (
                &vrt,
                TranslateOptions { bands: vec![3, 1], assign_ullr: Some((0.0, 48.0, 64.0, 0.0)), ..Default::default() },
                OverviewSelection::Auto,
            ),
            (
                &stats_tif,
                TranslateOptions { resolution: Some((7.0, 7.0)), resampling: Some(Resampling::Bilinear), ..Default::default() },
                OverviewSelection::Auto,
            ),
            // Read whole: the VRT's windows are not its whole read's.
            (&lanczos, TranslateOptions::default(), OverviewSelection::Auto),
        ];
        // Every case is written by strips but the average and the Lanczos resampling, the levels
        // read alone (`NONE` or a level) that are not copies, and the Lanczos VRT.
        for (k, (input, o, overview)) in cases.iter().enumerate() {
            let mut src = crate::open_raster_lazy(input).unwrap();
            let by_strips = matches!(plan_lazy(&mut src, o, *overview).unwrap(), Planned::Read(r) if r.by_strips);
            let resampling = o.resampling.unwrap_or_default();
            let expected = !matches!(resampling, Resampling::Average | Resampling::Lanczos)
                && *overview == OverviewSelection::Auto
                && *input != lanczos.as_str();
            assert_eq!(by_strips, expected, "case {k}: {o:?}");
        }
        for (k, (input, o, overview)) in cases.iter().enumerate() {
            for (driver, ext) in [(RasterDriver::Cog, "cog.tif"), (RasterDriver::GTiff, "tif")] {
                let options = crate::RasterWriteOptions { driver: Some(driver), compression: Some("DEFLATE".into()), ..Default::default() };
                let (whole, strips) = (at(&format!("{k}_whole.{ext}")), at(&format!("{k}_strips.{ext}")));
                let mut src = crate::open_raster_lazy(input).unwrap();
                crate::write_raster_dataset(&translate_lazy(&mut src, o, *overview).unwrap(), &whole, &options).unwrap();
                write_lazy(&mut src, o, *overview, &strips, &options, Some(3)).unwrap();
                let case = format!("case {k} ({driver:?}): {o:?}");
                if driver == RasterDriver::Cog {
                    // The same file, byte for byte.
                    assert!(std::fs::read(&whole).unwrap() == std::fs::read(&strips).unwrap(), "{case}");
                    continue;
                }
                // A GeoTIFF written by strips has its directory at its end: the same raster.
                let (a, b) = (crate::open_raster(&whole).unwrap(), crate::open_raster(&strips).unwrap());
                assert_eq!(a.raster.data.data_type(), b.raster.data.data_type(), "{case}");
                assert_eq!(a.raster.data.to_f64_vec(), b.raster.data.to_f64_vec(), "{case}");
                assert_eq!((a.raster.cols, a.raster.rows, a.raster.bands), (b.raster.cols, b.raster.rows, b.raster.bands), "{case}");
                assert_eq!(a.geo_transform(), b.geo_transform(), "{case}");
                assert_eq!(a.band_nodata, b.band_nodata, "{case}");
                assert_eq!(a.band_metadata, b.band_metadata, "{case}");
                assert_eq!(a.raster.metadata, b.raster.metadata, "{case}");
                assert_eq!(a.crs().to_wkt(), b.crs().to_wkt(), "{case}");
            }
        }
    }

    #[test]
    fn srcwin_and_band() {
        let d = ds(10, 8);
        let o = TranslateOptions { src_win: Some((2.0, 3.0, 4.0, 2.0)), bands: vec![2], ..Default::default() };
        let out = translate(&d, &o).unwrap();
        assert_eq!((out.raster.cols, out.raster.rows, out.raster.bands), (4, 2, 1));
        assert_eq!(out.geo_transform(), [120.0, 10.0, 0.0, 170.0, 0.0, -10.0]);
        assert_eq!(out.raster.data.to_f64_vec(), vec![1032.0, 1033.0, 1034.0, 1035.0, 1042.0, 1043.0, 1044.0, 1045.0]);
    }

    #[test]
    fn projwin_partially_outside() {
        let d = ds(10, 8);
        let o = TranslateOptions { proj_win: Some((80.0, 200.0, 120.0, 180.0)), bands: vec![1], ..Default::default() };
        let out = translate(&d, &o).unwrap();
        assert_eq!((out.raster.cols, out.raster.rows), (4, 2));
        // Two columns left of the raster are filled with 0 (no nodata).
        assert_eq!(out.raster.data.to_f64_vec(), vec![0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 10.0, 11.0]);
    }

    #[test]
    fn scale_and_type() {
        let d = ds(10, 8);
        let o = TranslateOptions {
            bands: vec![1],
            output_type: Some(DataType::U8),
            scale: Some(Scale { src: Some((0.0, 79.0)), dst: Some((0.0, 255.0)) }),
            ..Default::default()
        };
        let out = translate(&d, &o).unwrap();
        let v = out.raster.data.to_f64_vec();
        assert_eq!(v[0], 0.0);
        assert_eq!(v[79], 255.0);
        assert_eq!(v[1], (255.0f64 / 79.0).round());
    }

    #[test]
    fn outsize_percent() {
        let d = ds(10, 8);
        let o = TranslateOptions { out_size: Some((Size::Percent(50.0), Size::Percent(50.0))), bands: vec![1], ..Default::default() };
        let out = translate(&d, &o).unwrap();
        assert_eq!((out.raster.cols, out.raster.rows), (5, 4));
        assert_eq!(out.geo_transform()[1], 20.0);
        // Nearest: output pixel (0, 0) reads source (1, 1).
        assert_eq!(out.raster.data.to_f64_vec()[0], 11.0);
    }

    /// A dataset translated where its pixels are holds what a copy of them holds: a float32
    /// signaling NaN quieted and UInt64 values past `f64`'s whole numbers rounded alike, with new
    /// nodata, bands or bounds; other windows, types and band orders are copied.
    #[test]
    fn translate_in_place_converts_as_a_copy() {
        let mut f32s = ds(3, 2);
        let RasterData::F32(v) = &mut f32s.raster.data else { panic!("not float32") };
        (v[1], v[2], v[3]) = (f32::from_bits(0x7f80_0001), f32::from_bits(0xffc0_1234), -1.0);
        let mut u64s = ds(3, 2);
        u64s.raster.data_type = DataType::U64;
        u64s.raster.data = RasterData::U64((1..12).map(|i| (1u64 << 53) + i).chain([u64::MAX]).collect());
        let bits = |d: &RasterData| match d {
            RasterData::F32(v) => v.iter().map(|x| u64::from(x.to_bits())).collect::<Vec<_>>(),
            RasterData::F64(v) => v.iter().map(|x| x.to_bits()).collect(),
            RasterData::U64(v) => v.clone(),
            _ => unreachable!(),
        };
        let base = TranslateOptions::default();
        let options = [
            base.clone(),
            TranslateOptions { assign_nodata: Some(Some(5.0)), assign_ullr: Some((0.0, 20.0, 30.0, 0.0)), ..base.clone() },
            TranslateOptions { bands: vec![1, 2], output_type: Some(DataType::F32), ..base.clone() },
            TranslateOptions { bands: vec![2, 1], ..base.clone() },
            TranslateOptions { bands: vec![2], ..base.clone() },
            TranslateOptions { output_type: Some(DataType::F64), ..base.clone() },
            TranslateOptions { src_win: Some((1.0, 0.0, 2.0, 2.0)), ..base.clone() },
        ];
        let at = |d: &RasterData| crate::pixel::with_data!(d, v => v.as_ptr() as usize);
        for d in [f32s, u64s] {
            for (i, o) in options.iter().enumerate() {
                let owned = d.clone();
                let pixels = at(&owned.raster.data);
                let (copy, in_place) = (translate(&d, o).unwrap(), translate_owned(owned, o).unwrap());
                // The options copying every band in the source's type keep the pixels' buffer.
                let kept = i < 2 || (i == 2 && d.raster.data_type == DataType::F32);
                assert_eq!(at(&in_place.raster.data) == pixels, kept, "{o:?}");
                assert_eq!(bits(&in_place.raster.data), bits(&copy.raster.data), "{:?} {o:?}", d.raster.data_type);
                assert_eq!(in_place.band_nodata, copy.band_nodata);
                assert_eq!(in_place.geo_transform(), copy.geo_transform());
            }
        }
    }

    /// A float32 raster of uneven values, with -inf nodata pixels at the left corners if `nodata`.
    fn uneven(cols: usize, rows: usize, nodata: bool) -> RasterDataset {
        let cfg = RasterConfig {
            cols,
            rows,
            bands: 1,
            x_min: 100.0,
            y_min: 200.0 - rows as f64 * 10.0,
            cell_size: 10.0,
            cell_size_y: Some(10.0),
            nodata: f64::NEG_INFINITY,
            data_type: DataType::F32,
            crs: wbraster::CrsInfo::default(),
            metadata: vec![],
        };
        let mut data: Vec<f64> = (0..cols * rows).map(|i| (((i * 7919) % 1000) as f64 / 7.3) as f32 as f64).collect();
        if nodata {
            data[0] = f64::NEG_INFINITY;
            data[(rows - 1) * cols] = f64::NEG_INFINITY;
        }
        RasterDataset::new(Raster::from_data(cfg, data).unwrap(), nodata)
    }

    fn bits(d: &RasterDataset, o: &TranslateOptions) -> Vec<u32> {
        translate(d, o).unwrap().raster.data.to_f64_vec().iter().map(|v| (*v as f32).to_bits()).collect()
    }

    #[test]
    fn resamples_with_gdal_arithmetic() {
        // gdal_translate 3.12's outputs (float32 bits) for the same rasters.
        let size = |w, h| Some((Size::Pixels(w), Size::Pixels(h)));
        let average = |w, h| TranslateOptions { out_size: size(w, h), resampling: Some(Resampling::Average), ..Default::default() };
        // -outsize 4 3 -r average, with nodata: the sums over valid pixels.
        let expected = [
            0x427fa334, 0x42d30c40, 0x429a1539, 0x423b1ea6, 0x420ad453, 0x424acdc4, 0x42d01be0, 0x42a1d174, 0x428202b3, 0x421a4cc9,
            0x424c0abf, 0x42c60dc1,
        ];
        assert_eq!(bits(&uneven(11, 7, true), &average(4, 3)), expected);
        // Without nodata: weighted sums scaled by the pixel count.
        let expected = [
            0x4257cc43, 0x42d30c40, 0x429a1539, 0x423b1ea6, 0x420ad453, 0x424acdc4, 0x42d01be0, 0x42a1d174, 0x4289ac4b, 0x421a4cc9,
            0x424c0abf, 0x42c60dc1,
        ];
        assert_eq!(bits(&uneven(11, 7, false), &average(4, 3)), expected);
        // A factor of two: float32 sums of quarters, four pixels at a time, then one by one.
        let expected = [
            0x4281b9dd, 0x42de542a, 0x42b1f1f9, 0x42858fc8, 0x42325b2e, 0x41b32d96, 0x42192694, 0x42edabd6, 0x42c149a5, 0x4294e774,
            0x42510a86, 0x41f08c46, 0x413b65b2, 0x42fd0382, 0x42d0a150, 0x42a43f20, 0x426fb9dc, 0x4216f57b, 0x419b1189, 0x42835eaf,
            0x42dff8fc, 0x42b396cc, 0x4287349b, 0x4235a4d2,
        ];
        assert_eq!(bits(&uneven(12, 8, false), &average(6, 4)), expected);
        // -outsize 5 3 -r bilinear: an infinite nodata pixel in a kernel makes GDAL's result NaN.
        let o = TranslateOptions { out_size: size(5, 3), resampling: Some(Resampling::Bilinear), ..Default::default() };
        let out = translate(&uneven(11, 7, true), &o).unwrap().raster.data.to_f64_vec();
        let expected = [
            0xffc00000u32,
            0x42cebddf,
            0x42bc9c4e,
            0x428aadd7,
            0x423f3b9e,
            0x42156352,
            0x42273fe1,
            0x429f2136,
            0x42c75050,
            0x429f9bce,
            0xffc00000,
            0x42391b8e,
            0x41ff0784,
            0x42988216,
            0x42ccf694,
        ];
        for (v, e) in out.iter().zip(expected) {
            let e = f32::from_bits(e);
            assert!(if e.is_nan() { v.is_nan() } else { *v as f32 == e }, "{v} vs {e}");
        }
    }
}
