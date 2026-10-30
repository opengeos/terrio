// SPDX-FileCopyrightText: Copyright (c) 2017, Even Rouault <even dot rouault at spatialys dot com>
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.12.2:
//     frmts/grib/gribcreatecopy.cpp

//! A GRIB2 writer, as GDAL's GRIB driver writes (`gribcreatecopy.cpp`): one message per band,
//! with GDAL's sections, defaults and creation options, so that a file reads back in GDAL as the
//! file GDAL would have written.
//!
//! - Grids: latitude/longitude (template 3.0), transverse Mercator (3.12), Lambert conformal
//!   with two standard parallels (3.30) and Albers equal area (3.31); the earth's shape as GDAL
//!   writes it (WGS 84, GRS 80, a sphere, or the axes). Other projections are refused, as GDAL
//!   refuses those it does not write (and the ones it writes but terrio does not yet: Mercator,
//!   polar stereographic, Lambert azimuthal equal area, one-parallel Lambert conformal).
//! - Packings: simple (5.0) and IEEE floating point (5.4). `DATA_ENCODING=AUTO`, the default,
//!   chooses as GDAL does: IEEE for floating-point bands, simple packing for integer ones. GDAL
//!   writes bands whose nodata value occurs in the data with complex packing (5.2), which terrio
//!   does not write: such bands are refused with `AUTO`, and written as data otherwise, as GDAL
//!   writes them with an explicit encoding.
//! - Options, as GDAL names them, each also as `BAND_<n>_<KEY>`: `DISCIPLINE`, `IDS` and `IDS_*`,
//!   `PDS_PDTN`, `PDS_TEMPLATE_NUMBERS`, `INPUT_UNIT`, `DATA_ENCODING`, `NBITS`,
//!   `DECIMAL_SCALE_FACTOR`. As in GDAL, `DISCIPLINE`, `IDS`, `PDS_PDTN` and
//!   `PDS_TEMPLATE_NUMBERS` default to the band's `GRIB_*` metadata (a GRIB file copied to GRIB
//!   keeps its products), and temperatures in Celsius are written in Kelvin.
//!
//! Simple packing follows g2clib's `simpack` as GDAL calls it, in single precision, but for one
//! case: a range of exactly a power of two values, which `simpack` packs one bit short (the
//! largest values wrap to the smallest). terrio adds the bit.

use crate::dataset::geo_transform;
use crate::{BandMetadata, Error, RasterWriteOptions, Result};
use std::path::Path;
use wbraster::{DataType, Raster};

/// Writes `r` as a GRIB2 file, one message per band.
pub(crate) fn write(r: &Raster, nodata: Option<f64>, path: &Path, options: &RasterWriteOptions) -> Result<()> {
    let mut bytes = Vec::new();
    let opts = Options { co: &options.creation_options, bands: &options.band_metadata };
    if opts.flag("APPEND_SUBDATASET") && path.exists() {
        bytes = std::fs::read(path)?;
    }
    bytes.extend(to_bytes(r, nodata, &opts)?);
    std::fs::write(path, bytes)?;
    Ok(())
}

/// The creation options and band metadata a message is written with.
struct Options<'a> {
    co: &'a [(String, String)],
    bands: &'a [BandMetadata],
}

impl Options<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.co.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str())
    }

    /// GDAL's `CPLTestBool` of an option, false when unset.
    fn flag(&self, key: &str) -> bool {
        self.get(key).is_some_and(|v| !matches!(v.trim().to_ascii_uppercase().as_str(), "NO" | "FALSE" | "OFF" | "0"))
    }

    /// GDAL's `GetBandOption`: `BAND_<n>_<KEY>`, then `<KEY>`, then (with `tags`) the band's
    /// `GRIB_<KEY>` metadata item.
    fn band(&self, band: usize, key: &str, tags: bool) -> Option<&str> {
        self.get(&format!("BAND_{band}_{key}")).or_else(|| self.get(key)).or_else(|| {
            let item = format!("GRIB_{key}");
            let md = self.bands.get(band - 1).filter(|_| tags)?;
            md.items.iter().find(|(k, _)| k.eq_ignore_ascii_case(&item)).map(|(_, v)| v.as_str())
        })
    }

    /// GDAL's `GetIDSOption`: `IDS_<KEY>`, else the `KEY=value` of `IDS` (or of the band's
    /// `GRIB_IDS`).
    fn ids(&self, band: usize, key: &str) -> Option<String> {
        if let Some(v) = self.band(band, &format!("IDS_{key}"), false) {
            return Some(v.to_string());
        }
        let ids = self.band(band, "IDS", true)?;
        ids.split(' ').filter(|t| !t.is_empty()).find_map(|t| {
            let (k, v) = t.split_once('=')?;
            k.eq_ignore_ascii_case(key).then(|| v.to_string())
        })
    }
}

/// C's `atoi`: the leading integer of `s`, 0 when there is none.
fn atoi(s: &str) -> i64 {
    let t = s.trim_start();
    let (sign, digits) = match t.as_bytes().first() {
        Some(b'-') => (-1, &t[1..]),
        Some(b'+') => (1, &t[1..]),
        _ => (1, t),
    };
    let n: String = digits.chars().take_while(char::is_ascii_digit).take(18).collect();
    sign * n.parse::<i64>().unwrap_or(0)
}

