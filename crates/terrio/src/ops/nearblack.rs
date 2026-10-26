//! Collar cleaning: an equivalent of `nearblack` and `gdal raster clean-collar` (GDAL 3.12).
//!
//! Nearly black (or white, or another color) pixels along the edges of an image, such as the
//! noisy collar a lossy compression leaves around a scene, are set to exactly black (or white),
//! and optionally made transparent in an alpha band or a mask. Two algorithms are ported from
//! GDAL's `nearblack_lib.cpp` and `nearblack_lib_floodfill.cpp`, with GDAL's scan order, since
//! both depend on the order pixels are visited:
//!
//! - [`CollarAlgorithm::TwoPasses`] (`nearblack`'s default): a top-to-bottom then a
//!   bottom-to-top pass, each scanning every line from both sides, searching inwards until more
//!   than `max_non_black` consecutive non-black pixels are met.
//! - [`CollarAlgorithm::FloodFill`] (`gdal raster clean-collar`'s default): a span flood fill of
//!   the black pixels connected to the edges (4-connected). With `max_non_black > 0`, the
//!   two-pass algorithm runs first.
//!
//! Values are read as Byte (as GDAL's `RasterIO` converts them: rounded and clamped) and the
//! output is Byte.

use super::Grid;
use crate::{Error, RasterDataset, Result};
use wbraster::DataType;
use wbraster::raster::RasterData;

/// The collar-cleaning algorithm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CollarAlgorithm {
    /// Two passes, top-down then bottom-up (`nearblack -alg twopasses`, its default).
    #[default]
    TwoPasses,
    /// A flood fill from the edges (`-alg floodfill`, the default of `gdal raster clean-collar`).
    FloodFill,
}

impl CollarAlgorithm {
    /// Parses `twopasses` or `floodfill`.
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "twopasses" => Ok(Self::TwoPasses),
            "floodfill" => Ok(Self::FloodFill),
            _ => Err(Error::InvalidArgument(format!("unknown algorithm '{s}': floodfill or twopasses"))),
        }
    }
}

/// Options for [`nearblack`], named after `nearblack`'s.
#[derive(Debug, Clone, PartialEq)]
pub struct NearblackOptions {
    /// The colors of the collar (`-color`), one value per non-alpha band. Empty for black, or
    /// white with `white`.
    pub colors: Vec<Vec<i32>>,
    /// Search for nearly white (255) pixels rather than nearly black ones (`-white`).
    pub white: bool,
    /// How far from the colors values can be and still be part of the collar (`-near`,
    /// `--color-threshold`; 15 by default).
    pub near: i32,
    /// How many consecutive non-black pixels the search inwards goes past (`-nb`,
    /// `--pixel-distance`; 2 by default).
    pub max_non_black: i32,
    /// Add an alpha band, transparent in the collar (`-setalpha`).
    pub set_alpha: bool,
    /// Add a per-dataset mask, 0 in the collar (`-setmask`).
    pub set_mask: bool,
    /// The algorithm (`-alg`).
    pub algorithm: CollarAlgorithm,
}

impl Default for NearblackOptions {
    fn default() -> Self {
        Self {
            colors: Vec::new(),
            white: false,
            near: 15,
            max_non_black: 2,
            set_alpha: false,
            set_mask: false,
            algorithm: CollarAlgorithm::TwoPasses,
        }
    }
}

/// Parses a `--color` value: `black`, `white` (for `bands` bands) or integers separated by
/// commas (`r,g,b`).
pub fn parse_color(s: &str, bands: usize) -> Result<Vec<i32>> {
    match s.trim().to_ascii_lowercase().as_str() {
        "black" => Ok(vec![0; bands]),
        "white" => Ok(vec![255; bands]),
        t => t
            .split(',')
            .map(|v| {
                v.trim().parse::<i32>().map_err(|_| Error::InvalidArgument(format!("colors are integers separated by commas, not '{s}'")))
            })
            .collect(),
    }
}

/// The number of non-alpha bands of a raster: the bands `--color black`/`white` and colors
/// apply to. GDAL excludes a trailing alpha only when alpha/mask processing is enabled.
pub fn color_bands(ds: &RasterDataset, alpha_or_mask: bool) -> usize {
    let n = ds.band_count();
    n - usize::from(alpha_or_mask && is_alpha(ds, n))
}

