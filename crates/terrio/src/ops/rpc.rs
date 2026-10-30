// SPDX-FileCopyrightText: Copyright (c) 2003, Frank Warmerdam <warmerdam@pobox.com>
// SPDX-FileCopyrightText: Copyright (c) 2009-2013, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 1999, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2007-2013, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) HER MAJESTY THE QUEEN IN RIGHT OF CANADA (2008) as represented by the Canadian Nuclear Safety Commission
// SPDX-FileCopyrightText: Copyright (c) 2014-2015, NextGIS info@nextgis.ru
// SPDX-FileCopyrightText: Copyright (c) 1998, 2002, Frank Warmerdam <warmerdam@pobox.com>
// SPDX-FileCopyrightText: Copyright (c) 2007-2015, Even Rouault <even dot rouault at spatialys dot com>
// SPDX-FileCopyrightText: Copyright (c) 2001, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2008-2012, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2024, Javier Jimenez Shaw
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.13.3:
//     alg/gdal_rpc.cpp
//     gcore/gdal_misc.cpp
//     gcore/gdal_mdreader.cpp
//     frmts/gtiff/geotiff.cpp
//     alg/gdal_interpolateatpoint.cpp
//     alg/gdalresamplingkernels.h

//! Georeferencing by rational polynomial coefficients (RPCs), as GDAL's RPC transformer
//! (`GDALCreateRPCTransformerV2`, `alg/gdal_rpc.cpp`) computes it: longitude, latitude and height
//! to pixel/line by evaluating the polynomials, and pixel/line back to longitude and latitude by
//! GDAL's iterative inverse, at a fixed height (`RPC_HEIGHT`, scaled by `RPC_HEIGHT_SCALE`) or on
//! a DEM (`RPC_DEM`, sampled with `RPC_DEMINTERPOLATION`).
//!
//! ```no_run
//! use terrio::ops::rpc::{Rpc, RpcOptions, RpcTransformer};
//!
//! // An image whose pixels are an affine function of the longitude and latitude.
//! let items: Vec<(String, String)> = [
//!     ("LINE_OFF", "50"), ("SAMP_OFF", "50"), ("LAT_OFF", "45"), ("LONG_OFF", "10"),
//!     ("HEIGHT_OFF", "0"), ("LINE_SCALE", "50"), ("SAMP_SCALE", "50"), ("LAT_SCALE", "0.1"),
//!     ("LONG_SCALE", "0.1"), ("HEIGHT_SCALE", "500"),
//!     ("LINE_NUM_COEFF", "0 0 -1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0"),
//!     ("LINE_DEN_COEFF", "1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0"),
//!     ("SAMP_NUM_COEFF", "0 1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0"),
//!     ("SAMP_DEN_COEFF", "1 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0"),
//! ]
//! .into_iter()
//! .map(|(k, v)| (k.to_string(), v.to_string()))
//! .collect();
//! let rpc = Rpc::from_metadata(&items)?;
//! let t = RpcTransformer::new(&rpc, &RpcOptions::default())?;
//! let (pixel, line) = t.to_pixel(10.05, 44.95, 0.0).unwrap();
//! assert!((pixel - 75.5).abs() < 1e-9 && (line - 75.5).abs() < 1e-9);
//! let (lon, lat) = t.to_geo(pixel, line, 0.0).unwrap();
//! assert!((lon - 10.05).abs() < 1e-6 && (lat - 44.95).abs() < 1e-6);
//! # Ok::<(), terrio::Error>(())
//! ```

use crate::crs::{SpatialRef, Transformer};
use crate::{Error, Result};

/// GDAL's default error, in pixels, of the iterative pixel/line to longitude/latitude solution.
const DEFAULT_PIXEL_ERROR_THRESHOLD: f64 = 0.1;

/// The rational polynomial coefficients of an image: GDAL's `RPC` metadata domain
/// (`GDALRPCInfoV2`).
#[derive(Debug, Clone, PartialEq)]
pub struct Rpc {
    /// The RMS bias error, in metres (-1 when unknown).
    pub err_bias: f64,
    /// The RMS random error, in metres (-1 when unknown).
    pub err_rand: f64,
    /// The line offset.
    pub line_off: f64,
    /// The sample (column) offset.
    pub samp_off: f64,
    /// The latitude offset.
    pub lat_off: f64,
    /// The longitude offset.
    pub long_off: f64,
    /// The height offset.
    pub height_off: f64,
    /// The line scale.
    pub line_scale: f64,
    /// The sample scale.
    pub samp_scale: f64,
    /// The latitude scale.
    pub lat_scale: f64,
    /// The longitude scale.
    pub long_scale: f64,
    /// The height scale.
    pub height_scale: f64,
    /// The 20 coefficients of the line numerator.
    pub line_num_coeff: [f64; 20],
    /// The 20 coefficients of the line denominator.
    pub line_den_coeff: [f64; 20],
    /// The 20 coefficients of the sample numerator.
    pub samp_num_coeff: [f64; 20],
    /// The 20 coefficients of the sample denominator.
    pub samp_den_coeff: [f64; 20],
    /// The validity area of the coefficients (`MIN_LONG`, `MIN_LAT`, `MAX_LONG`, `MAX_LAT`;
    /// -180, -90, 180, 90 when not given).
    pub min_long: f64,
    /// See [`Rpc::min_long`].
    pub min_lat: f64,
    /// See [`Rpc::min_long`].
    pub max_long: f64,
    /// See [`Rpc::min_long`].
    pub max_lat: f64,
    /// The height used when neither `RPC_HEIGHT` nor `RPC_DEM` is given (`HEIGHT_DEFAULT`, which
    /// some drivers set), else 0.
    pub height_default: Option<f64>,
}

