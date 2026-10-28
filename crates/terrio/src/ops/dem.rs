//! Terrain analysis: an equivalent of `gdaldem`.
//!
//! Hillshade (with combined, multidirectional and Igor shading), slope, aspect, color relief,
//! the Terrain Ruggedness Index (Riley and Wilson), the Topographic Position Index and
//! roughness, computed as GDAL 3.12 computes them so that results can be compared pixel for
//! pixel:
//!
//! - every algorithm moves a 3×3 window over the band, reading Byte, Int16 and UInt16 bands as
//!   32-bit integers and the other types as `Float32`, with the arithmetic done in the same types
//!   and in the same order as GDAL;
//! - nodata is handled as GDAL handles it: a window whose centre is nodata gives nodata, and a
//!   window with a nodata neighbour gives nodata, or with `compute_edges` uses the centre value in
//!   place of the neighbour;
//! - the outermost rows and columns are nodata, or with `compute_edges` are computed from windows
//!   extended by linear extrapolation;
//! - the horizontal scale of geographic rasters is derived from the ellipsoid and the centre
//!   latitude unless given, as `gdaldem` does since GDAL 3.11;
//! - on x86-64, the default hillshade uses the same approximate reciprocal square root as GDAL's
//!   SSE code path on the rows where it uses it.

use super::Grid;
use crate::{Error, RasterDataset, Result};
use std::f64::consts::PI;
use wbraster::DataType;
use wbraster::raster::RasterData;

const D2R: f64 = PI / 180.0;
const R2D: f64 = 180.0 / PI;

/// How the gradient of a 3×3 window is computed (`-alg`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GradientAlg {
    /// Horn's formula (the default), better on rough terrain.
    #[default]
    Horn,
    /// Zevenbergen and Thorne's formula, better on smooth terrain.
    ZevenbergenThorne,
}

impl GradientAlg {
    /// Parses `Horn` or `ZevenbergenThorne` (case-insensitive).
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "horn" => Ok(Self::Horn),
            "zevenbergenthorne" => Ok(Self::ZevenbergenThorne),
            _ => Err(Error::InvalidArgument(format!("invalid gradient algorithm '{s}' (Horn, ZevenbergenThorne)"))),
        }
    }
}

/// The Terrain Ruggedness Index formula (`-alg` of `gdaldem TRI`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TriAlg {
    /// Riley et al. (1999): the square root of the sum of squared differences (the default).
    #[default]
    Riley,
    /// Wilson et al. (2007): the mean absolute difference, for bathymetry.
    Wilson,
}

impl TriAlg {
    /// Parses `Riley` or `Wilson` (case-insensitive).
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "riley" => Ok(Self::Riley),
            "wilson" => Ok(Self::Wilson),
            _ => Err(Error::InvalidArgument(format!("invalid TRI algorithm '{s}' (Riley, Wilson)"))),
        }
    }
}

/// The hillshade variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Shading {
    /// Lighting from one direction (`-az`, `-alt`).
    #[default]
    Standard,
    /// Hillshade combined with slope (`-combined`).
    Combined,
    /// Lighting from 225°, 270°, 315° and 360° combined (`-multidirectional`).
    MultiDirectional,
    /// Igor's shading, which darkens only the slopes facing away from the light (`-igor`).
    Igor,
}

/// How colors are chosen for values between the entries of a color file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorSelection {
    /// Linear interpolation between the surrounding entries (the default).
    #[default]
    Interpolate,
    /// The color of the nearest entry (`-nearest_color_entry`).
    Nearest,
    /// Only exact entries; other values get transparent black (`-exact_color_entry`).
    Exact,
}

/// Options for the DEM operations, named after `gdaldem`'s. Each operation uses the ones that
/// apply to it.
#[derive(Debug, Clone)]
pub struct DemOptions {
    /// GDAL arithmetic version; 3.12 preserves existing results.
    /// Standard hillshade in 3.13 follows CPU-dependent SIMD rounding; it is not bit-portable.
    pub kernel_version: super::warp::KernelVersion,
    /// The band to process, from 1 (`-b`).
    pub band: usize,
    /// Vertical exaggeration, for hillshade (`-z`).
    pub z: f64,
    /// Ratio of vertical to horizontal units, for both axes (`-s`; `111120` for a geographic
    /// raster in metres). By default derived from the CRS.
    pub scale: Option<f64>,
    /// The ratio for the horizontal axis (`-xscale`); needs `yscale`.
    pub xscale: Option<f64>,
    /// The ratio for the vertical axis (`-yscale`); needs `xscale`.
    pub yscale: Option<f64>,
    /// Azimuth of the light, in degrees clockwise from north (`-az`).
    pub azimuth: f64,
    /// Altitude of the light, in degrees above the horizon (`-alt`).
    pub altitude: f64,
    /// The hillshade variant.
    pub shading: Shading,
    /// The gradient formula for hillshade, slope and aspect (`-alg`).
    pub gradient: GradientAlg,
    /// Slope as a percentage rather than in degrees (`-p`).
    pub slope_percent: bool,
    /// Aspect as a trigonometric angle (counterclockwise from east) rather than an azimuth
    /// (`-trigonometric`).
    pub trigonometric: bool,
    /// Aspect of 0 rather than nodata for flat areas (`-zero_for_flat`).
    pub zero_for_flat: bool,
    /// The TRI formula (`-alg` of TRI).
    pub tri: TriAlg,
    /// Compute values at the raster edges and next to nodata too (`-compute_edges`).
    pub compute_edges: bool,
    /// Color selection for color relief.
    pub color_selection: ColorSelection,
    /// Add an alpha band to the color relief (`-alpha`).
    pub alpha: bool,
    /// For the operations written to a path a strip of rows at a time ([`dem_to_path`],
    /// [`color_relief_to_path`]): about how much memory, in MiB, the strips in flight may take
    /// (read ahead, computed, being written), which sets their height (like `gdalwarp -wm`). By
    /// default, strips of about 64 MiB of input values, as `dem_to_path` always used (about
    /// 300 to 550 MiB in flight for a Float32 DEM). The values do not depend on it. Below one row
    /// of the input's blocks, each strip decodes the blocks it touches again: slower, same values.
    pub max_memory_mib: Option<f64>,
}

impl Default for DemOptions {
    fn default() -> Self {
        Self {
            kernel_version: super::warp::KernelVersion::Gdal312,
            band: 1,
            z: 1.0,
            scale: None,
            xscale: None,
            yscale: None,
            azimuth: 315.0,
            altitude: 45.0,
            shading: Shading::Standard,
            gradient: GradientAlg::Horn,
            slope_percent: false,
            trigonometric: false,
            zero_for_flat: false,
            tri: TriAlg::Riley,
            compute_edges: false,
            color_selection: ColorSelection::Interpolate,
            alpha: false,
            max_memory_mib: None,
        }
    }
}

impl DemOptions {
    fn validate(&self) -> Result<()> {
        let bad = |what: &str| Err(Error::InvalidArgument(format!("invalid value for {what}")));
        if self.z.is_nan() || self.z <= 0.0 {
            return bad("z");
        }
        if self.scale.is_some_and(|s| s.is_nan() || s <= 0.0) {
            return bad("scale");
        }
        if self.xscale.is_some_and(|s| s.is_nan() || s <= 0.0) {
            return bad("xscale");
        }
        if self.yscale.is_some_and(|s| s.is_nan() || s <= 0.0) {
            return bad("yscale");
        }
        if self.altitude.is_nan() || self.altitude <= 0.0 {
            return bad("altitude");
        }
        if self.scale.is_some() && (self.xscale.is_some() || self.yscale.is_some()) {
            return Err(Error::InvalidArgument("scale and xscale/yscale exclude each other".into()));
        }
        if self.xscale.is_some() != self.yscale.is_some() {
            return Err(Error::InvalidArgument("xscale and yscale go together".into()));
        }
        if self.max_memory_mib.is_some_and(|m| m.is_nan() || m <= 0.0) {
            return bad("max_memory_mib");
        }
        Ok(())
    }
}

/// A source pixel type of the 3×3 algorithms: `i32` for Byte, Int16 and UInt16 bands, `f32` for
/// the others, as GDAL reads them.
trait Px: Copy + PartialOrd + Send + Sync {
    fn add(self, o: Self) -> Self;
    fn sub(self, o: Self) -> Self;
    fn abs(self) -> Self;
    fn f64(self) -> f64;
    fn f32(self) -> f32;
    /// `ComputeVal()`'s nodata test.
    fn is_nodata(self, nd: Self, nd_is_nan: bool) -> bool;
    /// The test that flags a line as holding nodata.
    fn line_nodata(self, nd: Self) -> bool;
    /// `INTERPOL()`: `2a - b`, kept off the nodata value.
    fn interpol(a: Self, b: Self, has_nd: bool, nd: Self) -> Self;
}

/// GDAL's `ARE_REAL_EQUAL()` for floats.
#[inline]
fn are_real_equal(a: f32, b: f32) -> bool {
    a == b || (a - b).abs() < f32::EPSILON * (a + b).abs() * 2.0
}

impl Px for i32 {
    #[inline]
    fn add(self, o: Self) -> Self {
        self.wrapping_add(o)
    }
    #[inline]
    fn sub(self, o: Self) -> Self {
        self.wrapping_sub(o)
    }
    #[inline]
    fn abs(self) -> Self {
        self.wrapping_abs()
    }
    #[inline]
    fn f64(self) -> f64 {
        self as f64
    }
    #[inline]
    fn f32(self) -> f32 {
        self as f32
    }
    #[inline]
    fn is_nodata(self, nd: Self, _: bool) -> bool {
        self == nd
    }
    #[inline]
    fn line_nodata(self, nd: Self) -> bool {
        self == nd
    }
    #[inline]
    fn interpol(a: Self, b: Self, has_nd: bool, nd: Self) -> Self {
        if has_nd && (a == nd || b == nd) {
            return nd;
        }
        let v = (2 * a as i64 - b as i64).clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        if has_nd && v == nd {
            return if v == i32::MAX { i32::MAX - 1 } else { v + 1 };
        }
        v
    }
}

impl Px for f32 {
    #[inline]
    fn add(self, o: Self) -> Self {
        self + o
    }
    #[inline]
    fn sub(self, o: Self) -> Self {
        self - o
    }
    #[inline]
    fn abs(self) -> Self {
        f32::abs(self)
    }
    #[inline]
    fn f64(self) -> f64 {
        self as f64
    }
    #[inline]
    fn f32(self) -> f32 {
        self
    }
    #[inline]
    fn is_nodata(self, nd: Self, nd_is_nan: bool) -> bool {
        if nd_is_nan { self.is_nan() } else { are_real_equal(self, nd) }
    }
    #[inline]
    fn line_nodata(self, nd: Self) -> bool {
        self == nd || self.is_nan()
    }
    #[inline]
    fn interpol(a: Self, b: Self, has_nd: bool, nd: Self) -> Self {
        if has_nd && (are_real_equal(a, nd) || are_real_equal(b, nd)) {
            return nd;
        }
        let v = 2.0 * a - b;
        if has_nd && are_real_equal(v, nd) {
            return nd * (1.0 + 3.0 * f32::EPSILON);
        }
        v
    }
}

/// `((w[a] + w[b]) + w[b]) + w[c]`, GDAL's weighted sum of three window cells.
#[inline]
fn sum121<T: Px>(w: &[T; 9], a: usize, b: usize, c: usize) -> T {
    w[a].add(w[b]).add(w[b]).add(w[c])
}

/// Horn's gradient numerators, as `((w0 + w3 + w3 + w6) - (w2 + w5 + w5 + w8))` and
/// `((w6 + w7 + w7 + w8) - (w0 + w1 + w1 + w2))`.
#[inline]
fn horn<T: Px>(w: &[T; 9]) -> (T, T) {
    (sum121(w, 0, 3, 6).sub(sum121(w, 2, 5, 8)), sum121(w, 6, 7, 8).sub(sum121(w, 0, 1, 2)))
}

/// The gradient `(x, y)` of a window scaled by the inverse resolutions.
#[inline]
fn gradient<T: Px>(w: &[T; 9], alg: GradientAlg, inv_ew: f64, inv_ns: f64) -> (f64, f64) {
    match alg {
        GradientAlg::Horn => {
            let (x, y) = horn(w);
            (x.f64() * inv_ew, y.f64() * inv_ns)
        }
        GradientAlg::ZevenbergenThorne => (w[3].sub(w[5]).f64() * inv_ew, w[7].sub(w[1]).f64() * inv_ns),
    }
}