/// Applies `gdal raster clean-collar`'s defaults: an input whose last band is an alpha band keeps
/// it updated (`set_alpha`), and one with a per-dataset mask (`has_mask`) keeps a mask
/// (`set_mask`), unless the other was requested.
pub fn keep_alpha_or_mask(o: &mut NearblackOptions, ds: &RasterDataset, has_mask: bool) {
    let (alpha, mask) = (o.set_alpha, o.set_mask);
    o.set_alpha = alpha || (!mask && is_alpha(ds, ds.band_count()));
    o.set_mask = mask || (!alpha && has_mask);
}

/// The result of [`nearblack`]: the Byte raster and, with `set_mask`, its per-dataset mask (0 in
/// the collar, 255 elsewhere, row by row).
#[derive(Debug, Clone)]
pub struct Collar {
    /// The output raster.
    pub dataset: RasterDataset,
    /// The mask, with `set_mask`.
    pub mask: Option<Vec<u8>>,
}

/// What the algorithms work on: the image's values, pixel-interleaved.
struct Image<'a> {
    width: usize,
    height: usize,
    /// `dst_bands` Byte values per pixel, row after row.
    data: &'a mut [u8],
    /// The mask, with `set_mask`.
    mask: Option<&'a mut [u8]>,
    /// The bands compared with the colors (the first ones).
    src_bands: usize,
    /// The bands per pixel; an extra last band is the alpha band.
    dst_bands: usize,
}

struct Params<'a> {
    colors: &'a [Vec<i32>],
    near: i32,
    max_non_black: i32,
}

impl Params<'_> {
    /// Whether no color is within `near` of the pixel (in every band).
    #[inline]
    fn non_black(&self, px: &[u8]) -> bool {
        let mut non_black = false;
        for c in self.colors {
            non_black = false;
            for (b, p) in px.iter().enumerate() {
                let p = i64::from(*p);
                let color = i64::from(c[b]);
                if color - p > i64::from(self.near) || p > i64::from(self.near) + color {
                    non_black = true;
                    break;
                }
            }
            if !non_black {
                break;
            }
        }
        non_black
    }
}

/// Sets pixel `i` of a line (and its alpha and mask) to the collar.
#[inline]
fn set(line: &mut [u8], mask: &mut Option<&mut [u8]>, i: usize, src_bands: usize, dst_bands: usize, value: u8) {
    line[i * dst_bands..i * dst_bands + src_bands].fill(value);
    if dst_bands > src_bands {
        line[i * dst_bands + dst_bands - 1] = 0;
    }
    if let Some(m) = mask {
        m[i] = 0;
    }
}

// Ported from GDAL's ProcessLine() (apps/nearblack_lib.cpp, MIT; attribution in NOTICE).
#[allow(clippy::too_many_arguments)]
fn process_line(
    line: &mut [u8],
    mut mask: Option<&mut [u8]>,
    start: isize,
    end: isize,
    src_bands: usize,
    dst_bands: usize,
    p: &Params,
    counts: &mut [i32],
    horizontal: bool,
    vertical: bool,
    bottom_up: bool,
    line_from_edge: usize,
) {
    let replace = if p.colors.len() == 1 && p.colors[0].first() == Some(&255) { 255 } else { 0 };
    let px = |line: &[u8], i: usize| p.non_black(&line[i * dst_bands..i * dst_bands + src_bands]);
    if vertical {
        let width = (start + 1).max(end + 1) as usize;
        #[allow(clippy::needless_range_loop)]
        for i in 0..width {
            if counts[i] > p.max_non_black {
                continue;
            }
            if px(line, i) {
                counts[i] += 1;
                if counts[i] > p.max_non_black {
                    continue;
                }
                if line_from_edge == 0 && p.max_non_black > 0 {
                    // A valid value on the first line: the setting is ignored.
                    counts[i] = p.max_non_black + 1;
                    continue;
                }
            }
            set(line, &mut mask, i, src_bands, dst_bands, replace);
        }
    }
    if horizontal {
        let mut non_black_pixels = 0;
        let max_non_black = if bottom_up { 0 } else { p.max_non_black };
        let dir = if start < end { 1 } else { -1 };
        let mut test = true;
        let mut i = start;
        while i != end {
            let iu = i as usize;
            if test {
                let non_black = px(line, iu);
                if non_black {
                    if counts[iu] <= max_non_black {
                        non_black_pixels = counts[iu];
                    } else {
                        non_black_pixels += 1;
                    }
                }
                if non_black_pixels > max_non_black {
                    test = false;
                    i += dir;
                    continue;
                }
                if non_black && max_non_black > 0 && i == start {
                    test = false;
                    i += dir;
                    continue;
                }
                set(line, &mut mask, iu, src_bands, dst_bands, replace);
            } else if counts[iu] == 0 {
                test = true;
                non_black_pixels = 0;
            }
            i += dir;
        }
    }
}

