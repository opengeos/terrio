//! GDAL-style processing operations: [`translate`] (`gdal_translate`), [`warp`] (`gdalwarp`),
//! [`mosaic`] (`gdalbuildvrt` and `gdal_merge.py`), [`vrt`] (GDAL's virtual raster format),
//! [`rasterize`] (`gdal_rasterize`), [`polygonize`] (`gdal_polygonize`), [`dem`] (`gdaldem`),
//! [`calc`] (`gdal_calc.py`), [`proximity`] (`gdal_proximity.py`), [`sieve`] (`gdal_sieve.py`),
//! [`geometry`] (centroids, singleparts and vertices), [`footprint`], [`sample`], and the vector
//! filtering of `ogr2ogr` ([`vector_filter`], [`sql`]).
//! [`topology`] provides planar buffers, polygon overlay/dissolve, validity, repair and cleanup.
//! [`vector_geom`] simplifies, segmentizes, builds hulls, swaps axes, changes geometry types,
//! combines and spatially sorts vector features (GDAL's `gdal vector` commands of those names),
//! with the algorithms of [`simplify`] and [`hull`].
//! [`neighbors`] computes focal statistics (`gdal raster neighbors`), [`nearblack`] cleans image
//! collars (`nearblack`, `gdal raster clean-collar`) and [`enhance`] applies lookup-table
//! contrast enhancement (`gdalenhance`).
//! [`contour`] generates elevation lines and [`zonal`] summarizes raster bands within polygons.
//!
//! ```no_run
//! use terrio::ops::{calc, dem};
//!
//! let dem = terrio::open_raster("dem.tif")?;
//! let slope = dem::slope_map(&dem, &dem::DemOptions { scale: Some(111_120.0), ..Default::default() })?;
//! // gdal_calc.py's `--calc="A * (A > 1500)"`.
//! let inputs = [calc::CalcInput { name: "A".into(), ds: &dem, band: 1 }];
//! let high = calc::calc(&inputs, &calc::CalcOptions { calc: vec!["A * (A > 1500)".into()], ..Default::default() })?;
//! terrio::write_raster_dataset(&high, "high.tif", &Default::default())?;
//! # Ok::<(), terrio::Error>(())
//! ```
//!
//! Pixel coordinates follow GDAL's convention: pixel `(col, row)` covers `[col, col + 1)` ×
//! `[row, row + 1)` and its centre is at `(col + 0.5, row + 0.5)`. Geotransforms are GDAL's
//! `[x_origin, pixel_width, 0, y_origin, 0, -pixel_height]`.

pub mod as_features;
pub mod blend;
pub mod calc;
pub mod compare;
pub mod concat;
pub mod contour;
pub mod create;
pub mod dem;
pub mod edit;
pub mod enhance;
pub mod fill_nodata;
pub mod footprint;
pub mod gcp;
pub mod geolocation;
pub mod geometry;
pub mod hull;
pub mod lineref;
pub mod make_point;
pub mod mosaic;
pub mod nearblack;
pub mod neighbors;
pub mod overview;
pub(crate) mod overview_kernel;
pub mod palette;
pub mod pansharpen;
pub mod partition;

pub mod pixel;
pub mod pixel_info;
pub mod polygonize;
pub mod proximity;
pub mod rasterize;
pub mod rpc;
pub mod sample;
pub mod sieve;
pub mod simplify;
pub mod sql;
pub(crate) mod strips;
pub mod tile_index;
pub mod topology;
pub mod translate;
pub mod update;
pub mod vector_edit;
pub mod vector_filter;
pub mod vector_geom;
pub mod vector_grid;
pub mod vector_schema;
pub mod viewshed;
pub mod vrt;
pub mod warp;
pub mod xyz;
pub mod zonal;

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

/// GDAL's ARE_REAL_EQUAL comparison at single precision.
#[inline]
pub(crate) fn gdal_real_equal32(a: f32, b: f32) -> bool {
    a == b || (a - b).abs() < f32::EPSILON * (a + b).abs() * 2.0
}

/// GDAL's ARE_REAL_EQUAL comparison at double precision (with float epsilon).
#[inline]
pub(crate) fn gdal_real_equal64(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() < f32::EPSILON as f64 * (a + b).abs() * 2.0
}

/// Whether a sample is valid under GDAL's typed nodata-mask comparison.
#[inline]
pub(crate) fn gdal_nodata_valid(value: f64, nodata: f64, dt: DataType) -> bool {
    match dt {
        DataType::F32 => !(nodata.is_nan() && value.is_nan() || gdal_real_equal32(value as f32, nodata as f32)),
        DataType::F64 => !(nodata.is_nan() && value.is_nan() || gdal_real_equal64(value, nodata)),
        _ => {
            let (lo, hi) = type_range(dt);
            nodata.is_nan() || nodata < lo || nodata > hi || nodata.fract() != 0.0 || value != nodata
        }
    }
}