/// The parameters of the standard, combined and Igor hillshades (`GDALHillshadeAlgData`).
#[derive(Debug, Clone, Copy)]
struct HillshadeParams {
    alg: GradientAlg,
    inv_ns: f64,
    inv_ew: f64,
    sin_alt: f64,
    az: f64,
    z_factor: f64,
    cos_az_mul_cos_alt_mul_z: f64,
    sin_az_mul_cos_alt_mul_z: f64,
    square_z: f64,
    sin_alt_mul_254: f64,
    cos_az_mul_cos_alt_mul_z_mul_254: f64,
    sin_az_mul_cos_alt_mul_z_mul_254: f64,
    square_z_mul_square_inv_res: f64,
    cos_az_mul_cos_alt_mul_z_mul_254_mul_inv_res: f64,
    sin_az_mul_cos_alt_mul_z_mul_254_mul_inv_res: f64,
}

impl HillshadeParams {
    fn new(gt: &[f64; 6], o: &DemOptions, xscale: f64, yscale: f64) -> Self {
        let inv_ns = 1.0 / (gt[5] * yscale);
        let inv_ew = 1.0 / (gt[1] * xscale);
        let sin_alt = (o.altitude * D2R).sin();
        let az = o.azimuth * D2R;
        let z_factor = o.z / if o.gradient == GradientAlg::ZevenbergenThorne { 2.0 } else { 8.0 };
        let cos_alt_mul_z = (o.altitude * D2R).cos() * z_factor;
        let cos_az_mul_cos_alt_mul_z = az.cos() * cos_alt_mul_z;
        let sin_az_mul_cos_alt_mul_z = az.sin() * cos_alt_mul_z;
        let square_z = z_factor * z_factor;
        let sin_alt_mul_254 = 254.0 * sin_alt;
        let cos_az_mul_cos_alt_mul_z_mul_254 = 254.0 * cos_az_mul_cos_alt_mul_z;
        let sin_az_mul_cos_alt_mul_z_mul_254 = 254.0 * sin_az_mul_cos_alt_mul_z;
        let same_res = gt[1] == -gt[5] && xscale == yscale;
        Self {
            alg: o.gradient,
            inv_ns,
            inv_ew,
            sin_alt,
            az,
            z_factor,
            cos_az_mul_cos_alt_mul_z,
            sin_az_mul_cos_alt_mul_z,
            square_z,
            sin_alt_mul_254,
            cos_az_mul_cos_alt_mul_z_mul_254,
            sin_az_mul_cos_alt_mul_z_mul_254,
            square_z_mul_square_inv_res: if same_res { square_z * inv_ew * inv_ew } else { 0.0 },
            cos_az_mul_cos_alt_mul_z_mul_254_mul_inv_res: if same_res { cos_az_mul_cos_alt_mul_z_mul_254 * -inv_ew } else { 0.0 },
            sin_az_mul_cos_alt_mul_z_mul_254_mul_inv_res: if same_res { sin_az_mul_cos_alt_mul_z_mul_254 * inv_ew } else { 0.0 },
        }
    }

    fn standard<T: Px>(&self, w: &[T; 9]) -> f32 {
        let (x, y) = gradient(w, self.alg, self.inv_ew, self.inv_ns);
        let xx_plus_yy = x * x + y * y;
        let cang = (self.sin_alt_mul_254 - (y * self.cos_az_mul_cos_alt_mul_z_mul_254 - x * self.sin_az_mul_cos_alt_mul_z_mul_254))
            / (1.0 + self.square_z * xx_plus_yy).sqrt();
        (if cang <= 0.0 { 1.0 } else { 1.0 + cang }) as f32
    }

    /// The x and y numerators of the same-resolution hillshade, accumulated as GDAL does.
    #[inline]
    fn same_res_acc<T: Px>(w: &[T; 9]) -> (f64, f64) {
        let mut acc_x = w[0].sub(w[8]);
        let six_minus_two = w[6].sub(w[2]);
        let mut acc_y = acc_x;
        let three_minus_five = w[3].sub(w[5]);
        let one_minus_seven = w[1].sub(w[7]);
        acc_x = acc_x.add(three_minus_five);
        acc_y = acc_y.add(one_minus_seven);
        acc_x = acc_x.add(three_minus_five);
        acc_y = acc_y.add(one_minus_seven);
        acc_x = acc_x.add(six_minus_two);
        acc_y = acc_y.sub(six_minus_two);
        (acc_x.f64(), acc_y.f64())
    }

    fn same_res<T: Px>(&self, w: &[T; 9]) -> f32 {
        let (x, y) = Self::same_res_acc(w);
        let xx_plus_yy = x * x + y * y;
        let cang = (self.sin_alt_mul_254
            + (x * self.sin_az_mul_cos_alt_mul_z_mul_254_mul_inv_res + y * self.cos_az_mul_cos_alt_mul_z_mul_254_mul_inv_res))
            / (1.0 + self.square_z_mul_square_inv_res * xx_plus_yy).sqrt();
        (if cang <= 0.0 { 1.0 } else { 1.0 + cang }) as f32
    }

    /// The same-resolution hillshade of GDAL's SSE code path, which uses an approximate
    /// reciprocal square root refined by one Newton-Raphson step.
    #[cfg(target_arch = "x86_64")]
    fn same_res_simd<T: Px>(&self, w: &[T; 9]) -> f32 {
        let (x, y) = Self::same_res_acc(w);
        let xx_plus_yy = x * x + y * y;
        let num = self.sin_alt_mul_254
            + self.sin_az_mul_cos_alt_mul_z_mul_254_mul_inv_res * x
            + self.cos_az_mul_cos_alt_mul_z_mul_254_mul_inv_res * y;
        let denom = 1.0 + self.square_z_mul_square_inv_res * xx_plus_yy;
        let r = approx_inv_sqrt(denom);
        let res = (num * r) as f32 + 1.0;
        if 1.0 > res { 1.0 } else { res }
    }

    fn combined<T: Px>(&self, w: &[T; 9]) -> f32 {
        const INV_SQUARE_OF_HALF_PI: f64 = 1.0 / ((PI * PI) / 4.0);
        let (x, y) = gradient(w, self.alg, self.inv_ew, self.inv_ns);
        let xx_plus_yy = x * x + y * y;
        let slope = xx_plus_yy * self.square_z;
        let mut cang =
            ((self.sin_alt - (y * self.cos_az_mul_cos_alt_mul_z - x * self.sin_az_mul_cos_alt_mul_z)) / (1.0 + slope).sqrt()).acos();
        cang = 1.0 - cang * slope.sqrt().atan() * INV_SQUARE_OF_HALF_PI;
        if cang <= 0.0 { 1.0 } else { (1.0 + 254.0 * cang) as f32 }
    }

    fn igor<T: Px>(&self, w: &[T; 9]) -> f32 {
        let (gx, gy) = gradient(w, self.alg, self.inv_ew, self.inv_ns);
        let slope_degrees = ((gx * gx + gy * gy).sqrt() * self.z_factor).atan() * R2D;
        let aspect = match self.alg {
            GradientAlg::Horn => {
                let dx = sum121(w, 2, 5, 8).sub(sum121(w, 0, 3, 6)).f64();
                let dy = sum121(w, 6, 7, 8).sub(sum121(w, 0, 1, 2)).f64();
                dy.atan2(-dx)
            }
            GradientAlg::ZevenbergenThorne => {
                let dx = w[5].sub(w[3]).f64();
                let dy = w[7].sub(w[1]).f64();
                dy.atan2(-dx)
            }
        };
        let slope_strength = slope_degrees / 90.0;
        let aspect_diff = difference_between_angles(aspect, PI * 3.0 / 2.0 - self.az, PI * 2.0);
        let aspect_strength = 1.0 - aspect_diff / PI;
        let shadowness = 1.0 - slope_strength * aspect_strength;
        (255.0 * shadowness) as f32
    }
}

/// GDAL's `ApproxADivByInvSqrtB()` reciprocal square root: `_mm_rsqrt_ps` on the value rounded
/// to single precision, then one Newton-Raphson step in double precision.
#[cfg(target_arch = "x86_64")]
#[inline]
fn approx_inv_sqrt(b: f64) -> f64 {
    use std::arch::x86_64::{_mm_cvtss_f32, _mm_rsqrt_ss, _mm_set_ss};
    // SAFETY: SSE is part of the x86-64 baseline, so these instructions are always available.
    let r = unsafe { _mm_cvtss_f32(_mm_rsqrt_ss(_mm_set_ss(b as f32))) } as f64;
    r * (1.5 - (b * 0.5) * (r * r))
}

fn normalize_angle(angle: f64, normalizer: f64) -> f64 {
    let a = angle % normalizer;
    if a < 0.0 { normalizer + a } else { a }
}

fn difference_between_angles(a1: f64, a2: f64, normalizer: f64) -> f64 {
    let mut diff = (normalize_angle(a1, normalizer) - normalize_angle(a2, normalizer)).abs();
    if diff > normalizer / 2.0 {
        diff = normalizer - diff;
    }
    diff
}

/// The parameters of the multidirectional hillshade.
#[derive(Debug, Clone, Copy)]
struct MultiDirectionalParams {
    alg: GradientAlg,
    inv_ns: f64,
    inv_ew: f64,
    square_z: f64,
    sin_alt_mul_127: f64,
    sin_alt_mul_254: f64,
    cos_alt_mul_z_mul_127: f64,
    cos225_az_mul_cos_alt_mul_z_mul_127: f64,
}

impl MultiDirectionalParams {
    fn new(gt: &[f64; 6], o: &DemOptions, xscale: f64, yscale: f64) -> Self {
        let z_factor = o.z / if o.gradient == GradientAlg::ZevenbergenThorne { 2.0 } else { 8.0 };
        let cos_alt_mul_z = (o.altitude * D2R).cos() * z_factor;
        Self {
            alg: o.gradient,
            inv_ns: 1.0 / (gt[5] * yscale),
            inv_ew: 1.0 / (gt[1] * xscale),
            square_z: z_factor * z_factor,
            sin_alt_mul_127: 127.0 * (o.altitude * D2R).sin(),
            sin_alt_mul_254: 254.0 * (o.altitude * D2R).sin(),
            cos_alt_mul_z_mul_127: 127.0 * cos_alt_mul_z,
            cos225_az_mul_cos_alt_mul_z_mul_127: 127.0 * (225.0 * D2R).cos() * cos_alt_mul_z,
        }
    }

    fn shade<T: Px>(&self, w: &[T; 9]) -> f32 {
        let (x, y) = gradient(w, self.alg, self.inv_ew, self.inv_ns);
        let xx = x * x;
        let yy = y * y;
        let xx_plus_yy = xx + yy;
        if xx_plus_yy == 0.0 {
            return (1.0 + self.sin_alt_mul_254) as f32;
        }
        let clamp0 = |v: f64| if v <= 0.0 { 0.0 } else { v };
        let val225 = clamp0(self.sin_alt_mul_127 + (x - y) * self.cos225_az_mul_cos_alt_mul_z_mul_127);
        let val270 = clamp0(self.sin_alt_mul_127 - x * self.cos_alt_mul_z_mul_127);
        let val315 = clamp0(self.sin_alt_mul_127 + (x + y) * self.cos225_az_mul_cos_alt_mul_z_mul_127);
        let val360 = clamp0(self.sin_alt_mul_127 - y * self.cos_alt_mul_z_mul_127);
        let weight_225 = 0.5 * xx_plus_yy - x * y;
        let weight_270 = xx;
        let weight_315 = xx_plus_yy - weight_225;
        let weight_360 = yy;
        let cang = ((weight_225 * val225 + weight_270 * val270 + weight_315 * val315 + weight_360 * val360) / xx_plus_yy)
            / (1.0 + self.square_z * xx_plus_yy).sqrt();
        (1.0 + cang) as f32
    }
}