impl Rpc {
    /// Reads GDAL's `RPC` metadata items (`LINE_OFF`, `LINE_NUM_COEFF`, ...), as
    /// `GDALExtractRPCInfoV2` does: missing scalars take GDAL's defaults, numbers are read up to
    /// the first character that is not part of one.
    ///
    /// # Errors
    /// When one of the four coefficient lists is missing.
    pub fn from_metadata(items: &[(String, String)]) -> Result<Self> {
        let get = |k: &str| items.iter().find(|(key, _)| key.eq_ignore_ascii_case(k)).map(|(_, v)| v.as_str());
        let scalar = |k: &str, default: f64| get(k).and_then(|v| tokens(v).first().map(|t| atof(t))).unwrap_or(default);
        let coeffs = |k: &str| -> Result<[f64; 20]> {
            let v = get(k).ok_or_else(|| Error::InvalidArgument(format!("the RPC item {k} is missing")))?;
            let mut out = [0.0; 20];
            for (o, t) in out.iter_mut().zip(tokens(v)) {
                *o = atof(t);
            }
            Ok(out)
        };
        Ok(Self {
            err_bias: scalar("ERR_BIAS", -1.0),
            err_rand: scalar("ERR_RAND", -1.0),
            line_off: scalar("LINE_OFF", 0.0),
            samp_off: scalar("SAMP_OFF", 0.0),
            lat_off: scalar("LAT_OFF", 0.0),
            long_off: scalar("LONG_OFF", 0.0),
            height_off: scalar("HEIGHT_OFF", 0.0),
            line_scale: scalar("LINE_SCALE", 1.0),
            samp_scale: scalar("SAMP_SCALE", 1.0),
            lat_scale: scalar("LAT_SCALE", 1.0),
            long_scale: scalar("LONG_SCALE", 1.0),
            height_scale: scalar("HEIGHT_SCALE", 1.0),
            line_num_coeff: coeffs("LINE_NUM_COEFF")?,
            line_den_coeff: coeffs("LINE_DEN_COEFF")?,
            samp_num_coeff: coeffs("SAMP_NUM_COEFF")?,
            samp_den_coeff: coeffs("SAMP_DEN_COEFF")?,
            min_long: scalar("MIN_LONG", -180.0),
            min_lat: scalar("MIN_LAT", -90.0),
            max_long: scalar("MAX_LONG", 180.0),
            max_lat: scalar("MAX_LAT", 90.0),
            height_default: get("HEIGHT_DEFAULT").map(atof),
        })
    }

    /// Reads the 92 values of a GeoTIFF `RPCCoefficientTag`, as GDAL reads them: turned into its
    /// metadata items with 15 significant digits.
    ///
    /// # Errors
    /// When there are not 92 values.
    pub fn from_tiff_tag(values: &[f64]) -> Result<Self> {
        if values.len() != 92 {
            return Err(Error::InvalidArgument(format!("an RPCCoefficientTag has 92 values, not {}", values.len())));
        }
        let v: Vec<f64> = values.iter().map(|x| round15(*x)).collect();
        let list = |i: usize| -> [f64; 20] { v[12 + 20 * i..32 + 20 * i].try_into().unwrap_or([0.0; 20]) };
        Ok(Self {
            err_bias: v[0],
            err_rand: v[1],
            line_off: v[2],
            samp_off: v[3],
            lat_off: v[4],
            long_off: v[5],
            height_off: v[6],
            line_scale: v[7],
            samp_scale: v[8],
            lat_scale: v[9],
            long_scale: v[10],
            height_scale: v[11],
            line_num_coeff: list(0),
            line_den_coeff: list(1),
            samp_num_coeff: list(2),
            samp_den_coeff: list(3),
            min_long: -180.0,
            min_lat: -90.0,
            max_long: 180.0,
            max_lat: 90.0,
            height_default: None,
        })
    }

    /// The 92 values of a GeoTIFF `RPCCoefficientTag`.
    pub fn to_tiff_tag(&self) -> Vec<f64> {
        let mut v = vec![
            self.err_bias,
            self.err_rand,
            self.line_off,
            self.samp_off,
            self.lat_off,
            self.long_off,
            self.height_off,
            self.line_scale,
            self.samp_scale,
            self.lat_scale,
            self.long_scale,
            self.height_scale,
        ];
        for c in [&self.line_num_coeff, &self.line_den_coeff, &self.samp_num_coeff, &self.samp_den_coeff] {
            v.extend_from_slice(c);
        }
        v
    }

    /// The pixel/line position of a longitude, latitude and height (`RPCTransformPoint`): the
    /// polynomials evaluated, with GDAL's convention that (0, 0) is the corner of the first pixel.
    pub fn evaluate(&self, lon: f64, lat: f64, height: f64) -> (f64, f64) {
        // Avoid dateline issues.
        let mut dlon = lon - self.long_off;
        if dlon < -270.0 {
            dlon += 360.0;
        } else if dlon > 270.0 {
            dlon -= 360.0;
        }
        let l = dlon / self.long_scale;
        let p = (lat - self.lat_off) / self.lat_scale;
        let h = (height - self.height_off) / self.height_scale;
        let terms = [
            1.0,
            l,
            p,
            h,
            l * p,
            l * h,
            p * h,
            l * l,
            p * p,
            h * h,
            l * p * h,
            l * l * l,
            l * p * p,
            l * h * h,
            l * l * p,
            p * p * p,
            p * h * h,
            l * l * h,
            p * p * h,
            h * h * h,
        ];
        // Even and odd terms summed apart, then added, as GDAL's SSE2 code (and its scalar code)
        // sums them.
        let eval = |c: &[f64; 20]| {
            let (mut a, mut b) = (0.0, 0.0);
            for i in (0..20).step_by(2) {
                a += terms[i] * c[i];
                b += terms[i + 1] * c[i + 1];
            }
            a + b
        };
        let x = eval(&self.samp_num_coeff) / eval(&self.samp_den_coeff);
        let y = eval(&self.line_num_coeff) / eval(&self.line_den_coeff);
        (x * self.samp_scale + self.samp_off + 0.5, y * self.line_scale + self.line_off + 0.5)
    }
}

/// The value printed with 15 significant digits and read back (`%.15g`).
fn round15(v: f64) -> f64 {
    if !v.is_finite() || v == 0.0 {
        return v;
    }
    format!("{v:.14e}").parse().unwrap_or(v)
}

/// The tokens of a metadata value, split on spaces, tabs and commas, as GDAL's
/// `_FetchDblFromMD` splits them.
fn tokens(s: &str) -> Vec<&str> {
    s.split([' ', '\t', ',', '\n', '\r']).filter(|t| !t.is_empty()).collect()
}

/// The number at the start of a string, 0 when none (C's `atof`, which `CPLAtof` is).
fn atof(s: &str) -> f64 {
    let s = s.trim_start();
    let mut best = 0.0;
    // The longest prefix of number characters that parses.
    for (i, c) in s.char_indices() {
        if !matches!(c, '+' | '-' | '.' | 'e' | 'E' | '0'..='9') {
            break;
        }
        if let Ok(v) = s[..i + 1].parse::<f64>() {
            best = v;
        }
    }
    best
}

/// How the DEM is sampled (`RPC_DEMINTERPOLATION`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum DemInterpolation {
    /// The nearest DEM cell (`near`).
    Near,
    /// Bilinear interpolation (`bilinear`, GDAL's default).
    #[default]
    Bilinear,
    /// A cubic B-spline (`cubic`).
    Cubic,
}

impl DemInterpolation {
    /// Parses `near`, `bilinear` or `cubic` (any case); other names give bilinear, as in GDAL.
    pub fn parse(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "near" => Self::Near,
            "cubic" => Self::Cubic,
            _ => Self::Bilinear,
        }
    }
}

