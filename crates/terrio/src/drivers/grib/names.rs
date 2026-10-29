//! GRIB2 names as degrib and GDAL's GRIB driver build them: the element, comment and unit of a
//! parameter (`metaname.cpp` `ParseElemName`), the name of a level (`ParseLevelName`), the
//! identification (`GRIB_IDS`) and the product template's values (`FindPDSTemplateGRIB2`). The
//! code tables are GDAL's (`scripts/gen_grib_tables.py`).
//!
//! Not reproduced: NDFD's and MOS's own names (centers 8 and 7.14), and the month and year
//! lengths of statistically processed products.

use super::{Field, be, soct, uoct};
use std::collections::HashMap;
use std::sync::OnceLock;

static DATA: &[u8] = include_bytes!("../../../data/grib2.tsv.zst");

/// A parameter of code table 4.2: short name, name, unit, unit conversion.
type Param = (String, String, String, String);

/// Runs of parameters of a category with the same record: first, last, record.
type Runs = Vec<(u16, u16, Param)>;

#[derive(Default)]
struct Tables {
    /// `(discipline, category)` to runs `(first, last, parameter)`.
    params: HashMap<(u8, u8), Runs>,
    /// `(center, subcenter or None, table)`, in GDAL's order.
    local_index: Vec<(i32, Option<i32>, String)>,
    /// `table` to `(discipline, category, number)` to parameter.
    local: HashMap<String, HashMap<(i32, i32, i32), Param>>,
    /// Code table 4.5: short name, name, unit.
    levels: HashMap<i32, (String, String, String)>,
    centers: HashMap<i32, String>,
    subcenters: HashMap<(i32, i32), String>,
}

fn tables() -> &'static Tables {
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = Tables::default();
        let text = decompress(DATA);
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            let f: Vec<&str> = line.split('\t').collect();
            let int = |i: usize| f.get(i).and_then(|v| v.parse::<i32>().ok());
            let s = |i: usize| f.get(i).map_or(String::new(), |v| v.to_string());
            match f[0] {
                "P" if f.len() >= 9 => {
                    let (Some(d), Some(c), Some(a), Some(b)) = (int(1), int(2), int(3), int(4)) else { continue };
                    // degrib's `GetGrib2Table4_2_Record`: the WMO-only rows have no short name,
                    // and their long name stands for it.
                    let short = if s(5).is_empty() { s(6) } else { s(5) };
                    t.params.entry((d as u8, c as u8)).or_default().push((a as u16, b as u16, (short, s(6), s(7), s(8))));
                }
                "I" if f.len() >= 4 => {
                    if let Some(c) = int(1) {
                        t.local_index.push((c, int(2), s(3)));
                    }
                }
                "L" if f.len() >= 9 => {
                    let (Some(d), Some(c), Some(n)) = (int(2), int(3), int(4)) else { continue };
                    t.local.entry(s(1)).or_default().entry((d, c, n)).or_insert((s(5), s(6), s(7), s(8)));
                }
                "V" if f.len() >= 5 => {
                    if let Some(c) = int(1) {
                        t.levels.entry(c).or_insert((s(2), s(3), s(4)));
                    }
                }
                "C" if f.len() >= 3 => {
                    if let Some(c) = int(1) {
                        t.centers.entry(c).or_insert(s(2));
                    }
                }
                "S" if f.len() >= 4 => {
                    if let (Some(c), Some(sc)) = (int(1), int(2)) {
                        t.subcenters.entry((c, sc)).or_insert(s(3));
                    }
                }
                _ => {}
            }
        }
        t
    })
}

pub(super) fn decompress(data: &[u8]) -> String {
    let mut text = String::new();
    if let Ok(mut d) = ruzstd::decoding::StreamingDecoder::new(data) {
        let _ = std::io::Read::read_to_string(&mut d, &mut text);
    }
    text
}

/// The names of a band.
pub(crate) struct Names {
    pub element: String,
    pub comment: String,
    pub unit: String,
    pub short_level: String,
    pub long_level: String,
}

