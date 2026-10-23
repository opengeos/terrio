//! GRIB1 messages, read as GDAL's GRIB driver reads them (degrib's `degrib1.cpp`): the product
//! definition (parameter tables of NCEP, ECMWF, DWD, CPTEC and others, level types, times), the
//! grid (lat/lon, Gaussian, rotated lat/lon, polar stereographic, Lambert conformal, Mercator),
//! the bitmap and simple packing, in double precision.

use super::grid::{self, Gds, Georef};
use super::tables::{self, UnitConvert};
use super::{Band, Decoded, MessageReader};
use crate::{Error, Result};

/// degrib's missing value of GRIB1 bitmaps, and the one it replaces it with when it can.
const UNDEFINED: f64 = 9.999e20;
const UNDEFINED_PRIM: f64 = 9999.0;

/// A GRIB1 message's description.
#[derive(Debug, Clone)]
pub(super) struct Field {
    pub(super) gds: Gds,
    /// The offsets of the bitmap (when there is one) and data sections in the message.
    bms: Option<usize>,
    bds: usize,
    dsf: i32,
    convert: UnitConvert,
}

fn u2(b: &[u8], i: usize) -> u32 {
    (u32::from(b.get(i).copied().unwrap_or(0)) << 8) | u32::from(b.get(i + 1).copied().unwrap_or(0))
}

fn u3(b: &[u8], i: usize) -> u32 {
    (u2(b, i) << 8) | u32::from(b.get(i + 2).copied().unwrap_or(0))
}

/// GRIB1's sign-and-magnitude integers of 2 and 3 octets.
fn s2(b: &[u8], i: usize) -> i32 {
    let v = u2(b, i);
    let m = (v & 0x7fff) as i32;
    if v & 0x8000 != 0 { -m } else { m }
}

fn s3(b: &[u8], i: usize) -> i32 {
    let v = u3(b, i);
    let m = (v & 0x7f_ffff) as i32;
    if v & 0x80_0000 != 0 { -m } else { m }
}

/// degrib's `fval_360`: an IBM 360 single-precision float.
fn fval_360(v: u32) -> f64 {
    let exp = i32::from(((v >> 24) & 0x7f) as u8) - 64;
    let mantissa = f64::from(v & 0xff_ffff) / f64::from(0x100_0000);
    let x = mantissa * 16f64.powi(exp);
    if v & 0x8000_0000 != 0 { -x } else { x }
}

/// The days from 1970-01-01 to a date of the proleptic Gregorian calendar.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// degrib's `ParseSect4Time2secV1`.
fn time_v1(t: i64, unit: u8) -> Option<f64> {
    const UNIT2SEC: [i64; 13] = [60, 3600, 86400, 0, 0, 0, 0, 0, 0, 0, 10800, 21600, 43200];
    match unit {
        254 => Some(t as f64),
        u if (u as usize) < UNIT2SEC.len() && UNIT2SEC[u as usize] != 0 => Some((t * UNIT2SEC[u as usize]) as f64),
        _ => None,
    }
}

/// The product definition section (section 1), as degrib's `ReadGrib1Sect1` parses it.
struct Pds {
    table_version: u8,
    center: u16,
    subcenter: u16,
    gen_process: u8,
    has_gds: bool,
    has_bms: bool,
    cat: u8,
    level_type: u8,
    level: u32,
    time_range: u8,
    ref_time: f64,
    valid_time: f64,
    dsf: i32,
}

