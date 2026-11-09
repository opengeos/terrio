// Written to match the behavior of:
//   GDAL v3.13.3:
//     frmts/xyz/xyzdataset.cpp
// No code is translated from these sources.

//! XYZ ASCII grids: one `x y z` line per cell centre, rows in order. Typed as GDAL types them.

use super::common;
use crate::dataset::fallible;
use crate::formats::RasterDriver;
use crate::{Error, RasterDataset, Result};
use std::path::Path;
use wbraster::raster::RasterData;
use wbraster::{DataType, Raster};

/// The most cells a grid may have for each of its points, and the most any grid may have
/// whatever its points: terrio's bounds, not GDAL's. GDAL 3.13.3 has no cell bound: it reads
/// sparse grids (6 points over 100001 x 2 cells, read here too as under MIN_MAX_CELLS) and
/// refuses points whose spacing it cannot determine (three points 10^9 apart at a spacing of
/// 10^-6, or at 10^300). A grid sparser than these bounds that GDAL reads is refused here. A file
/// of 10^6 points (about 25 MB) may thus declare at most 6.4 x 10^7 cells, 512 MB of Float64
/// values.
const MAX_CELLS_PER_POINT: usize = 64;
const MIN_MAX_CELLS: usize = 1 << 24;

/// The most cells a grid of `points` points may have.
fn max_cells(points: usize) -> usize {
    points.saturating_mul(MAX_CELLS_PER_POINT).max(MIN_MAX_CELLS)
}

/// The columns and rows of a grid of `points` points, when both are known and their cells within
/// [`max_cells`].
fn grid_dims(cols: Option<usize>, rows: Option<usize>, points: usize) -> Option<(usize, usize)> {
    cols.zip(rows).filter(|(c, r)| c.checked_mul(*r).is_some_and(|n| n <= max_cells(points)))
}

/// The number of cells `extent / step + 1` along an axis, `None` when it is out of range.
fn cells(extent: f64, step: f64) -> Option<usize> {
    let n = (extent / step).round() + 1.0;
    (n >= 1.0 && n < usize::MAX as f64).then_some(n as usize)
}

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
        if !x.is_finite() || !y.is_finite() {
            return Err(Error::InvalidData(format!("{}: a point at ({x}, {y}) off any grid", path.display())));
        }
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
    let (cols, rows) = (cells(x1 - x0, dx), cells(y1 - y0, dy));
    // The grid is the points' extent at their smallest spacing: a few points far apart at a tiny
    // spacing would declare more cells than any file of points could fill. Missing cells are
    // allowed, up to MAX_CELLS_PER_POINT for each point given (and any grid of up to
    // MIN_MAX_CELLS cells), and no more than the memory can hold.
    // Resource refusals, which the GDAL fallback does not retry.
    let most = max_cells(pts.len());
    let (cols, rows) = grid_dims(cols, rows, pts.len()).ok_or_else(|| {
        Error::ResourceLimit(format!(
            "{}: {} points spread over a grid of more than {most} cells: not a regular grid",
            path.display(),
            pts.len()
        ))
    })?;
    let n = cols * rows;
    fallible::check_usable(n.checked_mul(size_of::<f64>()), || format!("XYZ: a grid of {cols} x {rows} cells"))
        .map_err(|e| Error::ResourceLimit(format!("{}: {e}", path.display())))?;
    let mut grid = fallible::filled(n, 0.0f64, "an XYZ grid")?;
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
    // Truncated first: a reader skips the lines it cannot parse, so an interrupted write over the old
    // file would open as a grid of old and new points.
    std::fs::write(path, s)?;
    common::write_pam(path, &r.crs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_text(text: &str) -> Result<RasterDataset> {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("g.xyz");
        std::fs::write(&p, text).unwrap();
        read(&p)
    }

    #[test]
    fn reads_a_grid_with_a_missing_cell() {
        let ds = read_text("0 1 5\n1 1 6\n0 0 7\n").unwrap();
        assert_eq!((ds.raster.cols, ds.raster.rows), (2, 2));
        assert_eq!(ds.raster.data.to_f64_vec(), vec![5.0, 6.0, 7.0, 0.0]);
    }

    #[test]
    fn points_spread_over_a_huge_grid_are_refused() {
        // Three points 10^9 apart at a spacing of 10^-6: 10^15 cells.
        let e = read_text("0 0 1\n0.000001 0 1\n1000000000 0 1\n").unwrap_err();
        assert!(e.to_string().contains("not a regular grid"), "{e}");
        // Points at 10^300: a cell count past any integer.
        let e = read_text("0 0 1\n1 1 2\n1e300 1e300 3\n").unwrap_err();
        assert!(e.to_string().contains("not a regular grid"), "{e}");
        // Past 2^24 cells, a grid may have 64 cells a point: 3 points on a grid of 4101 x 4101
        // cells (just over 2^24) are refused before anything is allocated.
        let e = read_text("0 0 1\n1 1 1\n4100 4100 1\n").unwrap_err();
        assert!(e.to_string().contains("3 points spread over a grid of more than 16777216 cells"), "{e}");
        assert!(e.is_resource_refusal(), "{e:?}");
    }

    #[test]
    fn a_grid_may_have_64_cells_a_point_past_2_to_the_24() {
        // 2^20 points: up to 2^26 cells, beyond the 2^24 any grid may have.
        assert_eq!(grid_dims(Some(1 << 13), Some(1 << 13), 1 << 20), Some((1 << 13, 1 << 13)));
        assert_eq!(grid_dims(Some(1 << 13), Some((1 << 13) + 1), 1 << 20), None);
        // 3 points: 2^24 cells, no more.
        assert_eq!(grid_dims(Some(1 << 12), Some(1 << 12), 3), Some((1 << 12, 1 << 12)));
        assert_eq!(grid_dims(Some(1 << 13), Some(1 << 13), 3), None);
        assert_eq!(grid_dims(Some(usize::MAX), Some(2), usize::MAX), None);
        assert_eq!(grid_dims(None, Some(2), 3), None);
    }

    #[test]
    fn a_point_at_an_infinite_coordinate_is_refused() {
        let e = read_text("0 0 1\n1 0 2\ninf 0 3\n").unwrap_err();
        assert!(e.to_string().contains("off any grid"), "{e}");
    }
}
