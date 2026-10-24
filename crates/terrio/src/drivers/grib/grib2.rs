//! GRIB2 messages: their fields (one band each, several when a message repeats sections 2 to 7),
//! named and described as GDAL's GRIB driver does (degrib's inventory, and the `GRIB_*` band
//! metadata of `GRIBRasterBand`), and decoded as degrib decodes them (`ReadGrib2Record` and
//! `ParseGrid`).

use super::grid::{self, Gds};
use super::tables::{self, ElemInput, UnitConvert};
use super::unpack;
use super::{Band, Decoded, MessageReader};
use crate::{Error, Result};

/// Where a field's sections are in its message (offsets from the message's start).
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct FieldSections {
    pub(super) sec3: usize,
    pub(super) sec4: usize,
    pub(super) sec5: usize,
    /// The section 6 holding the field's bitmap (an earlier one when the field reuses it).
    pub(super) sec6: Option<usize>,
    /// The field's own section 6.
    pub(super) own_sec6: Option<usize>,
    pub(super) sec7: usize,
}

fn be_u32(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes([0, 1, 2, 3].map(|k| b.get(i + k).copied().unwrap_or(0)))
}

fn be_u16(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes([b.get(i).copied().unwrap_or(0), b.get(i + 1).copied().unwrap_or(0)])
}

/// degrib's `sbit_2Comp_oneByte`.
fn sbit_one(d: i8) -> i8 {
    if d == -127 || d >= 0 {
        return d;
    }
    let x = d.wrapping_neg();
    let y = d & 0x7f;
    if x < y { x.wrapping_neg() } else { y.wrapping_neg() }
}

/// degrib's `sbit_2Comp_fourByte`.
fn sbit_four(d: i32) -> i32 {
    if d == -2147483647 || d >= 0 {
        return d;
    }
    if d == i32::MIN {
        return 0;
    }
    let x = d.wrapping_neg();
    let y = d & 0x7fff_ffff;
    if x < y { -x } else { -y }
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

/// The date of a day count from 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + if m <= 2 { 1 } else { 0 }, m, d)
}

/// degrib's `InventoryParseTime`: seconds since 1970 of 7 octets (year on two, month, day,
/// hour, minute, second); `None` out of degrib's ranges.
fn parse_time(b: &[u8]) -> Option<f64> {
    if b.len() < 7 {
        return None;
    }
    let year = i64::from(be_u16(b, 0) as i16);
    let [mo, d, h, mi, s] = [b[2], b[3], b[4], b[5], b[6]].map(|v| v as i8);
    if !(1900..=2100).contains(&year) || mo > 12 || d == 0 || d > 31 || h > 24 || mi > 60 || s > 61 {
        return None;
    }
    let days = days_from_civil(year, i64::from(mo), i64::from(d));
    Some(days as f64 * 86400.0 + f64::from(h) * 3600.0 + f64::from(mi) * 60.0 + f64::from(s))
}

/// degrib's `Clock_AddMonthYear`.
fn add_month_year(ref_time: f64, incr_month: i64, incr_year: i64) -> f64 {
    let tot_day = (ref_time / 86400.0).floor() as i64;
    let (mut year, mut month, day) = civil_from_days(tot_day);
    let remain = ref_time - tot_day as f64 * 86400.0;
    month += incr_month;
    if month > 12 {
        let k = (month - 1) / 12;
        year += k;
        month -= 12 * k;
    } else if month <= 0 {
        let k = (month - 12) / 12;
        year += k;
        month -= 12 * k;
    }
    year += incr_year;
    let next = if month == 12 { days_from_civil(year + 1, 1, 1) } else { days_from_civil(year, month + 1, 1) };
    let last_day = next - days_from_civil(year, month, 1);
    days_from_civil(year, month, day.min(last_day)) as f64 * 86400.0 + remain
}