/// GDAL 3.13 changed terrain arithmetic and parameters to Float32.
#[derive(Clone, Copy)]
struct Terrain313 {
    alg: GradientAlg,
    inv_ew: f32,
    inv_ns: f32,
    sin_alt: f32,
    cos_az: f32,
    sin_az: f32,
    square_z: f32,
    same_res: bool,
    same_square: f32,
    same_x: f32,
    same_y: f32,
}
impl Terrain313 {
    fn new(gt: &[f64; 6], o: &DemOptions, xs: f64, ys: f64) -> Self {
        let inv_ew = (1.0 / (gt[1] * xs)) as f32;
        let inv_ns = (1.0 / (gt[5] * ys)) as f32;
        let alt = o.altitude as f32 * D2R as f32;
        let az = o.azimuth as f32 * D2R as f32;
        let z = (o.z / if o.gradient == GradientAlg::Horn { 8.0 } else { 2.0 }) as f32;
        let ca = alt.cos() * z;
        let cos_az = az.cos() * ca;
        let sin_az = az.sin() * ca;
        let square_z = z * z;
        Self {
            alg: o.gradient,
            inv_ew,
            inv_ns,
            sin_alt: 254.0 * alt.sin(),
            cos_az: 254.0 * cos_az,
            sin_az: 254.0 * sin_az,
            square_z,
            same_res: gt[1] == -gt[5] && xs == ys,
            same_square: square_z * inv_ew * inv_ew,
            same_x: (254.0 * sin_az) * inv_ew,
            same_y: (254.0 * cos_az) * -inv_ew,
        }
    }
    fn numerator<T: Px>(&self, w: &[T; 9]) -> (f32, f32) {
        let (x, y) = match self.alg {
            GradientAlg::Horn => horn(w),
            GradientAlg::ZevenbergenThorne => (w[3].sub(w[5]), w[7].sub(w[1])),
        };
        (x.f32(), y.f32())
    }
    fn shade<T: Px>(&self, w: &[T; 9], multi: bool) -> f32 {
        let (num, den) = if self.same_res && self.alg == GradientAlg::Horn {
            let (x, y) = HillshadeParams::same_res_acc(w);
            let (x, y) = (x as f32, y as f32);
            let terms =
                if multi { (self.sin_alt + self.same_x * x) + self.same_y * y } else { self.sin_alt + (x * self.same_x + y * self.same_y) };
            (terms, 1.0 + self.same_square * (x * x + y * y))
        } else {
            let (x, y) = self.numerator(w);
            let (x, y) = (x * self.inv_ew, y * self.inv_ns);
            (self.sin_alt - (y * self.cos_az - x * self.sin_az), 1.0 + self.square_z * (x * x + y * y))
        };
        let ratio = divide_sqrt313(num, den, multi);
        if ratio <= 0.0 { 1.0 } else { 1.0 + ratio }
    }
}

/// Match the reference's x86 estimate/refinement path, not a cross-CPU bit pattern.
/// Intel and AMD rsqrt estimates can differ; non-x86 uses exact division/sqrt.
#[inline]
fn divide_sqrt313(numerator: f32, denominator: f32, multi: bool) -> f32 {
    if !multi {
        return numerator / denominator.sqrt();
    }
    #[cfg(target_arch = "x86_64")]
    {
        use std::arch::x86_64::{_mm_cvtss_f32, _mm_rsqrt_ss, _mm_set_ss};
        // SAFETY: SSE is in the x86-64 baseline. GDAL refines the estimate in Float32.
        let estimate = unsafe { _mm_cvtss_f32(_mm_rsqrt_ss(_mm_set_ss(denominator))) };
        numerator * (estimate * (1.5 - (denominator * 0.5) * (estimate * estimate)))
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        numerator / denominator.sqrt()
    }
}

fn slope313<T: Px>(w: &[T; 9], alg: GradientAlg, ew: f64, ns: f64, percent: bool) -> f32 {
    let (x, y, div) = match alg {
        GradientAlg::Horn => {
            let (x, y) = horn(w);
            (x.f32(), y.f32(), 8.0)
        }
        GradientAlg::ZevenbergenThorne => (w[3].sub(w[5]).f32(), w[7].sub(w[1]).f32(), 2.0),
    };
    let dx = x * (1.0 / ew as f32);
    let dy = y * (1.0 / ns as f32);
    let root = (dx * dx + dy * dy).sqrt();
    if percent { (100.0 / div) * root } else { (root * (1.0 / div)).atan() * R2D as f32 }
}

/// Slope, in degrees or percent.
fn slope<T: Px>(w: &[T; 9], alg: GradientAlg, ewres_xscale: f64, nsres_yscale: f64, percent: bool) -> f32 {
    let (dx, dy, div) = match alg {
        GradientAlg::Horn => {
            let (x, y) = horn(w);
            (x.f64() / ewres_xscale, y.f64() / nsres_yscale, 8.0)
        }
        GradientAlg::ZevenbergenThorne => (w[3].sub(w[5]).f64() / ewres_xscale, w[7].sub(w[1]).f64() / nsres_yscale, 2.0),
    };
    let key = dx * dx + dy * dy;
    if percent { (100.0 * (key.sqrt() / div)) as f32 } else { ((key.sqrt() / div).atan() * R2D) as f32 }
}

/// Aspect, as an azimuth or a trigonometric angle; `flat` for flat areas.
fn aspect<T: Px>(w: &[T; 9], alg: GradientAlg, azimuth: bool, flat: f32) -> f32 {
    let (dx, dy) = match alg {
        GradientAlg::Horn => (sum121(w, 2, 5, 8).sub(sum121(w, 0, 3, 6)).f64(), sum121(w, 6, 7, 8).sub(sum121(w, 0, 1, 2)).f64()),
        GradientAlg::ZevenbergenThorne => (w[5].sub(w[3]).f64(), w[7].sub(w[1]).f64()),
    };
    let mut a = (dy.atan2(-dx) / D2R) as f32;
    if dx == 0.0 && dy == 0.0 {
        a = flat;
    } else if azimuth {
        a = if a > 90.0 { 450.0 - a } else { 90.0 - a };
    } else if a < 0.0 {
        a += 360.0;
    }
    if a == 360.0 { 0.0 } else { a }
}

fn tri_wilson<T: Px>(w: &[T; 9]) -> f32 {
    let c = w[4];
    let s = w[0].sub(c).abs().add(w[1].sub(c).abs()).add(w[2].sub(c).abs()).add(w[3].sub(c).abs());
    let s = s.add(w[5].sub(c).abs()).add(w[6].sub(c).abs()).add(w[7].sub(c).abs()).add(w[8].sub(c).abs());
    s.f32() * 0.125
}

fn tri_riley<T: Px>(w: &[T; 9]) -> f32 {
    let sq = |k: usize| {
        let d = w[k].sub(w[4]).f64();
        d * d
    };
    (sq(0) + sq(1) + sq(2) + sq(3) + sq(5) + sq(6) + sq(7) + sq(8)).sqrt() as f32
}

fn tpi<T: Px>(w: &[T; 9]) -> f32 {
    let s = w[0].add(w[1]).add(w[2]).add(w[3]).add(w[5]).add(w[6]).add(w[7]).add(w[8]);
    w[4].f32() - s.f32() * 0.125
}

fn roughness<T: Px>(w: &[T; 9]) -> f32 {
    let (mut lo, mut hi) = (w[0], w[0]);
    for &v in &w[1..] {
        if v > hi {
            hi = v;
        }
        if v < lo {
            lo = v;
        }
    }
    hi.sub(lo).f32()
}

/// A 3×3 algorithm.
type Alg<'a, T> = &'a (dyn Fn(&[T; 9]) -> f32 + Sync);

/// A 3×3 algorithm, with the optional vectorized version GDAL uses on rows free of nodata.
struct Kernel<'a, T> {
    f: Alg<'a, T>,
    /// Computes columns `1, 1 + 4, ...` in groups of four while `j < width - 4`, returning the
    /// first column it did not compute.
    multi: Option<Alg<'a, T>>,
}

/// The rows of a band a 3×3 algorithm runs over: `src` holds rows `src_rows` of a band of
/// `width` × `height` pixels, and the algorithm computes rows `out_rows`, which `src_rows` covers
/// with a row more on each side (where the band has one).
struct Rows<'a, T> {
    src: &'a [T],
    width: usize,
    height: usize,
    src_rows: std::ops::Range<usize>,
    out_rows: std::ops::Range<usize>,
}

/// `GDALGeneric3x3Processing()`: runs a 3×3 algorithm over rows of a band, returning the rows
/// computed. The values do not depend on how the band is split in rows.
fn process<T: Px>(rows: Rows<T>, nodata: Option<T>, dst_nodata: f32, edges: bool, k: &Kernel<T>) -> Vec<f32> {
    let Rows { src, width: w, height: h, src_rows, out_rows } = rows;
    let (s0, o0, o1) = (src_rows.start, out_rows.start, out_rows.end);
    let mut out = vec![dst_nodata; w * (o1 - o0)];
    if w == 0 || h == 0 || o1 <= o0 {
        return out;
    }
    let has_nd = nodata.is_some();
    let nd = nodata.unwrap_or_else(|| src[0]);
    let nd_nan = has_nd && nd.f64().is_nan();
    let line_nd: Vec<bool> = (0..src_rows.len()).map(|r| has_nd && src[r * w..(r + 1) * w].iter().any(|v| v.line_nodata(nd))).collect();
    let line_nd = |r: usize| line_nd[r - s0];
    let row = |r: usize| &src[(r - s0) * w..(r - s0 + 1) * w];
    let interpol = |a: T, b: T| T::interpol(a, b, has_nd, nd);
    let compute = |mut win: [T; 9], check: bool| -> f32 {
        if check {
            if win[4].is_nodata(nd, nd_nan) {
                return dst_nodata;
            }
            for i in 0..9 {
                if win[i].is_nodata(nd, nd_nan) {
                    if edges {
                        win[i] = win[4];
                    } else {
                        return dst_nodata;
                    }
                }
            }
        }
        (k.f)(&win)
    };

    // The first row.
    if o0 == 0 && edges && w >= 2 && h >= 2 {
        let (r0, r1) = (row(0), row(1));
        for j in 0..w {
            let (jmin, jmax) = (j.saturating_sub(1), (j + 1).min(w - 1));
            let win = [
                interpol(r0[jmin], r1[jmin]),
                interpol(r0[j], r1[j]),
                interpol(r0[jmax], r1[jmax]),
                r0[jmin],
                r0[j],
                r0[jmax],
                r1[jmin],
                r1[j],
                r1[jmax],
            ];
            out[j] = compute(win, has_nd);
        }
    }

    // The inner rows.
    let inner = |i: usize, o: &mut [f32]| {
        let (l1, l2, l3) = (row(i - 1), row(i), row(i + 1));
        let check = has_nd && (line_nd(i - 1) || line_nd(i) || line_nd(i + 1));
        if edges && w >= 2 {
            let win = [interpol(l1[0], l1[1]), l1[0], l1[1], interpol(l2[0], l2[1]), l2[0], l2[1], interpol(l3[0], l3[1]), l3[0], l3[1]];
            o[0] = compute(win, check);
        }
        let win_at = |j: usize| [l1[j - 1], l1[j], l1[j + 1], l2[j - 1], l2[j], l2[j + 1], l3[j - 1], l3[j], l3[j + 1]];
        let mut j = 1;
        if let Some(multi) = k.multi
            && !check
        {
            while j + 4 < w {
                for (jj, v) in o.iter_mut().enumerate().skip(j).take(4) {
                    *v = multi(&win_at(jj));
                }
                j += 4;
            }
        }
        while j + 1 < w {
            o[j] = compute(win_at(j), check);
            j += 1;
        }
        if edges && w >= 2 {
            let j = w - 1;
            let win = [
                l1[j - 1],
                l1[j],
                interpol(l1[j], l1[j - 1]),
                l2[j - 1],
                l2[j],
                interpol(l2[j], l2[j - 1]),
                l3[j - 1],
                l3[j],
                interpol(l3[j], l3[j - 1]),
            ];
            o[j] = compute(win, check);
        }
    };
    // The inner rows this computes.
    let (i0, i1) = (o0.max(1), o1.min(h.saturating_sub(1)));
    if i1 > i0 {
        let body = &mut out[(i0 - o0) * w..(i1 - o0) * w];
        #[cfg(not(target_family = "wasm"))]
        {
            let threads = crate::par::threads().min(16);
            let rows_per = (i1 - i0).div_ceil(threads).max(1);
            std::thread::scope(|s| {
                for (c, chunk) in body.chunks_mut(rows_per * w).enumerate() {
                    let inner = &inner;
                    s.spawn(move || {
                        for (k, o) in chunk.chunks_mut(w).enumerate() {
                            inner(i0 + c * rows_per + k, o);
                        }
                    });
                }
            });
        }
        #[cfg(target_family = "wasm")]
        for (k, o) in body.chunks_mut(w).enumerate() {
            inner(i0 + k, o);
        }
    }

    // The last row.
    if o1 == h && edges && w >= 2 && h >= 2 {
        let (l1, l2) = (row(h - 2), row(h - 1));
        let o = &mut out[(h - 1 - o0) * w..];
        for j in 0..w {
            let (jmin, jmax) = (j.saturating_sub(1), (j + 1).min(w - 1));
            let win = [
                l1[jmin],
                l1[j],
                l1[jmax],
                l2[jmin],
                l2[j],
                l2[jmax],
                interpol(l2[jmin], l1[jmin]),
                interpol(l2[j], l1[j]),
                interpol(l2[jmax], l1[jmax]),
            ];
            o[j] = compute(win, has_nd);
        }
    }
    out
}

