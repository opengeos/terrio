//! GRIB1 messages, read as GDAL 3.12's GRIB driver reads them: degrib's `degrib1.cpp`
//! (`ReadGrib1Record` and its sections), `grib1tab.cpp` (parameter and level tables, in
//! `grib1.tsv.zst`, from `gen_grib1_tables.py`) and `inventory.cpp` (`GRIB1_Inventory`).
//!
//! Values are unpacked in double precision, `(R + X * 2^E) / 10^D` with the reference value an IBM
//! single, converted from Kelvin to Celsius for temperatures (`GRIB_NORMALIZE_UNITS=YES`), and
//! reordered to scanning mode 0100. Points a bitmap masks hold degrib's missing value, which is
//! the band's nodata: 9999 when the data stays clear of it, else 9.999e20 (or the maximum plus
//! one). Grids: latitude/longitude (0), Gaussian (4), rotated latitude/longitude (10), polar
//! stereographic (5), Lambert conformal (3) and Mercator (1), with the shape of the earth GDAL
//! assigns them. Like GDAL, not read: complex (second order) packing, spherical harmonics,
//! predefined grids and bitmaps, quasi-regular grids.

use super::{BitReader, Decoded, Grid, scan_index_to_xy, try_filled};
use crate::{BandMetadata, Error, Result};
use std::collections::HashMap;
use std::sync::OnceLock;

static DATA: &[u8] = include_bytes!("grib1.tsv.zst");

/// degrib's missing value before it is moved (`UNDEFINED`), and the one it prefers.
const UNDEFINED: f64 = 9.999e20;
const UNDEFINED_PRIM: f64 = 9999.0;
const NMC: u8 = 7;
const ECMWF: u8 = 98;

fn bad(m: &str) -> Error {
    Error::InvalidData(format!("GRIB1: {m}"))
}

/// Octets `o..o + n` (1-based) of a section, unsigned.
fn u(s: &[u8], o: usize, n: usize) -> u32 {
    s.get(o - 1..o - 1 + n).map_or(0, |b| b.iter().fold(0u32, |v, &x| (v << 8) | x as u32))
}

/// Octets `o..o + n` of a section, sign and magnitude (`GRIB_SIGN_INT2`, `GRIB_SIGN_INT3`).
fn s(b: &[u8], o: usize, n: usize) -> i32 {
    let v = u(b, o, n) as i32;
    let sign = 1i32 << (8 * n - 1);
    if v & sign != 0 { -(v & (sign - 1)) } else { v }
}

/// degrib's `fval_360`: an IBM single precision number.
fn ibm(v: u32) -> f64 {
    let pow16 = 2f64.powi(4 * ((v >> 24) & 0x7f) as i32 - 256);
    let pow16 = if v & 0x8000_0000 != 0 { -pow16 } else { pow16 };
    pow16 * (v & 0xff_ffff) as f64 / 16_777_216.0
}

/// The sections of a GRIB1 message. `msg` is the whole message, or (for the header of a remote
/// file) as much of its start as was read: the bitmap and data may then be cut short.
pub(crate) struct Message<'a> {
    /// The length of the file the message is in.
    pub file_len: usize,
    pub pds: &'a [u8],
    pub gds: &'a [u8],
    pub bms: Option<&'a [u8]>,
    /// The data section, to the end of the message (degrib reads past a short section).
    pub bds: &'a [u8],
    /// Whether units are normalised (`GRIB_NORMALIZE_UNITS`): Kelvin to Celsius.
    pub normalize: bool,
}

/// The length of the GRIB1 message starting at `msg` ("GRIB", then 3 octets).
pub(crate) fn message_len(msg: &[u8]) -> Result<usize> {
    let len = u(msg, 5, 3) as usize;
    if len < 52 {
        return Err(bad(&format!("message length {len} below 52")));
    }
    Ok(len)
}

