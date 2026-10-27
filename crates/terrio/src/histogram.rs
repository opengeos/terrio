//! Band histograms as `gdalinfo -hist` computes them (`GDALRasterBand::GetDefaultHistogram()`).
//!
//! - Byte bands: 256 buckets from -0.5 to 255.5; Int8 bands: from -128.5 to 127.5.
//! - Other types: 256 buckets over the band's minimum and maximum, widened by half a bucket on
//!   each side (one bucket from `value - 0.5` to `value + 0.5` when they are equal). The minimum
//!   and maximum are GDAL's approximate statistics (`GetStatistics(bApproxOK=TRUE)`): those stored
//!   with the dataset (`STATISTICS_MINIMUM`/`MAXIMUM` in the GeoTIFF's metadata or the `.aux.xml`
//!   sidecar, or the format's own minimum and maximum), else those of the smallest overview of
//!   more than 2500 pixels, else those of a sample of the blocks: one every
//!   `sqrt(blocks per row × blocks per column)`.
//! - Every pixel is then counted (nodata and NaN excluded), values outside the range in the first
//!   or last bucket.
//!
//! A histogram already stored in the `.aux.xml` sidecar (GDAL saves those it computes) is used as
//! it is, as GDAL uses it.

use crate::RasterDataset;
use wbraster::DataType;

/// A band's histogram.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Histogram {
    /// The number of buckets.
    pub count: usize,
    /// The lower bound of the first bucket.
    #[serde(serialize_with = "json_c_double")]
    pub min: f64,
    /// The upper bound of the last bucket.
    #[serde(serialize_with = "json_c_double")]
    pub max: f64,
    /// The pixel count of each bucket.
    pub buckets: Vec<u64>,
}

/// Serializes a double as GDAL's JSON (json-c) does: `%.17g`, with `.0` after integers.
fn json_c_double<S: serde::Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
    use serde::Serialize;
    if !v.is_finite() {
        return s.serialize_f64(*v);
    }
    let mut text = crate::drivers::common::fmt_g(*v, 17);
    if !text.contains(['.', 'e']) {
        text.push_str(".0");
    }
    match serde_json::value::RawValue::from_string(text) {
        Ok(raw) => raw.serialize(s),
        Err(_) => s.serialize_f64(*v),
    }
}

impl Histogram {
    /// The histogram as `gdalinfo -hist` prints it: `  256 buckets from -0.5 to 255.5:` and a
    /// line of counts, each followed by a space.
    pub fn to_text(&self) -> String {
        let mut s = format!(
            "  {} buckets from {} to {}:\n  ",
            self.count,
            crate::drivers::common::fmt_g(self.min, 6),
            crate::drivers::common::fmt_g(self.max, 6)
        );
        for b in &self.buckets {
            s.push_str(&b.to_string());
            s.push(' ');
        }
        s.push('\n');
        s
    }
}

/// What GDAL knows of a band besides its pixels, for its default histogram.
#[derive(Debug, Clone, Default)]
pub(crate) struct BandContext {
    /// A histogram stored in the `.aux.xml` sidecar.
    pub stored_histogram: Option<Histogram>,
    /// The minimum and maximum stored with the dataset or known by the format.
    pub stored_range: Option<(f64, f64)>,
    /// The band's block size `(width, height)` as GDAL reads it (`None`: one block).
    pub block: Option<(usize, usize)>,
    /// The pixels of the overview GDAL computes approximate statistics on, when there is one.
    pub overview: Option<Vec<f64>>,
}

/// Whether a pixel counts: not NaN and not nodata, compared as GDAL compares them (exactly for
/// integers, within float precision for floats, `GDALNoDataValues`).
pub(crate) fn is_valid(v: f64, nodata: Option<f64>, t: DataType) -> bool {
    if v.is_nan() {
        return false;
    }
    let Some(nd) = nodata else { return true };
    match t {
        DataType::F32 => !are_real_equal_f32(v as f32, nd as f32),
        DataType::F64 => !are_real_equal(v, nd),
        _ => v != nd,
    }
}

/// GDAL's `ARE_REAL_EQUAL` for floats (2 ulps of `float` epsilon).
fn are_real_equal_f32(a: f32, b: f32) -> bool {
    a == b || (a - b).abs() < f32::EPSILON * (a + b).abs() * 2.0
}