/// A band read as GDAL's DEM tools read it (a `Float32` band as it is stored).
enum Band<'a> {
    Int(Vec<i32>, Option<i32>),
    Float(std::borrow::Cow<'a, [f32]>, Option<f32>),
}

fn read_band(ds: &RasterDataset, band: usize) -> Result<Band<'_>> {
    let r = &ds.raster;
    if band == 0 || band > r.bands {
        return Err(Error::InvalidArgument(format!("unable to fetch band #{band}")));
    }
    let n = r.rows * r.cols;
    let s = (band - 1) * n;
    let nodata = ds.nodata();
    macro_rules! int {
        ($v:expr, $lo:expr, $hi:expr) => {{
            let nd = nodata.and_then(|d| {
                let rounded = (d + 0.5).floor();
                ((d - rounded).abs() < 1e-2 && ($lo..=$hi).contains(&d)).then_some(rounded as i32)
            });
            Band::Int($v[s..s + n].iter().map(|x| *x as i32).collect(), nd)
        }};
    }
    macro_rules! float {
        ($v:expr) => {
            Band::Float(std::borrow::Cow::Owned($v[s..s + n].iter().map(|x| *x as f32).collect()), nodata.map(|d| d as f32))
        };
    }
    Ok(match &r.data {
        RasterData::U8(v) => int!(v, 0.0, 255.0),
        RasterData::I16(v) => int!(v, -32768.0, 32767.0),
        RasterData::U16(v) => int!(v, 0.0, 65535.0),
        RasterData::I8(v) => float!(v),
        RasterData::U32(v) => float!(v),
        RasterData::I32(v) => float!(v),
        RasterData::U64(v) => float!(v),
        RasterData::I64(v) => float!(v),
        RasterData::F32(v) => Band::Float(std::borrow::Cow::Borrowed(&v[s..s + n]), nodata.map(|d| d as f32)),
        RasterData::F64(v) => float!(v),
    })
}

/// The horizontal scales: given, or derived from the CRS as `gdaldem` does (degrees converted
/// to metres on the ellipsoid at the centre latitude; projected units converted to metres).
fn scales(ds: &RasterDataset, o: &DemOptions) -> (f64, f64) {
    if let Some(s) = o.scale {
        return (s, s);
    }
    if let (Some(x), Some(y)) = (o.xscale, o.yscale) {
        return (x, y);
    }
    let srs = ds.crs();
    let Some(wkt) = srs.to_wkt() else { return (1.0, 1.0) };
    let gt = ds.geo_transform();
    if srs.is_angular() {
        let ang = wkt_unit(&wkt, &["ANGLEUNIT[", "UNIT["]).unwrap_or(0.0174532925199433);
        let Some(a) = wkt_semi_major(&wkt) else { return (1.0, 1.0) };
        let yscale = ang * a;
        let mean_lat = (gt[3] + ds.raster.rows as f64 * gt[5] / 2.0) * ang;
        (yscale * mean_lat.cos(), yscale)
    } else if wkt.trim_start().starts_with("PROJCS") || wkt.trim_start().starts_with("PROJCRS") {
        let u = wkt_unit(&wkt, &["LENGTHUNIT[", "UNIT["]).unwrap_or(1.0);
        (u, u)
    } else {
        (1.0, 1.0)
    }
}

/// The conversion factor of the last unit node of a WKT with one of these keywords.
fn wkt_unit(wkt: &str, keys: &[&str]) -> Option<f64> {
    let upper = wkt.to_ascii_uppercase();
    let pos = keys.iter().filter_map(|k| upper.rfind(k).map(|p| p + k.len())).max()?;
    let rest = &wkt[pos..];
    let comma = rest.find(',')?;
    let tail = &rest[comma + 1..];
    let end = tail.find([',', ']'])?;
    tail[..end].trim().parse().ok()
}

/// The semi-major axis of a WKT's ellipsoid.
fn wkt_semi_major(wkt: &str) -> Option<f64> {
    let upper = wkt.to_ascii_uppercase();
    let pos = ["ELLIPSOID[", "SPHEROID["].iter().filter_map(|k| upper.find(k).map(|p| p + k.len())).min()?;
    let rest = &wkt[pos..];
    let quote_end = rest.strip_prefix('"').and_then(|r| r.find('"').map(|p| p + 2)).unwrap_or(0);
    let rest = &rest[quote_end..];
    let rest = rest.trim_start_matches([',', ' ']);
    let end = rest.find([',', ']'])?;
    rest[..end].trim().parse().ok()
}

/// A terrain product of a DEM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemProduct {
    /// Shaded relief (Byte, nodata 0).
    Hillshade,
    /// Slope (Float32, nodata -9999).
    Slope,
    /// Aspect (Float32, nodata -9999 unless `zero_for_flat`).
    Aspect,
    /// Terrain Ruggedness Index (Float32, nodata -9999).
    Tri,
    /// Topographic Position Index (Float32, nodata -9999).
    Tpi,
    /// Roughness: the largest difference within the window (Float32, nodata -9999).
    Roughness,
}

impl DemProduct {
    /// Parses a `gdaldem` mode name: `hillshade` (or `shade`), `slope`, `aspect`, `TRI`, `TPI`,
    /// `roughness`.
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "hillshade" | "shade" => Self::Hillshade,
            "slope" => Self::Slope,
            "aspect" => Self::Aspect,
            "tri" => Self::Tri,
            "tpi" => Self::Tpi,
            "roughness" => Self::Roughness,
            other => return Err(Error::InvalidArgument(format!("unknown DEM processing '{other}'"))),
        })
    }
}

/// A terrain product of a DEM band, set up from the DEM's metadata.
struct Terrain {
    product: DemProduct,
    o: DemOptions,
    gt: [f64; 6],
    xscale: f64,
    yscale: f64,
    width: usize,
    height: usize,
    out_type: DataType,
    dst_nodata: Option<f64>,
}

impl Terrain {
    /// `ds` is only read for its metadata.
    fn new(ds: &RasterDataset, product: DemProduct, o: &DemOptions) -> Result<Self> {
        o.validate()?;
        if o.kernel_version == super::warp::KernelVersion::Gdal313
            && (!matches!(product, DemProduct::Slope | DemProduct::Hillshade)
                || (product == DemProduct::Hillshade && o.shading != Shading::Standard))
        {
            return Err(Error::Unsupported("GDAL 3.13 terrain arithmetic currently supports slope and standard hillshade".into()));
        }
        if o.shading != Shading::Standard && product != DemProduct::Hillshade {
            return Err(Error::InvalidArgument("combined, multidirectional and Igor shading apply to hillshade only".into()));
        }
        let gt = ds.geo_transform();
        // Gradients take the pixel sizes from the geotransform's diagonal (as gdaldem does), which a
        // rotated grid doesn't have.
        if matches!(product, DemProduct::Hillshade | DemProduct::Slope | DemProduct::Aspect) && (gt[2] != 0.0 || gt[4] != 0.0) {
            return Err(Error::Unsupported("hillshade, slope and aspect of rotated grids are not supported".into()));
        }
        if o.band == 0 || o.band > ds.raster.bands {
            return Err(Error::InvalidArgument(format!("unable to fetch band #{}", o.band)));
        }
        let (xscale, yscale) = scales(ds, o);
        let (out_type, dst_nodata) = match product {
            DemProduct::Hillshade => (DataType::U8, Some(0.0)),
            DemProduct::Aspect if o.zero_for_flat => (DataType::F32, None),
            _ => (DataType::F32, Some(-9999.0)),
        };
        Ok(Self { product, o: o.clone(), gt, xscale, yscale, width: ds.raster.cols, height: ds.raster.rows, out_type, dst_nodata })
    }

    /// The output's metadata, without pixels.
    fn header(&self, ds: &RasterDataset) -> Result<RasterDataset> {
        let mut out = super::raster_header(Grid::of(&ds.raster), self.out_type, 1, self.dst_nodata, &ds.crs())?;
        // A rotated or south-up input keeps its geotransform.
        if let Some(gt) = ds.affine {
            out.set_geo_transform(gt);
        }
        Ok(out)
    }

    /// Computes rows `out_rows` from rows `src_rows` of the band, in the output's type.
    fn compute(&self, band: &Band, src_rows: std::ops::Range<usize>, out_rows: std::ops::Range<usize>) -> RasterData {
        let (product, o, gt) = (self.product, &self.o, self.gt);
        let (xscale, yscale, w, h) = (self.xscale, self.yscale, self.width, self.height);
        let dst_nd = self.dst_nodata.unwrap_or(0.0) as f32;
        macro_rules! run {
            ($t:ty, $src:expr, $nd:expr) => {{
                let hs = HillshadeParams::new(&gt, o, xscale, yscale);
                let md = MultiDirectionalParams::new(&gt, o, xscale, yscale);
                let modern = o.kernel_version == super::warp::KernelVersion::Gdal313;
                let f313 = Terrain313::new(&gt, o, xscale, yscale);
                let same_res = gt[1] == -gt[5] && xscale == yscale;
                let (ew, ns) = (gt[1] * xscale, gt[5] * yscale);
                let azimuth = !o.trigonometric;
                let f: Box<dyn Fn(&[$t; 9]) -> f32 + Sync> = match product {
                    DemProduct::Hillshade if modern => Box::new(move |w| f313.shade(w, false)),
                    DemProduct::Hillshade => match o.shading {
                        Shading::MultiDirectional => Box::new(move |w| md.shade(w)),
                        Shading::Combined => Box::new(move |w| hs.combined(w)),
                        Shading::Igor => Box::new(move |w| hs.igor(w)),
                        Shading::Standard if o.gradient == GradientAlg::Horn && same_res => Box::new(move |w| hs.same_res(w)),
                        Shading::Standard => Box::new(move |w| hs.standard(w)),
                    },
                    DemProduct::Slope => {
                        let (alg, percent) = (o.gradient, o.slope_percent);
                        Box::new(move |w| if modern { slope313(w, alg, ew, ns, percent) } else { slope(w, alg, ew, ns, percent) })
                    }
                    DemProduct::Aspect => {
                        let alg = o.gradient;
                        Box::new(move |w| aspect(w, alg, azimuth, dst_nd))
                    }
                    DemProduct::Tri if o.tri == TriAlg::Wilson => Box::new(tri_wilson::<$t>),
                    DemProduct::Tri => Box::new(tri_riley::<$t>),
                    DemProduct::Tpi => Box::new(tpi::<$t>),
                    DemProduct::Roughness => Box::new(roughness::<$t>),
                };
                #[cfg(target_arch = "x86_64")]
                let simd = move |w: &[$t; 9]| if modern { f313.shade(w, true) } else { hs.same_res_simd(w) };
                #[cfg(target_arch = "x86_64")]
                let multi: Option<&(dyn Fn(&[$t; 9]) -> f32 + Sync)> =
                    (product == DemProduct::Hillshade && o.shading == Shading::Standard && o.gradient == GradientAlg::Horn && same_res)
                        .then_some(&simd);
                #[cfg(not(target_arch = "x86_64"))]
                let multi: Option<&(dyn Fn(&[$t; 9]) -> f32 + Sync)> = None;
                let rows = Rows { src: $src, width: w, height: h, src_rows, out_rows };
                process::<$t>(rows, $nd, dst_nd, o.compute_edges, &Kernel { f: &*f, multi })
            }};
        }
        let values = match band {
            Band::Int(v, nd) => run!(i32, v, *nd),
            Band::Float(v, nd) => run!(f32, v, *nd),
        };
        // The values as they are stored: bytes, or the Float32 values themselves.
        if self.out_type == DataType::U8 {
            RasterData::U8(crate::par::map_range(values.len(), |i| float_to_byte(values[i]) as u8))
        } else {
            RasterData::F32(values)
        }
    }
}

