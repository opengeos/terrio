//! Lookup-table contrast enhancement: an equivalent of `gdalenhance` (GDAL 3.12).
//!
//! Each band has a scale range (`ScaleMin`, `ScaleMax`) divided into `bins` bins (256 in GDAL)
//! and, usually, a lookup table giving the output value of each bin. A pixel's bin is
//! `(value - min) * bins / (max - min)`, truncated and clamped to the bins; its output is the
//! table's entry for it (the bin itself without a table). Pixels equal to the band's nodata value
//! become that value after output conversion. NaN nodata does not compare equal and uses
//! the first LUT bin, matching GDAL 3.13.3.
//!
//! - [`equalization_luts`] computes histogram-equalization tables as `gdalenhance -equalize`
//!   does, from the band's default histogram (`gdalinfo -hist`'s, see [`crate::info`]): the
//!   cumulative histogram, without its first and last buckets, scaled to the bins.
//! - [`parse_luts`] and [`format_luts`] read and write `gdalenhance -config` files: one line per
//!   band, `1:Band -0.5:ScaleMin 255.5:ScaleMax 0 0 1 ...`.
//! - [`linear_luts`] is a linear stretch of a given range to the bins, without tables (GDAL
//!   parses `-src_scale` but does not apply it; Terrio does).

use crate::info::{InfoOptions, raster_info};
use crate::{Error, LazyRaster, RasterDataset, Result, Window, WindowOptions};
use wbraster::DataType;
use wbraster::raster::RasterData;

/// The enhancement of one band.
#[derive(Debug, Clone, PartialEq)]
pub struct BandLut {
    /// The value at the start of the first bin.
    pub min: f64,
    /// The value at the end of the last bin.
    pub max: f64,
    /// The output value of each bin; `None` outputs the bin.
    pub lut: Option<Vec<i32>>,
}

/// Options for [`enhance`].
#[derive(Debug, Clone, PartialEq)]
pub struct EnhanceOptions {
    /// The number of bins (256 in GDAL).
    pub bins: usize,
    /// The output data type (`-ot`). GDAL writes only Byte; other types hold the same values.
    pub output_type: DataType,
}

impl Default for EnhanceOptions {
    fn default() -> Self {
        Self { bins: 256, output_type: DataType::U8 }
    }
}

fn check_bins(bins: usize) -> Result<()> {
    if bins == 0 || bins > 1 << 20 {
        return Err(Error::InvalidArgument("the number of bins must be from 1 to 1048576".into()));
    }
    Ok(())
}

/// The equalization table of a histogram (GDAL's `ComputeEqualizationLUTs`).
fn equalize(h: &crate::info::Histogram, bins: usize) -> BandLut {
    let mut counts = h.buckets.clone();
    let n = counts.len();
    if n > 0 {
        // The extremes (nodata, saturated values) are left out.
        counts[0] = 0;
        counts[n - 1] = 0;
    }
    let mut cumulative = vec![0u64; n];
    let mut total = 0u64;
    for (c, h) in cumulative.iter_mut().zip(&counts) {
        *c = total + h / 2;
        total += h;
    }
    let total = total.max(1);
    let lut = (0..bins)
        .map(|i| {
            let k = ((i as u64 * n as u64) / bins as u64) as usize;
            let v = (cumulative.get(k).copied().unwrap_or(0) as u128 * bins as u128 / total as u128) as i64;
            v.clamp(0, bins as i64 - 1) as i32
        })
        .collect();
    BandLut { min: h.min, max: h.max, lut: Some(lut) }
}

fn from_histograms(info: &crate::info::RasterInfo, bins: usize) -> Result<Vec<BandLut>> {
    check_bins(bins)?;
    info.bands
        .iter()
        .map(|b| {
            let h = b.histogram.as_ref().ok_or_else(|| Error::InvalidData(format!("band {} has no valid pixel to equalize", b.band)))?;
            Ok(equalize(h, bins))
        })
        .collect()
}

