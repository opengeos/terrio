// SPDX-FileCopyrightText: 2015 Nathaniel J. Smith, Stefan van der Walt and Eric Firing (viridis, magma, inferno and plasma)
// SPDX-FileCopyrightText: 2002 Cynthia Brewer, Mark Harrower and The Pennsylvania State University (ColorBrewer RdBu and RdYlGn)
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT AND Apache-2.0 AND CC0-1.0
//
// Colormap stops sampled from:
//   matplotlib 3.10: lib/matplotlib/_cm_listed.py (viridis, magma, inferno, plasma; CC0-1.0)
//   ColorBrewer 2.0: the 11-class RdBu and RdYlGn schemes (Apache-2.0)
// The rest of this file is original Terrio code.

//! Pictures of data: a raster or vector dataset as an RGBA image, for a quick look.
//!
//! Rasters are read lazily: from the coarsest overview that still covers the output size, and with
//! no overview fit to read whole, one sampled row at a time, so a preview of a large remote file
//! transfers a small part of it. Pixels are drawn in the raster's own grid, without reprojecting
//! or applying a rotation in its geotransform. Vector layers are burned with
//! [`rasterize`], and are drawn with north up.
//!
//! The colormap stops are sampled from matplotlib's viridis, magma, inferno and plasma (CC0) and
//! ColorBrewer's RdBu and RdYlGn (Apache-2.0). See `NOTICE`.

use crate::ops::rasterize::{RasterizeOptions, rasterize};
use crate::{DataType, Error, FloatReadOptions, LazyRaster, Result, Window};
use wbvector::{Geometry, Layer};

/// The longest side of an image when no size is asked for, by default.
pub const DEFAULT_MAX_SIZE: usize = 1024;
/// The longest side an image may have.
pub const MAX_IMAGE_SIZE: usize = 16384;
/// The most pixels of a level read at once, per band: 32 MiB of `f64` values a band. Larger
/// levels are read one sampled row at a time instead.
const MAX_DIRECT_PIXELS: usize = 1 << 22;
/// The most pixels an image may have: 4096 x 4096. The values behind it take 8 bytes a band.
pub const MAX_IMAGE_PIXELS: usize = 1 << 24;
/// The longest side of the sample a [`Renderer`] measures its stretch on.
const STYLE_SAMPLE_SIZE: usize = 512;
/// The longest side of a vector dataset's virtual grid in a [`Renderer`].
const VECTOR_GRID_SIZE: usize = 4096;
/// The color of vector features drawn without an attribute.
const FEATURE_COLOR: [u8; 3] = [0x2b, 0x7b, 0xba];

/// How values map to the 0 to 1 range colors are chosen from.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Stretch {
    /// Byte bands drawn as red, green and blue as stored; otherwise the 2nd to 98th percentile.
    #[default]
    Auto,
    /// The smallest to the largest valid value.
    MinMax,
    /// Between two percentiles of the valid values, from 0 to 100.
    Percentile(f64, f64),
    /// Between two values.
    Range(f64, f64),
}

impl Stretch {
    /// Parses `auto`, `minmax`, `p2,p98` (percentiles) or `0,3000` (values).
    ///
    /// # Errors
    ///
    /// When the text is none of these, or the low end is not below the high end.
    pub fn parse(text: &str) -> Result<Self> {
        let bad = || Error::InvalidArgument(format!("stretch '{text}': expected auto, minmax, pLOW,pHIGH or LOW,HIGH"));
        let stretch = match text.trim().to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "minmax" => Self::MinMax,
            other => {
                let (low, high) = other.split_once(',').ok_or_else(bad)?;
                let (low, high) = (low.trim(), high.trim());
                match (low.strip_prefix('p'), high.strip_prefix('p')) {
                    (Some(l), Some(h)) => Self::Percentile(l.parse().map_err(|_| bad())?, h.parse().map_err(|_| bad())?),
                    (None, None) => Self::Range(low.parse().map_err(|_| bad())?, high.parse().map_err(|_| bad())?),
                    _ => return Err(bad()),
                }
            }
        };
        stretch.check()?;
        Ok(stretch)
    }

    /// Refuses a range whose low end is not below its high end, or percentiles outside 0 to 100,
    /// which [`Self::parse`] refuses too.
    ///
    /// # Errors
    ///
    /// When the stretch is one of these.
    pub fn check(self) -> Result<()> {
        let fine = match self {
            Self::Auto | Self::MinMax => true,
            Self::Percentile(l, h) => (0.0..=100.0).contains(&l) && (0.0..=100.0).contains(&h) && l < h,
            Self::Range(l, h) => l.is_finite() && h.is_finite() && l < h,
        };
        if fine {
            Ok(())
        } else {
            Err(Error::InvalidArgument("the stretch's low end must be below its high end, percentiles within 0 to 100".into()))
        }
    }
}

/// The colormaps by name; `_r` after a name reverses it.
pub const COLORMAPS: [&str; 8] = ["gray", "viridis", "magma", "inferno", "plasma", "terrain", "rdbu", "rdylgn"];

/// A colormap: evenly spaced colors, interpolated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Colormap {
    stops: &'static [u32],
    reversed: bool,
}