/// degrib's `ParseSect4Time2sec`: a forecast time in seconds.
fn time_to_sec(ref_time: f64, delt: i32, unit: u8) -> Option<f64> {
    const UNIT2SEC: [i64; 14] = [60, 3600, 86400, 0, 0, 0, 0, 0, 0, 0, 10800, 21600, 43200, 1];
    let d = i64::from(delt);
    match unit {
        3 => Some(add_month_year(ref_time, d, 0) - ref_time),
        4 => Some(add_month_year(ref_time, 0, d) - ref_time),
        5 => Some(add_month_year(ref_time, 0, d * 10) - ref_time),
        6 => Some(add_month_year(ref_time, 0, d * 30) - ref_time),
        7 => Some(add_month_year(ref_time, 0, d * 100) - ref_time),
        u if (u as usize) < UNIT2SEC.len() && UNIT2SEC[u as usize] != 0 => Some(delt as f64 * UNIT2SEC[u as usize] as f64),
        _ => None,
    }
}

/// The product definition templates degrib's inventory names.
const NAMED_TEMPLATES: &[u16] = &[0, 1, 2, 5, 6, 7, 8, 9, 10, 11, 12, 15, 20, 30, 32, 40, 48];

/// The product definition templates degrib parses when decoding (its `ParseSect4`): those of its
/// inventory but the radar product (4.20).
const PARSED_TEMPLATES: &[u16] = &[0, 1, 2, 5, 6, 7, 8, 9, 10, 11, 12, 15, 30, 32, 40, 48];

/// What degrib's inventory gives a field.
struct Inventory {
    ref_time: f64,
    valid_time: f64,
    fore_sec: f64,
    element: String,
    comment: String,
    unit: String,
    short_level: String,
    long_level: String,
    /// The unit conversion of the data.
    convert: UnitConvert,
}

