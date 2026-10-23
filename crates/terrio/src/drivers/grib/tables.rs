//! The tables GDAL's GRIB driver names messages with, and its naming rules: a port of degrib's
//! `metaname.cpp` (element names, comments, units and level names) as GDAL 3.12 ships it.
//!
//! The tables are GDAL's (`frmts/grib/data/*.csv` and degrib's `grib1tab.cpp`), packed by
//! `scripts/gen_grib_tables.py` into `data/grib_tables.txt.zst` and parsed on first use.

use std::collections::HashMap;
use std::sync::OnceLock;

static DATA: &[u8] = include_bytes!("../../../data/grib_tables.txt.zst");

/// The parsed tables: GRIB2 CSV files by name (rows of fields, the header row first), GRIB1
/// parameter tables by name (256 entries each) and GRIB1's level table.
pub(super) struct Tables {
    csv: HashMap<String, Vec<Vec<String>>>,
    pub(super) grib1: HashMap<String, Vec<Grib1Parm>>,
    pub(super) grib1_surface: Vec<Grib1Surface>,
}

/// An entry of a GRIB1 parameter table.
#[derive(Debug, Clone)]
pub(super) struct Grib1Parm {
    pub(super) name: String,
    pub(super) comment: String,
    pub(super) unit: String,
    pub(super) convert: UnitConvert,
}

/// An entry of GRIB1's level table.
#[derive(Debug, Clone)]
pub(super) struct Grib1Surface {
    pub(super) name: String,
    pub(super) comment: String,
    pub(super) unit: String,
    pub(super) two_part: bool,
}

/// degrib's unit conversions (`unit_convert`). GDAL normalizes units to metric
/// (`GRIB_NORMALIZE_UNITS=YES`), which only converts Kelvin to Celsius and log10 values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum UnitConvert {
    None,
    K2F,
    InchWater,
    M2Feet,
    M2Inch,
    MS2Knots,
    Log10,
    UVIndex,
    M2StatuteMile,
}

impl UnitConvert {
    fn parse(s: &str) -> Self {
        match s.trim() {
            "UC_K2F" => Self::K2F,
            "UC_InchWater" => Self::InchWater,
            "UC_M2Feet" => Self::M2Feet,
            "UC_M2Inch" => Self::M2Inch,
            "UC_MS2Knots" => Self::MS2Knots,
            "UC_LOG10" => Self::Log10,
            "UC_UVIndex" => Self::UVIndex,
            "UC_M2StatuteMile" => Self::M2StatuteMile,
            _ => Self::None,
        }
    }

    /// The equation `y = m x + b` converting values to metric units (degrib's `ComputeUnit` with
    /// `f_unit = 2`), `None` when values are kept; `m = -10` means `10^x`.
    pub(super) fn metric(self) -> Option<(f64, f64)> {
        match self {
            Self::K2F => Some((1.0, -273.15)),
            Self::Log10 => Some((-10.0, 0.0)),
            _ => None,
        }
    }
}

/// The tables, parsed on first use.
pub(super) fn tables() -> &'static Tables {
    static TABLES: OnceLock<Tables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut text = String::new();
        if let Ok(mut d) = ruzstd::decoding::StreamingDecoder::new(&mut &DATA[..]) {
            let _ = std::io::Read::read_to_string(&mut d, &mut text);
        }
        parse(&text)
    })
}

fn parse(text: &str) -> Tables {
    let mut t = Tables { csv: HashMap::new(), grib1: HashMap::new(), grib1_surface: Vec::new() };
    let mut sections: Vec<(&str, Vec<&str>)> = Vec::new();
    for line in text.lines() {
        if let Some(name) = line.strip_prefix('@') {
            sections.push((name, Vec::new()));
        } else if let Some((_, lines)) = sections.last_mut() {
            lines.push(line);
        }
    }
    for (name, lines) in sections {
        if name == "grib1:surface" {
            t.grib1_surface = lines
                .iter()
                .map(|l| {
                    let f: Vec<&str> = l.split('\t').collect();
                    let get = |i: usize| f.get(i).copied().unwrap_or_default().to_string();
                    Grib1Surface { name: get(1), comment: get(2), unit: get(3), two_part: get(4) == "1" }
                })
                .collect();
        } else if let Some(table) = name.strip_prefix("grib1:") {
            let entries = lines
                .iter()
                .map(|l| {
                    let f: Vec<&str> = l.split('\t').collect();
                    let get = |i: usize| f.get(i).copied().unwrap_or_default().to_string();
                    Grib1Parm { name: get(1), comment: get(2), unit: get(3), convert: UnitConvert::parse(&get(4)) }
                })
                .collect();
            t.grib1.insert(table.to_string(), entries);
        } else {
            t.csv.insert(name.to_string(), lines.iter().map(|l| csv_fields(l)).collect());
        }
    }
    t
}

