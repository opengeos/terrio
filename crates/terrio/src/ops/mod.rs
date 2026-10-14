//! GDAL-style processing operations: [`translate`] (`gdal_translate`), [`warp`] (`gdalwarp`),
//! [`mosaic`] (`gdalbuildvrt` and `gdal_merge.py`), [`vrt`] (GDAL's virtual raster format),
//! [`rasterize`] (`gdal_rasterize`), [`polygonize`] (`gdal_polygonize`), and the vector
//! filtering of `ogr2ogr` ([`vector_filter`], [`sql`]).
//!
//! Pixel coordinates follow GDAL's convention: pixel `(col, row)` covers `[col, col + 1)` ×
//! `[row, row + 1)` and its centre is at `(col + 0.5, row + 0.5)`. Geotransforms are GDAL's
//! `[x_origin, pixel_width, 0, y_origin, 0, -pixel_height]`.

pub mod mosaic;
pub mod polygonize;
pub mod rasterize;
pub mod sql;
pub mod translate;
pub mod vector_filter;
pub mod vrt;
pub mod warp;

use crate::crs::SpatialRef;
use crate::{Error, RasterDataset, Result};
use wbraster::raster::RasterData;
use wbraster::{CrsInfo, DataType, Raster, RasterConfig};

/// A resampling method, named as GDAL names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Resampling {
    /// Nearest neighbour.
    #[default]
    Nearest,
    /// Bilinear interpolation (2×2 kernel).
    Bilinear,
    /// Cubic convolution (Keys, a = -0.5; 4×4 kernel).
    Cubic,
    /// Cubic B-spline (4×4 kernel).
    CubicSpline,
    /// Lanczos windowed sinc (6×6 kernel).
    Lanczos,
    /// Average of the contributing pixels, weighted by their coverage.
    Average,
    /// Root mean square of the contributing pixels.
    Rms,
    /// Most frequent value of the contributing pixels.
    Mode,
    /// Minimum of the contributing pixels.
    Min,
    /// Maximum of the contributing pixels.
    Max,
    /// Median of the contributing pixels.
    Med,
    /// First quartile of the contributing pixels.
    Q1,
    /// Third quartile of the contributing pixels.
    Q3,
    /// Sum of the contributing pixels, weighted by their coverage.
    Sum,
}

impl Resampling {
    /// Parses a method name: `near`/`nearest`, `bilinear`, `cubic`, `cubicspline`, `lanczos`,
    /// `average`, `rms`, `mode`, `min`, `max`, `med`, `q1`, `q3`, `sum`.
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "near" | "nearest" => Self::Nearest,
            "bilinear" => Self::Bilinear,
            "cubic" => Self::Cubic,
            "cubicspline" => Self::CubicSpline,
            "lanczos" => Self::Lanczos,
            "average" => Self::Average,
            "rms" => Self::Rms,
            "mode" => Self::Mode,
            "min" => Self::Min,
            "max" => Self::Max,
            "med" => Self::Med,
            "q1" => Self::Q1,
            "q3" => Self::Q3,
            "sum" => Self::Sum,
            other => return Err(Error::InvalidArgument(format!("unknown resampling method '{other}'"))),
        })
    }

    /// GDAL's name for the method.
    pub fn name(self) -> &'static str {
        match self {
            Self::Nearest => "near",
            Self::Bilinear => "bilinear",
            Self::Cubic => "cubic",
            Self::CubicSpline => "cubicspline",
            Self::Lanczos => "lanczos",
            Self::Average => "average",
            Self::Rms => "rms",
            Self::Mode => "mode",
            Self::Min => "min",
            Self::Max => "max",
            Self::Med => "med",
            Self::Q1 => "q1",
            Self::Q3 => "q3",
            Self::Sum => "sum",
        }
    }

    /// The radius of the interpolation kernel, in pixels (0 for the area-based methods).
    pub(crate) fn radius(self) -> i32 {
        match self {
            Self::Bilinear => 1,
            Self::Cubic | Self::CubicSpline => 2,
            Self::Lanczos => 3,
            _ => 0,
        }
    }

    /// Whether the method combines all the source pixels a destination pixel covers.
    pub(crate) fn is_area_based(self) -> bool {
        matches!(self, Self::Average | Self::Rms | Self::Mode | Self::Min | Self::Max | Self::Med | Self::Q1 | Self::Q3 | Self::Sum)
    }

    /// The interpolation kernel's weight at distance `x` (in pixels), as GDAL computes it. Weights
    /// are normalized by their sum, so the B-spline's constant factor is left out.
    pub(crate) fn weight(self, x: f64) -> f64 {
        match self {
            Self::Bilinear => {
                let a = x.abs();
                if a <= 1.0 { 1.0 - a } else { 0.0 }
            }
            Self::Cubic => {
                let a = x.abs();
                let x2 = x * x;
                if a <= 1.0 {
                    x2 * (1.5 * a - 2.5) + 1.0
                } else if a <= 2.0 {
                    x2 * (-0.5 * a + 2.5) - 4.0 * a + 2.0
                } else {
                    0.0
                }
            }
            Self::CubicSpline => {
                let (xp2, xp1, xm1) = (x + 2.0, x + 1.0, x - 1.0);
                if xp2 <= 0.0 {
                    return 0.0;
                }
                let mut v = xp2 * xp2 * xp2;
                if xp1 > 0.0 {
                    v += -4.0 * xp1 * xp1 * xp1;
                    if x > 0.0 {
                        v += 6.0 * x * x * x;
                        if xm1 > 0.0 {
                            v += -4.0 * xm1 * xm1 * xm1;
                        }
                    }
                }
                v
            }
            Self::Lanczos => {
                if x == 0.0 {
                    return 1.0;
                }
                if x.abs() > 3.0 {
                    return 0.0;
                }
                let pix = std::f64::consts::PI * x;
                let pix_over_r = pix / 3.0;
                let s = pix_over_r.sin();
                let s2 = s * s;
                (3.0 - 4.0 * s2) * s2 / (pix * pix_over_r)
            }
            _ => {
                if x.abs() < 0.5 {
                    1.0
                } else {
                    0.0
                }
            }
        }
    }
}

