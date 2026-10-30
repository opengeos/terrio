// SPDX-FileCopyrightText: Copyright (c) 1998, 2002, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2007-2015, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2015, Faza Mahamood
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.13.3:
//     apps/gdal_translate_lib.cpp

//! Subsetting, resizing, rescaling and type conversion: an equivalent of `gdal_translate`.
//!
//! As in GDAL, the output is a window of the source (`srcwin`, or `projwin` in georeferenced
//! coordinates) read into an output of the same or another size, band by band, through the
//! same windowing as VRT sources. Values are then optionally rescaled and converted to the output
//! type with GDAL's rounding and clamping.

use super::Ct as Transformer;
use super::vrt::SourceRead;
use super::{Grid, Resampling, convert_value, type_range};
use crate::crs::SpatialRef;
use crate::{Error, RasterDataset, Result};
use wbraster::DataType;
use wbraster::raster::RasterData;

/// An output dimension: pixels or a percentage of the source window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Size {
    /// A number of pixels; 0 keeps the aspect ratio of the other dimension.
    Pixels(usize),
    /// A percentage of the source window.
    Percent(f64),
}

impl Size {
    /// Parses `512` or `50%`.
    pub fn parse(s: &str) -> Result<Self> {
        let bad = || Error::InvalidArgument(format!("invalid size '{s}'"));
        match s.strip_suffix('%') {
            Some(p) => Ok(Self::Percent(p.trim().parse().map_err(|_| bad())?)),
            None => Ok(Self::Pixels(s.trim().parse().map_err(|_| bad())?)),
        }
    }

    fn is_zero(&self) -> bool {
        matches!(self, Self::Pixels(0)) || matches!(self, Self::Percent(p) if *p == 0.0)
    }
}

/// Linear rescaling: `[src_min, src_max]` maps to `[dst_min, dst_max]`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Scale {
    /// The source range; the band's minimum and maximum when `None`.
    pub src: Option<(f64, f64)>,
    /// The output range; the output type's range (0–1 for floating-point types) when `None`.
    pub dst: Option<(f64, f64)>,
}

/// Options for [`translate`], named after `gdal_translate`'s.
#[derive(Debug, Clone, Default)]
pub struct TranslateOptions {
    /// The bands to keep, from 1 (`-b`). All when empty.
    pub bands: Vec<usize>,
    /// The output data type (`-ot`).
    pub output_type: Option<DataType>,
    /// The source window `(x_off, y_off, x_size, y_size)` in pixels (`-srcwin`).
    pub src_win: Option<(f64, f64, f64, f64)>,
    /// The source window `(ulx, uly, lrx, lry)` in georeferenced coordinates (`-projwin`).
    pub proj_win: Option<(f64, f64, f64, f64)>,
    /// The CRS of [`TranslateOptions::proj_win`] (`-projwin_srs`).
    pub proj_win_srs: Option<SpatialRef>,
    /// The output size (`-outsize`).
    pub out_size: Option<(Size, Size)>,
    /// The output resolution `(x, y)` (`-tr`).
    pub resolution: Option<(f64, f64)>,
    /// The resampling method when the size changes (`-r`). Nearest by default.
    pub resampling: Option<Resampling>,
    /// Linear rescaling (`-scale`), applied to all bands.
    pub scale: Option<Scale>,
    /// Apply a power function after scaling: `dst_min + (dst_max - dst_min) * t^exponent`, with
    /// `t` the clipped position in the source range (`-exponent`).
    pub exponent: Option<f64>,
    /// The nodata value to assign (`-a_nodata`): `Some(None)` removes it.
    pub assign_nodata: Option<Option<f64>>,
    /// The CRS to assign (`-a_srs`).
    pub assign_srs: Option<SpatialRef>,
    /// The bounds to assign `(ulx, uly, lrx, lry)` (`-a_ullr`).
    pub assign_ullr: Option<(f64, f64, f64, f64)>,
}

