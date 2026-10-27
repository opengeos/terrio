//! SRTM HGT tiles, as GDAL's SRTMHGT driver reads and writes them: one-degree tiles of big-endian
//! samples, named after their south-west corner (`N37W120.hgt`), without a header. The file's
//! size gives the grid (1201 x 1201 for 3", 3601 x 3601 for 1"...) and its name the location.
//! Besides `.hgt`, the NASADEM and SRTM water body variants GDAL reads (`.hgts`, `.err`, `.img`,
//! `.num`, `.raw`, `.swb`) are read; zipped tiles (`N37W120.hgt.zip`) are opened through the
//! archive.

use super::common::{self, Interleave, Layout};
use crate::formats::RasterDriver;
use crate::{Error, RasterDataset, Result};
use std::io::Read;
use std::path::Path;
use wbraster::{CrsInfo, DataType, Raster};

/// The value of void cells.
const NODATA: f64 = -32768.0;

/// The grid sizes GDAL writes, `(columns, rows)`: 3", 1" above 50 degrees of latitude, 1" and 0.5".
const WRITE_SIZES: [(usize, usize); 4] = [(1201, 1201), (1801, 3601), (3601, 3601), (7201, 7201)];

/// The extensions of the tiles GDAL reads, lowercase.
const EXTENSIONS: [&str; 7] = ["hgt", "hgts", "err", "img", "num", "raw", "swb"];

/// The grid and sample type of a tile of `len` bytes.
fn grid(len: u64) -> Option<(usize, usize, DataType)> {
    Some(match len {
        2_884_802 => (1201, 1201, DataType::I16),
        12_970_802 => (1801, 3601, DataType::I16),
        12_967_201 => (3601, 3601, DataType::U8),
        25_934_402 => (3601, 3601, DataType::I16),
        51_868_804 => (3601, 3601, DataType::F32),
        103_708_802 => (7201, 7201, DataType::I16),
        _ => return None,
    })
}

/// The latitude and longitude of the south-west corner of a tile named as GDAL expects
/// (`N37W120.hgt`, case-insensitive): a hemisphere letter, two digits of latitude, a hemisphere
/// letter and three digits of longitude, then a dot.
fn tile_origin(name: &str) -> Option<(i32, i32)> {
    let b = name.as_bytes();
    if b.len() < 11 || b[7] != b'.' {
        return None;
    }
    let digits = |s: &[u8]| -> Option<i32> { std::str::from_utf8(s).ok().filter(|t| t.bytes().all(|c| c.is_ascii_digit()))?.parse().ok() };
    let lat = match b[0].to_ascii_lowercase() {
        b'n' => digits(&b[1..3])?,
        b's' => -digits(&b[1..3])?,
        _ => return None,
    };
    let lon = match b[3].to_ascii_lowercase() {
        b'e' => digits(&b[4..7])?,
        b'w' => -digits(&b[4..7])?,
        _ => return None,
    };
    // Latitude is at most 90 degrees and longitude at most 180.
    if lat.abs() > 90 || lon.abs() > 180 {
        return None;
    }
    Some((lat, lon))
}

/// The file name of a path, and its lowercase extension.
fn name_and_extension(path: &Path) -> Option<(&str, String)> {
    let name = path.file_name()?.to_str()?;
    Some((name, name.rsplit_once('.')?.1.to_ascii_lowercase()))
}

/// Whether the file is an SRTM tile: named after its corner, with one of the tile extensions and
/// the size of a tile. An ERDAS Imagine file named as a tile is not one.
pub(crate) fn is_hgt(path: &Path) -> bool {
    let Some((name, ext)) = name_and_extension(path) else { return false };
    if tile_origin(name).is_none() || !EXTENSIONS.contains(&ext.as_str()) {
        return false;
    }
    if !std::fs::metadata(path).is_ok_and(|m| m.is_file() && grid(m.len()).is_some()) {
        return false;
    }
    let mut magic = [0u8; 15];
    let is_hfa = std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut magic)).is_ok() && &magic == b"EHFA_HEADER_TAG";
    !is_hfa
}

/// Reads an SRTM tile, georeferenced as GDAL does: WGS 84, cell centres on whole degrees at the
/// tile's edges (`AREA_OR_POINT=Point`), with -32768 as nodata except for Byte (water body) tiles.
pub(crate) fn read(path: &Path) -> Result<RasterDataset> {
    let bad = || Error::InvalidData(format!("{}: not an SRTM HGT tile", path.display()));
    let (name, ext) = name_and_extension(path).ok_or_else(bad)?;
    let (lat, lon) = tile_origin(name).ok_or_else(bad)?;
    let (cols, rows, data_type) = grid(std::fs::metadata(path)?.len()).ok_or_else(bad)?;
    let bytes = std::fs::read(path)?;
    let layout = Layout { cols, rows, bands: 1, data_type, little_endian: false, interleave: Interleave::Bsq, offset: 0, bottom_up: false };
    let data = common::decode(&bytes, &layout)?;
    let (dx, dy) = (1.0 / (cols - 1) as f64, 1.0 / (rows - 1) as f64);
    let gt = [lon as f64 - dx / 2.0, dx, 0.0, (lat + 1) as f64 + dy / 2.0, 0.0, -dy];
    let nodata = (data_type != DataType::U8).then_some(NODATA);
    let mut raster = common::make_raster(cols, rows, 1, data, gt, CrsInfo::from_epsg(4326), nodata)?;
    raster.metadata.push(("AREA_OR_POINT".into(), "Point".into()));
    let mut ds = RasterDataset::with_driver(raster, RasterDriver::SRTMHGT, nodata.is_some());
    // Error, image and water body layers are not heights.
    let unit = !matches!(ext.as_str(), "err" | "img" | "num" | "raw" | "swb");
    ds.band_metadata = vec![crate::BandMetadata { unit: unit.then(|| "m".into()), ..Default::default() }];
    Ok(ds)
}