/// Computes a terrain product of a DEM band, as `gdaldem` does.
pub fn dem(ds: &RasterDataset, product: DemProduct, o: &DemOptions) -> Result<RasterDataset> {
    let t = Terrain::new(ds, product, o)?;
    let h = ds.raster.rows;
    let data = {
        let band = read_band(ds, o.band)?;
        t.compute(&band, 0..h, 0..h)
    };
    let mut out = t.header(ds)?;
    out.raster.data = data;
    Ok(out)
}

/// Computes a terrain product of a DEM band and writes it to `path`, as `gdaldem` does, reading
/// the DEM and writing a GeoTIFF output a strip of rows at a time: neither is held whole (other
/// formats are written whole). The values are those of [`dem`].
///
/// A DEM that is also the output (`path` is the file `src` reads) is read whole first, so that
/// writing does not truncate it; [`DemOptions::max_memory_mib`] is then refused.
///
/// ```no_run
/// use terrio::ops::dem::{DemOptions, DemProduct, dem_to_path};
///
/// let mut src = terrio::open_raster_lazy("dem.tif")?;
/// dem_to_path(&mut src, DemProduct::Hillshade, &DemOptions::default(), "hillshade.tif", &Default::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn dem_to_path(
    src: &mut crate::LazyRaster,
    product: DemProduct,
    o: &DemOptions,
    path: &str,
    options: &crate::RasterWriteOptions,
) -> Result<()> {
    o.validate()?;
    if overwrites_input(src, o, path)? {
        return crate::write_raster_dataset(&dem(&src.read()?, product, o)?, path, options);
    }
    let out_bytes = if product == DemProduct::Hillshade { 1 } else { 4 };
    let rows = strip_rows(src, o, out_bytes);
    dem_in_strips(src, product, o, path, options, rows)
}

/// Whether `path` is the file `src` reads, which is then read whole first instead of in strips:
/// a memory budget, which could not be honored, is refused.
fn overwrites_input(src: &crate::LazyRaster, o: &DemOptions, path: &str) -> Result<bool> {
    let overwrites = super::strips::overwrites(path, &src.input_paths()?);
    if overwrites && o.max_memory_mib.is_some() {
        return Err(Error::InvalidArgument(
            "max_memory_mib does not apply to an output that is also the input, which is read whole".into(),
        ));
    }
    Ok(overwrites)
}

/// The rows of the strips of a DEM read a strip at a time, `row` bytes in flight per row: as many
/// as `max_memory_mib` holds, at least one, whole rows of the input's blocks when they fit; without
/// a budget, about 64 MiB of input values ([`super::strips::strip_rows`]).
fn budget_rows(src: &crate::LazyRaster, max_memory_mib: Option<f64>, row: usize) -> usize {
    let block = src.block_size().map(|b| b.1);
    let Some(mib) = max_memory_mib else {
        return super::strips::strip_rows(src.width(), 1, src.data_type().size_bytes().max(4), block);
    };
    let rows = ((mib * 1048576.0) as usize / row.max(1)).max(1);
    match block.filter(|b| *b > 1 && rows >= *b) {
        Some(b) => rows / b * b,
        None => rows,
    }
}

/// The rows of the strips a DEM operation written a strip at a time processes, for an output of
/// `out_bytes` bytes per pixel (all bands): as many rows as `max_memory_mib` holds (see
/// [`budget_rows`]). About three strips of input values (read ahead, read, worked on) and three of
/// output (computed, queued, being written) are in flight, with the `f64` values of one strip (8
/// bytes a pixel) and its 4-byte copy in [`read_band`].
fn strip_rows(src: &crate::LazyRaster, o: &DemOptions, out_bytes: usize) -> usize {
    let value = src.data_type().size_bytes().max(4);
    budget_rows(src, o.max_memory_mib, src.width().max(1) * (3 * value + 8 + 4 + 3 * out_bytes))
}

/// [`dem_to_path`] in strips of `rows` rows.
fn dem_in_strips(
    src: &mut crate::LazyRaster,
    product: DemProduct,
    o: &DemOptions,
    path: &str,
    options: &crate::RasterWriteOptions,
    rows: usize,
) -> Result<()> {
    let header = src.header();
    let t = Terrain::new(&header, product, o)?;
    let (w, h) = (t.width, t.height);
    let mut out = super::strips::StripWriter::create(&t.header(&header)?, path, options)?;
    let read = crate::WindowOptions { bands: vec![o.band], ..Default::default() };
    // Rows `y0..y1` of strip `i`, read with a row more on each side, for the 3×3 window.
    let span = |i: usize| {
        let (y0, y1) = (i * rows, ((i + 1) * rows).min(h));
        (y0, y1, y0.saturating_sub(1), (y1 + 1).min(h))
    };
    super::strips::pipelined(
        h.div_ceil(rows),
        |i| {
            let (_, _, s0, s1) = span(i);
            src.read_window(crate::Window::new(0, s0, w, s1 - s0), &read)
        },
        |i, strip| {
            let (y0, y1, s0, s1) = span(i);
            out.write(t.compute(&read_band(&strip, 1)?, s0..s1, y0..y1), y1 - y0)
        },
    )?;
    out.finish()
}

/// GDAL's conversion of a `Float32` value to a byte: NaN becomes 0, others are rounded half up
/// and clamped.
fn float_to_byte(v: f32) -> f64 {
    if v.is_nan() { 0.0 } else { ((v + 0.5).clamp(0.0, 255.0) as u8) as f64 }
}

/// Shaded relief of a DEM band (`gdaldem hillshade`).
pub fn hillshade(ds: &RasterDataset, o: &DemOptions) -> Result<RasterDataset> {
    dem(ds, DemProduct::Hillshade, o)
}

/// Slope of a DEM band (`gdaldem slope`).
pub fn slope_map(ds: &RasterDataset, o: &DemOptions) -> Result<RasterDataset> {
    dem(ds, DemProduct::Slope, o)
}

/// Aspect of a DEM band (`gdaldem aspect`).
pub fn aspect_map(ds: &RasterDataset, o: &DemOptions) -> Result<RasterDataset> {
    dem(ds, DemProduct::Aspect, o)
}

/// One entry of a color file: a value and its color.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorEntry {
    /// The value (NaN for a NaN nodata value).
    pub value: f64,
    /// Red.
    pub r: i32,
    /// Green.
    pub g: i32,
    /// Blue.
    pub b: i32,
    /// Alpha.
    pub a: i32,
}

/// A color file, parsed as `gdaldem color-relief` parses it: lines of `value R G B [A]` or
/// `value colorname [A]`, where the value may be `nv` (the nodata value) or a percentage of the
/// band's range, and separators are spaces, tabs, commas or colons. GMT `.cpt` files (with a
/// `COLOR_MODEL = RGB` comment) are read too.
#[derive(Debug, Clone, PartialEq)]
pub struct ColorMap {
    /// The entries, in file order.
    pub entries: Vec<ColorEntry>,
}

impl ColorMap {
    /// Parses a color file's text for a band, whose nodata value resolves `nv` and whose range
    /// resolves percentages: `range` when given (GDAL uses the statistics cached in the
    /// dataset's `.aux.xml`, see [`pam_statistics`]), otherwise the band's minimum and maximum.
    pub fn parse(text: &str, ds: &RasterDataset, band: usize, range: Option<(f64, f64)>) -> Result<Self> {
        Self::parse_with(text, ds.nodata(), range, || band_range(ds, band))
    }

    /// [`ColorMap::parse`] for a DEM read by window: percentages, when there are some and no
    /// `range` is given, are of the band's range read a strip of rows at a time (a pass over the
    /// band before it is colored), the strips taking about `max_memory_mib` MiB (as
    /// [`DemOptions::max_memory_mib`]).
    pub fn parse_lazy(
        text: &str,
        src: &mut crate::LazyRaster,
        band: usize,
        range: Option<(f64, f64)>,
        max_memory_mib: Option<f64>,
    ) -> Result<Self> {
        if max_memory_mib.is_some_and(|m| m.is_nan() || m <= 0.0) {
            return Err(Error::InvalidArgument("invalid max_memory_mib".into()));
        }
        let nodata = src.header().nodata();
        Self::parse_with(text, nodata, range, || lazy_band_range(src, band, max_memory_mib))
    }

    fn parse_with(
        text: &str,
        nodata: Option<f64>,
        range: Option<(f64, f64)>,
        band_range: impl FnOnce() -> Result<(f64, f64)>,
    ) -> Result<Self> {
        let mut band_range = Some(band_range);
        let mut range = range;
        let mut entries = Vec::new();
        let mut gmt = false;
        for line in text.split(['\n', '\r']) {
            if line.starts_with('#') && line.contains("COLOR_MODEL") {
                if !line.contains("COLOR_MODEL = RGB") {
                    return Err(Error::InvalidArgument("only COLOR_MODEL = RGB is supported".into()));
                }
                gmt = true;
            }
            if line.is_empty() || line.starts_with('#') || line.starts_with('/') {
                continue;
            }
            let t: Vec<&str> = line.split([' ', ',', '\t', ':']).filter(|s| !s.is_empty()).collect();
            if gmt && t.len() == 8 {
                for k in [0, 4] {
                    entries.push(ColorEntry { value: atof(t[k]), r: atoi(t[k + 1]), g: atoi(t[k + 2]), b: atoi(t[k + 3]), a: 255 });
                }
            } else if gmt && t.len() == 4 {
                if t[0].eq_ignore_ascii_case("N")
                    && let Some(nd) = nodata
                {
                    entries.push(ColorEntry { value: nd, r: atoi(t[1]), g: atoi(t[2]), b: atoi(t[3]), a: 255 });
                }
            } else if !gmt && t.len() >= 2 {
                let value = if t[0].eq_ignore_ascii_case("nv") {
                    match nodata {
                        Some(nd) => nd,
                        None => continue,
                    }
                } else if t[0].len() > 1 && t[0].ends_with('%') {
                    let pct = atof(t[0]) / 100.0;
                    if !(0.0..=1.0).contains(&pct) {
                        return Err(Error::InvalidArgument(format!("wrong value for a percentage: {}", t[0])));
                    }
                    let (lo, hi) = match range {
                        Some(r) => r,
                        None => *range.insert(band_range.take().expect("computed once")()?),
                    };
                    lo + pct * (hi - lo)
                } else {
                    atof(t[0])
                };
                let entry = if t.len() >= 4 {
                    ColorEntry { value, r: atoi(t[1]), g: atoi(t[2]), b: atoi(t[3]), a: if t.len() >= 5 { atoi(t[4]) } else { 255 } }
                } else {
                    let (r, g, b) = named_color(t[1]).ok_or_else(|| Error::InvalidArgument(format!("unknown color: {}", t[1])))?;
                    ColorEntry { value, r, g, b, a: if t.len() >= 3 { atoi(t[2]) } else { 255 } }
                };
                entries.push(entry);
            }
        }
        if entries.is_empty() {
            return Err(Error::InvalidArgument("no color association found in the color file".into()));
        }
        Ok(Self { entries })
    }
}

/// The minimum and maximum of a band cached in a GDAL `.aux.xml` file next to a dataset
/// (`STATISTICS_MINIMUM` and `STATISTICS_MAXIMUM`), as `gdaldem` uses them for percentages.
pub fn pam_statistics(path: &str, band: usize) -> Option<(f64, f64)> {
    let xml = std::fs::read_to_string(format!("{path}.aux.xml")).ok()?;
    let start = xml.find(&format!("<PAMRasterBand band=\"{band}\""))?;
    let block = &xml[start..];
    let block = &block[..block.find("</PAMRasterBand>").unwrap_or(block.len())];
    let value = |key: &str| -> Option<f64> {
        let tag = format!("<MDI key=\"{key}\">");
        let i = block.find(&tag)? + tag.len();
        let rest = &block[i..];
        rest[..rest.find('<')?].trim().parse().ok()
    };
    Some((value("STATISTICS_MINIMUM")?, value("STATISTICS_MAXIMUM")?))
}