impl<'a> Message<'a> {
    /// Splits a message into its sections, as `ReadGrib1Record` walks them.
    pub(crate) fn parse(msg: &'a [u8], file_len: usize) -> Result<Message<'a>> {
        let len = message_len(msg)?;
        let short = || bad("message cut short");
        let mut cur = 8;
        let pds_len = u(msg, cur + 1, 3) as usize;
        if pds_len < 28 || cur + pds_len > len {
            return Err(bad("invalid product definition section"));
        }
        let pds = msg.get(cur..cur + pds_len).ok_or_else(short)?;
        cur += pds_len;
        let flags = pds[7];
        if flags & 0x80 == 0 {
            return Err(Error::Unsupported("GRIB1 messages without a grid definition (predefined grids) are not supported".into()));
        }
        if len - cur < 6 {
            return Err(bad("ran out of data in the grid definition section"));
        }
        let gds_len = u(msg, cur + 1, 3) as usize;
        if gds_len < 6 || cur + gds_len > len {
            return Err(bad("invalid grid definition section"));
        }
        let gds = msg.get(cur..cur + gds_len).ok_or_else(short)?;
        cur += gds_len;
        let mut bms = None;
        if flags & 0x40 != 0 {
            if len - cur < 6 {
                return Err(bad("ran out of data in the bitmap section"));
            }
            let bms_len = u(msg, cur + 1, 3) as usize;
            if bms_len < 6 || cur + bms_len > len {
                return Err(bad("invalid bitmap section"));
            }
            bms = Some(msg.get(cur..(cur + bms_len).min(msg.len())).ok_or_else(short)?);
            cur += bms_len;
        }
        if cur >= len || len - cur < 11 {
            return Err(bad("ran out of data in the binary data section"));
        }
        let bds_len = u(msg, cur + 1, 3) as usize;
        if cur + bds_len > len {
            return Err(bad("invalid binary data section"));
        }
        let bds = msg.get(cur..len.min(msg.len())).ok_or_else(short)?;
        if bds.len() < 11 {
            return Err(short());
        }
        Ok(Message { file_len, pds, gds, bms, bds, normalize: true })
    }

    /// How much of the start of a message its header needs (up to the data section's first 11
    /// octets), given the `head` at hand: at least the returned count of bytes.
    pub(crate) fn header_needs(head: &[u8]) -> Option<usize> {
        let mut cur = 8 + u(head, 9, 3) as usize;
        if head.len() < cur + 6 {
            return Some(cur + 6);
        }
        cur += u(head, cur + 1, 3) as usize;
        if head[15] & 0x40 != 0 {
            if head.len() < cur + 6 {
                return Some(cur + 6);
            }
            cur += u(head, cur + 1, 3) as usize;
        }
        Some(cur + 11)
    }

    fn center(&self) -> u8 {
        self.pds[4]
    }

    fn subcenter(&self) -> u8 {
        self.pds[25]
    }

    fn table_version(&self) -> u8 {
        self.pds[3]
    }

    /// The decimal scale factor `D`.
    fn dsf(&self) -> i32 {
        s(self.pds, 27, 2)
    }

    /// The parameter (octet 9), or for NCEP's ensemble probabilities, the parameter they are of.
    fn parameter(&self) -> u8 {
        let p = self.pds;
        let cat = p[8];
        if self.center() == NMC && self.subcenter() == 2 && p.len() >= 60 && matches!(cat, 191..=193) {
            return p[45];
        }
        cat
    }

    /// The grid, as `ReadGrib1Sect2` reads it, in terms of the GRIB2 templates GDAL maps it to.
    pub(crate) fn grid(&self) -> Result<Grid> {
        let g = self.gds;
        let typ = g.get(5).copied().unwrap_or(255);
        let unit = 1e-3;
        let need = match typ {
            0 | 4 => 32,
            10 | 3 | 1 => 42,
            5 => 32,
            _ => return Err(Error::Unsupported(format!("GRIB1 grid type {typ} is not supported"))),
        };
        if g.len() < need {
            return Err(bad(&format!("grid definition of type {typ} too short")));
        }
        let (nx, ny) = (u(g, 7, 2), u(g, 9, 2));
        if typ != 5 && typ != 3 && typ != 1 && nx == 65535 {
            return Err(Error::Unsupported("quasi-regular GRIB1 grids are not supported".into()));
        }
        if nx == 0 || ny == 0 {
            return Err(bad("invalid grid dimension"));
        }
        // Resolution flags: an oblate earth (IAU 1965) or a sphere; NCEP's sphere is 6371.2 km.
        let oblate = g[16] & 0x40 != 0;
        let (mut sphere, mut maj, mut min) = if oblate { (false, 6378.160, 6356.775) } else { (true, 6367.47, 6367.47) };
        if self.center() == NMC && maj == 6367.47 {
            (sphere, maj, min) = (true, 6371.2, 6371.2);
        }
        let mut grid = Grid::new(nx, ny, sphere, maj, min);
        grid.lat1 = s(g, 11, 3) as f64 * unit;
        grid.lon1 = s(g, 14, 3) as f64 * unit;
        match typ {
            0 | 4 | 10 => {
                grid.template = match typ {
                    0 => 0,
                    4 => 40,
                    _ => 1,
                };
                grid.lat2 = s(g, 18, 3) as f64 * unit;
                grid.lon2 = s(g, 21, 3) as f64 * unit;
                grid.dx = u(g, 24, 2) as f64 * unit;
                grid.dy = if typ == 4 {
                    let np = u(g, 26, 2);
                    if np == 0 {
                        return Err(bad("Gaussian grid without parallels"));
                    }
                    90.0 / np as f64
                } else {
                    u(g, 26, 2) as f64 * unit
                };
                grid.scan = g[27];
                if typ == 10 {
                    let rotation = &g[27..37];
                    if !rotation.iter().all(|&b| b == 0) && !rotation.iter().all(|&b| b == 255) {
                        grid.south_lat = s(g, 33, 3) as f64 * unit;
                        grid.south_lon = s(g, 36, 3) as f64 * unit;
                        grid.angle_rotate = ibm(u(g, 39, 4));
                    }
                }
            }
            5 => {
                grid.template = 20;
                grid.orient_lon = s(g, 18, 3) as f64 * unit;
                grid.dx = s(g, 21, 3) as f64;
                grid.dy = s(g, 24, 3) as f64;
                let south = g[26] & 0x80 != 0;
                grid.mesh_lat = if south { -60.0 } else { 60.0 };
                grid.scale_lat1 = if south { -90.0 } else { 90.0 };
                grid.scale_lat2 = grid.scale_lat1;
                grid.scan = g[27];
            }
            3 => {
                grid.template = 30;
                grid.orient_lon = s(g, 18, 3) as f64 * unit;
                grid.dx = s(g, 21, 3) as f64;
                grid.dy = s(g, 24, 3) as f64;
                grid.scan = g[27];
                grid.scale_lat1 = s(g, 29, 3) as f64 * unit;
                grid.scale_lat2 = s(g, 32, 3) as f64 * unit;
                grid.mesh_lat = grid.scale_lat1;
            }
            _ => {
                grid.template = 10;
                grid.lat2 = s(g, 18, 3) as f64 * unit;
                grid.lon2 = s(g, 21, 3) as f64 * unit;
                grid.scale_lat1 = s(g, 24, 3) as f64 * unit;
                grid.scale_lat2 = grid.scale_lat1;
                grid.mesh_lat = grid.scale_lat1;
                grid.scan = g[27];
                grid.dx = s(g, 29, 3) as f64;
                grid.dy = s(g, 32, 3) as f64;
            }
        }
        // GDAL compares GRIB1's scanning mode as read (degrib reorders only GRIB2's flags).
        grid.gdal_scan = grid.scan;
        Ok(grid)
    }

    /// The unit conversion of the parameter: Kelvin to Celsius (`UC_K2F`) or none.
    fn kelvin(&self) -> bool {
        self.normalize && self.param().3 == "UC_K2F"
    }

    /// `(R, E, D, bits)` of the data section.
    fn scaling(&self) -> (f64, i32, i32, u32) {
        let b = self.bds;
        (ibm(u(b, 7, 4)), s(b, 5, 2), self.dsf(), b[10] as u32)
    }

    /// degrib's `min`: `R * 2^E / 10^D`, converted.
    fn minimum(&self) -> f64 {
        let (r, e, d, _) = self.scaling();
        self.convert(r * 2f64.powf(e as f64) / 10f64.powf(d as f64))
    }

    fn convert(&self, v: f64) -> f64 {
        if self.kelvin() { 1.0 * v + -273.15 } else { v }
    }

    fn check_packing(&self) -> Result<()> {
        let flag = self.bds[3];
        if flag & 0x80 != 0 {
            return Err(Error::Unsupported("GRIB1 spherical harmonics are not supported".into()));
        }
        if flag & 0x40 != 0 {
            return Err(Error::Unsupported("GRIB1 complex (second order) packing is not supported".into()));
        }
        if self.bds[10] > 32 {
            return Err(bad("more than 32 bits per value"));
        }
        Ok(())
    }

    /// The bitmap's checks (`ReadGrib1Sect3`): its size and a non-predefined bitmap.
    fn check_bitmap(&self, npts: usize) -> Result<()> {
        let Some(bms) = self.bms else { return Ok(()) };
        let sect_len = u(bms, 1, 3) as u64;
        if npts as u64 + bms.get(3).copied().unwrap_or(0) as u64 + 48 != sect_len * 8 {
            return Err(bad("bitmap size does not match the grid"));
        }
        if u(bms, 5, 2) != 0 {
            return Err(Error::Unsupported("GRIB1 predefined bitmaps are not supported".into()));
        }
        Ok(())
    }

    /// The nodata value GDAL reports: the missing value of a message with a bitmap. It depends on
    /// the data's maximum, which is decoded only when the bounds of the packing leave it open.
    pub(crate) fn nodata(&self) -> Result<Option<f64>> {
        if self.bms.is_none() {
            return Ok(None);
        }
        if !self.nodata_needs_data()? {
            return Ok(Some(UNDEFINED_PRIM));
        }
        Ok(self.decode()?.1)
    }

    /// Whether [`Message::nodata`] needs the data: a message with a bitmap whose packing's range
    /// holds 9999, so that degrib's `resetPrim` depends on the data's maximum. The header (the
    /// sections up to the data section's first octets) tells it; the bitmap is not needed.
    pub(crate) fn nodata_needs_data(&self) -> Result<bool> {
        if self.bms.is_none() {
            return Ok(false);
        }
        self.check_packing()?;
        let min = self.minimum();
        let (r, e, d, nbits) = self.scaling();
        let top = if nbits == 0 { 0.0 } else { 2f64.powi(nbits as i32) - 1.0 };
        let upper = self.convert((r + top * 2f64.powf(e as f64)) / 10f64.powf(d as f64));
        Ok(!(min > UNDEFINED_PRIM || upper.max(min) < UNDEFINED_PRIM))
    }

    /// Decodes the message (`ReadGrib1Sect3`, `ReadGrib1Sect4`): degrib's grid (scanning mode
    /// 0100, rows south to north), and the missing value of the points a bitmap masks.
    pub(crate) fn decode(&self) -> Result<(Decoded, Option<f64>)> {
        let grid = self.grid()?;
        let (nx, ny) = (grid.nx as usize, grid.ny as usize);
        let npts = nx * ny;
        // degrib's guard against grids a short file cannot hold: at most 1000 points a byte above
        // 100 Mi points.
        if npts > 100 * 1024 * 1024 && npts / 1000 > self.file_len {
            return Err(bad("file too short for its grid"));
        }
        self.check_bitmap(npts)?;
        self.check_packing()?;
        let (r, e, d, nbits) = self.scaling();
        // The parameter's unit conversion, looked up once: `convert` looks it up in the tables
        // (with string copies), which per value made decoding several times slower than GDAL's.
        let kelvin = self.kelvin();
        let convert_value = |v: f64| if kelvin { 1.0 * v + -273.15 } else { v };
        let min = convert_value(r * 2f64.powf(e as f64) / 10f64.powf(d as f64));
        let mut values = try_filled(npts, 0f64)?;
        let convert = (grid.scan & 0xe0 != 0x40).then_some(grid.scan);
        let index = |i: usize| match convert {
            Some(scan) => {
                let (x, y) = scan_index_to_xy(i, scan, nx, ny);
                x + y * nx
            }
            None => i,
        };
        let mut bits = BitReader::new(&self.bds[11..]);
        let (pow2, pow10) = (2f64.powf(e as f64), 10f64.powf(d as f64));
        let mut max = min;
        let mut next = |bits: &mut BitReader| -> Result<f64> {
            let x = bits.read(nbits).ok_or_else(|| bad("data section cut short"))?;
            let v = convert_value((r + x as f64 * pow2) / pow10);
            if max < v {
                max = v;
            }
            Ok(v)
        };
        let Some(bms) = self.bms else {
            for i in 0..npts {
                if nbits != 0 {
                    values[index(i)] = next(&mut bits)?;
                } else {
                    values[i] = min;
                }
            }
            return Ok((Decoded { nx, ny, values }, None));
        };
        let map = bms.get(6..).unwrap_or_default();
        if map.len() < npts.div_ceil(8) {
            return Err(bad("ran out of data in the bitmap section"));
        }
        let set = |i: usize| map[i / 8] & (0x80 >> (i % 8)) != 0;
        for i in 0..npts {
            values[index(i)] = if !set(i) {
                UNDEFINED
            } else if nbits != 0 {
                next(&mut bits)?
            } else {
                min
            };
        }
        // Missing points move to 9999 when the data leaves it clear (`resetPrim`).
        let reset = if max < UNDEFINED_PRIM || min > UNDEFINED_PRIM {
            UNDEFINED_PRIM
        } else if max >= UNDEFINED && min <= UNDEFINED {
            max + 1.0
        } else {
            0.0
        };
        if reset != 0.0 {
            for i in (0..npts).filter(|&i| !set(i)) {
                values[index(i)] = reset;
            }
        }
        Ok((Decoded { nx, ny, values }, Some(if reset != 0.0 { reset } else { UNDEFINED })))
    }

    /// The parameter's short name, description, unit and conversion, as `GRIB1_Table2LookUp`.
    fn param(&self) -> (String, String, String, String) {
        let cat = self.parameter();
        let (center, version) = (self.center(), self.table_version());
        let time_range = self.pds[20];
        if center == NMC && version == 129 && cat == 180 && time_range == 3 {
            return ("AVGOZCON".into(), "Average Ozone Concentration".into(), "PPB".into(), "UC_NONE".into());
        }
        let table = choose_table(center, self.subcenter(), version, self.pds[5]);
        let t = tables();
        let (name, comment, unit, conv) = t.params.get(&(table, cat)).cloned().unwrap_or_default();
        let name = if name == format!("var{cat}") {
            if center == ECMWF {
                format!("var{cat} of table {version} of center ECMWF")
            } else {
                format!("var{cat} of table {version} of center {center}")
            }
        } else {
            name
        };
        (name, comment, unit, conv)
    }

    /// The reference and valid times (seconds since 1970) as `ReadGrib1Sect1` computes them.
    pub(crate) fn times(&self) -> Result<(f64, f64)> {
        let p = self.pds;
        let year = if p[12] == 0 { p[24] as i64 * 100 } else { p[12] as i64 + (p[24] as i64 - 1) * 100 };
        let reference = parse_time(year, p[13], p[14], p[15], p[16]).ok_or_else(|| bad("invalid reference time"))?;
        let unit = p[17];
        let at = |delta: i64| time_to_sec(delta, unit).map_or(reference, |s| reference + s);
        let (p1, p2) = (at(p[18] as i64), at(p[19] as i64));
        let valid = match p[20] {
            2..=5 | 51 => p2,
            10 => at(u(p, 19, 2) as i64),
            _ => p1,
        };
        Ok((reference, valid))
    }

    /// The band's description and metadata, as GDAL's GRIB driver reports a GRIB1 message's (from
    /// `GRIB1_Inventory`).
    pub(crate) fn metadata(&self) -> Result<BandMetadata> {
        let (name, comment, unit, _) = self.param();
        let (reference, valid) = self.times()?;
        let (short, long) = level_names(self.pds);
        // `ConvertUnitInText`: Kelvin to Celsius in the unit and comment.
        let celsius = |t: String| if self.normalize { t.replacen("[K]", "[C]", 1) } else { t };
        let items: Vec<(String, String)> = vec![
            ("GRIB_UNIT".into(), celsius(format!("[{unit}]"))),
            ("GRIB_COMMENT".into(), celsius(format!("{comment} [{unit}]"))),
            ("GRIB_ELEMENT".into(), name),
            ("GRIB_SHORT_NAME".into(), short),
            ("GRIB_REF_TIME".into(), format!("{reference:.0}")),
            ("GRIB_VALID_TIME".into(), format!("{valid:.0}")),
            ("GRIB_FORECAST_SECONDS".into(), format!("{:.0}", valid - reference)),
        ];
        Ok(BandMetadata { description: Some(long), color_interp: Some("Undefined".into()), items, ..Default::default() })
    }
}

/// `GRIB1_Table3LookUp`: the short and long names of the level.
fn level_names(p: &[u8]) -> (String, String) {
    let (typ, value) = (p[9], u(p, 11, 2));
    let t = tables();
    let (name, comment, unit, two_part) = t.levels.get(&typ).cloned().unwrap_or_default();
    if two_part {
        let (l1, l2) = (value >> 8, value & 0xff);
        (format!("{l1}-{l2}-{name}"), format!("{l1}-{l2}[{unit}] {name} ({comment})"))
    } else {
        (format!("{value}-{name}"), format!("{value}[{unit}] {name} ({comment})"))
    }
}

/// degrib's `ParseSect4Time2secV1`: seconds in `delta` units of code table 4.
fn time_to_sec(delta: i64, unit: u8) -> Option<f64> {
    const UNIT2SEC: [i64; 13] = [60, 3600, 86400, 0, 0, 0, 0, 0, 0, 0, 10800, 21600, 43200];
    match unit {
        254 => Some(delta as f64),
        u => UNIT2SEC.get(u as usize).filter(|&&s| s != 0).map(|&s| (delta * s) as f64),
    }
}

/// degrib's `ParseTime` and `Clock_ScanDate`: seconds since 1970, `None` for an invalid date.
fn parse_time(mut year: i64, month: u8, day: u8, hour: u8, minute: u8) -> Option<f64> {
    if !(1900..=2100).contains(&year) {
        year += 2000;
    }
    if month > 12 || day == 0 || day > 31 || hour > 24 || minute > 60 {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let date = if month < 1 || day as i64 > days_in_month[month as usize - 1] {
        0
    } else {
        let (m, d) = (month as i64, day as i64);
        let y = if m <= 2 { year - 1 } else { year };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        (era * 146097 + doe - 719468) * 86400
    };
    Some(date as f64 + hour as f64 * 3600.0 + minute as f64 * 60.0)
}

/// `Choose_ParmTable`: the parameter table of a center, subcenter, table version and process.
fn choose_table(center: u8, subcenter: u8, version: u8, process: u8) -> &'static str {
    let t = match center {
        NMC => {
            let sub = if version <= 3 {
                match subcenter {
                    1 => Some("ncep_reanal"),
                    11 => Some("ncep_tdl"),
                    14 => Some("ncep_mdl"),
                    _ => None,
                }
            } else {
                None
            };
            sub.or(match version {
                0 | 3 => Some("ncep_opn"),
                1 | 2 => Some(if subcenter != 0 || (process != 80 && process != 180) { "ncep_opn" } else { "ncep_reanal" }),
                128 => Some("omb"),
                129 => Some("nceptab_129"),
                130 => Some("nceptab_130"),
                131 => Some("nceptab_131"),
                133 => Some("nceptab_133"),
                140 => Some("nceptab_140"),
                141 => Some("nceptab_141"),
                _ => None,
            })
        }
        57 => match subcenter {
            0 => Some("afwa_000"),
            1 | 4 => Some("afwa_001"),
            2 => Some("afwa_002"),
            3 => Some("afwa_003"),
            10 => Some("afwa_010"),
            11 => Some("afwa_011"),
            _ => None,
        },
        ECMWF => match version {
            128 | 129 | 130 | 131 | 140 | 150 | 160 | 170 | 180 | 210 | 215 | 217 | 218 | 228 => {
                ECMWF_TABLES.iter().find(|(v, _)| *v == version).map(|(_, t)| *t)
            }
            _ => None,
        },
        78 => match version {
            2 => Some("dwd_002"),
            201 => Some("dwd_201"),
            202 => Some("dwd_202"),
            203 => Some("dwd_203"),
            _ => None,
        },
        46 if version == 254 => Some("cptec_254"),
        9 => match subcenter {
            163 => Some("nohrsc"),
            161 => Some("ncep_opn"),
            _ => None,
        },
        96 => Some("athens"),
        88 if version == 128 => Some("norway128"),
        54 => Some("cmc"),
        _ => None,
    };
    t.unwrap_or("undefined")
}

const ECMWF_TABLES: [(u8, &str); 14] = [
    (128, "ecmwf_128"),
    (129, "ecmwf_129"),
    (130, "ecmwf_130"),
    (131, "ecmwf_131"),
    (140, "ecmwf_140"),
    (150, "ecmwf_150"),
    (160, "ecmwf_160"),
    (170, "ecmwf_170"),
    (180, "ecmwf_180"),
    (210, "ecmwf_210"),
    (215, "ecmwf_215"),
    (217, "ecmwf_217"),
    (218, "ecmwf_218"),
    (228, "ecmwf_228"),
];

/// A parameter: short name, description, unit, conversion.
type Param = (String, String, String, String);

#[derive(Default)]
struct Tables {
    params: HashMap<(&'static str, u8), Param>,
    /// Level types: name, description, unit, whether the value is two octets of one each.
    levels: HashMap<u8, (String, String, String, bool)>,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = Tables::default();
        let names: HashMap<String, &'static str> = TABLE_NAMES.iter().map(|n| (n.to_string(), *n)).collect();
        for line in super::names::decompress(DATA).lines() {
            let f: Vec<&str> = line.split('\t').collect();
            match f.as_slice() {
                ["P", table, i, name, comment, unit, conv] => {
                    if let (Some(&table), Ok(i)) = (names.get(*table), i.parse::<u8>()) {
                        t.params.insert((table, i), (name.to_string(), comment.to_string(), unit.to_string(), conv.to_string()));
                    }
                }
                ["S", i, name, comment, unit, two] => {
                    if let Ok(i) = i.parse::<u8>() {
                        t.levels.insert(i, (name.to_string(), comment.to_string(), unit.to_string(), *two == "1"));
                    }
                }
                _ => {}
            }
        }
        t
    })
}