/// The options of an RPC transformer, named after GDAL's transformer options.
#[derive(Debug, Clone, PartialEq)]
pub struct RpcOptions {
    /// A fixed height (`RPC_HEIGHT`), added to the heights given; or, with a DEM, an offset added
    /// to its heights.
    pub height: Option<f64>,
    /// A factor multiplying the DEM heights (`RPC_HEIGHT_SCALE`, 1 by default).
    pub height_scale: f64,
    /// A DEM giving the ground height (`RPC_DEM`): a raster path, read whole.
    pub dem: Option<String>,
    /// How the DEM is sampled (`RPC_DEMINTERPOLATION`).
    pub dem_interpolation: DemInterpolation,
    /// The DEM height used where the DEM has no value or does not reach
    /// (`RPC_DEM_MISSING_VALUE`); without it, such points fail to transform.
    pub dem_missing_value: Option<f64>,
    /// The DEM's CRS, overriding the one it declares (`RPC_DEM_SRS`).
    pub dem_srs: Option<String>,
    /// Whether the heights of a DEM in a compound CRS are moved from its vertical datum to the
    /// WGS 84 ellipsoid (`RPC_DEM_APPLY_VDATUM_SHIFT`, true by default). terrio does not move
    /// them: a compound `RPC_DEM_SRS` needs this set to false. (GeoTIFF DEMs are read with their
    /// horizontal CRS only, their heights taken as they are.)
    pub dem_apply_vdatum_shift: bool,
    /// The error, in pixels, at which the iterative pixel/line to longitude/latitude solution
    /// stops (`RPC_PIXEL_ERROR_THRESHOLD`; 0.1 by default).
    pub pixel_error_threshold: Option<f64>,
    /// The most iterations of that solution (`RPC_MAX_ITERATIONS`; 0 for GDAL's default: 10, or
    /// 20 with a DEM).
    pub max_iterations: u32,
}

impl Default for RpcOptions {
    fn default() -> Self {
        Self {
            height: None,
            height_scale: 1.0,
            dem: None,
            dem_interpolation: DemInterpolation::Bilinear,
            dem_missing_value: None,
            dem_srs: None,
            dem_apply_vdatum_shift: true,
            pixel_error_threshold: None,
            max_iterations: 0,
        }
    }
}

impl RpcOptions {
    /// Options from GDAL's transformer options (`RPC_HEIGHT=...`, as `gdalwarp -to` takes them).
    /// Options not about RPCs are ignored.
    ///
    /// # Errors
    /// For `RPC_FOOTPRINT`, which terrio does not support, and values that are not numbers.
    pub fn from_options(options: &[(String, String)]) -> Result<Self> {
        let mut o = Self::default();
        let num = |k: &str, v: &str| -> Result<f64> {
            v.trim().parse::<f64>().map_err(|_| Error::InvalidArgument(format!("{k} must be a number, not '{v}'")))
        };
        for (k, v) in options {
            match k.to_ascii_uppercase().as_str() {
                "RPC_HEIGHT" => o.height = Some(num(k, v)?),
                "RPC_HEIGHT_SCALE" => o.height_scale = num(k, v)?,
                "RPC_DEM" => o.dem = Some(v.clone()),
                "RPC_DEMINTERPOLATION" => o.dem_interpolation = DemInterpolation::parse(v),
                "RPC_DEM_MISSING_VALUE" => o.dem_missing_value = Some(num(k, v)?),
                "RPC_DEM_SRS" => o.dem_srs = Some(v.clone()),
                "RPC_DEM_APPLY_VDATUM_SHIFT" => {
                    o.dem_apply_vdatum_shift = !matches!(v.to_ascii_uppercase().as_str(), "NO" | "FALSE" | "OFF" | "0")
                }
                "RPC_PIXEL_ERROR_THRESHOLD" => o.pixel_error_threshold = Some(num(k, v)?),
                "RPC_MAX_ITERATIONS" => {
                    o.max_iterations =
                        v.trim().parse::<i64>().map_err(|_| Error::InvalidArgument(format!("{k} must be an integer, not '{v}'")))?.max(0)
                            as u32
                }
                "RPC_FOOTPRINT" => return Err(Error::Unsupported("RPC_FOOTPRINT is not supported".into())),
                _ => {}
            }
        }
        Ok(o)
    }
}

/// A DEM sampled for heights, band 1 in memory.
struct Dem {
    width: usize,
    height: usize,
    data: Vec<f64>,
    nodata: Option<f64>,
    gt: [f64; 6],
    inv: [f64; 6],
    /// WGS 84 longitude/latitude to the DEM's CRS, when it is another CRS.
    ct: Option<Transformer>,
    interpolation: DemInterpolation,
}

/// GDAL's `ARE_REAL_EQUAL` for doubles (which uses a float epsilon).
fn real_eq(a: f64, b: f64) -> bool {
    a == b || (a - b).abs() < f32::EPSILON as f64 * (a + b).abs() * 2.0
}

/// GDAL's cubic B-spline kernel (`CubicSplineKernel`).
fn cubic_spline(v: f64) -> f64 {
    if v > 2.0 {
        return 0.0;
    }
    let (xm1, xp1, xp2) = (v - 1.0, v + 1.0, v + 2.0);
    let a = if xp2 <= 0.0 { 0.0 } else { xp2 * xp2 * xp2 };
    let b = if xp1 <= 0.0 { 0.0 } else { xp1 * xp1 * xp1 };
    let c = if v <= 0.0 { 0.0 } else { v * v * v };
    let d = if xm1 <= 0.0 { 0.0 } else { xm1 * xm1 * xm1 };
    (1.0 / 6.0) * (a - (4.0 * b) + (6.0 * c) - (4.0 * d))
}

impl Dem {
    fn open(path: &str, o: &RpcOptions, rpc: &Rpc) -> Result<Self> {
        let ds = crate::open_raster(path)?;
        let gt = ds.geo_transform();
        let inv =
            super::invert_gt(&gt).ok_or_else(|| Error::InvalidData(format!("the geotransform of the DEM {path} is not invertible")))?;
        let srs = match &o.dem_srs {
            Some(s) => SpatialRef::parse(s)?,
            None => ds.crs(),
        };
        let ct = if srs.is_unknown() { None } else { Self::transformer(&srs, o.dem_apply_vdatum_shift, rpc)? };
        Ok(Self {
            width: ds.raster.cols,
            height: ds.raster.rows,
            data: super::band_f64(&ds.raster, 0),
            nodata: ds.nodata(),
            gt,
            inv,
            ct,
            interpolation: o.dem_interpolation,
        })
    }