impl Colormap {
    /// A colormap by name, in any case, such as `viridis` or `rdbu_r`. See [`COLORMAPS`].
    ///
    /// # Errors
    ///
    /// When there is no colormap of that name.
    pub fn parse(name: &str) -> Result<Self> {
        let lower = name.trim().to_ascii_lowercase();
        let (base, reversed) = lower.strip_suffix("_r").map_or((lower.as_str(), false), |b| (b, true));
        let stops: &'static [u32] = match base {
            "gray" | "grey" => &[0x000000, 0xffffff],
            "viridis" => &[0x440154, 0x472d7b, 0x3b528b, 0x2c728e, 0x21918c, 0x28ae80, 0x5ec962, 0xaddc30, 0xfde725],
            "magma" => &[0x000004, 0x180f3d, 0x440f76, 0x721f81, 0x9e2f7f, 0xcd4071, 0xf1605d, 0xfd9668, 0xfeca8d, 0xfcfdbf],
            "inferno" => &[0x000004, 0x1b0c41, 0x4a0c6b, 0x781c6d, 0xa52c60, 0xcf4446, 0xed6925, 0xfb9b06, 0xf7d13d, 0xfcffa4],
            "plasma" => &[0x0d0887, 0x47039f, 0x7301a8, 0x9c179e, 0xbd3786, 0xd8576b, 0xed7953, 0xfb9f3a, 0xfdca26, 0xf0f921],
            "terrain" => &[0x1a663f, 0x5a9e4b, 0xc2c67a, 0xb08a5a, 0x8a6a50, 0xf5f3ef],
            "rdbu" => &[0x67001f, 0xb2182b, 0xd6604d, 0xf4a582, 0xfddbc7, 0xf7f7f7, 0xd1e5f0, 0x92c5de, 0x4393c3, 0x2166ac, 0x053061],
            "rdylgn" => &[0xa50026, 0xd73027, 0xf46d43, 0xfdae61, 0xfee08b, 0xffffbf, 0xd9ef8b, 0xa6d96a, 0x66bd63, 0x1a9850, 0x006837],
            _ => {
                return Err(Error::InvalidArgument(format!(
                    "no colormap '{name}': expected one of {}, optionally with _r",
                    COLORMAPS.join(", ")
                )));
            }
        };
        Ok(Self { stops, reversed })
    }

    /// The color at `t`, from 0 to 1.
    pub fn color(&self, t: f64) -> [u8; 3] {
        let t = if self.reversed { 1.0 - t } else { t }.clamp(0.0, 1.0);
        let at = t * (self.stops.len() - 1) as f64;
        let i = (at as usize).min(self.stops.len() - 2);
        let (a, b, f) = (rgb(self.stops[i]), rgb(self.stops[i + 1]), at - i as f64);
        std::array::from_fn(|c| (f64::from(a[c]) + (f64::from(b[c]) - f64::from(a[c])) * f).round() as u8)
    }

    fn table(&self) -> Vec<[u8; 3]> {
        (0..256).map(|i| self.color(f64::from(i) / 255.0)).collect()
    }
}

fn rgb(value: u32) -> [u8; 3] {
    [(value >> 16) as u8, (value >> 8) as u8, value as u8]
}

/// What to draw and how.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderOptions {
    /// The width in pixels. With only one of width and height, the other keeps the aspect ratio.
    pub width: Option<usize>,
    /// The height in pixels.
    pub height: Option<usize>,
    /// The longest side when neither is given. Images are not enlarged to reach it.
    pub max_size: usize,
    /// One band, or three as red, green and blue, numbered from 1. Empty picks them: the bands
    /// whose color interpretation is red, green and blue; else the first three of Byte rasters;
    /// else the first.
    pub bands: Vec<usize>,
    /// The colormap of a single band. Without one, a raster's own palette, else gray.
    pub colormap: Option<Colormap>,
    /// How values map to colors. Ignored for a raster's own palette.
    pub stretch: Stretch,
    /// Vector data: color features by this numeric field rather than in one color.
    pub attribute: Option<String>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            width: None,
            height: None,
            max_size: DEFAULT_MAX_SIZE,
            bands: Vec::new(),
            colormap: None,
            stretch: Stretch::Auto,
            attribute: None,
        }
    }
}

/// An image: RGBA, 8 bits per channel, row after row from the top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// The width in pixels.
    pub width: usize,
    /// The height in pixels.
    pub height: usize,
    /// The pixels, 4 bytes each.
    pub rgba: Vec<u8>,
}

impl Image {
    /// The image as a PNG file's bytes.
    ///
    /// # Errors
    ///
    /// When encoding fails.
    pub fn to_png(&self) -> Result<Vec<u8>> {
        let bad = |e: png::EncodingError| Error::InvalidData(format!("cannot encode the PNG: {e}"));
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, self.width as u32, self.height as u32);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(bad)?;
        writer.write_image_data(&self.rgba).map_err(bad)?;
        writer.finish().map_err(bad)?;
        Ok(out)
    }
}

/// The output size for a source of `source` pixels.
fn output_size(source: (usize, usize), options: &RenderOptions) -> Result<(usize, usize)> {
    let (sw, sh) = (source.0.max(1) as f64, source.1.max(1) as f64);
    let scaled = |value: f64| (value.round() as usize).max(1);
    let size = match (options.width, options.height) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => (w, scaled(w as f64 * sh / sw)),
        (None, Some(h)) => (scaled(h as f64 * sw / sh), h),
        (None, None) => {
            if options.max_size == 0 {
                return Err(Error::InvalidArgument("the image size must be at least 1".into()));
            }
            let scale = (options.max_size as f64 / sw.max(sh)).min(1.0);
            (scaled(sw * scale), scaled(sh * scale))
        }
    };
    check_size(size.0, size.1)?;
    Ok(size)
}

/// Refuses an image with a side of 0 or over [`MAX_IMAGE_SIZE`], or more than [`MAX_IMAGE_PIXELS`].
fn check_size(width: usize, height: usize) -> Result<()> {
    if width == 0 || height == 0 || width > MAX_IMAGE_SIZE || height > MAX_IMAGE_SIZE || width * height > MAX_IMAGE_PIXELS {
        return Err(Error::InvalidArgument(format!(
            "the image must be 1 to {MAX_IMAGE_SIZE} pixels a side and at most {MAX_IMAGE_PIXELS} pixels in all, not {width}x{height}"
        )));
    }
    Ok(())
}

/// How a raster's values become colors.
#[derive(Debug, Clone, PartialEq)]
enum Mode {
    /// Three bands.
    Rgb,
    /// One band through a table of 256 colors.
    Mapped(Vec<[u8; 3]>),
    /// One band indexing a table.
    Palette(Vec<[u8; 4]>),
    /// Valid pixels in one color.
    Solid([u8; 3]),
}

/// What a raster's bands are drawn with, before any values are looked at.
#[derive(Debug, Clone, PartialEq)]
struct Plan {
    /// The bands read: the color bands, then any alpha band.
    bands: Vec<usize>,
    /// How many of them are color bands.
    colors: usize,
    /// The value of an opaque alpha.
    alpha_max: f64,
    mode: Mode,
}