/// Histogram-equalization tables of every band of a raster held in memory, as
/// `gdalenhance -equalize` computes them (from `gdalinfo -hist`'s histogram).
pub fn equalization_luts(ds: &RasterDataset, bins: usize) -> Result<Vec<BandLut>> {
    if ds.complex {
        return Err(Error::Unsupported("enhancing complex bands is not supported".into()));
    }
    from_histograms(&raster_info(ds, &InfoOptions { statistics: false, histogram: true }), bins)
}

/// Histogram-equalization tables of every band of the raster at `path`, with GDAL's view of it:
/// a histogram or statistics stored in its `.aux.xml` sidecar, and the overviews or blocks
/// GDAL's approximate minimum and maximum use.
pub fn equalization_luts_from_path(path: &str, bins: usize) -> Result<Vec<BandLut>> {
    match crate::info::info_path(path, &InfoOptions { statistics: false, histogram: true })? {
        crate::info::Info::Raster(r) => from_histograms(&r, bins),
        _ => Err(Error::InvalidArgument(format!("{path} is not a raster"))),
    }
}

/// Linear tables: the range `min..max` of every band of `bands` stretched over the bins.
pub fn linear_luts(bands: usize, min: f64, max: f64) -> Result<Vec<BandLut>> {
    if !(min.is_finite() && max.is_finite() && max > min) {
        return Err(Error::InvalidArgument("the source range must be finite, its maximum above its minimum".into()));
    }
    Ok(vec![BandLut { min, max, lut: None }; bands])
}

/// The value of the start of `s` as C's `atof` reads it (`1.5:ScaleMin` is 1.5), 0 without one.
fn atof(s: &str) -> f64 {
    let end = s
        .char_indices()
        .take_while(|(i, c)| {
            c.is_ascii_digit()
                || matches!(c, '.' | 'e' | 'E')
                || (matches!(c, '+' | '-') && (*i == 0 || matches!(s.as_bytes()[i - 1], b'e' | b'E')))
        })
        .map(|(i, c)| i + c.len_utf8())
        .last()
        .unwrap_or(0);
    (0..=end).rev().find_map(|e| s[..e].parse::<f64>().ok()).unwrap_or(0.0)
}

/// The integer at the start of `s` as C's `atoi` reads it, 0 without one.
fn atoi(s: &str) -> i64 {
    let s = s.trim_start();
    let (sign, digits) = match s.as_bytes().first() {
        Some(b'-') => (-1, &s[1..]),
        Some(b'+') => (1, &s[1..]),
        _ => (1, s),
    };
    let n: String = digits.chars().take_while(char::is_ascii_digit).collect();
    sign * n.parse::<i64>().unwrap_or(0)
}

/// Reads a `gdalenhance -config` file for `bands` bands of `bins` bins (GDAL's `ReadLUTs`): a
/// line per band, its number, minimum and maximum, then at least `bins` table entries.
pub fn parse_luts(text: &str, bands: usize, bins: usize) -> Result<Vec<BandLut>> {
    check_bins(bins)?;
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() != bands {
        return Err(Error::InvalidData(format!("expected {bands} lines in the LUT file, got {}", lines.len())));
    }
    lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let tokens: Vec<&str> = line.split_whitespace().collect();
            if tokens.len() < bins + 3 || atoi(tokens[0]) != i as i64 + 1 {
                return Err(Error::InvalidData(format!("line {} of the LUT file seems to be corrupt", i + 1)));
            }
            Ok(BandLut {
                min: atof(tokens[1]),
                max: atof(tokens[2]),
                lut: Some(tokens[3..3 + bins].iter().map(|t| atoi(t) as i32).collect()),
            })
        })
        .collect()
}

