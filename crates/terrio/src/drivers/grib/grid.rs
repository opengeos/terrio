// SPDX-FileCopyrightText: Copyright (c) 2007, ITC
// SPDX-FileCopyrightText: Copyright (c) 2008-2017, Even Rouault <even dot rouault at spatialys dot com>
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL (bundled NOAA MDL degrib) v3.12.2 (public domain):
//     frmts/grib/degrib/degrib/metaparse.cpp
//     frmts/grib/degrib/degrib/scan.c
//     frmts/grib/degrib/degrib/grib2api.c
//   GDAL (bundled NCEP g2clib) v3.12.2 (public domain):
//     frmts/grib/degrib/g2clib/gridtemplates.c
//   GDAL v3.12.2:
//     frmts/grib/gribdataset.cpp

//! GRIB2 grid definitions (section 3) and their georeferencing as GDAL's GRIB driver computes it
//! (degrib's `ParseSect3`, then `GRIBDataset::SetGribMetaData`): the CRS, with GDAL's earth
//! shapes, and the geotransform of north-up rasters, global grids from 0 to 360 degrees shifted
//! to -180 to 180.

use crate::{Error, Result};

/// The widths (in octets, negative for sign-and-magnitude values) of the values of the grid
/// templates terrio reads, from octet 15 (g2clib's `gridtemplates.c`).
fn template_map(number: u16) -> Option<&'static [i8]> {
    Some(match number {
        0 => &[1, 1, 4, 1, 4, 1, 4, 4, 4, 4, 4, -4, -4, 1, -4, -4, 4, 4, 1],
        1 => &[1, 1, 4, 1, 4, 1, 4, 4, 4, 4, 4, -4, -4, 1, -4, -4, 4, 4, 1, -4, 4, 4],
        10 => &[1, 1, 4, 1, 4, 1, 4, 4, 4, -4, -4, 1, -4, -4, -4, 1, 4, 4, 4],
        12 => &[1, 1, 4, 1, 4, 1, 4, 4, 4, -4, 4, 1, -4, -4, -4, 1, 4, 4, -4, -4, -4, -4],
        20 => &[1, 1, 4, 1, 4, 1, 4, 4, 4, -4, -4, 1, -4, -4, -4, 4, 1, 1],
        30 | 31 => &[1, 1, 4, 1, 4, 1, 4, 4, 4, -4, 4, 1, -4, 4, 4, 4, 1, 1, -4, -4, -4, 4],
        40 => &[1, 1, 4, 1, 4, 1, 4, 4, 4, 4, 4, -4, 4, 1, -4, 4, 4, 4, 1],
        90 => &[1, 1, 4, 1, 4, 1, 4, 4, 4, -4, 4, 1, 4, 4, 4, 4, 1, 4, 4, 4, 4],
        140 => &[1, 1, 4, 1, 4, 1, 4, 4, 4, -4, -4, -4, -4, 1, 4, 4, 1],
        _ => return None,
    })
}

/// The octet holding the scanning mode, for the templates degrib returns in scanning mode 0100
/// (its `unpk_grib2`): their data is reordered to start at the south-west corner, rows first.
fn scan_octet(number: u16) -> Option<usize> {
    match number {
        0..=3 | 40..=43 => Some(72),
        10 | 12 => Some(60),
        20 | 30 | 31 => Some(65),
        90 => Some(64),
        110 => Some(57),
        _ => None,
    }
}

/// GRIB2's missing value of a signed 4-octet value.
const MISSING_S4: i64 = -2147483647;

/// A grid as degrib describes it (its `gdsType`), angles in degrees and distances in metres.
#[derive(Debug, Clone, Default)]
pub(super) struct Gds {
    pub(super) template: u16,
    pub(super) nx: usize,
    pub(super) ny: usize,
    /// The semi-major and semi-minor axes in km; negative when invalid; 0 when undefined.
    pub(super) maj_earth: f64,
    pub(super) min_earth: f64,
    pub(super) sphere: bool,
    pub(super) lat1: f64,
    pub(super) lon1: f64,
    pub(super) lat2: f64,
    pub(super) lon2: f64,
    pub(super) dx: f64,
    pub(super) dy: f64,
    /// The scanning mode, as degrib returns it (0100 for the templates it reorders).
    pub(super) scan: u8,
    /// The scanning mode of the file.
    pub(super) file_scan: u8,
    /// Whether the data is reordered from `file_scan` to start at the south-west corner.
    pub(super) reorder: bool,
    pub(super) mesh_lat: f64,
    pub(super) orient_lon: f64,
    pub(super) scale_lat1: f64,
    pub(super) scale_lat2: f64,
    pub(super) south_lat: f64,
    pub(super) south_lon: f64,
    pub(super) angle_rotate: f64,
    pub(super) lat_origin: f64,
    pub(super) central_meridian: f64,
    pub(super) x0: f64,
    pub(super) y0: f64,
    pub(super) x1: f64,
    pub(super) y2: f64,
}