fn plan(source: &LazyRaster, options: &RenderOptions) -> Result<Plan> {
    options.stretch.check()?;
    let count = source.band_count();
    if count == 0 {
        return Err(Error::InvalidArgument("the raster has no bands".into()));
    }
    let metadata = source.band_metadata();
    let band_with = |name: &str| -> Option<usize> {
        metadata.iter().position(|m| m.color_interp.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(name))).map(|i| i + 1)
    };
    let byte = source.data_type() == DataType::U8;
    // A GeoTIFF's palette is in its header, not in `LazyRaster::color_table`.
    let palette = if source.color_table().is_empty() {
        crate::ops::palette::color_table(&source.header()).unwrap_or_default()
    } else {
        source.color_table().to_vec()
    };
    let mut bands = options.bands.clone();
    if let Some(b) = bands.iter().find(|b| **b == 0 || **b > count) {
        return Err(Error::InvalidArgument(format!("no band {b}: the raster has {count} band(s)")));
    }
    if bands.is_empty() {
        bands = match (band_with("Red"), band_with("Green"), band_with("Blue")) {
            (Some(r), Some(g), Some(b)) if options.colormap.is_none() => vec![r, g, b],
            _ if byte && count >= 3 && options.colormap.is_none() && palette.is_empty() => vec![1, 2, 3],
            _ => vec![1],
        };
    }
    let mode = match bands.len() {
        3 if options.colormap.is_some() => return Err(Error::InvalidArgument("a colormap draws one band, not three".into())),
        3 => Mode::Rgb,
        1 => match options.colormap {
            Some(colormap) => Mode::Mapped(colormap.table()),
            None if !palette.is_empty() && bands[0] == 1 => Mode::Palette(palette),
            None => Mode::Mapped(Colormap::parse("gray")?.table()),
        },
        n => return Err(Error::InvalidArgument(format!("draw one band or three, not {n}"))),
    };
    let colors = bands.len();
    if let Some(alpha) = band_with("Alpha").filter(|a| !bands.contains(a)) {
        bands.push(alpha);
    }
    let alpha_max = if source.data_type() == DataType::U16 { 65535.0 } else { 255.0 };
    Ok(Plan { bands, colors, alpha_max, mode })
}

/// The coarsest overview that still has `tw` by `th` pixels for `window`.
fn choose_overview(source: &LazyRaster, window: Window, tw: usize, th: usize) -> Option<usize> {
    let (full_w, full_h) = (source.width() as f64, source.height() as f64);
    let mut best = None;
    let mut best_pixels = f64::INFINITY;
    for (i, (w, h)) in source.overviews().into_iter().enumerate() {
        let (lw, lh) = (window.width as f64 * w as f64 / full_w, window.height as f64 * h as f64 / full_h);
        if lw >= tw as f64 && lh >= th as f64 && lw * lh < best_pixels {
            best = Some(i);
            best_pixels = lw * lh;
        }
    }
    best
}

/// The values of `bands` for `window` at `tw` by `th` pixels, band after band, NaN where invalid.
///
/// Shrinking averages the valid values read, but for palettes, which take the nearest pixel. With
/// an alpha band, read after the color bands, colors are averaged weighted by alpha, so that the
/// color under a transparent or faint pixel does not bleed into its neighbours.
fn read_region(source: &mut LazyRaster, plan: &Plan, window: Window, tw: usize, th: usize) -> Result<Vec<f64>> {
    let nearest = matches!(plan.mode, Mode::Palette(_));
    let overview = choose_overview(source, window, tw, th);
    let (lw, lh) = match overview {
        Some(i) => source.overviews()[i],
        None => (source.width(), source.height()),
    };
    let (full_w, full_h) = (source.width(), source.height());
    let scale_low = |v: usize, level: usize, full: usize| (v as u128 * level as u128 / full.max(1) as u128) as usize;
    let scale_high = |v: usize, level: usize, full: usize| (v as u128 * level as u128).div_ceil(full.max(1) as u128) as usize;
    let x0 = scale_low(window.x, lw, full_w).min(lw - 1);
    let y0 = scale_low(window.y, lh, full_h).min(lh - 1);
    let x1 = scale_high(window.x + window.width, lw, full_w).clamp(x0 + 1, lw);
    let y1 = scale_high(window.y + window.height, lh, full_h).clamp(y0 + 1, lh);
    let (sw, sh) = (x1 - x0, y1 - y0);
    let count = plan.bands.len();
    let options = FloatReadOptions { bands: plan.bands.clone(), overview, unscale: false };
    let centre = |i: usize, from: usize, to: usize| ((i as u128 * 2 + 1) * from as u128 / (to as u128 * 2)) as usize;
    let mut out = vec![f64::NAN; count * tw * th];
    if sw.saturating_mul(sh) > MAX_DIRECT_PIXELS {
        // Too large to hold: the rows the output's rows land on, and their nearest columns.
        if sw > MAX_DIRECT_PIXELS {
            return Err(Error::Unsupported(format!("cannot draw a raster {sw} pixels wide without overviews; build overviews first")));
        }
        for row in 0..th {
            let values = source.read_window_f64(Window::new(x0, y0 + centre(row, sh, th), sw, 1), &options)?;
            for b in 0..count {
                for col in 0..tw {
                    out[(b * th + row) * tw + col] = values[b * sw + centre(col, sw, tw)];
                }
            }
        }
        mask_by_alpha(&mut out, plan, tw * th);
        return Ok(out);
    }
    let mut values = source.read_window_f64(Window::new(x0, y0, sw, sh), &options)?;
    mask_by_alpha(&mut values, plan, sw * sh);
    let alpha = (count > plan.colors).then(|| &values[plan.colors * sw * sh..(plan.colors + 1) * sw * sh]);
    for b in 0..count {
        let band = &values[b * sw * sh..(b + 1) * sw * sh];
        // Color bands are weighted by alpha; the alpha band and alpha-less rasters evenly.
        let weights = alpha.filter(|_| b < plan.colors);
        let target = &mut out[b * tw * th..(b + 1) * tw * th];
        for row in 0..th {
            for col in 0..tw {
                target[row * tw + col] = if nearest || (sw <= tw && sh <= th) {
                    band[centre(row, sh, th) * sw + centre(col, sw, tw)]
                } else {
                    let (r0, r1) = (row * sh / th, ((row + 1) * sh).div_ceil(th).min(sh));
                    let (c0, c1) = (col * sw / tw, ((col + 1) * sw).div_ceil(tw).min(sw));
                    let (mut sum, mut total) = (0.0, 0.0);
                    for r in r0..r1 {
                        for c in c0..c1 {
                            let v = band[r * sw + c];
                            let weight = weights.map_or(1.0, |a| if a[r * sw + c].is_nan() { 0.0 } else { a[r * sw + c] });
                            if !v.is_nan() && weight > 0.0 {
                                sum += v * weight;
                                total += weight;
                            }
                        }
                    }
                    if total == 0.0 { f64::NAN } else { sum / total }
                };
            }
        }
    }
    Ok(out)
}