/// The first and last of code table 4.2's records for a parameter, and its unit conversion.
fn find_param(center: i32, subcenter: i32, master: i32, disc: u8, cat: u8, num: u8) -> Option<Param> {
    let t = tables();
    if master != 255
        && let Some(p) = t.params.get(&(disc, cat)).and_then(|runs| runs.iter().find(|(a, b, _)| (*a..=*b).contains(&(num as u16))))
    {
        return Some(p.2.clone());
    }
    let table = t.local_index.iter().find(|(c, sc, _)| *c == center && sc.is_none_or(|sc| sc == subcenter))?;
    t.local.get(&table.2)?.get(&(disc as i32, cat as i32, num as i32)).cloned()
}

/// The unit conversion code of a field's parameter (`UC_K2F`...), as degrib's `ParseElemName`
/// leaves it: none for derived forecast spreads and for forecast errors.
pub(crate) fn conversion_code(f: &Field) -> Option<String> {
    let p = Product::of(f);
    if matches!(p.derived, 2 | 3 | 4 | 5 | 7) || matches!(p.gen_process, 6 | 7) || p.is_probability(f.discipline) {
        return None;
    }
    find_param(p.center, p.subcenter, p.master, f.discipline, p.cat, p.num).map(|p| p.3)
}

/// What a product's names depend on.
struct Product {
    center: i32,
    subcenter: i32,
    master: i32,
    template: i32,
    cat: u8,
    num: u8,
    gen_process: i32,
    derived: i32,
    /// Octets the product template is shifted by (templates 4.40 and 4.48).
    offset: usize,
}

impl Product {
    /// Whether degrib names the product as a probability (`ElemNameProb`), which it never
    /// converts: templates 4.5 and 4.9, but NDFD's category 19.
    fn is_probability(&self, discipline: u8) -> bool {
        let ndfd = self.center == 8 && (self.subcenter == 65535 || self.subcenter == 0);
        matches!(self.template, 5 | 9) && !(ndfd && discipline == 0 && self.cat == 19)
    }

    fn of(f: &Field) -> Product {
        let s4 = f.sect4;
        let template = uoct(s4, 8, 2);
        let offset = match template {
            40 => 2,
            48 => 24,
            _ => 0,
        };
        let derived = if matches!(template, 2 | 3 | 4 | 12 | 13 | 14) && s4.len() >= 35 { uoct(s4, 35, 1) } else { -1 };
        Product {
            center: uoct(f.sect1, 6, 2),
            subcenter: uoct(f.sect1, 8, 2),
            master: uoct(f.sect1, 10, 1),
            template,
            cat: uoct(s4, 10, 1) as u8,
            num: uoct(s4, 11, 1) as u8,
            gen_process: uoct(s4, offset + 12, 1),
            derived,
            offset,
        }
    }
}

/// The length in hours of a statistically processed product's time range, as degrib's inventory
/// computes it for names (`lenTime`).
fn len_time(f: &Field, p: &Product, valid: f64, reference: f64, fore: f64) -> i64 {
    let s4 = f.sect4;
    let at = |unit_octet: usize| -> (i32, i64) {
        (uoct(s4, unit_octet, 1), be(s4.get(unit_octet..unit_octet + 4).unwrap_or(&[0; 4])) as u32 as i32 as i64)
    };
    let (mut unit, mut len) = match p.template {
        8 => at(49),
        9 => at(62),
        10 => at(50),
        11 => at(52),
        12 => at(51),
        _ => (1, (fore / 3600.0) as i64),
    };
    if unit == 255 {
        unit = 1;
        len = ((valid - fore - reference) / 3600.0) as i64;
    }
    len = match unit {
        0 => (len as f64 / 60.0) as i64,
        1 => len,
        2 => len * 24,
        10 => len * 3,
        11 => len * 6,
        12 => len * 12,
        13 => (len as f64 / 3600.0) as i64,
        _ => len,
    };
    if len == -0x7fff_ffff { 0 } else { len }
}