fn pds(p: &[u8]) -> Result<Pds> {
    if p.len() < 28 {
        return Err(Error::InvalidData("GRIB1 product definition section too short".into()));
    }
    let len = u3(p, 0) as usize;
    let year_of_century = i64::from(p[12]);
    let century = i64::from(p[24]);
    let mut year = if year_of_century == 0 { century * 100 } else { year_of_century + (century - 1) * 100 };
    if !(1900..=2100).contains(&year) {
        year += 2000;
    }
    let (mon, day, hour, min) = (p[13], p[14], p[15], p[16]);
    if mon > 12 || day == 0 || day > 31 || hour > 24 || min > 60 {
        return Err(Error::InvalidData(format!("GRIB1: invalid reference time {mon}/{day} {hour}:{min}")));
    }
    let ref_time =
        days_from_civil(year, i64::from(mon), i64::from(day)) as f64 * 86400.0 + f64::from(hour) * 3600.0 + f64::from(min) * 60.0;
    let unit = p[17];
    let time_range = p[20];
    let p1 = ref_time + time_v1(i64::from(p[18]), unit).unwrap_or(0.0);
    let p2 = ref_time + time_v1(i64::from(p[19]), unit).unwrap_or(0.0);
    let valid_time = match time_range {
        2..=5 | 51 => p2,
        10 => ref_time + time_v1(i64::from(u2(p, 18)), unit).unwrap_or(0.0),
        _ => p1,
    };
    let center = u16::from(p[4]);
    let subcenter = u16::from(p[25]);
    let mut cat = p[8];
    // NCEP's ensemble probabilities: the parameter is in the extension.
    if center == 7 && subcenter == 2 && len >= 45 && matches!(cat, 191..=193) && len >= 60 {
        cat = p[45];
    }
    Ok(Pds {
        table_version: p[3],
        center,
        subcenter,
        gen_process: p[5],
        has_gds: p[7] & 128 != 0,
        has_bms: p[7] & 64 != 0,
        cat,
        level_type: p[9],
        level: u2(p, 10),
        time_range,
        ref_time,
        valid_time,
        dsf: s2(p, 26),
    })
}

/// degrib's `Choose_ParmTable`: the name of the parameter table of a message.
fn parm_table(p: &Pds) -> &'static str {
    let (v, sub) = (p.table_version, p.subcenter);
    match p.center {
        7 => {
            if v <= 3 {
                match sub {
                    1 => return "parm_table_ncep_reanal",
                    11 => return "parm_table_ncep_tdl",
                    14 => return "parm_table_ncep_mdl",
                    _ => {}
                }
            }
            match v {
                0 | 3 => return "parm_table_ncep_opn",
                1 | 2 => {
                    return if sub != 0 || (p.gen_process != 80 && p.gen_process != 180) {
                        "parm_table_ncep_opn"
                    } else {
                        "parm_table_ncep_reanal"
                    };
                }
                128 => return "parm_table_omb",
                129 => return "parm_table_nceptab_129",
                130 => return "parm_table_nceptab_130",
                131 => return "parm_table_nceptab_131",
                133 => return "parm_table_nceptab_133",
                140 => return "parm_table_nceptab_140",
                141 => return "parm_table_nceptab_141",
                _ => {}
            }
        }
        57 => match sub {
            0 => return "parm_table_afwa_000",
            1 | 4 => return "parm_table_afwa_001",
            2 => return "parm_table_afwa_002",
            3 => return "parm_table_afwa_003",
            10 => return "parm_table_afwa_010",
            11 => return "parm_table_afwa_011",
            _ => {}
        },
        98 => {
            if let Some(t) = match v {
                128 => Some("parm_table_ecmwf_128"),
                129 => Some("parm_table_ecmwf_129"),
                130 => Some("parm_table_ecmwf_130"),
                131 => Some("parm_table_ecmwf_131"),
                140 => Some("parm_table_ecmwf_140"),
                150 => Some("parm_table_ecmwf_150"),
                160 => Some("parm_table_ecmwf_160"),
                170 => Some("parm_table_ecmwf_170"),
                180 => Some("parm_table_ecmwf_180"),
                210 => Some("parm_table_ecmwf_210"),
                215 => Some("parm_table_ecmwf_215"),
                217 => Some("parm_table_ecmwf_217"),
                218 => Some("parm_table_ecmwf_218"),
                228 => Some("parm_table_ecmwf_228"),
                _ => None,
            } {
                return t;
            }
        }
        78 => {
            if let Some(t) = match v {
                2 => Some("parm_table_dwd_002"),
                201 => Some("parm_table_dwd_201"),
                202 => Some("parm_table_dwd_202"),
                203 => Some("parm_table_dwd_203"),
                _ => None,
            } {
                return t;
            }
        }
        46 if v == 254 => return "parm_table_cptec_254",
        9 => match sub {
            163 => return "parm_table_nohrsc",
            161 => return "parm_table_ncep_opn",
            _ => {}
        },
        96 => return "parm_table_athens",
        88 if v == 128 => return "parm_table_norway128",
        54 => return "parm_table_cmc",
        _ => {}
    }
    "parm_table_undefined"
}