const TABLE_NAMES: [&str; 41] = [
    "ncep_opn",
    "omb",
    "nceptab_129",
    "nceptab_130",
    "nceptab_131",
    "nceptab_133",
    "nceptab_140",
    "nceptab_141",
    "nohrsc",
    "ncep_reanal",
    "ncep_tdl",
    "ncep_mdl",
    "ecmwf_128",
    "ecmwf_129",
    "ecmwf_130",
    "ecmwf_131",
    "ecmwf_140",
    "ecmwf_150",
    "ecmwf_160",
    "ecmwf_170",
    "ecmwf_180",
    "ecmwf_210",
    "ecmwf_215",
    "ecmwf_217",
    "ecmwf_218",
    "ecmwf_228",
    "dwd_002",
    "dwd_201",
    "dwd_202",
    "dwd_203",
    "cptec_254",
    "afwa_000",
    "afwa_001",
    "afwa_002",
    "afwa_003",
    "afwa_010",
    "afwa_011",
    "athens",
    "norway128",
    "cmc",
    "undefined",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ibm_singles() {
        // 0xC276A000 is -118.625 (IBM's own example); 0x41100000 is 1.0.
        assert_eq!(ibm(0xC276_A000), -118.625);
        assert_eq!(ibm(0x4110_0000), 1.0);
        assert_eq!(ibm(0), 0.0);
    }

    #[test]
    fn every_table_is_embedded() {
        let t = tables();
        assert_eq!(t.params.len(), TABLE_NAMES.len() * 256);
        assert_eq!(t.levels.len(), 256);
    }
}