/// GDAL's `ARE_REAL_EQUAL` for doubles (with `float` epsilon).
fn are_real_equal(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() < f32::EPSILON as f64 * (a + b).abs() * 2.0
}

/// The minimum and maximum of the valid values. `None` when there are none.
fn min_max(values: impl Iterator<Item = f64>, nodata: Option<f64>, t: DataType) -> Option<(f64, f64)> {
    let (mut lo, mut hi, mut n) = (f64::INFINITY, f64::NEG_INFINITY, 0usize);
    for v in values {
        if is_valid(v, nodata, t) {
            lo = lo.min(v);
            hi = hi.max(v);
            n += 1;
        }
    }
    (n > 0).then_some((lo, hi))
}

/// GDAL's approximate minimum and maximum of a band without overviews: the pixels of one block in
/// every `nSampleRate` (`ComputeStatistics(bApproxOK=TRUE)`).
fn sampled_min_max(
    values: &[f64],
    cols: usize,
    rows: usize,
    block: (usize, usize),
    nodata: Option<f64>,
    t: DataType,
) -> Option<(f64, f64)> {
    let (bw, bh) = (block.0.max(1), block.1.max(1));
    let per_row = cols.div_ceil(bw);
    let per_col = rows.div_ceil(bh);
    let mut rate = ((per_row as f64 * per_col as f64).sqrt() as usize).max(1);
    // GDAL avoids probing only the first column of blocks of a square raster.
    if rate == per_row && per_row > 1 {
        rate += 1;
    }
    let blocks = (0..per_row * per_col).step_by(rate);
    let pixels = blocks.flat_map(|i| {
        let (bx, by) = (i % per_row, i / per_row);
        let (x0, y0) = (bx * bw, by * bh);
        let (w, h) = ((cols - x0).min(bw), (rows - y0).min(bh));
        (y0..y0 + h).flat_map(move |y| values[y * cols + x0..y * cols + x0 + w].iter().copied())
    });
    min_max(pixels, nodata, t)
}

/// A band's default histogram (`GetDefaultHistogram()`), from its pixels and what GDAL knows of it.
/// `exact` is the band's exact minimum and maximum when statistics were computed first (as
/// `gdalinfo -stats -hist` does). `None` when the range cannot be computed (no valid pixel).
pub(crate) fn default_histogram(ds: &RasterDataset, band: usize, ctx: &BandContext, exact: Option<(f64, f64)>) -> Option<Histogram> {
    if let Some(h) = &ctx.stored_histogram {
        return Some(h.clone());
    }
    let r = &ds.raster;
    let t = r.data_type;
    let nodata = ds.band_nodata(band + 1);
    let values = crate::ops::band_f64(r, band);
    let mut buckets = 256usize;
    let (min, max) = match t {
        DataType::U8 => (-0.5, 255.5),
        DataType::I8 => (-128.5, 127.5),
        _ => {
            // Statistics just computed are kept as GDAL keeps them: as `%.14g` text.
            let round = |v: f64| crate::drivers::common::fmt_g(v, 14).parse().unwrap_or(v);
            let exact = exact.map(|(lo, hi)| (round(lo), round(hi)));
            let (lo, hi) = match (ctx.stored_range.or(exact), &ctx.overview) {
                (Some(range), _) => range,
                (None, Some(overview)) => min_max(overview.iter().copied(), nodata, t)?,
                (None, None) => match ctx.block {
                    Some(block) => sampled_min_max(&values, r.cols, r.rows, block, nodata, t)?,
                    None => min_max(values.iter().copied(), nodata, t)?,
                },
            };
            if lo == hi {
                buckets = 1;
                (lo - 0.5, hi + 0.5)
            } else {
                let half = (hi - lo) / (2.0 * (buckets - 1) as f64);
                (lo - half, hi + half)
            }
        }
    };
    Some(Histogram { count: buckets, min, max, buckets: count(&values, nodata, t, min, max, buckets) })
}

/// Counts the valid values in `n` buckets from `min` to `max`, those outside in the first or last
/// bucket (`GetHistogram(bIncludeOutOfRange=TRUE)`).
pub(crate) fn count(values: &[f64], nodata: Option<f64>, t: DataType, min: f64, max: f64, n: usize) -> Vec<u64> {
    let mut out = vec![0u64; n];
    let scale = n as f64 / (max - min);
    if max.partial_cmp(&min) != Some(std::cmp::Ordering::Greater) || !scale.is_finite() || scale == 0.0 {
        return out;
    }
    for &v in values {
        if !is_valid(v, nodata, t) {
            continue;
        }
        let i = ((v - min) * scale).floor();
        let i = if i < 0.0 {
            0
        } else if i >= n as f64 {
            n - 1
        } else {
            i as usize
        };
        out[i] += 1;
    }
    out
}