/// degrib's `GRIB1_Table2LookUp`: the parameter's name, comment, unit and unit conversion.
fn parameter(p: &Pds) -> (String, String, String, UnitConvert) {
    if p.center == 7 && p.table_version == 129 && p.cat == 180 && p.time_range == 3 {
        return ("AVGOZCON".into(), "Average Ozone Concentration".into(), "PPB".into(), UnitConvert::None);
    }
    let t = tables::tables();
    let Some(e) = t.grib1.get(parm_table(p)).and_then(|table| table.get(p.cat as usize)) else {
        return (format!("var{}", p.cat), "undefined".into(), "-".into(), UnitConvert::None);
    };
    let mut name = e.name.clone();
    if name == format!("var{}", p.cat) {
        name = if p.center == 98 {
            format!("var{} of table {} of center ECMWF", p.cat, p.table_version)
        } else {
            format!("var{} of table {} of center {}", p.cat, p.table_version, p.center)
        };
    }
    (name, e.comment.clone(), e.unit.clone(), e.convert)
}

/// degrib's `GRIB1_Table3LookUp`: the short and long names of the level.
fn level_names(p: &Pds) -> (String, String) {
    let t = tables::tables();
    let Some(s) = t.grib1_surface.get(p.level_type as usize) else { return (String::new(), String::new()) };
    if s.two_part {
        let (l1, l2) = (p.level >> 8, p.level & 0xff);
        (format!("{l1}-{l2}-{}", s.name), format!("{l1}-{l2}[{}] {} ({})", s.unit, s.name, s.comment))
    } else {
        (format!("{}-{}", p.level, s.name), format!("{}[{}] {} ({})", p.level, s.unit, s.name, s.comment))
    }
}

