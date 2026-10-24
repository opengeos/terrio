//! Weighted Brovey pansharpening, following GDAL's `gdalpansharpen.cpp` (MIT; see NOTICE).
//!
//! Input rasters must share a CRS and have north-up geotransforms. Spectral bands are
//! resampled in their native type, converted to the panchromatic working type (Byte,
//! UInt16, otherwise Float64), then sharpened and converted to the output type.
use super::vrt::SourceRead;
use super::{BandView, Grid, Resampling, build_raster, convert_value, type_range};
use crate::{Error, RasterDataset, Result};
use wbraster::DataType;

/// One spectral input; all inputs contribute to pseudo-panchromatic intensity.
#[derive(Debug, Clone, Copy)]
pub struct SpectralBand<'a> {
    /// Source raster.
    pub dataset: &'a RasterDataset,
    /// Band, from 1.
    pub band: usize,
}

/// How unequal input footprints determine the output extent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PansharpenExtent {
    /// Union, padded with zero at each source's original resolution (GDAL default).
    #[default]
    Union,
    /// Intersection of all input footprints.
    Intersection,
    /// Preserve the panchromatic footprint, without adjusting source extents.
    None,
}
impl PansharpenExtent {
    /// Parses union, intersection, none or nonewithoutwarning.
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "union" => Ok(Self::Union),
            "intersection" => Ok(Self::Intersection),
            "none" | "nonewithoutwarning" => Ok(Self::None),
            _ => Err(Error::InvalidArgument("pansharpen extent must be union, intersection, none or nonewithoutwarning".into())),
        }
    }
}

/// Common nodata setting used during both resampling and Brovey computation.
#[derive(Debug, Clone, Copy, Default)]
pub enum PansharpenNodata {
    /// Use nodata when all bands declare the same value.
    #[default]
    Auto,
    /// Disable nodata processing.
    None,
    /// Use this value in every band, overriding input declarations.
    Value(f64),
}

/// Options for [`pansharpen`].
#[derive(Debug, Clone)]
pub struct PansharpenOptions {
    /// Panchromatic band, from 1.
    pub pan_band: usize,
    /// Weights in spectral-input order; empty means 1 / number of inputs. Not normalized.
    pub weights: Vec<f64>,
    /// Spectral inputs to output, from 1; empty means all. Others still contribute intensity.
    pub output_bands: Vec<usize>,
    /// Spectral upsampling method (default cubic).
    pub resampling: Resampling,
    /// Clamp unsigned working values and outputs to 2^bits-1. Explicit, not inferred from NBITS.
    pub bit_depth: Option<u8>,
    /// Common nodata behavior.
    pub nodata: PansharpenNodata,
    /// Extent adjustment (default union).
    pub extent: PansharpenExtent,
    /// Output data type; defaults to the first selected spectral band's type.
    pub output_type: Option<DataType>,
}
impl Default for PansharpenOptions {
    fn default() -> Self {
        Self {
            pan_band: 1,
            weights: vec![],
            output_bands: vec![],
            resampling: Resampling::Cubic,
            bit_depth: None,
            nodata: PansharpenNodata::Auto,
            extent: PansharpenExtent::Union,
            output_type: None,
        }
    }
}

fn bad(message: &str) -> Error {
    Error::InvalidArgument(message.into())
}

fn check(ds: &RasterDataset, band: usize) -> Result<Grid> {
    if ds.complex || band == 0 || band > ds.raster.bands {
        return Err(bad("pansharpen requires existing real input bands"));
    }
    let grid = Grid { width: ds.raster.cols, height: ds.raster.rows, geo_transform: ds.geo_transform() };
    super::north_up(&grid)?;
    let gt = grid.geo_transform;
    if gt.iter().any(|v| !v.is_finite()) || gt[1] <= 0.0 || gt[5] >= 0.0 || grid.width == 0 || grid.height == 0 {
        return Err(bad("pansharpen requires finite north-up input grids"));
    }
    Ok(grid)
}

