//! Pixel-wise band operations: [`unscale`] (`gdal raster unscale`), [`nodata_to_alpha`]
//! (`gdal raster nodata-to-alpha`) and [`reclassify`] (`gdal raster reclassify`).
//!
//! The outputs keep the source's size, georeferencing (rotated geotransforms and GCPs included),
//! CRS and dataset metadata. Values are converted to the output type as GDAL's virtual rasters
//! convert them.

use super::convert_value;
use crate::{Error, RasterDataset, Result};
use wbraster::DataType;

/// Options for [`unscale`].
#[derive(Debug, Clone, Default)]
pub struct UnscaleOptions {
    /// The output data type (`--ot`). By default `Float32`, or `Float64` for `Float64` sources.
    pub output_type: Option<DataType>,
}

/// GDAL's `ARE_REAL_EQUAL` at single precision: equal, or within two float epsilons relative to
/// the magnitude.
#[inline]
fn real_equal32(a: f32, b: f32) -> bool {
    a == b || (a - b).abs() < f32::EPSILON * (a + b).abs() * 2.0
}

#[inline]
fn real_equal64(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() < f32::EPSILON as f64 * (a + b).abs() * 2.0
}

/// Whether GDAL's virtual raster sources compute in double precision: for 32- and 64-bit integer
/// and `Float64` outputs, and 32- and 64-bit integer sources (single precision otherwise).
fn works_in_f64(src: DataType, dst: DataType) -> bool {
    let wide = |t: DataType| matches!(t, DataType::I32 | DataType::U32 | DataType::I64 | DataType::U64);
    wide(src) || wide(dst) || dst == DataType::F64
}

/// Converts a working value to the output type as GDAL's virtual raster sources do: Byte by
/// adding 0.5 in single precision and truncating, other types by rounding and clamping.
#[inline]
fn output_value(v: f64, dst: DataType) -> f64 {
    if dst == DataType::U8 {
        let f = (v as f32 + 0.5).clamp(0.0, 255.0);
        return f.trunc() as f64;
    }
    convert_value(v, dst)
}

/// Applies each band's scale and offset (`value * scale + offset`), as `gdal raster unscale`
/// (`gdal_translate -unscale`) does. Nodata pixels keep the band's nodata value, which the output
/// declares unchanged; the output has no scale and offset. Bands without them are converted
/// unchanged.
pub fn unscale(ds: &RasterDataset, o: &UnscaleOptions) -> Result<RasterDataset> {
    if ds.complex {
        return Err(Error::Unsupported("unscaling complex bands".into()));
    }
    let src = ds.raster.data_type;
    let dst = o.output_type.unwrap_or(if src == DataType::F64 { DataType::F64 } else { DataType::F32 });
    let n = ds.raster.cols * ds.raster.rows;
    let nbands = ds.raster.bands;
    let wide = works_in_f64(src, dst);
    let bands: Vec<Vec<f64>> = (0..nbands)
        .map(|b| {
            let (scale, offset) = band_scale_offset(ds, b + 1);
            let nodata = ds.band_nodata(b + 1);
            let view = super::BandView::of(&ds.raster, b);
            let fill = nodata.map(|nd| convert_value(nd, dst));
            crate::par::map_range(n, |i| {
                let v = view.get(i);
                if wide {
                    if let (Some(nd), Some(f)) = (nodata, fill)
                        && (if nd.is_nan() { v.is_nan() } else { real_equal64(v, nd) })
                    {
                        return f;
                    }
                    output_value(v * scale + offset, dst)
                } else {
                    let v = v as f32;
                    if let (Some(nd), Some(f)) = (nodata, fill)
                        && (if nd.is_nan() { v.is_nan() } else { real_equal32(v, nd as f32) })
                    {
                        return f;
                    }
                    output_value((v as f64 * scale + offset) as f32 as f64, dst)
                }
            })
        })
        .collect();
    let data = super::bands_data(&bands, n, dst, |v, _| v);
    let mut out = super::derived(ds, data, nbands)?;
    let packing = |k: &str| k.starts_with(crate::info::INTERNAL) && (k.ends_with("_SCALE") || k.ends_with("_OFFSET"));
    out.raster.metadata.retain(|(k, _)| !packing(k));
    out.band_metadata = (0..nbands)
        .map(|b| {
            let mut m = ds.band_metadata.get(b).cloned().unwrap_or_default();
            m.scale = None;
            m.offset = None;
            m
        })
        .collect();
    copy_nodata(ds, &mut out);
    Ok(out)
}