/// degrib's `GRIB2Inventory2to7`, for a field's section 4.
fn inventory(prod_type: u8, sec1: &[u8], s4: &[u8]) -> Result<Inventory> {
    let bad = || Error::InvalidData("GRIB2: invalid section 1 reference time".into());
    let ref_time = parse_time(sec1.get(12..19).unwrap_or_default()).ok_or_else(bad)?;
    let center = be_u16(sec1, 5);
    let subcenter = be_u16(sec1, 7);
    let master_version = sec1.get(9).copied().unwrap_or(0);
    let at = |i: usize| s4.get(i).copied().unwrap_or(0);
    let i32_at = |i: usize| be_u32(s4, i) as i32;
    let template = be_u16(s4, 7);
    // degrib converts the units of the data from the parameter it parses again when decoding
    // (`MetaParse`), which for templates it does not parse is that of category 0, parameter 0.
    let blank = ElemInput { master_version, center, subcenter, prod_type, ..Default::default() };
    let data_convert = |named: UnitConvert| if PARSED_TEMPLATES.contains(&template) { named } else { tables::elem_name(&blank).convert };
    if !NAMED_TEMPLATES.contains(&template) {
        let u = || "unknown".to_string();
        return Ok(Inventory {
            ref_time,
            valid_time: 0.0,
            fore_sec: 0.0,
            element: u(),
            comment: u(),
            unit: u(),
            short_level: u(),
            long_level: u(),
            convert: data_convert(UnitConvert::None),
        });
    }
    let offset = match template {
        40 => 2,
        48 => 24,
        _ => 0,
    };
    let (cat, subcat, gen_process) = (at(9), at(10), at(offset + 11));
    let mut e = ElemInput {
        master_version,
        center,
        subcenter,
        prod_type,
        template,
        cat,
        subcat,
        gen_process,
        stat_process: 255,
        ..Default::default()
    };
    let (fore_sec, mut valid_time);
    let mut time_incr = 255u8;
    let mut range_unit: u8;
    let mut len_time: i64;
    if template == 20 || template == 30 || template == 254 {
        fore_sec = 0.0;
        valid_time = ref_time;
        range_unit = 255;
        len_time = 0;
    } else {
        e.gen_id = at(offset + 13);
        let unit = at(offset + 17);
        fore_sec = time_to_sec(ref_time, i32_at(offset + 18), unit)
            .ok_or_else(|| Error::InvalidData(format!("GRIB2: unknown time unit {unit}")))?;
        valid_time = ref_time + fore_sec;
        range_unit = 1;
        len_time = (fore_sec / 3600.0).clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i64;
        let limits = |e: &mut ElemInput| {
            e.prob_type = at(36);
            e.lower_prob = f64::from(sbit_four(i32_at(38))) * 10f64.powi(-i32::from(sbit_one(at(37) as i8)));
            e.upper_prob = f64::from(sbit_four(i32_at(43))) * 10f64.powi(-i32::from(sbit_one(at(42) as i8)));
        };
        let interval = |time: usize, incr: usize, valid: &mut f64, ti: &mut u8, ru: &mut u8, lt: &mut i64| {
            *valid = parse_time(s4.get(time..time + 7).unwrap_or_default()).unwrap_or(ref_time + fore_sec);
            *ti = at(incr);
            *ru = at(incr + 1);
            *lt = i64::from(i32_at(incr + 2));
        };
        match template {
            5 => limits(&mut e),
            6 => e.percentile = at(34) as i8,
            12 => interval(36, 49, &mut valid_time, &mut time_incr, &mut range_unit, &mut len_time),
            10 => {
                e.percentile = at(34) as i8;
                interval(35, 48, &mut valid_time, &mut time_incr, &mut range_unit, &mut len_time);
            }
            8 => {
                interval(34, 47, &mut valid_time, &mut time_incr, &mut range_unit, &mut len_time);
                e.stat_process = at(46);
            }
            11 => interval(37, 50, &mut valid_time, &mut time_incr, &mut range_unit, &mut len_time),
            9 => {
                limits(&mut e);
                interval(47, 60, &mut valid_time, &mut time_incr, &mut range_unit, &mut len_time);
            }
            _ => {}
        }
    }
    if matches!(template, 2 | 3 | 4 | 12 | 13 | 14) && s4.len() >= 35 {
        e.derived_fcst = Some(at(34));
    }
    if range_unit == 255 {
        range_unit = 1;
        len_time = ((valid_time - fore_sec - ref_time) / 3600.0).clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i64;
    }
    // Lengths in hours (or in months and years).
    let (len, unit) = match range_unit {
        0 => (len_time / 60, 1),
        1 => (len_time, 1),
        2 => (len_time * 24, 1),
        10 => (len_time * 3, 1),
        11 => (len_time * 6, 1),
        12 => (len_time * 12, 1),
        13 => (len_time / 3600, 1),
        3 => (len_time, 3),
        4 => (len_time, 4),
        5 => (len_time * 10, 4),
        6 => (len_time * 30, 4),
        7 => (len_time * 100, 4),
        u => return Err(Error::InvalidData(format!("GRIB2: unknown time range unit {u}"))),
    };
    let len = i32::try_from(len).map_err(|_| Error::InvalidData("GRIB2: time range out of range".into()))?;
    e.len_time = if len == -2147483647 { 0 } else { len };
    e.time_range_unit = unit;
    e.time_incr_type = time_incr;
    // The fixed surfaces.
    let (mut fst_type, mut fst, mut snd) = (0u8, None, None);
    let named_levels = !matches!(template, 20 | 30 | 32 | 254 | 1000 | 1001 | 1002);
    if named_levels {
        let surface = |o: usize| -> (u8, Option<f64>) {
            let t = at(offset + o);
            let us = at(offset + o + 1);
            let scale = if us & 0x80 != 0 { -i32::from(us & 0x7f) } else { i32::from(us) };
            let uv = be_u32(s4, offset + o + 2);
            let v = if uv & 0x8000_0000 != 0 { -((uv & 0x7fff_ffff) as i64) } else { i64::from(uv) };
            if v == -2147483647 || scale == -127 || t == 255 { (t, None) } else { (t, Some(v as f64 * 10f64.powi(-scale))) }
        };
        let (t1, v1) = surface(22);
        fst_type = t1;
        fst = Some(v1.unwrap_or(0.0));
        snd = surface(28).1;
    }
    e.fst_value = fst;
    e.snd_value = snd;
    let name = tables::elem_name(&e);
    let (short_level, long_level) = match fst {
        Some(v) => tables::level_name(center, fst_type, v, snd),
        None => ("0 undefined".to_string(), "0.000[-] undefined ()".to_string()),
    };
    Ok(Inventory {
        ref_time,
        valid_time,
        fore_sec,
        element: name.element,
        comment: name.comment,
        unit: name.unit,
        short_level,
        long_level,
        convert: data_convert(name.convert),
    })
}