/// Sets color bands to NaN where the alpha band read after them is 0.
fn mask_by_alpha(values: &mut [f64], plan: &Plan, pixels: usize) {
    if plan.bands.len() > plan.colors {
        let (colors, alpha) = values.split_at_mut(plan.colors * pixels);
        for (p, a) in alpha.iter().enumerate() {
            if *a == 0.0 {
                for b in 0..plan.colors {
                    colors[b * pixels + p] = f64::NAN;
                }
            }
        }
    }
}

/// The range each of the `colors` bands is stretched over, measured on `values`.
fn stretch_ranges(values: &[f64], colors: usize, pixels: usize, stretch: Stretch, byte_as_stored: bool) -> Vec<(f64, f64)> {
    (0..colors)
        .map(|b| match stretch {
            Stretch::Range(low, high) => (low, high),
            Stretch::Auto if byte_as_stored => (0.0, 255.0),
            Stretch::Auto => percentiles(&values[b * pixels..(b + 1) * pixels], 2.0, 98.0),
            Stretch::MinMax => percentiles(&values[b * pixels..(b + 1) * pixels], 0.0, 100.0),
            Stretch::Percentile(low, high) => percentiles(&values[b * pixels..(b + 1) * pixels], low, high),
        })
        .collect()
}

fn percentiles(band: &[f64], low: f64, high: f64) -> (f64, f64) {
    let mut valid: Vec<f64> = band.iter().copied().filter(|v| v.is_finite()).collect();
    if valid.is_empty() {
        return (0.0, 1.0);
    }
    valid.sort_unstable_by(f64::total_cmp);
    let at = |p: f64| valid[((valid.len() - 1) as f64 * p / 100.0).round() as usize];
    (at(low), at(high))
}

/// The position of `value` in `range`, from 0 to 1. A range of one value is drawn at its middle.
fn position(value: f64, (low, high): (f64, f64)) -> f64 {
    if high > low { ((value - low) / (high - low)).clamp(0.0, 1.0) } else { 0.5 }
}

fn shade(values: &[f64], plan: &Plan, ranges: &[(f64, f64)], width: usize, height: usize) -> Image {
    let pixels = width * height;
    let mut rgba = vec![0u8; pixels * 4];
    let eight_bits = |t: f64| (t * 255.0).round() as u8;
    for (p, out) in rgba.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let value = |band: usize| values[band * pixels + p];
        let color = match &plan.mode {
            Mode::Rgb => {
                let v = [value(0), value(1), value(2)];
                (!v.iter().any(|c| c.is_nan())).then(|| {
                    let c = |i: usize| eight_bits(position(v[i], ranges[i]));
                    [c(0), c(1), c(2), 255]
                })
            }
            Mode::Mapped(table) => {
                let v = value(0);
                (!v.is_nan()).then(|| {
                    let c = table[usize::from(eight_bits(position(v, ranges[0])))];
                    [c[0], c[1], c[2], 255]
                })
            }
            Mode::Palette(table) => {
                let v = value(0);
                (v >= 0.0).then(|| table.get(v as usize).copied()).flatten()
            }
            Mode::Solid(c) => (!value(0).is_nan()).then_some([c[0], c[1], c[2], 255]),
        };
        if let Some(mut color) = color {
            if plan.bands.len() > plan.colors {
                let alpha = value(plan.colors);
                let scaled = if alpha.is_nan() { 0.0 } else { (alpha / plan.alpha_max).clamp(0.0, 1.0) };
                color[3] = (f64::from(color[3]) * scaled).round() as u8;
            }
            *out = color;
        }
    }
    Image { width, height, rgba }
}

/// Draws a raster, reading as little of it as the size allows.
///
/// # Errors
///
/// When the options do not fit the raster, its bands are complex, or a read fails.
pub fn render_raster(source: &mut LazyRaster, options: &RenderOptions) -> Result<Image> {
    let plan = plan(source, options)?;
    let (width, height) = output_size((source.width(), source.height()), options)?;
    let window = Window::full(source.width(), source.height());
    let values = read_region(source, &plan, window, width, height)?;
    let byte_as_stored = source.data_type() == DataType::U8 && plan.mode == Mode::Rgb;
    let ranges = stretch_ranges(&values, plan.colors, width * height, options.stretch, byte_as_stored);
    Ok(shade(&values, &plan, &ranges, width, height))
}

/// The layers' common extent `(min_x, min_y, max_x, max_y)`, widened if it has no width or height.
fn vector_extent(layers: &[Layer]) -> Result<[f64; 4]> {
    let crs_key = |l: &Layer| l.crs.as_ref().map(|c| (c.epsg, c.wkt.clone()));
    if layers.windows(2).any(|pair| crs_key(&pair[0]) != crs_key(&pair[1])) {
        return Err(Error::InvalidArgument("the layers have different CRSs: choose one layer".into()));
    }
    let mut extent = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for feature in layers.iter().flat_map(|l| &l.features) {
        if let Some(b) = feature.geometry.as_ref().and_then(Geometry::bbox) {
            extent = [extent[0].min(b.min_x), extent[1].min(b.min_y), extent[2].max(b.max_x), extent[3].max(b.max_y)];
        }
    }
    if !extent.iter().all(|v| v.is_finite()) {
        return Err(Error::InvalidArgument("the layers have no geometries to draw".into()));
    }
    let pad = |low: &mut f64, high: &mut f64| {
        if *high <= *low {
            let margin = if *low == 0.0 { 1.0 } else { low.abs() * 1e-3 };
            *low -= margin;
            *high += margin;
        }
    };
    let [mut x0, mut y0, mut x1, mut y1] = extent;
    pad(&mut x0, &mut x1);
    pad(&mut y0, &mut y1);
    // Rasterizing leaves out what lies exactly on the grid's far edges, so keep features off them.
    let margin = (x1 - x0).max(y1 - y0) / VECTOR_GRID_SIZE as f64;
    Ok([x0 - margin, y0 - margin, x1 + margin, y1 + margin])
}