/// Formats tables as `gdalenhance` writes its `-config` file (GDAL's `WriteLUTs`).
pub fn format_luts(luts: &[BandLut]) -> String {
    use crate::drivers::common::fmt_g;
    let mut s = String::new();
    for (i, l) in luts.iter().enumerate() {
        s.push_str(&format!("{}:Band {}:ScaleMin {}:ScaleMax ", i + 1, fmt_g(l.min, 6), fmt_g(l.max, 6)));
        for v in l.lut.iter().flatten() {
            s.push_str(&format!("{v} "));
        }
        s.push('\n');
    }
    s
}

/// Checks the tables against a raster of `bands` bands.
fn check(luts: &[BandLut], bands: usize, o: &EnhanceOptions) -> Result<()> {
    check_bins(o.bins)?;
    if luts.len() != bands {
        return Err(Error::InvalidArgument(format!("{} table(s) for {bands} band(s)", luts.len())));
    }
    if luts.iter().any(|l| l.lut.as_ref().is_some_and(|t| t.len() < o.bins)) {
        return Err(Error::InvalidArgument(format!("tables need {} entries", o.bins)));
    }
    Ok(())
}

/// The output's metadata (GDAL's virtual clone in `WriteEnhanced`): the input's georeferencing,
/// metadata, band descriptions and color interpretations, and its nodata values for bands of the
/// output type.
fn header(src: &RasterDataset, o: &EnhanceOptions) -> Result<RasterDataset> {
    let bands = src.band_count();
    let mut out = super::raster_header(super::Grid::of(&src.raster), o.output_type, bands, None, &src.crs())?;
    if let Some(gt) = src.affine {
        out.set_geo_transform(gt);
    }
    out.gcps = src.gcps.clone();
    out.raster.metadata = src.raster.metadata.iter().filter(|(k, _)| !k.starts_with("STATISTICS_")).cloned().collect();
    let same = src.raster.data_type == o.output_type;
    out.band_nodata = (1..=bands).map(|b| src.band_nodata(b).filter(|_| same)).collect();
    out.has_nodata = out.band_nodata.first().copied().flatten().is_some();
    if let Some(nd) = out.band_nodata.first().copied().flatten() {
        out.raster.nodata = nd;
    }
    out.band_metadata = (0..bands)
        .map(|b| {
            let m = src.band_metadata.get(b).cloned().unwrap_or_default();
            crate::BandMetadata { description: m.description, color_interp: m.color_interp, ..Default::default() }
        })
        .collect();
    Ok(out)
}

// Define the Float32-to-Byte conversion observed in GDAL 3.13.3 on the reference
// build: truncate through a signed 32-bit integer, then retain its low byte.
// C++ out-of-range float conversion is not portable; use zero explicitly outside
// that intermediate range, including infinities, rather than Rust's saturating cast.
fn byte_nodata(value: f32) -> u8 {
    let value = f64::from(value);
    if !value.is_finite() || value < i32::MIN as f64 || value >= (i32::MAX as f64 + 1.) { 0 } else { (value as i32) as u8 }
}

/// Enhances the bands of `strip` (all the input's bands, in order), in the output type.
fn apply(strip: &RasterDataset, nodata: &[Option<f64>], luts: &[BandLut], o: &EnhanceOptions) -> RasterData {
    use crate::pixel::Pixel;
    let n = strip.raster.cols * strip.raster.rows;
    let mut data = RasterData::new_filled(o.output_type, n * luts.len(), 0.0);
    let bins = o.bins as i64;
    for (b, l) in luts.iter().enumerate() {
        let src = crate::ops::BandView::of(&strip.raster, b);
        let nd = nodata[b].map(|v| v as f32);
        let scale = o.bins as f64 / (l.max - l.min);
        let byte = o.output_type == DataType::U8;
        crate::pixel::with_data!(&mut data, d => {
            for i in 0..n {
                let original = src.get(i);
                let v = original as f32;
                let is_nodata = if byte { nd == Some(v) } else { nodata[b] == Some(original) };
                let value = if is_nodata {
                    if byte { f64::from(byte_nodata(v)) } else { original }
                } else {
                    let bin = (f64::from(v) - l.min) * scale;
                    // GDAL's tests, NaN included: not above 0 is bin 0, not below the last bin the last.
                    let bin = if bin.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
                        0
                    } else if bin.partial_cmp(&((bins - 1) as f64)) != Some(std::cmp::Ordering::Less) {
                        bins - 1
                    } else {
                        bin as i64
                    };
                    let out = l.lut.as_ref().map_or(bin, |t| t[bin as usize] as i64);
                    // GDAL casts the table's value to a byte.
                    if byte { f64::from(out as u8) } else { out as f64 }
                };
                d[b * n + i] = Pixel::from_f64(super::convert_value(value, o.output_type));
            }
        });
    }
    data
}