/// degrib's `ReadGrib1Sect2`: the grid, as a GRIB2-like description.
fn gds(g: &[u8], center: u16) -> Result<Gds> {
    let bad = |m: &str| Error::InvalidData(format!("GRIB1 grid: {m}"));
    if g.len() < 6 {
        return Err(bad("section too short"));
    }
    let len = u3(g, 0) as usize;
    let grid_type = g[5];
    let unit = 1e-3;
    let mut d = Gds::default();
    let earth = |d: &mut Gds, res: u8| {
        if res & 0x40 != 0 {
            (d.sphere, d.maj_earth, d.min_earth) = (false, 6378.160, 6356.775);
        } else {
            (d.sphere, d.maj_earth, d.min_earth) = (true, 6367.47, 6367.47);
        }
    };
    match grid_type {
        0 | 4 | 10 => {
            if (grid_type == 10 && len < 42) || len < 32 {
                return Err(bad("lat/lon grid section too short"));
            }
            d.template = match grid_type {
                4 => 40,
                10 => 1,
                _ => 0,
            };
            d.nx = u2(g, 6) as usize;
            if d.nx == 65535 {
                return Err(Error::Unsupported("GRIB1 quasi-regular grids (rows of varying lengths)".into()));
            }
            d.ny = u2(g, 8) as usize;
            d.lat1 = f64::from(s3(g, 10)) * unit;
            d.lon1 = f64::from(s3(g, 13)) * unit;
            earth(&mut d, g[16]);
            d.lat2 = f64::from(s3(g, 17)) * unit;
            d.lon2 = f64::from(s3(g, 20)) * unit;
            d.dx = f64::from(u2(g, 23)) * unit;
            d.dy = if grid_type == 4 {
                let np = u2(g, 25);
                if np == 0 {
                    return Err(bad("Gaussian grid without parallels"));
                }
                90.0 / f64::from(np)
            } else {
                f64::from(u2(g, 25)) * unit
            };
            d.file_scan = g[27];
            if grid_type == 10 && len >= 42 {
                // degrib checks the 10 octets from the scanning mode.
                let ext = g.get(27..37).unwrap_or_default();
                if !ext.iter().all(|b| *b == 0) && !ext.iter().all(|b| *b == 255) {
                    d.south_lat = f64::from(s3(g, 32)) * unit;
                    d.south_lon = f64::from(s3(g, 35)) * unit;
                    d.angle_rotate = fval_360(u2(g, 38) << 16 | u2(g, 40));
                }
            }
        }
        5 => {
            if len < 32 {
                return Err(bad("polar stereographic grid section too short"));
            }
            d.template = 20;
            d.nx = u2(g, 6) as usize;
            d.ny = u2(g, 8) as usize;
            d.lat1 = f64::from(s3(g, 10)) * unit;
            d.lon1 = f64::from(s3(g, 13)) * unit;
            earth(&mut d, g[16]);
            d.orient_lon = f64::from(s3(g, 17)) * unit;
            d.dx = f64::from(s3(g, 20));
            d.dy = f64::from(s3(g, 23));
            d.mesh_lat = 60.0;
            let c = g[26];
            if c & 128 != 0 {
                (d.scale_lat1, d.scale_lat2) = (-90.0, -90.0);
                d.mesh_lat = -60.0;
            } else {
                (d.scale_lat1, d.scale_lat2) = (90.0, 90.0);
            }
            d.file_scan = g[27];
        }
        3 => {
            if len < 42 {
                return Err(bad("Lambert conformal grid section too short"));
            }
            d.template = 30;
            d.nx = u2(g, 6) as usize;
            d.ny = u2(g, 8) as usize;
            d.lat1 = f64::from(s3(g, 10)) * unit;
            d.lon1 = f64::from(s3(g, 13)) * unit;
            earth(&mut d, g[16]);
            d.orient_lon = f64::from(s3(g, 17)) * unit;
            d.dx = f64::from(s3(g, 20));
            d.dy = f64::from(s3(g, 23));
            d.file_scan = g[27];
            d.scale_lat1 = f64::from(s3(g, 28)) * unit;
            d.scale_lat2 = f64::from(s3(g, 31)) * unit;
            d.mesh_lat = d.scale_lat1;
            d.south_lat = f64::from(s3(g, 34)) * unit;
            d.south_lon = f64::from(s3(g, 37)) * unit;
        }
        1 => {
            if len < 42 {
                return Err(bad("Mercator grid section too short"));
            }
            d.template = 10;
            d.nx = u2(g, 6) as usize;
            d.ny = u2(g, 8) as usize;
            d.lat1 = f64::from(s3(g, 10)) * unit;
            d.lon1 = f64::from(s3(g, 13)) * unit;
            earth(&mut d, g[16]);
            d.lat2 = f64::from(s3(g, 17)) * unit;
            d.lon2 = f64::from(s3(g, 20)) * unit;
            d.scale_lat1 = f64::from(s3(g, 23)) * unit;
            d.scale_lat2 = d.scale_lat1;
            d.mesh_lat = d.scale_lat1;
            d.file_scan = g[27];
            d.dx = f64::from(s3(g, 28));
            d.dy = f64::from(s3(g, 31));
        }
        t => return Err(Error::Unsupported(format!("GRIB1 grid type {t}"))),
    }
    // NCEP's sphere (6371.2 km) rather than GRIB1's.
    if center == 7 && d.maj_earth == 6367.47 {
        (d.sphere, d.maj_earth, d.min_earth) = (true, 6371.2, 6371.2);
    }
    d.scan = d.file_scan;
    d.reorder = d.file_scan & 0xe0 != 0x40;
    Ok(d)
}