/// Vector features, split into points (drawn three pixels wide, since one pixel is hard to see)
/// and other shapes, with each feature's bounding box `[min_x, min_y, max_x, max_y]`, so that a
/// region rasterizes only the features that touch it.
struct Features {
    shapes: Vec<Layer>,
    points: Vec<Layer>,
    shape_boxes: Vec<Vec<Option<[f64; 4]>>>,
    point_boxes: Vec<Vec<Option<[f64; 4]>>>,
}

impl Features {
    fn split(layers: Vec<Layer>) -> Self {
        let (mut shapes, mut points) = (Vec::new(), Vec::new());
        for mut layer in layers {
            let features = std::mem::take(&mut layer.features);
            let (mut shape_layer, mut point_layer) = (layer.clone(), layer);
            for feature in features {
                let is_point = matches!(feature.geometry, Some(Geometry::Point(_) | Geometry::MultiPoint(_)));
                if is_point { point_layer.features.push(feature) } else { shape_layer.features.push(feature) }
            }
            shapes.push(shape_layer);
            points.push(point_layer);
        }
        let boxes = |layers: &[Layer]| -> Vec<Vec<Option<[f64; 4]>>> {
            layers
                .iter()
                .map(|l| {
                    l.features
                        .iter()
                        .map(|f| f.geometry.as_ref().and_then(Geometry::bbox).map(|b| [b.min_x, b.min_y, b.max_x, b.max_y]))
                        .collect()
                })
                .collect()
        };
        let (shape_boxes, point_boxes) = (boxes(&shapes), boxes(&points));
        Self { shapes, points, shape_boxes, point_boxes }
    }
}

/// The features of `layers` whose box touches `extent`, borrowed when that is all of them.
fn touching<'a>(layers: &'a [Layer], boxes: &[Vec<Option<[f64; 4]>>], extent: [f64; 4]) -> std::borrow::Cow<'a, [Layer]> {
    let touches =
        |b: &Option<[f64; 4]>| b.is_some_and(|b| b[0] <= extent[2] && b[2] >= extent[0] && b[1] <= extent[3] && b[3] >= extent[1]);
    if boxes.iter().flatten().all(touches) {
        return std::borrow::Cow::Borrowed(layers);
    }
    let subset = layers
        .iter()
        .zip(boxes)
        .map(|(layer, boxes)| {
            let features = layer.features.iter().zip(boxes).filter(|(_, b)| touches(b)).map(|(f, _)| f.clone()).collect();
            Layer { features, ..empty_like(layer) }
        })
        .collect();
    std::borrow::Cow::Owned(subset)
}

/// `layer`'s name, schema and CRS, without its features.
fn empty_like(layer: &Layer) -> Layer {
    let mut empty = Layer::new(layer.name.clone());
    empty.geom_type = layer.geom_type;
    empty.crs = layer.crs.clone();
    empty.schema = layer.schema.clone();
    empty
}

/// Burns `layers` over `extent`: the attribute's values, or 1 where a feature is; NaN elsewhere,
/// and everywhere when there is no feature to burn.
fn burn(layers: &[Layer], attribute: Option<&str>, extent: [f64; 4], width: usize, height: usize) -> Result<Vec<f64>> {
    if layers.iter().all(|l| l.features.is_empty()) {
        return Ok(vec![f64::NAN; width * height]);
    }
    let options = RasterizeOptions {
        burn_values: if attribute.is_some() { Vec::new() } else { vec![1.0] },
        attribute: attribute.map(str::to_string),
        size: Some((width, height)),
        extent: Some((extent[0], extent[1], extent[2], extent[3])),
        data_type: Some(DataType::F64),
        init: vec![f64::NAN],
        nodata: Some(f64::NAN),
        all_touched: true,
        ..Default::default()
    };
    let raster = rasterize(layers, &options)?;
    raster.read_window_f64(Window::full(width, height), &FloatReadOptions::default())
}

/// Draws `features` over `extent` at `width` by `height` pixels: the shapes, then the points on
/// top, three pixels wide. Points are burned one pixel beyond each side, so that a point just
/// outside the extent still widens into it, as at the seams between map tiles.
fn draw_features(features: &Features, attribute: Option<&str>, extent: [f64; 4], width: usize, height: usize) -> Result<Vec<f64>> {
    let shapes = touching(&features.shapes, &features.shape_boxes, extent);
    let mut out = burn(&shapes, attribute, extent, width, height)?;
    let (px, py) = ((extent[2] - extent[0]) / width as f64, (extent[3] - extent[1]) / height as f64);
    let padded = [extent[0] - px, extent[1] - py, extent[2] + px, extent[3] + py];
    let points = touching(&features.points, &features.point_boxes, padded);
    if points.iter().all(|l| l.features.is_empty()) {
        return Ok(out);
    }
    let (pw, ph) = (width + 2, height + 2);
    let burned = burn(&points, attribute, padded, pw, ph)?;
    for row in 0..ph {
        for col in 0..pw {
            let v = burned[row * pw + col];
            if v.is_nan() {
                continue;
            }
            // Padded (row, col) is output (row - 1, col - 1); its 3 x 3 block is output rows
            // row - 2 to row and columns col - 2 to col.
            for r in row.saturating_sub(2)..row.min(height - 1) + 1 {
                for c in col.saturating_sub(2)..col.min(width - 1) + 1 {
                    out[r * width + c] = v;
                }
            }
        }
    }
    Ok(out)
}

/// Draws vector layers, with north up.
///
/// # Errors
///
/// When the layers have nothing to draw or different CRSs, or the attribute is not a field.
pub fn render_vector(layers: Vec<Layer>, options: &RenderOptions) -> Result<Image> {
    Renderer::vector(layers, options)?.image(options)
}

/// The layers named in `names`, in their order in `layers`; all of them when `names` is empty.
///
/// # Errors
///
/// When a name matches no layer.
pub fn select_layers(mut layers: Vec<Layer>, names: &[String]) -> Result<Vec<Layer>> {
    if names.is_empty() {
        return Ok(layers);
    }
    if let Some(missing) = names.iter().find(|n| !layers.iter().any(|l| &l.name == *n)) {
        return Err(Error::InvalidArgument(format!("no layer '{missing}'")));
    }
    layers.retain(|l| names.contains(&l.name));
    Ok(layers)
}