/// Copies a window of a raster to a new one, optionally resized, rescaled, retyped and with new
/// georeferencing, as `gdal_translate` does.
pub fn translate(ds: &RasterDataset, o: &TranslateOptions) -> Result<RasterDataset> {
    let r = &ds.raster;
    let (w, h) = (r.cols as f64, r.rows as f64);
    let src_gt = ds.geo_transform();
    let nearest = o.resampling.is_none_or(|m| m == Resampling::Nearest);

    // The source window.
    let mut win = o.src_win.unwrap_or((0.0, 0.0, w, h));
    if let Some((mut ulx, mut uly, mut lrx, mut lry)) = o.proj_win {
        if let Some(srs) = &o.proj_win_srs {
            let ds_srs = ds.crs();
            if !ds_srs.is_unknown() && !srs.same_as(&ds_srs) {
                let b = super::transform_bounds(&Transformer::new(srs, &ds_srs)?, (ulx, lry, lrx, uly), 21)?;
                (ulx, lry, lrx, uly) = b;
            }
        }
        let mut xoff = (ulx - src_gt[0]) / src_gt[1];
        let mut yoff = (uly - src_gt[3]) / src_gt[5];
        if nearest {
            xoff = (xoff + 0.001).floor();
            yoff = (yoff + 0.001).floor();
            ulx = xoff * src_gt[1] + src_gt[0];
            uly = yoff * src_gt[5] + src_gt[3];
        }
        let mut xs = (lrx - ulx) / src_gt[1];
        let mut ys = (lry - uly) / src_gt[5];
        if nearest {
            xs = (xs - 0.001).ceil();
            ys = (ys - 0.001).ceil();
        }
        win = (xoff, yoff, xs, ys);
    }
    if win.2 <= 0.0 || win.3 <= 0.0 {
        return Err(Error::InvalidArgument(format!("the source window {win:?} is empty")));
    }
    if win.0 + win.2 <= 0.0 || win.1 + win.3 <= 0.0 || win.0 >= w || win.1 >= h {
        return Err(Error::InvalidArgument(format!("the source window {win:?} falls completely outside the raster")));
    }

    // The output size.
    let (ow, oh) = if let Some((xr, yr)) = o.resolution {
        ((win.2 / xr * src_gt[1] + 0.5) as usize, (win.3 / yr * src_gt[5].abs() + 0.5) as usize)
    } else if let Some((sx, sy)) = o.out_size {
        let dim = |s: Size, len: f64| match s {
            Size::Pixels(p) => p,
            Size::Percent(p) => (p / 100.0 * len) as usize,
        };
        let (mut ow, mut oh) = (dim(sx, win.2), dim(sy, win.3));
        if sx.is_zero() {
            ow = (oh as f64 * win.2 / win.3 + 0.5) as usize;
        } else if sy.is_zero() {
            oh = (ow as f64 * win.3 / win.2 + 0.5) as usize;
        }
        (ow, oh)
    } else {
        ((win.2 - 0.001).ceil() as usize, (win.3 - 0.001).ceil() as usize)
    };
    if ow == 0 || oh == 0 {
        return Err(Error::InvalidArgument(format!("the output size {ow}x{oh} is empty")));
    }

    // The output geotransform.
    let gt = if let Some((ulx, uly, lrx, lry)) = o.assign_ullr {
        [ulx, (lrx - ulx) / ow as f64, 0.0, uly, 0.0, (lry - uly) / oh as f64]
    } else {
        let mut g = src_gt;
        g[0] += win.0 * g[1];
        g[3] += win.1 * g[5];
        g[1] *= win.2 / ow as f64;
        g[5] *= win.3 / oh as f64;
        if let Some((xr, yr)) = o.resolution {
            g[1] = xr;
            g[5] = if g[5] > 0.0 { yr } else { -yr };
        }
        g
    };

    let bands: Vec<usize> = if o.bands.is_empty() { (1..=r.bands).collect() } else { o.bands.clone() };
    if let Some(b) = bands.iter().find(|b| **b == 0 || **b > r.bands) {
        return Err(Error::InvalidArgument(format!("band {b} does not exist (the raster has {})", r.bands)));
    }
    let out_type = o.output_type.unwrap_or(r.data_type);
    let out_nodata: Vec<Option<f64>> = bands.iter().map(|b| o.assign_nodata.unwrap_or_else(|| ds.band_nodata(*b))).collect();

    // The destination window, after clamping the source window to the raster (FixSrcDstWindow).
    let (mut s, mut d) = ([win.0, win.1, win.2, win.3], [0.0, 0.0, ow as f64, oh as f64]);
    if o.resolution.is_some() && !nearest {
        d[2] = win.2 * src_gt[1] / gt[1];
        d[3] = win.3 * (src_gt[5] / gt[5]).abs();
    }
    if !fix_windows(&mut s, &mut d, w, h) {
        return Err(Error::InvalidArgument("the source window does not intersect the raster".into()));
    }

    let complex = o.scale.is_some() || o.exponent.is_some();
    let n = ow * oh;
    let mut out = RasterData::new_filled(out_type, n * bands.len(), 0.0);
    for (k, &b) in bands.iter().enumerate() {
        let src_nodata = ds.band_nodata(b);
        let values = super::BandView::of(r, b - 1);
        let valid: Option<Vec<bool>> = src_nodata.map(|nd| values.valid(nd));
        let read = SourceRead {
            values,
            valid: if complex || !nearest { valid.as_deref() } else { None },
            width: r.cols,
            height: r.rows,
            src_type: r.data_type,
            src_rect: s,
            dst_rect: d,
            resampling: o.resampling.unwrap_or_default(),
            working_float: complex,
            invalid_value: if complex { None } else { src_nodata },
        };
        // Scaling parameters, as gdal_translate derives them.
        let scaling = if complex {
            let sc = o.scale.unwrap_or_default();
            let (smin, mut smax) = match sc.src {
                Some(r) => r,
                None => min_max(values, valid.as_deref()).unwrap_or((0.0, 0.0)),
            };
            if smax == smin {
                smax += 0.1;
            }
            let (dmin, dmax) = sc.dst.unwrap_or_else(|| match out_type {
                DataType::F32 | DataType::F64 => (0.0, 1.0),
                DataType::U64 => (0.0, 18446744073709549568.0),
                DataType::I64 => (-9223372036854774784.0, 9223372036854773760.0),
                t => type_range(t),
            });
            Some((smin, smax, dmin, dmax))
        } else {
            None
        };
        let float_working = matches!(r.data_type, DataType::F64 | DataType::I32 | DataType::U32 | DataType::I64 | DataType::U64);
        let fill = convert_value(out_nodata[k].unwrap_or(0.0), out_type);
        let scaled = |v: f64| match scaling {
            None => v,
            Some((smin, smax, dmin, dmax)) => {
                let s = match o.exponent {
                    Some(e) if e != 1.0 => {
                        let t = ((v - smin) / (smax - smin)).clamp(0.0, 1.0);
                        (dmax - dmin) * t.powf(e) + dmin
                    }
                    _ => {
                        let ratio = (dmax - dmin) / (smax - smin);
                        v * ratio + (dmin - smin * ratio)
                    }
                };
                if float_working { s } else { s as f32 as f64 }
            }
        };
        // Written in the output's type, converted as they are stored.
        crate::pixel::with_data!(&mut out, v => {
            let band = &mut v[k * n..(k + 1) * n];
            band.iter_mut().for_each(|x| *x = fill as _);
            read.read(ow, oh, band, |x| convert_value(scaled(x), out_type) as _);
        });
    }
    let grid = Grid { width: ow, height: oh, geo_transform: gt };
    let srs = o.assign_srs.clone().unwrap_or_else(|| ds.crs());
    let mut res = super::build_raster_data(grid, out, bands.len(), out_nodata.first().copied().flatten(), &srs)?;
    res.raster.metadata = ds.raster.metadata.iter().filter(|(k, _)| k == "AREA_OR_POINT").cloned().collect();
    res.band_metadata = bands.iter().map(|&b| ds.band_metadata.get(b - 1).cloned().unwrap_or_default()).collect();
    if o.assign_ullr.is_none() {
        res.gcps = ds
            .gcps
            .iter()
            .cloned()
            .map(|mut g| {
                g.pixel = (g.pixel - win.0) * ow as f64 / win.2;
                g.line = (g.line - win.1) * oh as f64 / win.3;
                g
            })
            .collect();
    }
    res.band_nodata = out_nodata;
    res.driver = ds.driver;
    Ok(res)
}

