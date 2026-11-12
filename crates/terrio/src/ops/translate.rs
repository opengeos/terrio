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
//! windowing of VRT sources, the window being part of its raster rather than a source's
//! rectangles: kernels and areas reach the pixels around it. A window read at its own size is
//! copied pixel for pixel only on whole-pixel offsets, or by nearest neighbour from the pixel
//! holding each output pixel's centre; at a fractional offset it is resampled there by its
//! method, whether the raster is read whole or by window. Values are then optionally rescaled
//! and converted to the output type with GDAL's rounding and clamping.

use super::Ct as Transformer;
use super::overview::OverviewSelection;
use super::vrt::SourceRead;
use super::{Grid, Resampling, convert_value, type_range};
use crate::crs::SpatialRef;
use crate::{Error, LazyRaster, RasterDataset, ResampledReadOptions, Result};
use wbraster::DataType;
use wbraster::raster::RasterData;

/// An input opened by the translate policy, preserving lazy window reads where available.
pub enum OpenedRaster {
    /// A raster read by window.
    Lazy(LazyRaster),
    /// A raster decoded whole after the lazy reader refused it.
    Whole(Box<RasterDataset>),
}

impl OpenedRaster {
    fn translate(self, options: &TranslateOptions, overview: OverviewSelection) -> Result<RasterDataset> {
        match self {
            Self::Lazy(mut src) => translate_lazy(&mut src, options, overview),
            Self::Whole(src) => translate(&src, options),
        }
    }
}

/// Translates a path using lazy window/overview reads, with a whole-read fallback when needed.
/// Uses the shared policy of `terrio translate`. Invalid arguments, HTTP failures,
/// resource refusals and missing local files are terminal. Explicit overview levels
/// need a lazy reader and are refused before a whole-read fallback.
///
/// # Errors
///
/// Returns input-opening errors or errors from [`translate_lazy`] or [`translate`].
pub fn translate_path(path: &str, options: &TranslateOptions, overview: OverviewSelection) -> Result<RasterDataset> {
    open_path(path, overview)?.translate(options, overview)
}

/// [`translate_path`], streaming the destination when [`translate_lazy_to_file`] can do so.
///
/// # Errors
///
/// Returns input-opening, translation or destination-writing errors.
pub fn translate_path_to_file(
    source: &str,
    options: &TranslateOptions,
    overview: OverviewSelection,
    destination: &str,
    write_options: &crate::RasterWriteOptions,
) -> Result<()> {
    match open_path(source, overview)? {
        OpenedRaster::Lazy(mut src) => translate_lazy_to_file(&mut src, options, overview, destination, write_options),
        OpenedRaster::Whole(src) => {
            let mut write_options = write_options.clone();
            if options.assign_ullr.is_none() {
                crate::drivers::common::keep_unreferenced(&mut write_options, destination, src.geotransform_present.unwrap_or(true));
            }
            crate::write_raster_dataset(&translate(&src, options)?, destination, &write_options)
        }
    }
}

/// Translates owned input bytes, sharing them with the lazy reader without a second input copy.
/// A TIFF the lazy reader refuses may be decoded whole. The same terminal-error and explicit
/// overview policy as [`translate_path`] applies before that fallback. Non-TIFF bytes are
/// decoded in memory by the initial opener and do not get a second whole-read attempt:
/// a GDAL-only non-TIFF file may therefore translate by path but fail from bytes.
///
/// # Errors
///
/// Returns input-opening errors or errors from [`translate_lazy`] or [`translate`].
pub fn translate_bytes(bytes: Vec<u8>, name: &str, options: &TranslateOptions, overview: OverviewSelection) -> Result<RasterDataset> {
    open_bytes(bytes, name, overview)?.translate(options, overview)
}

/// Whether a failed lazy open must not be retried as a whole read.
fn terminal_open_error(error: &Error, name: &str) -> bool {
    matches!(error, Error::InvalidArgument(_) | Error::Http(_))
        || matches!(error, Error::Io(e) if crate::error::is_http_io(e))
        || error.is_resource_refusal()
        || matches!(error, Error::Io(e) if e.kind() == std::io::ErrorKind::NotFound && !crate::vfs::is_gdal_name(name))
}

/// Refuses a stored-level selector before allocating a whole raster.
fn require_whole_read(overview: OverviewSelection, name: &str, cause: Option<&Error>) -> Result<()> {
    if matches!(overview, OverviewSelection::Level(_) | OverviewSelection::AutoOffset(1..)) {
        let cause = cause.map(|e| format!(" ({})", crate::vsi::listing::redact_url(&e.to_string()))).unwrap_or_default();
        return Err(Error::Unsupported(format!(
            "overview needs a raster read by window; '{}' is read whole{cause}",
            crate::vsi::listing::redact_url(name)
        )));
    }
    Ok(())
}

