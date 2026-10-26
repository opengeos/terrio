//! Raster to XYZ text, as GDAL's `gdal2xyz.py`: a line per pixel with the X and Y of its center
//! and the values of the selected bands. The raster is read a strip of rows at a time and the
//! text written as it is made, so that neither is ever held whole.
//!
//! Lines and numbers are as `gdal2xyz.py` writes them: coordinates as `%.10g` when the raster
//! looks geographic (its origin and extent within 180 units) and `%.3f` otherwise; values as `%d`
//! when the first band is `Int32` or `UInt32`, else `%g`, converted to the first band's type.
//!
//! ```no_run
//! use terrio::ops::xyz::{XyzOptions, write_xyz};
//!
//! let mut dem = terrio::open_raster_lazy("dem.tif")?;
//! let mut out = std::io::BufWriter::new(std::fs::File::create("dem.xyz")?);
//! let lines = write_xyz(&mut dem, &mut out, &XyzOptions { skip_nodata: true, ..Default::default() })?;
//! println!("{lines} lines");
//! # Ok::<(), terrio::Error>(())
//! ```

use crate::{Error, LazyRaster, Result, Window, WindowOptions};
use std::io::Write;
use wbraster::DataType;

/// Options of [`write_xyz`], as `gdal2xyz.py`'s.
#[derive(Debug, Clone, PartialEq)]
pub struct XyzOptions {
    /// Bands to write, from 1 (`-b`). Empty means all bands (`-allbands`). The default is band 1,
    /// as `gdal2xyz.py`'s.
    pub bands: Vec<usize>,
    /// Write every `skip.0`-th column and every `skip.1`-th row (`skip`), from the window's first.
    pub skip: (usize, usize),
    /// The window `[x_offset, y_offset, width, height]` to write (`-srcwin`); the whole raster
    /// by default.
    pub srcwin: Option<[usize; 4]>,
    /// Leave out pixels whose values all equal the source nodata (`-skipnodata`).
    pub skip_nodata: bool,
    /// The source nodata value: one for all bands, or one per band (`-srcnodata`). By default,
    /// the bands' nodata values (when every band has one).
    pub src_nodata: Option<Vec<f64>>,
    /// Write these values instead of nodata pixels' (`-dstnodata`): one for all bands, or one per
    /// band. Ignored with `skip_nodata`, or without a source nodata.
    pub dst_nodata: Option<Vec<f64>>,
    /// The separator between values: a space, or a comma for `-csv`.
    pub delimiter: String,
}

impl Default for XyzOptions {
    fn default() -> Self {
        Self { bands: vec![1], skip: (1, 1), srcwin: None, skip_nodata: false, src_nodata: None, dst_nodata: None, delimiter: " ".into() }
    }
}

/// A value converted to a pixel type, as NumPy converts one into an array of it.
fn cast(v: f64, t: DataType) -> f64 {
    match t {
        DataType::F32 => v as f32 as f64,
        DataType::F64 => v,
        DataType::U8 => v as u8 as f64,
        DataType::I8 => v as i8 as f64,
        DataType::U16 => v as u16 as f64,
        DataType::I16 => v as i16 as f64,
        DataType::U32 => v as u32 as f64,
        DataType::I32 => v as i32 as f64,
        DataType::U64 => v as u64 as f64,
        DataType::I64 => v as i64 as f64,
    }
}

/// Per-band values from one (for all bands) or one per band.
fn per_band(values: &[f64], bands: usize, what: &str) -> Result<Vec<f64>> {
    match values.len() {
        1 => Ok(vec![values[0]; bands]),
        n if n == bands => Ok(values.to_vec()),
        n => Err(Error::InvalidArgument(format!("{what}: {n} values for {bands} bands (give one, or one per band)"))),
    }
}

/// Python's `"%g" % v`.
fn fmt_g(v: f64) -> String {
    if v.is_nan() {
        return "nan".into();
    }
    crate::drivers::common::fmt_g(v, 6)
}