/// Whether `v` is the nodata value `nodata` (NaN matches NaN).
#[inline]
pub(crate) fn is_nodata(v: f64, nodata: f64) -> bool {
    if nodata.is_nan() { v.is_nan() } else { v == nodata }
}

/// The north-up geotransform of a `w` x `h` grid over `b` (`[xmin, ymin, xmax, ymax]`), refusing
/// a box that is not ordered, has non-finite coordinates or gives a non-finite or zero pixel size.
pub(crate) fn bbox_geo_transform(b: [f64; 4], w: f64, h: f64) -> Result<[f64; 6]> {
    if !b.iter().all(|v| v.is_finite()) {
        return Err(Error::InvalidArgument("the bounding box has non-finite coordinates".into()));
    }
    if !(b[0] < b[2] && b[1] < b[3]) {
        return Err(Error::InvalidArgument("the bounding box is not xmin,ymin,xmax,ymax".into()));
    }
    let (dx, dy) = ((b[2] - b[0]) / w, (b[3] - b[1]) / h);
    if !(dx.is_finite() && dx > 0.0 && dy.is_finite() && dy > 0.0) {
        return Err(Error::InvalidArgument("the bounding box gives an invalid pixel size".into()));
    }
    Ok([b[0], dx, 0.0, b[3], 0.0, -dy])
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
    let mut out = vec![0.0; n];
    let len = crate::par::chunk_len(n, 1 << 16);
    crate::pixel::with_data!(&r.data, v => {
        let src = &v[start..start + n];
        crate::par::chunks_mut(&mut out, len, |i, o| {
            for (o, x) in o.iter_mut().zip(&src[i * len..]) {
                *o = crate::pixel::Pixel::to_f64(*x);
            }
        });
    });
    out
}

