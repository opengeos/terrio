//! Fixes for rasters read by wbraster's drivers: whether the file declares a nodata value (the
//! drivers default to -9999 when it does not), and CRSs the drivers miss.

use super::common;
use crate::RasterDataset;
use crate::formats::RasterDriver;
use std::path::Path;
use wbraster::Raster;

/// Wraps a raster read by wbraster, with what the file really declares.
pub(crate) fn finish(path: &str, driver: RasterDriver, mut raster: Raster) -> RasterDataset {
    let p = Path::new(path);
    let has_nodata = match driver {
        // Formats without a nodata value of their own (GDAL may keep one in `.aux.xml`).
        RasterDriver::PNG | RasterDriver::JPEG | RasterDriver::XYZ | RasterDriver::GPKG => false,
        RasterDriver::HFA => {
            if let Some(crs) = hfa_pe_string(p) {
                raster.crs = crs;
            }
            std::fs::read(p).is_ok_and(|b| contains(&b, b"Eimg_NonInitializedValue\0"))
        }
        RasterDriver::RST => rst_flag(p, &mut raster),
        // DTED's void value.
        RasterDriver::DTED => {
            raster.nodata = -32767.0;
            true
        }
        // PCRaster marks missing REAL4 cells with NaN, which GDAL reports as the lowest float.
        RasterDriver::PCRaster => {
            if let wbraster::raster::RasterData::F32(v) = &mut raster.data {
                for x in v.iter_mut().filter(|x| x.is_nan()) {
                    *x = -f32::MAX;
                }
                raster.nodata = -f32::MAX as f64;
            }
            true
        }
        _ => true,
    };
    RasterDataset::with_driver(raster, driver, has_nodata)
}

/// Fills in what a GDAL `.aux.xml` sidecar holds and the file does not: CRS, geotransform, nodata.
pub(crate) fn apply_pam(path: &str, ds: &mut RasterDataset) {
    if let Some(pam) = common::read_pam(Path::new(path)) {
        apply(pam, ds);
    }
}

/// Fills in what a sidecar holds and the dataset does not.
pub(crate) fn apply(pam: common::Pam, ds: &mut RasterDataset) {
    if !ds.has_nodata && !pam.band_nodata.is_empty() {
        ds.band_nodata = (1..=ds.raster.bands)
            .map(|b| pam.band_nodata.iter().find(|(i, _)| *i == b).map(|(_, v)| *v).or(pam.dataset_nodata).or(ds.nodata()))
            .collect();
        ds.has_nodata = ds.band_nodata[0].is_some();
        ds.raster.nodata = ds.band_nodata[0].unwrap_or(-9999.);
    }
    let r = &mut ds.raster;
    if r.crs.is_unknown()
        && let Some(crs) = pam.crs
    {
        r.crs = crs;
    }
    if let Some(gt) = pam.geo_transform
        && r.x_min == 0.0
        && r.cell_size_x == 1.0
        && r.cell_size_y == 1.0
        && gt[2] == 0.0
        && gt[4] == 0.0
    {
        r.cell_size_x = gt[1];
        r.cell_size_y = gt[5].abs();
        r.x_min = gt[0];
        r.y_min = gt[3] + gt[5] * r.rows as f64;
    }
    if !ds.has_nodata
        && let Some(nd) = pam.nodata
    {
        ds.raster.nodata = nd;
        ds.has_nodata = true;
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// The CRS of the ESRI PE string GDAL stores in an Imagine file (`ProjectionX` node), which
/// describes the CRS more completely than the projection parameters wbraster reads.
pub(crate) fn hfa_pe_string(path: &Path) -> Option<wbraster::CrsInfo> {
    let b = std::fs::read(path).ok()?;
    let start = ["PROJCS[", "GEOGCS["].iter().filter_map(|k| b.windows(k.len()).position(|w| w == k.as_bytes())).min()?;
    let mut depth = 0i32;
    for (i, c) in b[start..].iter().enumerate() {
        match c {
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    let wkt = std::str::from_utf8(&b[start..start + i + 1]).ok()?;
                    return common::crs_from_text(wkt);
                }
            }
            0 => return None,
            _ => {}
        }
    }
    None
}

/// Whether an Idrisi raster declares nodata (`flag def'n` of missing data or background), and
/// its value.
fn rst_flag(path: &Path, raster: &mut Raster) -> bool {
    let Ok(text) = std::fs::read_to_string(common::sibling(path, "rdc")) else { return false };
    let get = |k: &str| {
        text.lines().find(|l| l.to_ascii_lowercase().starts_with(k)).and_then(|l| l.split_once(':')).map(|(_, v)| v.trim().to_string())
    };
    let def = get("flag def").unwrap_or_default().to_ascii_lowercase();
    match get("flag value").and_then(|v| common::parse_f64(&v)) {
        Some(v) if def == "missing data" || def == "background" => {
            raster.nodata = v;
            true
        }
        _ => false,
    }
}