/// The numeric values of `field` across the layers' features.
fn attribute_values(layers: &[Layer], field: &str) -> Result<Vec<f64>> {
    let mut values = Vec::new();
    for layer in layers {
        let index = layer
            .schema
            .field_index(field)
            .ok_or_else(|| Error::InvalidArgument(format!("layer '{}' has no field '{field}'", layer.name)))?;
        // Features without a geometry are not drawn, so they do not set the range either.
        let drawn = layer.features.iter().filter(|f| f.geometry.is_some());
        values.extend(drawn.filter_map(|f| f.attributes.get(index).and_then(|v| v.as_f64())));
    }
    Ok(values)
}

/// A dataset drawn repeatedly, part by part, with the same colors: what a map view draws from.
pub struct Renderer {
    inner: Inner,
    plan: Plan,
    ranges: Vec<(f64, f64)>,
}

enum Inner {
    Raster(LazyRaster),
    Vector { features: Features, attribute: Option<String>, extent: [f64; 4], size: (usize, usize) },
}

impl std::fmt::Debug for Renderer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (width, height) = self.size();
        f.debug_struct("Renderer").field("width", &width).field("height", &height).finish_non_exhaustive()
    }
}

impl Renderer {
    /// A renderer of a raster, its stretch measured on a small sample.
    ///
    /// # Errors
    ///
    /// When the options do not fit the raster, or the read fails.
    pub fn raster(mut source: LazyRaster, options: &RenderOptions) -> Result<Self> {
        let plan = plan(&source, options)?;
        let sample = RenderOptions { width: None, height: None, max_size: STYLE_SAMPLE_SIZE, ..options.clone() };
        let (width, height) = output_size((source.width(), source.height()), &sample)?;
        let window = Window::full(source.width(), source.height());
        let values = read_region(&mut source, &plan, window, width, height)?;
        let byte_as_stored = source.data_type() == DataType::U8 && plan.mode == Mode::Rgb;
        let ranges = stretch_ranges(&values, plan.colors, width * height, options.stretch, byte_as_stored);
        Ok(Self { inner: Inner::Raster(source), plan, ranges })
    }

    /// A renderer of vector layers, drawn on a virtual grid of up to 4096 pixels a side.
    ///
    /// # Errors
    ///
    /// When the layers have nothing to draw or different CRSs, or the attribute is not a field.
    pub fn vector(layers: Vec<Layer>, options: &RenderOptions) -> Result<Self> {
        options.stretch.check()?;
        if options.colormap.is_some() && options.attribute.is_none() {
            return Err(Error::InvalidArgument("a colormap colors vector features by an attribute: give one".into()));
        }
        let extent = vector_extent(&layers)?;
        let (w, h) = (extent[2] - extent[0], extent[3] - extent[1]);
        let scale = VECTOR_GRID_SIZE as f64 / w.max(h);
        let size = (((w * scale).round() as usize).max(1), ((h * scale).round() as usize).max(1));
        let (mode, ranges) = match &options.attribute {
            None => (Mode::Solid(FEATURE_COLOR), vec![(0.0, 1.0)]),
            Some(field) => {
                // Measured on the features' values, each feature once, whatever area it covers.
                let values = attribute_values(&layers, field)?;
                let colormap = options.colormap.map_or_else(|| Colormap::parse("viridis"), Ok)?;
                (Mode::Mapped(colormap.table()), stretch_ranges(&values, 1, values.len(), options.stretch, false))
            }
        };
        let plan = Plan { bands: vec![1], colors: 1, alpha_max: 255.0, mode };
        let features = Features::split(layers);
        Ok(Self { inner: Inner::Vector { features, attribute: options.attribute.clone(), extent, size }, plan, ranges })
    }

    /// The size of the whole dataset in this renderer's pixels, which [`Self::region`] takes
    /// windows of.
    pub fn size(&self) -> (usize, usize) {
        match &self.inner {
            Inner::Raster(source) => (source.width(), source.height()),
            Inner::Vector { size, .. } => *size,
        }
    }