/// The fields of a CSV line: separated by commas, quoted with `"` (a doubled quote inside quotes
/// is a quote), as GDAL's CSV reader splits them.
fn csv_fields(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

impl Tables {
    /// The rows of a CSV table (without its header) with the indices of the named columns.
    fn rows(&self, file: &str, columns: &[&str]) -> Option<(Vec<usize>, &[Vec<String>])> {
        let rows = self.csv.get(file)?;
        let header = rows.first()?;
        let idx: Option<Vec<usize>> = columns.iter().map(|c| header.iter().position(|h| h == c)).collect();
        Some((idx?, &rows[1..]))
    }

    /// The center's name (`grib2_center.csv`).
    pub(super) fn center(&self, center: u16) -> Option<&str> {
        let (i, rows) = self.rows("grib2_center.csv", &["code", "name"])?;
        rows.iter().find(|r| atoi(&r[i[0]]) == i64::from(center)).map(|r| r[i[1]].as_str()).filter(|s| !s.is_empty())
    }

    /// The subcenter's name (`grib2_subcenter.csv`).
    pub(super) fn subcenter(&self, center: u16, subcenter: u16) -> Option<&str> {
        let (i, rows) = self.rows("grib2_subcenter.csv", &["center_code", "subcenter_code", "name"])?;
        rows.iter().find(|r| atoi(&r[i[0]]) == i64::from(center) && atoi(&r[i[1]]) == i64::from(subcenter)).map(|r| r[i[2]].as_str())
    }

    /// degrib's `GetGrib2Table4_2_Record`: the parameter in the WMO tables (merged with degrib's).
    fn table_4_2(&self, prod: u8, cat: u8, subcat: u8) -> Option<Param<'_>> {
        let file = format!("grib2_table_4_2_{prod}_{cat}.csv");
        let (i, rows) = self.rows(&file, &["subcat", "short_name", "name", "unit", "unit_conv"])?;
        let r = rows.iter().find(|r| atoi(&r[i[0]]) == i64::from(subcat))?;
        // Short names are missing from WMO-only entries: the long name then.
        let short = if r[i[1]].is_empty() { &r[i[2]] } else { &r[i[1]] };
        Some(Param { short, name: &r[i[2]], unit: &r[i[3]], convert: UnitConvert::parse(&r[i[4]]) })
    }

    /// degrib's `GetGrib2LocalTable4_2_Record`: the parameter in the center's local table.
    fn local_4_2(&self, center: u16, subcenter: u16, prod: u8, cat: u8, subcat: u8) -> Option<Param<'_>> {
        let (i, rows) = self.rows("grib2_table_4_2_local_index.csv", &["center_code", "subcenter_code", "filename"])?;
        let file = rows
            .iter()
            .find(|r| atoi(&r[i[0]]) == i64::from(center) && (r[i[1]].is_empty() || atoi(&r[i[1]]) == i64::from(subcenter)))
            .map(|r| r[i[2]].clone())?;
        let (i, rows) = self.rows(&file, &["prod", "cat", "subcat", "short_name", "name", "unit", "unit_conv"])?;
        let r = rows
            .iter()
            .find(|r| atoi(&r[i[0]]) == i64::from(prod) && atoi(&r[i[1]]) == i64::from(cat) && atoi(&r[i[2]]) == i64::from(subcat))?;
        Some(Param { short: &r[i[3]], name: &r[i[4]], unit: &r[i[5]], convert: UnitConvert::parse(&r[i[6]]) })
    }

    /// degrib's `Table45Lookup` (as GDAL changed it): the fixed surface's short name, name and
    /// unit, and whether the code is reserved.
    fn table_4_5(&self, code: u8, center: u16) -> (bool, &str, &str, &str) {
        if code > 191 && code < 255 && center != 7 {
            return (true, "RESERVED", "Reserved Local use", "-");
        }
        let Some((i, rows)) = self.rows("grib2_table_4_5.csv", &["code", "short_name", "name", "unit"]) else {
            return (false, "RESERVED", "Reserved", "-");
        };
        match rows.iter().find(|r| atoi(&r[i[0]]) == i64::from(code)) {
            Some(r) => {
                let reserved = code > 191 && code < 255 && r[i[2]] == "Reserved for local use";
                (reserved, if reserved { "RESERVED" } else { &r[i[1]] }, &r[i[2]], &r[i[3]])
            }
            None => (false, "RESERVED", "Reserved", "-"),
        }
    }
}