/// Writes the XYZ lines of `raster` to `out`, returning the number of lines.
pub fn write_xyz<W: Write>(raster: &mut LazyRaster, out: &mut W, options: &XyzOptions) -> Result<u64> {
    if raster.dataset().is_some_and(|d| d.complex) {
        return Err(Error::Unsupported("writing complex rasters as XYZ is not supported".into()));
    }
    let bands: Vec<usize> = if options.bands.is_empty() { (1..=raster.band_count()).collect() } else { options.bands.clone() };
    if let Some(b) = bands.iter().find(|b| **b == 0 || **b > raster.band_count()) {
        return Err(Error::InvalidArgument(format!("band {b} does not exist (the raster has {})", raster.band_count())));
    }
    let (sx, sy) = options.skip;
    if sx == 0 || sy == 0 {
        return Err(Error::InvalidArgument("skip must be at least 1".into()));
    }
    let (w, h) = (raster.width(), raster.height());
    let [x_off, y_off, x_size, y_size] = options.srcwin.unwrap_or([0, 0, w, h]);
    if x_size == 0 || y_size == 0 || !Window::new(x_off, y_off, x_size, y_size).fits(w, h) {
        return Err(Error::InvalidArgument(format!("srcwin {x_off} {y_off} {x_size} {y_size} is outside the {w}x{h} raster")));
    }
    let gt = raster.geo_transform();
    let geographic = gt[0].abs() < 180.0 && gt[3].abs() < 180.0 && (w as f64 * gt[1]).abs() < 180.0 && (h as f64 * gt[5]).abs() < 180.0;
    // Values are converted to the first band's type, as gdal2xyz.py's array of it holds them.
    let dt = raster.data_type();
    let integer_format = matches!(dt, DataType::I32 | DataType::U32);
    let n = bands.len();
    let src_nodata: Option<Vec<f64>> = match &options.src_nodata {
        Some(v) => Some(per_band(v, n, "srcnodata")?),
        None => bands.iter().map(|b| raster.band_nodata(*b)).collect(),
    }
    .map(|v| v.into_iter().map(|x| cast(x, dt)).collect());
    let dst_nodata = match (&options.dst_nodata, &src_nodata) {
        (Some(v), Some(_)) => Some(per_band(v, n, "dstnodata")?.into_iter().map(|x| cast(x, dt)).collect::<Vec<_>>()),
        _ => None,
    };
    let skip_nodata = options.skip_nodata && src_nodata.is_some();
    let replace = !skip_nodata && dst_nodata.is_some();
    let delim = options.delimiter.as_str();
    let block = raster.block_size().map_or(1, |(_, bh)| bh.max(1));
    let rows = ((8usize << 20) / (x_size * n * 8).max(1)).max(1).max(block.min(256));
    let read = WindowOptions { bands: bands.clone(), overview: None };
    let mut lines = 0u64;
    let mut text = String::new();
    let mut values = vec![0.0; n];
    let mut y = y_off;
    let end = y_off + y_size;
    while y < end {
        let count = rows.min(end - y);
        let data = raster.read_window(Window::new(x_off, y, x_size, count), &read)?.raster.data;
        let plane = x_size * count;
        for r in 0..count {
            let row = y + r;
            if (row - y_off) % sy != 0 {
                continue;
            }
            text.clear();
            for c in (0..x_size).step_by(sx) {
                for (b, v) in values.iter_mut().enumerate() {
                    *v = cast(data.get_f64(b * plane + r * x_size + c), dt);
                }
                let mut shown: &[f64] = &values;
                if let Some(nd) = &src_nodata
                    && (skip_nodata || replace)
                    && values.iter().zip(nd).all(|(v, n)| v == n || (v.is_nan() && n.is_nan()))
                {
                    if skip_nodata {
                        continue;
                    }
                    shown = dst_nodata.as_deref().unwrap_or(&values);
                }
                let x = (x_off + c) as f64 + 0.5;
                let yc = row as f64 + 0.5;
                let gx = gt[0] + x * gt[1] + yc * gt[2];
                let gy = gt[3] + x * gt[4] + yc * gt[5];
                if geographic {
                    text.push_str(&crate::drivers::common::fmt_g(gx, 10));
                    text.push_str(delim);
                    text.push_str(&crate::drivers::common::fmt_g(gy, 10));
                } else {
                    text.push_str(&format!("{gx:.3}{delim}{gy:.3}"));
                }
                text.push_str(delim);
                for (i, v) in shown.iter().enumerate() {
                    if i > 0 {
                        text.push_str(delim);
                    }
                    if integer_format && v.is_finite() {
                        text.push_str(&(*v as i64).to_string());
                    } else {
                        text.push_str(&fmt_g(*v));
                    }
                }
                text.push('\n');
                lines += 1;
            }
            out.write_all(text.as_bytes())?;
        }
        y += count;
    }
    out.flush()?;
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RasterDataset;
    use wbraster::{Raster, RasterConfig};

    fn raster(dt: DataType, nodata: Option<f64>) -> LazyRaster {
        let cfg = RasterConfig {
            cols: 3,
            rows: 2,
            bands: 2,
            x_min: 1000.0,
            y_min: 2000.0,
            cell_size: 10.0,
            nodata: nodata.unwrap_or(-9999.0),
            data_type: dt,
            ..Default::default()
        };
        let values = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 10.5, 20.0, 30.0, 40.0, 50.0, 6.0];
        LazyRaster::from(RasterDataset::new(Raster::from_data(cfg, values).unwrap(), nodata.is_some()))
    }

    fn xyz(r: &mut LazyRaster, o: &XyzOptions) -> String {
        let mut out = Vec::new();
        write_xyz(r, &mut out, o).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn writes_pixel_centres_and_values() {
        let mut r = raster(DataType::F32, None);
        assert_eq!(xyz(&mut r, &XyzOptions::default()).lines().next().unwrap(), "1005.000 2015.000 1");
        let all = XyzOptions { bands: vec![], delimiter: ",".into(), ..Default::default() };
        assert_eq!(xyz(&mut r, &all).lines().next().unwrap(), "1005.000,2015.000,1,10.5");
        let skip = XyzOptions { skip: (2, 2), ..Default::default() };
        assert_eq!(xyz(&mut r, &skip), "1005.000 2015.000 1\n1025.000 2015.000 3\n");
        let win = XyzOptions { srcwin: Some([1, 1, 2, 1]), ..Default::default() };
        assert_eq!(xyz(&mut r, &win), "1015.000 2005.000 5\n1025.000 2005.000 6\n");
    }

    #[test]
    fn nodata_is_skipped_or_replaced_when_every_band_matches() {
        let mut r = raster(DataType::I32, Some(6.0));
        let all = XyzOptions { bands: vec![], skip_nodata: true, ..Default::default() };
        assert_eq!(xyz(&mut r, &all).lines().count(), 5);
        let replace = XyzOptions { bands: vec![], dst_nodata: Some(vec![-1.0]), ..Default::default() };
        assert_eq!(xyz(&mut r, &replace).lines().last().unwrap(), "1025.000 2005.000 -1 -1");
        // Int32 values print as integers, after conversion to the first band's type.
        assert_eq!(xyz(&mut r, &XyzOptions { bands: vec![], ..Default::default() }).lines().next().unwrap(), "1005.000 2015.000 1 10");
    }

    #[test]
    fn rejects_windows_outside_the_raster() {
        let mut r = raster(DataType::U8, None);
        let o = XyzOptions { srcwin: Some([2, 0, 2, 1]), ..Default::default() };
        assert!(write_xyz(&mut r, &mut Vec::new(), &o).is_err());
    }
}