    /// The transformation from WGS 84 to the DEM's CRS, as `GDALRPCOpenDEM` sets it up: none when
    /// the DEM is in WGS 84 or the transformation is found to change nothing.
    fn transformer(srs: &SpatialRef, vdatum: bool, rpc: &Rpc) -> Result<Option<Transformer>> {
        let compound = srs.to_wkt().is_some_and(|w| {
            let w = w.trim_start().to_ascii_uppercase();
            w.starts_with("COMPD_CS") || w.starts_with("COMPOUNDCRS")
        });
        if compound && vdatum {
            return Err(Error::Unsupported(
                "moving the heights of a DEM in a compound CRS to the ellipsoid is not supported: set RPC_DEM_APPLY_VDATUM_SHIFT=NO".into(),
            ));
        }
        let srs = if compound { horizontal(srs)? } else { srs.clone() };
        let wgs84 = SpatialRef::from_epsg(4326)?;
        if srs.same_as(&wgs84) {
            return Ok(None);
        }
        let t = Transformer::new(&wgs84, &srs)?;
        // An apparent no-op (a DEM in NAD83, say) is not applied, as GDAL does.
        let (rx, ry) = if rpc.min_long != -180.0 || rpc.max_long != 180.0 {
            ((rpc.min_long + rpc.max_long) * 0.5, (rpc.min_lat + rpc.max_lat) * 0.5)
        } else {
            (rpc.long_off, rpc.lat_off)
        };
        let pts = [(-179.0, 89.0), (179.0, 89.0), (179.0, -89.0), (-179.0, -89.0), (0.0, 0.0), (rx, ry)];
        let nop = pts.iter().all(|&(x, y)| t.transform(x, y).is_ok_and(|(a, b)| (a - x).abs() < 1e-12 && (b - y).abs() < 1e-12));
        Ok(if nop { None } else { Some(t) })
    }

    fn value(&self, x: i64, y: i64) -> f64 {
        let x = x.clamp(0, self.width as i64 - 1) as usize;
        let y = y.clamp(0, self.height as i64 - 1) as usize;
        self.data[y * self.width + x]
    }

    fn is_nodata(&self, v: f64) -> bool {
        self.nodata.is_some_and(|n| real_eq(n, v))
    }

    /// The DEM's value at a DEM pixel/line position (`GDALInterpolateAtPoint`), or `None` outside
    /// the DEM or on nodata.
    fn sample(&self, mut x: f64, mut y: f64) -> Option<f64> {
        let (w, h) = (self.width as f64, self.height as f64);
        let mut method = self.interpolation;
        if method == DemInterpolation::Near {
            if x >= w && x <= w + 1e-5 {
                x -= 0.25;
            }
            if y >= h && y <= h + 1e-5 {
                y -= 0.25;
            }
        }
        if !(0.0..=w).contains(&x) || !(0.0..=h).contains(&y) {
            return None;
        }
        if (self.width < 4 || self.height < 4) && method == DemInterpolation::Cubic {
            method = DemInterpolation::Bilinear;
        }
        if (self.width < 2 || self.height < 2) && method == DemInterpolation::Bilinear {
            method = DemInterpolation::Near;
        }
        match method {
            DemInterpolation::Cubic => {
                let (fx, fy) = (x - 0.5, y - 0.5);
                let (ix, iy) = (fx.floor() as i64, fy.floor() as i64);
                let (dx, dy) = (fx - ix as f64, fy - iy as f64);
                let (mut sum, mut weight) = (0.0, 0.0);
                for ki in 0..4i64 {
                    for kj in 0..4i64 {
                        let wgt = cubic_spline((kj - 1) as f64 - dx) * cubic_spline((ki - 1) as f64 - dy);
                        let v = self.value(ix - 1 + kj, iy - 1 + ki);
                        if self.is_nodata(v) {
                            continue;
                        }
                        sum += v * wgt;
                        weight += wgt;
                    }
                }
                (weight != 0.0).then(|| sum / weight)
            }
            DemInterpolation::Bilinear => {
                let (fx, fy) = (x - 0.5, y - 0.5);
                let (ix, iy) = (fx.floor() as i64, fy.floor() as i64);
                let (dx, dy) = (fx - ix as f64, fy - iy as f64);
                let v = [self.value(ix, iy), self.value(ix + 1, iy), self.value(ix, iy + 1), self.value(ix + 1, iy + 1)];
                if v.iter().any(|&e| self.is_nodata(e)) {
                    return None;
                }
                let (dx1, dy1) = (1.0 - dx, 1.0 - dy);
                let z1 = v[0] * dx1 + v[1] * dx;
                let z2 = v[2] * dx1 + v[3] * dx;
                Some(z1 * dy1 + z2 * dy)
            }
            DemInterpolation::Near => {
                let v = self.value(x as i64, y as i64);
                (!self.is_nodata(v)).then_some(v)
            }
        }
    }
}

/// The horizontal CRS of a compound CRS (its first component).
fn horizontal(srs: &SpatialRef) -> Result<SpatialRef> {
    let wkt = srs.to_wkt().unwrap_or_default();
    // The first bracketed component after the compound CRS's (quoted) name.
    let start = wkt.find('[').map(|i| i + 1).unwrap_or(0);
    let mut rest = wkt[start..].trim_start();
    if let Some(name) = rest.strip_prefix('"') {
        rest = name.find('"').map_or("", |i| &name[i + 1..]);
    }
    let comma = rest.find(',').map(|i| i + 1).unwrap_or(0);
    let rest = rest[comma..].trim_start();
    let mut depth = 0i32;
    for (i, c) in rest.char_indices() {
        match c {
            '[' | '(' => depth += 1,
            ']' | ')' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(SpatialRef::from_wkt(&rest[..=i]));
                }
            }
            _ => {}
        }
    }
    Err(Error::Crs("cannot read the horizontal CRS of the DEM's compound CRS".into()))
}

/// GDAL's RPC transformer: pixel/line to longitude/latitude (iteratively) and back, at the
/// heights given plus a fixed height or a DEM's.
pub struct RpcTransformer {
    rpc: Rpc,
    pixel_error_threshold: f64,
    height_offset: f64,
    height_scale: f64,
    dem_missing_value: Option<f64>,
    max_iterations: u32,
    dem: Option<Dem>,
    /// The affine approximation of pixel/line to longitude/latitude, the iterations' start.
    pl_to_ll: [f64; 6],
    /// The height at the reference point.
    ref_z: f64,
}

impl std::fmt::Debug for RpcTransformer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcTransformer")
            .field("rpc", &self.rpc)
            .field("height_offset", &self.height_offset)
            .field("height_scale", &self.height_scale)
            .field("dem", &self.dem.as_ref().map(|d| (d.width, d.height)))
            .finish_non_exhaustive()
    }
}

