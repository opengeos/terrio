//! Esri ASCII grids (`.asc`), with a `.prj` CRS. Typed as GDAL types them: Int32 unless the
//! values or the nodata value have decimals, then Float32 (Float64 for out-of-range nodata).

use super::common;
use crate::formats::RasterDriver;
use crate::{Error, RasterDataset, Result};
use std::fmt::Write as _;
use std::path::Path;
use wbraster::raster::RasterData;
use wbraster::{DataType, Raster};

/// Reads an Esri ASCII grid.
/// The keywords of an Esri ASCII grid's header lines (in any case).
pub(crate) const HEADER_KEYS: &[&str] =
    &["ncols", "nrows", "xllcorner", "yllcorner", "xllcenter", "yllcenter", "cellsize", "dx", "dy", "nodata_value"];

pub(crate) fn read(path: &Path) -> Result<RasterDataset> {
    let text = std::fs::read_to_string(path)?;
    let bad = |what: &str| Error::InvalidData(format!("{}: {what}", path.display()));
    // The header is the lines that start with a known keyword; the values follow.
    let mut header: Vec<(String, String)> = Vec::new();
    let mut data_start = 0;
    for line in text.split_inclusive('\n') {
        let t = line.trim();
        let key = t.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
        if HEADER_KEYS.contains(&key.as_str()) {
            let mut it = t.split_whitespace();
            if let (Some(k), Some(v)) = (it.next(), it.next()) {
                header.push((k.to_ascii_lowercase(), v.to_string()));
            }
            data_start += line.len();
        } else if t.is_empty() {
            data_start += line.len();
        } else {
            break;
        }
    }
    let get = |k: &str| header.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str());
    let num = |k: &str| get(k).and_then(common::parse_f64);
    let cols = get("ncols").and_then(|v| v.parse::<usize>().ok()).ok_or_else(|| bad("missing ncols"))?;
    let rows = get("nrows").and_then(|v| v.parse::<usize>().ok()).ok_or_else(|| bad("missing nrows"))?;
    let (dx, dy) = match num("cellsize") {
        Some(c) => (c, c),
        None => (num("dx").ok_or_else(|| bad("missing cellsize"))?, num("dy").ok_or_else(|| bad("missing cellsize"))?),
    };
    let gt = match (num("xllcorner"), num("yllcorner"), num("xllcenter"), num("yllcenter")) {
        (Some(x), Some(y), _, _) => [x, dx, 0.0, y + rows as f64 * dy, 0.0, -dy],
        (_, _, Some(x), Some(y)) => [x - 0.5 * dx, dx, 0.0, y - 0.5 * dy + rows as f64 * dy, 0.0, -dy],
        _ => [0.0, dx, 0.0, 0.0, 0.0, -dy],
    };

    let body = &text[data_start..];
    let mut data_type = DataType::I32;
    let mut nodata = None;
    if let Some(nd) = get("nodata_value") {
        let v = if nd == "null" { -(f32::MAX as f64) } else { common::parse_f64(nd).ok_or_else(|| bad("invalid NODATA_value"))? };
        if nd == "null" || nd.contains(['.', ',']) || v.is_nan() || v < i32::MIN as f64 || v > i32::MAX as f64 {
            data_type = DataType::F32;
            if v.is_finite() && (v.abs() < f32::MIN_POSITIVE as f64 || v.abs() > f32::MAX as f64) && v != 0.0 {
                data_type = DataType::F64;
            }
        }
        nodata = Some(if data_type == DataType::F32 { map_to_f32(v) } else { v });
    }
    if data_type == DataType::I32 && body.bytes().any(|b| matches!(b, b'.' | b',' | b'e' | b'E')) {
        data_type = DataType::F32;
    }

    let n = cols * rows;
    let mut tokens = body.split_ascii_whitespace();
    let short = || bad(&format!("expected {n} values"));
    let data = match data_type {
        DataType::I32 => {
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                let t = tokens.next().ok_or_else(short)?;
                v.push(t.parse::<i32>().or_else(|_| t.parse::<f64>().map(|f| f as i32)).map_err(|_| bad(&format!("invalid value '{t}'")))?);
            }
            RasterData::I32(v)
        }
        DataType::F32 => {
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                let t = tokens.next().ok_or_else(short)?;
                let x = if t == "null" {
                    -f64::from(f32::MAX)
                } else {
                    common::parse_f64(t).ok_or_else(|| bad(&format!("invalid value '{t}'")))?
                };
                // GDAL clamps out-of-range values (infinities included) to the float range.
                v.push(if x.is_nan() { f32::NAN } else { x.clamp(-f64::from(f32::MAX), f64::from(f32::MAX)) as f32 });
            }
            RasterData::F32(v)
        }
        _ => {
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                let t = tokens.next().ok_or_else(short)?;
                v.push(if t == "null" { -f64::MAX } else { common::parse_f64(t).ok_or_else(|| bad(&format!("invalid value '{t}'")))? });
            }
            RasterData::F64(v)
        }
    };
    let crs = common::read_prj(path).unwrap_or_default();
    let raster = common::make_raster(cols, rows, 1, data, gt, crs, nodata)?;
    Ok(RasterDataset::with_driver(raster, RasterDriver::AAIGrid, nodata.is_some()))
}