/// Parses a GDAL data type name (`Byte`, `Int8`, `UInt16`, `Int16`, `UInt32`, `Int32`, `UInt64`,
/// `Int64`, `Float32`, `Float64`), also accepting terrio's names (`uint8`, `f32`...).
pub fn parse_data_type(s: &str) -> Result<DataType> {
    let dt = match s.to_ascii_lowercase().as_str() {
        "byte" | "uint8" => Some(DataType::U8),
        other => DataType::from_str(other),
    };
    dt.ok_or_else(|| Error::InvalidArgument(format!("unknown data type '{s}'")))
}

/// GDAL's name for a data type.
pub fn data_type_name(dt: DataType) -> &'static str {
    match dt {
        DataType::U8 => "Byte",
        DataType::I8 => "Int8",
        DataType::U16 => "UInt16",
        DataType::I16 => "Int16",
        DataType::U32 => "UInt32",
        DataType::I32 => "Int32",
        DataType::U64 => "UInt64",
        DataType::I64 => "Int64",
        DataType::F32 => "Float32",
        DataType::F64 => "Float64",
    }
}

/// The smallest and largest values of an integer type (infinite for floating-point types).
pub(crate) fn type_range(dt: DataType) -> (f64, f64) {
    match dt {
        DataType::U8 => (0.0, 255.0),
        DataType::I8 => (-128.0, 127.0),
        DataType::U16 => (0.0, 65535.0),
        DataType::I16 => (-32768.0, 32767.0),
        DataType::U32 => (0.0, 4294967295.0),
        DataType::I32 => (-2147483648.0, 2147483647.0),
        DataType::U64 => (0.0, 18446744073709551615.0),
        DataType::I64 => (-9223372036854775808.0, 9223372036854775807.0),
        DataType::F32 | DataType::F64 => (f64::NEG_INFINITY, f64::INFINITY),
    }
}

pub(crate) fn is_integer(dt: DataType) -> bool {
    !matches!(dt, DataType::F32 | DataType::F64)
}

/// Converts a value to a data type as GDAL's `GDALCopyWords` does: integers are rounded half away
/// from zero and clamped to the type's range, NaN becomes 0, and `Float32` keeps single precision.
pub(crate) fn convert_value(v: f64, dt: DataType) -> f64 {
    match dt {
        DataType::F64 => v,
        DataType::F32 => v as f32 as f64,
        _ => {
            if v.is_nan() {
                return 0.0;
            }
            let (lo, hi) = type_range(dt);
            v.round().clamp(lo, hi)
        }
    }
}

/// Converts a value as GDAL's warp kernel does: integers are rounded half up (`floor(v + 0.5)`) and
/// clamped.
pub(crate) fn warp_round(v: f64, dt: DataType) -> f64 {
    match dt {
        DataType::F64 => v,
        DataType::F32 => v as f32 as f64,
        _ => {
            if v.is_nan() {
                return 0.0;
            }
            let (lo, hi) = type_range(dt);
            if v < lo {
                lo
            } else if v > hi {
                hi
            } else {
                (v + 0.5).floor()
            }
        }
    }
}

/// Whether `v` is the nodata value `nodata` (NaN matches NaN).
#[inline]
pub(crate) fn is_nodata(v: f64, nodata: f64) -> bool {
    if nodata.is_nan() { v.is_nan() } else { v == nodata }
}

