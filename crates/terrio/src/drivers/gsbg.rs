//! Golden Software Surfer 6 binary grids (`DSBB`), as GDAL's GSBG driver writes them. Surfer's
//! ASCII and Surfer 7 grids are read by wbraster.

use super::common;
use crate::formats::RasterDriver;
use crate::{Error, RasterDataset, Result};
use std::path::Path;
use wbraster::{DataType, Raster};

/// The value Surfer uses for blanked (nodata) cells.
const BLANK: f32 = 1.701_41e38;

/// Whether the file is a Surfer 6 binary grid.
pub(crate) fn is_gsbg(path: &Path) -> bool {
    let mut b = [0u8; 4];
    std::fs::File::open(path).and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b)).is_ok() && &b == b"DSBB"
}

/// Opens a Surfer 6 binary grid, reading only its header now: its pixels are read by window. Rows
/// are stored bottom-up; coordinates are cell centres.
pub(crate) fn open(path: &Path) -> Result<common::RawRaster> {
    // The header: the tag, the grid size, its extent and its value range.
    let mut b = Vec::with_capacity(56);
    std::io::Read::read_to_end(&mut std::io::Read::take(std::fs::File::open(path)?, 56), &mut b)?;
    if b.len() < 56 || &b[..4] != b"DSBB" {
        return Err(Error::InvalidData(format!("{}: not a Surfer 6 binary grid", path.display())));
    }
    let (nx, ny) = (i16::from_le_bytes([b[4], b[5]]) as usize, i16::from_le_bytes([b[6], b[7]]) as usize);
    let f = |i: usize| f64::from_le_bytes(b[8 + 8 * i..16 + 8 * i].try_into().unwrap_or_default());
    let (xlo, xhi, ylo, yhi) = (f(0), f(1), f(2), f(3));
    let layout = common::Layout {
        cols: nx,
        rows: ny,
        bands: 1,
        data_type: DataType::F32,
        little_endian: true,
        interleave: common::Interleave::Bsq,
        offset: 56,
        bottom_up: true,
    };
    let dx = (xhi - xlo) / (nx as f64 - 1.0);
    let dy = (ylo - yhi) / (ny as f64 - 1.0);
    let gt = [xlo - dx / 2.0, dx, 0.0, yhi - dy / 2.0, 0.0, dy];
    let crs = common::read_prj(path).unwrap_or_default();
    let raster = common::raster_header(nx, ny, 1, DataType::F32, gt, crs, Some(BLANK as f64))?;
    common::RawRaster::open(RasterDataset::with_driver(raster, RasterDriver::GSBG, true), path, layout, &[])
}

/// [`open`] for a file `raw_opener` recognised by its first bytes, whatever driver its name points
/// to: refused when the process-wide allow-list leaves GSBG out, as a Surfer 6 grid named for
/// another driver (`x.tif`, `big.hgt`) reaches this point as that driver's file.
pub(crate) fn open_recognised(path: &Path) -> Result<common::RawRaster> {
    crate::formats::check_driver_allowed("GSBG", &path.to_string_lossy(), crate::formats::Kind::Raster)?;
    open(path)
}

/// Writes a Surfer 6 binary grid (Float32), with nodata cells blanked.
pub(crate) fn write(r: &Raster, nodata: Option<f64>, path: &Path) -> Result<()> {
    if r.bands != 1 {
        return Err(Error::Unsupported(format!("Surfer grids hold one band, not {}", r.bands)));
    }
    if r.cols > i16::MAX as usize || r.rows > i16::MAX as usize {
        return Err(Error::Unsupported("Surfer 6 grids hold at most 32767 columns and rows".into()));
    }
    let nd = nodata.map(|v| v as f32);
    let mut vals: Vec<f32> = r.data.to_f64_vec()[..r.cols * r.rows].iter().map(|v| *v as f32).collect();
    let (mut zmin, mut zmax) = (f64::MAX, f64::MIN);
    for v in vals.iter_mut() {
        if nd.is_some_and(|n| *v == n) || v.is_nan() {
            *v = BLANK;
        } else {
            zmin = zmin.min(*v as f64);
            zmax = zmax.max(*v as f64);
        }
    }
    let mut out = b"DSBB".to_vec();
    out.extend((r.cols as i16).to_le_bytes());
    out.extend((r.rows as i16).to_le_bytes());
    let xlo = r.x_min + r.cell_size_x / 2.0;
    let xhi = r.x_min + r.cell_size_x * (r.cols as f64 - 0.5);
    let ylo = r.y_min + r.cell_size_y / 2.0;
    let yhi = r.y_min + r.cell_size_y * (r.rows as f64 - 0.5);
    for v in [xlo, xhi, ylo, yhi, zmin, zmax] {
        out.extend(v.to_le_bytes());
    }
    for row in vals.chunks(r.cols).rev() {
        for v in row {
            out.extend(v.to_le_bytes());
        }
    }
    std::fs::write(path, out)?;
    common::write_pam(path, &r.crs)
}