/// The values of a section 3 by octet (degrib's `is3`: index `k` holds the value starting at
/// octet `k + 1`), template values read with their widths and signs.
fn is3(sec3: &[u8], map: &[i8]) -> Vec<i64> {
    let mut v = vec![0i64; sec3.len().max(15) + 8];
    let u = |i: usize, n: usize| -> u64 { (0..n).fold(0u64, |a, k| (a << 8) | u64::from(sec3.get(i + k).copied().unwrap_or(0))) };
    v[4] = u(4, 1) as i64;
    v[5] = u(5, 1) as i64;
    v[6] = u(6, 4) as i64;
    v[10] = u(10, 1) as i64;
    v[11] = u(11, 1) as i64;
    v[12] = u(12, 2) as i64;
    let mut at = 14;
    for &w in map {
        let n = w.unsigned_abs() as usize;
        if at >= v.len() {
            break;
        }
        let raw = u(at, n);
        v[at] = if w < 0 {
            let top = 1u64 << (8 * n - 1);
            let m = (raw & (top - 1)) as i64;
            if raw & top != 0 { -m } else { m }
        } else {
            (raw as u32) as i32 as i64
        };
        at += n;
    }
    v
}

/// degrib's `ParseSect3`.
pub(super) fn parse(sec3: &[u8]) -> Result<Gds> {
    let template = u16::from_be_bytes([sec3.get(12).copied().unwrap_or(0), sec3.get(13).copied().unwrap_or(0)]);
    let map = template_map(template).ok_or_else(|| Error::Unsupported(format!("GRIB2 grid template 3.{template}")))?;
    let need: usize = 14 + map.iter().map(|w| w.unsigned_abs() as usize).sum::<usize>();
    if sec3.len() < need {
        return Err(Error::InvalidData(format!("GRIB2 grid template 3.{template}: section 3 too short")));
    }
    let is3 = is3(sec3, map);
    let mut g = Gds { template, ..Default::default() };
    let invalid = |m: &str| Error::InvalidData(format!("GRIB2 grid: {m}"));
    // The shape of the earth (km).
    let mut earth_ok = true;
    match is3[14] {
        0 => (g.sphere, g.maj_earth, g.min_earth) = (true, 6367.47, 6367.47),
        6 => (g.sphere, g.maj_earth, g.min_earth) = (true, 6371.229, 6371.229),
        1 => {
            g.sphere = true;
            if is3[16] > 0 && is3[15] != 255 && is3[16] != MISSING_S4 {
                let r = is3[16] as f64 / (10f64.powi(is3[15] as i32) * 1000.0);
                g.maj_earth = if r < 6.4 { r * 1000.0 } else { r };
                g.min_earth = g.maj_earth;
            } else {
                earth_ok = false;
            }
        }
        2 => (g.sphere, g.maj_earth, g.min_earth) = (false, 6378.160, 6356.775),
        4 => (g.sphere, g.maj_earth, g.min_earth) = (false, 6378.137, 6378.137 * (1.0 - 1.0 / 298.257222101)),
        5 => (g.sphere, g.maj_earth, g.min_earth) = (false, 6378.137, 6378.137 * (1.0 - 1.0 / 298.257223563)),
        3 | 7 => {
            g.sphere = false;
            if is3[21] != MISSING_S4 && is3[20] != 255 && is3[26] != MISSING_S4 && is3[25] != 255 {
                // Shape 3 in km, 7 in m (each fixed when given in the other unit).
                let k = if is3[14] == 3 { 1.0 } else { 1000.0 };
                g.maj_earth = is3[21] as f64 / (10f64.powi(is3[20] as i32) * k);
                g.min_earth = is3[26] as f64 / (10f64.powi(is3[25] as i32) * k);
                for v in [&mut g.maj_earth, &mut g.min_earth] {
                    if is3[14] == 3 && *v > 6400.0 {
                        *v /= 1000.0;
                    } else if is3[14] == 7 && *v < 6.4 {
                        *v *= 1000.0;
                    }
                }
            } else {
                earth_ok = false;
            }
        }
        8 => (g.sphere, g.maj_earth, g.min_earth) = (true, 6371.2, 6371.2),
        s => return Err(invalid(&format!("undefined shape of the earth {s}"))),
    }
    if !earth_ok {
        // degrib leaves the axes at 0, which its check below makes invalid.
        (g.maj_earth, g.min_earth) = (0.0, 0.0);
    }
    if g.maj_earth > 6400.0 || g.maj_earth < 6300.0 || g.min_earth > 6400.0 || g.min_earth < 6300.0 {
        (g.maj_earth, g.min_earth) = (-1.0, -1.0);
    }
    g.nx = usize::try_from(is3[30]).map_err(|_| invalid("Nx"))?;
    g.ny = usize::try_from(is3[34]).map_err(|_| invalid("Ny"))?;
    if g.nx.checked_mul(g.ny) != Some(is3[6] as usize) {
        return Err(invalid("Nx * Ny is not the number of points"));
    }
    let unit = 1e-6;
    match template {
        0 | 1 | 40 => {
            let mut unit = unit;
            let (angle, subdivision) = (is3[38], is3[42]);
            if angle != 0 {
                if subdivision == 0 {
                    return Err(invalid("a subdivision of 0"));
                }
                unit = angle as f64 / subdivision as f64;
            }
            if [46, 50, 55, 59, 63, 67].iter().any(|&i| is3[i] == MISSING_S4) {
                return Err(invalid("lat/lon grid not defined completely"));
            }
            g.lat1 = is3[46] as f64 * unit;
            g.lon1 = is3[50] as f64 * unit;
            let res = is3[54] as u8;
            g.lat2 = is3[55] as f64 * unit;
            g.lon2 = is3[59] as f64 * unit;
            g.dx = is3[63] as f64 * unit;
            g.dy = if template == 40 {
                if is3[67] == 0 {
                    return Err(invalid("Gaussian grid without parallels"));
                }
                90.0 / is3[67] as f64
            } else {
                is3[67] as f64 * unit
            };
            g.file_scan = is3[71] as u8;
            if template == 1 {
                g.south_lat = is3[72] as f64 * unit;
                g.south_lon = is3[76] as f64 * unit;
                g.angle_rotate = is3[80] as f64 * unit;
            }
            if res & 32 != 0 && res & 16 == 0 {
                g.dy = g.dx;
            } else if res & 32 == 0 && res & 16 != 0 {
                g.dx = g.dy;
            }
        }
        10 => {
            if [38, 42, 47, 51, 55, 60].iter().any(|&i| is3[i] == MISSING_S4) {
                return Err(invalid("Mercator grid not defined completely"));
            }
            g.lat1 = is3[38] as f64 * unit;
            g.lon1 = is3[42] as f64 * unit;
            let res = is3[46] as u8;
            g.mesh_lat = is3[47] as f64 * unit;
            g.lat2 = is3[51] as f64 * unit;
            g.lon2 = is3[55] as f64 * unit;
            g.file_scan = is3[59] as u8;
            g.orient_lon = is3[60] as f64 * unit;
            g.dx = is3[64] as f64 / 1000.0;
            g.dy = is3[68] as f64 / 1000.0;
            if res & 32 != 0 && res & 16 == 0 {
                g.dy = g.dx;
            } else if res & 32 == 0 && res & 16 != 0 {
                g.dx = g.dy;
            }
        }
        12 => {
            g.lat_origin = is3[38] as f64 * unit;
            g.central_meridian = is3[42] as f64 * unit;
            // The scale factor, an IEEE float stored as a sign-and-magnitude integer.
            let s = is3[47];
            let bits = if s < 0 { ((-s) as u32) | 0x8000_0000 } else { s as u32 };
            g.scale_lat1 = f64::from(f32::from_bits(bits));
            g.x0 = is3[51] as f64 / 100.0;
            g.y0 = is3[55] as f64 / 100.0;
            g.file_scan = is3[59] as u8;
            g.dx = is3[60] as f64 / 100.0;
            g.dy = is3[64] as f64 / 100.0;
            g.x1 = is3[68] as f64 / 100.0;
            g.y2 = is3[80] as f64 / 100.0;
        }
        20 => {
            if [38, 42, 47, 51].iter().any(|&i| is3[i] == MISSING_S4) {
                return Err(invalid("polar stereographic grid not defined completely"));
            }
            g.lat1 = is3[38] as f64 * unit;
            g.lon1 = is3[42] as f64 * unit;
            g.mesh_lat = is3[47] as f64 * unit;
            g.orient_lon = is3[51] as f64 * unit;
            g.dx = is3[55] as f64 / 1000.0;
            g.dy = is3[59] as f64 / 1000.0;
            let center = is3[63] as u8;
            let pole = if center & 128 != 0 { -90.0 } else { 90.0 };
            (g.scale_lat1, g.scale_lat2) = (pole, pole);
            if center & 64 != 0 {
                return Err(invalid("bi-polar stereographic projections"));
            }
            g.file_scan = is3[64] as u8;
        }
        30 | 31 => {
            if [38, 42, 47, 51, 65, 69].iter().any(|&i| is3[i] == MISSING_S4) {
                return Err(invalid("conic grid not defined completely"));
            }
            g.lat1 = is3[38] as f64 * unit;
            g.lon1 = is3[42] as f64 * unit;
            g.mesh_lat = is3[47] as f64 * unit;
            g.orient_lon = is3[51] as f64 * unit;
            g.dx = is3[55] as f64 / 1000.0;
            g.dy = is3[59] as f64 / 1000.0;
            g.file_scan = is3[64] as u8;
            g.scale_lat1 = is3[65] as f64 * unit;
            g.scale_lat2 = is3[69] as f64 * unit;
        }
        90 => {
            g.lat1 = is3[38] as f64;
            g.lon1 = is3[42] as f64;
            g.dx = is3[47] as f64;
            g.dy = is3[51] as f64;
            g.file_scan = is3[63] as u8;
        }
        140 => {
            g.lat1 = is3[38] as f64 * unit;
            g.lon1 = is3[42] as f64 * unit;
            g.mesh_lat = is3[46] as f64 * unit;
            g.orient_lon = is3[50] as f64 * unit;
            g.dx = is3[55] as f64 / 1000.0;
            g.dy = is3[59] as f64 / 1000.0;
            g.file_scan = is3[63] as u8;
        }
        _ => unreachable!("templates without a map are rejected above"),
    }
    // degrib returns the data of the templates with a scanning mode in mode 0100.
    g.scan = g.file_scan;
    if scan_octet(template).is_some() {
        g.reorder = g.file_scan & 0xf0 != 64;
        g.scan = 64 + (g.file_scan & 0x0f);
    }
    Ok(g)
}