/// Inverts a north-up or rotated geotransform.
pub(crate) fn invert_gt(gt: &[f64; 6]) -> Option<[f64; 6]> {
    if gt[2] == 0.0 && gt[4] == 0.0 {
        if gt[1] == 0.0 || gt[5] == 0.0 {
            return None;
        }
        return Some([-gt[0] / gt[1], 1.0 / gt[1], 0.0, -gt[3] / gt[5], 0.0, 1.0 / gt[5]]);
    }
    let det = gt[1] * gt[5] - gt[2] * gt[4];
    if det.abs() < 1e-15 {
        return None;
    }
    let inv = 1.0 / det;
    Some([
        (gt[2] * gt[3] - gt[0] * gt[5]) * inv,
        gt[5] * inv,
        -gt[2] * inv,
        (-gt[1] * gt[3] + gt[0] * gt[4]) * inv,
        -gt[4] * inv,
        gt[1] * inv,
    ])
}

/// Applies a geotransform to pixel/line coordinates.
#[inline]
pub(crate) fn apply_gt(gt: &[f64; 6], x: f64, y: f64) -> (f64, f64) {
    (gt[0] + x * gt[1] + y * gt[2], gt[3] + x * gt[4] + y * gt[5])
}

/// The raster's CRS in the form rasters store it.
pub(crate) fn crs_info(srs: &SpatialRef) -> CrsInfo {
    match srs.epsg {
        Some(code) => {
            let mut c = CrsInfo::from_epsg(code);
            if c.wkt.is_none() {
                c.wkt = srs.to_wkt();
            }
            c
        }
        None => CrsInfo { epsg: None, wkt: srs.to_wkt(), proj4: srs.proj4.clone() },
    }
}

/// One band of pixels as `f64`.
pub(crate) fn band_f64(r: &Raster, band: usize) -> Vec<f64> {
    let n = r.rows * r.cols;
    let start = band * n;
    macro_rules! slice {
        ($v:expr) => {
            $v[start..start + n].iter().map(|x| *x as f64).collect()
        };
    }
    match &r.data {
        RasterData::U8(v) => slice!(v),
        RasterData::I8(v) => slice!(v),
        RasterData::U16(v) => slice!(v),
        RasterData::I16(v) => slice!(v),
        RasterData::U32(v) => slice!(v),
        RasterData::I32(v) => slice!(v),
        RasterData::U64(v) => slice!(v),
        RasterData::I64(v) => slice!(v),
        RasterData::F32(v) => slice!(v),
        RasterData::F64(v) => v[start..start + n].to_vec(),
    }
}

/// A raster grid: size and geotransform.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Grid {
    /// Width in pixels.
    pub width: usize,
    /// Height in pixels.
    pub height: usize,
    /// GDAL-style geotransform (north-up: `gt[2] == gt[4] == 0`, `gt[5] < 0`).
    pub geo_transform: [f64; 6],
}

impl Grid {
    /// The grid of a raster.
    pub fn of(r: &Raster) -> Self {
        Self { width: r.cols, height: r.rows, geo_transform: crate::geo_transform(r) }
    }

    /// The extent `(min_x, min_y, max_x, max_y)`.
    pub fn bounds(&self) -> (f64, f64, f64, f64) {
        let gt = &self.geo_transform;
        let x1 = gt[0] + gt[1] * self.width as f64;
        let y1 = gt[3] + gt[5] * self.height as f64;
        (gt[0].min(x1), gt[3].min(y1), gt[0].max(x1), gt[3].max(y1))
    }
}

/// Builds a raster dataset from bands of `f64` values, converting them with `convert`.
pub(crate) fn build_raster(
    grid: Grid,
    bands: Vec<Vec<f64>>,
    data_type: DataType,
    nodata: Option<f64>,
    srs: &SpatialRef,
    convert: fn(f64, DataType) -> f64,
) -> Result<RasterDataset> {
    let gt = grid.geo_transform;
    if gt[2] != 0.0 || gt[4] != 0.0 || gt[1] <= 0.0 || gt[5] >= 0.0 {
        return Err(Error::Unsupported("only north-up rasters are supported".into()));
    }
    let nbands = bands.len().max(1);
    let n = grid.width * grid.height;
    let mut data = RasterData::new_filled(data_type, n * nbands, 0.0);
    for (b, band) in bands.iter().enumerate() {
        macro_rules! fill {
            ($v:expr, $t:ty) => {
                for (o, x) in $v[b * n..(b + 1) * n].iter_mut().zip(band) {
                    *o = convert(*x, data_type) as $t;
                }
            };
        }
        match &mut data {
            RasterData::U8(v) => fill!(v, u8),
            RasterData::I8(v) => fill!(v, i8),
            RasterData::U16(v) => fill!(v, u16),
            RasterData::I16(v) => fill!(v, i16),
            RasterData::U32(v) => fill!(v, u32),
            RasterData::I32(v) => fill!(v, i32),
            RasterData::U64(v) => fill!(v, u64),
            RasterData::I64(v) => fill!(v, i64),
            RasterData::F32(v) => fill!(v, f32),
            RasterData::F64(v) => fill!(v, f64),
        }
    }
    let cfg = RasterConfig {
        cols: grid.width,
        rows: grid.height,
        bands: nbands,
        x_min: gt[0],
        y_min: gt[3] + gt[5] * grid.height as f64,
        cell_size: gt[1],
        cell_size_y: Some(-gt[5]),
        nodata: nodata.unwrap_or(default_nodata(data_type)),
        data_type,
        crs: crs_info(srs),
        metadata: Vec::new(),
    };
    let raster = Raster::from_data_native(cfg, data)?;
    Ok(RasterDataset::new(raster, nodata.is_some()))
}