/// Writes an SRTM tile as GDAL does: Int16 samples (values rounded and clamped), with nodata
/// cells set to -32768. The raster must have one of the tile sizes; only its first band is written.
/// A raster not in WGS 84, not on whole degrees or written under another name than its corner's
/// is written all the same, with a warning, as GDAL does.
pub(crate) fn write(r: &Raster, nodata: Option<f64>, path: &Path) -> Result<()> {
    if r.bands == 0 {
        return Err(Error::Unsupported("SRTM HGT tiles need a band".into()));
    }
    if r.bands > 1 {
        crate::warnings::warn("SRTMHGT driver only uses the first band of the dataset.".into());
    }
    if !WRITE_SIZES.contains(&(r.cols, r.rows)) {
        return Err(Error::Unsupported(format!(
            "SRTM HGT tiles are 1201x1201, 1801x3601, 3601x3601 or 7201x7201 cells, not {}x{}",
            r.cols, r.rows
        )));
    }
    if r.crs.epsg != Some(4326) {
        crate::warnings::warn("The source CRS is not WGS 84: the SRTM HGT tile is written as if it were.".into());
    }
    // The south-west cell's centre, which names the tile.
    let (west, south) = (r.x_min + r.cell_size_x / 2.0, r.y_min + r.cell_size_y / 2.0);
    let (lon, lat) = (west.round(), south.round());
    if (lon - west).abs() > 1e-10 || (lat - south).abs() > 1e-10 {
        crate::warnings::warn(
            "The corner coordinates of the source are not properly aligned on plain latitude/longitude boundaries.".into(),
        );
    }
    // GDAL does not check this: cells of another size are read back over one degree all the same.
    let one_degree = |cell: f64, n: usize| (cell * (n - 1) as f64 - 1.0).abs() <= 1e-9;
    if !one_degree(r.cell_size_x, r.cols) || !one_degree(r.cell_size_y, r.rows) {
        crate::warnings::warn(format!(
            "The source cells do not span one degree ({} x {} degrees for {}x{} cells): the SRTM HGT tile is read back as a one-degree tile.",
            r.cell_size_x * (r.cols - 1) as f64,
            r.cell_size_y * (r.rows - 1) as f64,
            r.cols,
            r.rows
        ));
    }
    let expected = format!(
        "{}{:02}{}{:03}.HGT",
        if lat >= 0.0 { 'N' } else { 'S' },
        lat.abs() as i32,
        if lon >= 0.0 { 'E' } else { 'W' },
        lon.abs() as i32
    );
    if !path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.eq_ignore_ascii_case(&expected)) {
        crate::warnings::warn(format!("Expected output filename is {expected}."));
    }
    let n = r.cols * r.rows;
    let mut out = Vec::with_capacity(n * 2);
    for v in r.data.iter_f64().take(n) {
        let void = nodata.is_some_and(|nd| v == nd || (nd.is_nan() && v.is_nan()));
        // Rounded, then saturated by `as` (NaN to 0), as GDAL converts to Int16.
        let s = if void { NODATA as i16 } else { v.round() as i16 };
        out.extend(s.to_be_bytes());
    }
    std::fs::write(path, out)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbraster::raster::RasterData;

    #[test]
    fn tile_names() {
        assert_eq!(tile_origin("N37W120.hgt"), Some((37, -120)));
        assert_eq!(tile_origin("s01e009.HGT"), Some((-1, 9)));
        assert_eq!(tile_origin("N37W120.SRTMSWBD.raw"), Some((37, -120)));
        assert_eq!(tile_origin("N37W12.hgt"), None);
        assert_eq!(tile_origin("X37W120.hgt"), None);
        assert_eq!(tile_origin("N3xW120.hgt"), None);
        assert_eq!(tile_origin("N90W180.hgt"), Some((90, -180)));
        assert_eq!(tile_origin("N91W120.hgt"), None);
        assert_eq!(tile_origin("N37W181.hgt"), None);
        assert_eq!(tile_origin("N99W999.hgt"), None);
    }

    /// A 3" tile with a height per row, void in its first cell.
    fn tile_bytes() -> Vec<u8> {
        let mut b = Vec::with_capacity(1201 * 1201 * 2);
        for row in 0..1201i16 {
            for col in 0..1201 {
                let v = if row == 0 && col == 0 { -32768 } else { row - 100 };
                b.extend(v.to_be_bytes());
            }
        }
        b
    }

    #[test]
    fn reads_a_3_second_tile() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("S01W120.hgt");
        std::fs::write(&p, tile_bytes()).unwrap();
        assert!(is_hgt(&p));
        let ds = crate::open_raster(p.to_str().unwrap()).unwrap();
        assert_eq!(ds.driver, RasterDriver::SRTMHGT);
        assert_eq!((ds.raster.cols, ds.raster.rows, ds.raster.data_type), (1201, 1201, DataType::I16));
        assert_eq!(ds.nodata(), Some(-32768.0));
        assert_eq!(ds.crs().epsg, Some(4326));
        let gt = ds.geo_transform();
        let d = 1.0 / 1200.0;
        let want = [-120.0 - d / 2.0, d, 0.0, 0.0 + d / 2.0, 0.0, -d];
        assert!(gt.iter().zip(want).all(|(a, b)| (a - b).abs() < 1e-12), "{gt:?}");
        assert_eq!(ds.raster.get(0, 0, 0), -32768.0);
        assert_eq!(ds.raster.get(0, 1200, 5), 1100.0);
        assert!(ds.raster.metadata.iter().any(|(k, v)| k == "AREA_OR_POINT" && v == "Point"));
        assert_eq!(ds.band_metadata[0].unit.as_deref(), Some("m"));
    }

    #[test]
    fn reads_a_water_body_tile_without_nodata() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("N37W120.raw");
        std::fs::write(&p, vec![255u8; 3601 * 3601]).unwrap();
        let ds = crate::open_raster(p.to_str().unwrap()).unwrap();
        assert_eq!((ds.raster.cols, ds.raster.data_type, ds.nodata()), (3601, DataType::U8, None));
        assert_eq!(ds.band_metadata[0].unit, None);
    }

    #[test]
    fn needs_the_name_and_size_of_a_tile() {
        let dir = tempfile::tempdir().unwrap();
        let wrong_name = dir.path().join("dem.hgt");
        std::fs::write(&wrong_name, tile_bytes()).unwrap();
        assert!(!is_hgt(&wrong_name));
        let wrong_size = dir.path().join("N37W120.hgt");
        std::fs::write(&wrong_size, [0u8; 100]).unwrap();
        assert!(!is_hgt(&wrong_size));
        assert!(crate::open_raster(wrong_size.to_str().unwrap()).is_err());
    }

    #[test]
    fn writes_what_it_reads() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("N37W120.hgt");
        std::fs::write(&src, tile_bytes()).unwrap();
        let ds = crate::open_raster(src.to_str().unwrap()).unwrap();
        let out = dir.path().join("out").join("N37W120.hgt");
        std::fs::create_dir(out.parent().unwrap()).unwrap();
        crate::warnings::take();
        crate::write_raster_dataset(&ds, out.to_str().unwrap(), &Default::default()).unwrap();
        assert!(crate::warnings::take().is_empty());
        assert_eq!(std::fs::read(&out).unwrap(), tile_bytes());
    }

    #[test]
    fn writes_floats_rounded_with_voids() {
        let d = 1.0 / 1200.0;
        let mut values = vec![1.6f32; 1201 * 1201];
        values[0] = -9999.0;
        values[1] = f32::NAN;
        values[2] = 40000.0;
        let gt = [-120.0 - d / 2.0, d, 0.0, 38.0 + d / 2.0, 0.0, -d];
        let r = common::make_raster(1201, 1201, 1, RasterData::F32(values), gt, CrsInfo::from_epsg(4326), Some(-9999.0)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("dem.hgt");
        crate::warnings::take();
        write(&r, Some(-9999.0), &p).unwrap();
        assert_eq!(crate::warnings::take(), ["Expected output filename is N37W120.HGT."]);
        let b = std::fs::read(&p).unwrap();
        let at = |i: usize| i16::from_be_bytes([b[2 * i], b[2 * i + 1]]);
        assert_eq!((at(0), at(1), at(2), at(3)), (-32768, 0, i16::MAX, 2));
    }

    #[test]
    fn warns_when_cells_do_not_span_a_degree() {
        let d = 2.0 / 1200.0;
        let gt = [-120.0 - d / 2.0, d, 0.0, 39.0 + d / 2.0, 0.0, -d];
        let r = common::make_raster(1201, 1201, 1, RasterData::I16(vec![0; 1201 * 1201]), gt, CrsInfo::from_epsg(4326), None).unwrap();
        let dir = tempfile::tempdir().unwrap();
        crate::warnings::take();
        write(&r, None, &dir.path().join("N37W120.hgt")).unwrap();
        let warnings = crate::warnings::take();
        assert!(warnings.iter().any(|w| w.starts_with("The source cells do not span one degree")), "{warnings:?}");
    }

    #[test]
    fn refuses_other_sizes() {
        let r = common::make_raster(10, 10, 1, RasterData::I16(vec![0; 100]), [0.0, 1.0, 0.0, 10.0, 0.0, -1.0], CrsInfo::default(), None)
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        assert!(write(&r, None, &dir.path().join("N00E000.hgt")).is_err());
    }
}
