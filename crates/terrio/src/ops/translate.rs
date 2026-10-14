//! Subsetting, resizing, rescaling and type conversion: an equivalent of `gdal_translate`.
//!
//! As in GDAL, the output is a window of the source (`srcwin`, or `projwin` in georeferenced
//! coordinates) read into an output of the same or another size, band by band, through the
//! same windowing as VRT sources. Values are then optionally rescaled and converted to the output
//! type with GDAL's rounding and clamping.

use super::Ct as Transformer;
use super::vrt::SourceRead;
use super::{Grid, Resampling, band_f64, build_raster, convert_value, is_nodata, type_range};
use crate::crs::SpatialRef;
use crate::{Error, RasterDataset, Result};
use wbraster::DataType;

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
    let src_nodata = ds.nodata();
    let out_nodata = match o.assign_nodata {
        Some(v) => v,
        None => src_nodata,
    };

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
    let mut out = Vec::with_capacity(bands.len());
    for &b in &bands {
        let values = band_f64(r, b - 1);
        let valid: Option<Vec<bool>> = src_nodata.map(|nd| super::par_map(&values, |v| !is_nodata(*v, nd)));
        let mut buf = vec![out_nodata.unwrap_or(0.0); n];
        let read = SourceRead {
            values: &values,
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
                None => min_max(&values, valid.as_deref()).unwrap_or((0.0, 0.0)),
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
        read.read(ow, oh, &mut buf, |v| match scaling {
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
        });
        out.push(buf);
    }
    let grid = Grid { width: ow, height: oh, geo_transform: gt };
    let srs = o.assign_srs.clone().unwrap_or_else(|| ds.crs());
    let mut res = build_raster(grid, out, out_type, out_nodata, &srs, convert_value)?;
    res.driver = ds.driver;
    Ok(res)
}

/// The smallest and largest valid values.
fn min_max(values: &[f64], valid: Option<&[bool]>) -> Option<(f64, f64)> {
    let mut it = values.iter().enumerate().filter(|(i, v)| !v.is_nan() && valid.is_none_or(|m| m[*i])).map(|(_, v)| *v);
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
}