/// GDAL's `Lon180to360`.
fn lon360(lon: f64) -> f64 {
    if lon == 180.0 { 180.0 } else { ((lon % 360.0) + 360.0) % 360.0 }
}

/// GRIB2's signed integers: sign bit and magnitude (GDAL's `WriteInt32`, `WriteInt16`).
fn int32(out: &mut Vec<u8>, v: i32) {
    let v = v.max(i32::MIN + 1);
    let u = if v < 0 { v.unsigned_abs() | 0x8000_0000 } else { v as u32 };
    out.extend(u.to_be_bytes());
}

fn int16(out: &mut Vec<u8>, v: i32) {
    let v = (v as i16).max(i16::MIN + 1);
    let u = if v < 0 { v.unsigned_abs() | 0x8000 } else { v as u16 };
    out.extend(u.to_be_bytes());
}

/// GDAL's `WriteScaled`: `floor(v / unit + 0.5)` as a signed integer.
fn scaled(out: &mut Vec<u8>, v: f64, unit: f64) {
    int32(out, (v / unit + 0.5).floor() as i32);
}

const MISSING4: u32 = 0xffff_ffff;

/// The parts of a WKT 1 CRS the writer needs.
struct Crs {
    /// `None` for a geographic CRS, else the WKT 1 projection name.
    projection: Option<String>,
    params: Vec<(String, f64)>,
    semi_major: f64,
    inv_flattening: f64,
    /// The linear unit of a projected CRS, in metres.
    linear_unit: f64,
    /// The angular unit, in degrees.
    angular_unit: f64,
    wkt: String,
}

impl Crs {
    fn parse(wkt: &str) -> Option<Crs> {
        let root = wkt1::parse(wkt)?;
        let (geogcs, projection, params, linear_unit) = match root.key.to_ascii_uppercase().as_str() {
            "GEOGCS" => (&root, None, Vec::new(), 1.0),
            "PROJCS" => {
                let params = root.children("PARAMETER").filter_map(|p| Some((p.name()?.to_string(), p.num(1)?))).collect();
                let unit = root.child("UNIT").and_then(|u| u.num(1)).unwrap_or(1.0);
                (root.child("GEOGCS")?, Some(root.child("PROJECTION")?.name()?.to_string()), params, unit)
            }
            _ => return None,
        };
        let spheroid = geogcs.child("DATUM")?.child("SPHEROID")?;
        let angular_unit = geogcs.child("UNIT").and_then(|u| u.num(1)).map_or(1.0, |r| r.to_degrees());
        Some(Crs {
            projection,
            params,
            semi_major: spheroid.num(1)?,
            inv_flattening: spheroid.num(2)?,
            linear_unit,
            angular_unit,
            wkt: wkt.to_string(),
        })
    }

    /// GDAL's `GetNormProjParm`, with `FindProjParm`'s alternate names for the latitude of
    /// origin and the central meridian (Albers' `latitude_of_center`, `longitude_of_center`).
    fn param(&self, name: &str) -> f64 {
        let find = |n: &str| self.params.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| *v);
        let v = find(name).or_else(|| match name {
            "latitude_of_origin" => find("latitude_of_center"),
            "central_meridian" => find("longitude_of_center").or_else(|| find("longitude_of_origin")),
            _ => None,
        });
        v.unwrap_or(0.0)
    }

    fn semi_minor(&self) -> f64 {
        if self.inv_flattening == 0.0 { self.semi_major } else { self.semi_major * (1.0 - 1.0 / self.inv_flattening) }
    }
}

/// A minimal WKT 1 reader: `KEYWORD[arg, ...]` trees of quoted strings, numbers and nodes, within
/// the bounds of terrio's CRS reader (`crs::wkt1`), which are GDAL's and PROJ's.
mod wkt1 {
    use crate::crs::wkt1::{MAX_LEN, MAX_LEVELS};

    pub(super) struct Node {
        pub key: String,
        pub args: Vec<Arg>,
    }

    pub(super) enum Arg {
        Str(String),
        Num(f64),
        Node(Node),
        Other,
    }

    impl Node {
        pub fn child(&self, key: &str) -> Option<&Node> {
            self.args.iter().find_map(|a| match a {
                Arg::Node(n) if n.key.eq_ignore_ascii_case(key) => Some(n),
                _ => None,
            })
        }

        pub fn children<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
            self.args.iter().filter_map(move |a| match a {
                Arg::Node(n) if n.key.eq_ignore_ascii_case(key) => Some(n),
                _ => None,
            })
        }

        pub fn name(&self) -> Option<&str> {
            match self.args.first() {
                Some(Arg::Str(s)) => Some(s),
                _ => None,
            }
        }