/// The scale and offset of a band (from 1): its properties, or the packing a driver recorded
/// (netCDF's `scale_factor` and `add_offset`), 1 and 0 by default.
pub fn band_scale_offset(ds: &RasterDataset, band: usize) -> (f64, f64) {
    let meta = ds.band_metadata.get(band - 1);
    let internal = |what: &str| {
        let key = format!("{}BAND_{band}_{what}", crate::info::INTERNAL);
        ds.raster.metadata.iter().find(|(k, _)| *k == key).and_then(|(_, v)| v.trim().parse::<f64>().ok())
    };
    let scale = meta.and_then(|m| m.scale).or_else(|| internal("SCALE")).unwrap_or(1.0);
    let offset = meta.and_then(|m| m.offset).or_else(|| internal("OFFSET")).unwrap_or(0.0);
    (scale, offset)
}

/// Gives `out` the nodata values of `ds`'s bands.
fn copy_nodata(ds: &RasterDataset, out: &mut RasterDataset) {
    out.has_nodata = ds.nodata().is_some();
    out.raster.nodata = ds.nodata().unwrap_or(out.raster.nodata);
    out.band_nodata = if ds.band_nodata.is_empty() { Vec::new() } else { (1..=ds.raster.bands).map(|b| ds.band_nodata(b)).collect() };
}

/// Options for [`nodata_to_alpha`].
#[derive(Debug, Clone, Default)]
pub struct NodataToAlphaOptions {
    /// Nodata values overriding the source's (`--nodata`): one for every band, or one per band.
    /// With one per band, a pixel is transparent when every band has its nodata value.
    pub nodata: Vec<f64>,
}

/// Replaces nodata values by an alpha band, as `gdal raster nodata-to-alpha` does: the bands are
/// copied without nodata values and followed by an alpha band of their type, 0 where the first
/// band's mask is 0 (its nodata value, or with several `nodata` values, where every band has its
/// value) and 255 elsewhere. A raster whose first band has no nodata value, or that already ends
/// with an alpha band, is returned unchanged.
pub fn nodata_to_alpha(ds: &RasterDataset, o: &NodataToAlphaOptions) -> Result<RasterDataset> {
    if ds.complex {
        return Err(Error::Unsupported("alpha bands for complex bands".into()));
    }
    let r = &ds.raster;
    let nbands = r.bands;
    let n = r.cols * r.rows;
    if nbands == 0 {
        return Err(Error::InvalidArgument("the raster has no bands".into()));
    }
    let nodata_mask = |band: usize, nd: f64| super::sieve::nodata_mask(&super::band_f64(r, band), nd, r.data_type);
    let valid: Vec<bool> = match o.nodata.len() {
        0 => {
            let ends_with_alpha = matches!(nbands, 2 | 4) && ds.band_color_interp(nbands).eq_ignore_ascii_case("Alpha");
            match ds.band_nodata(1) {
                Some(nd) if !ends_with_alpha => nodata_mask(0, nd),
                _ => return Ok(ds.clone()),
            }
        }
        1 => nodata_mask(0, o.nodata[0]),
        k if k == nbands => {
            let masks: Vec<Vec<bool>> = (0..nbands).map(|b| nodata_mask(b, o.nodata[b])).collect();
            (0..n).map(|i| masks.iter().any(|m| m[i])).collect()
        }
        k => {
            return Err(Error::InvalidArgument(format!("{k} nodata values for {nbands} bands: give one value, or one per band")));
        }
    };
    use wbraster::raster::RasterData as D;
    // Int8 alpha saturates at 127, as GDAL converts its Byte mask.
    let on = convert_value(255.0, r.data_type);
    macro_rules! with_alpha {
        ($($var:ident: $t:ty),*) => {
            match &r.data {
                $(D::$var(v) => {
                    let mut out = Vec::with_capacity(n * (nbands + 1));
                    out.extend_from_slice(&v[..n * nbands]);
                    out.extend(valid.iter().map(|ok| if *ok { on as $t } else { 0 as $t }));
                    D::$var(out)
                })*
            }
        };
    }
    let data = with_alpha!(U8: u8, I8: i8, U16: u16, I16: i16, U32: u32, I32: i32, U64: u64, I64: i64, F32: f32, F64: f64);
    let mut out = super::derived(ds, data, nbands + 1)?;
    out.band_metadata = (0..nbands).map(|b| ds.band_metadata.get(b).cloned().unwrap_or_default()).collect();
    if out.band_metadata.iter().all(|m| *m == Default::default()) {
        // Keep the drivers' default color interpretations of the bands.
        for (b, m) in out.band_metadata.iter_mut().enumerate() {
            m.color_interp = Some(ds.band_color_interp(b + 1).to_string());
        }
    }
    out.band_metadata.push(crate::BandMetadata { color_interp: Some("Alpha".into()), ..Default::default() });
    Ok(out)
}