/// degrib's `ScanIndex2XY`: the column and row (from the south-west corner, rows first) of the
/// `i`-th value in scanning mode `scan`.
pub(super) fn scan_index_xy(i: usize, scan: u8, nx: usize, ny: usize) -> (usize, usize) {
    let (mut x, mut y);
    if scan & 32 != 0 {
        x = i / ny;
        y = if scan & 16 != 0 && x % 2 == 1 { ny - 1 - i % ny } else { i % ny };
    } else {
        y = i / nx;
        x = if scan & 16 != 0 && y % 2 == 1 { nx - 1 - i % nx } else { i % nx };
    }
    if scan & 128 != 0 {
        x = nx - 1 - x;
    }
    if scan & 64 == 0 {
        y = ny - 1 - y;
    }
    (x, y)
}

/// GDAL's `Lon360to180`.
fn lon_360_to_180(lon: f64) -> f64 {
    if lon == 180.0 { 180.0 } else { (lon + 180.0) % 360.0 - 180.0 }
}

/// A number in WKT.
fn n(v: f64) -> String {
    let s = format!("{v}");
    if s.contains('e') { format!("{v:.17}") } else { s }
}

/// The georeferencing of a grid, as GDAL's GRIB driver gives it.
#[derive(Debug, Clone)]
pub(super) struct Georef {
    /// The CRS: WKT, or a PROJ string for rotated grids; `None` when GDAL gives none.
    pub(super) crs: Option<String>,
    pub(super) geo_transform: [f64; 6],
    /// The column of the file's grid that the raster starts with (GDAL's
    /// `nSplitAndSwapColumn`): grids from 0 to 360 degrees are rewrapped at the antimeridian.
    pub(super) split_column: usize,
}