/// The names of a field whose values degrib converts to `converted_unit` (`[C]`), if any.
pub(crate) fn lookup(f: &Field, converted_unit: Option<&str>, times: (f64, f64, f64)) -> Names {
    let p = Product::of(f);
    if !is_named(f) {
        let u = || "unknown".to_string();
        return Names { element: u(), comment: u(), unit: u(), short_level: u(), long_level: u() };
    }
    let (reference, valid, fore) = times;
    let param = find_param(p.center, p.subcenter, p.master, f.discipline, p.cat, p.num);
    let len = len_time(f, &p, valid, reference, fore);
    let hours = |len: i64| if len > 0 { format!("{len:02} hr ") } else { String::new() };
    let (mut element, mut comment, mut unit) = match param {
        // `ElemNameProb`: the probability of the parameter in a range, in percent.
        Some((short, name, unit, _)) if p.is_probability(f.discipline) => {
            let s4 = f.sect4;
            let limit = |factor: usize| -> f64 {
                let sf = sbit_2comp_1(uoct(s4, factor, 1) as u8 as i8) as i32;
                let value = sbit_2comp_4(uoct(s4, factor + 1, 4));
                value as f64 * 10f64.powi(-sf)
            };
            let (lower, mut upper) = (limit(38), limit(43));
            let scaled = upper * 25.4;
            if upper > scaled || scaled > i32::MAX as f64 || scaled < i32::MIN as f64 || scaled.is_nan() {
                upper = 0.0;
            }
            let g = |v: f64| super::super::common::fmt_g(v, 6);
            let range = match uoct(s4, 37, 1) {
                0 => format!("< {} {unit}", g(lower)),
                1 => format!("> {} {unit}", g(upper)),
                2 => format!(">= {}, < {} {unit}", g(lower), g(upper)),
                3 => format!("> {} {unit}", g(lower)),
                4 => format!("< {} {unit}", g(upper)),
                _ => unit.clone(),
            };
            let element = if len > 0 { format!("Prob{short}{len:02}") } else { format!("Prob{short}") };
            (element, format!("{}Prob of {name} {range}", hours(len)), "[%]".to_string())
        }
        None if p.is_probability(f.discipline) => {
            ("ProbUnknown".into(), format!("Prob of (prodType {}, cat {}, subcat {})", f.discipline, p.cat, p.num), "[%]".into())
        }
        // `ElemNamePerc`: a percentile of the parameter.
        Some((short, name, unit, _)) if matches!(p.template, 6 | 10) => {
            let percentile = uoct(f.sect4, 35, 1) as u8 as i8;
            let digits = short.len() >= 2 && short.bytes().rev().take(2).all(|b| b.is_ascii_digit());
            let local = p.master == 255 || find_param(-1, -1, p.master, f.discipline, p.cat, p.num).is_none();
            let element = if local && digits {
                short.clone()
            } else if local && (short == "Surge" || short == "SURGE") {
                format!("{short}{:02}", 100 - percentile as i32)
            } else {
                format!("{short}{percentile:02}")
            };
            (element, format!("{}{name} Percentile({percentile})", hours(len)), format!("[{unit}]"))
        }
        // degrib's NDFD and MOS names (`NDFD_Override`, and MOS's QPF and snow amounts).
        Some((short, name, unit, _)) if is_mos(&p) && matches!(short.as_str(), "APCP" | "ASNOW") => {
            let n = if short == "APCP" { "QPF" } else { "SnowAmt" };
            (format!("{n}{len:02}"), format!("{len:02} hr {name}"), format!("[{unit}]"))
        }
        Some((short, name, unit, _)) if (is_ndfd(&p) || is_mos(&p)) && ndfd_name(&short).is_some() => {
            (ndfd_name(&short).unwrap_or_default().to_string(), name, format!("[{unit}]"))
        }
        Some((short, name, unit, _)) => {
            let accum = (f.discipline == 1 && p.cat == 1 && p.num == 2)
                || (f.discipline == 0 && p.cat == 19 && p.num == 2)
                || (f.discipline == 0 && p.cat == 1 && p.num == 8)
                || (f.discipline == 0 && p.cat == 19 && p.num == 203);
            if accum && len > 0 {
                (format!("{short}{len:02}"), format!("{len:02} hr {name}"), format!("[{unit}]"))
            } else {
                (short, name, format!("[{unit}]"))
            }
        }
        None => ("unknown".into(), format!("(prodType {}, cat {}, subcat {})", f.discipline, p.cat, p.num), "[-]".into()),
    };
    let derived_unit = match p.derived {
        2 => Some("[stddev]"),
        3 => Some("[stddev normalized]"),
        4 => Some("[spread]"),
        5 => Some("[large anomaly index]"),
        7 => Some("[interquantile range]"),
        _ => None,
    };
    if let Some(u) = derived_unit {
        unit = u.into();
    }
    if matches!(p.gen_process, 6 | 7) {
        element = "ERR".into();
        comment = format!("{comment} error {unit}");
    } else {
        comment = format!("{comment} {unit}");
    }
    if let Some(u) = converted_unit {
        unit = u.into();
    }
    // GDAL's `ConvertUnitInText`, for `GRIB_NORMALIZE_UNITS=YES`.
    let metric = |s: &str| if f.normalize { s.replacen("[K]", "[C]", 1) } else { s.to_string() };
    let (short_level, long_level) = level_names(f, &p);
    Names { element, comment: metric(&comment), unit: metric(&unit), short_level, long_level }
}