/// The first histogram stored for a band (numbered from 1) in a dataset's `.aux.xml` sidecar.
pub(crate) fn pam_histogram(path: &str, band: usize) -> Option<Histogram> {
    let xml = std::fs::read_to_string(format!("{path}.aux.xml")).ok()?;
    let start = xml.find(&format!("<PAMRasterBand band=\"{band}\""))?;
    let block = &xml[start..];
    let block = &block[..block.find("</PAMRasterBand>").unwrap_or(block.len())];
    let item = &block[block.find("<HistItem>")?..];
    let item = &item[..item.find("</HistItem>")?];
    let element = |name: &str| -> Option<&str> {
        let open = format!("<{name}>");
        let i = item.find(&open)? + open.len();
        Some(item[i..i + item[i..].find('<')?].trim())
    };
    let min: f64 = element("HistMin")?.parse().ok()?;
    let max: f64 = element("HistMax")?.parse().ok()?;
    let count: usize = element("BucketCount")?.parse().ok()?;
    let buckets: Vec<u64> = element("HistCounts")?.split('|').filter_map(|c| c.trim().parse().ok()).collect();
    (buckets.len() == count).then_some(Histogram { count, min, max, buckets })
}

/// The minimum and maximum of a band (numbered from 1) stored with a dataset, in GDAL's order:
/// `STATISTICS_MINIMUM`/`MAXIMUM` metadata (the `.aux.xml` sidecar, then a GeoTIFF's
/// `GDAL_METADATA`), then what the format records itself (an EHdr `.stx` file, an Idrisi `.rdc`
/// header, a Surfer grid's Z range).
pub(crate) fn stored_range(ds: &RasterDataset, path: &str, band: usize) -> Option<(f64, f64)> {
    use crate::formats::RasterDriver as D;
    if let Some(r) = crate::ops::dem::pam_statistics(path, band) {
        return Some(r);
    }
    let p = std::path::Path::new(path);
    match ds.driver {
        D::GTiff | D::Cog => {
            let tags = crate::geotiff::read_geo_tags(p)?;
            let get = |k: &str| -> Option<f64> {
                tags.band_metadata.iter().find(|(b, n, _)| *b + 1 == band && n == k).and_then(|(_, _, v)| v.trim().parse().ok())
            };
            Some((get("STATISTICS_MINIMUM")?, get("STATISTICS_MAXIMUM")?))
        }
        D::EHdr => {
            // `band min max [mean stddev]` lines.
            let stx = std::fs::read_to_string(p.with_extension("stx")).ok()?;
            stx.lines().find_map(|l| {
                let v: Vec<f64> = l.split_whitespace().filter_map(|t| t.parse().ok()).collect();
                (v.len() >= 3 && v[0] as usize == band).then(|| (v[1], v[2]))
            })
        }
        D::RST => {
            let rdc = std::fs::read_to_string(p.with_extension("rdc")).ok()?;
            let field = |key: &str| -> Option<f64> {
                let line = rdc.lines().find(|l| l.split(':').next().is_some_and(|k| k.trim() == key))?;
                line.split_once(':')?.1.split_whitespace().nth(band - 1)?.parse().ok()
            };
            Some((field("min. value")?, field("max. value")?))
        }
        D::GSBG => {
            let mut header = [0u8; 56];
            std::io::Read::read_exact(&mut std::fs::File::open(p).ok()?, &mut header).ok()?;
            (&header[..4] == b"DSBB").then(|| {
                let f = |o: usize| f64::from_le_bytes(header[o..o + 8].try_into().unwrap_or_default());
                (f(40), f(48))
            })
        }
        _ => None,
    }
}

/// Which overview GDAL computes approximate statistics on (`GetRasterSampleOverview(2500)`): the
/// smallest of more than 2500 pixels, among `(width, height)` sizes. `None` for full resolution.
pub(crate) fn sample_overview(width: usize, height: usize, overviews: &[(usize, usize)]) -> Option<usize> {
    let mut best = width as f64 * height as f64;
    let mut chosen = None;
    for (i, (w, h)) in overviews.iter().enumerate() {
        let n = *w as f64 * *h as f64;
        if n < best && n > 2500.0 {
            best = n;
            chosen = Some(i);
        }
    }
    chosen
}