/// GDAL's `GRIB_IDS`: the identification section (section 1), described.
fn ids(sec1: &[u8]) -> Option<String> {
    let t = tables::tables();
    if sec1.len() < 21 || sec1.len() > 100_000 {
        return None;
    }
    let mut s = String::new();
    let center = be_u16(sec1, 5);
    if center != 255 && center != 65535 {
        s.push_str(&format!("CENTER={center}"));
        if let Some(name) = t.center(center) {
            s.push_str(&format!("({name})"));
        }
    }
    let sub = be_u16(sec1, 7);
    if sub != 65535 {
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(&format!("SUBCENTER={sub}"));
        if let Some(name) = t.subcenter(center, sub) {
            s.push_str(&format!("({name})"));
        }
    }
    if !s.is_empty() {
        s.push(' ');
    }
    let label = |v: u8, names: &[&str]| -> String {
        match names.get(v as usize) {
            Some(n) => format!("{v}({})", n.replace(' ', "_")),
            None => v.to_string(),
        }
    };
    s.push_str(&format!("MASTER_TABLE={} LOCAL_TABLE={} ", sec1[9], sec1[10]));
    s.push_str(&format!(
        "SIGNF_REF_TIME={} ",
        label(sec1[11], &["Analysis", "Start of Forecast", "Verifying time of forecast", "Observation time"])
    ));
    s.push_str(&format!(
        "REF_TIME={:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z ",
        be_u16(sec1, 12),
        sec1[14],
        sec1[15],
        sec1[16],
        sec1[17],
        sec1[18]
    ));
    s.push_str(&format!(
        "PROD_STATUS={} ",
        label(
            sec1[19],
            &[
                "Operational",
                "Operational test",
                "Research",
                "Re-analysis",
                "TIGGE",
                "TIGGE test",
                "S2S operational",
                "S2S test",
                "UERRA",
                "UERRA test"
            ]
        )
    ));
    s.push_str(&format!(
        "TYPE={}",
        label(
            sec1[20],
            &[
                "Analysis",
                "Forecast",
                "Analysis and forecast",
                "Control forecast",
                "Perturbed forecast",
                "Control and perturbed forecast",
                "Processed satellite observations",
                "Processed radar observations",
                "Event Probability"
            ]
        )
    ));
    Some(s)
}

/// GDAL's `ConvertUnitInText`: `[K]` written `[C]` when units are normalized.
fn convert_unit_text(normalize: bool, s: &str) -> String {
    if normalize { s.replacen("[K]", "[C]", 1) } else { s.to_string() }
}

/// Whether units are normalized to metric (GDAL's `GRIB_NORMALIZE_UNITS`, on by default; read
/// from the environment).
pub(super) fn normalize_units() -> bool {
    std::env::var("GRIB_NORMALIZE_UNITS").map_or(true, |v| !matches!(v.trim().to_ascii_uppercase().as_str(), "NO" | "FALSE" | "OFF" | "0"))
}