/// The product definition templates degrib's inventory names (`GRIB2Inventory2to7`); the fields
/// of others are "unknown" in every name.
pub(crate) fn is_named(f: &Field) -> bool {
    matches!(uoct(f.sect4, 8, 2), 0 | 1 | 2 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 | 15 | 20 | 30 | 32 | 40 | 48)
}

/// degrib's `IsData_NDFD`: the US National Weather Service's NDFD products.
fn is_ndfd(p: &Product) -> bool {
    p.center == 8 && (p.subcenter == 65535 || p.subcenter == 0)
}

/// degrib's `IsData_MOS`: NCEP's MOS products.
fn is_mos(p: &Product) -> bool {
    p.center == 7 && p.subcenter == 14
}

/// degrib's `NDFD_Override`: NDFD's names of some parameters.
fn ndfd_name(short: &str) -> Option<&'static str> {
    Some(match short {
        "TMP" => "T",
        "TMAX" => "MaxT",
        "TMIN" => "MinT",
        "DPT" => "Td",
        "APCP" => "QPF",
        "WDIR" => "WindDir",
        "WIND" => "WindSpd",
        "TCDC" => "Sky",
        "WVHGT" => "WaveHeight",
        "ASNOW" => "SnowAmt",
        "GUST" => "WindGust",
        "MAXRH" => "MaxRH",
        "HTSGW" => "WaveHeight",
        _ => return None,
    })
}

/// degrib's formatting of a level value: `%f` without trailing zeros.
fn level_value(v: f64) -> String {
    let s = format!("{v:.6}");
    let s = s.trim_end_matches('0');
    s.strip_suffix('.').unwrap_or(s).to_string()
}

/// `GRIB_SHORT_NAME` and the band description: the first fixed surface, as `ParseLevelName`
/// names it.
fn level_names(f: &Field, p: &Product) -> (String, String) {
    if matches!(p.template, 20 | 30 | 32 | 254 | 1000 | 1001 | 1002) {
        return ("0 undefined".into(), "0.000[-] undefined ()".into());
    }
    let s4 = f.sect4;
    let o = p.offset;
    let surface = |t: usize| -> (i32, Option<f64>) {
        let kind = uoct(s4, o + t, 1);
        let scale = soct(s4, o + t + 1, 1);
        let value = soct(s4, o + t + 2, 4);
        let missing = value == -0x7fff_ffff || scale == -127 || kind == 255;
        (kind, (!missing).then(|| value as f64 * 10f64.powi(-scale)))
    };
    let (kind, first) = surface(23);
    let first = first.unwrap_or(0.0);
    let (_, second) = surface(29);
    let (reserved, short, name, unit) = level_lookup(kind, p.center);
    let v = level_value(first);
    match (second, reserved) {
        (Some(s), true) => {
            let s = level_value(s);
            (format!("{v}-{s}-{short}({kind})"), format!("{v}-{s}[{unit}] {short}({kind}) ({name})"))
        }
        (Some(s), false) => {
            let s = level_value(s);
            (format!("{v}-{s}-{short}"), format!("{v}-{s}[{unit}] {short}=\"{name}\""))
        }
        (None, true) => (format!("{v}-{short}({kind})"), format!("{v}[{unit}] {short}({kind}) ({name})")),
        (None, false) => (format!("{v}-{short}"), format!("{v}[{unit}] {short}=\"{name}\"")),
    }
}