/// GDAL's `GRIBDataset::SetGribMetaData`: the CRS and geotransform of a grid.
pub(super) fn georef(g: &Gds) -> Georef {
    let (nx, ny) = (g.nx as f64, g.ny as f64);
    // The projection, without its geographic CRS: WKT1 PROJECTION and PARAMETERs.
    let proj: Option<String> = match g.template {
        0 | 40 | 1 => None,
        10 => Some(if g.orient_lon != 0.0 {
            // GDAL gives up on the CRS (and keeps the default geotransform).
            return Georef { crs: None, geo_transform: [0.0, 1.0, 0.0, 0.0, 0.0, 1.0], split_column: 0 };
        } else if g.mesh_lat == 0.0 {
            "PROJECTION[\"Mercator_1SP\"],PARAMETER[\"central_meridian\",0],PARAMETER[\"scale_factor\",1],PARAMETER[\"false_easting\",0],PARAMETER[\"false_northing\",0]".to_string()
        } else {
            format!(
                "PROJECTION[\"Mercator_2SP\"],PARAMETER[\"standard_parallel_1\",{}],PARAMETER[\"central_meridian\",0],PARAMETER[\"false_easting\",0],PARAMETER[\"false_northing\",0]",
                n(g.mesh_lat)
            )
        }),
        12 => {
            let k = if (g.scale_lat1 - 0.9996).abs() < 1e-8 { 0.9996 } else { g.scale_lat1 };
            Some(format!(
                "PROJECTION[\"Transverse_Mercator\"],PARAMETER[\"latitude_of_origin\",{}],PARAMETER[\"central_meridian\",{}],PARAMETER[\"scale_factor\",{}],PARAMETER[\"false_easting\",{}],PARAMETER[\"false_northing\",{}]",
                n(g.lat_origin),
                n(lon_360_to_180(g.central_meridian)),
                n(k),
                n(g.x0),
                n(g.y0)
            ))
        }
        20 => Some(format!(
            "PROJECTION[\"Polar_Stereographic\"],PARAMETER[\"latitude_of_origin\",{}],PARAMETER[\"central_meridian\",{}],PARAMETER[\"false_easting\",0],PARAMETER[\"false_northing\",0]",
            n(g.mesh_lat),
            n(g.orient_lon)
        )),
        30 => Some(format!(
            "PROJECTION[\"Lambert_Conformal_Conic_2SP\"],PARAMETER[\"latitude_of_origin\",{}],PARAMETER[\"central_meridian\",{}],PARAMETER[\"standard_parallel_1\",{}],PARAMETER[\"standard_parallel_2\",{}],PARAMETER[\"false_easting\",0],PARAMETER[\"false_northing\",0]",
            n(g.mesh_lat),
            n(lon_360_to_180(g.orient_lon)),
            n(g.scale_lat1),
            n(g.scale_lat2)
        )),
        31 => Some(format!(
            "PROJECTION[\"Albers_Conic_Equal_Area\"],PARAMETER[\"latitude_of_center\",{}],PARAMETER[\"longitude_of_center\",{}],PARAMETER[\"standard_parallel_1\",{}],PARAMETER[\"standard_parallel_2\",{}],PARAMETER[\"false_easting\",0],PARAMETER[\"false_northing\",0]",
            n(g.mesh_lat),
            n(lon_360_to_180(g.orient_lon)),
            n(g.scale_lat1),
            n(g.scale_lat2)
        )),
        // Hard-coded by GDAL: a full disk seen from a geostationary satellite at 0 degrees.
        90 => Some("PROJECTION[\"Geostationary_Satellite\"],PARAMETER[\"central_meridian\",0],PARAMETER[\"satellite_height\",35785831],PARAMETER[\"false_easting\",0],PARAMETER[\"false_northing\",0]".to_string()),
        140 => Some(format!(
            "PROJECTION[\"Lambert_Azimuthal_Equal_Area\"],PARAMETER[\"latitude_of_center\",{}],PARAMETER[\"longitude_of_center\",{}],PARAMETER[\"false_easting\",0],PARAMETER[\"false_northing\",0]",
            n(g.mesh_lat),
            n(lon_360_to_180(g.orient_lon))
        )),
        _ => None,
    };
    // The earth: GDAL's defaults (the Airy 1830 ellipsoid) without one; no CRS when invalid.
    let have_model = g.maj_earth > 0.0 && g.min_earth > 0.0;
    let a = if have_model { g.maj_earth * 1e3 } else { 6377563.396 };
    let b = if have_model {
        g.min_earth * 1e3
    } else if g.sphere {
        a
    } else {
        6356256.910
    };
    let invalid_earth = g.maj_earth < 0.0 || g.min_earth < 0.0;
    let geogcs = if g.sphere || a == b {
        format!(
            "GEOGCS[\"Coordinate System imported from GRIB file\",DATUM[\"unnamed\",SPHEROID[\"Sphere\",{},0]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]",
            n(a)
        )
    } else {
        let finv = a / (a - b);
        if (a - 6378137.0).abs() < 0.01 && (finv - 298.257223563).abs() < 1e-9 {
            if g.template == 0 {
                "EPSG:4326".to_string()
            } else {
                "GEOGCS[\"Coordinate System imported from GRIB file\",DATUM[\"WGS_1984\",SPHEROID[\"WGS 84\",6378137,298.257223563]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]".to_string()
            }
        } else if (a - 6378137.0).abs() < 0.01 && (finv - 298.257222101).abs() < 1e-9 {
            "GEOGCS[\"Coordinate System imported from GRIB file\",DATUM[\"unnamed\",SPHEROID[\"GRS80\",6378137,298.257222101]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]".to_string()
        } else {
            format!(
                "GEOGCS[\"Coordinate System imported from GRIB file\",DATUM[\"unnamed\",SPHEROID[\"Spheroid imported from GRIB file\",{},{}]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]",
                n(a),
                n(finv)
            )
        }
    };
    let crs = if invalid_earth {
        // GDAL refuses invalid spheroids (unless GRIB_USE_DEFAULT_SPHEROID is set): no CRS, and
        // the default geotransform.
        return Georef { crs: None, geo_transform: [0.0, 1.0, 0.0, 0.0, 0.0, 1.0], split_column: 0 };
    } else if g.template == 1 {
        // GDAL's pole rotation in the GRIB convention, as PROJ maps it to ob_tran.
        let ellps = if g.sphere || a == b { format!("+R={}", n(a)) } else { format!("+a={} +b={}", n(a), n(b)) };
        Some(format!(
            "+proj=ob_tran +o_proj=longlat +o_lat_p={} +o_lon_p={} +lon_0={} {ellps} +no_defs",
            n(-g.south_lat + 0.0),
            n(-g.angle_rotate + 0.0),
            n(lon_360_to_180(g.south_lon))
        ))
    } else {
        Some(match &proj {
            Some(p) => {
                let geog = if geogcs == "EPSG:4326" {
                    "GEOGCS[\"WGS 84\",DATUM[\"WGS_1984\",SPHEROID[\"WGS 84\",6378137,298.257223563]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]".to_string()
                } else {
                    geogcs.clone()
                };
                format!("PROJCS[\"unnamed\",{geog},{p},UNIT[\"Metre\",1]]")
            }
            None => geogcs.clone(),
        })
    };
    let (mut min_x, mut max_y, px, py);
    let mut split_column = 0;
    let mut crs = crs;
    if g.template == 90 {
        let extent = 11137496.552;
        min_x = -(extent / 2.0);
        max_y = extent / 2.0;
        px = extent / nx;
        py = extent / ny;
    } else if g.template == 12 {
        min_x = g.x1;
        max_y = g.y2;
        px = g.dx;
        py = g.dy;
    } else if proj.is_some() {
        // The first grid point, from longitude and latitude to the projection.
        let lon = lon_360_to_180(g.lon1);
        let projected = crs.as_deref().and_then(|wkt| {
            let geog_wkt = if geogcs == "EPSG:4326" {
                wkt.split_once(",PROJECTION").map(|(a, _)| a.trim_start_matches("PROJCS[\"unnamed\",").to_string())
            } else {
                Some(geogcs.clone())
            }?;
            let src = crate::crs::SpatialRef::from_wkt(&geog_wkt);
            let dst = crate::crs::SpatialRef::from_wkt(wkt);
            crate::crs::Transformer::new(&src, &dst).ok()?.transform(lon, g.lat1).ok()
        });
        match projected {
            Some((x, y)) => {
                min_x = x;
                max_y = if g.scan == 64 {
                    let mut southern_most = true;
                    // GDAL's heuristic for a JMA file whose first grid point is its northern-most.
                    if g.template == 30
                        && (g.scale_lat1 - 60.0).abs() <= 1e-8
                        && (g.scale_lat2 - 30.0).abs() <= 1e-8
                        && (g.mesh_lat - 30.0).abs() <= 1e-8
                        && (lon_360_to_180(g.orient_lon) - 140.0).abs() <= 1e-8
                        && let Some((cx, cy)) = crs.as_deref().and_then(|wkt| {
                            let src = crate::crs::SpatialRef::from_wkt(&geogcs);
                            let dst = crate::crs::SpatialRef::from_wkt(wkt);
                            crate::crs::Transformer::new(&src, &dst).ok()?.transform(140.0, 30.0).ok()
                        })
                    {
                        let nominal = (x + nx * g.dx / 2.0, y + ny * g.dy / 2.0);
                        let buggy = (nominal.0, y - ny * g.dy / 2.0);
                        let sq = |v: f64| v * v;
                        if sq(buggy.0 - cx) + sq(buggy.1 - cy) < sq(10.0) * (sq(nominal.0 - cx) + sq(nominal.1 - cy)) {
                            southern_most = false;
                        }
                    }
                    if southern_most { y + (ny - 1.0) * g.dy } else { y }
                } else {
                    y
                };
                px = g.dx;
                py = g.dy;
            }
            None => {
                min_x = 0.0;
                max_y = 0.0;
                px = 1.0;
                py = -1.0;
                crs = None;
            }
        }
    } else {
        // Lat/lon grids (rotated or not).
        min_x = g.lon1;
        max_y = g.lat1;
        let mut min_y = g.lat2;
        let max_x = g.lon2;
        if g.lat2 > max_y {
            max_y = g.lat2;
            min_y = g.lat1;
        }
        let mut sx = if g.nx == 1 {
            g.dx
        } else if g.lon1 > g.lon2 {
            (360.0 - (g.lon1 - g.lon2)) / (nx - 1.0)
        } else {
            (g.lon2 - g.lon1) / (nx - 1.0)
        };
        let mut sy = if g.ny == 1 { g.dy } else { (max_y - min_y) / (ny - 1.0) };
        if sx < 0.0 || (sx - g.dx).abs() > 0.002 {
            sx = g.dx;
        }
        if sy < 0.0 || (sy - g.dy).abs() > 0.002 {
            sy = g.dy;
        }
        // Longitudes from 0 to 360 shifted to -180 to 180 (GRIB_ADJUST_LONGITUDE_RANGE=YES).
        if min_x + sx >= 180.0 || max_x - sx >= 180.0 {
            if sx * nx > 360.0 + sx / 4.0 {
                // Overlapping global grids are kept as they are.
            } else if min_x == 180.0 {
                min_x = -180.0;
            } else if (360.0 - sx * nx).abs() < sx / 4.0 && min_x <= 180.0 && g.template == 0 {
                let candidate = ((180.0 - min_x) / sx).ceil() as usize;
                if candidate < g.nx {
                    split_column = candidate;
                    min_x = -180.0;
                }
            } else {
                // Spanning the antimeridian or not, the first longitude from -180 to 180.
                min_x = lon_360_to_180(min_x);
            }
        }
        px = sx;
        py = sy;
    }
    // The corner of the first pixel, rather than its center.
    min_x -= px / 2.0;
    max_y += py / 2.0;
    Georef { crs, geo_transform: [min_x, px, 0.0, max_y, 0.0, -py], split_column }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_scan_indices_as_degrib() {
        // Mode 0000 (rows west to east, north to south) on a 3 x 2 grid.
        assert_eq!(scan_index_xy(0, 0, 3, 2), (0, 1));
        assert_eq!(scan_index_xy(4, 0, 3, 2), (1, 0));
        // Mode 0100 (south to north): as is.
        assert_eq!(scan_index_xy(4, 64, 3, 2), (1, 1));
        // Columns first (0010 0000), boustrophedon (0001 0000).
        assert_eq!(scan_index_xy(1, 64 | 32, 3, 2), (0, 1));
        assert_eq!(scan_index_xy(3, 64 | 16, 3, 2), (2, 1));
        assert_eq!(lon_360_to_180(359.5), -0.5);
        assert_eq!(lon_360_to_180(180.0), 180.0);
    }
}