/// A value read by `strtod` from the start of `s`: the value and the number of bytes read, or
/// `None` when `s` does not start with a number (after white space).
fn strtod(s: &str) -> Option<(f64, usize)> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() && b[i].is_ascii_whitespace() {
        i += 1;
    }
    let start = i;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    for word in ["infinity", "inf", "nan"] {
        if b.get(i..i + word.len()).is_some_and(|p| p.eq_ignore_ascii_case(word.as_bytes())) {
            let end = i + word.len();
            let neg = b[start] == b'-';
            let v = if word == "nan" {
                f64::NAN
            } else if neg {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            };
            return Some((v, end));
        }
    }
    let digits_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    let mut digits = i > digits_start;
    if i < b.len() && b[i] == b'.' {
        i += 1;
        let frac = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        digits |= i > frac;
    }
    if !digits {
        return None;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut j = i + 1;
        if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
            j += 1;
        }
        let exp = j;
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp {
            i = j;
        }
    }
    s[start..i].parse::<f64>().ok().map(|v| (v, i))
}

/// Whether a value is exactly representable in a data type, as GDAL's `GDALIsValueExactAs`.
fn exact_as(v: f64, dt: DataType) -> bool {
    match dt {
        DataType::F64 => true,
        DataType::F32 => v.is_nan() || v as f32 as f64 == v,
        _ => {
            let (lo, hi) = super::type_range(dt);
            !v.is_nan() && v >= lo && v <= hi && v.fract() == 0.0 && !(dt == DataType::U64 && v >= hi) && !(dt == DataType::I64 && v >= hi)
        }
    }
}

/// A closed interval of source values (open bounds are moved to the next representable value).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Interval {
    min: f64,
    max: f64,
}

impl Interval {
    fn contains(&self, x: f64) -> bool {
        x >= self.min && x <= self.max
    }
}

/// Parses an interval (`[a,b)`, `(a,b]`, `(-inf,b]`...) or a single value at the start of `s`,
/// returning it and the rest of `s`.
fn parse_interval(s: &str) -> Result<(Interval, &str)> {
    let t = s.trim_start();
    let min_included = match t.as_bytes().first() {
        Some(b'(') => false,
        Some(b'[') => true,
        _ => {
            let (v, len) = strtod(t).ok_or_else(|| Error::InvalidArgument("an interval must start with '(' or '['".into()))?;
            return Ok((Interval { min: v, max: v }, &t[len..]));
        }
    };
    let t = t[1..].trim_start();
    let (min, rest) = if t.get(..4).is_some_and(|v| v.eq_ignore_ascii_case("-inf")) {
        (f64::NEG_INFINITY, &t[4..])
    } else {
        match strtod(t) {
            Some((v, len)) => (v, &t[len..]),
            None => return Err(Error::InvalidArgument("expected a number".into())),
        }
    };
    let Some(rest) = rest.strip_prefix(',') else {
        return Err(Error::InvalidArgument("expected a number".into()));
    };
    let t = rest.trim_start();
    let (max, rest) = if t.get(..3).is_some_and(|v| v.eq_ignore_ascii_case("inf")) {
        (f64::INFINITY, &t[3..])
    } else {
        match strtod(t) {
            Some((v, len)) => (v, &t[len..]),
            None => return Err(Error::InvalidArgument("an interval must end with ')' or ']'".into())),
        }
    };
    let max_included = match rest.as_bytes().first() {
        Some(b')') => false,
        Some(b']') => true,
        _ => return Err(Error::InvalidArgument("an interval must end with ')' or ']'".into())),
    };
    if min.is_nan() || max.is_nan() {
        return Err(Error::InvalidArgument("NaN is not a valid bound of an interval".into()));
    }
    if min > max {
        return Err(Error::InvalidArgument("the lower bound of an interval must not exceed its upper bound".into()));
    }
    let min = if min_included { min } else { next_up(min) };
    let max = if max_included { max } else { next_down(max) };
    Ok((Interval { min, max }, &rest[1..]))
}

