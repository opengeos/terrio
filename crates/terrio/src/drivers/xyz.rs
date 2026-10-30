// Written to match the behavior of:
//   GDAL v3.13.3:
//     frmts/xyz/xyzdataset.cpp
// No code is translated from these sources.

//! XYZ ASCII grids: one `x y z` line per cell centre, rows in order. Typed as GDAL types them.

use super::common;
use crate::formats::RasterDriver;
use crate::{Error, RasterDataset, Result};
use std::path::Path;
use wbraster::raster::RasterData;
use wbraster::{DataType, Raster};

/// Reads an XYZ grid. Missing cells are 0, as in GDAL.
pub(crate) fn read(path: &Path) -> Result<RasterDataset> {
    let text = std::fs::read_to_string(path)?;
    let mut pts: Vec<(f64, f64, f64)> = Vec::new();
    let mut data_type = DataType::U8;
    for line in text.lines() {
        let f: Vec<&str> = line.split([' ', '\t', ',', ';']).filter(|s| !s.is_empty()).collect();
        if f.len() < 3 {
            continue;
        }
        let (Ok(x), Ok(y), Some(z)) = (f[0].parse::<f64>(), f[1].parse::<f64>(), common::parse_f64(f[2])) else {
            continue; // a header line
        };
        if !(z >= i32::MIN as f64 && z <= i32::MAX as f64) || z.fract() != 0.0 {
            data_type = DataType::F32;
        } else if matches!(data_type, DataType::U8 | DataType::I16) && !(0.0..=255.0).contains(&z) {
            data_type = if (-32768.0..=32767.0).contains(&z) { DataType::I16 } else { DataType::I32 };
        }
        pts.push((x, y, z));
    }
    if pts.is_empty() {
        return Err(Error::InvalidData(format!("{}: no x y z values", path.display())));
    }
    let axis = |get: fn(&(f64, f64, f64)) -> f64| {
        let mut v: Vec<f64> = pts.iter().map(get).collect();
        v.sort_by(f64::total_cmp);
        v.dedup();
        let step = v.windows(2).map(|w| w[1] - w[0]).fold(f64::INFINITY, f64::min);
        (v[0], v[v.len() - 1], if step.is_finite() { step } else { 1.0 })
    };
    let (x0, x1, dx) = axis(|p| p.0);
    let (y0, y1, dy) = axis(|p| p.1);
    let cols = ((x1 - x0) / dx).round() as usize + 1;
    let rows = ((y1 - y0) / dy).round() as usize + 1;
    let mut grid = vec![0.0f64; cols * rows];
    for (x, y, z) in &pts {
        let c = ((x - x0) / dx).round() as usize;
        let r = ((y1 - y) / dy).round() as usize;
        if c < cols && r < rows {
            grid[r * cols + c] = *z;
        }
    }
    let data = RasterData::from_f64_vec(data_type, grid);
    let gt = [x0 - dx / 2.0, dx, 0.0, y1 + dy / 2.0, 0.0, -dy];
    let raster = common::make_raster(cols, rows, 1, data, gt, Default::default(), None)?;
    Ok(RasterDataset::with_driver(raster, RasterDriver::XYZ, false))
}

/// Writes an XYZ grid: integers for integer types up to 32 bits, Float32 values otherwise.
pub(crate) fn write(r: &Raster, path: &Path) -> Result<()> {
    if r.bands != 1 {
        return Err(Error::Unsupported(format!("XYZ grids hold one band, not {}", r.bands)));
    }
    let as_int = matches!(r.data_type, DataType::U8 | DataType::I16 | DataType::U16 | DataType::I32);
    let values = r.data.to_f64_vec();
    let mut s = String::with_capacity(r.cols * r.rows * 24);
    let top = r.y_max();
    for row in 0..r.rows {
        let y = top + (row as f64 + 0.5) * -r.cell_size_y;
        for col in 0..r.cols {
            let x = r.x_min + (col as f64 + 0.5) * r.cell_size_x;
            let v = values[row * r.cols + col];
            let z = if as_int { format!("{}", v as i64) } else { common::format_value(v as f32 as f64, DataType::F32) };
            s.push_str(&format!("{x} {y} {z}\n"));
        }
    }
    std::fs::write(path, s)?;
    common::write_pam(path, &r.crs)
}