// Ported from GDAL's GDALNearblackTwoPassesAlgorithm() (apps/nearblack_lib.cpp, MIT).
fn two_passes(img: &mut Image, p: &Params) {
    let (w, h, sb, db) = (img.width, img.height, img.src_bands, img.dst_bands);
    let mut counts = vec![0i32; w];
    let last = w as isize - 1;
    for pass in 0..2 {
        if pass == 1 {
            counts.fill(0);
        }
        for k in 0..h {
            let y = if pass == 0 { k } else { h - 1 - k };
            let line = &mut img.data[y * w * db..(y + 1) * w * db];
            let mut mask = img.mask.as_deref_mut().map(|m| &mut m[y * w..(y + 1) * w]);
            process_line(line, mask.as_deref_mut(), 0, last, sb, db, p, &mut counts, true, true, pass == 1, k);
            process_line(line, mask, last, 0, sb, db, p, &mut counts, true, false, pass == 1, k);
        }
    }
}

const UNINIT: u8 = 0;
const FALSE: u8 = 1;
const TRUE: u8 = 2;

/// GDAL's span flood fill state (`GDALNearblackFloodFillAlg`).
struct Flood<'a, 'b> {
    img: &'a mut Image<'b>,
    p: &'a Params<'a>,
    /// Whether each pixel must be set: unknown, no or yes (GDAL's visited dataset).
    must: Vec<u8>,
    replace: u8,
}

impl Flood<'_, '_> {
    fn must_set(&mut self, x: usize, y: usize) -> bool {
        let i = y * self.img.width + x;
        if self.must[i] != UNINIT {
            return self.must[i] == TRUE;
        }
        let (sb, db) = (self.img.src_bands, self.img.dst_bands);
        let px = &self.img.data[i * db..i * db + sb];
        // A pixel within `near` of one of the colors.
        let transparent = self.p.colors.iter().any(|c| {
            px.iter().enumerate().all(|(b, v)| {
                let v = *v as i32;
                !(c[b] - v > self.p.near || v > self.p.near + c[b])
            })
        });
        self.must[i] = if transparent { TRUE } else { FALSE };
        transparent
    }

    fn set(&mut self, x: usize, y: usize) {
        let w = self.img.width;
        let i = y * w + x;
        self.must[i] = FALSE;
        let (sb, db) = (self.img.src_bands, self.img.dst_bands);
        self.img.data[i * db..i * db + sb].fill(self.replace);
        if db > sb {
            self.img.data[i * db + db - 1] = 0;
        }
        if let Some(m) = self.img.mask.as_deref_mut() {
            m[i] = 0;
        }
    }