/// A nodata value as GDAL stores it for a Float32 grid.
fn map_to_f32(v: f64) -> f64 {
    if !v.is_finite() {
        v
    } else if v >= f32::MAX as f64 {
        f32::MAX as f64
    } else if v <= -(f32::MAX as f64) {
        -(f32::MAX as f64)
    } else {
        v as f32 as f64
    }
}

/// Writes an Esri ASCII grid and its `.prj`. Only the first band is written.
pub(crate) fn write(r: &Raster, nodata: Option<f64>, path: &Path) -> Result<()> {
    if r.bands != 1 {
        return Err(Error::Unsupported(format!("Esri ASCII grids hold one band, not {}", r.bands)));
    }
    let as_int = matches!(r.data_type, DataType::U8 | DataType::I8 | DataType::I16 | DataType::U16 | DataType::I32);
    let mut s = format!("ncols        {}\nnrows        {}\nxllcorner    {}\nyllcorner    {}\n", r.cols, r.rows, r.x_min, r.y_min);
    if r.cell_size_x == r.cell_size_y {
        let _ = writeln!(s, "cellsize     {}", r.cell_size_x);
    } else {
        let _ = writeln!(s, "dx           {}\ndy           {}", r.cell_size_x, r.cell_size_y);
    }
    if let Some(nd) = nodata {
        if as_int {
            let _ = writeln!(s, "NODATA_value {}", nd as i64);
        } else {
            let _ = writeln!(s, "NODATA_value {}", float_text(nd, r.data_type));
        }
    }
    let n = r.cols * r.rows;
    s.reserve(n * 8);
    macro_rules! rows {
        ($v:expr, $fmt:expr) => {
            for row in $v[..n].chunks(r.cols) {
                for (i, x) in row.iter().enumerate() {
                    if i > 0 {
                        s.push(' ');
                    }
                    s.push_str(&$fmt(*x));
                }
                s.push('\n');
            }
        };
    }
    match &r.data {
        RasterData::U8(v) => rows!(v, |x: u8| x.to_string()),
        RasterData::I8(v) => rows!(v, |x: i8| x.to_string()),
        RasterData::U16(v) => rows!(v, |x: u16| x.to_string()),
        RasterData::I16(v) => rows!(v, |x: i16| x.to_string()),
        RasterData::U32(v) => rows!(v, |x: u32| x.to_string()),
        RasterData::I32(v) => rows!(v, |x: i32| x.to_string()),
        RasterData::U64(v) => rows!(v, |x: u64| x.to_string()),
        RasterData::I64(v) => rows!(v, |x: i64| x.to_string()),
        RasterData::F32(v) => rows!(v, |x: f32| float_text(x as f64, DataType::F32)),
        RasterData::F64(v) => rows!(v, |x: f64| float_text(x, DataType::F64)),
    }
    std::fs::write(path, s)?;
    common::write_prj(path, &r.crs)
}

/// A float as text that reads back as a float (with a decimal point, so the grid stays typed
/// as floating point) and parses to the same value.
fn float_text(v: f64, t: DataType) -> String {
    let s = common::format_value(v, t);
    if v.is_finite() && !s.contains(['.', 'e', 'E']) { format!("{s}.0") } else { s }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_values_as_gdal_does() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.asc");
        std::fs::write(&p, "ncols 3\nnrows 2\nxllcorner 10\nyllcorner 20\ncellsize 5\nNODATA_value -9999\n1 2 3\n4 -9999 6\n").unwrap();
        let ds = read(&p).unwrap();
        assert_eq!(ds.raster.data_type, DataType::I32);
        assert_eq!(ds.nodata(), Some(-9999.0));
        assert_eq!(ds.geo_transform(), [10.0, 5.0, 0.0, 30.0, 0.0, -5.0]);
        std::fs::write(&p, "ncols 2\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 1\n1.5 -inf\n").unwrap();
        let ds = read(&p).unwrap();
        assert_eq!(ds.raster.data_f32().unwrap(), &[1.5, -f32::MAX]);
        assert!(!ds.has_nodata);
    }
}