        pub fn num(&self, i: usize) -> Option<f64> {
            match self.args.get(i) {
                Some(Arg::Num(v)) => Some(*v),
                _ => None,
            }
        }
    }

    /// The tree of a WKT; `None` for text GDAL does not import (longer than `MAX_LEN` bytes, as
    /// `importFromWkt`, or with a value at level `MAX_LEVELS`, PROJ's "too many nesting levels"),
    /// for which GDAL's GRIB writer has no CRS to write either.
    pub(super) fn parse(s: &str) -> Option<Node> {
        if s.len() > MAX_LEN {
            return None;
        }
        let b = s.trim().as_bytes();
        let mut i = 0;
        let n = node(b, &mut i, 0)?;
        Some(n)
    }

    fn skip(b: &[u8], i: &mut usize) {
        while *i < b.len() && b[*i].is_ascii_whitespace() {
            *i += 1;
        }
    }

    /// The node at `b[*i..]`, at nesting `depth` (the root at 0); `*i` ends after its closer. As
    /// PROJ's `WKTNode::createFrom`: either closer ends a node, a value (a node, a string, a number
    /// or a word, one level deeper than the node holding it) at level `MAX_LEVELS` is refused, and
    /// a node that does not parse fails the whole text.
    fn node(b: &[u8], i: &mut usize, depth: usize) -> Option<Node> {
        skip(b, i);
        let start = *i;
        while *i < b.len() && (b[*i].is_ascii_alphanumeric() || b[*i] == b'_') {
            *i += 1;
        }
        let key = std::str::from_utf8(&b[start..*i]).ok()?.to_string();
        skip(b, i);
        if !matches!(b.get(*i)?, b'[' | b'(') {
            return None;
        }
        *i += 1;
        let mut args = Vec::new();
        loop {
            skip(b, i);
            match *b.get(*i)? {
                // Either closer ends a node, whichever opened it, as PROJ's WKT parser (and so
                // GDAL, and terrio's CRS reader) reads WKT.
                b']' | b')' => {
                    *i += 1;
                    return Some(Node { key, args });
                }
                b',' => {
                    *i += 1;
                    continue;
                }
                _ => {}
            }
            // A value of this node, one level deeper.
            if depth + 1 >= MAX_LEVELS {
                return None;
            }
            match b[*i] {
                b'"' => {
                    let end = b[*i + 1..].iter().position(|&c| c == b'"')? + *i + 1;
                    args.push(Arg::Str(std::str::from_utf8(&b[*i + 1..end]).ok()?.to_string()));
                    *i = end + 1;
                }
                c if c == b'-' || c == b'+' || c == b'.' || c.is_ascii_digit() => {
                    let start = *i;
                    while *i < b.len() && (b[*i].is_ascii_alphanumeric() || matches!(b[*i], b'-' | b'+' | b'.')) {
                        *i += 1;
                    }
                    args.push(std::str::from_utf8(&b[start..*i]).ok()?.parse().map_or(Arg::Other, Arg::Num));
                }
                _ => {
                    // A node (a keyword and its opener), or a bare word (`EAST`, `NORTH`) or other
                    // token, up to the next delimiter.
                    let save = *i;
                    while *i < b.len() && (b[*i].is_ascii_alphanumeric() || b[*i] == b'_') {
                        *i += 1;
                    }
                    skip(b, i);
                    let opens = matches!(b.get(*i), Some(b'[' | b'('));
                    *i = save;
                    if opens {
                        args.push(Arg::Node(node(b, i, depth + 1)?));
                    } else {
                        while *i < b.len() && !matches!(b[*i], b',' | b']' | b')') {
                            *i += 1;
                        }
                        // Every value consumes input (here a token starts with a byte that is no
                        // delimiter, so it does): a step that did not would fail the parse rather
                        // than collect the same byte again and again.
                        if *i == save {
                            return None;
                        }
                        args.push(Arg::Other);
                    }
                }
            }
        }
    }
}

/// The grid of every message: section 3, and the column at which rows are split and swapped
/// (a global latitude/longitude grid written from 0 degrees east).
struct Grid {
    section: Vec<u8>,
    split: usize,
}