fn next_up(v: f64) -> f64 {
    if v.is_nan() || v == f64::INFINITY {
        v
    } else if v == 0.0 {
        f64::from_bits(1)
    } else if v > 0.0 {
        f64::from_bits(v.to_bits() + 1)
    } else {
        f64::from_bits(v.to_bits() - 1)
    }
}

fn next_down(v: f64) -> f64 {
    -next_up(-v)
}

/// The mapping of `gdal raster reclassify`, parsed: `from=to` entries separated by `;`, where
/// `from` is a value, an interval (`[a,b)`, `(a,b]`, `(-inf,b]`...), `NO_DATA`, `NAN` or
/// `DEFAULT`, and `to` a value, `NO_DATA` or `PASS_THROUGH`.
#[derive(Debug, Clone, Default)]
pub struct Reclassifier {
    intervals: Vec<(Interval, Option<f64>)>,
    nan_value: Option<f64>,
    default_value: Option<f64>,
    default_pass_through: bool,
}

impl Reclassifier {
    /// Parses a mapping, as GDAL's `gdal::Reclassifier::Init` does: `nodata` is the value
    /// `NO_DATA` stands for, and every output value must be exactly representable in
    /// `data_type` (any value when `None`). Intervals must not overlap.
    pub fn parse(mapping: &str, nodata: Option<f64>, data_type: Option<DataType>) -> Result<Self> {
        let mut out = Self::default();
        let mut rest = mapping;
        loop {
            let start = rest.trim_start();
            let mut from = None;
            let (mut is_default, mut is_nan) = (false, false);
            let starts = |w: &str| start.get(..w.len()).is_some_and(|v| v.eq_ignore_ascii_case(w));
            let after = if starts("DEFAULT") {
                is_default = true;
                &start[7..]
            } else if starts("NO_DATA") {
                let nd =
                    nodata.ok_or_else(|| Error::InvalidArgument("value mapped from NO_DATA, but the nodata value is not set".into()))?;
                from = Some(Interval { min: nd, max: nd });
                &start[7..]
            } else if starts("NAN") {
                is_nan = true;
                &start[3..]
            } else {
                let (i, r) = parse_interval(start)?;
                from = Some(i);
                r
            };
            let after = after.trim_start();
            let Some(after) = after.strip_prefix('=') else {
                return Err(Error::InvalidArgument(format!(
                    "failed to parse the mapping (expected '=', got '{}')",
                    after.chars().next().map(String::from).unwrap_or_default()
                )));
            };
            let to_text = after.trim_start();
            let mut pass_through = false;
            let (to, after) = if let Some(r) = to_text.strip_prefix("NO_DATA") {
                let nd = nodata.ok_or_else(|| Error::InvalidArgument("value mapped to NO_DATA, but the nodata value is not set".into()))?;
                (Some(nd), r)
            } else if let Some(r) = to_text.strip_prefix("PASS_THROUGH") {
                pass_through = true;
                (None, r)
            } else {
                match strtod(to_text) {
                    Some((v, len)) => (Some(v), &to_text[len..]),
                    None => return Err(Error::InvalidArgument("failed to parse the output value (expected a number or NO_DATA)".into())),
                }
            };
            let after = after.trim_start();
            if !after.is_empty() && !after.starts_with(';') {
                return Err(Error::InvalidArgument(format!(
                    "failed to parse the mapping (expected ';' or the end, got '{}')",
                    after.chars().next().map(String::from).unwrap_or_default()
                )));
            }
            if let (Some(v), Some(dt)) = (to, data_type)
                && !exact_as(v, dt)
            {
                return Err(Error::InvalidArgument(format!("value {v} cannot be represented as data type {}", super::data_type_name(dt))));
            }
            if is_nan {
                out.nan_value = Some(if pass_through { f64::NAN } else { to.unwrap_or(f64::NAN) });
            } else if is_default {
                if pass_through {
                    out.default_pass_through = true;
                } else {
                    out.default_value = to;
                }
            } else if let Some(i) = from {
                out.intervals.push((i, to));
            }
            match after.strip_prefix(';') {
                Some(r) => rest = r,
                None => break,
            }
        }
        out.intervals.sort_by(|a, b| a.0.min.partial_cmp(&b.0.min).unwrap_or(std::cmp::Ordering::Equal));
        for w in out.intervals.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            if !(a.0.min > b.0.max || a.0.max < b.0.min) {
                return Err(Error::InvalidArgument(format!(
                    "interval from {} to {} (mapped to {}) overlaps with interval from {} to {} (mapped to {})",
                    a.0.min,
                    a.0.max,
                    a.1.unwrap_or(f64::NAN),
                    b.0.min,
                    b.0.max,
                    b.1.unwrap_or(f64::NAN)
                )));
            }
        }
        Ok(out)
    }

    /// The new value of `v`: `None` when no entry maps it.
    pub fn reclassify(&self, v: f64) -> Option<f64> {
        if v.is_nan() {
            if let Some(n) = self.nan_value {
                return Some(n);
            }
        } else if let Some(i) = self.find(v) {
            return Some(self.intervals[i].1.unwrap_or(v));
        }
        if let Some(d) = self.default_value {
            return Some(d);
        }
        self.default_pass_through.then_some(v)
    }

    /// The interval containing `v`, by binary search as GDAL searches.
    fn find(&self, v: f64) -> Option<usize> {
        let (mut lo, mut hi) = (0usize, self.intervals.len().checked_sub(1)?);
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            let i = &self.intervals[mid].0;
            if i.contains(v) {
                return Some(mid);
            }
            if v < i.min {
                if mid == 0 {
                    return None;
                }
                hi = mid - 1;
            } else if v > i.max {
                lo = mid + 1;
            } else {
                return None;
            }
        }
        None
    }
}