impl RpcTransformer {
    /// The transformer `GDALCreateRPCTransformerV2` creates for `rpc` with `options` (the DEM is
    /// read whole).
    ///
    /// # Errors
    /// When the DEM cannot be read, or the RPCs are degenerate near their reference point.
    pub fn new(rpc: &Rpc, options: &RpcOptions) -> Result<Self> {
        let mut height = options.height;
        if height.is_none() && options.dem.is_none() {
            height = rpc.height_default;
        }
        let dem = match &options.dem {
            Some(p) => Some(Dem::open(p, options, rpc)?),
            None => None,
        };
        let mut t = Self {
            rpc: rpc.clone(),
            pixel_error_threshold: options.pixel_error_threshold.unwrap_or(DEFAULT_PIXEL_ERROR_THRESHOLD),
            height_offset: height.unwrap_or(0.0),
            height_scale: options.height_scale,
            dem_missing_value: options.dem_missing_value,
            max_iterations: options.max_iterations,
            dem,
            pl_to_ll: [0.0; 6],
            ref_z: 0.0,
        };
        // A reference point and the affine approximation around it.
        let (mut ref_pixel, mut ref_line) = (-1.0, -1.0);
        let (mut ref_lon, mut ref_lat) = (0.0, 0.0);
        if rpc.min_long != -180.0 || rpc.max_long != 180.0 {
            ref_lon = (rpc.min_long + rpc.max_long) * 0.5;
            ref_lat = (rpc.min_lat + rpc.max_lat) * 0.5;
            (ref_pixel, ref_line) = t.to_pixel(ref_lon, ref_lat, 0.0).unwrap_or_else(|| rpc.evaluate(ref_lon, ref_lat, 0.0));
        }
        if !(0.0..=100000.0).contains(&ref_pixel) || !(0.0..=100000.0).contains(&ref_line) {
            ref_lon = rpc.long_off;
            ref_lat = rpc.lat_off;
            (ref_pixel, ref_line) = t.to_pixel(ref_lon, ref_lat, 0.0).unwrap_or_else(|| rpc.evaluate(ref_lon, ref_lat, 0.0));
        }
        if let Some(z) = t.height_at(ref_lon, ref_lat).0 {
            t.ref_z = z;
        }
        let delta = 0.0001;
        let mut gt = [0.0; 6];
        let (px, py) = rpc.evaluate(ref_lon + delta, ref_lat, t.ref_z);
        gt[1] = (px - ref_pixel) / delta;
        gt[4] = (py - ref_line) / delta;
        let (px, py) = rpc.evaluate(ref_lon, ref_lat + delta, t.ref_z);
        gt[2] = (px - ref_pixel) / delta;
        gt[5] = (py - ref_line) / delta;
        gt[0] = ref_pixel - gt[1] * ref_lon - gt[2] * ref_lat;
        gt[3] = ref_line - gt[4] * ref_lon - gt[5] * ref_lat;
        t.pl_to_ll = super::invert_gt(&gt).ok_or_else(|| Error::InvalidData("cannot invert the RPCs' approximate geotransform".into()))?;
        Ok(t)
    }

    /// The RPCs.
    pub fn rpc(&self) -> &Rpc {
        &self.rpc
    }

    /// Whether heights come from a DEM.
    pub fn has_dem(&self) -> bool {
        self.dem.is_some()
    }

    /// The height at a longitude and latitude (`GDALRPCGetHeightAtLongLat`): the fixed height, or
    /// the DEM's scaled and offset; `None` where the DEM has none. Also the DEM pixel/line
    /// position last sampled.
    fn height_at(&self, lon: f64, lat: f64) -> (Option<f64>, (f64, f64)) {
        let mut dem_h = 0.0;
        let mut at = (0.0, 0.0);
        if let Some(d) = &self.dem {
            let (mut x, mut y) = (lon, lat);
            if let Some(ct) = &d.ct {
                match ct.transform(x, y) {
                    Ok(p) => (x, y) = p,
                    Err(_) => return (None, at),
                }
            }
            let mut retried = false;
            loop {
                at = super::apply_gt(&d.inv, x, y);
                if let Some(v) = d.sample(at.0, at.1) {
                    dem_h = v;
                    break;
                }
                // A DEM spanning [-180, 180] with the longitude beyond it.
                if !retried && d.ct.is_none() && (lon >= 180.0 || lon <= -180.0) {
                    let min = d.gt[0];
                    let max = d.gt[0] + d.width as f64 * d.gt[1];
                    if (min + 180.0).abs() < 0.1 && (max - 180.0).abs() < 0.1 {
                        x = if lon >= 180.0 { lon - 360.0 } else { lon + 360.0 };
                        y = lat;
                        retried = true;
                        continue;
                    }
                }
                match self.dem_missing_value {
                    Some(m) => dem_h = m,
                    None => return (None, at),
                }
                break;
            }
        }
        (Some(self.height_offset + dem_h * self.height_scale), at)
    }

    /// The pixel/line position of a longitude, latitude and height above the fixed height or the
    /// DEM, or `None` where the DEM has no height.
    pub fn to_pixel(&self, lon: f64, lat: f64, z: f64) -> Option<(f64, f64)> {
        if !lon.is_finite() || !lat.is_finite() {
            return None;
        }
        let h = self.height_at(lon, lat).0?;
        Some(self.rpc.evaluate(lon, lat, z + h))
    }

    /// Transforms longitudes and latitudes to pixel/line positions in place, as `GDALRPCTransform`
    /// does for many points: rows of points at one latitude are sampled from one row of a WGS 84
    /// DEM (GDAL's `GDALRPCTransformWholeLineWithDEM`). `ok` tells which points succeeded; points
    /// that are already failed (`ok` false on input) stay so.
    pub fn to_pixel_many(&self, xs: &mut [f64], ys: &mut [f64], zs: Option<&[f64]>, ok: &mut [bool]) {
        let n = xs.len();
        let z = |i: usize| zs.map_or(0.0, |z| z[i]);
        if n >= 10
            && let Some(d) = &self.dem
            && d.ct.is_none()
            && d.inv[1] > 0.0
            && d.inv[2] == 0.0
            && d.inv[4] == 0.0
            && ok.iter().all(|b| *b)
            && xs.iter().all(|x| x.is_finite())
            && ys.iter().all(|y| *y == ys[0])
            && self.whole_line(d, xs, ys, &z, ok)
        {
            return;
        }
        for i in 0..n {
            if !ok[i] {
                continue;
            }
            match self.to_pixel(xs[i], ys[i], z(i)) {
                Some((a, b)) => {
                    xs[i] = a;
                    ys[i] = b;
                }
                None => ok[i] = false,
            }
        }
    }