/// The block size GDAL reads a raster of terrio's own drivers with: one row for the formats GDAL
/// reads by scanline (and for netCDF, as GDAL reads classic files and netCDF-4 files chunked by
/// row, GDAL's default). `None` when the whole raster is one block for this purpose.
pub(crate) fn driver_block(ds: &RasterDataset) -> Option<(usize, usize)> {
    use crate::formats::RasterDriver as D;
    match ds.driver {
        D::AAIGrid
        | D::EHdr
        | D::ENVI
        | D::SAGA
        | D::ERS
        | D::RST
        | D::GSBG
        | D::XYZ
        | D::PNG
        | D::GRASSASCIIGrid
        | D::NetCDF
        | D::SRTMHGT => Some((ds.raster.cols, 1)),
        D::HFA => Some((64, 64)),
        D::GPKG => Some((256, 256)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::common;
    use wbraster::CrsInfo;
    use wbraster::raster::RasterData;

    fn raster(data: RasterData, cols: usize, rows: usize, nodata: Option<f64>) -> RasterDataset {
        let r = common::make_raster(cols, rows, 1, data, [0.0, 1.0, 0.0, 0.0, 0.0, -1.0], CrsInfo::default(), nodata).unwrap();
        RasterDataset::with_driver(r, crate::formats::RasterDriver::GTiff, nodata.is_some())
    }

    #[test]
    fn byte_histograms_cover_the_type() {
        let ds = raster(RasterData::U8(vec![0, 1, 1, 255, 7, 7]), 3, 2, Some(7.0));
        let h = default_histogram(&ds, 0, &BandContext::default(), None).unwrap();
        assert_eq!((h.count, h.min, h.max), (256, -0.5, 255.5));
        assert_eq!((h.buckets[0], h.buckets[1], h.buckets[7], h.buckets[255]), (1, 2, 0, 1));
        assert_eq!(h.buckets.iter().sum::<u64>(), 4);
    }

    #[test]
    fn other_types_widen_the_range_by_half_a_bucket() {
        let ds = raster(RasterData::I16(vec![10, 20, 30, 265, -32768, 10]), 3, 2, Some(-32768.0));
        let h = default_histogram(&ds, 0, &BandContext::default(), None).unwrap();
        assert_eq!(h.count, 256);
        assert_eq!((h.min, h.max), (9.5, 265.5));
        assert_eq!((h.buckets[0], h.buckets[10], h.buckets[255]), (2, 1, 1));
        let flat = raster(RasterData::F32(vec![2.5; 4]), 2, 2, None);
        let h = default_histogram(&flat, 0, &BandContext::default(), None).unwrap();
        assert_eq!((h.count, h.min, h.max, h.buckets.clone()), (1, 2.0, 3.0, vec![4]));
        assert_eq!(h.to_text(), "  1 buckets from 2 to 3:\n  4 \n");
    }

    #[test]
    fn approximate_ranges_sample_blocks() {
        // 4 rows of one-row blocks: every second row is sampled (rate 2).
        let ds = raster(RasterData::F32(vec![1.0, 2.0, 100.0, 200.0, 3.0, 4.0, 300.0, 400.0]), 2, 4, None);
        let ctx = BandContext { block: Some((2, 1)), ..Default::default() };
        let h = default_histogram(&ds, 0, &ctx, None).unwrap();
        let half = 3.0 / 510.0;
        assert_eq!((h.min, h.max), (1.0 - half, 4.0 + half));
        // Values beyond the range fall in the last bucket.
        assert_eq!(h.buckets[255], 5);
        assert_eq!(sample_overview(1000, 1000, &[(500, 500), (250, 250), (40, 40), (20, 20)]), Some(1));
        assert_eq!(sample_overview(40, 40, &[(20, 20)]), None);
    }

    #[test]
    fn json_numbers_as_gdal_prints_them() {
        let h = Histogram { count: 1, min: 365.2274509803922, max: 2.0, buckets: vec![3] };
        assert_eq!(serde_json::to_string(&h).unwrap(), r#"{"count":1,"min":365.22745098039218,"max":2.0,"buckets":[3]}"#);
    }
}