fn grid(r: &Raster, crs: &Crs) -> Result<Grid> {
    let gt = geo_transform(r);
    let (nx, ny) = (r.cols, r.rows);
    let mut llx = gt[0] + gt[1] / 2.0;
    let mut lly = gt[3] + gt[5] / 2.0 + (ny as f64 - 1.0) * gt[5];
    let mut urx = gt[0] + gt[1] / 2.0 + (nx as f64 - 1.0) * gt[1];
    let mut ury = gt[3] + gt[5] / 2.0;
    if ury < lly {
        std::mem::swap(&mut ury, &mut lly);
    }
    let mut s = Vec::with_capacity(84);
    s.extend(MISSING4.to_be_bytes());
    s.push(3);
    s.push(0); // Source of grid definition: code table 3.1
    s.extend(((nx as u64 * ny as u64) as u32).to_be_bytes());
    s.push(0); // No optional list of numbers of points
    s.push(0);
    let unsupported = |what: &str| Error::Unsupported(format!("GRIB2 writer: {what}"));
    if crs.angular_unit != 0.0 && (crs.angular_unit - 1.0).abs() > 1e-12 {
        return Err(unsupported("angular units other than degrees"));
    }
    let mut split = 0;
    let ellipsoid = |s: &mut Vec<u8>| {
        let (a, rf) = (crs.semi_major, crs.inv_flattening);
        if (a - 6378137.0).abs() < 0.01 && (rf - 298.257223563).abs() < 1e-9 {
            s.push(5);
            s.extend([0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        } else if (a - 6378137.0).abs() < 0.01 && (rf - 298.257222101).abs() < 1e-9 {
            s.push(4);
            s.extend([0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        } else if rf == 0.0 {
            s.extend([1, 2]);
            s.extend(((a * 100.0 + 0.5) as u32).to_be_bytes());
            s.extend([0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
        } else {
            s.push(7);
            s.extend([0xff, 0xff, 0xff, 0xff, 0xff, 2]);
            s.extend(((a * 100.0 + 0.5) as u32).to_be_bytes());
            s.push(2);
            s.extend(((crs.semi_minor() * 100.0 + 0.5) as u32).to_be_bytes());
        }
        s.extend((nx as u32).to_be_bytes());
        s.extend((ny as u32).to_be_bytes());
    };
    const DEG: f64 = 1e-6;
    match crs.projection.as_deref() {
        None => {
            s.extend(0u16.to_be_bytes());
            ellipsoid(&mut s);
            let adjust = crate::config::get("GRIB_ADJUST_LONGITUDE_RANGE")
                .is_none_or(|v| !matches!(v.trim().to_ascii_uppercase().as_str(), "NO" | "FALSE" | "OFF" | "0"));
            if llx < 0.0 && adjust {
                let orig = llx;
                llx = lon360(llx);
                urx = lon360(urx);
                if llx > urx && (360.0 - nx as f64 * gt[1]).abs() < gt[1] / 4.0 {
                    // A global grid: rewrapped around the prime meridian.
                    split = ((0.0 - orig) / gt[1]).ceil() as usize;
                    llx = 0.0;
                    urx = 360.0 - gt[1];
                }
            }
            s.extend(0u32.to_be_bytes()); // Basic angle
            s.extend(MISSING4.to_be_bytes()); // Subdivisions of the basic angle
            scaled(&mut s, lly, DEG);
            scaled(&mut s, llx, DEG);
            s.push(0x30); // Resolution and component flags
            scaled(&mut s, ury, DEG);
            scaled(&mut s, urx, DEG);
            scaled(&mut s, gt[1], DEG);
            scaled(&mut s, gt[5].abs(), DEG);
            s.push(0x40); // Scanning mode: bottom to top
        }
        Some(p) => {
            if (crs.linear_unit - 1.0).abs() > 1e-12 {
                return Err(unsupported("projected CRSs in units other than metres"));
            }
            match p.to_ascii_lowercase().as_str() {
                "transverse_mercator" => {
                    s.extend(12u16.to_be_bytes());
                    ellipsoid(&mut s);
                    scaled(&mut s, crs.param("latitude_of_origin"), DEG);
                    scaled(&mut s, lon360(crs.param("central_meridian")), DEG);
                    s.push(0x30);
                    s.extend((crs.param("scale_factor") as f32).to_bits().to_be_bytes());
                    const CM: f64 = 1e-2;
                    scaled(&mut s, crs.param("false_easting"), CM);
                    scaled(&mut s, crs.param("false_northing"), CM);
                    s.push(0x40);
                    scaled(&mut s, gt[1], CM);
                    scaled(&mut s, gt[5].abs(), CM);
                    for v in [llx, lly, urx, ury] {
                        scaled(&mut s, v, CM);
                    }
                }
                name @ ("lambert_conformal_conic_2sp" | "albers_conic_equal_area") => {
                    s.extend(if name.starts_with("lambert") { 30u16 } else { 31u16 }.to_be_bytes());
                    ellipsoid(&mut s);
                    let (lon, lat) = to_geographic(crs, llx, lly)?;
                    scaled(&mut s, lat, DEG);
                    scaled(&mut s, lon, DEG);
                    s.push(0); // Resolution and component flags
                    scaled(&mut s, crs.param("latitude_of_origin"), DEG);
                    scaled(&mut s, lon360(crs.param("central_meridian")), DEG);
                    const MM: f64 = 1e-3;
                    scaled(&mut s, gt[1], MM);
                    scaled(&mut s, gt[5].abs(), MM);
                    s.push(0); // Projection centre flag
                    s.push(0x40);
                    scaled(&mut s, crs.param("standard_parallel_1"), DEG);
                    scaled(&mut s, crs.param("standard_parallel_2"), DEG);
                    s.extend(MISSING4.to_be_bytes()); // Southern pole of the projection
                    s.extend(MISSING4.to_be_bytes());
                }
                _ => return Err(unsupported(&format!("projection {p} is not written"))),
            }
        }
    }
    let len = s.len() as u32;
    s[..4].copy_from_slice(&len.to_be_bytes());
    Ok(Grid { section: s, split })
}

/// The longitude and latitude of a projected point, on the CRS's own geographic CRS, with
/// negative longitudes made positive (GDAL's `TransformToGeo`).
fn to_geographic(crs: &Crs, x: f64, y: f64) -> Result<(f64, f64)> {
    let src = crate::crs::SpatialRef::from_wkt(&crs.wkt);
    let geo = src
        .geographic_wkt()
        .map(|w| crate::crs::SpatialRef::from_wkt(&w))
        .ok_or_else(|| Error::Unsupported("GRIB2 writer: the CRS has no geographic CRS to give its first point in".into()))?;
    let (mut lon, lat) = crate::crs::Transformer::new(&src, &geo)?.transform(x, y)?;
    if !(lon.is_finite() && lat.is_finite()) {
        return Err(Error::InvalidArgument("GRIB2 writer: the grid's first point has no longitude and latitude".into()));
    }
    if lon < 0.0 {
        lon += 360.0;
    }
    Ok((lon, lat))
}

/// Section 1, as GDAL's `WriteSection1`.
fn section1(o: &Options, band: usize) -> Vec<u8> {
    let num = |key: &str, default: i64| o.ids(band, key).map_or(default, |v| atoi(&v));
    let mut s = Vec::with_capacity(21);
    s.extend(21u32.to_be_bytes());
    s.push(1);
    s.extend((num("CENTER", 255) as u16).to_be_bytes());
    s.extend((num("SUBCENTER", 65535) as u16).to_be_bytes());
    s.push(num("MASTER_TABLE", 2) as u8);
    s.push(0); // Local table
    s.push(num("SIGNF_REF_TIME", 0) as u8);
    let (y, mo, d, h, mi, sec) = ref_time(&o.ids(band, "REF_TIME").unwrap_or_default());
    s.extend((y as u16).to_be_bytes());
    s.extend([mo as u8, d as u8, h as u8, mi as u8, sec as u8]);
    s.push(num("PROD_STATUS", 255) as u8);
    s.push(num("TYPE", 255) as u8);
    s
}

/// `sscanf(s, "%04d-%02d-%02dT%02d:%02d:%02dZ", ...)` over 1970-01-01T00:00:00Z: the fields
/// read before the first mismatch.
fn ref_time(s: &str) -> (i64, i64, i64, i64, i64, i64) {
    let mut out = [1970, 1, 1, 0, 0, 0];
    let b = s.as_bytes();
    let mut i = 0;
    for (k, (width, sep)) in [(4, b'-'), (2, b'-'), (2, b'T'), (2, b':'), (2, b':'), (2, b'Z')].into_iter().enumerate() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
            i += 1;
        }
        while i < b.len() && i - start < width && b[i].is_ascii_digit() {
            i += 1;
        }
        match std::str::from_utf8(&b[start..i]).ok().and_then(|t| t.parse::<i64>().ok()) {
            Some(v) => out[k] = v,
            None => break,
        }
        if b.get(i) != Some(&sep) {
            break;
        }
        i += 1;
    }
    (out[0], out[1], out[2], out[3], out[4], out[5])
}

/// GDAL's `ComputeValOffset`: temperatures in Kelvin are written from Celsius.
fn value_offset(numbers: &[i64], input_unit: Option<&str>) -> f32 {
    if numbers.len() >= 2 && numbers[0] == 0 {
        let p = numbers[1];
        let kelvin = ((0..=18).contains(&p) && !matches!(p, 8 | 10 | 11 | 16)) || p == 21 || p == 27;
        if kelvin && input_unit.is_none_or(|u| u.eq_ignore_ascii_case("C") || u.eq_ignore_ascii_case("[C]")) {
            return 273.15;
        }
    }
    0.0
}

/// Section 4, as GDAL's `WriteSection4`, and the offset its product applies to values.
fn section4(o: &Options, band: usize) -> Result<(Vec<u8>, f32)> {
    let pdtn = atoi(o.band(band, "PDS_PDTN", true).unwrap_or("0"));
    let mut numbers = o.band(band, "PDS_TEMPLATE_NUMBERS", false);
    let assembled = o.band(band, "PDS_TEMPLATE_ASSEMBLED_VALUES", false);
    if numbers.is_some() && assembled.is_some() {
        return Err(Error::InvalidArgument("PDS_TEMPLATE_NUMBERS and PDS_TEMPLATE_ASSEMBLED_VALUES are exclusive".into()));
    }
    if assembled.is_some() {
        return Err(Error::Unsupported("GRIB2 writer: PDS_TEMPLATE_ASSEMBLED_VALUES is not written; give PDS_TEMPLATE_NUMBERS".into()));
    }
    if numbers.is_none() {
        numbers = o.band(band, "PDS_TEMPLATE_NUMBERS", true);
    }
    let input_unit = o.band(band, "INPUT_UNIT", false).or_else(|| {
        let md = o.bands.get(band - 1)?;
        md.items.iter().find(|(k, _)| k.eq_ignore_ascii_case("GRIB_UNIT")).map(|(_, v)| v.as_str())
    });
    let mut s = Vec::with_capacity(34);
    s.extend(MISSING4.to_be_bytes());
    s.push(4);
    s.extend(0u16.to_be_bytes()); // No coordinate values after the template
    s.extend((pdtn as u16).to_be_bytes());
    let mut offset = 0.0;
    match numbers {
        None if pdtn == 0 => {
            s.extend([255, 255, 255, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255, 255, 255]);
        }
        None => {
            return Err(Error::InvalidArgument(
                "PDS_PDTN != 0 specified but both PDS_TEMPLATE_NUMBERS and PDS_TEMPLATE_ASSEMBLED_VALUES missing".into(),
            ));
        }
        Some(text) => {
            let values: Vec<i64> = text.split(' ').filter(|t| !t.is_empty()).map(atoi).collect();
            offset = value_offset(&values, input_unit);
            s.extend(values.iter().map(|&v| v as u8));
        }
    }
    let len = s.len() as u32;
    s[..4].copy_from_slice(&len.to_be_bytes());
    Ok((s, offset))
}

#[derive(Clone, Copy, PartialEq)]
enum Encoding {
    Simple,
    Ieee,
}

/// The values of band `b` in GRIB's order: rows from the bottom up (a north-up raster), each
/// split and swapped at `split`.
fn band_values(r: &Raster, b: usize, split: usize) -> Vec<f64> {
    let (nx, ny) = (r.cols, r.rows);
    let base = b * nx * ny;
    let mut out = Vec::with_capacity(nx * ny);
    for i in 0..ny {
        let row = base + (ny - 1 - i) * nx;
        out.extend((split..nx).chain(0..split).map(|c| r.data.get_f64(row + c)));
    }
    out
}

/// Sections 5, 6 and 7 of band `b`.
fn sections567(r: &Raster, b: usize, nodata: Option<f64>, o: &Options, offset: f32, split: usize) -> Result<Vec<u8>> {
    let band = b + 1;
    let floating = matches!(r.data_type, DataType::F32 | DataType::F64);
    let values = band_values(r, b, split);
    let has_nodata = nodata.is_some_and(|nd| values.contains(&nd));
    if o.band(band, "SPATIAL_DIFFERENCING_ORDER", false).is_some() {
        return Err(Error::Unsupported("GRIB2 writer: SPATIAL_DIFFERENCING_ORDER needs COMPLEX_PACKING, which is not written".into()));
    }
    let encoding = match o.band(band, "DATA_ENCODING", false).unwrap_or("AUTO").to_ascii_uppercase().as_str() {
        "AUTO" if has_nodata => {
            return Err(Error::Unsupported(format!(
                "GRIB2 writer: band {band} has nodata pixels, which GDAL writes with COMPLEX_PACKING; terrio does not \
                 write it (DATA_ENCODING=SIMPLE_PACKING or IEEE_FLOATING_POINT write the nodata value as data)"
            )));
        }
        "AUTO" if floating => Encoding::Ieee,
        "AUTO" | "SIMPLE_PACKING" => Encoding::Simple,
        "IEEE_FLOATING_POINT" => Encoding::Ieee,
        e @ ("COMPLEX_PACKING" | "PNG" | "JPEG2000") => {
            return Err(Error::Unsupported(format!("GRIB2 writer: DATA_ENCODING={e} is not written")));
        }
        e => return Err(Error::InvalidArgument(format!("Unsupported DATA_ENCODING={e}"))),
    };
    let n = values.len();
    let mut s = Vec::new();
    match encoding {
        Encoding::Ieee => {
            let single = matches!(r.data_type, DataType::U8 | DataType::U16 | DataType::I16 | DataType::F32);
            s.extend(12u32.to_be_bytes());
            s.push(5);
            s.extend((n as u32).to_be_bytes());
            s.extend(4u16.to_be_bytes());
            s.push(if single { 1 } else { 2 });
            s.extend([0, 0, 0, 6, 6, 255]);
            let size = if single { 4 } else { 8 };
            s.extend(((5 + n * size) as u32).to_be_bytes());
            s.push(7);
            s.reserve(n * size);
            for &v in &values {
                if single {
                    let mut f = v as f32;
                    if offset != 0.0 {
                        f += offset;
                    }
                    s.extend(f.to_bits().to_be_bytes());
                } else {
                    let mut d = v;
                    if offset != 0.0 {
                        d += offset as f64;
                    }
                    s.extend(d.to_bits().to_be_bytes());
                }
            }
        }
        Encoding::Simple => {
            let nbits = atoi(o.band(band, "NBITS", false).unwrap_or("0")).clamp(0, 31) as i32;
            let mut dsf = o.band(band, "DECIMAL_SCALE_FACTOR", false).map_or(0, atoi) as i32;
            if dsf > 0 && !floating {
                dsf = 0;
            }
            // GDAL's `GetFloatData`: single precision, the offset but on nodata pixels.
            let mut fld = Vec::with_capacity(n);
            let (mut fmin, mut fmax) = (f32::MAX, -f32::MAX);
            for &v in &values {
                let mut f = v as f32;
                if has_nodata && nodata.is_some_and(|nd| f == nd as f32) {
                    fld.push(f);
                    continue;
                }
                if !f.is_finite() {
                    return Err(Error::InvalidArgument(format!(
                        "GRIB2 writer: band {band} has non-finite values, which simple packing cannot hold"
                    )));
                }
                f += offset;
                fmin = fmin.min(f);
                fmax = fmax.max(f);
                fld.push(f);
            }
            let decimal_scale = 10f64.powi(dsf);
            let mut bits = nbits;
            let scaled_max_diff = (fmax - fmin) as f64 * decimal_scale;
            if floating && bits == 0 && scaled_max_diff > 0.0 && scaled_max_diff <= 256.0 {
                bits = 8;
            }
            let p = simpack(&fld, dsf, bits)?;
            s.extend(21u32.to_be_bytes());
            s.push(5);
            s.extend((n as u32).to_be_bytes());
            s.extend(0u16.to_be_bytes());
            s.extend(p.reference.to_bits().to_be_bytes());
            int16(&mut s, p.binary_scale);
            int16(&mut s, p.decimal_scale);
            s.push(p.nbits as u8);
            s.push(if floating { 0 } else { 1 });
            s.extend([0, 0, 0, 6, 6, 255]);
            s.extend(((5 + p.data.len()) as u32).to_be_bytes());
            s.push(7);
            s.extend(p.data);
        }
    }
    Ok(s)
}

/// What `simpack` returns: the reference value, scale factors, bits per value and packed values.
struct Packed {
    reference: f32,
    binary_scale: i32,
    decimal_scale: i32,
    nbits: u32,
    data: Vec<u8>,
}

/// g2clib's `int_power`.
fn int_power(x: f64, y: i32) -> f64 {
    let (mut x, mut y) = if y < 0 { (1.0 / x, -(y as i64)) } else { (x, y as i64) };
    let mut value = 1.0;
    while y != 0 {
        if y & 1 == 1 {
            value *= x;
        }
        x *= x;
        y >>= 1;
    }
    value
}

/// g2clib's `RINT`: `floor(d + 0.5)`, in double precision.
fn rint(d: f32) -> i64 {
    (d as f64 + 0.5).floor() as i64
}

/// g2clib's `simpack` as GDAL calls it (binary scale factor 0), in its single-precision
/// arithmetic; with one more bit when the scaled values would not fit (a range of exactly a
/// power of two).
fn simpack(fld: &[f32], decimal: i32, nbits_in: i32) -> Result<Packed> {
    // g2clib's `0.69314718f`: the same single-precision value.
    const ALOG2: f32 = std::f32::consts::LN_2;
    let dscale = int_power(10.0, decimal) as f32;
    let mut nbits = if nbits_in <= 0 || nbits_in > 31 { 0 } else { nbits_in };
    if dscale == 0.0 {
        return Err(Error::InvalidArgument("GRIB2 writer: invalid decimal scale factor".into()));
    }
    let (mut rmin, mut rmax) = (fld.first().copied().unwrap_or(0.0), fld.first().copied().unwrap_or(0.0));
    for &v in fld.iter().skip(1) {
        if v > rmax {
            rmax = v;
        }
        if v < rmin {
            rmin = v;
        }
    }
    for v in [rmin, rmax] {
        let f = (v as f64 * dscale as f64).floor();
        if !(f >= -(f32::MAX as f64) && f <= f32::MAX as f64) {
            return Err(Error::InvalidArgument("GRIB2 writer: scaled values not representable in single precision".into()));
        }
    }
    let mut rmin_d = rmin * dscale;
    let rmax_d = rmax * dscale;
    let mut binary = 0;
    if rmax_d - rmin_d >= 1.0 || (rmin != rmax && nbits != 0) {
        let mut ints: Option<Vec<i64>> = None;
        if nbits == 0 {
            if dscale != 1.0 {
                rmin_d = (rmin_d as f64).floor() as f32;
            }
            if (rmax_d - rmin_d) as f64 > i32::MAX as f64 {
                nbits = 31;
            } else {
                let temp = (((rmax_d - rmin_d) as f64).ceil().ln() / ALOG2 as f64) as f32;
                nbits = (temp as f64).ceil() as i32;
                if nbits > 31 {
                    nbits = 31;
                } else {
                    ints = Some(fld.iter().map(|&v| rint(v * dscale - rmin_d)).collect());
                }
            }
        }
        let ints = match ints {
            Some(i) => i,
            None => {
                if dscale != 1.0 {
                    rmin_d = (rmin_d as f64).floor() as f32;
                }
                let maxnum = int_power(2.0, nbits) - 1.0;
                let temp = ((maxnum / (rmax_d - rmin_d) as f64).ln() / ALOG2 as f64) as f32;
                binary = (-(temp as f64)).ceil() as i32;
                let bscale = int_power(2.0, -binary) as f32;
                fld.iter().map(|&v| rint((v * dscale - rmin_d) * bscale)).collect()
            }
        };
        // `simpack` can count a bit short for a range of exactly 2^n: the largest values would
        // wrap to the smallest.
        let top = ints.iter().copied().max().unwrap_or(0);
        while nbits < 31 && top >= 1i64 << nbits {
            nbits += 1;
        }
        let mut data = vec![0u8; (ints.len() * nbits as usize).div_ceil(8)];
        let mask = (1u64 << nbits) - 1;
        let mut bit = 0usize;
        for v in ints {
            let v = (v as u64) & mask;
            for k in (0..nbits).rev() {
                if (v >> k) & 1 == 1 {
                    data[bit / 8] |= 0x80 >> (bit % 8);
                }
                bit += 1;
            }
        }
        Ok(Packed { reference: rmin_d, binary_scale: binary, decimal_scale: decimal, nbits: nbits as u32, data })
    } else {
        // A constant field: no packed values, and E and D 0.
        let reference = if dscale != 1.0 { ((rmin as f64 * dscale as f64).floor() as f32) / dscale } else { rmin };
        Ok(Packed { reference, binary_scale: 0, decimal_scale: 0, nbits: 0, data: Vec::new() })
    }
}

/// The GRIB2 messages of every band of `r`.
fn to_bytes(r: &Raster, nodata: Option<f64>, o: &Options) -> Result<Vec<u8>> {
    if !matches!(r.data_type, DataType::U8 | DataType::U16 | DataType::I16 | DataType::U32 | DataType::I32 | DataType::F32 | DataType::F64)
    {
        return Err(Error::Unsupported(format!("GRIB2 writer: unsupported data type {:?}", r.data_type)));
    }
    if o.flag("WRITE_SUBGRIDS") {
        return Err(Error::Unsupported("GRIB2 writer: WRITE_SUBGRIDS=YES is not written".into()));
    }
    if r.cols == 0 || r.rows == 0 || r.bands == 0 {
        return Err(Error::InvalidArgument("GRIB2 writer: empty raster".into()));
    }
    if (r.cols as u64) * (r.rows as u64) > u32::MAX as u64 {
        return Err(Error::Unsupported("GRIB2 writer: more than 2^32 - 1 points".into()));
    }
    let wkt = crate::crs::SpatialRef::from_raster_crs(&r.crs)
        .to_wkt()
        .ok_or_else(|| Error::Unsupported("Unsupported or missing spatial reference system".into()))?;
    let crs = Crs::parse(&wkt).ok_or_else(|| Error::Unsupported("Unsupported or missing spatial reference system".into()))?;
    let grid = grid(r, &crs)?;
    let mut out = Vec::new();
    for b in 0..r.bands {
        let band = b + 1;
        let start = out.len();
        out.extend(b"GRIB");
        out.extend([0, 0]);
        out.push(atoi(o.band(band, "DISCIPLINE", true).unwrap_or("0")) as u8);
        out.push(2);
        out.extend([0u8; 8]);
        out.extend(section1(o, band));
        out.extend([0, 0, 0, 5, 2]); // An empty local use section, as GDAL writes
        out.extend(&grid.section);
        let (s4, offset) = section4(o, band)?;
        out.extend(s4);
        out.extend(sections567(r, b, nodata, o, offset, grid.split)?);
        out.extend(b"7777");
        let len = (out.len() - start) as u64;
        if len > i32::MAX as u64 {
            return Err(Error::Unsupported("GRIB message larger than 2 GB".into()));
        }
        out[start + 8..start + 16].copy_from_slice(&len.to_be_bytes());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simpack_keeps_a_power_of_two_range() {
        // 0..=256 needs 9 bits; g2clib's estimate gives 8, and 256 would read back as 0.
        let fld: Vec<f32> = (0..=256).map(|v| v as f32).collect();
        let p = simpack(&fld, 0, 0).unwrap();
        assert_eq!(p.nbits, 9);
        assert_eq!(p.reference, 0.0);
    }

    #[test]
    fn ref_time_as_sscanf() {
        assert_eq!(ref_time("2017-10-20T06:00:00Z"), (2017, 10, 20, 6, 0, 0));
        assert_eq!(ref_time(""), (1970, 1, 1, 0, 0, 0));
        assert_eq!(ref_time("2001-02"), (2001, 2, 1, 0, 0, 0));
    }

    #[test]
    fn atoi_as_c() {
        assert_eq!(atoi("7(US-NCEP)"), 7);
        assert_eq!(atoi(" -12x"), -12);
        assert_eq!(atoi("x"), 0);
    }

    /// A CRS whose WKT closes a node with the other closer (`[` ... `)`) is read as PROJ reads it,
    /// so GDAL, and terrio's CRS reader, which keeps such WKT as given: GDAL 3.12.1 writes the same
    /// GRIB2 file for all of these as for the bracketed one.
    #[test]
    fn either_closer_ends_a_wkt_node() {
        let brackets = r#"GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#;
        for wkt in [
            brackets.to_string(),
            brackets.replace("298.257223563]]", "298.257223563)]"),
            brackets.replace("0.0174532925199433]]", "0.0174532925199433])"),
            brackets.replace('[', "(").replace(']', ")"),
        ] {
            let crs = Crs::parse(&wkt).unwrap_or_else(|| panic!("not read: {wkt}"));
            assert_eq!((crs.semi_major, crs.inv_flattening), (6378137.0, 298.257223563), "{wkt}");
            assert!((crs.angular_unit - 1.0).abs() < 1e-12, "{wkt}: {}", crs.angular_unit);
            assert!(crs.projection.is_none(), "{wkt}");
        }
        // A byte that starts no value is skipped as a word, and the input ends the parse.
        assert!(wkt1::parse("A[*,B[1)]").is_some());
        assert!(wkt1::parse("A[B[1)").is_none());
    }

    /// GDAL does not import a WKT with a value at level 16 (PROJ's "too many nesting levels") or
    /// longer than 100,000 bytes, and then writes no GRIB2 file ("Unsupported or missing spatial
    /// reference system", GDAL 3.12.1 from a GeoTIFF's PAM `<SRS>`); one level or one byte less, it
    /// writes the same 575 bytes as for WGS 84. The writer reads the WKT within the same bounds.
    #[test]
    fn wkt_gdal_does_not_import_is_not_read() {
        let wgs84 = r#"GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#;
        // WGS 84 with a node whose string sits at `level`: GEOGCS at 0, B at 1, the A nodes below.
        let extra = |level: usize| format!("{},B[{}\"x\"{}]]", &wgs84[..wgs84.len() - 1], "A[".repeat(level - 2), "]".repeat(level - 2));
        let padded = |len: usize| wgs84.replacen(",DATUM", &format!(",{}DATUM", " ".repeat(len - wgs84.len())), 1);
        let read = |w: &str| Crs::parse(w).map(|c| (c.semi_major, c.inv_flattening));
        let wgs84_read = Some((6378137.0, 298.257223563));
        assert_eq!(read(&extra(15)), wgs84_read);
        assert_eq!(read(&extra(16)), None);
        assert_eq!(padded(100_000).len(), 100_000);
        assert_eq!(read(&padded(100_000)), wgs84_read);
        assert_eq!(read(&padded(100_001)), None);
    }
}