/// Pad/crop a band to common bounds at its original resolution, as GDAL's adjustment VRT does.
fn adjust(ds: &RasterDataset, band: usize, bounds: (f64, f64, f64, f64), enabled: bool, pixel: f64) -> Result<RasterDataset> {
    let g = check(ds, band)?;
    let gt = g.geo_transform;
    let old = g.bounds();
    let tolerance = pixel.max(gt[1]).max(-gt[5]);
    if !enabled || [old.0 - bounds.0, old.1 - bounds.1, old.2 - bounds.2, old.3 - bounds.3].iter().all(|d| d.abs() <= tolerance) {
        return super::translate::translate(ds, &super::translate::TranslateOptions { bands: vec![band], ..Default::default() });
    }
    let width = 0.5 + (bounds.2 - bounds.0) / gt[1];
    let height = 0.5 + (bounds.3 - bounds.1) / -gt[5];
    if !width.is_finite() || !height.is_finite() || width < 1.0 || height < 1.0 || width >= i32::MAX as f64 || height >= i32::MAX as f64 {
        return Err(bad("pansharpen adjusted grid dimensions are invalid or too large"));
    }
    let (width, height) = (width as usize, height as usize);
    let xoff = ((bounds.0 - gt[0]) / gt[1] + 0.001).floor() as isize;
    let yoff = ((bounds.3 - gt[3]) / gt[5] + 0.001).floor() as isize;
    let mut values = vec![0.0; width.checked_mul(height).ok_or_else(|| bad("pansharpen grid too large"))?];
    let input = BandView::of(&ds.raster, band - 1);
    for y in 0..height {
        for x in 0..width {
            let (sx, sy) = (x as isize + xoff, y as isize + yoff);
            if sx >= 0 && sy >= 0 && sx < g.width as isize && sy < g.height as isize {
                values[y * width + x] = input.get(sy as usize * g.width + sx as usize);
            }
        }
    }
    // GDAL's adjustment VRT drops source nodata. The common explicit override
    // is applied after adjustment; retaining it here would change Auto behavior.
    build_raster(
        Grid { width, height, geo_transform: [bounds.0, gt[1], 0.0, bounds.3, 0.0, gt[5]] },
        vec![values],
        ds.raster.data_type,
        None,
        &ds.crs(),
        |v, _| v,
    )
}

// GDAL's overview convolution is separable: masked horizontal rows carry an
// binary row validity to the vertical pass (GDAL 3.12); integer inputs use Float32 final
// interpolation before conversion back to their native type.
fn convolution(s: &SourceRead<'_>, rect: [f64; 4], width: usize, height: usize, work: DataType) -> Vec<f64> {
    let (rx, ry) = (rect[2] / width as f64, rect[3] / height as f64);
    let method = s.resampling;
    let taps = |dst: usize, ratio: f64, off: f64, size: usize| {
        let center = (dst as f64 + 0.5) * ratio + off;
        let scale = (1.0 / ratio).min(1.0);
        let radius = method.radius().max(1) as f64 / scale;
        let start = ((center - radius + 0.5).floor().max(0.0) as usize).min(size);
        let stop = ((center + radius + 0.5) as usize).min(size).max(start);
        (start, (start..stop).map(|p| method.weight(scale * (p as f64 - center + 0.5))).collect::<Vec<_>>())
    };
    let xt = (0..width).map(|x| taps(x, rx, rect[0], s.width)).collect::<Vec<_>>();
    let negative = matches!(method, Resampling::Cubic | Resampling::Lanczos);
    let yt = (0..height).map(|row| taps(row, ry, rect[1], s.height)).collect::<Vec<_>>();
    let first_row = yt.iter().map(|(start, _)| *start).min().unwrap_or(0);
    let last_row = yt.iter().map(|(start, weights)| start + weights.len()).max().unwrap_or(first_row);
    let mut horizontal = vec![0.0; width * (last_row - first_row)];
    let mut coverage = vec![0u8; width * (last_row - first_row)];
    for y in first_row..last_row {
        for (x, (start, weights)) in xt.iter().enumerate() {
            let (mut value, mut valid_weight) = (0.0, 0.0);
            let (mut consecutive, mut longest) = (0, 0);
            for (j, w) in weights.iter().enumerate() {
                let p = y * s.width + start + j;
                if s.valid.is_none_or(|m| m[p]) {
                    value += s.values.get(p) * w;
                    valid_weight += w;
                    consecutive += 1;
                    longest = longest.max(consecutive);
                } else {
                    consecutive = 0;
                }
            }
            if valid_weight > 0.0 && (!negative || s.valid.is_none() || longest >= weights.len() / 2) {
                let p = (y - first_row) * width + x;
                horizontal[p] = value / valid_weight;
                coverage[p] = 1;
            }
        }
    }
    let mut result = vec![s.invalid_value.unwrap_or(0.0); width * height];
    for (y, (start, weights)) in yt.iter().enumerate() {
        for x in 0..width {
            let (mut value, mut total) = (0.0, 0.0);
            let (mut consecutive, mut longest) = (0, 0);
            for (j, w) in weights.iter().enumerate() {
                let p = (start + j - first_row) * width + x;
                let weight = w * coverage[p] as f64;
                value += horizontal[p] * weight;
                total += weight;
                if coverage[p] > 0 {
                    consecutive += 1;
                    longest = longest.max(consecutive);
                } else {
                    consecutive = 0;
                }
            }
            if total > 0.0 && (!negative || s.valid.is_none() || longest >= weights.len() / 2) {
                let raw = value / total;
                let raw = if s.src_type == DataType::F64 { raw } else { raw as f32 as f64 };
                let mut native = convert_value(raw, s.src_type);
                if s.invalid_value == Some(native) {
                    native = if matches!(s.src_type, DataType::F32 | DataType::F64) {
                        native + 1e-5
                    } else if native == type_range(s.src_type).0 {
                        native + 1.0
                    } else {
                        native - 1.0
                    };
                }
                result[y * width + x] = convert_value(native, work);
            }
        }
    }
    result
}