/// degrib's `Table45Lookup`: whether reserved, the short name, name and unit of a surface.
fn level_lookup(code: i32, center: i32) -> (bool, String, String, String) {
    if (192..255).contains(&code) && center != 7 {
        return (true, "RESERVED".into(), "Reserved Local use".into(), "-".into());
    }
    match tables().levels.get(&code) {
        Some((short, name, unit)) => {
            let reserved = (192..255).contains(&code) && name == "Reserved for local use";
            (reserved, if reserved { "RESERVED".into() } else { short.clone() }, name.clone(), unit.clone())
        }
        None => (false, "RESERVED".into(), "Reserved".into(), "-".into()),
    }
}

/// `GRIB_IDS`, from the identification section, as `FindPDSTemplateGRIB2` lists it.
pub(crate) fn ids(s1: &[u8]) -> String {
    if s1.len() < 21 {
        return String::new();
    }
    let t = tables();
    let mut out: Vec<String> = Vec::new();
    let center = uoct(s1, 6, 2);
    if center != 255 && center != 65535 {
        match t.centers.get(&center).filter(|n| !n.is_empty()) {
            Some(name) => out.push(format!("CENTER={center}({name})")),
            None => out.push(format!("CENTER={center}")),
        }
    }
    let sub = uoct(s1, 8, 2);
    if sub != 65535 {
        match t.subcenters.get(&(center, sub)) {
            Some(name) => out.push(format!("SUBCENTER={sub}({name})")),
            None => out.push(format!("SUBCENTER={sub}")),
        }
    }
    out.push(format!("MASTER_TABLE={}", s1[9]));
    out.push(format!("LOCAL_TABLE={}", s1[10]));
    let named = |code: u8, names: &[&str]| match names.get(code as usize) {
        Some(n) => format!("{code}({})", n.replace(' ', "_")),
        None => code.to_string(),
    };
    out.push(format!(
        "SIGNF_REF_TIME={}",
        named(s1[11], &["Analysis", "Start of Forecast", "Verifying time of forecast", "Observation time"])
    ));
    out.push(format!("REF_TIME={:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", be(&s1[12..14]), s1[14], s1[15], s1[16], s1[17], s1[18]));
    out.push(format!(
        "PROD_STATUS={}",
        named(
            s1[19],
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
    out.push(format!(
        "TYPE={}",
        named(
            s1[20],
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
    out.join(" ")
}

/// How a product definition template grows (g2clib's `extpdstemplate`): the index of the value
/// counting the repeated groups, the octets of a group, and how many groups the map holds already.
type Repeat = (usize, &'static [i8], i64);

/// The map of a product definition template (g2clib's `pdstemplates.c`: octets of each value,
/// negative for signed values), and how it grows with its repeated groups.
fn pds_map(template: i32) -> Option<(&'static [i8], Option<Repeat>)> {
    const BASE: [i8; 15] = [1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4];
    const T0: [i8; 15] = BASE;
    const T1: [i8; 18] = [1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1];
    const T2: [i8; 17] = [1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1];
    const T5: [i8; 22] = [1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1, -1, -4, -1, -4];
    const T6: [i8; 16] = [1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1];
    const T8: [i8; 29] = [1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 2, 1, 1, 1, 1, 1, 1, 4, 1, 1, 1, 4, 1, 4];
    const T9: [i8; 36] =
        [1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1, -1, -4, -1, -4, 2, 1, 1, 1, 1, 1, 1, 4, 1, 1, 1, 4, 1, 4];
    const T10: [i8; 30] = [1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 2, 1, 1, 1, 1, 1, 1, 4, 1, 1, 1, 4, 1, 4];
    const T11: [i8; 32] = [1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1, 2, 1, 1, 1, 1, 1, 1, 4, 1, 1, 1, 4, 1, 4];
    const T12: [i8; 31] = [1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 2, 1, 1, 1, 1, 1, 1, 4, 1, 1, 1, 4, 1, 4];
    const T15: [i8; 18] = [1, 1, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1];
    const T40: [i8; 16] = [1, 1, 2, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4];
    const T41: [i8; 19] = [1, 1, 2, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4, 1, 1, 1];
    // Simulated satellite products: per contributing spectral band, five values.
    const T32: [i8; 10] = [1, 1, 1, 1, 1, 2, 1, 1, -4, 1];
    // Aerosols, optical properties.
    const T48: [i8; 26] = [1, 1, 2, 1, -1, -4, -1, -4, 1, -1, -4, -1, -4, 1, 1, 1, 2, 1, 1, -4, 1, -1, -4, 1, -1, -4];
    const TIME_RANGE: &[i8] = &[1, 1, 1, 4, 1, 4];
    const BAND: &[i8] = &[2, 2, 2, -1, -4];
    Some(match template {
        0 | 7 => (&T0, None),
        1 => (&T1, None),
        2 => (&T2, None),
        5 => (&T5, None),
        6 => (&T6, None),
        8 => (&T8, Some((21, TIME_RANGE, 1))),
        9 => (&T9, Some((28, TIME_RANGE, 1))),
        10 => (&T10, Some((22, TIME_RANGE, 1))),
        11 => (&T11, Some((24, TIME_RANGE, 1))),
        12 => (&T12, Some((23, TIME_RANGE, 1))),
        32 => (&T32, Some((9, BAND, 0))),
        48 => (&T48, None),
        15 => (&T15, None),
        40 => (&T40, None),
        41 => (&T41, None),
        _ => return None,
    })
}

/// `GRIB_PDS_TEMPLATE_ASSEMBLED_VALUES`: the template's values, as g2clib's `g2_unpack4` reads
/// them, when the template is one of the common ones and its size is what its map says.
pub(crate) fn assembled_values(s4: &[u8]) -> Option<String> {
    let template = uoct(s4, 8, 2);
    let (map, repeat) = pds_map(template)?;
    let coords = uoct(s4, 6, 2) as usize;
    let found = s4.len().checked_sub(9 + coords * 4)?;
    let mut entries: Vec<i8> = map.to_vec();
    let mut pos = 10;
    let mut values: Vec<String> = Vec::new();
    let mut i = 0;
    while i < entries.len() {
        let size = entries[i].unsigned_abs() as usize;
        if pos + size - 1 > s4.len() {
            return None;
        }
        let v = if entries[i] < 0 { soct(s4, pos, size) as i64 } else { uoct(s4, pos, size) as i64 };
        values.push(if entries[i] == 4 { (v as i32 as u32).to_string() } else { v.to_string() });
        if let Some((at, group, held)) = repeat
            && at == i
        {
            for _ in held..v {
                entries.extend_from_slice(group);
            }
        }
        pos += size;
        i += 1;
    }
    let total: usize = entries.iter().map(|e| e.unsigned_abs() as usize).sum();
    (total == found).then(|| values.join(" "))
}

/// degrib's `sbit_2Comp_fourByte`: a negative 32-bit value, read as two's complement, taken as the
/// smaller of its two's complement and sign-and-magnitude readings.
fn sbit_2comp_4(data: i32) -> i32 {
    if data == -0x7fff_ffff || data >= 0 {
        return data;
    }
    if data == i32::MIN {
        return 0;
    }
    let (x, y) = ((!data).wrapping_add(1), data & 0x7fff_ffff);
    if x < y { -x } else { -y }
}

/// degrib's `sbit_2Comp_oneByte`.
fn sbit_2comp_1(data: i8) -> i8 {
    if data == -127 || data >= 0 {
        return data;
    }
    let (x, y) = ((!data).wrapping_add(1), data & 0x7f);
    if x < y { x.wrapping_neg() } else { y.wrapping_neg() }
}