    /// GDAL's `GDALRPCTransformWholeLineWithDEM`, when the row of the DEM the points need is
    /// inside it; false otherwise (nothing done). Its height is GDAL's for this path,
    /// `(RPC_HEIGHT + dem) * RPC_HEIGHT_SCALE`, where a point alone gets
    /// `RPC_HEIGHT + dem * RPC_HEIGHT_SCALE` (`height_at`): both kept as GDAL computes them.
    fn whole_line(&self, d: &Dem, xs: &mut [f64], ys: &mut [f64], z: &dyn Fn(usize) -> f64, ok: &mut [bool]) -> bool {
        let n = xs.len();
        let (min_x, max_x) = xs.iter().fold((xs[0], xs[0]), |(a, b), x| (a.min(*x), b.max(*x)));
        let (mut x1, mut y1) = super::apply_gt(&d.inv, min_x, ys[0]);
        let (mut x2, _) = super::apply_gt(&d.inv, max_x, ys[0]);
        if d.interpolation != DemInterpolation::Near {
            x1 -= 0.5;
            y1 -= 0.5;
            x2 -= 0.5;
        }
        let mut left = x1.floor() as i64;
        let right = x2.floor() as i64;
        let mut width = right - left + 1;
        let mut top = y1.floor() as i64;
        let rows = match d.interpolation {
            DemInterpolation::Cubic => {
                left -= 1;
                width += 3;
                top -= 1;
                4
            }
            DemInterpolation::Bilinear => {
                width += 1;
                2
            }
            DemInterpolation::Near => 1,
        };
        if left < 0 || left + width > d.width as i64 || top < 0 || top + rows > d.height as i64 {
            return false;
        }
        let buf = |r: i64, c: i64| d.data[(top + r) as usize * d.width + c as usize];
        let fy = d.inv[3] + ys[0] * d.inv[5] - 0.5;
        let iy = fy as i64;
        let dy = fy - iy as f64;
        for i in 0..n {
            let dem_h;
            match d.interpolation {
                DemInterpolation::Cubic => {
                    let fx = d.inv[0] + xs[i] * d.inv[1] - 0.5;
                    let ix = fx as i64;
                    let dx = fx - ix as f64;
                    let (mut sum, mut weight) = (0.0, 0.0);
                    for ki in 0..4i64 {
                        for kj in 0..4i64 {
                            let wgt = cubic_spline((kj - 1) as f64 - dx) * cubic_spline((ki - 1) as f64 - dy);
                            let v = buf(ki, ix - 1 + kj);
                            if d.is_nodata(v) {
                                continue;
                            }
                            sum += v * wgt;
                            weight += wgt;
                        }
                    }
                    if weight == 0.0 {
                        match self.dem_missing_value {
                            Some(m) => dem_h = m,
                            None => {
                                ok[i] = false;
                                continue;
                            }
                        }
                    } else {
                        dem_h = sum / weight;
                    }
                }
                DemInterpolation::Bilinear => {
                    let fx = d.inv[0] + xs[i] * d.inv[1] - 0.5;
                    let ix = fx as i64;
                    let dx = fx - ix as f64;
                    let v = [buf(0, ix), buf(0, ix + 1), buf(1, ix), buf(1, ix + 1)];
                    if d.nodata.is_some() && v.iter().any(|e| d.is_nodata(*e)) {
                        let h = match v.iter().find(|e| !d.is_nodata(**e)) {
                            Some(e) => *e,
                            None => match self.dem_missing_value {
                                Some(m) => m,
                                None => {
                                    ok[i] = false;
                                    continue;
                                }
                            },
                        };
                        (xs[i], ys[i]) = self.rpc.evaluate(xs[i], ys[i], z(i) + (self.height_offset + h) * self.height_scale);
                        ok[i] = true;
                        continue;
                    }
                    let (dx1, dy1) = (1.0 - dx, 1.0 - dy);
                    let z1 = v[0] * dx1 + v[1] * dx;
                    let z2 = v[2] * dx1 + v[3] * dx;
                    dem_h = z1 * dy1 + z2 * dy;
                }
                DemInterpolation::Near => {
                    let fx = d.inv[0] + xs[i] * d.inv[1];
                    let v = buf(0, fx as i64);
                    if d.is_nodata(v) {
                        match self.dem_missing_value {
                            Some(m) => dem_h = m,
                            None => {
                                ok[i] = false;
                                continue;
                            }
                        }
                    } else {
                        dem_h = v;
                    }
                }
            }
            (xs[i], ys[i]) = self.rpc.evaluate(xs[i], ys[i], z(i) + (self.height_offset + dem_h) * self.height_scale);
            ok[i] = true;
        }
        true
    }

    /// The longitude and latitude of a pixel/line position at a height above the fixed height or
    /// the DEM, by GDAL's iterative solution (`RPCInverseTransformPoint`); `None` when it does
    /// not converge.
    pub fn to_geo(&self, pixel: f64, line: f64, z: f64) -> Option<(f64, f64)> {
        if !pixel.is_finite() || !line.is_finite() {
            return None;
        }
        let g = &self.pl_to_ll;
        let mut rx = g[0] + g[1] * pixel + g[2] * line;
        let mut ry = g[3] + g[4] * pixel + g[5] * line;
        let (mut last_x, mut last_y) = (0.0, 0.0);
        let (mut last_dx, mut last_dy) = (0.0, 0.0);
        let mut last_valid = false;
        let max_iter = if self.max_iterations > 0 {
            self.max_iterations
        } else if self.dem.is_some() {
            20
        } else {
            10
        };
        let mut below2 = 0;
        for iter in 0..max_iter {
            let (dem_h, (mut dp, mut dl)) = self.height_at(rx, ry);
            let dem_h = match dem_h {
                Some(h) => h,
                None if iter == 0 => {
                    // The first guess may be off the DEM: the reference height, or the DEM's raw
                    // value at its nearest edge.
                    let mut h = self.ref_z;
                    if let Some(d) = &self.dem {
                        let (w, hh) = (d.width as f64, d.height as f64);
                        if dp >= w {
                            dp = w - 0.5;
                        } else if dp < 0.0 {
                            dp = 0.5;
                        }
                        // GDAL tests (and resets) the pixel again here, not the line: kept so,
                        // for the same results.
                        if dl >= hh {
                            dl = hh - 0.5;
                        } else if dp < 0.0 {
                            dp = 0.5;
                        }
                        if let Some(v) = d.sample(dp, dl) {
                            h = v;
                        }
                    }
                    h
                }
                None => return None,
            };
            let (bp, bl) = self.rpc.evaluate(rx, ry, z + dem_h);
            let (dx, dy) = (bp - pixel, bl - line);
            let err = dx.abs().max(dy.abs());
            if err < self.pixel_error_threshold {
                return Some((rx, ry));
            }
            if self.dem.is_some() && last_valid && dx * last_dx < 0.0 && dy * last_dy < 0.0 {
                // Oscillating: the mean of the last two guesses.
                rx = (dx.abs() * last_x + last_dx.abs() * rx) / (dx.abs() + last_dx.abs());
                ry = (dy.abs() * last_y + last_dy.abs() * ry) / (dy.abs() + last_dy.abs());
                last_valid = false;
                below2 = 0;
                continue;
            }
            let boost = if self.dem.is_some() && below2 >= 5 && err < 2.0 { 10.0 } else { 1.0 };
            if err < 2.0 {
                below2 += 1;
            } else {
                below2 = 0;
            }
            let nx = rx - (dx * g[1] * boost) - (dy * g[2] * boost);
            let ny = ry - (dx * g[4] * boost) - (dy * g[5] * boost);
            (last_x, last_y) = (rx, ry);
            (rx, ry) = (nx, ny);
            (last_dx, last_dy) = (dx, dy);
            last_valid = true;
        }
        None
    }
}