/// Sharpen the selected spectral bands at the panchromatic resolution.
///
/// Weighted Brovey computes `spectral * pan / sum(weight * spectral)`; zero intensity
/// produces zero, or common nodata when enabled. Any nodata contributor invalidates all
/// output bands. Integer working values are rounded/clamped before output conversion,
/// including when the output itself is floating point, as in GDAL. Explicit input masks
/// are not used. The output is materialized; virtual pansharpened VRT output is deferred.
pub fn pansharpen(pan: &RasterDataset, spectral: &[SpectralBand<'_>], o: &PansharpenOptions) -> Result<RasterDataset> {
    let pg = check(pan, o.pan_band)?;
    if spectral.is_empty() {
        return Err(bad("pansharpen needs at least one spectral band"));
    }
    if !matches!(
        o.resampling,
        Resampling::Nearest
            | Resampling::Bilinear
            | Resampling::Cubic
            | Resampling::CubicSpline
            | Resampling::Lanczos
            | Resampling::Average
    ) {
        return Err(bad("pansharpen resampling must be nearest, bilinear, cubic, cubicspline, lanczos or average"));
    }
    let weights = if o.weights.is_empty() { vec![1.0 / spectral.len() as f64; spectral.len()] } else { o.weights.clone() };
    if weights.len() != spectral.len() || weights.iter().any(|v| !v.is_finite()) {
        return Err(bad("pansharpen needs one finite weight per spectral input"));
    }
    let output = if o.output_bands.is_empty() { (1..=spectral.len()).collect::<Vec<_>>() } else { o.output_bands.clone() };
    if output.iter().any(|b| *b == 0 || *b > spectral.len()) {
        return Err(bad("pansharpen output bands refer to spectral inputs, from 1"));
    }
    let work = match pan.raster.data_type {
        DataType::U8 => DataType::U8,
        DataType::U16 => DataType::U16,
        _ => DataType::F64,
    };
    if o.bit_depth.is_some_and(|bits| bits == 0 || bits > 31 || (work == DataType::U8 && bits > 8) || (work == DataType::U16 && bits > 16))
    {
        return Err(bad("pansharpen bit depth must fit the panchromatic type and be between 1 and 31"));
    }
    if let PansharpenNodata::Value(v) = o.nodata
        && !v.is_finite()
    {
        return Err(bad("pansharpen nodata must be finite"));
    }
    let mut bounds = pg.bounds();
    let pixel = pg.geo_transform[1].max(-pg.geo_transform[5]);
    for b in spectral {
        let grid = check(b.dataset, b.band)?;
        let (pan_crs, spectral_crs) = (pan.crs(), b.dataset.crs());
        if !(pan_crs.is_unknown() && spectral_crs.is_unknown()) && !pan_crs.same_as(&spectral_crs) {
            return Err(bad("pansharpen inputs must share a CRS; reproject first"));
        }
        let bb = grid.bounds();
        let pan_bounds = pg.bounds();
        let different = [pan_bounds.0 - bb.0, pan_bounds.1 - bb.1, pan_bounds.2 - bb.2, pan_bounds.3 - bb.3]
            .iter()
            .any(|d| d.abs() > grid.geo_transform[1].max(-grid.geo_transform[5]));
        if different {
            match o.extent {
                PansharpenExtent::Union => bounds = (bounds.0.min(bb.0), bounds.1.min(bb.1), bounds.2.max(bb.2), bounds.3.max(bb.3)),
                PansharpenExtent::Intersection => bounds = (bounds.0.max(bb.0), bounds.1.max(bb.1), bounds.2.min(bb.2), bounds.3.min(bb.3)),
                PansharpenExtent::None => {}
            }
        }
    }
    if bounds.0 >= bounds.2 || bounds.1 >= bounds.3 {
        return Err(bad("pansharpen input extents do not intersect"));
    }
    if o.extent != PansharpenExtent::None
        && [pg.bounds().0 - bounds.0, pg.bounds().1 - bounds.1, pg.bounds().2 - bounds.2, pg.bounds().3 - bounds.3]
            .iter()
            .any(|d| d.abs() > pixel)
    {
        let gt = pg.geo_transform;
        bounds.0 = gt[0] + ((bounds.0 - gt[0]) / gt[1] + 0.5).floor() * gt[1];
        bounds.2 = gt[0] + ((bounds.2 - gt[0]) / gt[1] + 0.5).floor() * gt[1];
        bounds.1 = gt[3] + ((bounds.1 - gt[3]) / gt[5] + 0.5).floor() * gt[5];
        bounds.3 = gt[3] + ((bounds.3 - gt[3]) / gt[5] + 0.5).floor() * gt[5];
    }
    let pan = adjust(pan, o.pan_band, bounds, o.extent != PansharpenExtent::None, pixel)?;
    let bands = spectral
        .iter()
        .map(|b| adjust(b.dataset, b.band, bounds, o.extent != PansharpenExtent::None, pixel))
        .collect::<Result<Vec<_>>>()?;
    let nodata = match o.nodata {
        PansharpenNodata::None => None,
        PansharpenNodata::Value(v) => Some(v),
        PansharpenNodata::Auto => pan.nodata().filter(|v| v.is_finite() && bands.iter().all(|b| b.nodata() == Some(*v))),
    };
    let grid = Grid::of(&pan.raster);
    let n = grid.width * grid.height;
    let gt = grid.geo_transform;
    let max = if matches!(work, DataType::U8 | DataType::U16) { o.bit_depth.map(|b| ((1u64 << b) - 1) as f64) } else { None };
    let mut values = Vec::new();
    for ds in bands {
        let sgt = ds.geo_transform();
        let input = BandView::of(&ds.raster, 0);
        let source_nodata = nodata.or(ds.nodata());
        let valid = if o.resampling == Resampling::Nearest { None } else { source_nodata.map(|nd| input.valid(nd)) };
        let (mut x, mut y) = ((gt[0] - sgt[0]) / sgt[1], (gt[3] - sgt[3]) / sgt[5]);
        if x.abs() < 1e-8 {
            x = 0.0;
        }
        if y.abs() < 1e-8 {
            y = 0.0;
        }
        // GDAL truncates floating RasterIO origins toward zero, so a fractional
        // offset down to (but excluding) -1 still has a valid integer origin 0.
        if x <= -1.0 || y <= -1.0 {
            return Err(bad("pansharpen spectral window is outside input; use union or intersection extent"));
        }
        let rect = [
            x,
            y,
            (grid.width as f64 * gt[1] / sgt[1]).min(ds.raster.cols as f64 - x),
            (grid.height as f64 * gt[5] / sgt[5]).min(ds.raster.rows as f64 - y),
        ];
        if rect[2] <= 0.0 || rect[3] <= 0.0 {
            return Err(bad("pansharpen spectral window does not intersect input"));
        }
        let mut out = vec![nodata.unwrap_or(0.0); n];
        let read = SourceRead {
            values: input,
            valid: valid.as_deref(),
            width: ds.raster.cols,
            height: ds.raster.rows,
            src_type: ds.raster.data_type,
            src_rect: rect,
            dst_rect: [0.0, 0.0, grid.width as f64, grid.height as f64],
            resampling: o.resampling,
            working_float: false,
            invalid_value: source_nodata,
        };
        match o.resampling {
            Resampling::Nearest => {
                let (rx, ry) = (rect[2] / grid.width as f64, rect[3] / grid.height as f64);
                for row in 0..grid.height {
                    let sy = ((row as f64 + 0.5) * ry + y + 1e-10).clamp(0.0, ds.raster.rows as f64 - 1.0) as usize;
                    for col in 0..grid.width {
                        let sx = ((col as f64 + 0.5) * rx + x + 1e-10).clamp(0.0, ds.raster.cols as f64 - 1.0) as usize;
                        out[row * grid.width + col] = convert_value(input.get(sy * ds.raster.cols + sx), work);
                    }
                }
            }
            Resampling::Average => {
                let averaged = super::vrt::gdal_average(
                    &read,
                    (x.max(0.0) as usize, y.max(0.0) as usize),
                    (rect[0], rect[1], rect[2], rect[3]),
                    (grid.width, grid.height),
                );
                for (target, value) in out.iter_mut().zip(averaged) {
                    *target = convert_value(convert_value(value.unwrap_or(source_nodata.unwrap_or(0.0)), ds.raster.data_type), work);
                }
            }
            _ => out = convolution(&read, rect, grid.width, grid.height, work),
        }
        if matches!(o.resampling, Resampling::Cubic | Resampling::CubicSpline | Resampling::Lanczos)
            && let Some(max) = max
        {
            for v in &mut out {
                *v = v.min(max);
            }
        }
        values.push(out);
    }
    let pan_values = super::band_f64(&pan.raster, 0);
    let output_crs = pan.crs();
    drop(pan);
    let no = nodata.map(|v| convert_value(v, work));
    let dtype = o.output_type.unwrap_or(spectral[output[0] - 1].dataset.raster.data_type);
    let mut result = vec![vec![0.0; n]; output.len()];
    for p in 0..n {
        let sum = weights.iter().zip(&values).map(|(w, b)| w * b[p]).sum::<f64>();
        let invalid = no.is_some_and(|nd| pan_values[p] == nd || values.iter().any(|b| b[p] == nd) || sum == 0.0);
        for (i, b) in output.iter().enumerate() {
            let v = if invalid {
                no.unwrap()
            } else {
                let mut v = convert_value(values[*b - 1][p] * if sum == 0.0 { 0.0 } else { pan_values[p] / sum }, work);
                if let Some(max) = max {
                    v = v.min(max);
                }
                if no == Some(v) {
                    v = if work == DataType::F64 {
                        v + 1e-5
                    } else if v == type_range(work).0 {
                        v + 1.0
                    } else {
                        v - 1.0
                    };
                }
                v
            };
            result[i][p] = convert_value(v, dtype);
        }
    }
    drop(values);
    drop(pan_values);
    build_raster(grid, result, dtype, nodata, &output_crs, |v, _| v)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn raster(values: Vec<Vec<f64>>, width: usize, height: usize, pixel: f64, dtype: DataType, nodata: Option<f64>) -> RasterDataset {
        build_raster(
            Grid { width, height, geo_transform: [0.0, pixel, 0.0, height as f64 * pixel, 0.0, -pixel] },
            values,
            dtype,
            nodata,
            &crate::crs::SpatialRef::from_epsg(32611).unwrap(),
            |v, _| v,
        )
        .unwrap()
    }
    #[test]
    fn unequal_resolution_weights_selection_and_integer_working_type() {
        let pan = raster(vec![vec![80.0; 16]], 4, 4, 1.0, DataType::U16, None);
        let ms = raster(vec![vec![20.0; 4], vec![40.0; 4]], 2, 2, 2.0, DataType::U16, None);
        let input = [SpectralBand { dataset: &ms, band: 1 }, SpectralBand { dataset: &ms, band: 2 }];
        let o = PansharpenOptions { output_type: Some(DataType::F64), ..Default::default() };
        let out = pansharpen(&pan, &input, &o).unwrap();
        assert_eq!(out.raster.get(0, 1, 1), 53.0);
        assert_eq!(out.raster.get(1, 1, 1), 107.0);
        let weighted = pansharpen(&pan, &input, &PansharpenOptions { weights: vec![0.75, 0.25], output_bands: vec![2], ..o }).unwrap();
        assert_eq!(weighted.raster.bands, 1);
        assert_eq!(weighted.raster.get(0, 1, 1), 128.0);
    }
    #[test]
    fn nodata_zero_intensity_and_bitdepth() {
        let pan = raster(vec![vec![100.0, 100.0]], 2, 1, 1.0, DataType::U16, Some(0.0));
        let ms = raster(vec![vec![0.0, 20.0], vec![10.0, 40.0]], 2, 1, 1.0, DataType::U16, Some(0.0));
        let input = [SpectralBand { dataset: &ms, band: 1 }, SpectralBand { dataset: &ms, band: 2 }];
        let out = pansharpen(&pan, &input, &PansharpenOptions { resampling: Resampling::Nearest, ..Default::default() }).unwrap();
        assert_eq!(out.nodata(), Some(0.0));
        assert_eq!(out.raster.get(0, 0, 0), 0.0);
        assert_eq!(out.raster.get(1, 0, 0), 0.0);
        assert_eq!(out.raster.get(0, 0, 1), 67.0);
        let pan = raster(vec![vec![4095.0]], 1, 1, 1.0, DataType::U16, None);
        let ms = raster(vec![vec![5000.0], vec![1000.0]], 1, 1, 1.0, DataType::U16, None);
        let input = [SpectralBand { dataset: &ms, band: 1 }, SpectralBand { dataset: &ms, band: 2 }];
        let out = pansharpen(&pan, &input, &PansharpenOptions { bit_depth: Some(12), ..Default::default() }).unwrap();
        assert_eq!(out.raster.get(0, 0, 0), 4095.0);
        assert_eq!(out.raster.get(1, 0, 0), 1607.0);
    }
    #[test]
    fn disabled_nodata_preserves_nearest_samples_as_numbers() {
        let pan = raster(vec![vec![100.0; 2]], 2, 1, 1.0, DataType::U16, Some(9999.0));
        let ms = raster(vec![vec![9999.0, 20.0], vec![10.0, 40.0]], 2, 1, 1.0, DataType::U16, Some(9999.0));
        let inputs = [SpectralBand { dataset: &ms, band: 1 }, SpectralBand { dataset: &ms, band: 2 }];
        let out = pansharpen(
            &pan,
            &inputs,
            &PansharpenOptions { resampling: Resampling::Nearest, nodata: PansharpenNodata::None, ..Default::default() },
        )
        .unwrap();
        assert_eq!(out.nodata(), None);
        assert_eq!(out.raster.get(0, 0, 0), 200.0);
        assert_eq!(out.raster.get(1, 0, 0), 0.0);
    }
    #[test]
    fn ungeoreferenced_rasters_share_the_same_affine_space() {
        let mut src = raster(vec![vec![10.0; 4]], 2, 2, 1.0, DataType::U8, None);
        src.raster.crs = Default::default();
        let input = [SpectralBand { dataset: &src, band: 1 }];
        let out = pansharpen(&src, &input, &Default::default()).unwrap();
        assert_eq!(out.raster.get(0, 0, 0), 10.0);
        assert!(out.crs().is_unknown());
    }
    #[test]
    fn pan_extent_reads_only_the_matching_spectral_rows() {
        let pan = raster(vec![vec![80.0; 4]], 2, 2, 1.0, DataType::U16, None);
        let ms = raster(vec![vec![20.0; 64], vec![40.0; 64]], 8, 8, 1.0, DataType::U16, None);
        let inputs = [SpectralBand { dataset: &ms, band: 1 }, SpectralBand { dataset: &ms, band: 2 }];
        let out = pansharpen(&pan, &inputs, &PansharpenOptions { extent: PansharpenExtent::None, ..Default::default() }).unwrap();
        assert_eq!((out.raster.cols, out.raster.rows), (2, 2));
        assert_eq!(out.raster.get(0, 0, 0), 53.0);
        assert_eq!(out.raster.get(1, 0, 0), 107.0);
    }
    #[test]
    fn invalid_inputs_are_errors() {
        let src = raster(vec![vec![10.0; 4]], 2, 2, 1.0, DataType::U8, None);
        let input = [SpectralBand { dataset: &src, band: 1 }];
        assert!(pansharpen(&src, &[], &Default::default()).is_err());
        assert!(pansharpen(&src, &input, &PansharpenOptions { weights: vec![1.0, 1.0], ..Default::default() }).is_err());
        assert!(pansharpen(&src, &input, &PansharpenOptions { bit_depth: Some(12), ..Default::default() }).is_err());
        assert!(pansharpen(&src, &input, &PansharpenOptions { output_bands: vec![0], ..Default::default() }).is_err());
    }
}