/// Enhances a raster held in memory with a table per band, as `gdalenhance` does.
///
/// ```no_run
/// use terrio::ops::enhance::{EnhanceOptions, enhance, equalization_luts};
///
/// let image = terrio::open_raster("image.tif")?;
/// let luts = equalization_luts(&image, 256)?;
/// let out = enhance(&image, &luts, &EnhanceOptions::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn enhance(ds: &RasterDataset, luts: &[BandLut], o: &EnhanceOptions) -> Result<RasterDataset> {
    if ds.complex {
        return Err(Error::Unsupported("enhancing complex bands is not supported".into()));
    }
    check(luts, ds.band_count(), o)?;
    let mut out = header(ds, o)?;
    let nodata: Vec<Option<f64>> = (1..=ds.band_count()).map(|b| ds.band_nodata(b)).collect();
    out.raster.data = apply(ds, &nodata, luts, o);
    Ok(out)
}

/// Enhances a raster and writes it to `path`, reading the input and writing a GeoTIFF or COG a
/// strip of rows at a time (other formats are written whole). The values are those of
/// [`enhance`]. `path` must not be the file `src` reads.
pub fn enhance_to_path(
    src: &mut LazyRaster,
    luts: &[BandLut],
    o: &EnhanceOptions,
    path: &str,
    options: &crate::RasterWriteOptions,
) -> Result<()> {
    let h0 = src.header();
    if h0.complex {
        return Err(Error::Unsupported("enhancing complex bands is not supported".into()));
    }
    let bands = src.band_count();
    check(luts, bands, o)?;
    let (w, h) = (src.width(), src.height());
    let nodata: Vec<Option<f64>> = (1..=bands).map(|b| h0.band_nodata(b)).collect();
    let rows = super::strips::strip_rows(
        w,
        bands * 2,
        src.data_type().size_bytes().max(o.output_type.size_bytes()),
        src.block_size().map(|b| b.1),
    );
    let mut out = super::strips::StripWriter::create(&header(&h0, o)?, path, options)?;
    let read = WindowOptions::default();
    super::strips::pipelined(
        h.div_ceil(rows),
        |i| {
            let (y0, y1) = (i * rows, ((i + 1) * rows).min(h));
            src.read_window(Window::new(0, y0, w, y1 - y0), &read)
        },
        |_, strip| {
            let n = strip.raster.rows;
            out.write(apply(&strip, &nodata, luts, o), n)
        },
    )?;
    out.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_files_round_trip_as_gdal_writes_them() {
        let luts = vec![BandLut { min: -0.5, max: 255.5, lut: Some((0..4).collect()) }];
        let text = format_luts(&luts);
        assert_eq!(text, "1:Band -0.5:ScaleMin 255.5:ScaleMax 0 1 2 3 \n");
        assert_eq!(parse_luts(&text, 1, 4).unwrap(), luts);
        assert!(parse_luts(&text, 2, 4).is_err());
        assert!(parse_luts(&text, 1, 5).is_err());
        assert!(parse_luts("2:Band 0:ScaleMin 1:ScaleMax 0 0 0 0", 1, 4).is_err());
    }

    #[test]
    fn equalization_follows_the_cumulative_histogram() {
        let h = crate::info::Histogram { count: 4, min: 0.0, max: 4.0, buckets: vec![9, 2, 2, 9] };
        // Extremes left out: cumulative 0, 1, 3, 4 of 4.
        let l = equalize(&h, 4);
        assert_eq!(l.lut.unwrap(), vec![0, 1, 3, 3]);
    }

    #[test]
    fn nan_nodata_uses_first_lut_bin_like_gdal_313() {
        let grid = super::super::Grid { width: 1, height: 1, geo_transform: [0., 1., 0., 1., 0., -1.] };
        let ds = super::super::build_raster_data(grid, RasterData::F32(vec![f32::NAN]), 1, Some(f64::NAN), &Default::default()).unwrap();
        let luts = vec![BandLut { min: 0., max: 8., lut: Some((0..256).rev().collect()) }];
        let out = enhance(&ds, &luts, &EnhanceOptions::default()).unwrap();
        assert_eq!(out.raster.data.to_f64_vec(), vec![255.]);
    }

    #[test]
    fn byte_lut_values_wrap_like_gdal_313() {
        let grid = super::super::Grid { width: 1, height: 1, geo_transform: [0., 1., 0., 1., 0., -1.] };
        let ds = super::super::build_raster_data(grid, RasterData::U8(vec![1]), 1, None, &Default::default()).unwrap();
        let luts = vec![BandLut { min: 0., max: 16., lut: Some(vec![300; 256]) }];
        assert_eq!(enhance(&ds, &luts, &EnhanceOptions::default()).unwrap().raster.data.to_f64_vec(), vec![44.]);
    }

    #[test]
    fn byte_nodata_conversion_matches_gdal_313_reference_build() {
        for (nodata, expected) in [(-9999., 241.), (300., 44.), (1e30, 0.), (f64::INFINITY, 0.)] {
            let grid = super::super::Grid { width: 2, height: 1, geo_transform: [0., 1., 0., 1., 0., -1.] };
            let ds =
                super::super::build_raster_data(grid, RasterData::F64(vec![nodata, 128.]), 1, Some(nodata), &Default::default()).unwrap();
            let luts = linear_luts(1, -0.5, 255.5).unwrap();
            let out = enhance(&ds, &luts, &EnhanceOptions::default()).unwrap();
            assert_eq!(out.raster.data.to_f64_vec(), vec![expected, 128.]);
        }
    }

    #[test]
    fn float64_output_preserves_exact_nodata_without_widening_its_match() {
        let nodata = 1.0000000001;
        let grid = super::super::Grid { width: 2, height: 1, geo_transform: [0., 1., 0., 1., 0., -1.] };
        let ds = super::super::build_raster_data(grid, RasterData::F64(vec![nodata, 1.]), 1, Some(nodata), &Default::default()).unwrap();
        let luts = vec![BandLut { min: 0., max: 256., lut: Some(vec![42; 256]) }];
        let out = enhance(&ds, &luts, &EnhanceOptions { output_type: DataType::F64, ..Default::default() }).unwrap();
        assert_eq!(out.raster.data.to_f64_vec(), vec![nodata, 42.]);
        assert_eq!(out.band_nodata(1), Some(nodata));
    }

    #[test]
    fn values_map_to_bins_and_nodata_stays() {
        let grid = super::super::Grid { width: 4, height: 1, geo_transform: [0., 1., 0., 1., 0., -1.] };
        let ds = super::super::build_raster_data(grid, RasterData::U8(vec![0, 10, 200, 255]), 1, Some(10.0), &Default::default()).unwrap();
        let luts = linear_luts(1, -0.5, 255.5).unwrap();
        let out = enhance(&ds, &luts, &EnhanceOptions::default()).unwrap();
        assert_eq!(out.raster.data.to_f64_vec(), vec![0., 10., 200., 255.]);
        assert_eq!(out.band_nodata(1), Some(10.0));
        assert!((atof("-0.5:ScaleMin") + 0.5).abs() < 1e-15 && atof("1e3x") == 1000.0 && atoi("12:Band") == 12);
    }
}