    // Ported from GDALNearblackFloodFillAlg::Fill() (apps/nearblack_lib_floodfill.cpp, MIT):
    // the "combined-scan-and-fill span filler" of https://en.wikipedia.org/wiki/Flood_fill.
    fn fill(&mut self, x0: usize, y0: usize) {
        let (w, h) = (self.img.width as isize, self.img.height as isize);
        if !self.must_set(x0, y0) {
            return;
        }
        let mut queue = std::collections::VecDeque::new();
        let (x0, y0) = (x0 as isize, y0 as isize);
        queue.push_back((x0, x0, y0, 1isize));
        if y0 > 0 {
            queue.push_back((x0, x0, y0 - 1, -1));
        }
        while let Some((x1, x2, y, dy)) = queue.pop_front() {
            let yu = y as usize;
            let mut x = x1;
            if x > 0 && self.must_set(x as usize, yu) {
                while self.must_set((x - 1) as usize, yu) {
                    self.set((x - 1) as usize, yu);
                    x -= 1;
                    if x == 0 {
                        break;
                    }
                }
            }
            if x >= 0 && x < x1 && y - dy >= 0 && y - dy < h {
                queue.push_back((x, x1 - 1, y - dy, -dy));
            }
            let mut x1 = x1;
            while x1 <= x2 {
                while self.must_set(x1 as usize, yu) {
                    self.set(x1 as usize, yu);
                    x1 += 1;
                    if x1 == w {
                        break;
                    }
                }
                if x < x1 && y + dy >= 0 && y + dy < h {
                    queue.push_back((x, x1 - 1, y + dy, dy));
                }
                if x1 - 1 > x2 && y - dy >= 0 && y - dy < h {
                    queue.push_back((x2 + 1, x1 - 1, y - dy, -dy));
                }
                x1 += 1;
                while x1 < x2 && !self.must_set(x1 as usize, yu) {
                    x1 += 1;
                }
                x = x1;
            }
        }
    }

    fn run(&mut self) {
        let (w, h) = (self.img.width, self.img.height);
        for x in 0..w {
            self.fill(x, 0);
        }
        for y in 1..h.saturating_sub(1) {
            self.fill(0, y);
            self.fill(w - 1, y);
        }
        for x in 0..w {
            self.fill(x, h - 1);
        }
    }
}

/// Runs the algorithm on an image whose values (and alpha and mask) are initialized.
fn clean(img: &mut Image, colors: &[Vec<i32>], o: &NearblackOptions) {
    let p = Params { colors, near: o.near, max_non_black: o.max_non_black };
    if img.width == 0 || img.height == 0 {
        return;
    }
    if o.algorithm == CollarAlgorithm::TwoPasses || o.max_non_black > 0 {
        two_passes(img, &p);
    }
    if o.algorithm == CollarAlgorithm::FloodFill {
        let n = img.width * img.height;
        let replace = if o.white { 255 } else { 0 };
        Flood { img, p: &p, must: vec![UNINIT; n], replace }.run();
    }
}

/// The colors to search for: those given, else black or white over `bands` bands.
fn colors(o: &NearblackOptions, bands: usize) -> Result<Vec<Vec<i32>>> {
    if o.near < 0 || o.max_non_black < 0 || o.max_non_black == i32::MAX {
        return Err(Error::InvalidArgument("near must be nonnegative and max_non_black must be between 0 and 2147483646".into()));
    }
    let colors = if o.colors.is_empty() { vec![vec![if o.white { 255 } else { 0 }; bands]] } else { o.colors.clone() };
    if colors.iter().any(|c| c.len() != bands) {
        return Err(Error::InvalidArgument(format!("colors must have as many values as the {bands} non-alpha input band(s)")));
    }
    Ok(colors)
}

/// Whether band `b` (from 1) of a raster is an alpha band.
fn is_alpha(ds: &RasterDataset, b: usize) -> bool {
    b > 0 && b <= ds.band_count() && ds.band_color_interp(b).eq_ignore_ascii_case("Alpha")
}

/// Copies `bands` bands of `ds` (from band 1) into pixel-interleaved Byte values with `stride`
/// values per pixel, converted as GDAL converts them to Byte.
fn interleave(ds: &RasterDataset, bands: usize, stride: usize, out: &mut [u8]) {
    let n = ds.raster.cols * ds.raster.rows;
    for b in 0..bands {
        let v = crate::ops::BandView::of(&ds.raster, b);
        for i in 0..n {
            out[i * stride + b] = super::convert_value(v.get(i), DataType::U8) as u8;
        }
    }
}