/// The minimum and maximum of a band's valid values.
fn band_range(ds: &RasterDataset, band: usize) -> Result<(f64, f64)> {
    if band == 0 || band > ds.raster.bands {
        return Err(Error::InvalidArgument(format!("unable to fetch band #{band}")));
    }
    let nodata = ds.nodata();
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for v in super::band_f64(&ds.raster, band - 1) {
        if v.is_nan() || nodata.is_some_and(|nd| super::is_nodata(v, nd)) {
            continue;
        }
        lo = lo.min(v);
        hi = hi.max(v);
    }
    if lo > hi {
        return Err(Error::InvalidArgument("the band has no valid value to compute percentages from".into()));
    }
    Ok((lo, hi))
}

/// The minimum and maximum of the valid values of a band read by window, a strip of rows at a
/// time: as many rows as `max_memory_mib` holds of the values read and their `f64` copy.
fn lazy_band_range(src: &mut crate::LazyRaster, band: usize, max_memory_mib: Option<f64>) -> Result<(f64, f64)> {
    if band == 0 || band > src.band_count() {
        return Err(Error::InvalidArgument(format!("unable to fetch band #{band}")));
    }
    let header = src.header();
    let nodata = header.nodata();
    let (w, h) = (src.width(), src.height());
    let rows = budget_rows(src, max_memory_mib, w.max(1) * (src.data_type().size_bytes() + 8));
    let read = crate::WindowOptions { bands: vec![band], ..Default::default() };
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for y in (0..h).step_by(rows.max(1)) {
        let strip = src.read_window(crate::Window::new(0, y, w, rows.min(h - y)), &read)?;
        for v in super::band_f64(&strip.raster, 0) {
            if v.is_nan() || nodata.is_some_and(|nd| super::is_nodata(v, nd)) {
                continue;
            }
            lo = lo.min(v);
            hi = hi.max(v);
        }
    }
    if lo > hi {
        return Err(Error::InvalidArgument("the band has no valid value to compute percentages from".into()));
    }
    Ok((lo, hi))
}

/// GDAL's named colors.
fn named_color(name: &str) -> Option<(i32, i32, i32)> {
    const COLORS: [(&str, f32, f32, f32); 16] = [
        ("white", 1.00, 1.00, 1.00),
        ("black", 0.00, 0.00, 0.00),
        ("red", 1.00, 0.00, 0.00),
        ("green", 0.00, 1.00, 0.00),
        ("blue", 0.00, 0.00, 1.00),
        ("yellow", 1.00, 1.00, 0.00),
        ("magenta", 1.00, 0.00, 1.00),
        ("cyan", 0.00, 1.00, 1.00),
        ("aqua", 0.00, 0.75, 0.75),
        ("grey", 0.75, 0.75, 0.75),
        ("gray", 0.75, 0.75, 0.75),
        ("orange", 1.00, 0.50, 0.00),
        ("brown", 0.75, 0.50, 0.25),
        ("purple", 0.50, 0.00, 1.00),
        ("violet", 0.50, 0.00, 1.00),
        ("indigo", 0.00, 0.50, 1.00),
    ];
    COLORS
        .iter()
        .find(|c| c.0.eq_ignore_ascii_case(name))
        .map(|c| ((255.0f32 * c.1) as i32, (255.0f32 * c.2) as i32, (255.0f32 * c.3) as i32))
}

/// C's `atof()`: the longest numeric prefix, 0 when there is none.
pub(crate) fn atof(s: &str) -> f64 {
    let s = s.trim_start();
    let lower = s.to_ascii_lowercase();
    let unsigned = lower.trim_start_matches(['+', '-']);
    let neg = lower.starts_with('-');
    for (word, v) in [("infinity", f64::INFINITY), ("inf", f64::INFINITY), ("nan", f64::NAN)] {
        if unsigned.starts_with(word) && lower.len() - unsigned.len() <= 1 {
            return if neg { -v } else { v };
        }
    }
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    let digits_start = i;
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i == digits_start || &s[digits_start..i] == "." {
        return 0.0;
    }
    let mantissa_end = i;
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        let mut k = i + 1;
        if k < b.len() && (b[k] == b'+' || b[k] == b'-') {
            k += 1;
        }
        let exp_digits = k;
        while k < b.len() && b[k].is_ascii_digit() {
            k += 1;
        }
        if k > exp_digits {
            i = k;
        }
    }
    s[..i].parse().or_else(|_| s[..mantissa_end].parse()).unwrap_or(0.0)
}

/// C's `atoi()`: the leading integer, 0 when there is none.
pub(crate) fn atoi(s: &str) -> i32 {
    let s = s.trim_start();
    let b = s.as_bytes();
    let mut i = 0;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        i += 1;
    }
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    s[..i].parse::<i64>().map(|v| v as i32).unwrap_or(0)
}

/// Sorts the entries and adjusts duplicates and the neighbours of the nodata value, as
/// `GDALColorReliefProcessColors()` does.
fn process_colors(c: &mut Vec<ColorEntry>, nodata: Option<f64>, mode: ColorSelection) {
    let cmp = |a: &ColorEntry, b: &ColorEntry| {
        let less = |a: &ColorEntry, b: &ColorEntry| (a.value.is_nan() && !b.value.is_nan()) || a.value < b.value;
        if less(a, b) {
            std::cmp::Ordering::Less
        } else if less(b, a) {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    };
    c.sort_by(cmp);
    let initial = c.len();
    let mut repeated = 0usize;
    for i in 1..initial {
        let (prev, cur) = (c[i - 1], c[i]);
        if mode != ColorSelection::Exact && nodata.is_some_and(|nd| cur.value == nd) {
            let v = cur.value.next_down();
            if v > prev.value {
                c.push(ColorEntry { value: v, ..prev });
            }
        } else if mode != ColorSelection::Exact && nodata.is_some_and(|nd| prev.value == nd) {
            let v = prev.value.next_up();
            if v < cur.value {
                c.push(ColorEntry { value: v, ..cur });
            }
        } else if repeated == 0 && cur.value == prev.value {
            repeated = i;
        } else if repeated != 0 && cur.value != prev.value {
            let (total, left) = if repeated >= 2 {
                let lower = c[repeated - 2];
                (cur.value - lower.value, prev.value - lower.value)
            } else {
                (cur.value - prev.value, 0.0)
            };
            let count = i - repeated + 1;
            if total > prev.value.abs() * count as f64 * f64::EPSILON {
                let mut m = 0.5 - count as f64 * left / total;
                for e in &mut c[repeated - 1..i] {
                    e.value += (prev.value.abs() * m) * f64::EPSILON;
                    m += 1.0;
                }
            }
            repeated = 0;
        }
    }
    if c.len() != initial {
        c.sort_by(cmp);
    }
}

/// `GDALColorReliefGetRGBA()`: the color of a value.
fn rgba(c: &[ColorEntry], v: f64, mode: ColorSelection) -> [i32; 4] {
    let col = |e: &ColorEntry| [e.r, e.g, e.b, e.a];
    let mut lower = 0;
    if c[0].value.is_nan() {
        if v.is_nan() {
            return col(&c[0]);
        }
        lower = 1;
    }
    let mut upper = c.len() - 1;
    let i = loop {
        let mid = (lower + upper) / 2;
        if upper - lower <= 1 {
            break if v <= c[lower].value {
                lower
            } else if v <= c[upper].value {
                upper
            } else {
                upper + 1
            };
        } else if c[mid].value >= v {
            upper = mid;
        } else {
            lower = mid;
        }
    };
    if i == 0 {
        return if mode == ColorSelection::Exact && c[0].value != v { [0; 4] } else { col(&c[0]) };
    }
    if i == c.len() {
        return if mode == ColorSelection::Exact && c[i - 1].value != v { [0; 4] } else { col(&c[i - 1]) };
    }
    if c[i - 1].value == v {
        return col(&c[i - 1]);
    }
    if c[i].value == v {
        return col(&c[i]);
    }
    if mode == ColorSelection::Exact {
        return [0; 4];
    }
    if mode == ColorSelection::Nearest && c[i - 1].value != v {
        let k = if v - c[i - 1].value < c[i].value - v { i - 1 } else { i };
        return col(&c[k]);
    }
    if c[i - 1].value.is_nan() {
        return col(&c[i]);
    }
    let ratio = (v - c[i - 1].value) / (c[i].value - c[i - 1].value);
    let lerp = |a: i32, b: i32| ((0.5 + a as f64 + ratio * (b - a) as f64) as i32).clamp(0, 255);
    let (a, b) = (col(&c[i - 1]), col(&c[i]));
    [lerp(a[0], b[0]), lerp(a[1], b[1]), lerp(a[2], b[2]), lerp(a[3], b[3])]
}

/// The color relief of a DEM band as a virtual raster (`gdaldem color-relief -of VRT`): three
/// Byte bands (four with `alpha`), each a `ComplexSource` of the band with a lookup table from
/// the color map, as GDAL builds them. `src_path` is the DEM's path, written in the VRT (relative
/// to `vrt_path`'s directory when the DEM is below it); `ds` is the DEM read from it.
///
/// GDAL reads the pixels of such a VRT by interpolating its lookup tables, which differs from
/// [`color_relief`] by rounding only (and not at all with exact or nearest color entries).
pub fn color_relief_vrt(
    ds: &RasterDataset,
    src_path: &str,
    colors: &ColorMap,
    o: &DemOptions,
    vrt_path: Option<&str>,
) -> Result<super::vrt::VrtDataset> {
    use super::vrt::{VrtBand, VrtDataset, VrtSource};
    // The VRT is written over the DEM it reads from: that would destroy the DEM, so it is refused.
    if vrt_path.is_some_and(|p| super::strips::overwrites(p, &[src_path.to_string()])) {
        return Err(Error::InvalidArgument("a VRT output cannot replace the DEM it is built from".into()));
    }
    let r = &ds.raster;
    if o.band == 0 || o.band > r.bands {
        return Err(Error::InvalidArgument(format!("unable to fetch band #{}", o.band)));
    }
    let mut c = colors.entries.clone();
    if c.is_empty() {
        return Err(Error::InvalidArgument("the color map is empty".into()));
    }
    process_colors(&mut c, ds.nodata(), o.color_selection);
    let vrt_dir = vrt_path.and_then(|p| std::path::Path::new(p).parent());
    let (filename, relative_to_vrt) = super::mosaic::source_path(src_path, vrt_dir);
    let (w, h) = (r.cols as f64, r.rows as f64);
    let nbands = if o.alpha { 4 } else { 3 };
    let bands = (0..nbands)
        .map(|band| {
            let component = |e: &ColorEntry| f64::from([e.r, e.g, e.b, e.a][band]);
            // GDALGenerateVRTColorRelief(): each entry, with the midpoints between entries for
            // the nearest entry and zero just around them for exact entries.
            let mut lut = Vec::new();
            for (i, e) in c.iter().enumerate() {
                if i > 0 && o.color_selection == ColorSelection::Nearest && e.value != c[i - 1].value.next_up() {
                    let mid = (e.value + c[i - 1].value) / 2.0;
                    lut.push((mid.next_down(), component(&c[i - 1])));
                    lut.push((mid, component(e)));
                } else {
                    if o.color_selection == ColorSelection::Exact {
                        lut.push((e.value.next_down(), 0.0));
                    }
                    lut.push((e.value, component(e)));
                }
                if o.color_selection == ColorSelection::Exact {
                    lut.push((e.value.next_up(), 0.0));
                }
            }
            let source = VrtSource {
                filename: filename.clone(),
                relative_to_vrt,
                band: o.band,
                src_rect: [0.0, 0.0, w, h],
                dst_rect: [0.0, 0.0, w, h],
                complex: true,
                nodata: None,
                scale: None,
                lut: Some(lut),
                resampling: super::Resampling::Nearest,
                properties: Some((r.cols, r.rows, r.data_type)),
            };
            let interp = ["Red", "Green", "Blue", "Alpha"][band];
            VrtBand { data_type: DataType::U8, nodata: None, color_interp: Some(interp.into()), sources: vec![source] }
        })
        .collect();
    let crs = ds.crs();
    Ok(VrtDataset {
        width: r.cols,
        height: r.rows,
        geo_transform: Some(ds.geo_transform()),
        srs: (!crs.is_unknown()).then_some(crs),
        bands,
    })
}

/// A color map set up to color a DEM band as `gdaldem color-relief` does.
struct Relief {
    entries: Vec<ColorEntry>,
    mode: ColorSelection,
    nbands: usize,
    data_type: DataType,
    /// The colors of every value of Byte, Int16 and UInt16 bands, which are read as integers.
    cache: Option<Vec<[i32; 4]>>,
}

impl Relief {
    /// `ds` is only read for its metadata.
    fn new(ds: &RasterDataset, colors: &ColorMap, o: &DemOptions) -> Result<Self> {
        let r = &ds.raster;
        if o.band == 0 || o.band > r.bands {
            return Err(Error::InvalidArgument(format!("unable to fetch band #{}", o.band)));
        }
        let mut c = colors.entries.clone();
        if c.is_empty() {
            return Err(Error::InvalidArgument("the color map is empty".into()));
        }
        process_colors(&mut c, ds.nodata(), o.color_selection);
        // Byte, Int16 and UInt16 are read as integers, the other types as Float32.
        let exact = matches!(r.data_type, DataType::U8 | DataType::I16 | DataType::U16);
        let cache: Option<Vec<[i32; 4]>> = exact.then(|| {
            let offset = if r.data_type == DataType::I16 { 32768 } else { 0 };
            let len = if r.data_type == DataType::U8 { 256 } else { 65536 };
            (0..len).map(|i| rgba(&c, (i - offset) as f64, o.color_selection)).collect()
        });
        Ok(Self { entries: c, mode: o.color_selection, nbands: if o.alpha { 4 } else { 3 }, data_type: r.data_type, cache })
    }

    /// The colors of the values of a band, as Byte bands, band after band.
    fn colors(&self, band: &super::BandView) -> RasterData {
        let n = band.len();
        let offset = if self.data_type == DataType::I16 { 32768 } else { 0 };
        let color = |v: f64| match &self.cache {
            Some(table) => table[(v as i64 + offset) as usize],
            None => rgba(&self.entries, v as f32 as f64, self.mode),
        };
        let mut out = vec![0u8; n * self.nbands];
        if n == 0 {
            return RasterData::U8(out);
        }
        let (first, rest) = out.split_at_mut(n);
        let mut planes: Vec<&mut [u8]> = vec![first];
        let mut rest = rest;
        for _ in 1..self.nbands {
            let (p, r) = rest.split_at_mut(n);
            planes.push(p);
            rest = r;
        }
        // In chunks of pixels, in parallel: every band of a chunk at once.
        let len = crate::par::chunk_len(n, 1 << 16);
        let mut chunks: Vec<Vec<&mut [u8]>> = Vec::new();
        let mut planes: Vec<std::slice::ChunksMut<'_, u8>> = planes.into_iter().map(|p| p.chunks_mut(len)).collect();
        while let Some(first) = planes[0].next() {
            let mut c = vec![first];
            for p in planes.iter_mut().skip(1) {
                c.extend(p.next());
            }
            chunks.push(c);
        }
        crate::par::for_each_owned(chunks, |k, mut bands| {
            for i in 0..bands[0].len() {
                let px = color(band.get(k * len + i));
                for (b, out) in bands.iter_mut().enumerate() {
                    out[i] = px[b] as u8;
                }
            }
        });
        RasterData::U8(out)
    }
}

/// Colors a DEM band with a color map (`gdaldem color-relief`): three Byte bands (four with
/// `alpha`), without nodata.
pub fn color_relief(ds: &RasterDataset, colors: &ColorMap, o: &DemOptions) -> Result<RasterDataset> {
    let relief = Relief::new(ds, colors, o)?;
    let r = &ds.raster;
    super::north_up(&Grid::of(r))?;
    let data = relief.colors(&super::BandView::of(r, o.band - 1));
    super::build_raster_data(Grid::of(r), data, relief.nbands, None, &ds.crs())
}

/// Colors a DEM band with a color map as [`color_relief`] does and writes the result to `path`,
/// reading the DEM and writing a GeoTIFF output a strip of rows at a time: neither is held whole
/// (other formats are written whole). The values are those of [`color_relief`]. A DEM that is
/// also the output is read whole first, and [`DemOptions::max_memory_mib`] is then refused.
///
/// ```no_run
/// use terrio::ops::dem::{ColorMap, DemOptions, color_relief_to_path};
///
/// let mut src = terrio::open_raster_lazy("dem.tif")?;
/// let colors = ColorMap::parse_lazy("0 0 0 255\n1000 255 255 255\nnv 0 0 0 0", &mut src, 1, None, None)?;
/// let o = DemOptions { alpha: true, ..Default::default() };
/// color_relief_to_path(&mut src, &colors, &o, "relief.tif", &Default::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn color_relief_to_path(
    src: &mut crate::LazyRaster,
    colors: &ColorMap,
    o: &DemOptions,
    path: &str,
    options: &crate::RasterWriteOptions,
) -> Result<()> {
    o.validate()?;
    if overwrites_input(src, o, path)? {
        return crate::write_raster_dataset(&color_relief(&src.read()?, colors, o)?, path, options);
    }
    let rows = strip_rows(src, o, if o.alpha { 4 } else { 3 });
    color_relief_in_strips(src, colors, o, path, options, rows)
}