    /// `raster` or `vector`.
    pub fn kind(&self) -> &'static str {
        match &self.inner {
            Inner::Raster(_) => "raster",
            Inner::Vector { .. } => "vector",
        }
    }

    /// The geotransform of [`Self::size`]'s pixels, when the dataset has one.
    pub fn geo_transform(&self) -> Option<[f64; 6]> {
        match &self.inner {
            Inner::Raster(source) => source.has_geo_transform().then(|| source.geo_transform()),
            Inner::Vector { extent, size, .. } => {
                Some([extent[0], (extent[2] - extent[0]) / size.0 as f64, 0.0, extent[3], 0.0, -(extent[3] - extent[1]) / size.1 as f64])
            }
        }
    }

    /// Draws `window` at `width` by `height` pixels.
    ///
    /// # Errors
    ///
    /// When the window is outside the dataset, the size is out of bounds, or a read fails.
    pub fn region(&mut self, window: Window, width: usize, height: usize) -> Result<Image> {
        let (full_w, full_h) = self.size();
        window.check(full_w, full_h)?;
        if window.width == 0 || window.height == 0 {
            return Err(Error::InvalidArgument("the window needs a width and a height".into()));
        }
        check_size(width, height)?;
        let values = match &mut self.inner {
            Inner::Raster(source) => read_region(source, &self.plan, window, width, height)?,
            Inner::Vector { features, attribute, extent, size } => {
                let (ux, uy) = ((extent[2] - extent[0]) / size.0 as f64, (extent[3] - extent[1]) / size.1 as f64);
                let part = [
                    extent[0] + window.x as f64 * ux,
                    extent[3] - (window.y + window.height) as f64 * uy,
                    extent[0] + (window.x + window.width) as f64 * ux,
                    extent[3] - window.y as f64 * uy,
                ];
                draw_features(features, attribute.as_deref(), part, width, height)?
            }
        };
        Ok(shade(&values, &self.plan, &self.ranges, width, height))
    }

    /// Draws the whole dataset at the size `options` ask for.
    ///
    /// # Errors
    ///
    /// When the size is out of bounds, or a read fails.
    pub fn image(&mut self, options: &RenderOptions) -> Result<Image> {
        let (full_w, full_h) = self.size();
        let (width, height) = output_size((full_w, full_h), options)?;
        self.region(Window::full(full_w, full_h), width, height)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbraster::RasterConfig;
    use wbvector::{Feature, FieldDef, FieldType, FieldValue};

    fn raster(cols: usize, rows: usize, bands: usize, nodata: f64, values: Vec<f64>) -> LazyRaster {
        let config = RasterConfig { cols, rows, bands, data_type: DataType::F64, nodata, ..Default::default() };
        let raster = crate::pixel::raster_from_data(config, wbraster::raster::RasterData::F64(values)).unwrap();
        LazyRaster::from(crate::RasterDataset::new(raster, true))
    }

    fn pixel(image: &Image, col: usize, row: usize) -> [u8; 4] {
        let at = (row * image.width + col) * 4;
        image.rgba[at..at + 4].try_into().unwrap()
    }

    #[test]
    fn stretch_and_colormap_names_parse_and_refuse() {
        assert_eq!(Stretch::parse("p2,p98").unwrap(), Stretch::Percentile(2.0, 98.0));
        assert_eq!(Stretch::parse("-5,10").unwrap(), Stretch::Range(-5.0, 10.0));
        assert_eq!(Stretch::parse("MinMax").unwrap(), Stretch::MinMax);
        for bad in ["5,1", "p2,98", "p-1,p50", "nan,1", "x"] {
            assert!(Stretch::parse(bad).is_err(), "{bad}");
        }
        for name in COLORMAPS {
            assert!(Colormap::parse(name).is_ok());
        }
        assert_eq!(Colormap::parse("viridis").unwrap().color(0.0), [0x44, 0x01, 0x54]);
        assert_eq!(Colormap::parse("VIRIDIS_r").unwrap().color(0.0), [0xfd, 0xe7, 0x25]);
        assert!(Colormap::parse("nope").is_err());
    }

    #[test]
    fn nodata_is_transparent_and_values_stretch_to_the_range() {
        let mut source = raster(3, 1, 1, -1.0, vec![-1.0, 0.0, 10.0]);
        let options = RenderOptions { stretch: Stretch::Range(0.0, 10.0), ..Default::default() };
        let image = render_raster(&mut source, &options).unwrap();
        assert_eq!((image.width, image.height), (3, 1));
        assert_eq!(pixel(&image, 0, 0)[3], 0);
        assert_eq!(pixel(&image, 1, 0), [0, 0, 0, 255]);
        assert_eq!(pixel(&image, 2, 0), [255, 255, 255, 255]);
    }

    #[test]
    fn shrinking_averages_valid_pixels_and_skips_invalid_ones() {
        let mut source = raster(4, 2, 1, -1.0, vec![0.0, 10.0, -1.0, -1.0, 0.0, 10.0, -1.0, -1.0]);
        let options = RenderOptions { width: Some(2), stretch: Stretch::Range(0.0, 10.0), ..Default::default() };
        let image = render_raster(&mut source, &options).unwrap();
        assert_eq!((image.width, image.height), (2, 1));
        assert_eq!(pixel(&image, 0, 0), [128, 128, 128, 255]);
        assert_eq!(pixel(&image, 1, 0)[3], 0);
    }

    #[test]
    fn an_explicit_band_trio_draws_rgb_and_other_counts_are_refused() {
        let mut source = raster(1, 1, 3, -1.0, vec![10.0, 0.0, 5.0]);
        let options = RenderOptions { bands: vec![1, 2, 3], stretch: Stretch::Range(0.0, 10.0), ..Default::default() };
        assert_eq!(pixel(&render_raster(&mut source, &options).unwrap(), 0, 0), [255, 0, 128, 255]);
        for bands in [vec![1, 2], vec![4], vec![0]] {
            assert!(render_raster(&mut source, &RenderOptions { bands, ..Default::default() }).is_err());
        }
        let mapped = RenderOptions { bands: vec![1, 2, 3], colormap: Some(Colormap::parse("viridis").unwrap()), ..Default::default() };
        assert!(render_raster(&mut source, &mapped).is_err());
    }

    #[test]
    fn a_palette_colors_its_indices_without_averaging_them() {
        let config = RasterConfig { cols: 2, rows: 1, bands: 1, data_type: DataType::U8, nodata: 9.0, ..Default::default() };
        let mut dataset =
            crate::RasterDataset::new(crate::pixel::raster_from_data(config, wbraster::raster::RasterData::U8(vec![0, 2])).unwrap(), true);
        dataset.color_table = vec![[10, 20, 30, 255], [0, 0, 0, 255], [200, 100, 50, 255]];
        let mut source = LazyRaster::from(dataset);
        let image = render_raster(&mut source, &RenderOptions::default()).unwrap();
        assert_eq!(pixel(&image, 0, 0), [10, 20, 30, 255]);
        assert_eq!(pixel(&image, 1, 0), [200, 100, 50, 255]);
        let shrunk = render_raster(&mut source, &RenderOptions { width: Some(1), ..Default::default() }).unwrap();
        assert!([[10, 20, 30, 255], [200, 100, 50, 255]].contains(&pixel(&shrunk, 0, 0)));
    }

    #[test]
    fn shrinking_weights_colors_by_alpha() {
        let config = RasterConfig { cols: 2, rows: 1, bands: 4, data_type: DataType::U8, nodata: -1.0, ..Default::default() };
        // A faint red pixel beside an opaque blue one.
        let data = wbraster::raster::RasterData::U8(vec![255, 0, 0, 0, 0, 255, 10, 255]);
        let mut dataset = crate::RasterDataset::new(crate::pixel::raster_from_data(config, data).unwrap(), false);
        dataset.band_metadata = ["Red", "Green", "Blue", "Alpha"]
            .iter()
            .map(|c| crate::BandMetadata { color_interp: Some((*c).to_string()), ..Default::default() })
            .collect();
        let image = render_raster(&mut LazyRaster::from(dataset), &RenderOptions { width: Some(1), ..Default::default() }).unwrap();
        let p = pixel(&image, 0, 0);
        // Red 255 x 10 and blue 255 x 255, over the alpha sum 265; alpha a plain mean.
        assert_eq!(p, [10, 0, 245, 133]);
    }

    #[test]
    fn invalid_stretches_and_oversized_images_are_refused() {
        let mut source = raster(2, 1, 1, -1.0, vec![0.0, 1.0]);
        for stretch in [Stretch::Range(5.0, 1.0), Stretch::Percentile(2.0, 120.0)] {
            assert!(render_raster(&mut source, &RenderOptions { stretch, ..Default::default() }).is_err());
        }
        let huge = RenderOptions { width: Some(16384), height: Some(16384), ..Default::default() };
        assert!(render_raster(&mut source, &huge).is_err());
    }

    #[test]
    fn points_at_a_region_edge_widen_into_it() {
        let mut layer = Layer::new("points");
        layer.push(Feature::with_geometry(0, Geometry::point(0.0, 0.0), 0));
        layer.push(Feature::with_geometry(1, Geometry::point(100.0, 100.0), 0));
        let mut renderer = Renderer::vector(vec![layer], &RenderOptions::default()).unwrap();
        let (_, height) = renderer.size();
        let whole = renderer.region(Window::new(0, height - 10, 10, 10), 10, 10).unwrap();
        assert_eq!(pixel(&whole, 0, 9)[3], 255);
        // The point is in column 0; the region from column 1 leaves it out but shows its symbol.
        let right = renderer.region(Window::new(1, height - 10, 10, 10), 10, 10).unwrap();
        assert_eq!(pixel(&right, 0, 9)[3], 255);
    }

    #[test]
    fn layers_are_selected_by_name_and_unknown_names_refused() {
        let layers = vec![Layer::new("a"), Layer::new("b")];
        assert_eq!(select_layers(layers.clone(), &[]).unwrap().len(), 2);
        assert_eq!(select_layers(layers.clone(), &["b".into()]).unwrap()[0].name, "b");
        assert!(select_layers(layers, &["c".into()]).is_err());
    }

    #[test]
    fn points_beside_shapes_are_widened_and_drawn_on_top() {
        let mut layer = square_layer();
        layer.push(Feature::with_geometry(1, Geometry::point(5.0, 5.0), 1));
        layer.push(Feature::with_geometry(2, Geometry::point(10.0, 10.0), 1));
        let image = render_vector(vec![layer], &RenderOptions { max_size: 100, ..Default::default() }).unwrap();
        // The middle point, away from the square, is 3 x 3 pixels.
        let opaque = (40..60).flat_map(|r| (40..60).map(move |c| (c, r))).filter(|&(c, r)| pixel(&image, c, r)[3] == 255).count();
        assert_eq!(opaque, 9);
    }

    #[test]
    fn rows_are_sampled_when_a_level_is_too_large_to_hold() {
        // 5000 x 5000 > the direct budget, with no overviews: only sampled rows are read.
        let (cols, rows) = (5000, 5000);
        let values: Vec<f64> = (0..rows).flat_map(|r| std::iter::repeat_n(r as f64, cols)).collect();
        let mut source = raster(cols, rows, 1, -1.0, values);
        let options = RenderOptions { width: Some(10), stretch: Stretch::MinMax, ..Default::default() };
        let image = render_raster(&mut source, &options).unwrap();
        assert_eq!((image.width, image.height), (10, 10));
        assert!(pixel(&image, 0, 0)[0] < pixel(&image, 0, 9)[0]);
    }

    #[test]
    fn a_renderer_keeps_its_colors_across_regions() {
        let values = vec![0.0, 10.0, 20.0, 30.0];
        let mut renderer =
            Renderer::raster(raster(4, 1, 1, -1.0, values), &RenderOptions { stretch: Stretch::MinMax, ..Default::default() }).unwrap();
        let right = renderer.region(Window::new(2, 0, 2, 1), 2, 1).unwrap();
        assert_eq!(pixel(&right, 1, 0), [255, 255, 255, 255]);
        assert_eq!(pixel(&right, 0, 0)[0], 170);
        assert!(renderer.region(Window::new(3, 0, 2, 1), 2, 1).is_err());
    }

    fn ring(points: &[(f64, f64)]) -> Vec<wbvector::Coord> {
        points.iter().map(|&(x, y)| wbvector::Coord::xy(x, y)).collect()
    }

    fn square_layer() -> Layer {
        let mut layer = Layer::new("squares");
        layer.schema.add_field(FieldDef::new("height", FieldType::Float));
        let mut feature =
            Feature::with_geometry(0, Geometry::polygon(ring(&[(0.0, 0.0), (2.0, 0.0), (2.0, 1.0), (0.0, 1.0), (0.0, 0.0)]), vec![]), 1);
        feature.attributes[0] = FieldValue::Float(7.0);
        layer.push(feature);
        layer
    }

    #[test]
    fn vector_layers_draw_in_their_aspect_with_a_transparent_background() {
        let mut layer = square_layer();
        layer.features[0].geometry = Some(Geometry::polygon(ring(&[(0.0, 0.0), (2.0, 0.0), (2.0, 1.0), (0.0, 0.0)]), vec![]));
        let image = render_vector(vec![layer], &RenderOptions { max_size: 64, ..Default::default() }).unwrap();
        assert_eq!((image.width, image.height), (64, 32));
        assert_eq!(pixel(&image, 60, 5)[..3], FEATURE_COLOR);
        assert_eq!(pixel(&image, 2, 2)[3], 0);
    }

    #[test]
    fn vector_attributes_are_colored_and_colormaps_need_one() {
        let options =
            RenderOptions { max_size: 16, attribute: Some("height".into()), stretch: Stretch::Range(0.0, 7.0), ..Default::default() };
        let image = render_vector(vec![square_layer()], &options).unwrap();
        let missing = RenderOptions { attribute: Some("nope".into()), ..Default::default() };
        assert!(render_vector(vec![square_layer()], &missing).is_err());
        assert_eq!(pixel(&image, 8, 4)[..3], [0xfd, 0xe7, 0x25]);
        let no_field = RenderOptions { colormap: Some(Colormap::parse("gray").unwrap()), ..Default::default() };
        assert!(render_vector(vec![square_layer()], &no_field).is_err());
    }

    #[test]
    fn points_are_widened_and_an_empty_dataset_is_refused() {
        let mut layer = Layer::new("points");
        layer.push(Feature::with_geometry(0, Geometry::point(0.0, 0.0), 0));
        layer.push(Feature::with_geometry(1, Geometry::point(10.0, 10.0), 0));
        let image = render_vector(vec![layer], &RenderOptions { max_size: 20, ..Default::default() }).unwrap();
        let opaque = image.rgba.as_chunks::<4>().0.iter().filter(|p| p[3] == 255).count();
        assert!(opaque >= 6, "{opaque}");
        assert!(render_vector(vec![Layer::new("empty")], &RenderOptions::default()).is_err());
    }
}