/// Cleans the collar of a raster held in memory, writing a new Byte raster as `nearblack -o`
/// does: the input's bands (but an alpha band, with `set_alpha` or `set_mask`), plus an alpha
/// band with `set_alpha`, and the georeferencing; no nodata value.
///
/// ```no_run
/// use terrio::ops::nearblack::{CollarAlgorithm, NearblackOptions, nearblack};
///
/// let scene = terrio::open_raster("scene.tif")?;
/// let o = NearblackOptions { set_alpha: true, algorithm: CollarAlgorithm::FloodFill, ..Default::default() };
/// terrio::write_raster_dataset(&nearblack(&scene, &o)?.dataset, "clean.tif", &Default::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn nearblack(ds: &RasterDataset, o: &NearblackOptions) -> Result<Collar> {
    if o.set_alpha && o.set_mask {
        return Err(Error::InvalidArgument("set_alpha and set_mask are mutually exclusive".into()));
    }
    if ds.complex {
        return Err(Error::Unsupported("collars of complex bands are not supported".into()));
    }
    let n_in = ds.band_count();
    let (mut src_bands, mut dst_bands) = (n_in, n_in);
    let last_alpha = is_alpha(ds, n_in);
    if o.set_alpha {
        if last_alpha {
            src_bands -= 1;
        } else {
            dst_bands += 1;
        }
    }
    if o.set_mask && last_alpha {
        dst_bands -= 1;
        src_bands = src_bands.saturating_sub(1);
    }
    if dst_bands == 0 {
        return Err(Error::InvalidArgument("the output would have no band".into()));
    }
    let colors = colors(o, src_bands)?;
    let (w, h) = (ds.raster.cols, ds.raster.rows);
    let mut data = vec![0u8; w * h * dst_bands];
    interleave(ds, src_bands, dst_bands, &mut data);
    if o.set_alpha {
        for px in data.chunks_exact_mut(dst_bands) {
            px[dst_bands - 1] = 255;
        }
    }
    let mut mask = o.set_mask.then(|| vec![255u8; w * h]);
    let mut img = Image { width: w, height: h, data: &mut data, mask: mask.as_deref_mut(), src_bands, dst_bands };
    clean(&mut img, &colors, o);
    let mut out = super::raster_header(Grid::of(&ds.raster), DataType::U8, dst_bands, None, &ds.crs())?;
    if let Some(gt) = ds.affine {
        out.set_geo_transform(gt);
    }
    out.gcps = ds.gcps.clone();
    out.band_nodata = vec![None; dst_bands];
    if o.set_alpha {
        out.band_metadata = vec![Default::default(); dst_bands];
        out.band_metadata[dst_bands - 1].color_interp = Some("Alpha".into());
    }
    let n = w * h;
    let mut planar = vec![0u8; n * dst_bands];
    for (i, px) in data.chunks_exact(dst_bands).enumerate() {
        for (b, v) in px.iter().enumerate() {
            planar[b * n + i] = *v;
        }
    }
    out.raster.data = RasterData::U8(planar);
    Ok(Collar { dataset: out, mask })
}