/// The band of a GRIB1 message at `offset` (of `len` octets).
pub(super) fn bands(r: &mut MessageReader, offset: u64, len: u64) -> Result<Vec<Band>> {
    let normalize = super::grib2::normalize_units();
    let head = r.bytes(offset + 8, 3)?;
    let pds_len = u3(head, 0) as usize;
    if 8 + pds_len > len as usize || pds_len < 3 {
        return Err(Error::InvalidData(format!("GRIB1 message at {offset}: invalid product definition section")));
    }
    let p = pds(r.bytes(offset + 8, pds_len)?)?;
    let (name, comment, unit, convert) = parameter(&p);
    let (short_level, long_level) = level_names(&p);
    let mut pos = 8 + pds_len;
    if !p.has_gds {
        return Err(Error::Unsupported(format!("GRIB1 message at {offset} without a grid definition (predefined grids)")));
    }
    let gl = u3(r.bytes(offset + pos as u64, 3)?, 0) as usize;
    let g = gds(r.bytes(offset + pos as u64, gl.min(len as usize - pos))?, p.center)?;
    pos += gl;
    let bms = if p.has_bms {
        let bl = u3(r.bytes(offset + pos as u64, 3)?, 0) as usize;
        let at = pos;
        pos += bl;
        Some(at)
    } else {
        None
    };
    if pos + 11 > len as usize {
        return Err(Error::InvalidData(format!("GRIB1 message at {offset}: no data section")));
    }
    let unit_text = format!("[{unit}]");
    let comment = format!("{comment} [{unit}]");
    let conv = |s: &str| if normalize { s.replacen("[K]", "[C]", 1) } else { s.to_string() };
    let items = vec![
        ("GRIB_UNIT".to_string(), conv(&unit_text)),
        ("GRIB_COMMENT".to_string(), conv(&comment)),
        ("GRIB_ELEMENT".to_string(), name),
        ("GRIB_SHORT_NAME".to_string(), short_level),
        ("GRIB_REF_TIME".to_string(), format!("{:.0}", p.ref_time)),
        ("GRIB_VALID_TIME".to_string(), format!("{:.0}", p.valid_time)),
        ("GRIB_FORECAST_SECONDS".to_string(), format!("{:.0}", p.valid_time - p.ref_time)),
    ];
    let field = Field { gds: g.clone(), bms, bds: pos, dsf: p.dsf, convert };
    // degrib's missing value where the bitmap masks points: 9999 unless valid values reach it.
    let nodata = if p.has_bms {
        let msg = r.bytes(offset, len as usize)?.to_vec();
        // A message terrio cannot decode has none (and fails when read).
        decode_values(&msg, &field, normalize, false).ok().map(|(_, missing)| missing)
    } else {
        None
    };
    Ok(vec![Band { offset, len, items, description: long_level, nodata, size: (g.nx, g.ny), gds2: None, grib1: Some(field) }])
}

/// Decodes a GRIB1 message (`msg`, the whole message) as degrib does: values from the south-west
/// corner, rows first, in double precision.
pub(super) fn decode(msg: &[u8], f: &Field, normalize: bool) -> Result<Decoded> {
    let (values, _) = decode_values(msg, f, normalize, true)?;
    Ok(Decoded { nx: f.gds.nx, ny: f.gds.ny, values })
}