/// The fields of a GRIB2 message at `offset` (of `len` octets), as bands.
pub(super) fn bands(r: &mut MessageReader, offset: u64, len: u64) -> Result<Vec<Band>> {
    let normalize = normalize_units();
    let head = r.bytes(offset, 16)?.to_vec();
    let discipline = head[6];
    let mut pos = 16usize;
    let end = usize::try_from(len).map_err(|_| Error::Unsupported("a GRIB message larger than this platform's memory".into()))?;
    let mut sec1: Vec<u8> = Vec::new();
    let mut current = FieldSections::default();
    let mut last_bitmap: Option<usize> = None;
    let mut out = Vec::new();
    loop {
        // The end section ("7777"), read only when the message does not end there.
        if pos + 4 >= end {
            break;
        }
        let h = r.bytes(offset + pos as u64, 5.min(end - pos))?.to_vec();
        if h.starts_with(b"7777") {
            break;
        }
        if h.len() < 5 {
            break;
        }
        let slen = be_u32(&h, 0) as usize;
        let num = h[4];
        // Section 6 holds its bitmap indicator at octet 6.
        if slen < 5 || (num == 6 && slen < 6) || slen > end - pos {
            return Err(Error::InvalidData(format!("GRIB2 message at {offset}: invalid section {num}")));
        }
        match num {
            1 => sec1 = r.bytes(offset + pos as u64, slen)?.to_vec(),
            2 => {}
            3 => current.sec3 = pos,
            4 => current.sec4 = pos,
            5 => current.sec5 = pos,
            6 => {
                current.own_sec6 = Some(pos);
                let indicator = r.bytes(offset + pos as u64 + 5, 1)?[0];
                current.sec6 = match indicator {
                    0 => {
                        last_bitmap = Some(pos);
                        Some(pos)
                    }
                    254 => last_bitmap,
                    _ => None,
                };
            }
            7 => {
                if current.sec3 == 0 || current.sec4 == 0 || current.sec5 == 0 {
                    return Err(Error::InvalidData(format!("GRIB2 message at {offset}: data without sections 3, 4 and 5")));
                }
                current.sec7 = pos;
                out.push(band(r, offset, len, discipline, &sec1, current, normalize)?);
            }
            n => return Err(Error::InvalidData(format!("GRIB2 message at {offset}: unexpected section {n}"))),
        }
        pos += slen;
    }
    if out.is_empty() {
        return Err(Error::InvalidData(format!("GRIB2 message at {offset} without data")));
    }
    Ok(out)
}

/// A section of a message, read whole.
fn section(r: &mut MessageReader, offset: u64, pos: usize) -> Result<Vec<u8>> {
    let h = r.bytes(offset + pos as u64, 4)?;
    let n = be_u32(h, 0) as usize;
    Ok(r.bytes(offset + pos as u64, n)?.to_vec())
}

#[allow(clippy::too_many_arguments)]
fn band(r: &mut MessageReader, offset: u64, len: u64, discipline: u8, sec1: &[u8], s: FieldSections, normalize: bool) -> Result<Band> {
    let sec3 = section(r, offset, s.sec3)?;
    let sec4 = section(r, offset, s.sec4)?;
    let sec5 = section(r, offset, s.sec5)?;
    // Without a section 6, no bitmap.
    let indicator = match s.own_sec6 {
        Some(p) => r.bytes(offset + p as u64 + 5, 1)?[0],
        None => 255,
    };
    let gds = grid::parse(&sec3)?;
    super::point_count(gds.nx, gds.ny, len)?;
    let inv = inventory(discipline, sec1, &sec4)?;
    let mut items: Vec<(String, String)> = vec![
        ("GRIB_UNIT".into(), convert_unit_text(normalize, &inv.unit)),
        ("GRIB_COMMENT".into(), convert_unit_text(normalize, &inv.comment)),
        ("GRIB_ELEMENT".into(), inv.element.clone()),
        ("GRIB_SHORT_NAME".into(), inv.short_level.clone()),
        ("GRIB_REF_TIME".into(), format!("{:.0}", inv.ref_time)),
        ("GRIB_VALID_TIME".into(), format!("{:.0}", inv.valid_time)),
        ("GRIB_FORECAST_SECONDS".into(), format!("{:.0}", inv.fore_sec)),
    ];
    const DISCIPLINES: [&str; 11] = [
        "Meteorological",
        "Hydrological",
        "Land Surface",
        "Space products",
        "Space products",
        "Reserved",
        "Reserved",
        "Reserved",
        "Reserved",
        "Reserved",
        "Oceanographic Products",
    ];
    let d = match DISCIPLINES.get(discipline as usize) {
        Some(name) => format!("{discipline}({})", name.replace(' ', "_")),
        None => discipline.to_string(),
    };
    items.push(("GRIB_DISCIPLINE".into(), d));
    if let Some(ids) = ids(sec1) {
        items.push(("GRIB_IDS".into(), ids));
    }
    if sec4.len() >= 9 && sec4.len() <= 100_000 {
        let ncoord = be_u16(&sec4, 5) as usize;
        let pdtn = be_u16(&sec4, 7);
        items.push(("GRIB_PDS_PDTN".into(), pdtn.to_string()));
        let count = (sec4.len() - 9).saturating_sub(ncoord * 4);
        let body = &sec4[9..9 + count];
        items.push(("GRIB_PDS_TEMPLATE_NUMBERS".into(), body.iter().map(u8::to_string).collect::<Vec<_>>().join(" ")));
        if let Some(values) = super::pds::assembled_values(pdtn, body) {
            items.push(("GRIB_PDS_TEMPLATE_ASSEMBLED_VALUES".into(), values));
        }
    }
    // The nodata value: the primary missing value of complex packing, else 9999 where a bitmap
    // masks points (degrib's).
    let drs = be_u16(&sec5, 9);
    let mut nodata = None;
    if (drs == 2 || drs == 3) && sec5.len() >= 31 {
        let mgmt = sec5[22];
        if mgmt == 1 || mgmt == 2 {
            nodata = Some(match sec5[20] {
                0 => f64::from(f32::from_bits(be_u32(&sec5, 23))),
                _ => f64::from(be_u32(&sec5, 23) as i32),
            });
        }
    }
    if nodata.is_none() && (indicator == 0 || (indicator == 254 && s.sec6.is_some())) {
        nodata = Some(9999.0);
    }
    Ok(Band {
        offset,
        len,
        items,
        description: inv.long_level,
        nodata,
        size: (gds.nx, gds.ny),
        gds2: Some((gds, s, inv.convert)),
        grib1: None,
    })
}