/// C's `atoi`: the leading integer of a string, 0 without one.
fn atoi(s: &str) -> i64 {
    let s = s.trim_start();
    let (sign, rest) = match s.as_bytes().first() {
        Some(b'-') => (-1, &s[1..]),
        Some(b'+') => (1, &s[1..]),
        _ => (1, s),
    };
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    sign * digits.parse::<i64>().unwrap_or(0)
}

/// A parameter of a table 4.2.
struct Param<'a> {
    short: &'a str,
    name: &'a str,
    unit: &'a str,
    convert: UnitConvert,
}

/// C's `printf("%g")`: 6 significant digits, trailing zeros removed, scientific notation for
/// exponents below -4 or from 6.
pub(super) fn fmt_g(v: f64) -> String {
    if v == 0.0 {
        return if v.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    if !v.is_finite() {
        return if v.is_nan() {
            "nan".into()
        } else if v > 0.0 {
            "inf".into()
        } else {
            "-inf".into()
        };
    }
    let sci = format!("{v:.5e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let strip =
        |s: &str| -> String { if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s.to_string() } };
    if !(-4..6).contains(&exp) {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", strip(mantissa), exp.abs())
    } else {
        strip(&format!("{v:.*}", (5 - exp).max(0) as usize))
    }
}

/// `printf("%f")` with trailing zeros (and a trailing point) removed, as degrib writes levels.
fn fmt_level(v: f64) -> String {
    let s = format!("{v:.6}");
    let s = s.trim_end_matches('0');
    s.strip_suffix('.').unwrap_or(s).to_string()
}

/// What degrib's `ParseElemName` needs to name a GRIB2 field.
#[derive(Debug, Clone, Default)]
pub(super) struct ElemInput {
    pub(super) master_version: u8,
    pub(super) center: u16,
    pub(super) subcenter: u16,
    pub(super) prod_type: u8,
    pub(super) template: u16,
    pub(super) cat: u8,
    pub(super) subcat: u8,
    pub(super) len_time: i32,
    pub(super) time_range_unit: u8,
    pub(super) stat_process: u8,
    pub(super) time_incr_type: u8,
    pub(super) gen_id: u8,
    pub(super) prob_type: u8,
    pub(super) lower_prob: f64,
    pub(super) upper_prob: f64,
    /// Table 4.7 (derived forecasts), when the template has one.
    pub(super) derived_fcst: Option<u8>,
    pub(super) percentile: i8,
    pub(super) gen_process: u8,
    pub(super) fst_value: Option<f64>,
    pub(super) snd_value: Option<f64>,
}

/// A GRIB2 field's name: degrib's element, comment and unit, and its unit conversion.
#[derive(Debug, Clone)]
pub(super) struct ElemName {
    pub(super) element: String,
    pub(super) comment: String,
    pub(super) unit: String,
    pub(super) convert: UnitConvert,
}

fn is_ndfd(center: u16, subcenter: u16) -> bool {
    center == 8 && (subcenter == 65535 || subcenter == 0)
}

fn is_mos(center: u16, subcenter: u16) -> bool {
    center == 7 && subcenter == 14
}

/// NDFD's names of some NCEP parameters.
const NDFD_OVERRIDE: &[(&str, &str)] = &[
    ("TMP", "T"),
    ("TMAX", "MaxT"),
    ("TMIN", "MinT"),
    ("DPT", "Td"),
    ("APCP", "QPF"),
    ("WDIR", "WindDir"),
    ("WIND", "WindSpd"),
    ("TCDC", "Sky"),
    ("WVHGT", "WaveHeight"),
    ("ASNOW", "SnowAmt"),
    ("GUST", "WindGust"),
    ("MAXRH", "MaxRH"),
    ("HTSGW", "WaveHeight"),
];

/// A label of a duration: `"%02d mon "`, `"%02d yr "` or `"%02d hr "` style prefixes, and the
/// suffix of names (`m`, `y` or nothing).
fn span(unit: u8, len: i32) -> (String, &'static str) {
    match unit {
        3 => (format!("{len:02} mon"), "m"),
        4 => (format!("{len:02} yr"), "y"),
        _ => (format!("{len:02} hr"), ""),
    }
}

/// degrib's `ParseElemName`.
pub(super) fn elem_name(e: &ElemInput) -> ElemName {
    let ndfd = is_ndfd(e.center, e.subcenter);
    let mut out = if e.template == 9 || e.template == 5 {
        if ndfd && e.prod_type == 0 && e.cat == 19 { elem_norm(e) } else { elem_prob(e) }
    } else if e.template == 10 || e.template == 6 {
        elem_perc(e)
    } else {
        elem_norm(e)
    };
    if let Some(d) = e.derived_fcst
        && matches!(d, 2 | 3 | 4 | 5 | 7)
    {
        out.unit = match d {
            2 => "[stddev]",
            3 => "[stddev normalized]",
            4 => "[spread]",
            5 => "[large anomaly index]",
            _ => "[interquantile range]",
        }
        .to_string();
        out.convert = UnitConvert::None;
    }
    if e.gen_process == 6 || e.gen_process == 7 {
        out.convert = UnitConvert::None;
        out.element = "ERR".to_string();
        out.comment = format!("{} error {}", out.comment, out.unit);
    } else {
        out.comment = format!("{} {}", out.comment, out.unit);
    }
    out
}

/// degrib's `ElemNameProb`: probability templates (4.5, 4.9).
fn elem_prob(e: &ElemInput) -> ElemName {
    let t = tables();
    let (ndfd, mos) = (is_ndfd(e.center, e.subcenter), is_mos(e.center, e.subcenter));
    let unit = "[%]".to_string();
    let mut upper = e.upper_prob;
    let tmp = upper * 25.4;
    if upper > tmp || tmp > f64::from(i32::MAX) || tmp < f64::from(i32::MIN) || tmp.is_nan() {
        upper = 0.0;
    }
    let len = e.len_time;
    let (sp, suf) = span(e.time_range_unit, len);
    let done = |element: String, comment: String| ElemName { element, comment, unit: unit.clone(), convert: UnitConvert::None };
    if ndfd || mos {
        if e.prod_type == 0 && e.cat == 1 && e.subcat == 8 {
            let (name, comment) = if e.prob_type == 0 {
                if len > 0 {
                    (format!("ProbPrcpBlw{len:02}{suf}"), format!("{sp} Prob of Precip below average"))
                } else {
                    ("ProbPrcpBlw".to_string(), "Prob of precip below average".to_string())
                }
            } else if e.prob_type == 3 {
                if len > 0 {
                    (format!("ProbPrcpAbv{len:02}{suf}"), format!("{sp} Prob of Precip above average"))
                } else {
                    ("ProbPrcpAbv".to_string(), "Prob of precip above average".to_string())
                }
            } else if len > 0 {
                let code = (upper / 0.254 + 0.5) as i32;
                if e.time_range_unit == 3 || e.time_range_unit == 4 {
                    let name = if upper != 0.254 { format!("PoP{len:02}{suf}-{code:03}") } else { format!("PoP{len:02}{suf}") };
                    (name, format!("{sp} Prob of Precip > {} In.", fmt_g(upper / 25.4)))
                } else {
                    let name = if upper != 0.254 && upper != 300.0 { format!("PoP{len:02}-{code:03}") } else { format!("PoP{len:02}") };
                    let comment = if upper != 300.0 {
                        format!("{sp} Prob of Precip > {} In.", fmt_g(upper / 25.4))
                    } else {
                        format!("{sp} Prob of Precip > 0.01 In.")
                    };
                    (name, comment)
                }
            } else {
                let name = if upper != 0.254 { format!("PoP-p{:03}", (upper / 0.254 + 0.5) as i32) } else { "PoP".to_string() };
                (name, format!("Prob of Precip > {} In.", fmt_g(upper / 25.4)))
            };
            return done(name, comment);
        }
        if e.prod_type == 10 && e.cat == 3 && e.subcat == 192 {
            let k = (upper / 0.3048 + 0.5) as i32;
            let name = if e.time_incr_type == 2 { format!("ProbSurge{k:02}i") } else { format!("ProbSurge{k:02}c") };
            return done(name, format!("{sp} Prob of Hurricane Storm Surge > {} m", fmt_g(upper)));
        }
    }
    if ndfd && e.prod_type == 0 && e.cat == 2 && e.subcat == 1 {
        let k = (upper * 3600.0 / 1852.0 + 0.5) as i32;
        let name = if e.time_incr_type == 2 { format!("ProbWindSpd{k:02}i") } else { format!("ProbWindSpd{k:02}c") };
        return done(name, format!("{sp} Prob of Wind speed > {} m/s", fmt_g(upper)));
    }
    let generic = if e.master_version != 255 { t.table_4_2(e.prod_type, e.cat, e.subcat) } else { None }
        .filter(|p| p.name != "Reserved for local use");
    let (lower, upper) = (e.lower_prob, upper);
    if let Some(p) = generic {
        let (mut name, mut comment) = if len > 0 {
            (format!("Prob{}{len:02}{suf}", p.short), format!("{sp} Prob of {} ", p.name))
        } else {
            (format!("Prob{}", p.short), format!("Prob of {} ", p.name))
        };
        let tmp_ndfd = (ndfd || mos) && p.short == "TMP";
        let rename = |kind: &str| if len > 0 { format!("Prob{}{kind}{len:02}{suf}", p.short) } else { format!("Prob{}{kind}", p.short) };
        match e.prob_type {
            0 if tmp_ndfd => {
                comment.push_str("below average");
                name = rename("Blw");
            }
            0 => comment.push_str(&format!("< {} {}", fmt_g(lower), p.unit)),
            1 if tmp_ndfd => {
                comment.push_str("above average");
                name = rename("Abv");
            }
            1 => comment.push_str(&format!("> {} {}", fmt_g(upper), p.unit)),
            2 => comment.push_str(&format!(">= {}, < {} {}", fmt_g(lower), fmt_g(upper), p.unit)),
            3 if tmp_ndfd => {
                comment.push_str("above average");
                name = rename("Abv");
            }
            3 => comment.push_str(&format!("> {} {}", fmt_g(lower), p.unit)),
            4 if tmp_ndfd => {
                comment.push_str("below average");
                name = rename("Blw");
            }
            4 => comment.push_str(&format!("< {} {}", fmt_g(upper), p.unit)),
            _ => comment.push_str(p.unit),
        }
        return done(name, comment);
    }
    if let Some(p) = t.local_4_2(e.center, e.subcenter, e.prod_type, e.cat, e.subcat) {
        let (name, mut comment) = if len > 0 {
            (format!("Prob{}{len:02}{suf}", p.short), format!("{sp} Prob of {} ", p.name))
        } else {
            (format!("Prob{}", p.short), format!("Prob of {} ", p.name))
        };
        match e.prob_type {
            0 => comment.push_str(&format!("< {} {}", fmt_g(lower), p.unit)),
            1 => comment.push_str(&format!("> {} {}", fmt_g(upper), p.unit)),
            2 => comment.push_str(&format!(">= {}, < {} {}", fmt_g(lower), fmt_g(upper), p.unit)),
            3 => comment.push_str(&format!("> {} {}", fmt_g(lower), p.unit)),
            4 => comment.push_str(&format!("< {} {}", fmt_g(upper), p.unit)),
            _ => comment.push_str(p.unit),
        }
        return done(name, comment);
    }
    done("ProbUnknown".to_string(), format!("Prob of (prodType {}, cat {}, subcat {})", e.prod_type, e.cat, e.subcat))
}

/// degrib's `ElemNamePerc`: percentile templates (4.6, 4.10).
fn elem_perc(e: &ElemInput) -> ElemName {
    let t = tables();
    let len = e.len_time;
    let pct = e.percentile;
    let (sp, suf) = span(e.time_range_unit, len);
    let comment_of = |name: &str| {
        if len > 0 { format!("{sp} {name} Percentile({pct})") } else { format!("{name} Percentile({pct})") }
    };
    let generic = if e.master_version != 255 { t.table_4_2(e.prod_type, e.cat, e.subcat) } else { None }
        .filter(|p| p.name != "Reserved for local use");
    if let Some(p) = generic {
        let unit = format!("[{}]", p.unit);
        if is_ndfd(e.center, e.subcenter) || is_mos(e.center, e.subcenter) {
            if p.short == "ASNOW" {
                let name = format!("Snow{len:02}{suf}e{pct:02}{suf}");
                return ElemName { element: name, comment: format!("{sp} {} Percentile({pct})", p.name), unit, convert: p.convert };
            }
            if let Some((_, ndfd_name)) = NDFD_OVERRIDE.iter().find(|(g, _)| *g == p.short) {
                return ElemName { element: format!("{ndfd_name}{pct:02}"), comment: comment_of(p.name), unit, convert: p.convert };
            }
        }
        return ElemName { element: format!("{}{pct:02}", p.short), comment: comment_of(p.name), unit, convert: p.convert };
    }
    if let Some(p) = t.local_4_2(e.center, e.subcenter, e.prod_type, e.cat, e.subcat) {
        let b = p.short.as_bytes();
        let name = if b.len() >= 2 && b[b.len() - 1].is_ascii_digit() && b[b.len() - 2].is_ascii_digit() {
            p.short.to_string()
        } else if p.short == "Surge" || p.short == "SURGE" {
            format!("{}{:02}", p.short, 100 - i32::from(pct))
        } else {
            format!("{}{pct:02}", p.short)
        };
        return ElemName { element: name, comment: comment_of(p.name), unit: format!("[{}]", p.unit), convert: p.convert };
    }
    ElemName {
        element: "unknown".to_string(),
        comment: format!("(prodType {}, cat {}, subcat {})", e.prod_type, e.cat, e.subcat),
        unit: "[-]".to_string(),
        convert: UnitConvert::None,
    }
}

/// degrib's `ElemNameNorm`: the other templates.
fn elem_norm(e: &ElemInput) -> ElemName {
    let t = tables();
    let len = e.len_time;
    let (sp, suf) = span(e.time_range_unit, len);
    if e.prod_type == 0 && e.template == 8 && e.cat == 14 && e.subcat == 193 {
        let (element, comment) = if len > 0 {
            let label = match e.time_range_unit {
                3 => format!("{len} mon"),
                4 => format!("{len} yr"),
                _ => format!("{len} hr"),
            };
            (format!("Ozone{len:02}{suf}"), format!("{label} Average Ozone Concentration"))
        } else {
            ("AVGOZCON".to_string(), "Average Ozone Concentration".to_string())
        };
        return ElemName { element, comment, unit: "[PPB]".to_string(), convert: UnitConvert::None };
    }
    if e.center == 7
        && e.prod_type == 0
        && e.cat == 13
        && e.subcat == 195
        && let (Some(f), Some(s)) = (e.fst_value, e.snd_value)
    {
        let delt = f - s;
        let unit = "[log10(10^-6g/m^3)]".to_string();
        let pick = if e.gen_id == 6 {
            if (-100.0..=100.0).contains(&delt) {
                Some(("dusts", "Surface level dust"))
            } else if (-5000.0..=5000.0).contains(&delt) {
                Some(("dustc", "Average vertical column dust"))
            } else {
                None
            }
        } else if (-100.0..=100.0).contains(&delt) {
            Some(("smokes", "Surface level smoke from fires"))
        } else if (-5000.0..=5000.0).contains(&delt) {
            Some(("smokec", "Average vertical column smoke from fires"))
        } else {
            None
        };
        if let Some((n, c)) = pick {
            return ElemName { element: n.to_string(), comment: c.to_string(), unit, convert: UnitConvert::Log10 };
        }
    }
    let generic = if e.master_version != 255 { t.table_4_2(e.prod_type, e.cat, e.subcat) } else { None }
        .filter(|p| p.name != "Reserved for local use");
    if let Some(p) = generic {
        let unit = format!("[{}]", p.unit);
        let named = |element: String, comment: String| ElemName { element, comment, unit: unit.clone(), convert: p.convert };
        let mos = is_mos(e.center, e.subcenter);
        if mos && (p.short == "APCP" || p.short == "ASNOW") {
            let base = if p.short == "APCP" { "QPF" } else { "SnowAmt" };
            return named(format!("{base}{len:02}{suf}"), format!("{sp} {}", p.name));
        }
        if is_ndfd(e.center, e.subcenter) || mos {
            if p.short == "EVP" {
                return if e.stat_process == 10 {
                    named(format!("EvpDep{len:02}"), format!("{len:02} hr Evapo-Transpiration departure from normal"))
                } else {
                    named(format!("Evp{len:02}"), format!("{len:02} hr Evapo-Transpiration"))
                };
            }
            if let Some((_, ndfd_name)) = NDFD_OVERRIDE.iter().find(|(g, _)| *g == p.short) {
                return named(ndfd_name.to_string(), p.name.to_string());
            }
        }
        let accum = (e.prod_type == 1 && e.cat == 1 && e.subcat == 2)
            || (e.prod_type == 0 && e.cat == 19 && e.subcat == 2)
            || (e.prod_type == 0 && e.cat == 1 && e.subcat == 8)
            || (e.prod_type == 0 && e.cat == 19 && e.subcat == 203);
        return if accum && len > 0 {
            named(format!("{}{len:02}{suf}", p.short), format!("{sp} {}", p.name))
        } else {
            named(p.short.to_string(), p.name.to_string())
        };
    }
    if let Some(p) = t.local_4_2(e.center, e.subcenter, e.prod_type, e.cat, e.subcat) {
        return ElemName { element: p.short.to_string(), comment: p.name.to_string(), unit: format!("[{}]", p.unit), convert: p.convert };
    }
    ElemName {
        element: "unknown".to_string(),
        comment: format!("(prodType {}, cat {}, subcat {})", e.prod_type, e.cat, e.subcat),
        unit: "[-]".to_string(),
        convert: UnitConvert::None,
    }
}

/// degrib's `ParseLevelName`: the short (`2-HTGL`) and long (`2[m] HTGL="Specified height level
/// above ground"`) names of a GRIB2 field's level.
pub(super) fn level_name(center: u16, surf_type: u8, value: f64, snd: Option<f64>) -> (String, String) {
    let (reserved, short, name, unit) = tables().table_4_5(surf_type, center);
    let val = fmt_level(value);
    match snd {
        Some(s) => {
            let snd = fmt_level(s);
            if reserved {
                (format!("{val}-{snd}-{short}({surf_type})"), format!("{val}-{snd}[{unit}] {short}({surf_type}) ({name})"))
            } else {
                (format!("{val}-{snd}-{short}"), format!("{val}-{snd}[{unit}] {short}=\"{name}\""))
            }
        }
        None => {
            if reserved {
                (format!("{val}-{short}({surf_type})"), format!("{val}[{unit}] {short}({surf_type}) ({name})"))
            } else {
                (format!("{val}-{short}"), format!("{val}[{unit}] {short}=\"{name}\""))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_as_printf_g() {
        assert_eq!(fmt_g(273.0), "273");
        assert_eq!(fmt_g(0.254), "0.254");
        assert_eq!(fmt_g(0.01), "0.01");
        assert_eq!(fmt_g(1e-5), "1e-05");
        assert_eq!(fmt_g(1234567.0), "1.23457e+06");
        assert_eq!(fmt_g(100000.0), "100000");
        assert_eq!(fmt_g(-2.5), "-2.5");
        assert_eq!(fmt_level(2.0), "2");
        assert_eq!(fmt_level(0.5), "0.5");
        assert_eq!(fmt_level(100000.0), "100000");
    }

    #[test]
    fn names_fields_as_degrib() {
        let t = tables();
        assert_eq!(t.center(7), Some("US-NCEP"));
        let e = ElemInput { master_version: 2, center: 7, prod_type: 0, cat: 0, subcat: 0, ..Default::default() };
        let n = elem_name(&e);
        assert_eq!((n.element.as_str(), n.comment.as_str(), n.unit.as_str()), ("TMP", "Temperature [K]", "[K]"));
        assert_eq!(n.convert, UnitConvert::K2F);
        assert_eq!(level_name(7, 103, 2.0, None), ("2-HTGL".to_string(), "2[m] HTGL=\"Specified height level above ground\"".to_string()));
    }
}