/// degrib's `ReadGrib1Sect4` (and `ReadGrib1Sect3`): the values (when `values` is set) and the
/// missing value of masked points.
fn decode_values(msg: &[u8], f: &Field, normalize: bool, values: bool) -> Result<(Vec<f64>, f64)> {
    let bad = |m: &str| Error::InvalidData(format!("GRIB1 data: {m}"));
    let (nx, ny) = (f.gds.nx, f.gds.ny);
    let npts = nx * ny;
    // The bitmap.
    let bitmap: Option<Vec<bool>> = match f.bms {
        Some(at) => {
            let b = msg.get(at..).ok_or_else(|| bad("no bitmap section"))?;
            let len = u3(b, 0) as usize;
            let unused = usize::from(b.get(3).copied().unwrap_or(0));
            if npts + unused + 48 != len * 8 {
                return Err(bad("bitmap of another size than the grid"));
            }
            if u2(b, 4) != 0 {
                return Err(Error::Unsupported("GRIB1 predefined bitmaps".into()));
            }
            let bits = b.get(6..len).ok_or_else(|| bad("bitmap truncated"))?;
            Some((0..npts).map(|i| bits[i / 8] & (0x80 >> (i % 8)) != 0).collect())
        }
        None => None,
    };
    let b = msg.get(f.bds..).ok_or_else(|| bad("no data section"))?;
    if b.len() < 11 {
        return Err(bad("data section too short"));
    }
    let flag = b[3];
    if flag & 128 != 0 {
        return Err(Error::Unsupported("GRIB1 spherical harmonics".into()));
    }
    if flag & 64 != 0 {
        return Err(Error::Unsupported("GRIB1 complex packing".into()));
    }
    let esf = s2(b, 4);
    let ref_val = fval_360(u3(b, 6) << 8 | u32::from(b[9]));
    let nbits = u32::from(b[10]);
    if nbits > 32 {
        return Err(bad("more than 32 bits per value"));
    }
    let data = &b[11..(u3(b, 0) as usize).min(b.len()).max(11)];
    let (m, add) = if normalize { f.convert.metric().unwrap_or((1.0, 0.0)) } else { (1.0, 0.0) };
    let convert = |v: f64| if m == -10.0 { 10f64.powf(v) } else { m * v + add };
    let p10 = 10f64.powi(f.dsf);
    let p2 = 2f64.powi(esf);
    let min = if m == -10.0 { 10f64.powf(ref_val * p2 / p10) } else { m * (ref_val * p2 / p10) + add };
    let mut max = min;
    let index = |i: usize| -> usize {
        if f.gds.reorder {
            let (x, y) = grid::scan_index_xy(i, f.gds.file_scan, nx, ny);
            x + y * nx
        } else {
            i
        }
    };
    let mut out = if values { vec![0f64; npts] } else { Vec::new() };
    let mut bit = 0usize;
    let mut next = || -> Result<f64> {
        if bit + nbits as usize > data.len() * 8 {
            return Err(bad("data section truncated"));
        }
        let mut v: u64 = 0;
        for k in 0..nbits as usize {
            let p = bit + k;
            v = (v << 1) | u64::from((data[p / 8] >> (7 - p % 8)) & 1);
        }
        bit += nbits as usize;
        Ok(convert((ref_val + v as f64 * p2) / p10))
    };
    match &bitmap {
        Some(bm) => {
            for (i, set) in bm.iter().enumerate() {
                let v = if !set {
                    UNDEFINED
                } else if nbits != 0 {
                    let v = next()?;
                    max = max.max(v);
                    v
                } else {
                    min
                };
                if values {
                    out[index(i)] = v;
                }
            }
            let reset = if max < UNDEFINED_PRIM || min > UNDEFINED_PRIM {
                UNDEFINED_PRIM
            } else if max >= UNDEFINED && min <= UNDEFINED {
                max + 1.0
            } else {
                0.0
            };
            let missing = if reset != 0.0 { reset } else { UNDEFINED };
            if values && reset != 0.0 {
                for (i, set) in bm.iter().enumerate() {
                    if !set {
                        out[index(i)] = reset;
                    }
                }
            }
            Ok((out, missing))
        }
        None => {
            if values {
                for i in 0..npts {
                    if nbits != 0 {
                        out[index(i)] = next()?;
                    } else {
                        out[i] = min;
                    }
                }
            }
            Ok((out, UNDEFINED))
        }
    }
}

/// The georeferencing of a GRIB1 grid, as GDAL gives it.
pub(super) fn georef(f: &Field) -> Georef {
    grid::georef(&f.gds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_ibm_floats() {
        assert_eq!(fval_360(0x4110_0000), 1.0);
        assert_eq!(fval_360(0xc276_a000), -118.625);
        assert_eq!(fval_360(0), 0.0);
    }
}