/// Cleans the collar of a raster in place, as `nearblack` does without `-o` (or
/// `gdal raster clean-collar --update`): the result has the input's bands and data type (Byte
/// values written back to them), an existing last alpha band is updated with `set_alpha` (which
/// needs one), and the mask is returned with `set_mask`.
pub fn nearblack_in_place(ds: &mut RasterDataset, o: &NearblackOptions) -> Result<Option<Vec<u8>>> {
    use crate::pixel::Pixel;
    if o.set_alpha && o.set_mask {
        return Err(Error::InvalidArgument("set_alpha and set_mask are mutually exclusive".into()));
    }
    if ds.complex {
        return Err(Error::Unsupported("collars of complex bands are not supported".into()));
    }
    if ds.raster.data_type != DataType::U8 {
        crate::warnings::warn(
            "nearblack converts every input sample to Byte before writing it back; non-Byte values may be clamped or rounded".into(),
        );
    }
    let n_bands = ds.band_count();
    let last_alpha = is_alpha(ds, n_bands);
    let (mut src_bands, mut dst_bands) = (n_bands, n_bands);
    if o.set_alpha {
        if !last_alpha {
            return Err(Error::InvalidArgument("the last band is not an alpha band".into()));
        }
        src_bands -= 1;
    }
    if o.set_mask && last_alpha {
        src_bands = src_bands.saturating_sub(1);
        dst_bands -= 1;
    }
    let colors = colors(o, src_bands)?;
    let (w, h) = (ds.raster.cols, ds.raster.rows);
    let mut data = vec![0u8; w * h * dst_bands];
    interleave(ds, dst_bands, dst_bands, &mut data);
    if o.set_alpha {
        for px in data.chunks_exact_mut(dst_bands) {
            px[dst_bands - 1] = 255;
        }
    }
    let mut mask = o.set_mask.then(|| vec![255u8; w * h]);
    let mut img = Image { width: w, height: h, data: &mut data, mask: mask.as_deref_mut(), src_bands, dst_bands };
    clean(&mut img, &colors, o);
    let n = w * h;
    crate::pixel::with_data!(&mut ds.raster.data, v => {
        for b in 0..dst_bands {
            for i in 0..n {
                v[b * n + i] = Pixel::from_f64(f64::from(data[i * dst_bands + b]));
            }
        }
    });
    Ok(mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(w: usize, h: usize, values: &[u8]) -> RasterDataset {
        let grid = Grid { width: w, height: h, geo_transform: [0., 1., 0., h as f64, 0., -1.] };
        super::super::build_raster_data(grid, RasterData::U8(values.to_vec()), 1, None, &Default::default()).unwrap()
    }

    fn values(c: &Collar) -> Vec<u8> {
        match &c.dataset.raster.data {
            RasterData::U8(v) => v.clone(),
            _ => unreachable!(),
        }
    }

    #[test]
    fn extreme_color_thresholds_do_not_overflow() {
        let ds = image(1, 1, &[128]);
        let o = NearblackOptions { colors: vec![vec![i32::MAX]], near: i32::MAX, max_non_black: 0, ..Default::default() };
        assert_eq!(values(&nearblack(&ds, &o).unwrap()), vec![0]);
        let bad = NearblackOptions { max_non_black: i32::MAX, ..Default::default() };
        assert!(nearblack(&ds, &bad).is_err());
    }

    #[test]
    fn flood_fill_reaches_connected_dark_pixels_only() {
        #[rustfmt::skip]
        let v = [
            5, 5, 5, 5, 5,
            5, 200, 200, 200, 5,
            5, 200, 3, 200, 5,
            5, 200, 200, 200, 5,
            5, 5, 5, 5, 5,
        ];
        let o = NearblackOptions { algorithm: CollarAlgorithm::FloodFill, max_non_black: 0, set_mask: true, ..Default::default() };
        let c = nearblack(&image(5, 5, &v), &o).unwrap();
        let out = values(&c);
        // The enclosed dark pixel is not reached.
        assert_eq!(out[12], 3);
        assert_eq!(out[0], 0);
        assert_eq!(c.mask.as_ref().unwrap()[12], 255);
        assert_eq!(c.mask.as_ref().unwrap()[0], 0);
    }

    #[test]
    fn two_passes_stop_at_non_black_runs() {
        // One line: dark, dark, bright, bright, bright, dark.
        let v = [4, 4, 100, 100, 100, 6];
        let o = NearblackOptions { max_non_black: 0, set_alpha: true, ..Default::default() };
        let c = nearblack(&image(6, 1, &v), &o).unwrap();
        let out = values(&c);
        assert_eq!(&out[..6], &[0, 0, 100, 100, 100, 0]);
        assert_eq!(&out[6..], &[0, 0, 255, 255, 255, 0]);
        assert_eq!(c.dataset.band_color_interp(2), "Alpha");
    }

    #[test]
    fn colors_must_match_the_bands() {
        let o = NearblackOptions { colors: vec![vec![0, 0, 0]], ..Default::default() };
        assert!(nearblack(&image(2, 2, &[0; 4]), &o).is_err());
        assert_eq!(parse_color("white", 3).unwrap(), vec![255; 3]);
        assert_eq!(parse_color("1,2,3", 3).unwrap(), vec![1, 2, 3]);
        assert!(parse_color("1,a", 2).is_err());
    }
    #[test]
    fn alpha_and_mask_are_exclusive_for_both_core_paths() {
        let mut ds = image(2, 2, &[0; 4]);
        let o = NearblackOptions { set_alpha: true, set_mask: true, ..Default::default() };
        assert!(nearblack(&ds, &o).err().unwrap().to_string().contains("exclusive"));
        assert!(nearblack_in_place(&mut ds, &o).unwrap_err().to_string().contains("exclusive"));
    }
}