/// [`color_relief_to_path`] in strips of `rows` rows.
fn color_relief_in_strips(
    src: &mut crate::LazyRaster,
    colors: &ColorMap,
    o: &DemOptions,
    path: &str,
    options: &crate::RasterWriteOptions,
    rows: usize,
) -> Result<()> {
    let header = src.header();
    let relief = Relief::new(&header, colors, o)?;
    let grid = Grid::of(&header.raster);
    super::north_up(&grid)?;
    let out_header = super::raster_header(grid, DataType::U8, relief.nbands, None, &header.crs())?;
    let mut out = super::strips::StripWriter::create(&out_header, path, options)?;
    let (w, h) = (header.raster.cols, header.raster.rows);
    let read = crate::WindowOptions { bands: vec![o.band], ..Default::default() };
    super::strips::pipelined(
        h.div_ceil(rows),
        |i| {
            let (y0, y1) = (i * rows, ((i + 1) * rows).min(h));
            src.read_window(crate::Window::new(0, y0, w, y1 - y0), &read)
        },
        |_, strip| out.write(relief.colors(&super::BandView::of(&strip.raster, 0)), strip.raster.rows),
    )?;
    out.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::Grid;
    use crate::ops::build_raster;

    fn dem_ds(values: Vec<f64>, w: usize, h: usize, dt: DataType, nodata: Option<f64>, res: f64) -> RasterDataset {
        let grid = Grid { width: w, height: h, geo_transform: [0.0, res, 0.0, h as f64 * res, 0.0, -res] };
        build_raster(grid, vec![values], dt, nodata, &crate::crs::SpatialRef::default(), super::super::convert_value).unwrap()
    }

    fn plane(w: usize, h: usize) -> Vec<f64> {
        (0..h).flat_map(|r| (0..w).map(move |c| (c as f64) * 10.0 + (r as f64) * 5.0)).collect()
    }

    /// Every product computed a few rows at a time, written as a GeoTIFF and in another format,
    /// is the product of the whole band.
    #[test]
    fn strips_give_the_whole_band_values() {
        let dir = tempfile::tempdir().unwrap();
        let values: Vec<f64> = (0..13 * 11).map(|i| ((i * 37 % 101) as f64) * 3.5 - 40.0).collect();
        let mut v = values.clone();
        v[20] = -9999.0;
        v[13 * 10 + 4] = -9999.0;
        for (dt, nodata) in [(DataType::F32, Some(-9999.0)), (DataType::I16, None)] {
            let ds = dem_ds(v.clone(), 13, 11, dt, nodata, 10.0);
            let input = dir.path().join("dem.tif");
            crate::write_raster_dataset(&ds, input.to_str().unwrap(), &Default::default()).unwrap();
            let products =
                [DemProduct::Hillshade, DemProduct::Slope, DemProduct::Aspect, DemProduct::Tri, DemProduct::Tpi, DemProduct::Roughness];
            for (product, edges) in products.iter().flat_map(|p| [(*p, false), (*p, true)]) {
                let o = DemOptions { compute_edges: edges, ..Default::default() };
                let whole = dem(&ds, product, &o).unwrap();
                for (rows, ext) in [(1, "tif"), (2, "tif"), (4, "tif"), (11, "tif"), (3, "asc")] {
                    let out = dir.path().join(format!("out.{ext}"));
                    let mut src = crate::open_raster_lazy(input.to_str().unwrap()).unwrap();
                    dem_in_strips(&mut src, product, &o, out.to_str().unwrap(), &Default::default(), rows).unwrap();
                    let back = crate::open_raster(out.to_str().unwrap()).unwrap();
                    let (a, b) = (super::super::band_f64(&whole.raster, 0), super::super::band_f64(&back.raster, 0));
                    assert_eq!(a, b, "{dt:?} {product:?} edges {edges} rows {rows} {ext}");
                    assert_eq!(back.nodata(), whole.nodata());
                }
            }
        }
    }

    /// A color relief computed a few rows at a time, written as a GeoTIFF and in another format,
    /// is the color relief of the whole band: band types read as integers (through the lookup
    /// table) and as floats, nodata (`nv`) with alpha, percentages of the band's range read by
    /// window, and every color selection.
    #[test]
    fn color_relief_strips_give_the_whole_band_colors() {
        let dir = tempfile::tempdir().unwrap();
        let mut v: Vec<f64> = (0..17 * 13).map(|i| ((i * 37 % 101) as f64) * 3.5 - 40.0).collect();
        v[20] = -9999.0;
        v[17 * 12 + 4] = -9999.0;
        let text = "-40 0 0 255
50% 0 255 0 200
100 255 0 0
100% white
nv 10 20 30 0
";
        for (dt, nodata) in [(DataType::F32, Some(-9999.0)), (DataType::I16, Some(-9999.0)), (DataType::F64, None), (DataType::U8, None)] {
            let values: Vec<f64> = if dt == DataType::U8 { v.iter().map(|x| x.clamp(0.0, 255.0)).collect() } else { v.clone() };
            let ds = dem_ds(values, 17, 13, dt, nodata, 10.0);
            let input = dir.path().join("dem.tif");
            crate::write_raster_dataset(&ds, input.to_str().unwrap(), &Default::default()).unwrap();
            let map = ColorMap::parse(text, &ds, 1, None).unwrap();
            let mut lazy = crate::open_raster_lazy(input.to_str().unwrap()).unwrap();
            assert_eq!(ColorMap::parse_lazy(text, &mut lazy, 1, None, None).unwrap(), map, "{dt:?}");
            // A row at a time.
            assert_eq!(ColorMap::parse_lazy(text, &mut lazy, 1, None, Some(1e-9)).unwrap(), map, "{dt:?}");
            assert!(ColorMap::parse_lazy(text, &mut lazy, 1, None, Some(0.0)).is_err());
            for (alpha, mode) in [(true, ColorSelection::Interpolate), (false, ColorSelection::Nearest), (true, ColorSelection::Exact)] {
                let o = DemOptions { alpha, color_selection: mode, ..Default::default() };
                let whole = color_relief(&ds, &map, &o).unwrap();
                for (rows, ext) in [(1, "tif"), (2, "tif"), (5, "tif"), (13, "tif"), (3, "png")] {
                    let out = dir.path().join(format!("relief.{ext}"));
                    let mut src = crate::open_raster_lazy(input.to_str().unwrap()).unwrap();
                    color_relief_in_strips(&mut src, &map, &o, out.to_str().unwrap(), &Default::default(), rows).unwrap();
                    let back = crate::open_raster(out.to_str().unwrap()).unwrap();
                    assert_eq!(back.raster.bands, whole.raster.bands);
                    assert_eq!(
                        back.raster.data.to_f64_vec(),
                        whole.raster.data.to_f64_vec(),
                        "{dt:?} alpha {alpha} {mode:?} rows {rows} {ext}"
                    );
                    assert_eq!(back.geo_transform(), whole.geo_transform());
                    assert_eq!(back.nodata(), whole.nodata());
                }
            }
        }
    }

    /// A memory budget sets the strip height only: tiny budgets (a row per strip), and the
    /// output written over the DEM it reads (read whole first) give the same values.
    #[test]
    fn memory_budgets_and_overwrites_keep_the_values() {
        let dir = tempfile::tempdir().unwrap();
        let v: Vec<f64> = (0..40 * 30).map(|i| ((i * 53 % 211) as f64) * 1.5).collect();
        let ds = dem_ds(v, 40, 30, DataType::F32, None, 5.0);
        let input = dir.path().join("dem.tif");
        let input = input.to_str().unwrap();
        crate::write_raster_dataset(&ds, input, &Default::default()).unwrap();
        let mut src = crate::open_raster_lazy(input).unwrap();
        assert_eq!(strip_rows(&src, &DemOptions { max_memory_mib: Some(1e-9), ..Default::default() }, 4), 1);
        assert!(strip_rows(&src, &DemOptions { max_memory_mib: Some(1.0), ..Default::default() }, 4) > 30);
        // Without a budget, the strips dem_to_path has always used.
        assert_eq!(strip_rows(&src, &DemOptions::default(), 1), crate::ops::strips::strip_rows(40, 1, 4, src.block_size().map(|b| b.1)));
        assert!(
            dem_to_path(
                &mut src,
                DemProduct::Slope,
                &DemOptions { max_memory_mib: Some(0.0), ..Default::default() },
                "x.tif",
                &Default::default()
            )
            .is_err()
        );
        for budget in [None, Some(1e-9), Some(0.002)] {
            let o = DemOptions {
                compute_edges: true,
                shading: Shading::MultiDirectional,
                max_memory_mib: budget,
                alpha: true,
                ..Default::default()
            };
            let out = dir.path().join("out.tif");
            let out = out.to_str().unwrap();
            dem_to_path(&mut src, DemProduct::Hillshade, &o, out, &Default::default()).unwrap();
            assert_eq!(
                crate::open_raster(out).unwrap().raster.data.to_f64_vec(),
                dem(&ds, DemProduct::Hillshade, &o).unwrap().raster.data.to_f64_vec(),
                "{budget:?}"
            );
            let map = ColorMap::parse(
                "0 0 0 0
300 255 255 255",
                &ds,
                1,
                None,
            )
            .unwrap();
            color_relief_to_path(&mut src, &map, &o, out, &Default::default()).unwrap();
            assert_eq!(
                crate::open_raster(out).unwrap().raster.data.to_f64_vec(),
                color_relief(&ds, &map, &o).unwrap().raster.data.to_f64_vec(),
                "{budget:?}"
            );
        }
        // Over its own input.
        let copy = dir.path().join("copy.tif");
        let copy = copy.to_str().unwrap();
        std::fs::copy(input, copy).unwrap();
        let mut src = crate::open_raster_lazy(copy).unwrap();
        // A budget cannot apply to a DEM read whole: refused, before the output is written.
        let budget = DemOptions { max_memory_mib: Some(64.0), ..Default::default() };
        assert!(dem_to_path(&mut src, DemProduct::Slope, &budget, copy, &Default::default()).is_err());
        let map = ColorMap::parse_lazy(
            "0 0 0 0
300 255 255 255",
            &mut src,
            1,
            None,
            None,
        )
        .unwrap();
        assert!(color_relief_to_path(&mut src, &map, &budget, copy, &Default::default()).is_err());
        dem_to_path(&mut src, DemProduct::Slope, &DemOptions::default(), copy, &Default::default()).unwrap();
        drop(src);
        assert_eq!(
            crate::open_raster(copy).unwrap().raster.data.to_f64_vec(),
            dem(&ds, DemProduct::Slope, &DemOptions::default()).unwrap().raster.data.to_f64_vec()
        );
    }

    #[test]
    fn a_vrt_cannot_replace_its_dem() {
        let dir = tempfile::tempdir().unwrap();
        let v: Vec<f64> = (0..40 * 30).map(|i| ((i * 53 % 211) as f64) * 1.5).collect();
        let ds = dem_ds(v, 40, 30, DataType::F32, None, 5.0);
        let input = dir.path().join("dem.tif");
        let input = input.to_str().unwrap();
        crate::write_raster_dataset(&ds, input, &Default::default()).unwrap();
        let map = ColorMap::parse("0 0 0 0\n300 255 255 255", &ds, 1, None).unwrap();
        let o = DemOptions::default();
        assert!(color_relief_vrt(&ds, input, &map, &o, Some(input)).is_err());
        #[cfg(unix)]
        {
            let link = dir.path().join("link.tif");
            std::fs::hard_link(input, &link).unwrap();
            assert!(color_relief_vrt(&ds, input, &map, &o, link.to_str()).is_err());
        }
        // The DEM is unchanged by the refusals.
        assert_eq!(crate::open_raster(input).unwrap().raster.data.to_f64_vec(), ds.raster.data.to_f64_vec());
        // A VRT beside the DEM is written.
        let vrt = dir.path().join("relief.vrt");
        let vrt = vrt.to_str().unwrap();
        color_relief_vrt(&ds, input, &map, &o, Some(vrt)).unwrap().write(vrt).unwrap();
        assert!(std::fs::read_to_string(vrt).unwrap().starts_with("<VRTDataset"));
    }

    #[test]
    fn gradients_of_rotated_grids_are_refused() {
        let mut ds = dem_ds(plane(6, 5), 6, 5, DataType::F32, None, 10.0);
        ds.set_geo_transform([0.0, 0.0, 10.0, 0.0, 10.0, 0.0]);
        for product in [DemProduct::Hillshade, DemProduct::Slope, DemProduct::Aspect] {
            assert!(matches!(dem(&ds, product, &DemOptions::default()), Err(Error::Unsupported(_))), "{product:?}");
        }
        // Window statistics don't use the pixel size, and keep the rotated geotransform.
        let tri = dem(&ds, DemProduct::Tri, &DemOptions::default()).unwrap();
        assert_eq!(tri.geo_transform(), [0.0, 0.0, 10.0, 0.0, 10.0, 0.0]);
    }

    /// Captured from GDAL 3.13.3 on this Float32 3x3 source, without SIMD columns.
    #[test]
    fn modern_terrain_matches_gdal_float32_golden_values() {
        let ds = dem_ds(vec![942.315, 946.702, 951.43, 945.132, 943.95, 959.37, 948.15, 954.318, 960.01], 3, 3, DataType::F32, None, 3.5);
        let options = DemOptions { kernel_version: super::super::warp::KernelVersion::Gdal313, ..Default::default() };
        let slope = slope_map(&ds, &options).unwrap();
        // One ULP allows platform libm rounding in atan; benchmark gates stay exact.
        let bits = (slope.raster.get(0, 1, 1) as f32).to_bits();
        assert!(bits.abs_diff(1115697630) <= 1, "slope differs from GDAL 3.13.3 by more than one ULP");
        let hillshade = hillshade(&ds, &options).unwrap();
        assert_eq!(hillshade.raster.get(0, 1, 1), 236.0);
    }

    #[test]
    fn slope_of_a_plane() {
        let ds = dem_ds(plane(6, 5), 6, 5, DataType::F32, None, 10.0);
        let out = slope_map(&ds, &DemOptions::default()).unwrap();
        let v = out.raster.get(0, 2, 2);
        let expected = ((1.0f64 + 0.25).sqrt()).atan().to_degrees();
        assert!((v - expected).abs() < 1e-4, "{v} != {expected}");
        assert_eq!(out.raster.get(0, 0, 0), -9999.0);
        assert_eq!(out.nodata(), Some(-9999.0));
        let edges = slope_map(&ds, &DemOptions { compute_edges: true, ..Default::default() }).unwrap();
        // Corners repeat their column rather than extrapolating it, as in GDAL.
        assert!((edges.raster.get(0, 0, 1) - expected).abs() < 1e-4);
        assert!((edges.raster.get(0, 2, 0) - expected).abs() < 1e-4);
    }

    #[test]
    fn aspect_of_a_plane() {
        // Rising to the east: the slope faces west (270°).
        let ds = dem_ds(plane(5, 5).iter().enumerate().map(|(i, _)| (i % 5) as f64).collect(), 5, 5, DataType::I16, None, 1.0);
        let out = aspect_map(&ds, &DemOptions::default()).unwrap();
        assert_eq!(out.raster.get(0, 2, 2), 270.0);
        let trig = aspect_map(&ds, &DemOptions { trigonometric: true, ..Default::default() }).unwrap();
        assert_eq!(trig.raster.get(0, 2, 2), 180.0);
        let flat = dem_ds(vec![1.0; 25], 5, 5, DataType::I16, None, 1.0);
        assert_eq!(aspect_map(&flat, &DemOptions::default()).unwrap().raster.get(0, 2, 2), -9999.0);
        let zero = aspect_map(&flat, &DemOptions { zero_for_flat: true, ..Default::default() }).unwrap();
        assert_eq!(zero.raster.get(0, 2, 2), 0.0);
        assert_eq!(zero.nodata(), None);
    }

    #[test]
    fn nodata_neighbours() {
        let mut v = plane(5, 5);
        v[6] = -1.0;
        let ds = dem_ds(v, 5, 5, DataType::F32, Some(-1.0), 1.0);
        let out = dem(&ds, DemProduct::Roughness, &DemOptions::default()).unwrap();
        assert_eq!(out.raster.get(0, 2, 2), -9999.0);
        assert_ne!(out.raster.get(0, 2, 3), -9999.0);
        let edges = dem(&ds, DemProduct::Roughness, &DemOptions { compute_edges: true, ..Default::default() }).unwrap();
        assert_ne!(edges.raster.get(0, 2, 2), -9999.0);
        assert_eq!(edges.raster.get(0, 1, 1), -9999.0);
    }

    #[test]
    fn tri_tpi() {
        let mut v = vec![0.0; 9];
        v[4] = 8.0;
        let ds = dem_ds(v, 3, 3, DataType::F32, None, 1.0);
        let o = DemOptions::default();
        assert_eq!(dem(&ds, DemProduct::Tpi, &o).unwrap().raster.get(0, 1, 1), 8.0);
        assert_eq!(dem(&ds, DemProduct::Tri, &DemOptions { tri: TriAlg::Wilson, ..o.clone() }).unwrap().raster.get(0, 1, 1), 8.0);
        let riley = dem(&ds, DemProduct::Tri, &o).unwrap().raster.get(0, 1, 1);
        assert!((riley - (8.0f64 * 64.0).sqrt()).abs() < 1e-4);
    }

    #[test]
    fn hillshade_flat_is_sin_alt() {
        let ds = dem_ds(vec![100.0; 36], 6, 6, DataType::F32, None, 30.0);
        let out = hillshade(&ds, &DemOptions::default()).unwrap();
        // 1 + 254 * sin(45°) = 180.6
        assert_eq!(out.raster.get(0, 2, 2), 181.0);
        assert_eq!(out.raster.get(0, 0, 0), 0.0);
        let md = hillshade(&ds, &DemOptions { shading: Shading::MultiDirectional, ..Default::default() }).unwrap();
        assert_eq!(md.raster.get(0, 2, 2), 181.0);
    }

    #[test]
    fn color_map_parsing() {
        let ds = dem_ds(vec![0.0, 50.0, 100.0, -1.0], 2, 2, DataType::F32, Some(-1.0), 1.0);
        let text = "# comment\n0% blue\n100 255,0,0\n50%:white:128\nnv 0 0 0 0\n";
        let m = ColorMap::parse(text, &ds, 1, None).unwrap();
        assert_eq!(m.entries.len(), 4);
        assert_eq!(m.entries[2], ColorEntry { value: 50.0, r: 255, g: 255, b: 255, a: 128 });
        let out = color_relief(&ds, &m, &DemOptions { alpha: true, ..Default::default() }).unwrap();
        assert_eq!(out.raster.bands, 4);
        assert_eq!(out.raster.get(0, 1, 1), 0.0);
        assert_eq!(out.raster.get(3, 1, 1), 0.0);
        assert_eq!(out.raster.get(2, 0, 0), 255.0);
        assert_eq!(out.raster.get(0, 1, 0), 255.0);
        assert_eq!(atof("12.5%"), 12.5);
        assert_eq!(atoi("12abc"), 12);
    }

    #[test]
    fn wkt_parameters() {
        let wkt = r#"GEOGCRS["NAD83",DATUM["North American Datum 1983",ELLIPSOID["GRS 1980",6378137,298.257222101,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]],CS[ellipsoidal,2],ANGLEUNIT["degree",0.0174532925199433]]"#;
        assert_eq!(wkt_semi_major(wkt), Some(6378137.0));
        assert_eq!(wkt_unit(wkt, &["ANGLEUNIT[", "UNIT["]), Some(0.0174532925199433));
        let wkt1 = r#"GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],UNIT["degree",0.0174532925199433]]"#;
        assert_eq!(wkt_semi_major(wkt1), Some(6378137.0));
    }
}