/// The mapping held by a mapping file, as `gdal raster reclassify --mapping @file` reads it: one
/// entry per line, leading white space and `#` comments removed, blank lines skipped.
pub fn mapping_from_file_text(text: &str) -> Result<String> {
    let mut mapping = String::new();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        if !mapping.is_empty() {
            mapping.push(';');
        }
        mapping.push_str(line);
        if mapping.len() > 10_000_000 {
            return Err(Error::InvalidArgument("the mapping is too large".into()));
        }
    }
    Ok(mapping)
}

/// Options for [`reclassify`].
#[derive(Debug, Clone, Default)]
pub struct ReclassifyOptions {
    /// The mapping (see [`Reclassifier`]).
    pub mapping: String,
    /// The output data type (`--ot`); by default that of the source.
    pub output_type: Option<DataType>,
}

/// Maps the values of every band to new values, as `gdal raster reclassify` does. Each band's
/// `NO_DATA` is its own nodata value, which the output declares unchanged. A value no entry
/// maps is an error.
pub fn reclassify(ds: &RasterDataset, o: &ReclassifyOptions) -> Result<RasterDataset> {
    if ds.complex {
        return Err(Error::Unsupported("reclassifying complex bands".into()));
    }
    let r = &ds.raster;
    let dst = o.output_type.unwrap_or(r.data_type);
    let n = r.cols * r.rows;
    // GDAL validates against the first band's nodata value before reading.
    Reclassifier::parse(&o.mapping, ds.band_nodata(1), o.output_type)?;
    let mut bands = Vec::with_capacity(r.bands);
    for b in 0..r.bands {
        let rc = Reclassifier::parse(&o.mapping, ds.band_nodata(b + 1), Some(dst))?;
        let view = super::BandView::of(r, b);
        let values: Vec<Option<f64>> = crate::par::map_range(n, |i| rc.reclassify(view.get(i)).map(|v| convert_value(v, dst)));
        let mut band = Vec::with_capacity(n);
        for (i, v) in values.into_iter().enumerate() {
            match v {
                Some(v) => band.push(v),
                None => {
                    return Err(Error::InvalidArgument(format!("encountered value {} with no specified mapping", view.get(i))));
                }
            }
        }
        bands.push(band);
    }
    let data = super::bands_data(&bands, n, dst, |v, _| v);
    let mut out = super::derived(ds, data, r.bands)?;
    // GDAL's derived bands carry nodata values only: no color table, descriptions or scales.
    // Its GeoTIFFs then call several bands Undefined, and a single band Gray.
    if r.bands > 1 {
        out.band_metadata = vec![crate::BandMetadata { color_interp: Some("Undefined".into()), ..Default::default() }; r.bands];
    }
    copy_nodata(ds, &mut out);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{Grid, build_raster};

    fn ds(bands: Vec<Vec<f64>>, w: usize, h: usize, dt: DataType, nodata: Option<f64>) -> RasterDataset {
        let grid = Grid { width: w, height: h, geo_transform: [10.0, 1.0, 0.0, 20.0, 0.0, -1.0] };
        build_raster(grid, bands, dt, nodata, &crate::crs::SpatialRef::default(), convert_value).unwrap()
    }

    #[test]
    fn unscales_with_nodata_and_types() {
        let mut src = ds(vec![vec![1.0, 2.0, -9999.0, 4.0], vec![1.0, 2.0, 3.0, -9999.0]], 2, 2, DataType::I16, Some(-9999.0));
        src.band_metadata = vec![
            crate::BandMetadata { scale: Some(0.5), offset: Some(10.0), unit: Some("m".into()), ..Default::default() },
            crate::BandMetadata { scale: Some(2.0), ..Default::default() },
        ];
        let out = unscale(&src, &Default::default()).unwrap();
        assert_eq!(out.raster.data_type, DataType::F32);
        assert_eq!(out.nodata(), Some(-9999.0));
        assert_eq!((0..4).map(|i| out.raster.data.get_f64(i)).collect::<Vec<_>>(), [10.5, 11.0, -9999.0, 12.0]);
        assert_eq!((4..8).map(|i| out.raster.data.get_f64(i)).collect::<Vec<_>>(), [2.0, 4.0, 6.0, -9999.0]);
        assert_eq!(out.band_metadata[0].scale, None);
        assert_eq!(out.band_metadata[0].unit.as_deref(), Some("m"));
        let byte = unscale(&src, &UnscaleOptions { output_type: Some(DataType::U8) }).unwrap();
        assert_eq!((0..4).map(|i| byte.raster.data.get_f64(i)).collect::<Vec<_>>(), [11.0, 11.0, 0.0, 12.0]);
        assert_eq!(byte.nodata(), Some(-9999.0));
        let f64src = ds(vec![vec![1.0]], 1, 1, DataType::F64, None);
        assert_eq!(unscale(&f64src, &Default::default()).unwrap().raster.data_type, DataType::F64);
    }

    #[test]
    fn nodata_becomes_alpha() {
        let src = ds(vec![vec![1.0, 2.0, 3.0], vec![2.0, 6.0, 4.0]], 3, 1, DataType::U16, Some(2.0));
        let out = nodata_to_alpha(&src, &Default::default()).unwrap();
        assert_eq!(out.raster.bands, 3);
        assert_eq!(out.nodata(), None);
        assert_eq!(out.raster.data_type, DataType::U16);
        assert_eq!((6..9).map(|i| out.raster.data.get_f64(i)).collect::<Vec<_>>(), [255.0, 0.0, 255.0]);
        assert_eq!(out.band_color_interp(3), "Alpha");
        // One value per band: transparent only where every band matches.
        let per_band = nodata_to_alpha(&src, &NodataToAlphaOptions { nodata: vec![1.0, 2.0] }).unwrap();
        assert_eq!((6..9).map(|i| per_band.raster.data.get_f64(i)).collect::<Vec<_>>(), [0.0, 255.0, 255.0]);
        let single = nodata_to_alpha(&src, &NodataToAlphaOptions { nodata: vec![3.0] }).unwrap();
        assert_eq!((6..9).map(|i| single.raster.data.get_f64(i)).collect::<Vec<_>>(), [255.0, 255.0, 0.0]);
        assert!(nodata_to_alpha(&src, &NodataToAlphaOptions { nodata: vec![1.0, 2.0, 3.0] }).is_err());
        let plain = ds(vec![vec![1.0]], 1, 1, DataType::U8, None);
        assert_eq!(nodata_to_alpha(&plain, &Default::default()).unwrap().raster.bands, 1);
    }

    #[test]
    fn parses_gdal_mappings() {
        let r = Reclassifier::parse("[1,3)=10; 3=20; (3, 5]=PASS_THROUGH; NO_DATA=0; DEFAULT=99", Some(-1.0), Some(DataType::U8)).unwrap();
        assert_eq!(r.reclassify(1.0), Some(10.0));
        assert_eq!(r.reclassify(2.999), Some(10.0));
        assert_eq!(r.reclassify(3.0), Some(20.0));
        assert_eq!(r.reclassify(4.5), Some(4.5));
        assert_eq!(r.reclassify(-1.0), Some(0.0));
        assert_eq!(r.reclassify(7.0), Some(99.0));
        let r = Reclassifier::parse("(-inf, 0]=1;(0,inf)=2;NAN=3", None, None).unwrap();
        assert_eq!(r.reclassify(-1e300), Some(1.0));
        assert_eq!(r.reclassify(0.0), Some(1.0));
        assert_eq!(r.reclassify(1e-300), Some(2.0));
        assert_eq!(r.reclassify(f64::NAN), Some(3.0));
        let r = Reclassifier::parse("1=2", None, None).unwrap();
        assert_eq!(r.reclassify(5.0), None);
        let r = Reclassifier::parse("1=2;DEFAULT=PASS_THROUGH", None, None).unwrap();
        assert_eq!(r.reclassify(5.0), Some(5.0));
        for bad in ["[1,3)=1;[2,4)=2", "1=300", "NO_DATA=1", "1=NO_DATA", "[3,1]=1", "1:2", "1=2;", "{1,2}=3", "1=x"] {
            assert!(Reclassifier::parse(bad, None, Some(DataType::U8)).is_err(), "{bad}");
        }
        assert_eq!(mapping_from_file_text("  1=2 # one\n\n[2,3)=4\r\n").unwrap(), "1=2;[2,3)=4");
    }

    #[test]
    fn reclassifies_bands() {
        let src = ds(vec![vec![1.0, 2.0, 255.0, 4.0]], 2, 2, DataType::U8, Some(255.0));
        let o = ReclassifyOptions { mapping: "[0,3)=1;[3,10]=2;NO_DATA=NO_DATA".into(), output_type: None };
        let out = reclassify(&src, &o).unwrap();
        assert_eq!((0..4).map(|i| out.raster.data.get_f64(i)).collect::<Vec<_>>(), [1.0, 1.0, 255.0, 2.0]);
        assert_eq!(out.nodata(), Some(255.0));
        let o = ReclassifyOptions { mapping: "[0,3)=1".into(), output_type: None };
        assert!(reclassify(&src, &o).unwrap_err().to_string().contains("no specified mapping"));
        let o = ReclassifyOptions { mapping: "DEFAULT=1000".into(), output_type: None };
        assert!(reclassify(&src, &o).is_err());
        let o = ReclassifyOptions { mapping: "DEFAULT=1000".into(), output_type: Some(DataType::I16) };
        assert_eq!(reclassify(&src, &o).unwrap().raster.data_type, DataType::I16);
    }
    #[test]
    fn mapping_comments_and_non_ascii_errors() {
        let mapping = mapping_from_file_text("# classes\n1=2 # one\n  # another\n[2,3)=4\n# end").unwrap();
        assert_eq!(mapping, "1=2;[2,3)=4");
        assert!(Reclassifier::parse(&mapping, None, None).is_ok());
        for text in ["é=1", "[éé,3)=1", "[0,é)=1", "默认=1"] {
            assert!(Reclassifier::parse(text, None, None).is_err(), "{text}");
        }
    }
    #[test]
    fn nan_nodata_mapping_matches_gdal_rejection() {
        // GDAL 3.13.3 also rejects the NaN interval as overlapping. NAN is the
        // explicit mapping key for NaN pixels, regardless of declared nodata.
        assert!(Reclassifier::parse("NO_DATA=0;[0,10)=1", Some(f64::NAN), None).is_err());
        let mapping = Reclassifier::parse("NAN=0;[0,10)=1", Some(f64::NAN), None).unwrap();
        assert_eq!(mapping.reclassify(f64::NAN), Some(0.));
        assert_eq!(mapping.reclassify(5.), Some(1.));
    }
}