/// The smallest and largest valid values.
fn min_max(values: super::BandView, valid: Option<&[bool]>) -> Option<(f64, f64)> {
    let mut it = (0..values.len()).map(|i| (i, values.get(i))).filter(|(i, v)| !v.is_nan() && valid.is_none_or(|m| m[*i])).map(|(_, v)| v);
    let first = it.next()?;
    Some(it.fold((first, first), |(a, b), v| (a.min(v), b.max(v))))
}

/// GDAL's `FixSrcDstWindow()`: clamps the source window to the raster and shrinks the destination
/// window in proportion.
fn fix_windows(s: &mut [f64; 4], d: &mut [f64; 4], w: f64, h: f64) -> bool {
    let (mut modified_x, mut modified_y) = (false, false);
    let (mut sxo, mut syo, mut sxs, mut sys) = (s[0], s[1], s[2], s[3]);
    if sxo < 0.0 {
        sxs += sxo;
        sxo = 0.0;
        modified_x = true;
    }
    if syo < 0.0 {
        sys += syo;
        syo = 0.0;
        modified_y = true;
    }
    if sxo + sxs > w {
        sxs = w - sxo;
        modified_x = true;
    }
    if syo + sys > h {
        sys = h - syo;
        modified_y = true;
    }
    if !modified_x && !modified_y {
        return true;
    }
    if sxs <= 0.0 || sys <= 0.0 {
        return false;
    }
    let (dxo, dyo, dxs, dys) = (d[0], d[1], d[2], d[3]);
    if modified_x {
        let ratio = dxs / s[2];
        d[0] = dxo + (sxo - s[0]) * ratio;
        d[2] = sxs * ratio;
    }
    if modified_y {
        let ratio = dys / s[3];
        d[1] = dyo + (syo - s[1]) * ratio;
        d[3] = sys * ratio;
    }
    *s = [sxo, syo, sxs, sys];
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbraster::{Raster, RasterConfig};

    fn ds(cols: usize, rows: usize) -> RasterDataset {
        let cfg = RasterConfig {
            cols,
            rows,
            bands: 2,
            x_min: 100.0,
            y_min: 200.0 - rows as f64 * 10.0,
            cell_size: 10.0,
            cell_size_y: Some(10.0),
            nodata: -1.0,
            data_type: DataType::F32,
            crs: wbraster::CrsInfo::from_epsg(32610),
            metadata: vec![],
        };
        let n = cols * rows;
        let data = (0..2 * n).map(|i| (i % n) as f64 + (i / n) as f64 * 1000.0).collect();
        RasterDataset::new(Raster::from_data(cfg, data).unwrap(), false)
    }

    #[test]
    fn srcwin_and_band() {
        let d = ds(10, 8);
        let o = TranslateOptions { src_win: Some((2.0, 3.0, 4.0, 2.0)), bands: vec![2], ..Default::default() };
        let out = translate(&d, &o).unwrap();
        assert_eq!((out.raster.cols, out.raster.rows, out.raster.bands), (4, 2, 1));
        assert_eq!(out.geo_transform(), [120.0, 10.0, 0.0, 170.0, 0.0, -10.0]);
        assert_eq!(out.raster.data.to_f64_vec(), vec![1032.0, 1033.0, 1034.0, 1035.0, 1042.0, 1043.0, 1044.0, 1045.0]);
    }

    #[test]
    fn projwin_partially_outside() {
        let d = ds(10, 8);
        let o = TranslateOptions { proj_win: Some((80.0, 200.0, 120.0, 180.0)), bands: vec![1], ..Default::default() };
        let out = translate(&d, &o).unwrap();
        assert_eq!((out.raster.cols, out.raster.rows), (4, 2));
        // Two columns left of the raster are filled with 0 (no nodata).
        assert_eq!(out.raster.data.to_f64_vec(), vec![0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 10.0, 11.0]);
    }

    #[test]
    fn scale_and_type() {
        let d = ds(10, 8);
        let o = TranslateOptions {
            bands: vec![1],
            output_type: Some(DataType::U8),
            scale: Some(Scale { src: Some((0.0, 79.0)), dst: Some((0.0, 255.0)) }),
            ..Default::default()
        };
        let out = translate(&d, &o).unwrap();
        let v = out.raster.data.to_f64_vec();
        assert_eq!(v[0], 0.0);
        assert_eq!(v[79], 255.0);
        assert_eq!(v[1], (255.0f64 / 79.0).round());
    }

    #[test]
    fn outsize_percent() {
        let d = ds(10, 8);
        let o = TranslateOptions { out_size: Some((Size::Percent(50.0), Size::Percent(50.0))), bands: vec![1], ..Default::default() };
        let out = translate(&d, &o).unwrap();
        assert_eq!((out.raster.cols, out.raster.rows), (5, 4));
        assert_eq!(out.geo_transform()[1], 20.0);
        // Nearest: output pixel (0, 0) reads source (1, 1).
        assert_eq!(out.raster.data.to_f64_vec()[0], 11.0);
    }

    /// A float32 raster of uneven values, with -inf nodata pixels at the left corners if `nodata`.
    fn uneven(cols: usize, rows: usize, nodata: bool) -> RasterDataset {
        let cfg = RasterConfig {
            cols,
            rows,
            bands: 1,
            x_min: 100.0,
            y_min: 200.0 - rows as f64 * 10.0,
            cell_size: 10.0,
            cell_size_y: Some(10.0),
            nodata: f64::NEG_INFINITY,
            data_type: DataType::F32,
            crs: wbraster::CrsInfo::default(),
            metadata: vec![],
        };
        let mut data: Vec<f64> = (0..cols * rows).map(|i| (((i * 7919) % 1000) as f64 / 7.3) as f32 as f64).collect();
        if nodata {
            data[0] = f64::NEG_INFINITY;
            data[(rows - 1) * cols] = f64::NEG_INFINITY;
        }
        RasterDataset::new(Raster::from_data(cfg, data).unwrap(), nodata)
    }

    fn bits(d: &RasterDataset, o: &TranslateOptions) -> Vec<u32> {
        translate(d, o).unwrap().raster.data.to_f64_vec().iter().map(|v| (*v as f32).to_bits()).collect()
    }

    #[test]
    fn resamples_with_gdal_arithmetic() {
        // gdal_translate 3.12's outputs (float32 bits) for the same rasters.
        let size = |w, h| Some((Size::Pixels(w), Size::Pixels(h)));
        let average = |w, h| TranslateOptions { out_size: size(w, h), resampling: Some(Resampling::Average), ..Default::default() };
        // -outsize 4 3 -r average, with nodata: the sums over valid pixels.
        let expected = [
            0x427fa334, 0x42d30c40, 0x429a1539, 0x423b1ea6, 0x420ad453, 0x424acdc4, 0x42d01be0, 0x42a1d174, 0x428202b3, 0x421a4cc9,
            0x424c0abf, 0x42c60dc1,
        ];
        assert_eq!(bits(&uneven(11, 7, true), &average(4, 3)), expected);
        // Without nodata: weighted sums scaled by the pixel count.
        let expected = [
            0x4257cc43, 0x42d30c40, 0x429a1539, 0x423b1ea6, 0x420ad453, 0x424acdc4, 0x42d01be0, 0x42a1d174, 0x4289ac4b, 0x421a4cc9,
            0x424c0abf, 0x42c60dc1,
        ];
        assert_eq!(bits(&uneven(11, 7, false), &average(4, 3)), expected);
        // A factor of two: float32 sums of quarters, four pixels at a time, then one by one.
        let expected = [
            0x4281b9dd, 0x42de542a, 0x42b1f1f9, 0x42858fc8, 0x42325b2e, 0x41b32d96, 0x42192694, 0x42edabd6, 0x42c149a5, 0x4294e774,
            0x42510a86, 0x41f08c46, 0x413b65b2, 0x42fd0382, 0x42d0a150, 0x42a43f20, 0x426fb9dc, 0x4216f57b, 0x419b1189, 0x42835eaf,
            0x42dff8fc, 0x42b396cc, 0x4287349b, 0x4235a4d2,
        ];
        assert_eq!(bits(&uneven(12, 8, false), &average(6, 4)), expected);
        // -outsize 5 3 -r bilinear: an infinite nodata pixel in a kernel makes GDAL's result NaN.
        let o = TranslateOptions { out_size: size(5, 3), resampling: Some(Resampling::Bilinear), ..Default::default() };
        let out = translate(&uneven(11, 7, true), &o).unwrap().raster.data.to_f64_vec();
        let expected = [
            0xffc00000u32,
            0x42cebddf,
            0x42bc9c4e,
            0x428aadd7,
            0x423f3b9e,
            0x42156352,
            0x42273fe1,
            0x429f2136,
            0x42c75050,
            0x429f9bce,
            0xffc00000,
            0x42391b8e,
            0x41ff0784,
            0x42988216,
            0x42ccf694,
        ];
        for (v, e) in out.iter().zip(expected) {
            let e = f32::from_bits(e);
            assert!(if e.is_nan() { v.is_nan() } else { *v as f32 == e }, "{v} vs {e}");
        }
    }
}