/// The RPCs of a raster file, as GDAL reads them: the GeoTIFF `RPCCoefficientTag`, else a
/// `_RPC.TXT` or `.RPB` sidecar. `None` when it has none.
///
/// # Errors
/// When the file cannot be resolved (a remote file that cannot be fetched).
pub fn read_rpc(path: &str) -> Result<Option<Rpc>> {
    let local = crate::vfs::resolve(path)?;
    if let Some(tag) = crate::geotiff::read_geo_tags(local.path()).and_then(|t| t.rpc) {
        return Rpc::from_tiff_tag(&tag).map(Some);
    }
    rpc_sidecar(local.path())
}

/// The RPCs of the `_RPC.TXT` or `.RPB` sidecar of a local file, when it has one.
pub(crate) fn rpc_sidecar(p: &std::path::Path) -> Result<Option<Rpc>> {
    let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else { return Ok(None) };
    let dir = p.parent().unwrap_or(std::path::Path::new(""));
    for side in [format!("{stem}_rpc.txt"), format!("{stem}_RPC.TXT"), format!("{stem}.RPB"), format!("{stem}.rpb")] {
        let f = dir.join(&side);
        if let Ok(text) = std::fs::read(&f) {
            let text: String = text.iter().map(|b| *b as char).collect();
            if let Some(items) = parse_rpc_file(&text, side.to_ascii_lowercase().ends_with(".rpb")) {
                return Rpc::from_metadata(&items).map(Some);
            }
        }
    }
    Ok(None)
}

