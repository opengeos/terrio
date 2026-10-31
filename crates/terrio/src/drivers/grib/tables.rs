// SPDX-FileCopyrightText: Copyright (c) 2007, ITC
// SPDX-FileCopyrightText: Copyright (c) 2008-2017, Even Rouault <even dot rouault at spatialys dot com>
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL (bundled NOAA MDL degrib) v3.12.2 (public domain):
//     frmts/grib/degrib/degrib/inventory.cpp
//     frmts/grib/degrib/degrib/metaname.cpp
//     frmts/grib/degrib/degrib/metaparse.cpp
//   GDAL v3.12.2:
//     frmts/grib/gribdataset.cpp

//! The band metadata GDAL's GRIB driver reports (`gribdataset.cpp`, degrib's `inventory.cpp`),
//! and the unit conversions degrib applies with `GRIB_NORMALIZE_UNITS=YES` (GDAL's default).

use super::{Field, be, names, uoct};
use crate::BandMetadata;

/// A unit conversion degrib applies to values (`ComputeUnit`, metric units).
#[derive(Debug, Clone, Copy)]
pub(crate) enum Conversion {
    /// Kelvin to Celsius: `v - 273.15` (`UC_K2F`).
    KelvinToCelsius,
    /// `10^v` (`UC_LOG10`).
    Pow10,
}

impl Conversion {
    /// The unit after conversion, when it replaces the table's.
    pub(crate) fn unit(self) -> Option<&'static str> {
        match self {
            Conversion::KelvinToCelsius => Some("[C]"),
            Conversion::Pow10 => None,
        }
    }

    /// Converts a value, as `ParseGrid` does (`unitM * value + unitB`, or `pow(10, value)`).
    pub(crate) fn apply(self, v: f64) -> f64 {
        match self {
            Conversion::KelvinToCelsius => 1.0 * v + -273.15,
            Conversion::Pow10 => 10f64.powf(v),
        }
    }
}

/// The conversion degrib applies to a field's values, if any: metric units convert only
/// temperatures (and log10 concentrations).
pub(crate) fn unit_conversion(f: &Field) -> Option<Conversion> {
    if !f.normalize {
        return None;
    }
    match names::conversion_code(f)?.as_str() {
        "UC_K2F" => Some(Conversion::KelvinToCelsius),
        "UC_LOG10" => Some(Conversion::Pow10),
        _ => None,
    }
}

/// Seconds since 1970 of a date and time (UTC).
fn epoch(year: i64, month: i64, day: i64, hour: i64, minute: i64, second: i64) -> i64 {
    // Days from the civil calendar (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146097 + doe - 719468) * 86400 + hour * 3600 + minute * 60 + second
}

/// The time at octets `o..o + 7` of a section (year on two octets, then month, day, hour,
/// minute, second), as degrib's `InventoryParseTime` reads it.
fn time_at(s: &[u8], o: usize) -> Option<i64> {
    let b = s.get(o - 1..o + 6)?;
    let year = be(&b[0..2]) as i64;
    let (month, day, hour, minute, second) = (b[2] as i64, b[3] as i64, b[4] as i64, b[5] as i64, b[6] as i64);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 24 || minute > 60 || second > 61 {
        return None;
    }
    Some(epoch(year, month, day, hour, minute, second))
}

/// Seconds in `delt` units of code table 4.4, as degrib's `ParseSect4Time2sec` (months and longer
/// are not converted).
fn time_to_sec(delt: i32, unit: u8) -> Option<f64> {
    const UNIT2SEC: [i64; 14] = [60, 3600, 86400, 0, 0, 0, 0, 0, 0, 0, 10800, 21600, 43200, 1];
    let s = *UNIT2SEC.get(unit as usize)?;
    (s != 0).then_some(delt as f64 * s as f64)
}

/// The reference time, valid time and forecast seconds of a field, as degrib's `GRIB2Inventory`
/// computes them: the valid time of statistically processed products is the end of their
/// overall time interval.
pub(crate) fn times(f: &Field) -> (f64, f64, f64) {
    let reference = time_at(f.sect1, 13).unwrap_or(0) as f64;
    let s4 = f.sect4;
    let template = uoct(s4, 8, 2);
    if matches!(template, 20 | 30 | 254) {
        return (reference, reference, 0.0);
    }
    let offset = match template {
        40 => 2,
        48 => 24,
        _ => 0,
    };
    let unit = uoct(s4, offset + 18, 1) as u8;
    let delt = be(s4.get(offset + 18..offset + 22).unwrap_or(&[0; 4])) as u32 as i32;
    let fore = time_to_sec(delt, unit).unwrap_or(0.0);
    let mut valid = reference + fore;
    let end = match template {
        8 => Some(35),
        9 => Some(48),
        10 => Some(36),
        11 => Some(38),
        12 => Some(37),
        _ => None,
    };
    if let Some(t) = end.and_then(|o| time_at(s4, o)) {
        valid = t as f64;
    }
    (reference, valid, fore)
}

/// The names of code table 0.0 (disciplines), as `FindPDSTemplateGRIB2` lists them.
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

/// The description and metadata of a band, as GDAL's GRIB driver reports them.
pub(crate) fn band_metadata(f: &Field) -> BandMetadata {
    let t = times(f);
    let n = names::lookup(f, unit_conversion(f).and_then(Conversion::unit), t);
    // degrib's inventory gives the fields of templates it does not name no valid time.
    let (reference, valid, fore) = if names::is_named(f) { t } else { (t.0, 0.0, 0.0) };
    let mut items: Vec<(String, String)> = vec![
        ("GRIB_UNIT".into(), n.unit),
        ("GRIB_COMMENT".into(), n.comment),
        ("GRIB_ELEMENT".into(), n.element),
        ("GRIB_SHORT_NAME".into(), n.short_level),
        ("GRIB_REF_TIME".into(), format!("{reference:.0}")),
        ("GRIB_VALID_TIME".into(), format!("{valid:.0}")),
        ("GRIB_FORECAST_SECONDS".into(), format!("{fore:.0}")),
        ("GRIB_DISCIPLINE".into(), discipline(f.discipline)),
        ("GRIB_IDS".into(), names::ids(f.sect1)),
        ("GRIB_PDS_PDTN".into(), uoct(f.sect4, 8, 2).to_string()),
        ("GRIB_PDS_TEMPLATE_NUMBERS".into(), template_numbers(f.sect4)),
    ];
    if let Some(values) = names::assembled_values(f.sect4) {
        items.push(("GRIB_PDS_TEMPLATE_ASSEMBLED_VALUES".into(), values));
    }
    // GDAL's GRIB bands have no color interpretation, the first included.
    BandMetadata { description: Some(n.long_level), color_interp: Some("Undefined".into()), items, ..Default::default() }
}

/// `GRIB_DISCIPLINE`: the code and, for known disciplines, its name.
fn discipline(code: u8) -> String {
    match DISCIPLINES.get(code as usize) {
        Some(name) => format!("{code}({})", name.replace(' ', "_")),
        None => code.to_string(),
    }
}

/// `GRIB_PDS_TEMPLATE_NUMBERS`: the template's octets (from octet 10, without the coordinate
/// values), in decimal.
fn template_numbers(s4: &[u8]) -> String {
    let coords = uoct(s4, 6, 2) as usize;
    let len = s4.len().saturating_sub(9).saturating_sub(coords * 4);
    s4[9..9 + len].iter().map(|b| b.to_string()).collect::<Vec<_>>().join(" ")
}