/// Decodes a field of a GRIB2 message (`msg`, the whole message), as degrib does: the values in
/// scanning mode 0100 (from the south-west corner, rows first) for the grids it reorders, units
/// normalized as asked, missing values as degrib marks them.
pub(super) fn decode(msg: &[u8], gds: &Gds, s: &FieldSections, convert: UnitConvert, normalize: bool) -> Result<Decoded> {
    let sect = |pos: usize| -> Result<&[u8]> {
        let n = be_u32(msg, pos) as usize;
        msg.get(pos..pos.saturating_add(n)).filter(|s| s.len() >= 5).ok_or_else(|| Error::InvalidData("GRIB2 message truncated".into()))
    };
    let sec5 = sect(s.sec5)?;
    let sec7 = sect(s.sec7)?;
    let ndpts = be_u32(sec5, 5) as usize;
    let npts = super::point_count(gds.nx, gds.ny, msg.len() as u64)?;
    let drs = be_u16(sec5, 9);
    if ndpts > npts {
        return Err(Error::InvalidData(format!("GRIB2 field of {ndpts} values on a grid of {npts} points")));
    }
    let packed = unpack::unpack(sec5, &sec7[5..], ndpts)?;
    // The bitmap (1 where a value is packed).
    let bitmap: Option<Vec<bool>> = match s.sec6 {
        Some(p) => {
            let sec6 = sect(p)?;
            let bits = sec6.get(6..).unwrap_or_default();
            Some((0..npts).map(|i| bits.get(i / 8).is_some_and(|b| b & (0x80 >> (i % 8)) != 0)).collect())
        }
        None => None,
    };
    // g2clib's expansion: packed values where the bitmap is set, zeros elsewhere.
    let fld: Vec<f32> = match &bitmap {
        Some(bm) => {
            if npts < ndpts {
                return Err(Error::InvalidData("GRIB2 bitmap of fewer points than the data".into()));
            }
            let mut out = vec![0f32; npts];
            let mut n = 0;
            for (i, set) in bm.iter().enumerate() {
                if *set {
                    out[i] = *packed.get(n).ok_or_else(|| Error::InvalidData("GRIB2 bitmap of more points than the data".into()))?;
                    n += 1;
                }
            }
            out
        }
        None => {
            if npts != ndpts {
                return Err(Error::InvalidData(format!("GRIB2 field of {ndpts} values on a grid of {npts} points")));
            }
            packed
        }
    };
    // degrib's missing values: complex packing's (f_miss 1 or 2), primary and secondary.
    let field_type = sec5.get(20).copied().unwrap_or(0);
    let (f_miss, miss_pri, miss_sec) = if (drs == 2 || drs == 3) && sec5.len() >= 31 && sec5[22] <= 2 {
        let read = |i: usize| -> f64 {
            if field_type == 0 { f64::from(f32::from_bits(be_u32(sec5, i))) } else { f64::from(be_u32(sec5, i) as i32 as f32) }
        };
        (sec5[22], read(23), read(27))
    } else {
        (0, 0.0, 0.0)
    };
    // Integer fields are truncated (degrib's `TransferInt`), except for IEEE packing.
    let integer = field_type == 1 && drs != 4 && drs != 50 && drs != 51;
    let to_value = |v: f32| -> f64 {
        if integer {
            let d = f64::from(v);
            if d >= f64::from(i32::MAX) {
                f64::from(i32::MAX)
            } else if d <= f64::from(i32::MIN) {
                f64::from(i32::MIN)
            } else if v.is_nan() {
                0.0
            } else {
                f64::from(v as i32)
            }
        } else {
            f64::from(v)
        }
    };
    let raw: Vec<f64> = fld.iter().map(|v| to_value(*v)).collect();
    let (m, b) = if normalize { convert.metric().unwrap_or((1.0, 0.0)) } else { (1.0, 0.0) };
    let convert_value = |v: f64| if m == -10.0 { 10f64.powf(v) } else { m * v + b };
    let is_missing = |v: f64| (f_miss == 1 && v == miss_pri) || (f_miss == 2 && (v == miss_pri || v == miss_sec));
    // Into scanning mode 0100, converting units.
    let mut out = vec![0f64; npts];
    let index = |i: usize| {
        if gds.reorder {
            let (x, y) = grid::scan_index_xy(i, gds.file_scan, gds.nx, gds.ny);
            x + y * gds.nx
        } else {
            i
        }
    };
    let (mut min, mut max) = (f64::INFINITY, f64::NEG_INFINITY);
    for (i, &v) in raw.iter().enumerate() {
        let value = if f_miss == 0 || !is_missing(v) {
            let c = convert_value(v);
            min = min.min(c);
            max = max.max(c);
            c
        } else {
            v
        };
        out[index(i)] = value;
    }
    // Missing values that unit conversion made valid ones are moved past the maximum.
    if (f_miss == 1 || f_miss == 2) && min <= max {
        let (mut xp, mut xs) = (miss_pri, miss_sec);
        let mut readjust = false;
        if miss_pri >= min && miss_pri <= max {
            xp = max + 1.0;
            readjust = true;
        }
        if f_miss == 2 && miss_sec >= min && miss_sec <= max {
            xs = max + 2.0;
            readjust = true;
        }
        if readjust {
            for (i, &v) in raw.iter().enumerate() {
                if v == miss_pri {
                    out[index(i)] = xp;
                } else if f_miss == 2 && v == miss_sec {
                    out[index(i)] = xs;
                }
            }
        }
    }
    // Points the bitmap masks: 9999, without missing values of complex packing.
    if let Some(bm) = &bitmap
        && f_miss != 1
        && f_miss != 2
    {
        for (i, set) in bm.iter().enumerate() {
            if !set {
                out[index(i)] = 9999.0;
            }
        }
    }
    Ok(Decoded { nx: gds.nx, ny: gds.ny, values: out })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_times_as_degrib() {
        assert_eq!(parse_time(&[0x07, 0xea, 10, 22, 0, 0, 0]), Some(1792627200.0));
        assert_eq!(parse_time(&[0x07, 0xea, 13, 22, 0, 0, 0]), None);
        // A month after 31 January: the last day of February.
        let jan31 = days_from_civil(2024, 1, 31) as f64 * 86400.0;
        assert_eq!(add_month_year(jan31, 1, 0), days_from_civil(2024, 2, 29) as f64 * 86400.0);
        assert_eq!(time_to_sec(0.0, 6, 1), Some(21600.0));
        assert_eq!(sbit_four(-5), -5);
        assert_eq!(sbit_four(0x8000_0005u32 as i32), -5);
        assert_eq!(sbit_one(0x85u8 as i8), -5);
    }
}