/// A band's values in their type, read as `f64` where they are used (no `f64` copy of the band).
#[derive(Clone, Copy)]
pub(crate) enum BandView<'a> {
    U8(&'a [u8]),
    I8(&'a [i8]),
    U16(&'a [u16]),
    I16(&'a [i16]),
    U32(&'a [u32]),
    I32(&'a [i32]),
    U64(&'a [u64]),
    I64(&'a [i64]),
    F32(&'a [f32]),
    F64(&'a [f64]),
}

impl<'a> BandView<'a> {
    /// Band `band` (from 0) of a raster.
    pub(crate) fn of(r: &'a Raster, band: usize) -> Self {
        let n = r.rows * r.cols;
        let s = band * n..(band + 1) * n;
        match &r.data {
            wbraster::raster::RasterData::U8(v) => Self::U8(&v[s]),
            wbraster::raster::RasterData::I8(v) => Self::I8(&v[s]),
            wbraster::raster::RasterData::U16(v) => Self::U16(&v[s]),
            wbraster::raster::RasterData::I16(v) => Self::I16(&v[s]),
            wbraster::raster::RasterData::U32(v) => Self::U32(&v[s]),
            wbraster::raster::RasterData::I32(v) => Self::I32(&v[s]),
            wbraster::raster::RasterData::U64(v) => Self::U64(&v[s]),
            wbraster::raster::RasterData::I64(v) => Self::I64(&v[s]),
            wbraster::raster::RasterData::F32(v) => Self::F32(&v[s]),
            wbraster::raster::RasterData::F64(v) => Self::F64(&v[s]),
        }
    }

    /// Value `i`, as `f64`.
    #[inline]
    pub(crate) fn get(self, i: usize) -> f64 {
        use crate::pixel::Pixel;
        match self {
            Self::U8(v) => v[i].to_f64(),
            Self::I8(v) => v[i].to_f64(),
            Self::U16(v) => v[i].to_f64(),
            Self::I16(v) => v[i].to_f64(),
            Self::U32(v) => v[i].to_f64(),
            Self::I32(v) => v[i].to_f64(),
            Self::U64(v) => v[i].to_f64(),
            Self::I64(v) => v[i].to_f64(),
            Self::F32(v) => v[i].to_f64(),
            Self::F64(v) => v[i],
        }
    }

    /// Which values are not `nodata` (see [`is_nodata`]).
    pub(crate) fn valid(self, nodata: f64) -> Vec<bool> {
        crate::par::map_range(self.len(), |i| !is_nodata(self.get(i), nodata))
    }

    /// The number of values.
    pub(crate) fn len(self) -> usize {
        match self {
            Self::U8(v) => v.len(),
            Self::I8(v) => v.len(),
            Self::U16(v) => v.len(),
            Self::I16(v) => v.len(),
            Self::U32(v) => v.len(),
            Self::I32(v) => v.len(),
            Self::U64(v) => v.len(),
            Self::I64(v) => v.len(),
            Self::F32(v) => v.len(),
            Self::F64(v) => v.len(),
        }
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
    north_up(&grid)?;
    let nbands = bands.len().max(1);
    let data = bands_data(&bands, grid.width * grid.height, data_type, convert);
    build_raster_data(grid, data, nbands, nodata, srs)
}

/// The values of bands of `n` pixels (at least one band), converted to `data_type` with `convert`.
pub(crate) fn bands_data(bands: &[Vec<f64>], n: usize, data_type: DataType, convert: fn(f64, DataType) -> f64) -> RasterData {
    let nbands = bands.len().max(1);
    let mut data = RasterData::new_filled(data_type, n * nbands, 0.0);
    let len = crate::par::chunk_len(n, 1 << 16);
    for (b, band) in bands.iter().enumerate() {
        macro_rules! fill {
            ($v:expr, $t:ty) => {
                crate::par::chunks_mut(&mut $v[b * n..(b + 1) * n], len, |i, o| {
                    for (o, x) in o.iter_mut().zip(&band[i * len..]) {
                        *o = convert(*x, data_type) as $t;
                    }
                })
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
    data
}

/// A raster with the size, georeferencing (rotated geotransforms, GCPs and geolocation arrays
/// included), CRS and dataset metadata of `ds`, and new bands: `data` holds `nbands` bands of
/// `ds`'s size. Band properties and nodata values are left for the caller to set.
pub(crate) fn derived(ds: &RasterDataset, data: RasterData, nbands: usize) -> Result<RasterDataset> {
    let r = &ds.raster;
    if data.len() != r.cols * r.rows * nbands {
        return Err(Error::InvalidData(format!("{} values for {nbands} band(s) of {}x{} pixels", data.len(), r.cols, r.rows)));
    }
    let raster = Raster {
        cols: r.cols,
        rows: r.rows,
        bands: nbands,
        x_min: r.x_min,
        y_min: r.y_min,
        cell_size_x: r.cell_size_x,
        cell_size_y: r.cell_size_y,
        nodata: r.nodata,
        data_type: data.data_type(),
        crs: r.crs.clone(),
        metadata: r.metadata.clone(),
        data,
    };
    let mut out = RasterDataset::with_driver(raster, ds.driver, false);
    out.affine = ds.affine;
    out.gcps = ds.gcps.clone();
    out.geolocation = ds.geolocation.clone();
    Ok(out)
}

/// Builds a raster dataset from its values (`nbands` bands, band after band), of their type.
pub(crate) fn build_raster_data(
    grid: Grid,
    data: RasterData,
    nbands: usize,
    nodata: Option<f64>,
    srs: &SpatialRef,
) -> Result<RasterDataset> {
    if data.len() != grid.width * grid.height * nbands {
        return Err(Error::InvalidData(format!("{} values for {nbands} band(s) of {}x{} pixels", data.len(), grid.width, grid.height)));
    }
    let cfg = raster_config(grid, data.data_type(), nbands, nodata, srs)?;
    let raster = crate::pixel::raster_from_data(cfg, data)?;
    Ok(RasterDataset::new(raster, nodata.is_some()))
}

/// The metadata of a raster dataset (size, bands, data type, georeferencing, nodata), without
/// its pixels: its data is empty. For outputs written a strip at a time ([`strips`]).
pub(crate) fn raster_header(
    grid: Grid,
    data_type: DataType,
    nbands: usize,
    nodata: Option<f64>,
    srs: &SpatialRef,
) -> Result<RasterDataset> {
    let cfg = raster_config(grid, data_type, nbands, nodata, srs)?;
    let raster = Raster {
        cols: cfg.cols,
        rows: cfg.rows,
        bands: cfg.bands,
        x_min: cfg.x_min,
        y_min: cfg.y_min,
        cell_size_x: cfg.cell_size,
        cell_size_y: cfg.cell_size_y.unwrap_or(cfg.cell_size),
        nodata: cfg.nodata,
        data_type,
        crs: cfg.crs,
        metadata: cfg.metadata,
        data: RasterData::new_filled(data_type, 0, 0.0),
    };
    Ok(RasterDataset::new(raster, nodata.is_some()))
}

fn raster_config(grid: Grid, data_type: DataType, nbands: usize, nodata: Option<f64>, srs: &SpatialRef) -> Result<RasterConfig> {
    north_up(&grid)?;
    let gt = grid.geo_transform;
    Ok(RasterConfig {
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
    })
}

/// Rasters are built north-up only.
fn north_up(grid: &Grid) -> Result<()> {
    let gt = grid.geo_transform;
    if gt[2] != 0.0 || gt[4] != 0.0 || gt[1] <= 0.0 || gt[5] >= 0.0 {
        return Err(Error::Unsupported("only north-up rasters are supported".into()));
    }
    Ok(())
}

/// The nodata value stored in rasters that declare none (rasters always carry one).
fn default_nodata(dt: DataType) -> f64 {
    match dt {
        DataType::U8 | DataType::U16 | DataType::U32 | DataType::U64 => 0.0,
        _ => -9999.0,
    }
}

/// The coordinate transformation the operations use (datum choices as GDAL makes them).
pub(crate) use crate::crs::Transformer as Ct;

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