/// GDAL's `RPC` items of a `_RPC.TXT` (or, with `rpb`, an `.RPB`) sidecar's text
/// (`GDALLoadRPCFile`, `GDALLoadRPBFile`), or `None` when it lacks some.
pub fn parse_rpc_file(text: &str, rpb: bool) -> Option<Vec<(String, String)>> {
    const COEFFS: [&str; 4] = ["LINE_NUM_COEFF", "LINE_DEN_COEFF", "SAMP_NUM_COEFF", "SAMP_DEN_COEFF"];
    let mut out: Vec<(String, String)> = Vec::new();
    if rpb {
        let names = [
            ("errBias", "ERR_BIAS"),
            ("errRand", "ERR_RAND"),
            ("lineOffset", "LINE_OFF"),
            ("sampOffset", "SAMP_OFF"),
            ("latOffset", "LAT_OFF"),
            ("longOffset", "LONG_OFF"),
            ("heightOffset", "HEIGHT_OFF"),
            ("lineScale", "LINE_SCALE"),
            ("sampScale", "SAMP_SCALE"),
            ("latScale", "LAT_SCALE"),
            ("longScale", "LONG_SCALE"),
            ("heightScale", "HEIGHT_SCALE"),
            ("lineNumCoef", "LINE_NUM_COEFF"),
            ("lineDenCoef", "LINE_DEN_COEFF"),
            ("sampNumCoef", "SAMP_NUM_COEFF"),
            ("sampDenCoef", "SAMP_DEN_COEFF"),
        ];
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        for (src, dst) in names {
            let Some(i) = flat.find(&format!("{src} =")) else { continue };
            let rest = flat[i + src.len() + 2..].trim_start();
            let value = rest.split(';').next().unwrap_or("").trim().trim_matches(|c| c == '(' || c == ')');
            let vals: Vec<&str> = value.split(',').map(str::trim).filter(|v| !v.is_empty()).collect();
            out.push((dst.to_string(), vals.join(" ")));
        }
    } else {
        let mut coeffs: Vec<std::collections::BTreeMap<usize, String>> = vec![Default::default(); 4];
        for line in text.lines() {
            let Some((key, value)) = line.split_once(':') else { continue };
            let (key, value) = (key.trim(), value.split_whitespace().next().unwrap_or(""));
            let mut placed = false;
            for (c, map) in COEFFS.iter().zip(coeffs.iter_mut()) {
                if let Some(suffix) = key.strip_prefix(c).and_then(|s| s.strip_prefix('_'))
                    && !suffix.is_empty()
                    && suffix.bytes().all(|b| b.is_ascii_digit())
                    && let Ok(i) = suffix.parse::<usize>()
                {
                    map.insert(i, value.to_string());
                    placed = true;
                    break;
                }
            }
            if !placed {
                out.push((key.to_string(), value.to_string()));
            }
        }
        for (c, map) in COEFFS.iter().zip(coeffs) {
            if map.keys().copied().eq(1..=20) {
                out.push((c.to_string(), map.into_values().collect::<Vec<_>>().join(" ")));
            }
        }
    }
    let needed = ["LINE_OFF", "SAMP_OFF", "LAT_OFF", "LONG_OFF", "HEIGHT_OFF"].into_iter().chain(COEFFS);
    let has = |k: &str| out.iter().any(|(key, _)| key == k);
    needed.into_iter().all(has).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RPCs of a 100 x 100 image over 0.2 degrees, with a parallax along the samples.
    fn rpc() -> Rpc {
        let mut items: Vec<(String, String)> = [
            ("LINE_OFF", "50"),
            ("SAMP_OFF", "50"),
            ("LAT_OFF", "45"),
            ("LONG_OFF", "10"),
            ("HEIGHT_OFF", "100"),
            ("LINE_SCALE", "50"),
            ("SAMP_SCALE", "50"),
            ("LAT_SCALE", "0.1"),
            ("LONG_SCALE", "0.1"),
            ("HEIGHT_SCALE", "500"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let mut line = [0.0; 20];
        line[2] = -1.0;
        line[4] = 0.01;
        let mut samp = [0.0; 20];
        samp[1] = 1.0;
        samp[3] = 0.05;
        samp[7] = 0.02;
        let mut den = [0.0; 20];
        den[0] = 1.0;
        den[1] = 0.001;
        let s = |c: &[f64; 20]| c.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(" ");
        items.push(("LINE_NUM_COEFF".into(), s(&line)));
        items.push(("LINE_DEN_COEFF".into(), s(&den)));
        items.push(("SAMP_NUM_COEFF".into(), s(&samp)));
        items.push(("SAMP_DEN_COEFF".into(), s(&den)));
        Rpc::from_metadata(&items).unwrap()
    }

    #[test]
    fn forward_and_inverse_agree() {
        let r = rpc();
        for height in [None, Some(250.0)] {
            let t = RpcTransformer::new(&r, &RpcOptions { height, ..Default::default() }).unwrap();
            for (p, l) in [(0.0, 0.0), (37.5, 81.25), (100.0, 100.0), (50.0, 50.0)] {
                let (lon, lat) = t.to_geo(p, l, 0.0).unwrap();
                let (bp, bl) = t.to_pixel(lon, lat, 0.0).unwrap();
                assert!((bp - p).abs() < 0.1 && (bl - l).abs() < 0.1, "{p} {l} -> {bp} {bl}");
            }
        }
        // The height moves the samples (the parallax term).
        let a = RpcTransformer::new(&r, &RpcOptions::default()).unwrap().to_pixel(10.0, 45.0, 0.0).unwrap();
        let b =
            RpcTransformer::new(&r, &RpcOptions { height: Some(500.0), ..Default::default() }).unwrap().to_pixel(10.0, 45.0, 0.0).unwrap();
        assert!((b.0 - a.0 - 50.0 * 0.05 * (500.0 / 500.0)).abs() < 1e-9 && (b.1 - a.1).abs() < 1e-12);
    }

    /// Heights from a DEM: a row of points at one latitude (sampled from one row of the DEM)
    /// gives what the points give one by one; points off the DEM fail unless a missing value is
    /// given; the inverse finds the points back.
    #[test]
    fn dem_heights() {
        let dir = tempfile::tempdir().unwrap();
        let dem = dir.path().join("dem.tif");
        let (cols, rows) = (300usize, 260usize);
        let cfg = wbraster::RasterConfig {
            cols,
            rows,
            bands: 1,
            x_min: 9.85,
            y_min: 44.88,
            cell_size: 0.001,
            cell_size_y: Some(0.001),
            nodata: -9999.0,
            data_type: crate::DataType::F32,
            crs: wbraster::CrsInfo::from_epsg(4326),
            metadata: vec![],
        };
        let data = (0..rows * cols).map(|i| 300.0 + ((i % cols) as f64 * 0.37).sin() * 200.0 + (i / cols) as f64).collect();
        let ds = crate::RasterDataset::new(wbraster::Raster::from_data(cfg, data).unwrap(), true);
        crate::write_raster_dataset(&ds, dem.to_str().unwrap(), &Default::default()).unwrap();
        let r = rpc();
        for interpolation in [DemInterpolation::Bilinear, DemInterpolation::Cubic, DemInterpolation::Near] {
            let o = RpcOptions { dem: Some(dem.to_str().unwrap().into()), dem_interpolation: interpolation, ..Default::default() };
            let t = RpcTransformer::new(&r, &o).unwrap();
            let mut xs: Vec<f64> = (0..30).map(|i| 9.95 + i as f64 * 0.003).collect();
            let mut ys = vec![45.02; 30];
            let one: Vec<_> = xs.iter().map(|x| t.to_pixel(*x, 45.02, 0.0).unwrap()).collect();
            let mut ok = vec![true; 30];
            t.to_pixel_many(&mut xs, &mut ys, None, &mut ok);
            for i in 0..30 {
                assert!(ok[i] && (xs[i] - one[i].0).abs() < 1e-9 && (ys[i] - one[i].1).abs() < 1e-9, "{interpolation:?} {i}");
            }
            let (lon, lat) = t.to_geo(40.0, 60.0, 0.0).unwrap();
            let (p, l) = t.to_pixel(lon, lat, 0.0).unwrap();
            assert!((p - 40.0).abs() < 0.1 && (l - 60.0).abs() < 0.1);
            assert!(t.to_pixel(10.2, 45.2, 0.0).is_none());
        }
        let o = RpcOptions { dem: Some(dem.to_str().unwrap().into()), dem_missing_value: Some(0.0), ..Default::default() };
        let t = RpcTransformer::new(&r, &o).unwrap();
        let flat = RpcTransformer::new(&r, &RpcOptions::default()).unwrap();
        assert_eq!(t.to_pixel(10.2, 45.2, 0.0), flat.to_pixel(10.2, 45.2, 0.0));
    }

    #[test]
    fn tiff_tag_round_trip_and_options() {
        let r = rpc();
        let back = Rpc::from_tiff_tag(&r.to_tiff_tag()).unwrap();
        assert_eq!(back.line_num_coeff, r.line_num_coeff);
        assert_eq!(round15(0.1 + 0.2), 0.3);
        let o = RpcOptions::from_options(&[
            ("RPC_HEIGHT".into(), "12.5".into()),
            ("rpc_deminterpolation".into(), "CUBIC".into()),
            ("RPC_MAX_ITERATIONS".into(), "30".into()),
            ("SRC_METHOD".into(), "RPC".into()),
        ])
        .unwrap();
        assert_eq!((o.height, o.dem_interpolation, o.max_iterations), (Some(12.5), DemInterpolation::Cubic, 30));
        assert!(RpcOptions::from_options(&[("RPC_FOOTPRINT".into(), "POLYGON EMPTY".into())]).is_err());
        assert_eq!(atof("1.5e3abc"), 1500.0);
        assert_eq!(atof("x"), 0.0);
        assert_eq!(atof("1.5\u{b1}2"), 1.5);
    }

    #[test]
    fn rpc_sidecars() {
        let mut txt = String::from("LINE_OFF: 50 pixels\nSAMP_OFF: 50\nLAT_OFF: 45\nLONG_OFF: 10\nHEIGHT_OFF: 0\n");
        for i in 1..=20 {
            for c in COEFF_NAMES {
                txt.push_str(&format!("{c}_{i}: {}\n", if i == 1 { 1 } else { 0 }));
            }
        }
        let items = parse_rpc_file(&txt, false).unwrap();
        let r = Rpc::from_metadata(&items).unwrap();
        assert_eq!((r.line_off, r.line_num_coeff[0], r.line_num_coeff[1]), (50.0, 1.0, 0.0));
        let rpb = "satId = \"X\";\nBEGIN_GROUP = IMAGE\n lineOffset = 50;\n sampOffset = 50;\n latOffset = 45;\n longOffset = 10;\n heightOffset = 0;\n lineNumCoef = (\n 1.0,\n 2.0);\n lineDenCoef = (1, 0);\n sampNumCoef = (1);\n sampDenCoef = (1);\nEND_GROUP = IMAGE";
        let r = Rpc::from_metadata(&parse_rpc_file(rpb, true).unwrap()).unwrap();
        assert_eq!((r.long_off, r.line_num_coeff[1]), (10.0, 2.0));
    }

    const COEFF_NAMES: [&str; 4] = ["LINE_NUM_COEFF", "LINE_DEN_COEFF", "SAMP_NUM_COEFF", "SAMP_DEN_COEFF"];
}