fn open_path(path: &str, overview: OverviewSelection) -> Result<OpenedRaster> {
    let cause = if path.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("gdal:")) {
        None
    } else {
        match crate::open_raster_lazy(path) {
            Ok(src) => return Ok(OpenedRaster::Lazy(src)),
            Err(error) if terminal_open_error(&error, path) => return Err(error),
            Err(error) => Some(error),
        }
    };
    require_whole_read(overview, path, cause.as_ref())?;
    crate::open_raster(path).map(|src| OpenedRaster::Whole(Box::new(src)))
}

/// Shared byte-opening policy, also used by the legacy memory entry point.
pub(crate) fn open_bytes(bytes: Vec<u8>, name: &str, overview: OverviewSelection) -> Result<OpenedRaster> {
    if crate::tiles::archive_kind(name).is_some() || !crate::memory::codec::is_tiff(&bytes) {
        return match crate::memory::open_raster_lazy_bytes(bytes, name) {
            Ok(src) => Ok(OpenedRaster::Lazy(src)),
            Err(error) if terminal_open_error(&error, name) => Err(error),
            Err(error) => {
                require_whole_read(overview, name, Some(&error))?;
                Err(error)
            }
        };
    }
    crate::formats::check_allowed_bytes(name, &bytes, Some(crate::formats::Kind::Raster))?;
    let shared = std::sync::Arc::new(bytes);
    let cause = match LazyRaster::from_tiff_shared_bytes(shared.clone()) {
        Ok(Some(src)) => return Ok(OpenedRaster::Lazy(src)),
        Err(error) if terminal_open_error(&error, name) => return Err(error),
        Err(error) => Some(error),
        Ok(None) => None,
    };
    require_whole_read(overview, name, cause.as_ref())?;
    // The failed reader has dropped its reference; take back the original owned input.
    let bytes = std::sync::Arc::unwrap_or_clone(shared);
    crate::memory::open_raster_bytes(bytes, name).map(|src| OpenedRaster::Whole(Box::new(src)))
}

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
    let n = crate::dataset::fallible::len(&[ow, oh], "a translated raster")?;
    let in_place = pixels.is_some();
    let mut out = match pixels {
        Some(pixels) => pixels,
        None => {
            crate::dataset::fallible::check_raster_buffer("a translated raster", ow, oh, bands.len(), out_type)?;
            let samples = crate::dataset::fallible::len(&[n, bands.len()], "a translated raster")?;
            crate::dataset::fallible::raster_data(out_type, samples, 0.0, "a translated raster")?
        }
    };
    let retained = crate::dataset::fallible::len(&[n, bands.len().saturating_sub(1), out_type.size_bytes()], "translated bands")?;
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
            within_rects: false,
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
            read.read_with_retained(ow, oh, band, retained, |x| convert_value(scaled(x), out_type) as _)?;
        });
    }
    let grid = Grid { width: ow, height: oh, geo_transform: gt };
    let srs = o.assign_srs.clone().unwrap_or_else(|| ds.crs());
    let mut res = super::build_raster_data(grid, out, bands.len(), out_nodata.first().copied().flatten(), &srs)?;
    res.complex = ds.complex
        && matches!(out_type, DataType::I16 | DataType::I32 | DataType::F32 | DataType::F64)
        && bands.len().is_multiple_of(2)
        && bands.as_chunks::<2>().0.iter().all(|p| p[0] % 2 == 1 && p[1] == p[0] + 1);
    res.raster.metadata = ds.raster.metadata.iter().filter(|(k, _)| k == "AREA_OR_POINT").cloned().collect();
    res.band_metadata = if res.complex {
        bands.as_chunks::<2>().0.iter().map(|p| ds.band_metadata.get((p[0] - 1) / 2).cloned().unwrap_or_default()).collect()
    } else if ds.complex {
        bands.iter().map(|&b| ds.band_metadata.get((b - 1) / 2).cloned().unwrap_or_default()).collect()
    } else {
        bands.iter().map(|&b| ds.band_metadata.get(b - 1).cloned().unwrap_or_default()).collect()
    };
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
/// that replace one of the source's files are written from the output translated whole. A source
/// without a geotransform of its own (an unreferenced GeoTIFF or JPEG 2000 file) gives an output
/// without one, unless `assign_ullr` gives it one ([`LazyRaster::keep_unreferenced`]).
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
    // A source without a geotransform gives an output without one, unless one is assigned.
    let mut options = options.clone();
    if o.assign_ullr.is_none() {
        src.keep_unreferenced(&mut options, path);
    }
    write_lazy(src, o, overview, path, &options, None)
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
    // own, and, without an output size, explicit level numbers use its resolution.
    // AUTO-n keeps the full-resolution output size, as GDAL 3.13.3 does.
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
            let (ow, oh) = if o.out_size.is_some() || o.resolution.is_some() || matches!(overview, OverviewSelection::AutoOffset(_)) {
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
    // gdal_translate's virtual dataset reads its bands together when they are the source's
    // first ones in order (`VRTDataset::CheckCompatibleForDatasetIO`). A copy reads the same
    // values either way, so its bands are read together: a Zarr chunk holding many bands is then
    // fetched and decoded once, not once per band. A VRT read whole is the exception: reading
    // several bands holds them all as 8-byte values at once, so its bands are read one by one.
    let whole_vrt = src.driver() == crate::formats::RasterDriver::VRT && !src.reads_by_window();
    let together = (copy && !whole_vrt) || bands.iter().enumerate().all(|(i, &b)| b == i + 1);
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

    #[test]
    fn whole_read_refusal_redacts_the_input_and_cause() {
        let name = "https://user:secret@example.com/image.tif?token=private";
        let cause = Error::Unsupported(format!(
            "https://public.example/a redirected to https://other:hidden@private.example/b; cannot read {name}"
        ));
        let error = super::require_whole_read(OverviewSelection::Level(0), name, Some(&cause)).unwrap_err().to_string();
        assert!(error.contains("overview needs a raster read by window"), "{error}");
        assert!(error.contains("example.com/image.tif"), "{error}");
        for credential in ["user", "secret", "token=private", "other:hidden", "hidden@"] {
            assert!(!error.contains(credential), "{error}");
        }
    }

    #[test]
    fn input_policy_keeps_terminal_errors() {
        for error in [
            Error::InvalidArgument("invalid option".into()),
            Error::Http("request refused".into()),
            Error::Io(std::io::Error::other(Error::Http("directory request refused".into()))),
            Error::ResourceLimit("memory budget".into()),
            Error::DecodedLimitExceeded { resource: "TIFF tag", limit: 1 },
            Error::Io(std::io::Error::from(std::io::ErrorKind::OutOfMemory)),
            Error::Io(std::io::Error::from(std::io::ErrorKind::NotFound)),
        ] {
            assert!(terminal_open_error(&error, "missing.tif"));
        }
        assert!(!terminal_open_error(&Error::Unsupported("codec".into()), "input.tif"));
        let not_found = || Error::Io(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert!(terminal_open_error(&not_found(), r"C:\data\missing.tif"));
        assert!(terminal_open_error(&not_found(), "C:/data/missing.tif"));
        assert!(terminal_open_error(&not_found(), "C:missing.tif"));
        assert!(terminal_open_error(&not_found(), r"\\?\C:\data\missing.tif"));
        assert!(!terminal_open_error(&not_found(), "gdal:missing.tif"));
        assert!(!terminal_open_error(&not_found(), "/vsimem/example.grb"));
        assert!(!terminal_open_error(&not_found(), "/vsis3/bucket:key.tif"));
        assert!(!terminal_open_error(&not_found(), "s3://bucket/missing.tif"));
        let missing = tempfile::tempdir().unwrap().path().join("missing.tif");
        let result = translate_path(missing.to_str().unwrap(), &TranslateOptions::default(), OverviewSelection::Level(0));
        assert!(matches!(result, Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::NotFound));
    }

    #[cfg(windows)]
    #[test]
    fn input_policy_keeps_missing_windows_paths_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let verbatim = dir.path().canonicalize().unwrap().join("missing.tif");
        let verbatim = verbatim.to_str().unwrap();
        let plain = verbatim.strip_prefix(r"\\?\").unwrap();
        let drive_relative = format!("{}{}", &plain[..2], &plain[3..]);
        for path in [plain, &plain.replace('\\', "/"), &drive_relative, verbatim] {
            let result = translate_path(path, &TranslateOptions::default(), OverviewSelection::Level(0));
            assert!(matches!(result, Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound), "{path}");
        }
    }

    #[test]
    fn input_policy_refuses_levels_before_whole_reads() {
        let broken = [b"II*\0".as_slice(), &[255; 60]].concat();
        for overview in [OverviewSelection::Level(0), OverviewSelection::AutoOffset(1)] {
            let result = translate_bytes(broken.clone(), "broken.tif", &TranslateOptions::default(), overview);
            assert!(matches!(result, Err(Error::Unsupported(message)) if message.contains("is read whole (")));
            let result = translate_path("gdal:does-not-exist.tif", &TranslateOptions::default(), overview);
            assert!(matches!(result, Err(Error::Unsupported(message)) if message.contains("is read whole")));
        }
        let result = translate_bytes(broken, "broken.tif", &TranslateOptions::default(), OverviewSelection::Auto);
        assert!(result.is_err());
        assert!(!matches!(result, Err(Error::Unsupported(message)) if message.contains("is read whole")));
    }

    #[test]
    fn input_policy_preserves_lazy_path_byte_and_streamed_outputs() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.tif");
        let output = dir.path().join("output.tif");
        let mut source = ds(37, 29);
        source.has_nodata = true;
        if let RasterData::F32(values) = &mut source.raster.data {
            values[37 * 29 + 3 * 37 + 4] = -1.0;
        }
        crate::write_raster_dataset(&source, input.to_str().unwrap(), &crate::RasterWriteOptions::default()).unwrap();
        let options = TranslateOptions { src_win: Some((3.0, 2.0, 11.0, 9.0)), bands: vec![2], ..Default::default() };
        let expected = translate(&source, &options).unwrap();
        assert!(matches!(open_path(input.to_str().unwrap(), OverviewSelection::Auto), Ok(OpenedRaster::Lazy(_))));
        let bytes = std::fs::read(&input).unwrap();
        assert!(matches!(open_bytes(bytes.clone(), "input.tif", OverviewSelection::Auto), Ok(OpenedRaster::Lazy(_))));
        let from_path = translate_path(input.to_str().unwrap(), &options, OverviewSelection::Auto).unwrap();
        let from_bytes = translate_bytes(bytes, "input.tif", &options, OverviewSelection::Auto).unwrap();
        translate_path_to_file(input.to_str().unwrap(), &options, OverviewSelection::Auto, output.to_str().unwrap(), &Default::default())
            .unwrap();
        for actual in [from_path, from_bytes, crate::open_raster(output.to_str().unwrap()).unwrap()] {
            assert_eq!((actual.raster.cols, actual.raster.rows, actual.raster.bands), (11, 9, 1));
            assert_eq!(actual.raster.data.to_f64_vec(), expected.raster.data.to_f64_vec());
            assert_eq!(actual.geo_transform(), expected.geo_transform());
            assert_eq!(actual.crs().epsg, expected.crs().epsg);
            assert_eq!(actual.raster.data_type, expected.raster.data_type);
            assert_eq!(actual.nodata(), expected.nodata());
        }
    }

    #[cfg(feature = "netcdf-hdf5")]
    #[test]
    fn input_policy_keeps_resource_refusals_before_overview_fallback() {
        if crate::usable_memory().is_some_and(|bytes| bytes >= 8 * (1u64 << 40)) {
            eprintln!("skipping resource-refusal read: the known memory limit permits the 8 TiB fixture");
            return; // This host permits the fixture's declared allocation.
        }
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/hdf5_fill_only.h5");
        let options = TranslateOptions::default();
        let overview = OverviewSelection::Level(0);
        let path_result = translate_path(path, &options, overview);
        assert!(matches!(path_result, Err(error) if error.is_resource_refusal()));
        let byte_result = translate_bytes(std::fs::read(path).unwrap(), "input.h5", &options, overview);
        assert!(matches!(byte_result, Err(error) if error.is_resource_refusal()));
    }

    /// GDAL 3.13.3 keeps a window's full-resolution size for AUTO-n without -outsize/-tr.
    #[test]
    fn automatic_overview_offsets_keep_the_window_size() {
        use crate::ops::overview::{OverviewOptions, add_overviews};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.tif");
        let path = path.to_str().unwrap();
        crate::write_raster_dataset(&ds(64, 48), path, &Default::default()).unwrap();
        add_overviews(path, &OverviewOptions { levels: vec![2, 4, 8], ..Default::default() }).unwrap();
        let mut src = crate::open_raster_lazy(path).unwrap();
        let options = TranslateOptions { src_win: Some((4.0, 2.0, 20.0, 12.0)), ..Default::default() };
        for offset in [0, 1, 2, 8] {
            let out = translate_lazy(&mut src, &options, OverviewSelection::AutoOffset(offset)).unwrap();
            assert_eq!((out.raster.cols, out.raster.rows), (20, 12), "AUTO-{offset}");
            assert_eq!(out.geo_transform(), [140.0, 10.0, 0.0, 180.0, 0.0, -10.0]);
        }
        let auto = translate_lazy(&mut src, &options, OverviewSelection::Auto).unwrap();
        let zero = translate_lazy(&mut src, &options, OverviewSelection::AutoOffset(0)).unwrap();
        assert_eq!(auto.raster.data.to_f64_vec(), zero.raster.data.to_f64_vec());
        let explicit = translate_lazy(&mut src, &options, OverviewSelection::Level(0)).unwrap();
        assert_eq!((explicit.raster.cols, explicit.raster.rows), (10, 6));
        assert_eq!(explicit.geo_transform(), [140.0, 20.0, 0.0, 180.0, 0.0, -20.0]);
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
    fn copies_read_their_bands_together_as_band_by_band() {
        // A copy reads all its bands at once (a Zarr chunk holding many bands is then fetched
        // once), where gdal_translate's VRT reads bands that are not the first ones one by one:
        // the same values, nodata and masks, with a per-dataset mask, per-band nodata and alpha.
        let dir = tempfile::tempdir().unwrap();
        let rgb = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/resampled_reads/rgb_mask.tif").to_string();
        let vrt = dir.path().join("nodata_alpha.vrt").to_str().unwrap().to_string();
        let band = |b: usize, source: usize, extra: &str| {
            format!(
                "<VRTRasterBand dataType=\"Byte\" band=\"{b}\">{extra}<SimpleSource><SourceFilename relativeToVRT=\"0\">{rgb}</SourceFilename>\
                 <SourceBand>{source}</SourceBand><SrcRect xOff=\"0\" yOff=\"0\" xSize=\"64\" ySize=\"48\"/>\
                 <DstRect xOff=\"0\" yOff=\"0\" xSize=\"64\" ySize=\"48\"/></SimpleSource></VRTRasterBand>"
            )
        };
        let bands_xml: String =
            [0, 7, 9].iter().enumerate().map(|(i, nd)| band(i + 1, i + 1, &format!("<NoDataValue>{nd}</NoDataValue>"))).collect();
        std::fs::write(
            &vrt,
            format!(
                "<VRTDataset rasterXSize=\"64\" rasterYSize=\"48\"><GeoTransform>500000, 30, 0, 4100000, 0, -30</GeoTransform>{bands_xml}{}</VRTDataset>",
                band(4, 1, "<ColorInterp>Alpha</ColorInterp>")
            ),
        )
        .unwrap();
        for (path, bands) in [
            (&rgb, vec![2, 3]),
            (&rgb, vec![2, 2]),
            (&rgb, vec![3, 2, 3]),
            (&vrt, vec![2, 3, 4]),
            (&vrt, vec![4, 2]),
            (&vrt, vec![2, 2]),
            (&vrt, vec![4, 2, 4]),
        ] {
            let mut src = crate::open_raster_lazy(path).unwrap();
            let read = ResampledReadOptions {
                bands: bands.clone(),
                window: Some((3.0, 5.0, 20.0, 10.0)),
                size: (20, 10),
                resampling: Resampling::Nearest,
                overview: OverviewSelection::Auto,
            };
            let together = src.read_resampled(&read).unwrap();
            let by_band = src.read_resampled_by_band(&read).unwrap();
            assert_eq!(together.raster.data.to_f64_vec(), by_band.raster.data.to_f64_vec(), "{path} {bands:?}");
            assert_eq!(together.band_nodata, by_band.band_nodata, "{path} {bands:?}");
            let (_, masks) = src.read_resampled_with_masks(&read).unwrap();
            let each: Vec<u8> = bands
                .iter()
                .flat_map(|&b| src.read_resampled_with_masks(&ResampledReadOptions { bands: vec![b], ..read.clone() }).unwrap().1)
                .collect();
            assert_eq!(masks, each, "{path} {bands:?}");
            assert!(masks.contains(&0) && masks.contains(&255), "{path} {bands:?}: masked and valid pixels");
            // translate takes that read, and gives what translating the whole source gives.
            let o = TranslateOptions { src_win: Some((3.0, 5.0, 20.0, 10.0)), bands: bands.clone(), ..Default::default() };
            assert!(matches!(plan_lazy(&mut src, &o, OverviewSelection::Auto).unwrap(), Planned::Read(r) if r.together && r.copy));
            let lazy = translate_lazy(&mut src, &o, OverviewSelection::Auto).unwrap();
            let eager = translate(&src.read().unwrap(), &o).unwrap();
            assert_eq!(lazy.raster.data.to_f64_vec(), eager.raster.data.to_f64_vec(), "{path} {bands:?}");
            assert_eq!(lazy.band_nodata, eager.band_nodata, "{path} {bands:?}");
        }
        // A VRT read whole (of resampled sources) reads bands that are not its first ones one by
        // one, as it holds the bands of a read as 8-byte values at once.
        let whole = dir.path().join("lanczos.vrt").to_str().unwrap().to_string();
        let lanczos = |b: usize| {
            format!(
                "<VRTRasterBand dataType=\"Byte\" band=\"{b}\"><ComplexSource resampling=\"lanczos\"><SourceFilename relativeToVRT=\"0\">{rgb}</SourceFilename>\
                 <SourceBand>{b}</SourceBand><SrcRect xOff=\"0\" yOff=\"0\" xSize=\"64\" ySize=\"48\"/>\
                 <DstRect xOff=\"0\" yOff=\"0\" xSize=\"20\" ySize=\"15\"/></ComplexSource></VRTRasterBand>"
            )
        };
        std::fs::write(&whole, format!("<VRTDataset rasterXSize=\"20\" rasterYSize=\"15\">{}{}</VRTDataset>", lanczos(1), lanczos(2)))
            .unwrap();
        let mut src = crate::open_raster_lazy(&whole).unwrap();
        assert!(!src.reads_by_window());
        let o = TranslateOptions { src_win: Some((2.0, 3.0, 10.0, 8.0)), bands: vec![2], ..Default::default() };
        assert!(matches!(plan_lazy(&mut src, &o, OverviewSelection::Auto).unwrap(), Planned::Read(r) if r.copy && !r.together));
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

    /// A raster of 12 x 8 float64 pixels of 1 m from (0, 8), in EPSG:32610, holding
    /// `value(column, row)`.
    fn grid(value: impl Fn(usize, usize) -> f64) -> RasterDataset {
        typed_grid(DataType::F64, value)
    }

    /// [`grid`], of `data_type` pixels.
    fn typed_grid(data_type: DataType, value: impl Fn(usize, usize) -> f64) -> RasterDataset {
        let cfg = RasterConfig {
            cols: 12,
            rows: 8,
            bands: 1,
            x_min: 0.0,
            y_min: 0.0,
            cell_size: 1.0,
            cell_size_y: Some(1.0),
            nodata: -9999.0,
            data_type,
            crs: wbraster::CrsInfo::from_epsg(32610),
            metadata: vec![],
        };
        let data = (0..96).map(|i| value(i % 12, i / 12)).collect();
        RasterDataset::new(Raster::from_data(cfg, data).unwrap(), false)
    }

    /// Whole values from 0 to 999 in no order, a different one in each pixel.
    fn uneven_values(c: usize, r: usize) -> f64 {
        ((c + 12 * r) * 7919 % 1000) as f64
    }

    /// Windows of 6 x 4 pixels from a fractional offset in x, in y and in both, each as a
    /// `srcwin` and as the `projwin` of the same bounds, with their offset in pixels.
    fn fractional_windows() -> Vec<(TranslateOptions, (f64, f64))> {
        let mut windows = Vec::new();
        for (x, y) in [(2.25, 1.0), (2.0, 1.25), (2.25, 1.75)] {
            windows.push((TranslateOptions { src_win: Some((x, y, 6.0, 4.0)), ..Default::default() }, (x, y)));
            windows.push((TranslateOptions { proj_win: Some((x, 8.0 - y, x + 6.0, 4.0 - y)), ..Default::default() }, (x, y)));
        }
        windows
    }

    /// A window at a fractional offset, read at its own size, is resampled at that offset, from
    /// a `srcwin` or a `projwin`, in x, in y and in both. On a ramp of ten per column and a
    /// hundred per row, a bilinear, cubic or cubic spline kernel, an average and a sum give each
    /// output pixel the ramp's value at its centre. On uneven values, each area statistic is that
    /// of the source pixels the output pixel overlaps; on 2 x 2 blocks of equal values, so is
    /// their most frequent value, which is not always the first pixel's. Nearest neighbour takes
    /// the pixel holding each output pixel's centre (a `projwin` read by it starts on a whole
    /// pixel).
    #[test]
    fn fractional_windows_read_at_their_size_are_resampled_at_their_offset() {
        let ramp = grid(|c, r| 10.0 * c as f64 + 100.0 * r as f64);
        let uneven = grid(uneven_values);
        for (window, (x, y)) in fractional_windows() {
            let by = |m: Resampling| TranslateOptions { resampling: Some(m), ..window.clone() };
            for m in [Resampling::Bilinear, Resampling::Cubic, Resampling::CubicSpline, Resampling::Average, Resampling::Sum] {
                let out = translate(&ramp, &by(m)).unwrap();
                assert_eq!(out.geo_transform(), [x, 1.0, 0.0, 8.0 - y, 0.0, -1.0], "{m:?} {window:?}");
                let v = out.raster.data.to_f64_vec();
                for (k, value) in v.iter().enumerate() {
                    let (i, j) = ((k % 6) as f64, (k / 6) as f64);
                    let centre = 10.0 * (x + i) + 100.0 * (y + j);
                    assert!((value - centre).abs() < 1e-9, "{m:?} {window:?}: pixel ({i}, {j}) is {value}, not {centre}");
                }
            }
            for m in [
                Resampling::Average,
                Resampling::Rms,
                Resampling::Mode,
                Resampling::Min,
                Resampling::Max,
                Resampling::Med,
                Resampling::Q1,
                Resampling::Q3,
                Resampling::Sum,
            ] {
                let v = translate(&uneven, &by(m)).unwrap().raster.data.to_f64_vec();
                for (k, value) in v.iter().enumerate() {
                    // The source pixels output pixel (i, j) overlaps, by row, each weighted by its
                    // area inside the output pixel for an average, RMS or sum, else by 1.
                    let (x0, y0) = (x + (k % 6) as f64, y + (k / 6) as f64);
                    let mut overlapped = Vec::new();
                    for r in y0.floor() as usize..(y0 + 1.0).ceil() as usize {
                        for c in x0.floor() as usize..(x0 + 1.0).ceil() as usize {
                            let area =
                                ((x0 + 1.0).min(c as f64 + 1.0) - x0.max(c as f64)) * ((y0 + 1.0).min(r as f64 + 1.0) - y0.max(r as f64));
                            let weighted = matches!(m, Resampling::Average | Resampling::Rms | Resampling::Sum);
                            overlapped.push((uneven_values(c, r), if weighted { area } else { 1.0 }));
                        }
                    }
                    let expected = crate::ops::warp::area_value(m, &mut overlapped, true).unwrap();
                    assert!((value - expected).abs() < 1e-9, "{m:?} {window:?}: pixel {k} is {value}, not {expected}");
                }
            }
        }
        // Each output pixel of a window fractional in x and y overlaps 2 x 2 source pixels, which
        // the blocks give a most frequent value other than the first pixel's in places.
        let block = |c: usize, r: usize| (c / 2 + r / 2) as f64;
        let blocks = grid(block);
        for (window, (x, y)) in fractional_windows().into_iter().filter(|(_, (x, y))| x.fract() != 0.0 && y.fract() != 0.0) {
            let v = translate(&blocks, &TranslateOptions { resampling: Some(Resampling::Mode), ..window.clone() })
                .unwrap()
                .raster
                .data
                .to_f64_vec();
            let (mut expected, mut first) = (Vec::new(), Vec::new());
            for k in 0..24 {
                let (c, r) = ((x + (k % 6) as f64) as usize, (y + (k / 6) as f64) as usize);
                let mut overlapped = [(c, r), (c + 1, r), (c, r + 1), (c + 1, r + 1)].map(|(c, r)| (block(c, r), 1.0));
                expected.push(crate::ops::warp::area_value(Resampling::Mode, &mut overlapped, true).unwrap());
                first.push(block(c, r));
            }
            assert_ne!(expected, first, "{window:?}: a most frequent value other than the first pixel's");
            assert_eq!(v, expected, "Mode {window:?}");
        }
        for (x, y) in [(2.25, 1.0), (2.75, 1.0), (2.0, 1.25), (2.0, 1.75), (2.25, 1.75)] {
            let o = TranslateOptions { src_win: Some((x, y, 6.0, 4.0)), resampling: Some(Resampling::Nearest), ..Default::default() };
            let v = translate(&uneven, &o).unwrap().raster.data.to_f64_vec();
            let holding = |k: usize| uneven_values((x + (k % 6) as f64 + 0.5) as usize, (y + (k / 6) as f64 + 0.5) as usize);
            assert_eq!(v, (0..24).map(holding).collect::<Vec<_>>(), "nearest from ({x}, {y})");
        }
    }

    /// A window at a fractional offset translates to the values the raster in memory gives from
    /// a GeoTIFF read by window, and from a file of a format read whole (an Esri ASCII grid,
    /// whose values are float32), by each method a GeoTIFF's window is resampled by, from a
    /// `srcwin` and a `projwin`, in x, in y and in both, and at a fractional size. A read whole
    /// gives them exactly; a read by window to within 1e-14 of each value, as it sums a kernel's
    /// or an area's terms in another order.
    #[test]
    fn fractional_windows_translate_alike_from_memory_from_a_whole_read_and_by_window() {
        let dir = tempfile::tempdir().unwrap();
        let (memory, memory_f32) = (grid(uneven_values), typed_grid(DataType::F32, uneven_values));
        let (tif, asc) = (dir.path().join("uneven.tif"), dir.path().join("uneven.asc"));
        let (tif, asc) = (tif.to_str().unwrap(), asc.to_str().unwrap());
        crate::write_raster_dataset(&memory, tif, &crate::RasterWriteOptions::default()).unwrap();
        crate::write_raster_dataset(&memory_f32, asc, &crate::RasterWriteOptions::default()).unwrap();
        let mut windows: Vec<TranslateOptions> = fractional_windows().into_iter().map(|(w, _)| w).collect();
        // 6.5 x 4 pixels into 7 x 4, 6.5 x 3.5 pixels from a fractional offset into 7 x 4, and
        // 7.4 x 3.4 into 8 x 4.
        windows.push(TranslateOptions { src_win: Some((2.0, 1.0, 6.5, 4.0)), ..Default::default() });
        windows.push(TranslateOptions { src_win: Some((2.6, 1.6, 6.5, 3.5)), ..Default::default() });
        windows.push(TranslateOptions { src_win: Some((2.0, 1.25, 7.4, 3.4)), ..Default::default() });
        for window in &windows {
            for m in [
                Resampling::Nearest,
                Resampling::Bilinear,
                Resampling::Cubic,
                Resampling::CubicSpline,
                Resampling::Lanczos,
                Resampling::Average,
                Resampling::Rms,
                Resampling::Mode,
            ] {
                let o = TranslateOptions { resampling: Some(m), ..window.clone() };
                let mut lazy = crate::open_raster_lazy(tif).unwrap();
                assert!(matches!(plan_lazy(&mut lazy, &o, OverviewSelection::Auto).unwrap(), Planned::Read(_)), "{m:?} {window:?}");
                let read_whole = match open_path(asc, OverviewSelection::Auto).unwrap() {
                    OpenedRaster::Whole(_) => true,
                    OpenedRaster::Lazy(mut s) => matches!(plan_lazy(&mut s, &o, OverviewSelection::Auto).unwrap(), Planned::Translated(_)),
                };
                assert!(read_whole, "{m:?} {window:?}");
                let (expected, by_window) = (translate(&memory, &o).unwrap(), translate_path(tif, &o, OverviewSelection::Auto).unwrap());
                assert_eq!(by_window.geo_transform(), expected.geo_transform(), "{m:?} {window:?}");
                for (k, (a, b)) in by_window.raster.data.to_f64_vec().iter().zip(expected.raster.data.to_f64_vec()).enumerate() {
                    assert!((a - b).abs() <= 1e-14 * b.abs().max(1.0), "{m:?} {window:?}: pixel {k} is {a} by window, {b} in memory");
                }
                let (expected, whole) = (translate(&memory_f32, &o).unwrap(), translate_path(asc, &o, OverviewSelection::Auto).unwrap());
                assert_eq!(whole.geo_transform(), expected.geo_transform(), "{m:?} {window:?}");
                assert_eq!(whole.raster.data_type, DataType::F32, "{m:?} {window:?}");
                assert_eq!(whole.raster.data.to_f64_vec(), expected.raster.data.to_f64_vec(), "{m:?} {window:?}");
            }
        }
    }

    /// A window at whole offsets, read at its own size, is the source's pixels copied, by every
    /// method, from a `srcwin` and from a `projwin`.
    #[test]
    fn whole_windows_read_at_their_size_are_copied_by_every_method() {
        let src = grid(uneven_values);
        let windows = [
            TranslateOptions { src_win: Some((2.0, 1.0, 6.0, 4.0)), ..Default::default() },
            TranslateOptions { proj_win: Some((2.0, 7.0, 8.0, 3.0)), ..Default::default() },
        ];
        let copied: Vec<f64> = (0..24).map(|k| uneven_values(2 + k % 6, 1 + k / 6)).collect();
        for window in &windows {
            for m in [
                Resampling::Nearest,
                Resampling::Bilinear,
                Resampling::Cubic,
                Resampling::CubicSpline,
                Resampling::Lanczos,
                Resampling::Average,
                Resampling::Rms,
                Resampling::Mode,
                Resampling::Min,
                Resampling::Max,
                Resampling::Med,
                Resampling::Q1,
                Resampling::Q3,
                Resampling::Sum,
            ] {
                let out = translate(&src, &TranslateOptions { resampling: Some(m), ..window.clone() }).unwrap();
                assert_eq!(out.geo_transform(), [2.0, 1.0, 0.0, 7.0, 0.0, -1.0], "{m:?} {window:?}");
                assert_eq!(out.raster.data.to_f64_vec(), copied, "{m:?} {window:?}");
            }
        }
    }
}
