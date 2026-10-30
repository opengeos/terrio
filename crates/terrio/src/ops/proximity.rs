// SPDX-FileCopyrightText: Copyright (c) 2008, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2009-2010, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.13.3:
//     alg/gdalproximity.cpp

//! Distance to target pixels: an equivalent of `gdal_proximity.py` (GDAL's
//! `GDALComputeProximity()`).
//!
//! As in GDAL, the band is read as 32-bit integers; target pixels are the non-zero ones or those
//! with given values; distances propagate in two passes (down then up, each sweeping left and
//! right) from the nearest target found so far, and are rounded through the output type between
//! the passes when that type is a signed integer, as GDAL stores them in the output band.

use super::{Grid, build_raster};
use crate::{Error, RasterDataset, Result};
use wbraster::DataType;

/// Options for [`proximity`], named after `gdal_proximity.py`'s.
#[derive(Debug, Clone)]
pub struct ProximityOptions {
    /// The source band, from 1 (`-srcband`).
    pub band: usize,
    /// The target pixel values (`-values`). Non-zero pixels by default.
    pub values: Vec<i32>,
    /// Distances in georeferenced units (the pixel width) rather than pixels (`-distunits GEO`).
    pub geo_units: bool,
    /// The largest distance computed; pixels farther get `nodata` (`-maxdist`).
    pub max_distance: Option<f64>,
    /// The value of pixels beyond `max_distance` or without target (`-nodata`); 65535 by
    /// default. It is not declared as the output's nodata value, as in GDAL.
    pub nodata: Option<f64>,
    /// Keep the source's nodata pixels as `nodata` (`-use_input_nodata YES`).
    pub use_input_nodata: bool,
    /// Give pixels within `max_distance` this value rather than their distance
    /// (`-fixed-buf-val`).
    pub fixed_value: Option<f64>,
    /// The output type (`-ot`), Float32 by default.
    pub output_type: DataType,
}

impl Default for ProximityOptions {
    fn default() -> Self {
        Self {
            band: 1,
            values: Vec::new(),
            geo_units: false,
            max_distance: None,
            nodata: None,
            use_input_nodata: false,
            fixed_value: None,
            output_type: DataType::F32,
        }
    }
}

/// Parses `gdal_proximity.py`'s `-values` list, `1,2,3`.
pub fn parse_values(s: &str) -> Vec<i32> {
    s.split(',').filter(|t| !t.is_empty()).map(super::dem::atoi).collect()
}

/// GDAL's conversion of a value to an integer type (`GDALCopyWords`): NaN becomes 0, others are
/// rounded half away from zero and clamped.
pub(crate) fn float_to_int(v: f64, dt: DataType) -> f64 {
    if v.is_nan() {
        return 0.0;
    }
    let (lo, hi) = super::type_range(dt);
    let unsigned = matches!(dt, DataType::U8 | DataType::U16 | DataType::U32 | DataType::U64);
    let r = if unsigned || v > 0.0 { v + 0.5 } else { v - 0.5 };
    r.clamp(lo, hi).trunc()
}

/// A `Float32` value as stored in a band of type `dt`, rounded in single precision as GDAL does.
fn store(v: f32, dt: DataType) -> f64 {
    match dt {
        DataType::F32 | DataType::F64 => v as f64,
        _ if v.is_nan() => 0.0,
        _ => {
            let (lo, hi) = super::type_range(dt);
            let unsigned = matches!(dt, DataType::U8 | DataType::U16 | DataType::U32 | DataType::U64);
            let r = if unsigned || v > 0.0 { v + 0.5 } else { v - 0.5 };
            (r as f64).clamp(lo, hi).trunc()
        }
    }
}

/// Reads a band as GDAL reads it into integers of type `dt` (`Float32` values are rounded in
/// single precision).
pub(crate) fn read_ints(ds: &RasterDataset, band: usize, dt: DataType) -> Vec<f64> {
    let single = ds.raster.data_type == DataType::F32;
    super::band_f64(&ds.raster, band - 1).into_iter().map(|v| if single { store(v as f32, dt) } else { float_to_int(v, dt) }).collect()
}

#[allow(clippy::too_many_arguments)]
fn process_line(
    src: &[i32],
    near_x: &mut [i32],
    near_y: &mut [i32],
    forward: bool,
    line: i32,
    max_dist: f64,
    prox: &mut [f32],
    src_nodata: Option<f64>,
    targets: &[i32],
) {
    let n = src.len() as i32;
    let (start, end, step) = if forward { (0, n, 1) } else { (n - 1, -1, -1) };
    let sq = |x1: f64, x2: f64, y1: f64, y2: f64| (x1 - x2) * (x1 - x2) + (y1 - y2) * (y1 - y2);
    let mut i = start;
    while i != end {
        let p = i as usize;
        let is_target = if targets.is_empty() { src[p] != 0 } else { targets.contains(&src[p]) };
        if is_target {
            prox[p] = 0.0;
            near_x[p] = i;
            near_y[p] = line;
            i += step;
            continue;
        }
        let m = max_dist.max(n as f64);
        let mut near = m * m * 2.0;
        if near_x[p] != -1 {
            let d = sq(near_x[p] as f64, i as f64, near_y[p] as f64, line as f64);
            if d < near {
                near = d;
            } else {
                near_x[p] = -1;
                near_y[p] = -1;
            }
        }
        let last = i - step;
        if i != start && near_x[last as usize] != -1 {
            let l = last as usize;
            let d = sq(near_x[l] as f64, i as f64, near_y[l] as f64, line as f64);
            if d < near {
                near = d;
                near_x[p] = near_x[l];
                near_y[p] = near_y[l];
            }
        }
        let tr = i + step;
        if tr != end && near_x[tr as usize] != -1 {
            let t = tr as usize;
            let d = sq(near_x[t] as f64, i as f64, near_y[t] as f64, line as f64);
            if d < near {
                near = d;
                near_x[p] = near_x[t];
                near_y[p] = near_y[t];
            }
        }
        if near_x[p] != -1
            && src_nodata.is_none_or(|nd| src[p] as f64 != nd)
            && near <= max_dist * max_dist
            && (prox[p] < 0.0 || near < (prox[p] as f64) * (prox[p] as f64))
        {
            prox[p] = near.sqrt() as f32;
        }
        i += step;
    }
}