/// The nodata value stored in rasters that declare none (rasters always carry one).
fn default_nodata(dt: DataType) -> f64 {
    match dt {
        DataType::U8 | DataType::U16 | DataType::U32 | DataType::U64 => 0.0,
        _ => -9999.0,
    }
}

/// A coordinate transformation between two CRSs, choosing datum operations as PROJ does for
/// GDAL by default: NAD83 and WGS 84 are treated as equivalent ("ballpark" transformation).
pub(crate) struct Ct {
    src: wbprojection::Crs,
    dst: wbprojection::Crs,
}

impl Ct {
    /// A transformation from `src` to `dst`.
    pub(crate) fn new(src: &SpatialRef, dst: &SpatialRef) -> Result<Self> {
        Ok(Self { src: src.to_wbp()?, dst: dst.to_wbp()? })
    }

    /// Transforms one point.
    pub(crate) fn transform(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        Ok(self.src.transform_to_with_policy(x, y, &self.dst, wbprojection::CrsTransformPolicy::Auto)?)
    }
}

/// Transforms a bounding box between two CRSs, as GDAL's `OGRCoordinateTransformation::TransformBounds`
/// does with `densify` extra points per edge.
pub(crate) fn transform_bounds(t: &Ct, b: (f64, f64, f64, f64), densify: usize) -> Result<(f64, f64, f64, f64)> {
    let steps = densify + 1;
    let (mut x0, mut y0, mut x1, mut y1) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for i in 0..=steps {
        let f = i as f64 / steps as f64;
        let x = b.0 + (b.2 - b.0) * f;
        let y = b.1 + (b.3 - b.1) * f;
        for (px, py) in [(x, b.1), (x, b.3), (b.0, y), (b.2, y)] {
            if let Ok((tx, ty)) = t.transform(px, py)
                && tx.is_finite()
                && ty.is_finite()
            {
                x0 = x0.min(tx);
                y0 = y0.min(ty);
                x1 = x1.max(tx);
                y1 = y1.max(ty);
            }
        }
    }
    if !x0.is_finite() {
        return Err(Error::Crs("no point of the bounds could be transformed".into()));
    }
    Ok((x0, y0, x1, y1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_like_gdal() {
        assert_eq!(convert_value(2.5, DataType::U8), 3.0);
        assert_eq!(convert_value(-2.5, DataType::I16), -3.0);
        assert_eq!(convert_value(300.0, DataType::U8), 255.0);
        assert_eq!(convert_value(-1.0, DataType::U8), 0.0);
        assert_eq!(convert_value(f64::NAN, DataType::I32), 0.0);
        assert_eq!(warp_round(-2.5, DataType::I16), -2.0);
    }

    #[test]
    fn kernels_are_normalizable() {
        for r in [Resampling::Bilinear, Resampling::Cubic, Resampling::CubicSpline, Resampling::Lanczos] {
            assert!((r.weight(0.0) - if r == Resampling::CubicSpline { 4.0 } else { 1.0 }).abs() < 1e-12, "{r:?}");
            let sum: f64 = (-3..=3).map(|i| r.weight(i as f64 + 0.3)).sum();
            assert!(sum > 0.0);
        }
        assert_eq!(Resampling::parse("near").unwrap(), Resampling::Nearest);
    }

    #[test]
    fn inverts_geotransforms() {
        let gt = [100.0, 2.0, 0.0, 50.0, 0.0, -3.0];
        let inv = invert_gt(&gt).unwrap();
        let (x, y) = apply_gt(&gt, 3.0, 4.0);
        let (c, r) = apply_gt(&inv, x, y);
        assert!((c - 3.0).abs() < 1e-12 && (r - 4.0).abs() < 1e-12);
    }
}