/// Computes the distance of every pixel to the nearest target pixel, as `gdal_proximity.py`
/// does. The output has the source's size and georeferencing, and no nodata value.
pub fn proximity(ds: &RasterDataset, o: &ProximityOptions) -> Result<RasterDataset> {
    if o.band == 0 || o.band > ds.raster.bands {
        return Err(Error::InvalidArgument(format!("the raster has no band {}", o.band)));
    }
    let (w, h) = (ds.raster.cols, ds.raster.rows);
    let dist_mult = if o.geo_units { ds.geo_transform()[1].abs() } else { 1.0 };
    let max_dist = match o.max_distance {
        Some(d) => d / dist_mult,
        None => (w + h) as f64,
    };
    let src_nodata = if o.use_input_nodata { ds.nodata() } else { None };
    let fill = o.nodata.unwrap_or(65535.0) as f32;
    // GDAL keeps the first pass in the output band when its type is signed, in a temporary
    // Float32 band otherwise.
    let work_type = match o.output_type {
        DataType::U8 | DataType::U16 | DataType::U32 => DataType::F32,
        t => t,
    };
    let src: Vec<i32> = read_ints(ds, o.band, DataType::I32).into_iter().map(|v| v as i32).collect();
    let mut first = vec![0f32; w * h];
    let (mut near_x, mut near_y) = (vec![-1i32; w], vec![-1i32; w]);
    let mut prox = vec![0f32; w];
    for line in 0..h {
        let s = &src[line * w..(line + 1) * w];
        prox.iter_mut().for_each(|p| *p = -1.0);
        process_line(s, &mut near_x, &mut near_y, true, line as i32, max_dist, &mut prox, src_nodata, &o.values);
        process_line(s, &mut near_x, &mut near_y, false, line as i32, max_dist, &mut prox, src_nodata, &o.values);
        for (f, p) in first[line * w..(line + 1) * w].iter_mut().zip(&prox) {
            *f = store(*p, work_type) as f32;
        }
    }
    near_x.iter_mut().for_each(|v| *v = -1);
    near_y.iter_mut().for_each(|v| *v = -1);
    let mut out = vec![0f64; w * h];
    for line in (0..h).rev() {
        let s = &src[line * w..(line + 1) * w];
        prox.copy_from_slice(&first[line * w..(line + 1) * w]);
        process_line(s, &mut near_x, &mut near_y, false, line as i32, max_dist, &mut prox, src_nodata, &o.values);
        process_line(s, &mut near_x, &mut near_y, true, line as i32, max_dist, &mut prox, src_nodata, &o.values);
        for (dst, p) in out[line * w..(line + 1) * w].iter_mut().zip(&prox) {
            let v = if *p < 0.0 {
                fill
            } else if *p > 0.0 {
                match o.fixed_value {
                    Some(f) => f as f32,
                    None => *p * dist_mult as f32,
                }
            } else {
                *p
            };
            *dst = store(v, o.output_type);
        }
    }
    build_raster(Grid::of(&ds.raster), vec![out], o.output_type, None, &ds.crs(), |v, _| v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ds(values: Vec<f64>, w: usize, h: usize) -> RasterDataset {
        let grid = Grid { width: w, height: h, geo_transform: [0.0, 10.0, 0.0, h as f64 * 10.0, 0.0, -10.0] };
        build_raster(grid, vec![values], DataType::U8, None, &crate::crs::SpatialRef::default(), super::super::convert_value).unwrap()
    }

    #[test]
    fn distances() {
        let mut v = vec![0.0; 25];
        v[12] = 1.0;
        let src = ds(v, 5, 5);
        let out = proximity(&src, &ProximityOptions::default()).unwrap();
        assert_eq!(out.raster.get(0, 2, 2), 0.0);
        assert_eq!(out.raster.get(0, 2, 4), 2.0);
        assert!((out.raster.get(0, 0, 0) - 8f64.sqrt()).abs() < 1e-6);
        assert_eq!(out.nodata(), None);
        let geo =
            proximity(&src, &ProximityOptions { geo_units: true, max_distance: Some(15.0), nodata: Some(-1.0), ..Default::default() })
                .unwrap();
        assert_eq!(geo.raster.get(0, 2, 3), 10.0);
        assert_eq!(geo.raster.get(0, 0, 0), -1.0);
        let fixed = proximity(&src, &ProximityOptions { fixed_value: Some(5.0), output_type: DataType::U8, ..Default::default() }).unwrap();
        assert_eq!(fixed.raster.get(0, 0, 0), 5.0);
        assert_eq!(fixed.raster.get(0, 2, 2), 0.0);
        let none = proximity(&src, &ProximityOptions { values: parse_values("7,8"), ..Default::default() }).unwrap();
        assert_eq!(none.raster.get(0, 2, 2), 65535.0);
    }
}
