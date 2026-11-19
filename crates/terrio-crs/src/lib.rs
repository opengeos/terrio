// SPDX-FileCopyrightText: Copyright (c) 2000, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2008-2013, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.12.1:
//     ogr/ogrct.cpp
//
// Written to match the behavior of:
//   GDAL v3.13.3:
//     ogr/ogrspatialreference.cpp
//   PROJ 9.8.1:
//     src/trans.cpp
// No code is translated from these sources.

//! Coordinate reference systems and transformations between them.
//!
//! Coordinates follow GDAL's traditional GIS order. East/north geographic axes are
//! normalized to longitude/latitude; west-positive planetary systems retain their
//! declared axis order and directions.
//!
//! ```no_run
//! use terrio_crs::{SpatialRef, Transformer};
//!
//! let nad27 = SpatialRef::parse("EPSG:4267")?;
//! let utm = SpatialRef::parse("+proj=utm +zone=13 +datum=WGS84")?;
//! println!("{} -> {}", nad27.label(), utm.label());
//! let t = Transformer::new(&nad27, &utm)?;
//! let (x, y) = t.transform(-105.5, 39.5)?;
//! let bounds = t.transform_bounds((-106.0, 39.0, -105.0, 40.0))?;
//! println!("{}", utm.to_wkt().unwrap_or_default());
//! # Ok::<(), terrio_crs::Error>(())
//! ```
//!
//! Datum shift grids (NTv2, GeoTIFF) and geoid models are used when they can be found, as GDAL
//! uses them (see [`register_grid`] and the grid directories and network access described in
//! the [CRS documentation](https://opengeos.github.io/terrio/crs/)). Compound CRSs (such as
//! `EPSG:4326+5773`) and geographic 3D CRSs transform heights with [`Transformer::transform_3d`];
//! time-dependent transformations between dynamic CRSs take the coordinate epoch of
//! [`TransformOptions`]:
//!
//! ```no_run
//! use terrio_crs::{SpatialRef, TransformOptions, Transformer};
//!
//! // Ellipsoidal heights (WGS 84) to EGM96 heights.
//! let t = Transformer::new(&SpatialRef::parse("EPSG:4979")?, &SpatialRef::parse("EPSG:4326+5773")?)?;
//! let (lon, lat, h) = t.transform_3d(-100.0, 40.0, 100.0)?;
//! // ITRF2014 coordinates at epoch 2010.0 to GDA2020.
//! let opts = TransformOptions { source_coordinate_epoch: Some(2010.0), ..Default::default() };
//! let t = Transformer::with_options(&SpatialRef::parse("EPSG:7912")?, &SpatialRef::parse("EPSG:7843")?, &opts)?;
//! let (lon, lat, h) = t.transform_3d(135.0, -25.0, 0.0)?;
//! # Ok::<(), terrio_crs::Error>(())
//! ```

mod error;
pub use error::{Error, Result};
pub mod host;
/// The bounds of the WKT reader, which are GDAL's and PROJ's: the length of a definition, in bytes.
pub const WKT_MAX_LEN: usize = wkt1::MAX_LEN;
/// The bounds of the WKT reader: the nesting of its nodes.
pub const WKT_MAX_LEVELS: usize = wkt1::MAX_LEVELS;
#[cfg(all(unix, not(target_family = "wasm")))]
pub use host::hold_for_fork;
pub use host::{Host, set_host};
mod warnings;
pub use warnings::take as take_warnings;
#[cfg(remote)]
pub mod network;
use serde::Serialize;
#[cfg(feature = "wbprojection-fallback")]
use wbprojection as wbp;

mod bodies;
mod catalog;
mod datum;
mod def;
mod epsg;
pub mod geokeys;
mod grid;
mod iau;
mod keywords;
mod math;
#[cfg(test)]
mod model;
mod params;
mod proj_write;
mod projections;
mod projjson;
mod projjson_gdal;
mod projjson_order;
mod projjson_write;
mod rotation;
mod route;
pub(crate) mod wkt1;
mod wkt1_write;
mod wkt2;

/// Recognized boolean PROJ parameters with their normalized values, in definition order.
/// Numeric and other non-boolean parameters are omitted. Invalid quoting and unknown
/// values on recognized switches are refused, using the same rules as the CRS parser.
pub fn proj_boolean_parameters(definition: &str) -> Result<Vec<(String, bool)>> {
    let parameters = params::Params::parse(definition);
    parameters.validate_quoting()?;
    parameters.validate_flags()?;
    Ok(parameters.boolean_parameters())
}

pub use catalog::{AUTHORITIES, CrsEntry, CrsKind, CrsQuery, EPSG_VERSION, check_authorities, list_crs};
pub(crate) use def::Def;
pub use grid::{
    GridUse, confine_grid_search_paths, default_grid_search_paths, register_grid, reset_grid_cache, set_grid_search_paths,
    set_network_enabled,
};
#[doc(hidden)]
pub use wkt1_write::{Described, describe, epsg_ellipsoid, from_proj_on};

/// Starts loading terrio's EPSG table in the background, for work that will need it once it has
/// read its input.
#[doc(hidden)]
pub fn prefetch() {
    epsg::prefetch();
}

/// The EPSG geographic 2D CRS on an EPSG geodetic datum with an ellipsoid (PROJ parameters such
/// as `+ellps=GRS80` or `+a=6378137 +rf=298.257222101`) and the Greenwich prime meridian, the
/// lowest current code: the geographic CRS of GeoKeys that give the datum (NAD83(HARN), 6152)
/// but not the geographic CRS, as GDAL identifies it (EPSG:4152). `None` when the datum has none.
#[doc(hidden)]
pub fn geographic_on_datum(datum: u32, ellipsoid: &str) -> Option<u32> {
    let own = Def::from_proj(&format!("+proj=longlat {ellipsoid} +no_defs")).ok()?;
    epsg::geographic_on(datum).iter().copied().find(|code| {
        !epsg::is_deprecated(*code)
            && epsg::get(*code).is_some_and(|e| {
                !e.three_d
                    && !e.proj.contains("+pm=")
                    && Def::from_proj(&e.proj).is_ok_and(|d| (own.ell.a - d.ell.a).abs() < 1e-3 && (own.ell.es - d.ell.es).abs() < 1e-9)
            })
    })
}

/// The EPSG datum of an EPSG geographic CRS of terrio's table. `None` for another code.
#[doc(hidden)]
pub fn geographic_datum(geog: u32) -> Option<u32> {
    epsg::get(geog).map(|e| e.datum).filter(|d| *d != 0)
}

/// The datum shift to WGS 84 of an EPSG geographic 2D CRS, or of the base geographic CRS of an
/// EPSG projected or compound CRS, as `+towgs84` carries it: that of the first transformation to
/// WGS 84 (EPSG:4326) that needs no grid, in PROJ 9.9.0's order (pyproj's
/// `TransformerGroup("EPSG:<code>", "EPSG:4326").transformers`), when PROJ runs it as a single
/// Helmert transformation of 3 or 7 parameters. That includes a concatenation PROJ runs as one
/// (a change of prime meridian or between geographic and geocentric coordinates around it, two
/// geocentric translations through a datum on the same ellipsoid). When PROJ's ballpark comes
/// first, it is the transformation PROJ's `proj_create_crs_to_crs` finds through another datum,
/// as terrio's [`Transformer`] does (RD/83 through ETRS89 without its grid): terrio's EPSG table
/// lists it before the ballpark.
///
/// The parameters are tx, ty, tz in metres, rx, ry, rz in arc-seconds and ds in ppm, in the
/// position vector convention: a coordinate frame rotation's rotations are negated, and an
/// inverted transformation's seven parameters are negated: the first-order inverse that
/// `+towgs84` can carry, where PROJ inverts the step exactly (they differ at the second order,
/// with rotations or a scale). A geocentric translation has zero rotations and scale; a
/// transformation that shifts nothing (WGS 84's realizations, NAD83 to WGS 84 (1)) gives zeros,
/// as `+towgs84=0,0,0`.
///
/// Every EPSG geographic 2D CRS other than WGS 84 is checked against PROJ 9.9.0
/// (`tests/data/helmert_to_wgs84.tsv`, `scripts/gen_helmert_to_wgs84.py`).
///
/// `None` for WGS 84 itself, a code terrio's table does not have, and when that transformation
/// is another one: a Molodensky-Badekas, a time-dependent or an exact-rotation Helmert
/// transformation, a geographic offset, several Helmert transformations, or PROJ's ballpark
/// (no transformation without a grid). Also for MGI (Ferro) (EPSG:4805), whose transformation
/// turns longitudes by another Ferro meridian than the CRS's own.
///
/// Unlike [`SpatialRef::to_proj4_gdal`], which follows GDAL 3.13.3 and writes no `+towgs84` when a
/// datum has several transformations to WGS 84, this gives the one PROJ picks without grids.
///
/// ```
/// // ED50 to WGS 84 (1), also for ED50 / UTM zone 32N.
/// assert_eq!(terrio_crs::helmert_to_wgs84(4230), Some([-87.0, -98.0, -121.0, 0.0, 0.0, 0.0, 0.0]));
/// assert_eq!(terrio_crs::helmert_to_wgs84(23032), terrio_crs::helmert_to_wgs84(4230));
/// assert_eq!(terrio_crs::helmert_to_wgs84(4326), None);
/// ```
pub fn helmert_to_wgs84(code: u32) -> Option<[f64; 7]> {
    // A compound CRS's horizontal CRS.
    let base = if epsg::get(code).is_some() { code } else { epsg::compound(code)?.horizontal };
    let entry = epsg::get(base)?;
    let geog = epsg::get(if entry.geog != 0 { entry.geog } else { base })?;
    if geog.datum == 6326 {
        return None;
    }
    let op = geog.ops.iter().find(|op| !op.needs_grids())?;
    if op.ballpark {
        return None;
    }
    // The datum shift PROJ runs, without the conversions to and from geocentric coordinates.
    let mut shifts = op.steps.iter().filter(|s| !matches!(s, datum::Step::Cart { .. }));
    match (shifts.next(), shifts.next()) {
        (None, _) => Some([0.0; 7]),
        (Some(datum::Step::Helmert { pivot: [0.0, 0.0, 0.0], rates: None, seven: datum::Seven(Some(p)), inv, .. }), None) => {
            let p: &[f64; 7] = p;
            // Without negative zeros, which a `+towgs84` written with `{}` would carry.
            Some(p.map(|v| {
                if v == 0.0 {
                    0.0
                } else if *inv {
                    -v
                } else {
                    v
                }
            }))
        }
        _ => None,
    }
}

/// The WKT GDAL writes for an EPSG code, from terrio's EPSG table.
#[doc(hidden)]
pub fn epsg_wkt(code: u32) -> Option<&'static str> {
    epsg::get(code).map(|e| e.wkt.as_str())
}

/// The EPSG codes of the CRSs that replace a deprecated EPSG CRS, those PROJ lists
/// (`proj_get_non_deprecated`), in order of code: `[5105]` for EPSG:4855. Empty for a current
/// CRS, a deprecated one without replacement, and a code terrio does not know.
///
/// ```no_run
/// assert_eq!(terrio_crs::epsg_replacements(4855), &[5105]);
/// assert!(terrio_crs::epsg_replacements(5105).is_empty());
/// ```
pub fn epsg_replacements(code: u32) -> &'static [u32] {
    epsg::replacements(code)
}

/// The ESRI CRSs whose projection terrio computes otherwise than PROJ (by more than a millimetre
/// in their area of use, as `scripts/compare_esri_with_proj.py` reports with PROJ 9.8), refused
/// rather than transformed wrongly: none since terrio's Cassini follows PROJ's far from its
/// central meridian too (World Cassini, ESRI:54028).
pub(crate) const ESRI_UNLIKE_PROJ: &[u32] = &[];

/// Whether a code taken for an EPSG code is one of PROJ's ESRI authority instead, outside the
/// EPSG registry (wbprojection holds ESRI's 54008, 54009 and 54030 as EPSG codes).
#[doc(hidden)]
pub fn is_esri_code(code: u32) -> bool {
    epsg::get(code).is_none() && epsg::compound(code).is_none() && epsg::vertical(code).is_none() && epsg::esri(code).is_some()
}

// Optional legacy registry lookups. Built-in EPSG/ESRI tables remain independent of the fallback.
fn registry_wkt(code: u32) -> Option<String> {
    #[cfg(feature = "wbprojection-fallback")]
    {
        wbp::to_ogc_wkt(code).ok()
    }
    #[cfg(not(feature = "wbprojection-fallback"))]
    {
        let _ = code;
        None
    }
}

fn registry_name(code: u32) -> Option<String> {
    #[cfg(feature = "wbprojection-fallback")]
    {
        wbp::epsg::epsg_info(code).map(|i| i.name.to_string())
    }
    #[cfg(not(feature = "wbprojection-fallback"))]
    {
        epsg::get(code).map(|e| e.name.clone())
    }
}

fn epsg_reference(s: &str) -> Option<u32> {
    let code = canonical_epsg_reference(s);
    #[cfg(feature = "wbprojection-fallback")]
    {
        // Keep legacy reference suffixes, but normalize canonical versioned
        // references first: the fallback can mistake their version for the code.
        code.or_else(|| {
            let upper = s.trim().to_ascii_uppercase();
            // Keep legacy suffixes after a recognized reference, without letting
            // arbitrary text, compound references or unrecognized WKT leak a code.
            if upper.contains(',') || (upper.contains('+') && !upper.starts_with('+')) {
                return None;
            }
            let leading_code = |text: &str| {
                let end = text.bytes().take_while(u8::is_ascii_digit).count();
                text[..end].parse::<u32>().ok()
            };
            let mut reference = upper.strip_prefix('+').filter(|n| n.bytes().all(|b| b.is_ascii_digit())).and_then(leading_code);
            reference = reference.or_else(|| {
                upper.strip_prefix("EPSG:").and_then(|rest| {
                    let code = rest.trim_start_matches(':');
                    let end = code.bytes().take_while(u8::is_ascii_digit).count();
                    (!code[end..].starts_with(['.', ':'])).then(|| leading_code(code)).flatten()
                })
            });
            reference = reference.or_else(|| {
                upper.strip_prefix("URN:OGC:DEF:CRS:EPSG:").and_then(|rest| {
                    let (version, code) = rest.split_once(':')?;
                    version.is_empty().then(|| leading_code(code)).flatten()
                })
            });
            for prefix in [
                "HTTP://WWW.OPENGIS.NET/DEF/CRS/EPSG/",
                "HTTPS://WWW.OPENGIS.NET/DEF/CRS/EPSG/",
                "HTTP://OPENGIS.NET/DEF/CRS/EPSG/",
                "HTTPS://OPENGIS.NET/DEF/CRS/EPSG/",
            ] {
                reference = reference.or_else(|| {
                    upper.strip_prefix(prefix).and_then(|rest| {
                        let (version, code) = rest.split_once('/')?;
                        (version.is_empty() || version == "0").then(|| leading_code(code)).flatten()
                    })
                });
            }
            for prefix in [
                "HTTP://WWW.OPENGIS.NET/GML/SRS/EPSG.XML#",
                "HTTPS://WWW.OPENGIS.NET/GML/SRS/EPSG.XML#",
                "HTTP://OPENGIS.NET/GML/SRS/EPSG.XML#",
                "HTTPS://OPENGIS.NET/GML/SRS/EPSG.XML#",
            ] {
                reference = reference.or_else(|| upper.strip_prefix(prefix).and_then(leading_code));
            }
            // The legacy parser can mistake a short code before '/' for a version.
            // Keep its compatibility forms only when it returns the prefix's own code.
            let reference = reference?;
            wbp::epsg_from_srs_reference(s).filter(|code| *code == reference)
        })
    }
    #[cfg(not(feature = "wbprojection-fallback"))]
    code
}

/// Canonical authority forms are independent of the optional projection engine.
fn canonical_epsg_reference(s: &str) -> Option<u32> {
    let digits = |s: &str| {
        if !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()) { s.parse().ok() } else { None }
    };
    let s = s.trim();
    if let Some(code) = digits(s) {
        return Some(code);
    }
    let upper = s.to_ascii_uppercase();
    if let Some(code) = upper.strip_prefix("EPSG:") {
        return digits(code.strip_prefix(':').unwrap_or(code));
    }
    if let Some(code) = upper.strip_prefix("URN:X-OGC:DEF:CRS:EPSG:") {
        return digits(code);
    }
    if let Some(rest) = upper.strip_prefix("URN:OGC:DEF:CRS:EPSG:") {
        let (_, code) = rest.split_once(':')?;
        return digits(code);
    }
    for prefix in [
        "HTTP://WWW.OPENGIS.NET/DEF/CRS/EPSG/",
        "HTTPS://WWW.OPENGIS.NET/DEF/CRS/EPSG/",
        "HTTP://OPENGIS.NET/DEF/CRS/EPSG/",
        "HTTPS://OPENGIS.NET/DEF/CRS/EPSG/",
    ] {
        if let Some(rest) = upper.strip_prefix(prefix) {
            let (version, code) = rest.split_once('/')?;
            if version.is_empty() {
                return None;
            }
            return digits(code);
        }
    }
    None
}

/// The parameters of a CRS's conversion, as its PROJ string gives them (see
/// [`SpatialRef::projection_params`]).
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ProjParams {
    /// PROJ's name of the method (`+proj=`): `tmerc`, `utm`, `longlat`, `geocent`...
    pub method: String,
    /// The central meridian (`+lon_0`), in degrees from the prime meridian.
    pub lon_0: f64,
    /// The latitude of origin (`+lat_0`), in degrees.
    pub lat_0: f64,
    /// The scale factor (`+k_0` or `+k`).
    pub k_0: f64,
    /// The false easting (`+x_0`), in metres.
    pub x_0: f64,
    /// The false northing (`+y_0`), in metres.
    pub y_0: f64,
    /// The prime meridian (`+pm`), in degrees east of Greenwich.
    pub prime_meridian: f64,
    /// The semi-major axis of the ellipsoid, in metres.
    pub a: f64,
    /// The semi-minor axis of the ellipsoid, in metres.
    pub b: f64,
    /// Metres per linear unit of the coordinates (`+to_meter` or `+units`); 1 for a geographic CRS.
    pub to_meter: f64,
}

/// The order of a CRS's first two axes (see [`SpatialRef::axis_order`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AxisOrder {
    /// East then north: longitude/latitude, easting/northing.
    EastNorth,
    /// North then east: latitude/longitude, northing/easting.
    NorthEast,
    /// Other axes, such as west/south or geocentric X/Y/Z, or the single axis of a vertical CRS.
    Other,
    /// A CRS terrio can't read, or an EPSG code it reads only through wbprojection.
    Unknown,
}

/// A coordinate reference system, known by an EPSG (or ESRI) code, a WKT definition, a PROJ
/// string or a combination of them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize)]
pub struct SpatialRef {
    /// The EPSG code, when the CRS has (or was identified as having) one.
    pub epsg: Option<u32>,
    /// The WKT definition.
    pub wkt: Option<String>,
    /// The PROJ string definition.
    pub proj4: Option<String>,
    /// The code of PROJ's ESRI authority (`ESRI:54009`), when the CRS has (or was identified as
    /// having) one rather than an EPSG code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub esri: Option<u32>,
}

impl SpatialRef {
    /// A CRS from an EPSG code. Fails when the code is not in the EPSG registry terrio carries.
    /// A deprecated code keeps its own definition, as in PROJ ([`Self::from_epsg_non_deprecated`]
    /// gives its replacement, as GDAL does by default). The ESRI codes wbprojection holds as EPSG
    /// codes (54008, 54009 and 54030) give those ESRI CRSs; other ESRI codes are not EPSG codes,
    /// as in PROJ.
    pub fn from_epsg(code: u32) -> Result<Self> {
        let wkt = match epsg::get(code) {
            Some(e) => spherical_extension(&e.wkt, &e.proj),
            None => match (epsg::compound(code), epsg::vertical(code)) {
                (Some(c), _) => c.wkt.clone(),
                (None, Some(v)) => v.wkt.clone(),
                _ => {
                    // Match wbprojection's three legacy ESRI-as-EPSG aliases, as
                    // documented above. Other ESRI codes require the ESRI namespace.
                    #[cfg(not(feature = "wbprojection-fallback"))]
                    if matches!(code, 54008 | 54009 | 54030) && epsg::esri(code).is_some() {
                        return Self::from_esri(code);
                    }
                    let wkt = registry_wkt(code).ok_or_else(|| Error::Crs(format!("unknown EPSG code {code}")))?;
                    if epsg::esri(code).is_some() {
                        return Self::from_esri(code);
                    }
                    wkt
                }
            },
        };
        Ok(Self { epsg: Some(code), wkt: Some(wkt), ..Default::default() })
    }

    /// A CRS from an EPSG code as GDAL 3.12 gives it by default (`OSR_USE_NON_DEPRECATED=YES`): a
    /// deprecated code with a single replacement ([`epsg_replacements`]) gives that replacement's
    /// CRS, any other code its own, as [`Self::from_epsg`].
    ///
    /// ```no_run
    /// use terrio_crs::SpatialRef;
    /// // The deprecated ETRS89 / NTM zone 5 by its own definition, as PROJ gives it ...
    /// assert_eq!(SpatialRef::from_epsg(4855)?.epsg, Some(4855));
    /// // ... and as GDAL gives it by default, its replacement's.
    /// assert_eq!(SpatialRef::from_epsg_non_deprecated(4855)?.epsg, Some(5105));
    /// # Ok::<(), terrio_crs::Error>(())
    /// ```
    pub fn from_epsg_non_deprecated(code: u32) -> Result<Self> {
        match epsg_replacements(code) {
            [replacement] => Self::from_epsg(*replacement),
            _ => Self::from_epsg(code),
        }
    }

    /// Whether the CRS is a deprecated EPSG CRS (one with an EPSG code EPSG has deprecated).
    pub fn is_deprecated(&self) -> bool {
        self.epsg.is_some_and(epsg::is_deprecated)
    }

    /// A CRS from a code of PROJ's ESRI authority, such as 54009 (World Mollweide), with the WKT
    /// GDAL writes for it. Fails when terrio does not know the code or cannot compute its
    /// projection.
    pub fn from_esri(code: u32) -> Result<Self> {
        let e = epsg::esri(code).ok_or_else(|| Error::Crs(format!("unknown ESRI code {code}")))?;
        if ESRI_UNLIKE_PROJ.contains(&code) {
            return Err(Error::Crs(format!(
                "ESRI:{code} ({}) is not supported: terrio does not compute its projection as PROJ does",
                e.name
            )));
        }
        Def::from_proj(&e.proj).map_err(|err| Error::Crs(format!("ESRI:{code} ({}) is not supported: {err}", e.name)))?;
        Ok(Self { esri: Some(code), wkt: Some(e.wkt.clone()), ..Default::default() })
    }

    /// A CRS from a WKT definition (WKT1 or WKT2). The EPSG code (or else the ESRI code) is
    /// identified when possible. The WKT ends at its first NUL, as in PROJ and GDAL.
    pub fn from_wkt(wkt: &str) -> Self {
        Self::from_wkt_text(until_nul(wkt))
    }

    /// [`Self::from_wkt`] of the whole text, uncut at a NUL: for stored WKT and WKT terrio
    /// built, whose names (a PROJJSON's) may keep one (see [`until_nul`]).
    pub(crate) fn from_wkt_text(wkt: &str) -> Self {
        if let Some(compound) = esri_compound_wkt(wkt) {
            return Self::from_wkt_text(&compound);
        }
        // A WKT 1 extended with its PROJ string (`EXTENSION["PROJ4", ...]`, as GDAL writes CRSs
        // WKT 1 cannot describe) is that PROJ string, as GDAL reads it.
        // A registry CRS's own WKT with its PROJ string as an extension (`spherical_extension`,
        // EPSG:3973 to 3975 and 9311) is its code, with its datum's transformations, which the
        // extension (without a datum, as the table holds it) does not give.
        if let Some(code) = declared_epsg(wkt).filter(|c| is_registry_extension(wkt, *c)) {
            return Self { epsg: Some(code), ..Default::default() };
        }
        if let Some(proj4) = proj4_extension(wkt)
            && let Ok(own) = Def::from_proj_extension(&proj4)
        {
            // Its EPSG (or ESRI) code, declared or identified, when its definition converts as
            // the PROJ string does, on its ellipsoid and datum: the extension wins over a code
            // that does not, as over the nodes. A PROJ string without datum shift is on WGS 84
            // for a code on WGS 84 itself (EPSG:3857's own WKT gives its datum as a null grid).
            // A declared code terrio does not know converts as nothing, and is dropped.
            let fits = |code: SpatialRef| {
                Def::new(&code).is_ok_and(|r| {
                    let unshifted = own.ops().is_empty() && own.datum_code().is_none() && r.is_wgs84();
                    let ellipsoid = (own.ell.a - r.ell.a).abs() < 1e-3 && (own.ell.es - r.ell.es).abs() < 1e-9;
                    // On the code's datum by its transformations, not by a shift of its own
                    // (`+datum=OSGB36 +nadgrids=@null`, no shift to WGS 84); unless the code's
                    // transformations to WGS 84 are all null shifts too (WGS 84 itself), so that
                    // its own place the points alike. Not NAD83, shifted in places (Hawaii, by
                    // NAD83 to WGS 84 (2) and (3)), which PROJ's `to_epsg` takes by its nodes.
                    let null_shift = own.ops().iter().any(|op| op.grids().iter().any(|(g, _)| g.trim_start_matches('@') == "null"));
                    let registry_null = r.is_wgs84()
                        || r.geog().and_then(epsg::get).is_some_and(|e| !e.ops.is_empty() && e.ops.iter().all(|op| op.is_null_shift()));
                    own.same_conversion(&r) && ellipsoid && (unshifted || (own.same_datum(&r) && (!null_shift || registry_null)))
                })
            };
            let epsg = declared_epsg(wkt)
                .or_else(|| identify_epsg(wkt))
                .filter(|code| fits(SpatialRef { epsg: Some(*code), ..Default::default() }));
            // The extension is kept through the WKT, not as the CRS's PROJ string: PROJ computes
            // on it as a pipeline step reads it, whose ellipsoid rules and leniency differ from
            // those of its CRS parser, which reads a PROJ string given.
            let mut s = Self { epsg, wkt: Some(wkt.to_string()), proj4: None, esri: None }.with_esri();
            if let Some(code) = s.esri
                && !fits(SpatialRef { esri: Some(code), ..Default::default() })
            {
                s.esri = None;
            }
            return s;
        }
        // Do not let a registry code repair an incomplete variant C definition or a refused
        // PROJ4 extension, including codes identified by the optional projection fallback.
        // Keep it as WKT so the transformer reports the parsing error. Valid PROJ4
        // extensions have already taken precedence above.
        if (is_variant_c(wkt) || proj4_extension(wkt).is_some()) && Def::from_wkt(wkt).is_err() {
            return Self { wkt: Some(wkt.to_string()), ..Default::default() };
        }
        let epsg = identify_epsg(wkt);
        Self { epsg, wkt: Some(wkt.to_string()), ..Default::default() }.with_esri()
    }

    /// The CRS with an ESRI code in place of a code terrio's EPSG registry does not hold (the
    /// registry identifies ESRI's 54008, 54009 and 54030 as EPSG codes), or identified from the
    /// WKT when there is no EPSG code: the ESRI code it declares, or that of the ESRI CRS of the
    /// same name whose definition converts coordinates as the WKT does, as GDAL identifies them.
    #[doc(hidden)]
    pub fn with_esri(mut self) -> Self {
        // A declared ESRI code wins over an identified EPSG code (ESRI:102100 is EPSG:3857's
        // definition), as GDAL keeps it.
        if let Some(code) = self.wkt.as_deref().and_then(|w| declared_code(w, "ESRI").filter(|c| esri_fits(w, *c))) {
            self.epsg = None;
            self.esri = Some(code);
            return self;
        }
        if let Some(code) = self.epsg
            && is_esri_code(code)
        {
            self.epsg = None;
            if self.wkt.is_none() {
                self.esri = Some(code);
            }
        }
        if self.epsg.is_none()
            && self.esri.is_none()
            && let Some(w) = &self.wkt
        {
            self.esri = identify_esri(w);
        }
        self
    }

    /// The ESRI code only when its registered conversion, datum and ellipsoid match.
    /// Used by name-only writers, which cannot preserve conflicting stored definitions.
    #[doc(hidden)]
    pub fn trusted_esri(&self) -> Option<u32> {
        let code = self.esri?;
        let own = match (self.proj4.as_deref(), self.wkt.as_deref()) {
            (Some(proj), _) => Def::from_proj(proj).ok()?,
            (None, Some(wkt)) => Def::from_wkt(wkt).ok()?,
            (None, None) => return epsg::esri(code).map(|_| code),
        };
        let registered = Def::new(&SpatialRef { esri: Some(code), ..Default::default() }).ok()?;
        (own.same_conversion(&registered)
            && own.same_datum(&registered)
            && (own.ell.a - registered.ell.a).abs() < 1e-3
            && (own.ell.es - registered.ell.es).abs() < 1e-9)
            .then_some(code)
    }

    /// A compound CRS from the EPSG codes of a horizontal and a vertical CRS, as GDAL builds
    /// `EPSG:4326+5773`.
    pub fn compound(horizontal: u32, vertical: u32) -> Result<Self> {
        let h = Self::from_epsg(horizontal)?;
        let v = epsg::vertical(vertical).ok_or_else(|| Error::Crs(format!("EPSG:{vertical} is not a vertical CRS")))?;
        let name = format!("{} + {}", h.name().unwrap_or_default(), v.name);
        let wkt = format!("COMPD_CS[\"{name}\",{},{}]", h.to_wkt().unwrap_or_default(), v.wkt);
        Ok(Self { wkt: Some(wkt), ..Default::default() })
    }

    /// Validate string-reference component roles without changing metadata composition callers.
    fn compound_reference(horizontal: u32, vertical: u32) -> Result<Self> {
        let h = Self::from_epsg(horizontal)?;
        if epsg::vertical(horizontal).is_some()
            || epsg::compound(horizontal).is_some()
            || Def::new(&h).is_ok_and(|d| d.is_geocentric() || d.has_height())
        {
            return Err(Error::Crs(format!("EPSG:{horizontal} is not a two-dimensional horizontal CRS")));
        }
        Self::compound(horizontal, vertical)
    }

    /// A CRS from a PROJ string such as `+proj=utm +zone=33 +datum=WGS84`.
    pub fn from_proj4(proj4: &str) -> Result<Self> {
        let p = params::Params::parse(proj4);
        p.validate_quoting()?;
        p.validate_flags()?;
        if let Some(code) = p.str("init").and_then(|i| i.to_ascii_lowercase().strip_prefix("epsg:").and_then(|c| c.parse().ok())) {
            return Self::from_epsg(code);
        }
        // Checks that terrio can transform with it.
        Def::from_proj(proj4)?;
        Ok(Self { wkt: proj_to_wkt(proj4), proj4: Some(proj4.to_string()), ..Default::default() })
    }

    /// A geographic or projected CRS of the IAU_2015 registry (PROJ 9.8.1).
    pub fn from_iau(code: u32) -> Result<Self> {
        let entry = iau::get(code).ok_or_else(|| Error::Crs(format!("unknown IAU_2015 code {code}")))?;
        let definition = Def::from_proj(&entry.proj)?;
        if definition.is_geographic() != entry.geographic {
            return Err(Error::Crs("IAU definition has an unsupported coordinate system".into()));
        }
        Ok(Self { proj4: Some(entry.proj.clone()), wkt: Some(entry.wkt.clone()), ..Default::default() })
    }

    /// The CRS as PROJJSON v0.7: geographic 2D and 3D, geocentric, projected, vertical, compound
    /// and bound (`+towgs84`) CRSs, as PROJ writes them, with the differences listed in the CRS
    /// documentation. Fails for CRSs it cannot write (dynamic CRSs, projections without an EPSG
    /// method).
    pub fn to_projjson(&self) -> Result<String> {
        projjson_write::write(self)
    }

    /// Parses the supported PROJJSON v0.7 geographic, projected or bound CRS subset.
    /// Unsupported coordinate systems, datum models and operations return errors.
    pub fn from_projjson(text: &str) -> Result<Self> {
        // The text ends at its first NUL, as PROJ's `proj_create` reads it; a NUL a JSON string
        // gives (`\u0000`) is part of its name.
        projjson::parse(until_nul(text))
    }

    /// Parses a user-supplied CRS: `EPSG:4326`, `4326`, `urn:ogc:def:crs:EPSG::4326`, an OGC URL,
    /// `OGC:CRS84`, `WGS84`, `ESRI:54009` (or `urn:ogc:def:crs:ESRI::54009`), a WKT definition,
    /// a PROJ/PROJJSON string, or the path of a regular file holding one of these (such as a
    /// shapefile's `.prj`). Special files, including pipes and devices, are refused without
    /// reading them. Use [`Self::parse_literal`] for text read by the caller.
    pub fn parse(s: &str) -> Result<Self> {
        Self::parse_definition(s, true)
    }

    /// Parses a literal authority reference, WKT, PROJ or PROJJSON definition without
    /// interpreting the input as a local filename. Use this for external metadata.
    pub fn parse_literal(s: &str) -> Result<Self> {
        Self::parse_definition(s, false)
    }

    /// [`Self::parse`], reading a file only when `files` (not for a file's own text).
    #[cfg_attr(target_family = "wasm", allow(unused_variables))]
    fn parse_definition(s: &str, files: bool) -> Result<Self> {
        // The definition ends at its first NUL, as PROJ's `proj_create` reads it.
        let s = until_nul(s).trim();
        if s.is_empty() {
            return Err(Error::Crs("empty definition".into()));
        }
        let upper = s.to_ascii_uppercase();
        if matches!(upper.as_str(), "WGS84" | "WGS 84" | "OGC:CRS84" | "CRS84" | "URN:OGC:DEF:CRS:OGC:1.3:CRS84")
            || upper.ends_with("/OGC/1.3/CRS84")
        {
            return Self::from_epsg(4326);
        }
        if s.starts_with('+') {
            return Self::from_proj4(s);
        }
        if s.starts_with('{') {
            return Self::from_projjson(s);
        }
        if let Some(code) = iau::reference(s) {
            return Self::from_iau(code?);
        }
        // OGC 07-092r3 7.5.2: two CRS components, not a single authority reference.
        // GDAL 3.13.3 retains the horizontal and vertical parts of this form.
        if let Some(parts) = upper.strip_prefix("URN:OGC:DEF:CRS,") {
            let mut parts = parts.split(',');
            let (Some(horizontal), Some(vertical), None) = (parts.next(), parts.next(), parts.next()) else {
                return Err(Error::Crs("a compound CRS URN requires two CRS components".into()));
            };
            let code = |part: &str| -> Result<u32> {
                let Some(rest) = part.trim().strip_prefix("CRS:EPSG:") else {
                    return Err(Error::Crs(format!("unsupported compound CRS component '{part}'")));
                };
                let Some((_, code)) = rest.split_once(':') else {
                    return Err(Error::Crs(format!("invalid compound CRS component '{part}'")));
                };
                if code.is_empty() || !code.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(Error::Crs(format!("invalid compound CRS component '{part}'")));
                }
                code.parse().map_err(|_| Error::Crs(format!("invalid compound CRS code '{code}'")))
            };
            return Self::compound_reference(code(horizontal)?, code(vertical)?);
        }
        // `EPSG:4326+5773`: a horizontal and a vertical CRS.
        if let Some(rest) = upper.strip_prefix("EPSG:")
            && let Some((h, v)) = rest.split_once('+')
            && h.trim().bytes().chain(v.trim().bytes()).all(|byte| byte.is_ascii_digit())
            && let (Ok(h), Ok(v)) = (h.trim().parse::<u32>(), v.trim().parse::<u32>())
        {
            return Self::compound_reference(h, v);
        }
        if looks_like_wkt(s) {
            // As GDAL's `importFromWkt`: WKT its reader refuses (unbalanced, nested deeper than
            // PROJ allows, over 100,000 bytes) is an error, not an opaque CRS.
            let head = || s.chars().take(80).collect::<String>();
            let Some(root) = wkt1::parse(s) else {
                return Err(Error::Crs(format!("not a WKT definition GDAL reads: '{}'", head())));
            };
            // Every WKT CRS production requires member nodes, such as a geographic CRS's datum
            // and a projected CRS's base CRS (OGC 01-009 7.2; ISO 19162:2019 8.1, 9.1): a keyword
            // with only its name, `GEOGCS["x"]`, defines nothing.
            if !root.args.iter().any(|arg| matches!(arg, wkt1::Arg::Node(_))) {
                return Err(Error::Crs(format!("WKT {} has no definition after its name: '{}'", root.key, head())));
            }
            return Ok(Self::from_wkt(s));
        }
        if let Some(code) = esri_reference(&upper) {
            return Self::from_esri(code);
        }
        if let Some(code) = epsg_reference(s) {
            return Self::from_epsg(code);
        }
        // A file, read as GDAL's `SetFromUserInput` reads one: into a 100,000-byte buffer, a file
        // filling it (99,999 bytes or more) refused rather than read whole, as `/dev/zero` was;
        // its text is a definition, never the path of another file (one naming itself recursed
        // until the stack overflowed).
        #[cfg(not(target_family = "wasm"))]
        if files && let Some(f) = open_definition_file(s) {
            use std::io::Read;
            const MAX: u64 = 99_999;
            let mut bytes = Vec::new();
            if f.take(MAX).read_to_end(&mut bytes).is_ok()
                && (bytes.len() as u64) < MAX
                && let Ok(text) = String::from_utf8(bytes)
            {
                return Self::parse_definition(&text, false).map_err(|_| Error::Crs(format!("cannot parse CRS definition file '{s}'")));
            }
        }
        Err(Error::Crs(format!("cannot parse '{s}'")))
    }

    /// The WKT of the geographic CRS this CRS is based on (its own when geographic): from the
    /// EPSG table for an EPSG CRS, else the first `GEOGCS` node of its WKT 1 (written again from
    /// its tree, in `[`/`]` brackets). `None` when neither is known. Taken from the CRS's WKT, it
    /// has its names cut at a NUL ([`wkt_for_files`]), so that it reads as a whole WKT with
    /// [`SpatialRef::from_wkt`], which ends text at a NUL.
    #[doc(hidden)]
    pub fn geographic_wkt(&self) -> Option<String> {
        if let Some(entry) = self.wkt.as_deref().and_then(|wkt| iau::identify(wkt, self.proj4.as_deref())).and_then(iau::get) {
            return iau::get(entry.base).map(|base| base.wkt.clone());
        }
        if let Some(e) = self.epsg.and_then(epsg::get).or_else(|| self.esri.and_then(epsg::esri))
            && let Some(g) = epsg::get(e.geog)
        {
            return Some(g.wkt.clone());
        }
        // The first `GEOGCS` of the tree (not a name's text, and in `(`/`)` brackets too).
        let geogcs = wkt1::parse(&self.to_wkt()?)?.find(&["GEOGCS"])?.to_wkt();
        Some(wkt_for_files(&geogcs).into_owned())
    }

    /// EPSG code of the geographic CRS underlying this definition, when known. For a WKT 1 CRS on a
    /// datum no CRS is on, this is the CRS its name identifies it as (GR96 for ESRI's
    /// GCS_Greenland_1996), whose transformations the definition does not take.
    pub fn geographic_code(&self) -> Option<u32> {
        Def::shared(self).ok().and_then(|d| d.geographic_code())
    }

    /// Rotated-pole CF parameters, semi-major axis and eccentricity squared.
    pub fn rotated_pole_parameters(&self) -> Option<([f64; 3], f64, f64)> {
        let d = Def::shared(self).ok()?;
        Some((d.rotated_pole()?, d.ell.a, d.ell.es))
    }

    /// Whether nothing is known about the CRS.
    pub fn is_unknown(&self) -> bool {
        self.epsg.is_none() && self.esri.is_none() && self.wkt.is_none() && self.proj4.is_none()
    }

    /// The authority and code of the CRS: `("EPSG", 4326)`, or `("ESRI", 54009)` for a CRS of
    /// PROJ's ESRI authority; IAU codes use `("IAU_2015", code)`.
    pub fn authority(&self) -> Option<(&'static str, u32)> {
        match (self.epsg, self.esri) {
            (Some(code), _) => Some(("EPSG", code)),
            (None, Some(code)) => Some(("ESRI", code)),
            (None, None) => self.wkt.as_deref().and_then(|wkt| iau::identify(wkt, self.proj4.as_deref())).map(|code| ("IAU_2015", code)),
        }
    }

    /// The authority code as GDAL writes it: `EPSG:4326` or `ESRI:54009`.
    pub fn authority_code(&self) -> Option<String> {
        self.authority().map(|(a, code)| format!("{a}:{code}"))
    }

    /// The PROJ string of the CRS as PROJ 9.8.1 writes it (`proj_as_proj_string` with
    /// `PJ_PROJ_4`, pyproj's `CRS.to_proj4()`, `projinfo -o PROJ`), or `None` for a CRS PROJ has
    /// none for (an engineering CRS, Polar Stereographic variant C, an EPSG code terrio reads only
    /// through the wbprojection fallback).
    ///
    /// Like PROJ's, the string leaves out what a PROJ string cannot hold, so it is no substitute
    /// for the code or WKT to keep a CRS:
    /// - the datum transformation of a registry CRS (`+datum=` names only WGS 84, NAD83 and NAD27;
    ///   `+towgs84` comes only from a definition that gives it, a WKT's `TOWGS84` or `BOUNDCRS`,
    ///   which PROJ writes with the ellipsoid in place of the datum, even for a registry CRS);
    /// - the angular unit of a geographic CRS (EPSG:4807 is in grads, its string in degrees);
    /// - the axis order, but for south-orientated Transverse Mercator and Krovak (`+axis=wsu`,
    ///   `+axis=swu`);
    /// - a PROJ string's vertical unit (`+vunits`) without a geoid model (`+geoidgrids`).
    ///
    /// A compound CRS (`EPSG:4326+5773`) is its horizontal CRS's string with the vertical unit
    /// (and a WKT's geoid model, `+geoidgrids`). A CRS given by a PROJ string or a WKT without
    /// a code comes back as PROJ writes it: `+datum=` for WGS84, NAD83 and NAD27 when nothing
    /// changes them (`+ellps` beside `+datum` is ignored), else the datum's ellipsoid; the
    /// ellipsoid by name, else a sphere as `+R`, else as given (`+rf` or `+b`); `+towgs84` with
    /// seven values, or `+nadgrids` in its place; units by PROJ's names; UTM zones as
    /// `+proj=utm`; GDAL's Web Mercator string as PROJ's. Its parameters stay in their order,
    /// which PROJ rewrites in its own, adding the defaults. A WKT's PROJ4 extension is written as
    /// it is, as PROJ writes it. A planetocentric CRS on an ellipsoid (`+geoc`) has none. A WKT's
    /// grid shift (`PROJ4_GRIDS` in its datum, a `BOUNDCRS` with a grid) is not read, so not
    /// written.
    /// As in PROJ 9.8.1, export need not preserve a WKT's coordinate mapping: south-orientated
    /// Transverse Mercator with north-east axes transforms south-west but exports west-south;
    /// Krovak with west-south axes exports without that axis order. Keep the WKT when the
    /// declared coordinate-system axes must survive a round trip.
    ///
    /// ```
    /// use terrio_crs::SpatialRef;
    ///
    /// let utm = SpatialRef::from_epsg(32632)?;
    /// assert_eq!(utm.to_proj4().as_deref(), Some("+proj=utm +zone=32 +datum=WGS84 +units=m +no_defs +type=crs"));
    /// # Ok::<(), terrio_crs::Error>(())
    /// ```
    pub fn to_proj4(&self) -> Option<String> {
        proj_write::write(self)
    }

    /// The PROJ string GDAL writes for the CRS (`OSRExportToProj4`, GDAL 3.13.3 on PROJ 9.9.0):
    /// [`Self::to_proj4`] without `+type=crs`, and for an EPSG or ESRI code's own definition (from
    /// [`Self::from_epsg`] or [`Self::from_esri`]), with what GDAL adds to it: the datum
    /// transformation to WGS 84 as `+towgs84`, when PROJ finds a single one for the CRS's area of
    /// use (GDAL's default `OSR_ADD_TOWGS84_ON_EXPORT_TO_PROJ4=YES`), and a compound CRS's geoid
    /// model as `+geoidgrids` and `+geoid_crs`. `None` where [`Self::to_proj4`] is.
    ///
    /// A WKT 1 or WKT 2 whose root `AUTHORITY` or `ID` names the code gets the same, as GDAL
    /// binds a CRS by its root identifier, unless it has a datum shift of its own, which it keeps;
    /// another CRS keeps its own `+towgs84`, if any. A deprecated code has its own definition's,
    /// as GDAL with `OSR_USE_NON_DEPRECATED=NO`. `docs/crs.md` lists where terrio's string differs
    /// from GDAL's (PROJJSON, `+init=epsg:N`, compound CRSs given by a definition).
    ///
    /// ```
    /// use terrio_crs::SpatialRef;
    ///
    /// let gk3 = SpatialRef::from_epsg(31467)?;
    /// assert_eq!(
    ///     gk3.to_proj4_gdal().as_deref(),
    ///     Some("+proj=tmerc +lat_0=0 +lon_0=9 +k=1 +x_0=3500000 +y_0=0 +ellps=bessel +towgs84=598.1,73.7,418.2,0.202,0.045,-2.455,6.7 +units=m +no_defs")
    /// );
    /// # Ok::<(), terrio_crs::Error>(())
    /// ```
    pub fn to_proj4_gdal(&self) -> Option<String> {
        let proj = self.to_proj4()?;
        let mut tokens: Vec<String> = proj_write::tokens(&proj, true).into_iter().filter(|t| t.replace('"', "") != "+type=crs").collect();
        // The table first: rebuilding the code's own definition is the costlier check. A code's
        // own PROJ string has no `+towgs84`, `+nadgrids` or `+geoidgrids` of its own where the
        // table adds one (`a_registry_string_has_nothing_the_table_adds`); a WKT naming the code
        // can, and keeps it, as in GDAL.
        let Some((towgs84, geoid)) =
            self.authority().and_then(|(authority, code)| epsg::gdal_additions(authority, code)).filter(|_| self.gdal_binds_to_wgs84())
        else {
            return Some(tokens.join(" "));
        };
        // After the ellipsoid, before the units and what follows them.
        let has = |tokens: &[String], keys: &[&str]| tokens.iter().any(|t| keys.iter().any(|k| t.starts_with(k)));
        if !towgs84.is_empty() && !has(&tokens, &["+towgs84=", "+nadgrids="]) {
            let at = tokens
                .iter()
                .position(|t| {
                    ["+units=", "+to_meter=", "+vunits=", "+nadgrids=", "+geoidgrids="].iter().any(|k| t.starts_with(k))
                        || t == "+no_defs"
                        || t == "+wktext"
                })
                .unwrap_or(tokens.len());
            tokens.insert(at, format!("+towgs84={towgs84}"));
        }
        if !geoid.is_empty() && !has(&tokens, &["+geoidgrids="]) {
            let geoid = geoid.split_whitespace().map(str::to_string);
            if tokens.iter().any(|t| t == "+wktext") {
                // A CRS PROJ writes through its PROJ4 extension (Web Mercator's): GDAL appends
                // the geoid model after it, and the vertical unit last.
                let vunits: Vec<String> = tokens.iter().filter(|t| t.starts_with("+vunits=")).cloned().collect();
                tokens.retain(|t| !t.starts_with("+vunits="));
                tokens.extend(geoid);
                tokens.extend(vunits);
            } else {
                let at = tokens.iter().position(|t| t.starts_with("+vunits=") || t == "+no_defs").unwrap_or(tokens.len());
                tokens.splice(at..at, geoid);
            }
        }
        Some(tokens.join(" "))
    }

    /// Whether GDAL binds this CRS to WGS 84 by its code, as its PROJ string export does: a CRS
    /// that is its EPSG or ESRI code's own definition (that of [`Self::from_epsg`] or
    /// [`Self::from_esri`], or a bare code), or a WKT, without a PROJ string set over it, whose
    /// root `AUTHORITY` or `ID` declares the code. A compound CRS's WKT does not count (GDAL's
    /// results for those vary; the tests list them).
    fn gdal_binds_to_wgs84(&self) -> bool {
        let Some((authority, code)) = self.authority() else { return false };
        // A bare code is the code's definition; a PROJ string set over a WKT is what is written.
        if self.wkt.is_none() && self.proj4.is_none() {
            return true;
        }
        if self.proj4.is_none()
            && self.wkt.as_deref().and_then(gdal_root_identifier).is_some_and(|(a, c)| a.eq_ignore_ascii_case(authority) && c == code)
        {
            return true;
        }
        let own = match authority {
            "EPSG" => Self::from_epsg(code),
            "ESRI" => Self::from_esri(code),
            _ => return false,
        };
        own.is_ok_and(|own| own.wkt == self.wkt && own.proj4 == self.proj4 && own.esri == self.esri)
    }

    /// The WKT definition, built from the EPSG (or ESRI) code or PROJ string when no WKT is
    /// stored.
    pub fn to_wkt(&self) -> Option<String> {
        if let Some(w) = &self.wkt {
            return Some(w.clone());
        }
        if let Some(e) = self.esri.and_then(epsg::esri) {
            return Some(e.wkt.clone());
        }
        if let Some(code) = self.epsg {
            return epsg::get(code)
                .map(|e| spherical_extension(&e.wkt, &e.proj))
                .or_else(|| epsg::compound(code).map(|c| c.wkt.clone()))
                .or_else(|| epsg::vertical(code).map(|v| v.wkt.clone()))
                .or_else(|| registry_wkt(code));
        }
        self.proj4.as_deref().and_then(proj_to_wkt)
    }

    /// The CRS in WKT 1 that needs no PROJ4 extension, for formats whose readers ignore
    /// `EXTENSION["PROJ4", ...]` (shapefile `.prj`, GeoPackage, GeoTIFF citations), as PROJ 9.8.1
    /// exports `WKT1_GDAL` (with the AXIS nodes GDAL's `exportToWkt` writes). `None` where
    /// [`to_wkt`](Self::to_wkt) would give another CRS to such a reader, or is no WKT 1:
    /// - a CRS PROJ cannot write in WKT 1: geographic 3D CRSs (EPSG:4979), Equal Earth
    ///   (EPSG:8857), the registry CRSs whose method has no WKT 1 name (Colombia Urban, Modified
    ///   Krovak...), rotated poles (`+proj=ob_tran`), and EPSG codes terrio reads only through
    ///   the wbprojection fallback, whose WKT is not the registry's;
    /// - a planetocentric CRS on an ellipsoid, which PROJ writes as a `GEOGCS` that its readers
    ///   take for planetographic latitudes;
    /// - a WKT 1 that keeps part of its CRS in the extension: a datum grid (`+nadgrids`, a WKT 2
    ///   bound CRS's `PARAMETERFILE`), an axis order or parameter its nodes do not describe,
    ///   GDAL's `custom_proj4` method.
    ///
    /// A WKT 1 with an extension is kept when an EPSG or ESRI `AUTHORITY` of its root identifies
    /// the CRS, as GDAL's Web Mercator (EPSG:3857). A stored WKT 1 is returned as it is when it
    /// needs no extension (or its root code is the CRS's own), else `None`. A stored
    /// WKT 2 that terrio identifies as a code, without a datum shift of its own, is the code's
    /// WKT 1; with the code's two horizontal axes swapped (ETRS89 in longitude and latitude), the
    /// code's WKT 1 with its axes swapped and without the codes that give their order. Another
    /// WKT 2 is the WKT 1 of its PROJ string ([`to_proj4`](Self::to_proj4)) when that reads back
    /// as the same conversion with the WKT 2's axes (east-north, or north-east for a geographic
    /// CRS), on the EPSG geographic CRS of a datum terrio identifies (OSGB36), which keeps the
    /// datum's name, code and transformations.
    ///
    /// ```
    /// use terrio_crs::SpatialRef;
    ///
    /// assert!(SpatialRef::from_epsg(32632)?.to_wkt1_strict().is_some());
    /// assert!(SpatialRef::from_epsg(4979)?.to_wkt1_strict().is_none());
    /// let grid = SpatialRef::from_proj4("+proj=longlat +ellps=clrk66 +nadgrids=conus +no_defs")?;
    /// assert!(grid.to_wkt().is_some_and(|w| w.contains("EXTENSION[\"PROJ4\"")));
    /// assert_eq!(grid.to_wkt1_strict(), None);
    /// # Ok::<(), terrio_crs::Error>(())
    /// ```
    pub fn to_wkt1_strict(&self) -> Option<String> {
        wkt1_write::strict(self)
    }

    /// The CRS's name, such as `WGS 84 / UTM zone 33N`.
    pub fn name(&self) -> Option<String> {
        // The registry's name first: a stored WKT may carry another vendor's name (ESRI `.prj`).
        if let Some(e) = self.epsg.and_then(epsg::get) {
            return Some(e.name.clone());
        }
        if let Some(c) = self.epsg.and_then(epsg::compound) {
            return Some(c.name.clone());
        }
        if let Some(v) = self.epsg.and_then(epsg::vertical) {
            return Some(v.name.clone());
        }
        if let Some(e) = self.esri.and_then(epsg::esri) {
            return Some(e.name.clone());
        }
        let registry = self.epsg.and_then(registry_wkt);
        registry
            .as_deref()
            .and_then(wkt_name)
            .or_else(|| self.to_wkt().as_deref().and_then(wkt_name))
            .or_else(|| self.epsg.and_then(registry_name))
    }

    /// A short label: `EPSG:4326` (or `ESRI:54009`) when the code is known, otherwise the name.
    pub fn label(&self) -> String {
        match (self.authority_code(), self.name()) {
            (Some(code), _) => code,
            (None, Some(name)) => name,
            (None, None) => "unknown".into(),
        }
    }

    /// Metres per unit of the CRS's coordinates, as GDAL reckons them for tile matrix sets: the
    /// linear unit of a projected CRS, the length of a degree on the semi-major axis of a
    /// geographic one (1 when unknown).
    #[doc(hidden)]
    pub fn metres_per_unit(&self) -> f64 {
        let wkt = self.to_wkt().unwrap_or_default();
        if self.is_geographic() {
            return semi_major_axis(&wkt).unwrap_or(6_378_137.0) * std::f64::consts::PI / 180.0;
        }
        // WKT 1's UNIT, or WKT 2's LENGTHUNIT (of the CRS, else of its first axis).
        wkt1::parse(&wkt)
            .and_then(|root| {
                let unit = root.child("UNIT").or_else(|| root.child("LENGTHUNIT"));
                let unit = unit.or_else(|| root.children("AXIS").find_map(|a| a.child("LENGTHUNIT")));
                unit.and_then(|u| u.num(1))
            })
            .filter(|v| *v > 0.0)
            .unwrap_or(1.0)
    }

    /// One longitude revolution in native angular units, or `None` for a nongeographic CRS.
    #[doc(hidden)]
    pub fn longitude_period(&self) -> Option<f64> {
        Def::shared(self).ok()?.longitude_period()
    }

    /// Whether the CRS has geocentric Cartesian X/Y/Z coordinates.
    pub fn is_geocentric(&self) -> bool {
        Def::shared(self).is_ok_and(|d| d.is_geocentric())
    }

    /// The EPSG codes of the horizontal and the vertical CRS of a compound CRS: those of the
    /// registry's compound CRS of the CRS's code, else those its WKT's two parts declare (the
    /// horizontal one may be identified). `None` for other CRSs, and when a code is missing.
    pub fn compound_parts(&self) -> Option<(u32, u32)> {
        if let Some(c) = self.epsg.and_then(epsg::compound) {
            return Some((c.horizontal, c.vertical));
        }
        let wkt = self.wkt.as_deref()?;
        let (horizontal, vertical) = def::split_compound(&esri_compound_wkt(wkt).unwrap_or_else(|| wkt.to_string()))?;
        // Its vertical CRS's code only of its unit and direction (not heights in feet as EPSG:5701).
        let vertical = def::vertical_code(&vertical)?;
        Some((identify_epsg(&horizontal)?, vertical))
    }

    /// Whether coordinates have a height that transformations change: a compound CRS (with a
    /// vertical CRS), a geographic 3D or a geocentric CRS. [`Transformer::transform_3d`]
    /// transforms heights between two such CRSs.
    pub fn has_height(&self) -> bool {
        Def::shared(self).is_ok_and(|d| d.has_height())
    }

    /// Whether the enabled engines recognize a geographic (longitude/latitude) CRS.
    /// Unrecognized definitions return `false`, as do projected and geocentric CRSs.
    pub fn is_geographic(&self) -> bool {
        match Def::shared(self) {
            Ok(d) => d.is_geographic(),
            #[cfg(feature = "wbprojection-fallback")]
            Err(_) => self.to_wbp().map(|c| c.is_geographic()).unwrap_or(false),
            #[cfg(not(feature = "wbprojection-fallback"))]
            Err(_) => false,
        }
    }

    /// Whether the first two horizontal axes point north then east, as for latitude/longitude
    /// and northing/easting CRSs. Unknown or undeclared axes return `false`.
    /// GML authority-order names need these axes swapped into traditional GIS XY order
    /// (GDAL 3.12.1 `EPSGTreatsAsLatLong` / `EPSGTreatsAsNorthingEasting`).
    pub fn has_north_east_axis_order(&self) -> bool {
        let Some(root) = self.to_wkt().as_deref().and_then(horizontal_root) else { return false };
        let mut axes = root.children("AXIS");
        let points = |axis: Option<&wkt1::Node>, direction: &str| {
            axis.is_some_and(|axis| matches!(axis.args.get(1), Some(wkt1::Arg::Ident(d)) if d.eq_ignore_ascii_case(direction)))
        };
        points(axes.next(), "north") && points(axes.next(), "east")
    }

    /// The order of the first two axes, as PROJ 9.8.1 reads the CRS: the registry's axes for an
    /// EPSG or ESRI code, `+axis` for a CRS given as a PROJ string (east then north by default),
    /// and otherwise its WKT's `AXIS` nodes. Without them, a WKT 1 geographic CRS with an EPSG
    /// code is in that code's order, and other CRSs east then north. Geocentric and vertical CRSs,
    /// and Polar Stereographic, whose axes point along meridians (unless a WKT states others), are
    /// [`AxisOrder::Other`], as are axes other than north and east (`northEast`, `south`).
    ///
    /// Unlike [`Self::has_north_east_axis_order`], which goes by the CRS's WKT, a PROJ string
    /// such as `+proj=longlat +datum=WGS84` is east then north.
    /// ```
    /// use terrio_crs::{AxisOrder, SpatialRef};
    /// assert_eq!(SpatialRef::from_epsg(4326)?.axis_order(), AxisOrder::NorthEast);
    /// assert_eq!(SpatialRef::from_proj4("+proj=longlat +datum=WGS84")?.axis_order(), AxisOrder::EastNorth);
    /// # Ok::<(), terrio_crs::Error>(())
    /// ```
    pub fn axis_order(&self) -> AxisOrder {
        // A vertical CRS, by its code or its WKT (`VERT_CS`, ESRI's `VERTCS`, `VERTCRS`,
        // `VERTICALCRS`), has a single axis.
        let vertical_wkt = self.wkt.as_deref().is_some_and(|w| keywords::is(keywords::root_key(w), keywords::VERTICAL_CRS));
        if self.epsg.and_then(epsg::vertical).is_some() || vertical_wkt {
            return AxisOrder::Other;
        }
        // An EPSG code terrio reads only through wbprojection, whose WKT has no axes (it drops
        // the west-orientated ones of EPSG:3053 and others).
        if self.epsg.is_some_and(|c| epsg::get(c).is_none() && epsg::compound(c).is_none() && self.wkt == registry_wkt(c)) {
            return AxisOrder::Unknown;
        }
        let Ok(def) = Def::shared(self) else { return AxisOrder::Unknown };
        if def.is_geocentric() {
            return AxisOrder::Other;
        }
        let order = |first: u8, second: u8| match (first.to_ascii_lowercase(), second.to_ascii_lowercase()) {
            (b'e', b'n') => AxisOrder::EastNorth,
            (b'n', b'e') => AxisOrder::NorthEast,
            _ => AxisOrder::Other,
        };
        // PROJ's Polar Stereographic has both axes pointing south (or north) along meridians,
        // unless a WKT states others.
        let polar = def.params().is_some_and(|p| p.method == "stere" && p.lat_0.abs() == 90.0);
        // A PROJ string's axes (one from the WKT of an IAU_2015 CRS goes by the WKT, as PROJ
        // reads it).
        let iau = self.wkt.as_deref().and_then(iau::declared).is_some();
        // A WKT's PROJ4 extension is its PROJ string too.
        let extension = self.wkt.as_deref().and_then(proj4_extension).is_some();
        if !self.registry_entry() && !iau && (self.proj4.is_some() || extension) {
            let [first, second] = def.axis_directions();
            return if polar { AxisOrder::Other } else { order(first, second) };
        }
        let Some(root) = self.to_wkt().as_deref().and_then(horizontal_root) else { return AxisOrder::Unknown };
        // Only the four directions count: `northEast` is another one.
        let direction = |axis: &wkt1::Node| match axis.args.get(1) {
            Some(wkt1::Arg::Ident(d)) if d.eq_ignore_ascii_case("north") => b'n',
            Some(wkt1::Arg::Ident(d)) if d.eq_ignore_ascii_case("east") => b'e',
            _ => 0,
        };
        let mut axes = root.children("AXIS");
        match (axes.next(), axes.next()) {
            // Axes the stored WKT states, a registry code's included (from_epsg stores the
            // registry's WKT, whose polar CRSs state their axes, but EPSG:2985 and 2986's do not).
            (Some(first), Some(second)) if self.wkt.is_some() => order(direction(first), direction(second)),
            // Polar Stereographic without a stored WKT, or with one that states no axes.
            _ if polar => AxisOrder::Other,
            // Otherwise, those of the WKT built from a code alone.
            (Some(first), Some(second)) => order(direction(first), direction(second)),
            // A WKT 1 geographic CRS without axes is in its EPSG code's order, as PROJ reads it,
            // else (or for a code terrio doesn't have) in longitude/latitude order, as any other
            // CRS. (Every geographic EPSG code of PROJ 9.8.1 is in terrio's table, so none is
            // read only through wbprojection here.)
            _ => match root.epsg().filter(|_| root.key.eq_ignore_ascii_case("GEOGCS") && self.wkt.is_some()) {
                Some(code) => match (SpatialRef { epsg: Some(code), ..Default::default() }).axis_order() {
                    AxisOrder::Unknown => AxisOrder::EastNorth,
                    known => known,
                },
                None => AxisOrder::EastNorth,
            },
        }
    }

    /// The parameters of the CRS's conversion, as its PROJ string (from the registry, as given,
    /// or converted from its WKT) has them, with PROJ's defaults (a central meridian of 0, a scale
    /// factor of 1, no false easting or northing). A UTM or UPS zone gives its own, as in PROJ's conversion. A compound CRS
    /// gives those of its horizontal CRS. `None` for a CRS terrio can't read, or reads only
    /// through wbprojection's WKT parser.
    ///
    /// The values are those of the definition, not derived ones: Mercator's `+lat_ts` is not
    /// turned into a scale factor, nor Oblique Mercator's `+lonc` into a central meridian. A WKT
    /// identified as an EPSG code (one whose content is that CRS) gives the code's, as terrio
    /// transforms it. South-orientated Transverse Mercator gives `+x_0`/`+y_0` as its PROJ string
    /// has them, negated with x and y: a WKT's false easting of 1000 is an `x_0` of -1000.
    /// ```
    /// use terrio_crs::SpatialRef;
    /// let p = SpatialRef::from_epsg(32633)?.projection_params().unwrap();
    /// assert_eq!((p.method.as_str(), p.lon_0, p.k_0, p.x_0), ("utm", 15.0, 0.9996, 500000.0));
    /// # Ok::<(), terrio_crs::Error>(())
    /// ```
    pub fn projection_params(&self) -> Option<ProjParams> {
        let def = Def::shared(self).ok()?;
        let mut params = def.params()?.clone();
        // A registry CRS is on the registry's ellipsoid, as PROJ reports it: the one its PROJ
        // string names may be rounded (Bessel Namibia's `bess_nam`, 0.28 m) or a sphere computed on
        // (Web Mercator). The WKT's is taken in metres (EPSG:3139's is in feet).
        if let Some((a, b)) = self.registry_horizontal_wkt().and_then(wkt_ellipsoid).filter(|(a, _)| (a - params.a).abs() < 1.0) {
            (params.a, params.b) = (a, b);
        }
        Some(params)
    }

    /// The WKT of the registry entry `Def::new` reads the CRS from (that of the horizontal CRS
    /// of a compound one).
    fn registry_horizontal_wkt(&self) -> Option<&'static str> {
        let entry = match (self.epsg, self.esri) {
            (Some(code), _) => epsg::get(code).or_else(|| epsg::compound(code).and_then(|c| epsg::get(c.horizontal))),
            (None, Some(code)) => Some(code).filter(|c| !ESRI_UNLIKE_PROJ.contains(c)).and_then(epsg::esri),
            // A compound CRS without a code of its own (`EPSG:3857+5773`), by its horizontal one.
            (None, None) => self.compound_parts().and_then(|(horizontal, _)| epsg::get(horizontal)),
        };
        entry.map(|e| e.wkt.as_str())
    }

    /// Whether `Def::new` reads the CRS from the EPSG or ESRI table.
    fn registry_entry(&self) -> bool {
        match self.epsg {
            Some(code) => epsg::get(code).is_some() || epsg::compound(code).is_some(),
            None => self.esri.is_some_and(|c| !ESRI_UNLIKE_PROJ.contains(&c) && epsg::esri(c).is_some()),
        }
    }

    /// The celestial body the CRS is on, as PROJ 9.8.1 names it: `Earth` for an EPSG or ESRI
    /// code, the body of an IAU_2015 CRS (`Mars`), and for another CRS, `Earth` for an ellipsoid
    /// within 0.7% of the Earth's size, else the body of PROJ's table whose semi-major axis is
    /// within 0.7% of the ellipsoid's (`+R=3396190` is on Mars). `None` for another ellipsoid,
    /// which PROJ calls `Non-Earth body`, for one near the size of several bodies, and for a CRS
    /// terrio can't read.
    /// ```
    /// use terrio_crs::SpatialRef;
    /// assert_eq!(SpatialRef::from_iau(49900)?.body(), Some("Mars"));
    /// assert_eq!(SpatialRef::from_epsg(4326)?.body(), Some("Earth"));
    /// # Ok::<(), terrio_crs::Error>(())
    /// ```
    pub fn body(&self) -> Option<&'static str> {
        let def = Def::shared(self).ok()?;
        if let Some(code) = def.declared_body() {
            return bodies::NAIF.binary_search_by_key(&code, |(c, _)| *c).ok().map(|i| bodies::NAIF[i].1);
        }
        // PROJ's `Ellipsoid::guessBodyName`: the Earth within 0.7% of its mean radius, else the
        // one body whose semi-major axis is within 0.7% of the ellipsoid's.
        let a = def.params().map_or(def.ell.a, |p| p.a);
        if def::earth_sized(a) {
            return Some("Earth");
        }
        let mut near = bodies::RADII.iter().filter(|(_, r)| (r - a).abs() / r <= 0.007).map(|(name, _)| *name);
        let first = near.next()?;
        near.all(|name| name == first).then_some(first)
    }

    /// Whether the CRS is on the Earth (see [`Self::body`]).
    pub fn is_earth(&self) -> bool {
        self.body() == Some("Earth")
    }

    /// Whether this is a rotated-pole grid (PROJ's `+proj=ob_tran`, CF's
    /// `rotated_latitude_longitude`): its coordinates are in degrees but are not longitudes and
    /// latitudes, so it is not geographic.
    pub fn is_rotated_pole(&self) -> bool {
        self.epsg.is_none() && Def::shared(self).is_ok_and(|d| d.is_rotated_pole())
    }

    /// Whether coordinates are in degrees: a geographic CRS or a rotated-pole grid.
    #[doc(hidden)]
    pub fn is_angular(&self) -> bool {
        self.is_geographic() || self.is_rotated_pole()
    }

    /// Whether both describe the same CRS, as far as can be told.
    pub fn same_as(&self, other: &SpatialRef) -> bool {
        // Compound CRSs of the same horizontal and vertical CRSs, whether or not one has the
        // registry's code (EPSG:9707 and `EPSG:4326+5773`).
        if let (Some(a), Some(b)) = (self.compound_parts(), other.compound_parts()) {
            return a == b;
        }
        match (self.authority(), other.authority()) {
            (Some(a), Some(b)) => a == b,
            _ => {
                // WKT can retain an IAU ID while an explicit PROJ override changes
                // the effective definition. Equal stored WKT alone is insufficient.
                let iau = |s: &SpatialRef| s.wkt.as_deref().and_then(iau::declared).is_some();
                if (iau(self) || iau(other)) && self.proj4 != other.proj4 {
                    return false;
                }
                self.to_wkt().is_some() && self.to_wkt() == other.to_wkt()
            }
        }
    }

    /// Converts to the projection engine's CRS.
    #[doc(hidden)]
    #[cfg(feature = "wbprojection-fallback")]
    pub fn to_wbp(&self) -> Result<wbp::Crs> {
        // wbprojection reads Polar Stereographic (variant C) as variant B, with the false origin
        // at the pole: a CRS on it (by its EPSG code's WKT, else its own) is given to it as the
        // PROJ string terrio reads, or not at all.
        if let Some(w) = self.epsg.and_then(epsg::get).map(|e| e.wkt.as_str()).or(self.wkt.as_deref())
            && is_variant_c(w)
        {
            let p = wkt_proj(w).ok_or_else(|| Error::Crs("a Polar Stereographic (variant C) CRS terrio cannot convert".into()))?;
            let c = wbp::from_proj_string(&p)?;
            // `wkt_proj` gives a WKT's PROJ4 extension when it has one.
            if self.proj4.is_none() && proj4_extension(w).is_some() {
                same_figure_as_step(&c, &p)?;
            }
            return Ok(c);
        }
        if let Some(code) = self.epsg
            && let Ok(c) = wbp::Crs::from_epsg(code)
        {
            return Ok(c);
        }
        // wbprojection would read a rotated-pole grid as its geographic base CRS.
        if self.is_rotated_pole() {
            return Err(Error::Crs("a rotated-pole CRS has no wbprojection equivalent".into()));
        }
        // The PROJ string (or a WKT's PROJ4 extension) over its WKT, which wbprojection reads
        // with fewer parameters.
        // An extension only as terrio reads it: one it refuses (`+rf=0`) goes by its WKT, as
        // before terrio read extensions as pipeline steps.
        if let Some(p) = self.proj4_or_extension()
            && let Ok(c) = wbp::from_proj_string(&mercator_lat_ts_as_k(&p))
        {
            // An extension is computed as a pipeline step, whose figure wbprojection may read
            // otherwise (`+ellps=WGS84 +f=0` is a sphere to the step, WGS 84 to wbprojection):
            // refused then, rather than describe another figure.
            if self.proj4.is_none() {
                same_figure_as_step(&c, &p)?;
            }
            return Ok(c);
        }
        if let Some(w) = &self.wkt {
            // Only WKT the reader accepts: wbprojection's parser does not bound its recursion.
            if wkt1::parse(w).is_none() {
                return Err(Error::Crs("not a WKT definition GDAL reads".into()));
            }
            // Without a root code that is not its content's, nor root codes other than the one
            // it declares, which wbprojection's parser reads as the CRS (an unknown method
            // declaring EPSG:27700), as `Def` reads it.
            return Ok(wbp::Crs::from_wkt(without_unfit_root_code(w).as_deref().unwrap_or(w))?);
        }
        Err(Error::Crs("the CRS is unknown".into()))
    }
}

/// The WKT 1 of a PROJ string, as GDAL writes it (see [`wkt1_write`]).
pub(crate) fn proj_to_wkt(p: &str) -> Option<String> {
    wkt1_write::from_proj(p)
}

/// Whether a WKT 1 or WKT 2 names Polar Stereographic (variant C), EPSG method 9830, as a
/// projection or conversion method, at any depth (in a bound or compound CRS too).
pub(crate) fn is_variant_c(wkt: &str) -> bool {
    fn names(n: &wkt1::Node) -> bool {
        (["PROJECTION", "METHOD"].iter().any(|k| n.key.eq_ignore_ascii_case(k))
            && (n.epsg() == Some(9830) || n.name().is_some_and(|m| wkt1::norm(m) == "polarstereographicvariantc")))
            || n.args.iter().any(|a| matches!(a, wkt1::Arg::Node(c) if names(c)))
    }
    wkt1::parse(wkt).is_some_and(|root| names(&root))
}

/// A CRS's stored WKT as it is written where its readers end text at its first NUL (a `.prj`,
/// a raster's or layer's WKT, which GDAL reads as a C string): each quoted name cut at its first
/// NUL, as PROJ 9.8.1 gives such a name (`CRS.name` of a PROJJSON name `"WGS\u0000 84"` is
/// `WGS`), so that the text stays a whole WKT. PROJ's own WKT export of such a CRS ends at the
/// NUL (`PROJCRS["WGS`), which no reader takes back. Text without a NUL is returned as it is.
#[doc(hidden)]
pub fn wkt_for_files(wkt: &str) -> std::borrow::Cow<'_, str> {
    if !wkt.contains('\0') {
        return wkt.into();
    }
    let mut out = String::with_capacity(wkt.len());
    let (mut quoted, mut cut) = (false, false);
    let mut chars = wkt.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            // A doubled quote inside a name is a quote of the name.
            '"' if quoted && chars.peek() == Some(&'"') => {
                chars.next();
                if !cut {
                    out.push_str("\"\"");
                }
            }
            '"' => {
                quoted = !quoted;
                cut = false;
                out.push(c);
            }
            '\0' if quoted => cut = true,
            // A NUL outside quotes is dropped: the parser refuses one, so a CRS read by terrio
            // has none there, and text with one set by hand (`SpatialRef::wkt`) is written as
            // the WKT the reader would see without it, rather than one its readers cut short.
            '\0' => {}
            _ if cut => {}
            _ => out.push(c),
        }
    }
    out.into()
}

/// Text up to its first NUL: a definition given as text, as PROJ 9.8.1 and GDAL take it (a C
/// string, which ends there; text after a complete WKT was ignored before as well). Who cuts:
/// - the entry points for a definition given as text cut it themselves ([`SpatialRef::from_wkt`],
///   `parse`, `parse_literal`, `from_projjson`, and `geokeys::read` its GeoAsciiParamsTag);
/// - [`wkt_proj`] and [`identify_epsg`] read their text whole: a caller with text from a file (a
///   `.prj`, a raster's or layer's WKT) cuts it first with this;
/// - a CRS's own stored WKT ([`SpatialRef::to_wkt`], the WKT of a PROJJSON, whose names keep a
///   NUL (`\u0000`) as PROJ 9.8.1 reads them) is not cut: inside terrio-crs it goes through
///   `SpatialRef::from_wkt_text`, whose call sites say `uncut`; WKT built from it to be parsed
///   again or written goes through [`wkt_for_files`].
///
/// The WKT parser refuses a NUL outside quotes.
#[doc(hidden)]
pub fn until_nul(text: &str) -> &str {
    text.split('\0').next().unwrap_or_default()
}

/// The PROJ string of a WKT definition as terrio reads it: its PROJ4 extension, else its WKT 1
/// or WKT 2 conversion. `None` for a WKT terrio reads otherwise (through wbprojection). The WKT
/// is read whole, a NUL in a name included, as a CRS's stored WKT ([`SpatialRef::to_wkt`]) may
/// keep one from a PROJJSON name. Text read from a file must be cut at its first NUL by the
/// caller ([`until_nul`]), as GDAL reads it.
#[doc(hidden)]
pub fn wkt_proj(wkt: &str) -> Option<String> {
    if let Some(p) = proj4_extension(wkt) {
        return Some(p);
    }
    let root = wkt1::parse(wkt)?;
    let c = if wkt2::handles(&root) { wkt2::to_proj(&root).ok()? } else { wkt1::to_proj(&root)? };
    Some(c.proj)
}

/// Whether PROJ string `s`, as a WKT's PROJ4 extension (which PROJ computes on as a pipeline
/// step), places points where PROJ's CRS rules for the string place them: the same conversion
/// and figure (but for the round-off between WGS 84 and GRS 1980), and the same datum, or one
/// that places points around the projection's origin within 1 mm, both judged as without grids
/// (PROJ's default here), whatever is installed: `+datum=NAD83`, which the step takes as WGS 84
/// and whose transformations are null without grids, places as the string; a step's grid shift
/// (NAD27's) does not, nor OSGB36's Helmert shift against `pj_datum_set`'s. The writers that describe a PROJ string by those rules (PROJJSON, GeoKeys)
/// describe such an extension only, and the WKT writer writes the string itself so.
pub(crate) fn extension_places_as_string(s: &str) -> bool {
    // Both as they place points without grids, whatever is installed: PROJ's default here. Their
    // diagnostics (a missing grid, a ballpark) are this check's, not the caller's.
    warnings::silenced(|| grid::without_grids(|| extension_places_without_grids(s)))
}

/// [`extension_places_as_string`], under [`grid::without_grids`].
fn extension_places_without_grids(s: &str) -> bool {
    let (Ok(crs), Ok(step)) = (Def::from_proj(s), Def::from_proj_extension(s)) else { return false };
    if !crs.same_conversion(&step) || (crs.ell.a - step.ell.a).abs() > 1e-6 || (crs.ell.es - step.ell.es).abs() > 1e-10 {
        return false;
    }
    if crs.same_datum(&step) {
        return true;
    }
    // The step's grid shift (NAD27's, potsdam's `+nadgrids`) is no datum the CRS rules have.
    let real_grids = |op: &std::sync::Arc<datum::Op>| op.grids().iter().any(|(g, _)| g.trim_start_matches('@') != "null");
    if step.ops().iter().any(real_grids) {
        return false;
    }
    // Each with a WKT of its own stored. The `Transformer`s below compare their CRSs by WKT
    // (`same_as`), and the WKT of a CRS given only by a PROJ string is written from that string,
    // which writes its extension by this check: without the stored placeholder, this recurses
    // until the stack overflows. `Def::new` reads `proj4` first, so the placeholder is not read.
    let given = SpatialRef { proj4: Some(s.to_string()), wkt: Some("given".into()), ..Default::default() };
    let extension = SpatialRef {
        wkt: Some(format!(
            "GEOGCS[\"step\",DATUM[\"step\",SPHEROID[\"step\",6378137,298.257223563]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433],EXTENSION[\"PROJ4\",\"{}\"]]",
            s.replace('"', "")
        )),
        ..Default::default()
    };
    let Ok(wgs84) = SpatialRef::from_epsg(4326) else { return false };
    let (Ok(to_given), Ok(to_extension)) = (Transformer::new(&wgs84, &given), Transformer::new(&wgs84, &extension)) else {
        return false;
    };
    // The datum's transformations without grids (NAD83's null one, OSGB36's Helmert ones)
    // against the step's shift, around the origin (in Greenwich longitudes): on a 3 × 3 grid
    // 5° apart and 4 points 15° away.
    let (lon, lat) = crs.params().map_or((0.0, 0.0), |p| (p.lon_0 + p.prime_meridian, p.lat_0));
    // Metres per coordinate unit (`+to_meter`).
    let scale = if crs.is_geographic() { 111_195.0 } else { crs.to_meter() };
    let offsets = [-5.0, 0.0, 5.0].into_iter().flat_map(|x| [-5.0, 0.0, 5.0].map(|y| (x, y)));
    let mut compared = 0;
    let alike = offsets.chain([(-15.0, 0.0), (15.0, 0.0), (0.0, -15.0), (0.0, 15.0)]).all(|(dx, dy)| {
        let point = ((lon + dx + 180.0).rem_euclid(360.0) - 180.0, (lat + dy).clamp(-89.0, 89.0));
        match (to_given.transform(point.0, point.1), to_extension.transform(point.0, point.1)) {
            (Ok(a), Ok(b)) => {
                compared += 1;
                ((a.0 - b.0) * scale).hypot((a.1 - b.1) * scale) < 1e-3
            }
            // Outside the projection's domain on both sides.
            (Err(_), Err(_)) => true,
            _ => false,
        }
    });
    // Not alike when no point could be compared.
    alike && compared > 0
}

/// Fails when wbprojection reads the PROJ string `p` of a WKT's PROJ4 extension on another figure
/// than terrio's pipeline step does (`+ellps=WGS84 +f=0` is a sphere to the step, WGS 84 to
/// wbprojection, which takes `+ellps` before `+f`), rather than describe another figure.
#[cfg(feature = "wbprojection-fallback")]
fn same_figure_as_step(c: &wbp::Crs, p: &str) -> Result<()> {
    let Ok(step) = Def::from_proj_extension(p) else { return Ok(()) };
    let e = &c.projection.params().ellipsoid;
    let es = e.f * (2.0 - e.f);
    if (e.a - step.ell.a).abs() > 1e-3 || (es - step.ell.es).abs() > 1e-10 {
        return Err(Error::Crs(format!("wbprojection reads another ellipsoid than terrio's for {p}")));
    }
    Ok(())
}

impl SpatialRef {
    /// The CRS's PROJ string, or else its WKT 1's `EXTENSION["PROJ4", ...]` as terrio reads it (one
    /// it refuses, such as `+rf=0`, is none): the string a CRS from such a WKT computes on.
    #[doc(hidden)]
    pub fn proj4_or_extension(&self) -> Option<String> {
        self.proj4.clone().or_else(|| self.wkt.as_deref().and_then(proj4_extension).filter(|p| Def::from_proj_extension(p).is_ok()))
    }
}

/// Whether a WKT 1 has a root `EXTENSION["PROJ4", ...]`, which PROJ and terrio compute on in place
/// of its nodes.
#[doc(hidden)]
pub fn has_proj4_extension(wkt: &str) -> bool {
    proj4_extension(wkt).is_some()
}

/// The PROJ string of a WKT 1's root `EXTENSION["PROJ4", ...]`.
pub(crate) fn proj4_extension(wkt: &str) -> Option<String> {
    let root = wkt1::parse(wkt)?;
    let ext = root.children("EXTENSION").find(|n| n.name().is_some_and(|k| k.eq_ignore_ascii_case("PROJ4")))?;
    match ext.args.get(1)? {
        wkt1::Arg::Str(p) if p.trim_start().starts_with('+') => Some(p.trim().to_string()),
        _ => None,
    }
}

/// A PROJ string whose Mercator standard parallel (`+lat_ts`) is given as the scale at the
/// equator it implies (`+k`): wbprojection's Mercator has no standard parallel.
#[cfg(feature = "wbprojection-fallback")]
fn mercator_lat_ts_as_k(p: &str) -> std::borrow::Cow<'_, str> {
    let tokens: Vec<&str> = p.split_whitespace().collect();
    let lat_ts = tokens.iter().find_map(|t| t.strip_prefix("+lat_ts=")).and_then(|v| v.parse::<f64>().ok());
    let (true, Some(ts)) = (tokens.contains(&"+proj=merc"), lat_ts) else { return p.into() };
    let Ok(crs) = wbp::from_proj_string(p) else { return p.into() };
    let (sin, e2) = (ts.to_radians().sin(), crs.datum.ellipsoid.e2);
    let k = ts.to_radians().cos() / (1.0 - e2 * sin * sin).sqrt();
    let kept = tokens.iter().filter(|t| !t.starts_with("+lat_ts=") && !t.starts_with("+k=") && !t.starts_with("+k_0="));
    let mut out: Vec<String> = kept.map(|t| t.to_string()).collect();
    out.push(format!("+k={k}"));
    out.join(" ").into()
}

impl std::fmt::Display for SpatialRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label())
    }
}

/// Datum realizations, which a WKT definition names when it means them.
const REALIZATIONS: &[&str] = &["2011", "HARN", "NSRS", "CSRS", "CORS", "MA11", "PA11", "NAD83(", "ITRF", "GDA2020", "ETRF"];

/// ESRI's compound CRS, a horizontal CRS followed by a `VERTCS`, as the `COMPD_CS` PROJ 9.9.0
/// reads it as (EPSG:9707 in ESRI's WKT), named after its two parts as by PROJ. `None` for another
/// WKT, and when the `COMPD_CS` would be longer than GDAL imports (the WKT is then read by its
/// first node, as before). [`SpatialRef::from_wkt`], [`identify_epsg`] and `Def::from_wkt` read
/// it so, for a `SpatialRef` built from a file's text too.
pub(crate) fn esri_compound_wkt(wkt: &str) -> Option<String> {
    let (horizontal, vertical) = wkt1::esri_compound(wkt)?;
    let name = format!("{} + {}", horizontal.name().unwrap_or("unnamed"), vertical.name().unwrap_or("unnamed"));
    let args = vec![wkt1::Arg::Str(name), wkt1::Arg::Node(horizontal), wkt1::Arg::Node(vertical)];
    Some(wkt1::Node { key: "COMPD_CS".into(), args }.to_wkt()).filter(|w| w.len() <= wkt1::MAX_LEN)
}

/// The semi-major axis in metres of the first `SPHEROID` or `ELLIPSOID` of a WKT definition (in
/// its `LENGTHUNIT`, as PROJ reads a WKT 2 ellipsoid in feet).
#[doc(hidden)]
pub fn semi_major_axis(wkt: &str) -> Option<f64> {
    Some(wkt1::parse(wkt)?.find(keywords::ELLIPSOID)?.ellipsoid()?.0)
}

/// The semi-major and semi-minor axes in metres of the first `SPHEROID` or `ELLIPSOID` of a WKT
/// definition, from its axis and inverse flattening (0 for a sphere).
fn wkt_ellipsoid(wkt: &str) -> Option<(f64, f64)> {
    let (a, rf) = wkt1::parse(wkt)?.find(keywords::ELLIPSOID)?.ellipsoid()?;
    Some((a, if rf == 0.0 { a } else { a * (1.0 - 1.0 / rf) }))
}

/// The EPSG datums the geodetic datum of a WKT definition is: the one its `AUTHORITY` (WKT 1) or
/// `ID` (WKT 2) declares, else those it is named as (`D_European_1950` is ED50). Empty for a datum
/// with another name (`Unknown based on Bessel 1841 ellipsoid`), as for GDAL.
#[cfg(feature = "wbprojection-fallback")]
fn wkt_datums(wkt: &str) -> Vec<u32> {
    wkt_datums_and_others(wkt).0
}

/// The first geodetic datum (or datum ensemble) node of a WKT definition.
fn wkt_datum(wkt: &str) -> Option<wkt1::Node> {
    wkt1::parse(wkt).and_then(|root| root.find(keywords::GEODETIC_DATUM).cloned())
}

/// The name and EPSG code (from its `ID` or `AUTHORITY`) of the first datum of a WKT definition.
pub(crate) fn wkt_datum_name_and_code(wkt: &str) -> Option<(String, Option<u32>)> {
    let datum = wkt_datum(wkt)?;
    Some((datum.name().unwrap_or_default().to_string(), datum.epsg()))
}

/// The EPSG geographic CRS PROJ 9.8.1 identifies a WKT 1 `GEOGCS` as by its name alone, on a datum
/// no EPSG geographic CRS is on and with which the CRS's datum has a name in common: ESRI's
/// GCS_Greenland_1996 on `D_Greenland_1996` (Greenland 1996, 6747) as GR96, EPSG:4747, on the GR96
/// ensemble (1421). Not for another name on that datum, where PROJ identifies none, nor for a
/// geographic 3D CRS (ESRI's `LINUNIT`), which the name table leaves out, nor for WKT 2, where PROJ
/// does not read ESRI's datum names. The caller gives it as the geographic CRS only, not as the
/// CRS's own code: PROJ places such a CRS by a ballpark, not by the transformations of GR96.
pub(crate) fn identify_by_shared_name(geogcs: &str) -> Option<u32> {
    let (datums, _) = wkt_datums_and_others(geogcs);
    if datums.is_empty() || !datums.iter().all(|d| epsg::geographic_on(*d).is_empty()) {
        return None;
    }
    let root = wkt1::parse(geogcs)?;
    if !root.key.eq_ignore_ascii_case("GEOGCS") || root.child("LINUNIT").is_some() {
        return None;
    }
    // ESRI's names for a `GCS_` geographic CRS, as PROJ's WKT reader maps them.
    let esri = root.name().is_some_and(|n| n.starts_with("GCS_"));
    let code = epsg::crs_named_as_proj(root.name()?, false, false, esri)?;
    let datum = epsg::get(code)?.datum;
    if datums.contains(&datum) || !datums.iter().any(|d| epsg::datums_share_a_name(*d, datum)) {
        return None;
    }
    // The same conversion on the same ellipsoid.
    let own = Def::wkt_conversion(geogcs)?;
    let theirs = Def::new(&SpatialRef { epsg: Some(code), ..Default::default() }).ok()?;
    (own.same_conversion(&theirs) && (own.ell.a - theirs.ell.a).abs() < 1e-3 && (own.ell.es - theirs.ell.es).abs() < 1e-9).then_some(code)
}

/// The EPSG datums of a WKT definition's geodetic datum, and the other datums
/// whose CRSs ESRI writes with its name (ETRS89-NOR for `D_ETRS_1989`).
fn wkt_datums_and_others(wkt: &str) -> (Vec<u32>, &'static [u32]) {
    let Some(datum) = wkt_datum(wkt) else { return (Vec::new(), &[]) };
    if let Some(code) = datum.epsg() {
        return (vec![code], &[]);
    }
    let name = datum.name().unwrap_or_default();
    (epsg::datums_named(name), epsg::datums_written_as(name))
}

/// The EPSG code of the compound CRS of a horizontal and a vertical EPSG CRS, as GDAL finds one
/// for a GeoTIFF's GeoKeys, which give the two codes but none for the compound
/// (`GTIFGetOGISDefnAsOSR`'s `FindBestMatch(100)`): the registry's only compound CRS of the two,
/// or the one named `name` among several. `None` when there is none.
pub fn compound_code(horizontal: u32, vertical: u32, name: Option<&str>) -> Option<u32> {
    epsg::compound_of(horizontal, vertical, name)
}

/// Identifies the EPSG code of a WKT definition, as GDAL does: a CRS on the WKT's datum (by its
/// code or name) with the same conversion, units and ellipsoid. A datum without code that terrio
/// does not know by name has no code, whatever its ellipsoid (#125). Among the CRSs of terrio's
/// table that match, the one named as the WKT's CRS, then the one with its axis order, then the
/// lowest code; for other CRSs, wbprojection's registry, where among equally good matches the
/// base datum is preferred over its later realizations (NAD83 over NAD83(HARN) and NAD83(2011))
/// unless the WKT names one, then the lowest code.
///
/// The WKT is read whole, a NUL in a name included, as [`wkt_proj`] reads it: text read from a
/// file must be cut at its first NUL by the caller ([`until_nul`]).
#[doc(hidden)]
pub fn identify_epsg(wkt: &str) -> Option<u32> {
    if let Some(compound) = esri_compound_wkt(wkt) {
        return identify_epsg(&compound);
    }
    // A declared code only when the WKT's own content is that CRS, as PROJ 9.8.1 keeps the
    // content of a WKT whose root `ID` or `AUTHORITY` is wrong (a geographic code on a projected
    // CRS, a code that does not exist, a compound code's units); else the WKT is identified by its
    // content, as without the code.
    if let Some(code) = declared_epsg(wkt) {
        if declared_fits(wkt, code) {
            return Some(code);
        }
        // Content terrio cannot read (an unknown method, or a CRS without a conversion such as
        // PROJ's simplified WKT 2 of a geocentric CRS) is not identified loosely without the
        // code: no code, as PROJ identifies none for those.
        Def::wkt_conversion(wkt)?;
        return strip_root_code(wkt).and_then(|w| identify_epsg(&w));
    }
    // A compound CRS is not its horizontal CRS: only the code it declares identifies it, as in
    // `SpatialRef::from_wkt`.
    if def::split_compound(wkt).is_some() {
        return None;
    }
    // A vertical CRS by its name, datum, unit and direction, as PROJ 9.9.0 identifies it.
    if keywords::is(keywords::root_key(wkt), keywords::VERTICAL_CRS) {
        return def::vertical_code(wkt);
    }
    let (datums, others) = wkt_datums_and_others(wkt);
    // No known datum (or no datum node, or WKT the reader cannot parse): no code, and on purpose
    // not the registry's loose guess (#125).
    if datums.is_empty() {
        return None;
    }
    let own = Def::wkt_conversion(wkt);
    // The same conversion on the same ellipsoid.
    let same = |own: &Def, code: u32| {
        let registered = SpatialRef { epsg: Some(code), ..Default::default() };
        Def::new(&registered)
            .is_ok_and(|d| own.same_conversion(&d) && (own.ell.a - d.ell.a).abs() < 1e-3 && (own.ell.es - d.ell.es).abs() < 1e-9)
    };
    // Among CRSs that match, the one named as the WKT's CRS (`RGF93 v2 (lon-lat)`, ESRI's `GCS_`
    // prefix aside), else a current one over a deprecated one (as PROJ ranks them), else the base
    // datum over its later realizations (`NAD83`, which EPSG lists as an alias of NAD83(HARN) too)
    // unless the WKT names one, else the lowest code.
    let key = |n: &str| epsg::datum_key(n.strip_prefix("GCS_").unwrap_or(n));
    let name = wkt_name(wkt).map(|n| key(&n));
    let named = |c: u32| epsg::get(c).is_some_and(|e| Some(key(&e.name)) == name);
    let upper = wkt.to_ascii_uppercase();
    let wkt_names_realization = REALIZATIONS.iter().any(|r| upper.contains(r));
    let realization = |code: u32| {
        let name = registry_name(code).map(|name| name.to_ascii_uppercase()).unwrap_or_default();
        !wkt_names_realization && REALIZATIONS.iter().any(|r| name.contains(r))
    };
    // A CRS of the table on these datums with the WKT's conversion, as GDAL finds it.
    let search = |datums: &[u32]| {
        let own = own.as_ref()?;
        let geogs = datums.iter().flat_map(|d| epsg::geographic_on(*d)).copied();
        // A geocentric or geographic 3D CRS among the geodetic CRSs of its kind (EPSG:4979's
        // content is not EPSG:4326, as PROJ identifies it).
        let kind = if own.is_geocentric() {
            Some(epsg::GeodeticKind::Geocentric)
        } else if own.is_geographic() && geographic_dimension_3d(wkt) == Some(true) {
            Some(epsg::GeodeticKind::Geographic3D)
        } else {
            None
        };
        if let Some(kind) = kind {
            return datums
                .iter()
                .filter_map(|d| epsg::registry().by_datum.get(d))
                .flatten()
                .copied()
                .filter(|c| epsg::registry().geodetic.get(c).is_some_and(|g| g.kind == kind) && same(own, *c))
                // `by_datum` holds current CRSs only.
                .min_by_key(|c| (!named(*c), *c));
        }
        if own.is_geographic() {
            return geogs.filter(|c| same(own, *c)).min_by_key(|c| (!named(*c), epsg::is_deprecated(*c), realization(*c), *c));
        }
        let method = conversion_method(wkt)?;
        let found = geogs.flat_map(epsg::projected_on).filter(|(c, m)| same_method(m, &method) && same(own, *c)).map(|(c, _)| *c);
        // Else the one with the WKT's axis order (ETRS89 / UTM zone 32N rather than its N-E
        // variant for ESRI's WKT, without axes).
        let own_north_first = north_first(wkt);
        let axes = |c: u32| epsg::get(c).is_some_and(|e| north_first(&e.wkt) != own_north_first);
        found.min_by_key(|c| (!named(*c), epsg::is_deprecated(*c), axes(*c), realization(*c), *c))
    };
    // On the datum the WKT names, else on those whose CRSs ESRI writes with its name: GDAL
    // identifies ESRI's ETRS_1989_NTM_Zone_18 on D_ETRS_1989 by its name as EPSG:5118, on
    // ETRS89-NOR.
    if let Some(code) = search(&datums).or_else(|| search(others)) {
        return Some(code);
    }
    #[cfg(feature = "wbprojection-fallback")]
    {
        // A current CRS on the WKT's datum. Its datum: its code in the table, else its WKT's (WKT 2
        // CRSs of the table, on datum ensembles, and those only the registry has).
        let on_datum = |code: u32| {
            let theirs = match epsg::get(code) {
                // Not a deprecated code that has a replacement (one without stays, EPSG:2008).
                Some(_) if !epsg_replacements(code).is_empty() => return false,
                Some(e) if e.datum != 0 => vec![e.datum],
                Some(e) => wkt_datums(&e.wkt),
                None => wbp::to_ogc_wkt(code).map(|w| wkt_datums(&w)).unwrap_or_default(),
            };
            theirs.iter().any(|d| datums.contains(d))
        };
        // Else wbprojection's registry (projections terrio cannot compare, CRSs its table lacks).
        let report = wbp::identify_epsg_from_wkt_report(wkt, wbp::EpsgIdentifyPolicy::Lenient)?;
        // The registry matches loosely: keep a code only when its definition converts coordinates
        // as the WKT does, on its datum (GDAL leaves a custom CRS without code).
        let fits = |code: u32| {
            if !on_datum(code) {
                return false;
            }
            // A projection terrio cannot compare: at least the same ellipsoid (a sinusoidal
            // projection on MODIS's sphere is not ESRI's World Sinusoidal on WGS 84).
            let Some(own) = &own else {
                let candidate = registry_wkt(code);
                return match (semi_major_axis(wkt), candidate.as_deref().and_then(semi_major_axis)) {
                    (Some(a), Some(b)) => (a - b).abs() < 1e-3,
                    _ => true,
                };
            };
            same(own, code)
        };
        // The registry can take a datum's or prime meridian's code for the CRS's: then match the WKT
        // by its parameters alone.
        if report.used_embedded_epsg && !report.resolved_code.is_some_and(|c| wbp::to_ogc_wkt(c).is_ok()) {
            let stripped = strip_authorities(wkt);
            return if stripped != wkt { identify_epsg(&stripped) } else { None };
        }
        if report.used_embedded_epsg || !report.ambiguous {
            return report.resolved_code.filter(|c| fits(*c));
        }
        let best = report.top_candidates.first()?.total_score;
        report
            .top_candidates
            .iter()
            .filter(|c| c.total_score >= best && fits(c.code))
            .min_by_key(|c| (realization(c.code), c.code))
            .map(|c| c.code)
            .or(report.resolved_code.filter(|c| fits(*c)))
    }
    #[cfg(not(feature = "wbprojection-fallback"))]
    None
}

/// A registry CRS's WKT 1, with its PROJ string as an extension when it is on the authalic sphere
/// of its ellipsoid (`+R_A`, the spherical Lambert azimuthal equal-area of EPSG:3973 and 3974),
/// which WKT 1 has no method for: PROJ 9.8.1 writes those as the ellipsoidal method, which it
/// and terrio read back about 13 km away (a WKT's content, not its code, gives the CRS).
/// Read back, that WKT is its code (see [`SpatialRef::from_wkt_text`]).
fn spherical_extension(wkt: &str, proj: &str) -> String {
    if !proj.split_whitespace().any(|p| p == "+R_A") || !wkt.starts_with("PROJCS[") || wkt.contains("EXTENSION[") {
        return wkt.to_string();
    }
    let proj =
        format!("{} +no_defs", proj.split_whitespace().filter(|p| *p != "+no_defs" && *p != "+type=crs").collect::<Vec<_>>().join(" "));
    // Before the root's code, as GDAL writes it.
    // The root's own `AUTHORITY`: its last child, closed with the root (`...]]`), not a child's.
    let at = wkt.rfind(",AUTHORITY[").filter(|i| wkt[*i..].matches(']').count() == 2).unwrap_or(wkt.len() - 1);
    format!("{},EXTENSION[\"PROJ4\",\"{proj}\"]{}", &wkt[..at], &wkt[at..])
}

/// Whether a WKT is the registry's own WKT of an EPSG CRS with its PROJ string as an extension
/// (see [`spherical_extension`]).
pub(crate) fn is_registry_extension(wkt: &str, code: u32) -> bool {
    epsg::get(code).is_some_and(|e| e.wkt != wkt && spherical_extension(&e.wkt, &e.proj) == wkt)
}

/// Whether a horizontal CRS's WKT is on the datum of the registry's CRS `code` (`theirs`), as
/// [`declared_fits`] compares it.
pub(crate) fn declared_datum_fits(wkt: &str, code: u32, theirs: &Def) -> bool {
    // On another datum than the code's, when the WKT's names one terrio knows (ETRS89 declaring
    // NAD83, both on GRS 1980): not the code, whose transformations would move it wrongly.
    // Both as their names identify them (and the code's datum), others included.
    // Datums terrio has CRSs on (not a superseded code a name still gives: Greenland 1996's 6747).
    let (named_datums, own_others) = wkt_datums_and_others(wkt);
    let mut own_datums = named_datums.clone();
    own_datums.retain(|d| !epsg::geographic_on(*d).is_empty());
    let registered = SpatialRef { epsg: Some(code), ..Default::default() }.to_wkt();
    let registered = registered.as_deref().map(|w| def::split_compound(w).map(|(h, _)| h).unwrap_or_else(|| w.to_string()));
    let (mut their_datums, their_others) = registered.as_deref().map(wkt_datums_and_others).unwrap_or_default();
    their_datums.extend(theirs.datum_code());
    // A datum of the same name is the same: EPSG gives Greenland 1996 two codes (6747 and 1421),
    // and a deprecated CRS (EPSG:4640) is on a superseded datum of the current one's name, which
    // ESRI's `D_RRAF_1991` identifies.
    let own_name = datum_name(wkt);
    let same_name = || {
        let own: Vec<String> =
            own_name.iter().cloned().chain(named_datums.iter().filter_map(|d| epsg::datum_name(*d))).map(|n| epsg::datum_key(&n)).collect();
        let registered_name = registered.as_deref().and_then(datum_name);
        registered_name
            .into_iter()
            .chain(their_datums.iter().filter_map(|d| epsg::datum_name(*d)))
            .any(|n| own.contains(&epsg::datum_key(&n)))
    };
    // A datum terrio does not know is not the code's unless named as it (PROJ keeps no code
    // either): a local datum declaring ETRS89 would get ETRS89's transformations.
    // PROJ 9.8.1's placeholder `unknown` (`D_unknown`), spelt so, matches any datum.
    let placeholder = own_name.as_deref().is_some_and(|n| n.strip_prefix("D_").unwrap_or(n) == "unknown");
    // A named datum terrio has no CRS on (Greenland 1996's 6747 and 1421) is compared too.
    let unknown = own_datums.is_empty() && own_others.is_empty() && own_name.is_some() && !placeholder;
    !((!own_datums.is_empty() || unknown)
        && !their_datums.is_empty()
        && !own_datums.iter().chain(&named_datums).any(|d| their_datums.contains(d) || their_others.contains(d))
        && !their_datums.iter().any(|d| own_others.contains(d))
        // A named datum terrio has no CRSs on, and the code's datum, that PROJ's database gives a
        // name in common (`GR96` for Greenland 1996, 6747, and its ensemble, 1421). Not datums
        // terrio has CRSs on, which aliases also share (`NAD83` for NAD83 and NAD83(HARN)).
        && !named_datums
            .iter()
            .filter(|d| !own_datums.contains(d))
            .any(|d| their_datums.iter().any(|t| epsg::datums_share_a_name(*d, *t)))
        && !same_name())
}

/// Whether a geographic CRS's WKT is the EPSG CRS it declares but for its prime meridian: on its
/// datum, so with its transformations after the meridian's shift, as PROJ 9.8.1 places NTF (Paris)
/// on another meridian through NTF (Paris) and "NTF to WGS 84 (1)".
pub(crate) fn declared_fits_but_meridian(wkt: &str, code: u32) -> bool {
    let (Some(mut own), Ok(theirs)) = (Def::wkt_conversion(wkt), Def::new(&SpatialRef { epsg: Some(code), ..Default::default() })) else {
        return false;
    };
    // A geographic code, not a compound one (a vertical CRS the GEOGCS has none of).
    if !theirs.is_geographic() || theirs.vertical().is_some() || own.pm() == theirs.pm() {
        return false;
    }
    own.set_pm(theirs.pm());
    own.same_conversion(&theirs)
        && (own.ell.a - theirs.ell.a).abs() < 1e-3
        && (own.ell.es - theirs.ell.es).abs() < 1e-9
        && declared_datum_fits(wkt, code, &theirs)
}

/// Whether a WKT's own content is the EPSG CRS it declares: the same conversion (units and
/// axes included) on the same ellipsoid and datum, and of the same kind (geocentric, geographic 2D
/// or 3D). When terrio cannot read the content (a method only the registry has), when the WKT
/// names the code's own projection method.
pub(crate) fn declared_fits(wkt: &str, code: u32) -> bool {
    // Only a horizontal CRS's content is compared (not a vertical or engineering CRS's).
    let horizontal = wkt1::parse(wkt).is_some_and(|r| r.is(keywords::HORIZONTAL_CRS) || r.is(keywords::COMPOUND_CRS));
    // A vertical CRS's code only of its unit and direction; another root's (engineering, local)
    // not the code of a CRS terrio places (a vertical CRS declaring EPSG:4326 is not WGS 84).
    if !horizontal {
        let vertical = wkt1::parse(wkt).is_some_and(|r| r.is(keywords::VERTICAL_CRS));
        return if vertical {
            def::vertical_code(wkt) == Some(code)
        } else {
            epsg::vertical(code).is_none() && Def::new(&SpatialRef { epsg: Some(code), ..Default::default() }).is_err()
        };
    }
    // A compound CRS by its horizontal CRS, and its vertical CRS below.
    let compound = def::split_compound(wkt);
    let horizontal = compound.as_ref().map(|(h, _)| h.clone());
    let own = Def::wkt_conversion(horizontal.as_deref().unwrap_or(wkt));
    // A code terrio cannot build (EPSG:1, or one only wbprojection's registry has, without that
    // feature): kept only when the content cannot be read either, as there is nothing to compare.
    let Ok(theirs) = Def::new(&SpatialRef { epsg: Some(code), ..Default::default() }) else { return own.is_none() };
    // The vertical CRS of a compound code is the code's: its vertical code when the WKT's has one,
    // its unit and direction (heights in feet or depths declaring a code in metres up are not
    // that code, whose heights terrio would give).
    // Nor is a CRS without a vertical CRS a compound code (EPSG:9707's heights it has none of).
    if compound.is_none() && theirs.vertical().is_some() {
        return false;
    }
    if let Some((_, v)) = &compound {
        let Some(their_vertical) = theirs.vertical() else { return false };
        // Another vertical code, unless its vertical datum is the code's by name (PROJ keeps
        // EPSG:7405 whose vertical CRS, Ordnance Datum Newlyn, declares another code).
        let vertical_datum = |w: &str| {
            let root = wkt1::parse(w)?;
            let datum = root.child_in(keywords::VERTICAL_DATUM)?;
            datum.name().map(epsg::datum_key)
        };
        let same_vertical_datum = || {
            let theirs = their_vertical.code.and_then(epsg::vertical).and_then(|t| vertical_datum(&t.wkt));
            vertical_datum(v).is_some_and(|own| Some(own) == theirs)
        };
        if declared_epsg(v).is_some_and(|c| their_vertical.code.is_some_and(|t| t != c)) && !same_vertical_datum() {
            return false;
        }
        let Some((to_meter, down)) = vertical_axis(v) else { return false };
        if (to_meter - their_vertical.to_meter).abs() > 1e-9 * their_vertical.to_meter || down != their_vertical.down {
            return false;
        }
    }
    if !declared_datum_fits(horizontal.as_deref().unwrap_or(wkt), code, &theirs) {
        return false;
    }
    // A geographic 2D CRS is not a 3D code, nor the reverse (PROJ identifies EPSG:4979's WKT
    // declaring 4326 as 4979).
    if let Some(three_d) = geographic_dimension_3d(horizontal.as_deref().unwrap_or(wkt))
        && theirs.is_geographic()
        && theirs.vertical().is_none()
        && three_d != theirs.has_height()
    {
        return false;
    }
    match own {
        Some(own) => own.same_conversion(&theirs) && (own.ell.a - theirs.ell.a).abs() < 1e-3 && (own.ell.es - theirs.ell.es).abs() < 1e-9,
        None => {
            // The code's method, by EPSG's name (PROJ's own WKT 2 of Belge 72, `Lambert Conic
            // Conformal (2SP Belgium)`, which the registry's WKT 1 names otherwise) or by the
            // registry WKT's name. Not another method (Belge 72 or an unknown method declaring
            // EPSG:27700).
            let method = |w: &str| {
                let root = wkt1::parse(w)?;
                let node = root.child("PROJECTION").or_else(|| root.child("CONVERSION").and_then(|c| c.child("METHOD")))?.clone();
                Some(node)
            };
            // A CRS without a conversion (a geocentric CRS in PROJ's simplified WKT 2, whose
            // bare `UNIT` the reader refuses): the code's, on its ellipsoid.
            // Of the code's kind: a geocentric WKT is not a geographic code on its ellipsoid.
            // A compound CRS by its horizontal CRS, the registry's too.
            let wkt = horizontal.as_deref().unwrap_or(wkt);
            let Some(own) = method(wkt) else {
                let Some(root) = wkt1::parse(wkt) else { return false };
                let converted = root.child("PROJECTION").is_some() || root.child("CONVERSION").is_some();
                let cartesian = root
                    .child("CS")
                    .is_some_and(|c| matches!(c.args.first(), Some(wkt1::Arg::Ident(t)) if t.eq_ignore_ascii_case("Cartesian")));
                let geocentric = root.key.eq_ignore_ascii_case("GEOCCS") || (root.is(keywords::GEODETIC_CRS) && cartesian);
                return !converted
                    && geocentric == theirs.is_geocentric()
                    && semi_major_axis(wkt).is_some_and(|a| (a - theirs.ell.a).abs() < 1e-3);
            };
            let registered = epsg::get(code)
                .map(|e| e.wkt.clone())
                .or_else(|| registry_wkt(code))
                .map(|w| def::split_compound(&w).map(|(h, _)| h).unwrap_or(w));
            let Some(name) = own.name().map(wkt1::norm) else { return false };
            catalog::epsg_method(code).is_some_and(|m| wkt1::norm(m) == name)
                || registered.as_deref().and_then(method).is_some_and(|t| t.name().map(wkt1::norm).as_deref() == Some(name.as_str()))
        }
    }
}

/// The node of a WKT whose axes are its horizontal axes, as PROJ 9.9.0 reads them: a compound
/// CRS's horizontal CRS, and a bound CRS's source CRS (#1012 item 60); else its root.
fn horizontal_root(wkt: &str) -> Option<wkt1::Node> {
    let horizontal = def::split_compound(wkt).map(|(horizontal, _)| horizontal);
    let root = wkt1::parse(horizontal.as_deref().unwrap_or(wkt))?;
    let source = root.key.eq_ignore_ascii_case("BOUNDCRS").then(|| root.child("SOURCECRS")).flatten().and_then(|s| {
        s.args.iter().find_map(|a| match a {
            wkt1::Arg::Node(n) => Some(n.clone()),
            _ => None,
        })
    });
    Some(source.unwrap_or(root))
}

/// The metres per unit of a vertical CRS's WKT (`VERT_CS`, ESRI's `VERTCS`, `VERTCRS`), from its
/// `UNIT` or its axis's, and whether its axis points down (depths): by its `AXIS`, else by ESRI's
/// `PARAMETER["Direction",-1]` (PROJ 9.9.0 takes exactly -1 as down, and an `AXIS` before it).
pub(crate) fn vertical_axis(wkt: &str) -> Option<(f64, bool)> {
    let root = wkt1::parse(wkt)?;
    let axis = root.child("AXIS");
    let down = match axis {
        Some(a) => matches!(a.args.get(1), Some(wkt1::Arg::Ident(d)) if d.eq_ignore_ascii_case("down")),
        None => root.children("PARAMETER").any(|p| p.name().is_some_and(|n| n.eq_ignore_ascii_case("Direction")) && p.num(1) == Some(-1.0)),
    };
    let unit = root
        .child("UNIT")
        .or_else(|| root.child("LENGTHUNIT"))
        .or_else(|| axis.and_then(|a| a.child("LENGTHUNIT").or_else(|| a.child("UNIT"))));
    Some((unit.and_then(|u| u.num(1)).unwrap_or(1.0), down))
}

/// Whether a WKT 2 geographic CRS is 3D, by its `CS` dimension; `None` when it is not one. A
/// WKT 1 `GEOGCS` is either, as PROJ 9.8.1 keeps EPSG:4909 (GR96 3D) declared by one with two
/// axes, and Esri's WKT 1 of a 3D CRS has none.
fn geographic_dimension_3d(wkt: &str) -> Option<bool> {
    let root = wkt1::parse(wkt)?;
    let cs = root.child("CS")?;
    let ellipsoidal = matches!(cs.args.first(), Some(wkt1::Arg::Ident(t)) if t.eq_ignore_ascii_case("ellipsoidal"));
    let geodetic = root.is(keywords::GEOGRAPHIC_OR_GEODETIC_CRS);
    (geodetic && ellipsoidal).then(|| cs.num(1) == Some(3.0))
}

/// A WKT definition with no root identifier wbprojection's parser would read as another CRS, as
/// it reads a WKT by its root code (by its last, of several): without its root code when that code
/// is not its content's (see [`declared_fits`]); of several root identifiers, with only the EPSG
/// code it declares when that is its content's, else with none (see [`declared_epsg`]). `None` when
/// it has one identifier that is its content's or no EPSG code.
#[cfg(feature = "wbprojection-fallback")]
pub(crate) fn without_unfit_root_code(wkt: &str) -> Option<String> {
    let mut root = wkt1::parse(wkt)?;
    if root.ids().count() < 2 {
        return match declared_epsg(wkt) {
            Some(c) if !declared_fits(wkt, c) => strip_root_code(wkt),
            _ => None,
        };
    }
    // The declared identifier's place among the root's children, when it fits.
    let keep = declared_id(&root, "EPSG")
        .filter(|id| id.code().is_some_and(|c| declared_fits(wkt, c)))
        .and_then(|id| root.args.iter().position(|a| matches!(a, wkt1::Arg::Node(n) if std::ptr::eq(n, id))));
    let mut i = 0;
    root.args.retain(|a| {
        let kept = !matches!(a, wkt1::Arg::Node(n) if n.is(keywords::ID)) || Some(i) == keep;
        i += 1;
        kept
    });
    Some(root.to_wkt())
}

/// A WKT definition without its root's identifiers (all of them, of any authority; see
/// [`declared_epsg`]), written again from its nodes; `None` when its root has none.
pub(crate) fn strip_root_code(wkt: &str) -> Option<String> {
    let mut root = wkt1::parse(wkt)?;
    let before = root.args.len();
    root.args.retain(|a| !matches!(a, wkt1::Arg::Node(n) if n.is(keywords::ID)));
    (root.args.len() < before).then(|| root.to_wkt())
}

/// The projection method (`+proj=tmerc`) of a WKT definition's conversion.
fn conversion_method(wkt: &str) -> Option<String> {
    let root = wkt1::parse(wkt)?;
    let c = if wkt2::handles(&root) { wkt2::to_proj(&root).ok()? } else { wkt1::to_proj(&root)? };
    c.proj.split_whitespace().next().map(str::to_string)
}

/// Whether the first axis of a WKT definition's coordinate system points north or south
/// (northing, easting), rather than east (or is not given).
fn north_first(wkt: &str) -> bool {
    let first = wkt1::parse(wkt).and_then(|root| root.child("AXIS").and_then(|a| a.args.get(1).cloned()));
    matches!(first, Some(wkt1::Arg::Ident(d)) if ["north", "south"].iter().any(|n| d.eq_ignore_ascii_case(n)))
}

/// Whether two `+proj=` methods are the same (UTM being a transverse Mercator).
fn same_method(a: &str, b: &str) -> bool {
    let m = |s: &str| if s == "+proj=utm" { "+proj=tmerc".to_string() } else { s.to_string() };
    m(a) == m(b)
}

/// A WKT definition without its `AUTHORITY[...]` nodes.
#[cfg(feature = "wbprojection-fallback")]
fn strip_authorities(wkt: &str) -> String {
    let mut out = wkt.to_string();
    while let Some(start) = out.find(",AUTHORITY[") {
        let Some(len) = out[start..].find(']') else { break };
        out.replace_range(start..start + len + 1, "");
    }
    out
}

/// The EPSG code a WKT definition declares for itself: its outermost node's `AUTHORITY["EPSG",
/// "4269"]` (WKT 1) or `ID["EPSG",4269]` (WKT 2), not those of its datum or units. The identifier
/// is read wherever it is among the root's children and whatever it holds (a `URI` or `CITATION`, a
/// `REMARK` or `AXIS` after it), as PROJ 9.9.0 reads it.
///
/// Of several root identifiers, the one PROJ 9.9.0 identifies an EPSG code by (`to_epsg()`)
/// counts: the first, passing over ESRI ones and EPSG ones whose code is no CRS of the root's kind
/// (`ID["ESRI",104000],ID["EPSG",4326]` and `ID["EPSG",27700],ID["EPSG",4326]` declare EPSG:4326),
/// but not another code of the root's kind (`ID["EPSG",4277],ID["EPSG",4326]` declares EPSG:4277,
/// which is then not its content), and none when all are passed over
/// (`ID["ESRI",102100],ID["EPSG",27700]` on a geographic CRS). terrio's registry stands in for
/// PROJ's database; an identifier of another authority counts (`ID["IGNF","WGS84G"]` before
/// EPSG:4326 declares no EPSG code, as in PROJ, which has IGNF:WGS84G). A single identifier is read
/// whatever its code.
pub(crate) fn declared_epsg(wkt: &str) -> Option<u32> {
    declared_code(wkt, "EPSG")
}

/// The kind of CRS a root keyword is: the kind PROJ creates an identifier's code for
/// (`createGeodeticCRS` for a geographic or geocentric CRS, …).
#[derive(Clone, Copy, PartialEq)]
enum RootKind {
    Geodetic,
    Projected,
    Vertical,
    Compound,
    Other,
}

impl RootKind {
    fn of(key: &str) -> Self {
        if [keywords::GEOGRAPHIC_OR_GEODETIC_CRS, keywords::WKT1_GEOGRAPHIC, keywords::WKT1_GEOCENTRIC]
            .iter()
            .any(|keys| keywords::is(key, keys))
        {
            Self::Geodetic
        } else if keywords::is(key, keywords::PROJECTED_CRS) || keywords::is(key, keywords::WKT1_PROJECTED) {
            Self::Projected
        } else if keywords::is(key, keywords::VERTICAL_CRS) {
            Self::Vertical
        } else if keywords::is(key, keywords::COMPOUND_CRS) {
            Self::Compound
        } else {
            Self::Other
        }
    }
}

/// The authority of an `ID` or `AUTHORITY` node, quoted or not (PROJ 9.9.0 reads
/// `AUTHORITY[EPSG,4326]` as EPSG:4326 too), in capitals.
fn id_authority(id: &wkt1::Node) -> Option<String> {
    match id.args.first() {
        Some(wkt1::Arg::Str(a) | wkt1::Arg::Ident(a)) => Some(a.to_ascii_uppercase()),
        _ => None,
    }
}

/// Whether PROJ's identification for `wanted` (`EPSG`, `ESRI`) passes over a root identifier of a
/// CRS of kind `own`: an ESRI identifier when EPSG is wanted (an EPSG one when ESRI is), an
/// identifier whose code is not a number, and one whose code the registry has no CRS of the root's
/// kind for (none at all, or a projected CRS's code on a geographic CRS). An identifier of any
/// other authority (`IGNF`) is not passed over. A root of no kind the registry holds (an
/// engineering or bound CRS, [`RootKind::Other`]) takes a registered code of any kind: only a code
/// the registry does not have is passed over there.
fn id_passed_over(id: &wkt1::Node, wanted: &str, own: RootKind) -> bool {
    let theirs = id_authority(id);
    let registered: Vec<&str> = match (theirs.as_deref(), id.code()) {
        (Some(a @ ("EPSG" | "ESRI")), _) if a != wanted => return true,
        (Some("EPSG"), Some(c)) => {
            [epsg::get(c).map(|e| e.wkt.as_str()), epsg::vertical(c).map(|v| v.wkt.as_str()), epsg::compound(c).map(|v| v.wkt.as_str())]
                .into_iter()
                .flatten()
                .collect()
        }
        (Some("ESRI"), Some(c)) => epsg::esri(c).map(|e| e.wkt.as_str()).into_iter().collect(),
        // A code that is no number (`ID["EPSG","not-a-code"]`), which PROJ cannot create either.
        (Some("EPSG" | "ESRI"), None) => return true,
        _ => return false,
    };
    !registered.iter().any(|w| own == RootKind::Other || RootKind::of(keywords::root_key(w)) == own)
}

/// The code of an authority a WKT definition declares for itself, as [`declared_epsg`]. A single
/// root identifier is read whatever its code; of several, none when all are passed over.
fn declared_code(wkt: &str, authority: &str) -> Option<u32> {
    declared_id(&wkt1::parse(wkt)?, authority)?.code()
}

/// The root identifier of `authority` a WKT's root declares its code by, as [`declared_code`].
fn declared_id<'a>(root: &'a wkt1::Node, authority: &str) -> Option<&'a wkt1::Node> {
    let (own, wanted) = (RootKind::of(&root.key), authority.to_ascii_uppercase());
    let ids: Vec<&wkt1::Node> = root.ids().collect();
    let id = match ids.as_slice() {
        [id] => *id,
        rest => rest.iter().copied().find(|id| !id_passed_over(id, &wanted, own))?,
    };
    (id_authority(id)? == wanted).then_some(id)
}

/// Whether a WKT's content is the ESRI CRS it declares, as [`declared_fits`] decides it for an
/// EPSG code: the same conversion on the same ellipsoid and datum; content terrio cannot read only
/// when it names the ESRI CRS's own method (an unknown method declaring ESRI:54009 is not
/// Mollweide).
fn esri_fits(w: &str, code: u32) -> bool {
    // On its datum (a local datum declaring an ESRI CRS on WGS 84 is not that CRS).
    if !epsg::esri(code).is_some_and(|e| esri_same_datum(w, &e.wkt)) {
        return false;
    }
    let Some(own) = Def::wkt_conversion(w) else {
        let method = |w: &str| {
            let root = wkt1::parse(w)?;
            let node = root.child("PROJECTION").or_else(|| root.child("CONVERSION").and_then(|c| c.child("METHOD")))?.clone();
            Some((node.name().map(wkt1::norm)?, node.epsg()))
        };
        // The registry WKT's method, by its name or PROJ's spherical variant of it (PROJ writes
        // ESRI:102035's `Orthographic` as `Orthographic (Spherical)`), or by the EPSG method PROJ
        // names otherwise (ESRI:54002's `Equirectangular` as `Equidistant Cylindrical
        // (Spherical)`, EPSG 1029); not another method, nor a method of no known name.
        let (Some((m, id)), Some((r, _))) = (method(w), epsg::esri(code).and_then(|e| method(&e.wkt))) else {
            return false;
        };
        // On the registry's ellipsoid too (in metres in its Esri WKT 1): the definition terrio
        // builds is the registry's, not the WKT's.
        let a = epsg::esri(code).and_then(|e| semi_major_axis(&e.wkt));
        let same_ellipsoid = a.is_none_or(|a| semi_major_axis(w).is_some_and(|own| (own - a).abs() < 1e-3));
        return same_ellipsoid && (m == r || m == format!("{r}spherical") || (r == "equirectangular" && id == Some(1029)));
    };
    // The conversion, and the datum's ellipsoid by the registry WKT's (in metres in an Esri WKT 1),
    // not the definition's: a spherical method's `+R_A` sphere (ESRI:54003, Miller on WGS 84).
    let a = epsg::esri(code).and_then(|e| semi_major_axis(&e.wkt));
    Def::new(&SpatialRef { esri: Some(code), ..Default::default() })
        .is_ok_and(|r| own.same_conversion(&r) && a.is_none_or(|a| (own.ell.a - a).abs() < 1e-3))
}

/// The ESRI code of a WKT definition: the one it declares (`AUTHORITY["ESRI","54009"]`), or that
/// of the ESRI CRS of the same name (GDAL's ESRI PE strings carry none) whose definition converts
/// coordinates as the WKT does, on the same ellipsoid.
fn identify_esri(wkt: &str) -> Option<u32> {
    // A declared code only when the content is that CRS; else the content is identified by name.
    if let Some(code) = declared_code(wkt, "ESRI").filter(|c| esri_fits(wkt, *c)) {
        return Some(code);
    }
    let name = wkt_name(wkt)?;
    let candidates = epsg::esri_named(&name);
    if candidates.is_empty() {
        return None;
    }
    // The WKT's conversion, through wbprojection for projections terrio's WKT reader does not map;
    // a name alone does not identify a CRS.
    let own = Def::wkt_conversion(wkt).or_else(|| Def::new(&SpatialRef { wkt: Some(wkt.to_string()), ..Default::default() }).ok())?;
    let a = semi_major_axis(wkt);
    candidates.into_iter().find(|code| {
        let Some(e) = epsg::esri(*code) else { return false };
        let Ok(registered) = Def::from_proj(&e.proj) else { return false };
        if !a.is_none_or(|a| (a - registered.ell.a).abs() < 1e-3) {
            return false;
        }
        esri_same_datum(wkt, &e.wkt) && own.same_conversion(&registered)
    })
}

/// Whether a WKT is on an ESRI CRS's datum (its registry WKT's). On the same datum, by name: PROJ
/// 9.8.1 identifies no code for ESRI's Monte_Mario_Italy_1 on a datum renamed `D_Local`, and places
/// it by a ballpark, not by Monte Mario's transformations. By the EPSG datums the names are (ESRI's
/// `D_North_American_1983` and the registry's `North_American_Datum_1983` are NAD83); by name when
/// one is not known; either way when neither is (a datum without a transformation).
fn esri_same_datum(wkt: &str, registered: &str) -> bool {
    let datum = datum_name(wkt).map(|n| epsg::datum_key(&n));
    let (own_datums, own_others) = wkt_datums_and_others(wkt);
    let (their_datums, their_others) = wkt_datums_and_others(registered);
    match (own_datums.is_empty(), their_datums.is_empty()) {
        (false, false) => {
            own_datums.iter().any(|d| their_datums.contains(d) || their_others.contains(d))
                || their_datums.iter().any(|d| own_others.contains(d))
        }
        (true, true) => true,
        _ => datum.is_none() || datum_name(registered).map(|n| epsg::datum_key(&n)) == datum,
    }
}

/// The name of a WKT definition's geodetic datum (`DATUM`, `GEODETICDATUM`, `TRF` or a datum
/// ensemble, as [`wkt_datums_and_others`] finds it), its first one (a compound or bound CRS's
/// horizontal or source CRS).
fn datum_name(wkt: &str) -> Option<String> {
    // A datum node without a quoted name gives none, and the search goes on after it.
    fn find(n: &wkt1::Node) -> Option<String> {
        if n.is(keywords::GEODETIC_DATUM) {
            return n.name().map(str::to_string);
        }
        n.args.iter().find_map(|a| match a {
            wkt1::Arg::Node(c) => find(c),
            _ => None,
        })
    }
    find(&wkt1::parse(wkt)?)
}

/// The ESRI code of a reference such as `ESRI:54009` (whatever its case, as PROJ reads it),
/// `urn:ogc:def:crs:ESRI::54009` or `http://www.opengis.net/def/crs/ESRI/0/54009`, given in
/// upper case.
fn esri_reference(upper: &str) -> Option<u32> {
    let code = upper
        .strip_prefix("ESRI:")
        .or_else(|| upper.strip_prefix("URN:OGC:DEF:CRS:ESRI:").map(|r| r.rsplit(':').next().unwrap_or(r)))
        .or_else(|| upper.split_once("/DEF/CRS/ESRI/").map(|(_, r)| r.rsplit('/').next().unwrap_or(r)))?;
    code.trim().parse().ok()
}

/// Open only regular definition files, checking the descriptor too in case the path changed.
#[cfg(not(target_family = "wasm"))]
fn open_definition_file(path: &str) -> Option<std::fs::File> {
    if !std::fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    // A path swapped for a FIFO must not wait for a writer before its type can be checked.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NONBLOCK);
    let file = options.open(path).ok()?;
    file.metadata().ok()?.is_file().then_some(file)
}

fn looks_like_wkt(s: &str) -> bool {
    let head = keywords::root_key(s);
    [keywords::HORIZONTAL_CRS, keywords::COMPOUND_CRS, keywords::VERTICAL_CRS, keywords::ENGINEERING_CRS]
        .iter()
        .any(|keys| keywords::is(head, keys))
}

/// The name in a WKT definition's outermost node.
fn wkt_name(wkt: &str) -> Option<String> {
    let start = wkt.find('"')? + 1;
    let end = start + wkt[start..].find('"')?;
    let name = &wkt[start..end];
    // Generated definitions append the code: `WGS 84 / UTM zone 33N (EPSG:32633)`.
    let name = match name.rfind(" (EPSG:") {
        Some(i) if name.ends_with(')') => &name[..i],
        _ => name,
    };
    Some(name.to_string())
}

/// Options of a [`Transformer`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TransformOptions {
    /// The coordinate epoch of the source coordinates, as a decimal year (`2010.5`), for a
    /// dynamic source CRS (GDAL's `-s_coord_epoch`).
    pub source_coordinate_epoch: Option<f64>,
    /// The coordinate epoch of the target coordinates, for a dynamic target CRS (GDAL's
    /// `-t_coord_epoch`).
    pub target_coordinate_epoch: Option<f64>,
    /// Allow approximate datum operations. `None` uses GDAL's default (yes).
    pub allow_ballpark: Option<bool>,
    /// Require the best operation assuming all grids were available (GDAL's ONLY_BEST).
    pub only_best: bool,
    /// Take the heights of 2D CRSs (without heights of their own) as ellipsoidal heights, so
    /// that datum transformations change them, as GDAL does after `PromoteTo3D` on both CRSs.
    /// By default they are kept as they are, as GDAL keeps them between 2D CRSs.
    pub promote_to_3d: bool,
}

impl TransformOptions {
    /// Parse a dedicated coordinate-operation option list, rejecting unknown keys.
    pub fn from_strict_options<K: AsRef<str>, V: AsRef<str>>(items: &[(K, V)]) -> Result<Self> {
        for (key, _) in items {
            let key = key.as_ref();
            if !["ALLOW_BALLPARK", "ONLY_BEST", "PROMOTE_TO_3D"].iter().any(|k| key.eq_ignore_ascii_case(k)) {
                return Err(Error::InvalidArgument(format!("unknown coordinate operation option: {key}")));
            }
        }
        Self::from_options(items)
    }

    /// Parse GDAL transformer/coordinate-operation options. Unrelated keys are ignored.
    pub fn from_options<K: AsRef<str>, V: AsRef<str>>(items: &[(K, V)]) -> Result<Self> {
        let mut out = Self::default();
        for (key, value) in items {
            let (key, value) = (key.as_ref(), value.as_ref());
            let boolean = || match value.to_ascii_uppercase().as_str() {
                "YES" | "TRUE" | "ON" | "1" => Ok(true),
                "NO" | "FALSE" | "OFF" | "0" => Ok(false),
                _ => Err(Error::InvalidArgument(format!("{key} must be YES or NO"))),
            };
            match key.to_ascii_uppercase().as_str() {
                "ALLOW_BALLPARK" => out.allow_ballpark = Some(boolean()?),
                "ONLY_BEST" => out.only_best = boolean()?,
                "PROMOTE_TO_3D" => out.promote_to_3d = boolean()?,
                _ => {}
            }
        }
        Ok(out)
    }
}

/// Transforms coordinates from one CRS to another, as GDAL does: linear and angular units, axis
/// order and prime meridians as the CRSs define them, and, when the datums differ, the datum
/// transformation GDAL (PROJ) picks for each point among those it lists for the pair, or chains
/// through an intermediate datum such as WGS 84. Datum shift grids are used when they can be
/// found, as GDAL uses them; heights of geographic 3D and compound CRSs go through geoid
/// models; time-dependent transformations use the coordinate epoch (see [`TransformOptions`]).
///
/// A transformer is `Send + Sync` and caches what it looks up on first use (routes, grids, geoid
/// models), so build it once per pair of CRSs and share it, between threads too, through an
/// [`Arc`](std::sync::Arc) rather than building it again. [`inverse`](Self::inverse) builds the
/// transformer of the other direction.
pub struct Transformer {
    /// The CRSs and options it was built from, for [`inverse`](Self::inverse).
    src_ref: SpatialRef,
    dst_ref: SpatialRef,
    src: Def,
    dst: Def,
    cartesian_src: Option<Def>,
    cartesian_dst: Option<Def>,
    identity: bool,
    route: route::Route,
    best_route: std::sync::OnceLock<route::Route>,
    bounds_reverse_route: std::sync::OnceLock<route::Route>,
    policy: TransformOptions,
    missing_grid_checked: std::sync::OnceLock<()>,
    /// The missing grids of the best operation, for the warning: those of the first point it
    /// was found missing for.
    missing_grids: std::sync::OnceLock<String>,
    /// The geoid models with grids of the source and target vertical CRSs, found on first use.
    src_geoid_grids: std::sync::OnceLock<Vec<std::sync::Arc<datum::Op>>>,
    dst_geoid_grids: std::sync::OnceLock<Vec<std::sync::Arc<datum::Op>>>,
    emitted_diagnostics: std::sync::atomic::AtomicU8,
    diagnostics: std::sync::atomic::AtomicU8,
    warning_handler: fn(String),
    /// The time of time-dependent transformations, as GDAL sets it from the coordinate epochs.
    time: Option<f64>,
    /// The height transformation, between CRSs that both have heights.
    heights: Option<Box<Heights>>,
    /// Web Mercator to WGS 84, which GDAL computes itself rather than with PROJ.
    web_mercator_to_wgs84: bool,
    /// The reverse of [`web_mercator_to_wgs84`](Self::web_mercator_to_wgs84).
    wgs84_to_web_mercator: bool,
    /// A point motion operation moving coordinates of one datum between two coordinate epochs
    /// (the Canada velocity grids of NAD83(CSRS), NKG_RF17vel), as PROJ applies it.
    motion: Option<Vec<datum::Step>>,
    /// The transformations PROJ lists between the vertical CRSs of compound CRSs (offsets,
    /// VERTCON grids...), used instead of going through ellipsoidal heights when there are some,
    /// each with the horizontal route from the source to the geographic CRS it interpolates in,
    /// when that is on another datum.
    vertical_ops: Vec<(std::sync::Arc<datum::Op>, Option<route::Route>)>,
    /// The source's and target's vertical CRSs when `vertical_ops` are listed between them, and
    /// the transformations PROJ lists between them when every grid can be found (for
    /// [`Transformer::best_operation_grids_at`]), built on first use.
    vertical_codes: Option<(u32, u32)>,
    all_vertical_ops: std::sync::OnceLock<Vec<(std::sync::Arc<datum::Op>, Option<route::Route>)>>,
    /// The source's horizontal CRS, which the routes of `vertical_ops` start from.
    src_horizontal: Def,
    /// Whether the vertical CRSs share their datum (heights only change unit).
    same_vertical_datum: bool,
}

impl Drop for Transformer {
    fn drop(&mut self) {
        self.flush_warnings();
    }
}

/// A side of a height transformation: the CRS and the geographic 3D CRSs of its geoid models.
struct Side {
    /// The CRS itself, then the geographic CRSs of its geoid models.
    defs: Vec<Def>,
    /// The geoid models (from ellipsoidal heights to the CRS's heights), by index in `defs`.
    models: Vec<(usize, Vec<std::sync::Arc<datum::Op>>)>,
    /// Horizontal routes between the CRS and each of `defs` (from the CRS for the source side,
    /// to it for the target side).
    routes: Vec<Option<route::Route>>,
}

/// How heights are transformed: through ellipsoidal heights, from the source CRS (or one of its
/// geoid models' CRSs) to the target CRS (or one of its geoid models').
struct Heights {
    src: Side,
    dst: Side,
    /// Routes between the ellipsoidal CRSs of both sides, built when first used.
    middle: std::sync::Mutex<std::collections::HashMap<(usize, usize), std::sync::Arc<route::Route>>>,
}

impl Side {
    fn new(def: &Def, source: bool) -> Self {
        let mut s = Side { defs: vec![def.clone()], models: Vec::new(), routes: vec![None] };
        let Some(v) = def.vertical() else { return s };
        let horizontal = def.horizontal();
        // Models on the CRS's own datum first, as PROJ prefers them.
        let mut models: Vec<&(Option<u32>, Vec<std::sync::Arc<datum::Op>>)> = v.models.iter().collect();
        let own = |g: &Option<u32>| match g {
            None => true,
            Some(g) => epsg::get(*g).is_some_and(|e| Some(e.geog) == def.geog() || (e.datum != 0 && Some(e.datum) == def.datum_code())),
        };
        models.sort_by_key(|(g, _)| !own(g));
        for (g, ops) in models {
            if own(g) {
                s.models.push((0, ops.clone()));
                continue;
            }
            let Some(gd) = g.and_then(def_of) else { continue };
            let r = if source { route::Route::new(&horizontal, &gd) } else { route::Route::new(&gd, &horizontal) };
            s.defs.push(gd);
            s.routes.push(Some(r));
            s.models.push((s.defs.len() - 1, ops.clone()));
        }
        s
    }
}

/// Whether GDAL transforms from `src` to `dst` with its own Web Mercator to WGS 84 formula rather
/// than with PROJ (`OGRProjCT::DetectWebMercatorToWGS84()`): EPSG:3857 (or the deprecated 3785 and
/// 900913, or ESRI's 102100) to EPSG:4326. Not from WGS 84 to Web Mercator, which PROJ computes.
fn web_mercator_to_wgs84(src: &SpatialRef, dst: &SpatialRef) -> bool {
    let mercator = matches!(src.epsg, Some(3857 | 3785 | 900913)) || src.esri == Some(102100);
    mercator && dst.epsg == Some(4326) && dst.esri.is_none()
}

/// GDAL's Web Mercator to WGS 84 (`OGRProjCT::TransformWithErrorCodes()`): longitude and latitude
/// in radians on the sphere of radius 6378137, which differ from PROJ's in the last bits.
fn mercator_sphere_inverse(x: f64, y: f64) -> Option<(f64, f64)> {
    use std::f64::consts::PI;
    const REVERSE_SPHERE_RADIUS: f64 = 1.0 / 6378137.0;
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    let lon = wrap_longitude(x * REVERSE_SPHERE_RADIUS);
    Some((lon, PI / 2.0 - 2.0 * (-y * REVERSE_SPHERE_RADIUS).exp().atan()))
}

/// A longitude in radians brought within [-π, π] as GDAL's Web Mercator inverse brings it: within
/// 10⁻¹⁴ of ±π to ±π, else by whole revolutions ([`unwind_longitude`]).
fn wrap_longitude(lon: f64) -> f64 {
    use std::f64::consts::PI;
    if lon > PI {
        if lon < PI + 1e-14 { PI } else { unwind_longitude(lon) }
    } else if lon < -PI {
        // GDAL adds 2π while the longitude is below -π, which mirrors subtracting it from the
        // opposite longitude, bit for bit, but for a result of exactly 0, which an addition makes
        // +0 and the mirror's negation -0: `0.0 -` keeps +0.
        if lon > -PI - 1e-14 { -PI } else { 0.0 - unwind_longitude(-lon) }
    } else {
        lon
    }
}

/// A longitude in radians above π brought back to (-π, π] as GDAL's `do { lon -= 2π } while (lon >
/// π)` brings it, bit for bit, in a few steps per power of two rather than one per revolution:
/// GDAL's loop takes 0.4 s for an easting of 10¹⁶ m (2.5·10⁸ revolutions), ten times longer for
/// ten times more, and never ends from 2⁵⁶ radians (4.6·10²³ m) on, where subtracting 2π no longer
/// changes a longitude. Those, which GDAL never returns, are reduced exactly modulo 2π.
fn unwind_longitude(mut lon: f64) -> f64 {
    use std::f64::consts::PI;
    const TWO_PI: f64 = 2.0 * PI;
    loop {
        let next = lon - TWO_PI;
        if next == lon {
            // `%` is exact: the remainder of the longitude itself.
            let r = lon % TWO_PI;
            return if r > PI { r - TWO_PI } else { r };
        }
        // Within the binade [2^e, 2^(e+1)) of a longitude, while it stays above 2^e + 2π, each
        // step subtracts the same amount, `lon - next` (2π rounded to the binade's spacing, with no
        // tie from 128 radians on, where 2π is not halfway between two multiples of the spacing):
        // all but the last steps in the binade at once, exactly (the result is a multiple of the
        // spacing in the binade, `k` being at most one more than the steps that keep it 2π above
        // 2^e). Below 128 radians GDAL's loop takes at most 20 steps.
        if lon >= 128.0 {
            let low = f64::from_bits(lon.to_bits() & (0x7ff << 52));
            let k = ((lon - low - 2.0 * TWO_PI) / (lon - next)).floor();
            if lon - low > 4.0 * TWO_PI && k >= 1.0 {
                lon -= k * (lon - next);
                continue;
            }
        }
        lon = next;
        if lon <= PI {
            return lon;
        }
    }
}

/// The point motion operation of a datum over a time span (years), from
/// `crates/terrio-crs/data/point_motion.tsv` (`scripts/gen_point_motion.py`).
fn point_motion(datum: u32, dt: f64) -> Option<Vec<datum::Step>> {
    static TABLE: &str = include_str!("../data/point_motion.tsv");
    let line =
        TABLE.lines().filter(|l| !l.starts_with('#')).find(|l| l.split('\t').next().and_then(|c| c.parse::<u32>().ok()) == Some(datum))?;
    let steps = line.split('\t').nth(2)?.replace("{dt}", &format!("{dt:?}"));
    datum::parse_steps(&steps)
}

/// The message for a best operation that needs missing grids, naming them (`names`, joined).
fn missing_grid_message(names: &str) -> String {
    let named = if names.is_empty() { String::new() } else { format!(" ({names})") };
    format!("best coordinate operation needs a missing grid{named}; set TERRIO_NETWORK=ON or TERRIO_GRID_DIR")
}

/// Vertical transformations, each with the horizontal route from the source's horizontal CRS
/// to the geographic CRS it interpolates in, when that is on another datum.
/// With `all`, routes as if every grid were available (for the grids a point needs).
fn with_vertical_routes(ops: Vec<epsg::VerticalOp>, horizontal: &Def, all: bool) -> Vec<(std::sync::Arc<datum::Op>, Option<route::Route>)> {
    ops.into_iter()
        .map(|v| {
            // On the geographic 2D CRS of a 3D one (ETRS89 for EPSG:4937).
            let route = v
                .interpolation
                .map(|g| epsg::get(g).map_or(g, |e| e.geog))
                .and_then(def_of)
                .filter(|g| g.datum_code().is_none() || g.datum_code() != horizontal.datum_code())
                .map(|g| if all { route::Route::all(horizontal, &g) } else { route::Route::new(horizontal, &g) });
            (v.op, route)
        })
        .collect()
}

/// The definition of an EPSG geographic 3D CRS.
fn def_of(code: u32) -> Option<Def> {
    Def::new(&SpatialRef { epsg: Some(code), ..Default::default() }).ok()
}

impl Heights {
    fn middle(&self, i: usize, j: usize) -> std::sync::Arc<route::Route> {
        let mut m = self.middle.lock().unwrap_or_else(|e| e.into_inner());
        m.entry((i, j)).or_insert_with(|| std::sync::Arc::new(route::Route::new(&self.src.defs[i], &self.dst.defs[j]))).clone()
    }
}

/// Picks and runs a geoid model for a point (degrees for the areas), `reverse` from heights to
/// ellipsoidal heights. With the point, whether the operation that ran was a geoid model, rather
/// than the operation without steps used when the model's grid is missing (which keeps heights
/// as they are).
fn run_model(ops: &[std::sync::Arc<datum::Op>], p: datum::Point, reverse: bool) -> Option<(datum::Point, bool)> {
    let (lon, lat) = (p.lon.to_degrees(), p.lat.to_degrees());
    let applied = std::cell::Cell::new(false);
    datum::transform_with(
        ops,
        |op, _| op.contains(lon, lat),
        |i| {
            datum::run_op(&ops[i], p, reverse, true).inspect(|_| {
                applied.set(!ops[i].steps.is_empty());
                route::note(Some(&ops[i]), reverse, route::OpKind::Vertical);
            })
        },
    )
    .map(|q| (q, applied.get()))
}

/// A coordinate operation a [`Transformer`] uses for a point: see [`Transformer::operation_at`].
#[derive(Debug, Clone, PartialEq)]
pub struct CoordinateOperation {
    /// The names of the transformations, in the order they run, joined by ` + ` as PROJ joins
    /// them (`Inverse of OSGB36 to WGS 84 (6)`). Conversions (projections, units, axes) are not
    /// named. PROJ's ballpark transformations are `Ballpark geographic offset` and
    /// `Ballpark vertical transformation`.
    pub name: String,
    /// The accuracy in metres: the sum of the transformations' accuracies, as PROJ adds them;
    /// `None` when one of them has none (a ballpark, a `+towgs84` definition).
    pub accuracy: Option<f64>,
    /// The grids the transformations interpolate in.
    pub grids: Vec<String>,
    /// Whether one of them is a ballpark: no datum shift, or heights kept as they are.
    pub ballpark: bool,
}

impl CoordinateOperation {
    fn from_used(used: Vec<route::Used>) -> Option<Self> {
        if used.is_empty() {
            return None;
        }
        let name = used.iter().map(|u| u.name.as_str()).collect::<Vec<_>>().join(" + ");
        let accuracy = used.iter().try_fold(0.0, |sum, u| (u.accuracy >= 0.0).then_some(sum + u.accuracy));
        let mut grids: Vec<String> = Vec::new();
        for g in used.iter().flat_map(|u| u.grids.iter()) {
            if !grids.contains(g) {
                grids.push(g.clone());
            }
        }
        Some(Self { name, accuracy, grids, ballpark: used.iter().any(|u| u.ballpark) })
    }
}

impl Transformer {
    /// This transformer, converting degrees as GDAL 3.13.3's reverse transformation does. GDAL
    /// inverts the forward pipeline (`OGRProjCT::GetInverse()`), so PROJ undoes its `unitconvert`
    /// by dividing by the factor, which rounds about a tenth of the degrees differently from
    /// multiplying by the reciprocal. GDAL's warper maps output pixels to the source this way.
    /// Web Mercator and WGS 84 are unchanged both ways: GDAL has its own formula from Web Mercator,
    /// and no PROJ pipeline to invert in the other direction, so it builds a direct one.
    /// It applies to every transformation of this transformer, 3D ones too. GDAL parity was
    /// measured for the 2D [`transform`](Self::transform) that the warper calls.
    pub fn with_reverse_degree_division(mut self) -> Self {
        if !self.web_mercator_to_wgs84 && !self.wgs84_to_web_mercator {
            self.src.inverted_degrees = true;
            self.dst.inverted_degrees = true;
        }
        self
    }

    /// A transformer from `src` to `dst`.
    pub fn new(src: &SpatialRef, dst: &SpatialRef) -> Result<Self> {
        Self::with_options(src, dst, &TransformOptions::default())
    }

    /// A transformer from `src` to `dst` with options, such as the coordinate epochs of
    /// time-dependent transformations.
    ///
    /// As GDAL does, the source epoch is used when the source CRS is dynamic and the target
    /// static, the target epoch when the target is dynamic and the source static, and the source
    /// epoch when both are dynamic and both epochs are given.
    pub fn with_options(src: &SpatialRef, dst: &SpatialRef, opts: &TransformOptions) -> Result<Self> {
        let (mut s, mut d) = (Def::new(src)?, Def::new(dst)?);
        s.compatible_body(&d)?;
        if opts.promote_to_3d {
            s.promote_to_3d();
            d.promote_to_3d();
        }
        let cartesian_src = s.is_geocentric().then(|| s.clone());
        let cartesian_dst = d.is_geocentric().then(|| d.clone());
        if cartesian_src.is_some() {
            s = s.geographic_endpoint();
        }
        if cartesian_dst.is_some() {
            d = d.geographic_endpoint();
        }
        let time = match (s.is_dynamic(), d.is_dynamic(), opts.source_coordinate_epoch, opts.target_coordinate_epoch) {
            (true, false, Some(e), _) => Some(e),
            (false, true, _, Some(e)) => Some(e),
            (true, true, Some(e), Some(_)) => Some(e),
            _ => None,
        };
        let mut route = route::Route::new(&s, &d);
        if opts.allow_ballpark == Some(false) && !route.disallow_ballpark() {
            return Err(Error::NoOperation("cannot find coordinate operations with ALLOW_BALLPARK=NO".into()));
        }
        let motion = match (s.datum_code(), opts.source_coordinate_epoch, opts.target_coordinate_epoch) {
            // Between geographic CRSs of the datum, as PROJ applies it (not to projected ones).
            (Some(datum), Some(from), Some(to))
                if s.datum_code() == d.datum_code() && from != to && s.is_geographic() && d.is_geographic() =>
            {
                point_motion(datum, to - from)
            }
            _ => None,
        };
        let heights = ((s.has_height() && d.has_height()) || cartesian_src.is_some() || cartesian_dst.is_some()).then(|| {
            Box::new(Heights {
                src: Side::new(&s, true),
                dst: Side::new(&d, false),
                middle: std::sync::Mutex::new(std::collections::HashMap::new()),
            })
        });
        let codes = (s.vertical().and_then(|v| v.code), d.vertical().and_then(|v| v.code));
        let (vertical_ops, same_vertical_datum, vertical_codes) = match codes {
            (Some(a), Some(b)) if a != b => {
                let datum = |c: u32| epsg::vertical(c).map(|v| v.datum).filter(|d| *d != 0);
                let same = datum(a).is_some() && datum(a) == datum(b);
                let ops = if same { Vec::new() } else { epsg::vertical_transformations(a, b) };
                (with_vertical_routes(ops, &s.horizontal(), false), same, (!same).then_some((a, b)))
            }
            _ => (Vec::new(), false, None),
        };
        let src_horizontal = s.horizontal();
        let identity = src.same_as(dst) && motion.is_none();
        let wgs84_to_web_mercator = web_mercator_to_wgs84(dst, src);
        let web_mercator_to_wgs84 = web_mercator_to_wgs84(src, dst);
        Ok(Self {
            identity,
            src_ref: src.clone(),
            dst_ref: dst.clone(),
            src: s,
            dst: d,
            cartesian_src,
            cartesian_dst,
            route,
            time,
            heights,
            motion,
            vertical_ops,
            same_vertical_datum,
            vertical_codes,
            all_vertical_ops: std::sync::OnceLock::new(),
            src_horizontal,
            web_mercator_to_wgs84,
            wgs84_to_web_mercator,
            best_route: std::sync::OnceLock::new(),
            bounds_reverse_route: std::sync::OnceLock::new(),
            policy: opts.clone(),
            missing_grid_checked: std::sync::OnceLock::new(),
            missing_grids: std::sync::OnceLock::new(),
            src_geoid_grids: std::sync::OnceLock::new(),
            dst_geoid_grids: std::sync::OnceLock::new(),
            emitted_diagnostics: std::sync::atomic::AtomicU8::new(0),
            diagnostics: std::sync::atomic::AtomicU8::new(0),
            warning_handler: warnings::warn,
        })
    }

    /// Route this transformer's diagnostics to a caller-provided warning sink.
    /// The sink runs on the thread calling `flush_warnings` (or dropping the transformer). A
    /// transformer kept in a `thread_local!` is dropped while the thread's locals are destroyed,
    /// so a sink that records into a thread-local should use `LocalKey::try_with`, and must not
    /// panic.
    pub fn with_warning_handler(mut self, handler: fn(String)) -> Self {
        self.warning_handler = handler;
        self
    }

    /// Transforms one point. With a geocentric endpoint, input Z defaults to zero
    /// and output Z is discarded; use `transform_3d` to retain all three ordinates.
    pub fn transform(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        if self.cartesian_src.is_some() || self.cartesian_dst.is_some() {
            let (x, y, _) = self.transform_at(x, y, 0.0, self.time)?;
            return Ok((x, y));
        }
        if self.identity {
            return Ok((x, y));
        }
        let (x, y, _) = self.transform_2d(x, y, 0.0, self.time)?;
        Ok((x, y))
    }

    /// Transforms points in place, each as [`transform`](Self::transform) does. Stops at the
    /// first point that fails and returns its error: the points before it are transformed, it and
    /// those after it are left as they were. Where failures are expected, as at the edges of a
    /// projection's domain, [`transform_slice_or_nan`](Self::transform_slice_or_nan) goes on.
    ///
    /// The datum operation is still chosen for each point (by its area of use), as `transform`
    /// chooses it, so the results are those of `transform`, bit for bit. Where `transform` returns
    /// every point as it is (identical CRSs, neither geocentric, without a point motion between
    /// coordinate epochs), the points are left as they are without visiting them.
    ///
    /// ```no_run
    /// use terrio_crs::{SpatialRef, Transformer};
    ///
    /// let utm = SpatialRef::from_epsg(32632)?;
    /// let t = Transformer::new(&SpatialRef::from_epsg(4326)?, &utm)?;
    /// let mut points = vec![(9.0, 47.0), (7.5, 45.9)];
    /// t.transform_slice(&mut points)?;
    /// # Ok::<(), terrio_crs::Error>(())
    /// ```
    pub fn transform_slice(&self, xy: &mut [(f64, f64)]) -> Result<()> {
        if self.is_identity_2d() {
            return Ok(());
        }
        for p in xy.iter_mut() {
            *p = self.transform(p.0, p.1)?;
        }
        Ok(())
    }

    /// Transforms points in place, each as [`transform`](Self::transform) does, setting the
    /// points it fails for to `(NaN, NaN)` and going on, and returns how many there were. For
    /// renderers, which draw what transforms and skip the rest, such as a grid of pixels reaching
    /// beyond a projection's domain.
    ///
    /// As PROJ 9.8.1's `proj_trans_generic` does, every point that fails is marked, whatever the
    /// reason: outside a projection's domain or a velocity grid ([`Error::Crs`]), and refused under
    /// the strict options, such as a missing grid with [`TransformOptions::only_best`]
    /// ([`Error::MissingGrid`]) or no allowed operation with
    /// [`TransformOptions::allow_ballpark`] off ([`Error::NoOperation`]). PROJ marks them
    /// `HUGE_VAL` (infinity); NaN is terrio's choice, so that a marked point is never taken for a
    /// coordinate. To learn whether a missing grid refused points under `only_best`, call
    /// [`check_policy`](Self::check_policy) after the batch, as raster workers do; it is not called
    /// for you. Only errors that are not about a point (I/O, an invalid argument, and any other
    /// kind) are returned, at once: as with
    /// [`transform_slice`](Self::transform_slice), the points before it are then transformed (or
    /// marked), and it and those after it are left as they were.
    ///
    /// ```no_run
    /// use terrio_crs::{SpatialRef, Transformer};
    ///
    /// let ortho = SpatialRef::from_proj4("+proj=ortho +lat_0=0 +lon_0=0 +datum=WGS84")?;
    /// let t = Transformer::new(&SpatialRef::from_epsg(4326)?, &ortho)?;
    /// let mut points = vec![(10.0, 10.0), (170.0, 0.0)];
    /// assert_eq!(t.transform_slice_or_nan(&mut points)?, 1); // 170°E is on the far side
    /// assert!(points[1].0.is_nan());
    /// # Ok::<(), terrio_crs::Error>(())
    /// ```
    pub fn transform_slice_or_nan(&self, xy: &mut [(f64, f64)]) -> Result<usize> {
        if self.is_identity_2d() {
            return Ok(0);
        }
        let mut failed = 0;
        for p in xy.iter_mut() {
            *p = match self.transform(p.0, p.1) {
                Ok(q) => q,
                Err(Error::Crs(_) | Error::MissingGrid(_) | Error::NoOperation(_)) => {
                    failed += 1;
                    (f64::NAN, f64::NAN)
                }
                Err(e) => return Err(e),
            };
        }
        Ok(failed)
    }

    /// Whether [`transform`](Self::transform) returns every point as it is.
    fn is_identity_2d(&self) -> bool {
        self.identity && self.cartesian_src.is_none() && self.cartesian_dst.is_none()
    }

    /// The transformer of the other direction, from this one's target CRS to its source CRS,
    /// with the same options, the coordinate epochs swapped, and the same warning handler.
    ///
    /// It is built as [`with_options`](Self::with_options) builds one, so it picks its datum
    /// operations for the other direction as `with_options` picks them for that pair (the tests
    /// compare both directions with PROJ 9.8.1). Between two dynamic CRSs with both coordinate
    /// epochs given, each direction runs at its own source epoch, by `with_options`'s rule, so a
    /// time-dependent transformation is not evaluated at the same time both ways and a round trip
    /// does not come back to the point. This is not GDAL's `OGRProjCT::GetInverse()`, which
    /// inverts the forward pipeline; its rounding of degrees is what
    /// [`with_reverse_degree_division`](Self::with_reverse_degree_division) reproduces, and the
    /// inverse keeps that rounding when this transformer has it.
    ///
    /// ```no_run
    /// use terrio_crs::{SpatialRef, Transformer};
    ///
    /// let lv95 = SpatialRef::from_epsg(2056)?;
    /// let to_wgs84 = Transformer::new(&lv95, &SpatialRef::from_epsg(4326)?)?;
    /// let back = to_wgs84.inverse()?;
    /// let (lon, lat) = to_wgs84.transform(2_600_000.0, 1_200_000.0)?;
    /// let (e, n) = back.transform(lon, lat)?;
    /// # Ok::<(), terrio_crs::Error>(())
    /// ```
    pub fn inverse(&self) -> Result<Transformer> {
        let opts = TransformOptions {
            source_coordinate_epoch: self.policy.target_coordinate_epoch,
            target_coordinate_epoch: self.policy.source_coordinate_epoch,
            ..self.policy.clone()
        };
        let mut t = Self::with_options(&self.dst_ref, &self.src_ref, &opts)?.with_warning_handler(self.warning_handler);
        if self.src.inverted_degrees {
            t = t.with_reverse_degree_division();
        }
        Ok(t)
    }

    fn best_route(&self) -> &route::Route {
        self.best_route.get_or_init(|| {
            let mut route = route::Route::all(&self.src, &self.dst);
            if self.policy.allow_ballpark == Some(false) {
                // The primary route was already validated by with_options; retain the
                // same restriction when considering unavailable operations.
                route.disallow_ballpark();
            }
            route
        })
    }

    /// Emit recorded diagnostics on the caller's thread. Repeated calls emit only new warnings.
    /// Operations should call this before draining their warning sink; Drop is a fallback.
    pub fn flush_warnings(&self) {
        let flags = self.diagnostics.load(std::sync::atomic::Ordering::Relaxed);
        let previous = self.emitted_diagnostics.fetch_or(flags, std::sync::atomic::Ordering::Relaxed);
        let flags = flags & !previous;
        let missing =
            if flags & 2 != 0 { missing_grid_message(self.missing_grids.get().map_or("", String::as_str)) } else { String::new() };
        for (bit, message) in [
            (1, "ballpark coordinate operation used between different ellipsoids; datum shift is unknown"),
            (2, missing.as_str()),
            (4, "coordinate operation used outside its area of use"),
            (
                8,
                "geoid model did not apply (its grid is missing, unreadable, or has no value at the point), so heights were not converted by it; set TERRIO_NETWORK=ON or TERRIO_GRID_DIR",
            ),
        ] {
            if flags & bit != 0 {
                (self.warning_handler)(message.into());
            }
        }
    }

    /// Return any strict coordinate-operation failure observed by this transformer, including
    /// those encountered by raster workers which represent failed points as invalid pixels.
    pub fn check_policy(&self) -> Result<()> {
        self.flush_warnings();
        let flags = self.diagnostics.load(std::sync::atomic::Ordering::Relaxed);
        if self.policy.only_best && flags & 2 != 0 {
            return Err(Error::MissingGrid(format!(
                "ONLY_BEST=YES: {}",
                missing_grid_message(self.missing_grids.get().map_or("", String::as_str))
            )));
        }
        Ok(())
    }

    /// Records that the best operation for a point needs a missing grid, for the warning (which
    /// names those of the first such point), and gives the missing grids of this one.
    fn record_missing_grid(&self, op: &datum::Op) -> String {
        let names: Vec<String> = op
            .grids()
            .into_iter()
            .filter(|(n, optional)| !optional && !grid::available(n))
            .map(|(n, _)| grid::diagnostic_grid_name(&grid::grid_use(&n, false).name))
            .collect();
        let names = names.join(", ");
        self.missing_grids.get_or_init(|| names.clone());
        self.diagnostics.fetch_or(2, std::sync::atomic::Ordering::Relaxed);
        names
    }

    fn record_operation(&self, op: Option<&datum::Op>, outside: bool) {
        let ballpark = op.is_none_or(|op| op.ballpark) && (self.src.ell.a != self.dst.ell.a || self.src.ell.es != self.dst.ell.es);
        let flags = u8::from(ballpark) | if outside { 4 } else { 0 };
        if flags != 0 && self.diagnostics.load(std::sync::atomic::Ordering::Relaxed) & flags != flags {
            self.diagnostics.fetch_or(flags, std::sync::atomic::Ordering::Relaxed);
        }
    }

    fn note_missing_grid(&self, x: f64, y: f64, p: datum::Point) {
        if self.route.same_datum() {
            return;
        }
        // Advisory availability diagnostics use the first point; strict selection checks
        // every point. Ordinary transforms select their actual operation only once.
        self.missing_grid_checked.get_or_init(|| {
            let (ideal, _) = self.best_route().selected(&self.src, x, y, p);
            if let Some(op) = ideal.filter(|op| !op.available()) {
                self.record_missing_grid(op);
            }
        });
    }

    fn check_point_policy(&self, x: f64, y: f64, p: datum::Point) -> Result<Option<&datum::Op>> {
        if self.route.same_datum() {
            return Ok(None);
        }
        let selected = if self.policy.only_best {
            let (ideal, outside) = self.best_route().selected(&self.src, x, y, p);
            self.record_operation(ideal, outside);
            if let Some(op) = ideal.filter(|op| !op.available()) {
                let names = self.record_missing_grid(op);
                return Err(Error::MissingGrid(format!("ONLY_BEST=YES: {}", missing_grid_message(&names))));
            }
            ideal
        } else {
            self.note_missing_grid(x, y, p);
            if self.policy.allow_ballpark == Some(false) { self.route.selected(&self.src, x, y, p).0 } else { None }
        };
        if self.policy.allow_ballpark == Some(false) && selected.is_none_or(|op| op.ballpark) {
            return Err(Error::NoOperation("ALLOW_BALLPARK=NO: no non-ballpark coordinate operation for this point".into()));
        }
        Ok(selected)
    }

    /// Refuses keeping the heights of a vertical CRS as they are (PROJ's ballpark vertical
    /// transformation) when no geoid model or vertical transformation applies to a point, as GDAL
    /// does: with `ALLOW_BALLPARK=NO`, and with `ONLY_BEST=YES` when there was one (`operations`)
    /// whose grid is missing or which does not cover the point.
    fn check_vertical_policy(&self, operations: bool) -> Result<()> {
        if self.policy.only_best && operations {
            return Err(Error::MissingGrid(
                "ONLY_BEST=YES: the best vertical operation does not apply (its grid is missing, or the point is outside it); set TERRIO_NETWORK=ON or TERRIO_GRID_DIR"
                    .into(),
            ));
        }
        if self.policy.allow_ballpark == Some(false) {
            return Err(Error::NoOperation("ALLOW_BALLPARK=NO: no geoid model or vertical transformation applies to this point".into()));
        }
        Ok(())
    }

    /// Records, for the warning, that a side's heights were not converted by a geoid model although
    /// one with a grid covers the point: its grid is missing, could not be read or downloaded
    /// (availability alone cannot tell, as a `.tif` counts as available with the network on), or
    /// has no value there. Callers have established that no geoid model applied. Under the strict
    /// options [`check_vertical_policy`](Self::check_vertical_policy) has already refused the point.
    fn note_missing_geoid(
        &self,
        models: &std::sync::OnceLock<Vec<std::sync::Arc<datum::Op>>>,
        vertical: Option<&std::sync::Arc<def::Vertical>>,
        p: datum::Point,
    ) {
        if self.diagnostics.load(std::sync::atomic::Ordering::Relaxed) & 8 != 0 {
            return; // already recorded: the warning is reported once
        }
        let Some(code) = vertical.and_then(|v| v.code) else { return };
        let (lon, lat) = (p.lon.to_degrees(), p.lat.to_degrees());
        if models.get_or_init(|| epsg::geoid_grid_models(code)).iter().any(|op| op.contains(lon, lat)) {
            self.diagnostics.fetch_or(8, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The 2D transformation of a point, with its height kept (or, for transformations PROJ runs
    /// without keeping heights, transformed).
    fn transform_2d(&self, x: f64, y: f64, z: f64, t: Option<f64>) -> Result<(f64, f64, f64)> {
        let fail = || Error::Crs(format!("cannot transform ({x}, {y})"));
        if self.web_mercator_to_wgs84 {
            let (lon, lat) = mercator_sphere_inverse(x, y).ok_or_else(fail)?;
            // Degrees as GDAL gives them (radians times `180 / pi`), in the CRS's axis order.
            let (tx, ty) = self.dst.project(lon, lat).ok_or_else(fail)?;
            return Ok((tx, ty, z));
        }
        let (lon, lat) = self.src.unproject(x, y).ok_or_else(fail)?;
        let p = datum::Point { lon, lat, h: z, t };
        let selected = if self.policy.only_best {
            self.check_point_policy(x, y, p)?
        } else {
            self.note_missing_grid(x, y, p);
            None
        };
        let refused_ballpark = std::cell::Cell::new(false);

        let p = match &self.motion {
            Some(steps) => datum::run(steps, p, false, true).ok_or_else(|| {
                let names = steps.iter().flat_map(|s| s.grid_names()).map(|(g, _)| g).collect::<Vec<_>>().join(", ");
                // A velocity grid that is not there, or whose download failed (with the network
                // on, a grid counts as available until it is fetched), is a missing grid, not a
                // point outside it.
                if datum::grids_missing(steps) {
                    return Error::MissingGrid(format!(
                        "cannot move ({x}, {y}) between coordinate epochs: the velocity grid ({names}) is not available; set TERRIO_NETWORK=ON or TERRIO_GRID_DIR"
                    ));
                }
                Error::Crs(format!("cannot move ({x}, {y}) between coordinate epochs: outside the velocity grid ({names})"))
            })?,
            None => p,
        };
        let q = if self.policy.only_best {
            self.best_route().apply_selected(&self.src, x, y, p, false, selected)
        } else {
            self.route.apply_reporting(&self.src, x, y, p, false, &|op, outside| {
                if self.policy.allow_ballpark == Some(false) && op.is_none_or(|op| op.ballpark) {
                    refused_ballpark.set(true);
                } else {
                    self.record_operation(op, outside);
                }
            })
        }
        .ok_or_else(fail)?;
        if refused_ballpark.get() {
            return Err(Error::NoOperation("ALLOW_BALLPARK=NO: no non-ballpark coordinate operation for this point".into()));
        }
        let (tx, ty) = self.dst.project(q.lon, q.lat).ok_or_else(fail)?;
        Ok((tx, ty, q.h))
    }

    /// Transforms a point with a height. Between CRSs that both have heights (geographic 3D and
    /// compound CRSs), heights go through the datum transformation and geoid models as in GDAL;
    /// otherwise they are kept, as GDAL keeps them between 2D CRSs.
    pub fn transform_3d(&self, x: f64, y: f64, z: f64) -> Result<(f64, f64, f64)> {
        self.transform_at(x, y, z, self.time)
    }

    /// Transforms a point with a height at a time (a decimal year), for time-dependent
    /// transformations; the time replaces the coordinate epoch of the options.
    pub fn transform_4d(&self, x: f64, y: f64, z: f64, t: f64) -> Result<(f64, f64, f64)> {
        self.transform_at(x, y, z, Some(t))
    }

    /// The coordinate operation [`Transformer::transform_3d`] uses for a point: the
    /// transformations between datums and vertical CRSs it runs, as PROJ's
    /// `proj_trans_get_last_used_operation` gives them after transforming the point. `None` when
    /// the point needs none (CRSs of one datum, without a vertical change). Fails as
    /// `transform_3d` fails for the point, under the same options.
    ///
    /// The point is transformed to find out, so this costs a transformation; the result is not
    /// kept, and the transformer is not changed (it can be shared between threads).
    pub fn operation_at(&self, x: f64, y: f64, z: f64) -> Result<Option<CoordinateOperation>> {
        let (result, used) = route::recording(|| self.transform_at(x, y, z, self.time));
        result?;
        Ok(CoordinateOperation::from_used(used))
    }

    /// The grids the transformation of a point needs, with where to get them, missing ones
    /// included: those of the operation PROJ's `proj_trans` picks for the point with every grid
    /// present, as PROJ's `proj_coordoperation_get_grid_used` reports them, and of a point motion
    /// between coordinate epochs. Empty when that operation needs no grid.
    ///
    /// An application that downloads grids itself (with terrio's network off, in WebAssembly
    /// too) fetches the missing ones from their `url`, gives them to [`register_grid`] under
    /// their `name` (or saves them in a search directory and calls [`reset_grid_cache`], as
    /// whether a grid is found is cached), and builds the transformer again. The
    /// operation is the best one for the point whose grid is missing, or else the one that
    /// applies there: when the point is outside a grid once it is present, the next operation's
    /// grids come up, so ask again until every grid listed is available.
    ///
    /// Between compound CRSs, the grids of the transformation between their vertical CRSs
    /// (VERTCON...) are listed too. Geoid models are not listed (`+geoidgrids`, nor those of a
    /// geoid model's CRS, from a geographic 3D CRS too). Between CRSs with heights (geographic
    /// 3D, compound), the horizontal grids listed are those between their geographic CRSs, which
    /// the transformations through a geoid model's CRS may not use.
    ///
    /// To find the operation that applies, the point is transformed as by `transform`, so with
    /// the network on, the grids it reads are fetched (their blocks for the point, or whole with
    /// `TERRIO_GRID_DOWNLOAD=YES`) and cached as for a transformation.
    ///
    /// ```no_run
    /// use terrio_crs::{SpatialRef, Transformer, register_grid};
    /// # fn download(url: &str) -> Vec<u8> { unimplemented!() }
    /// let (src, dst) = (SpatialRef::parse("EPSG:4267")?, SpatialRef::parse("EPSG:4326")?);
    /// loop {
    ///     let t = Transformer::new(&src, &dst)?;
    ///     let missing: Vec<_> = t.best_operation_grids_at(-100.0, 40.0, 0.0)?.into_iter().filter(|g| !g.available).collect();
    ///     if missing.is_empty() {
    ///         break;
    ///     }
    ///     // us_noaa_conus.tif and us_noaa_kshpgn.tif, from https://cdn.proj.org/<name>.
    ///     for g in missing {
    ///         // A grid PROJ's database does not know has no URL: give up on it.
    ///         let url = g.url.as_deref().ok_or_else(|| terrio_crs::Error::MissingGrid(g.name.clone()))?;
    ///         register_grid(&g.name, download(url));
    ///     }
    /// }
    /// # Ok::<(), terrio_crs::Error>(())
    /// ```
    pub fn best_operation_grids_at(&self, x: f64, y: f64, z: f64) -> Result<Vec<GridUse>> {
        let fail = || Error::Crs(format!("cannot transform ({x}, {y}, {z})"));
        // A geocentric source: its geographic coordinates, as `transform_at` takes them.
        let (x, y, z) = match &self.cartesian_src {
            Some(src) => src.unproject_geocentric(x, y, z).ok_or_else(fail)?,
            None => (x, y, z),
        };
        let mut names: Vec<(String, bool)> = Vec::new();
        if let Some(steps) = &self.motion {
            names.extend(steps.iter().flat_map(|s| s.grid_names()));
        }
        if !self.web_mercator_to_wgs84 && !self.route.same_datum() {
            let (lon, lat) = self.src.unproject(x, y).ok_or_else(fail)?;
            let p = datum::Point { lon, lat, h: z, t: self.time };
            names.extend(self.best_route().grid_operations_at(&self.src, x, y, p).into_iter().flat_map(|op| op.grids()));
        }
        // A transformation between the vertical CRSs of compound CRSs (VERTCON...), as
        // `vertical_height` picks it when every grid can be found.
        if let (Some((a, b)), Some(v)) = (self.vertical_codes, self.src.vertical()) {
            let all = self
                .all_vertical_ops
                .get_or_init(|| with_vertical_routes(epsg::vertical_transformations_all(a, b), &self.src_horizontal, true));
            let (lon, lat) = self.src.unproject(x, y).ok_or_else(fail)?;
            let p = datum::Point { lon, lat, h: z * v.to_meter, t: self.time };
            // Stops at the first whose grid, or the grid of its route, is missing.
            // Or whose download failed, before or by its run: the point stops there, as for the
            // horizontal transformation (`Route::grid_operations_at`).
            let lacks = |op: &datum::Op| !op.available() || op.network_failed();
            let missing = |i: usize| {
                lacks(&all[i].0)
                    || all[i].1.as_ref().is_some_and(|r| r.grid_operations_at(&self.src_horizontal, x, y, p).into_iter().any(lacks))
            };
            if let Some((i, _)) = self.vertical_pick(all, x, y, p, missing) {
                names.extend(all[i].0.grids());
                // The horizontal transformation to the CRS it interpolates in (NZGD49 to NZGD2000
                // for the Auckland 1946 height grid), as PROJ chains it.
                if let Some(r) = &all[i].1 {
                    names.extend(r.grid_operations_at(&self.src_horizontal, x, y, p).into_iter().flat_map(|op| op.grids()));
                }
            }
        }
        let mut uses: Vec<GridUse> = Vec::new();
        for (name, optional) in names.into_iter().filter(|(n, _)| n != "null") {
            let used = grid::grid_use(&name, optional);
            match uses.iter_mut().find(|u| u.name == used.name) {
                // Required when any step requires it.
                Some(u) => u.optional &= used.optional,
                None => uses.push(used),
            }
        }
        Ok(uses)
    }

    fn transform_at(&self, x: f64, y: f64, z: f64, t: Option<f64>) -> Result<(f64, f64, f64)> {
        let fail = || Error::Crs(format!("cannot transform ({x}, {y}, {z})"));
        if self.cartesian_src.is_some() || self.cartesian_dst.is_some() {
            if !x.is_finite() || !y.is_finite() || !z.is_finite() {
                return Err(fail());
            }
            if self.identity {
                return Ok((x, y, z));
            }
            if self.cartesian_src.is_none()
                && self.src.is_geographic()
                && self.src.unproject(x, y).is_none_or(|(_, lat)| lat.abs() > std::f64::consts::FRAC_PI_2 + 1e-12)
            {
                return Err(fail());
            }
            let (x, y, z) = match &self.cartesian_src {
                Some(src) => src.unproject_geocentric(x, y, z).ok_or_else(fail)?,
                None => (x, y, z),
            };
            let (x, y, z) = self.transform_geographic_at(x, y, z, t)?;
            if !x.is_finite() || !y.is_finite() || !z.is_finite() {
                return Err(fail());
            }
            return match &self.cartesian_dst {
                Some(dst) => dst.project_geocentric(x, y, z).ok_or_else(fail),
                None => Ok((x, y, z)),
            };
        }
        self.transform_geographic_at(x, y, z, t)
    }

    fn transform_geographic_at(&self, x: f64, y: f64, z: f64, t: Option<f64>) -> Result<(f64, f64, f64)> {
        if self.identity {
            return Ok((x, y, z));
        }
        let Some(h) = &self.heights else { return self.transform_2d(x, y, z, t) };
        // A point motion between epochs moves ellipsoidal heights with the position.
        if self.motion.is_some() && self.src.vertical().is_none() && self.dst.vertical().is_none() {
            return self.transform_2d(x, y, z, t);
        }
        // Compound CRSs with the same vertical CRS, or vertical CRSs of one datum: heights only
        // change unit.
        if let (Some(a), Some(b)) = (self.src.vertical(), self.dst.vertical())
            && a.code.is_some()
            && (a.code == b.code || self.same_vertical_datum)
        {
            let (tx, ty, _) = self.transform_2d(x, y, 0.0, t)?;
            return Ok((tx, ty, b.height_of(a.to_metres(z))));
        }
        // A transformation between the vertical CRSs (as PROJ prefers it to geoid models).
        if let (Some(a), Some(b), false) = (self.src.vertical(), self.dst.vertical(), self.vertical_ops.is_empty()) {
            let (tx, ty, _) = self.transform_2d(x, y, 0.0, t)?;
            let (lon, lat) = self.src.unproject(x, y).ok_or_else(|| Error::Crs(format!("cannot transform ({x}, {y}, {z})")))?;
            // In metres, in the source's axis direction: the steps turn heights into depths.
            let p = datum::Point { lon, lat, h: z * a.to_meter, t };
            return Ok((
                tx,
                ty,
                match self.vertical_height(x, y, p) {
                    Some(h) => h / b.to_meter,
                    // None applies: heights are kept, as by PROJ's ballpark.
                    None => {
                        self.check_vertical_policy(true)?;
                        route::note(None, false, route::OpKind::Vertical);
                        b.height_of(a.to_metres(z))
                    }
                },
            ));
        }
        let fail = || Error::Crs(format!("cannot transform ({x}, {y}, {z})"));
        let (lon, lat) = self.src.unproject(x, y).ok_or_else(fail)?;
        let _ = self.check_point_policy(x, y, datum::Point { lon, lat, h: z, t })?;
        let p0 = datum::Point { lon, lat, h: self.src.vertical().map_or(z, |v| v.to_metres(z)), t };
        // To ellipsoidal heights, on the source CRS or a geoid model's CRS.
        let (mut start, mut geoid) = (None, false);
        for (i, ops) in &h.src.models {
            let q = match &h.src.routes[*i] {
                Some(r) => r.apply(&self.src, x, y, p0, false),
                None => Some(p0),
            };
            if let Some(q) = q
                && let Some((r, applied)) = run_model(ops, q, true)
            {
                start = Some((*i, r));
                geoid = applied;
                break;
            }
        }
        // No geoid model: heights are kept (PROJ's ballpark vertical transformation).
        if start.is_none() && self.src.vertical().is_some_and(|v| !v.ballpark) {
            return Err(fail());
        }
        if self.src.vertical().is_some() && !geoid {
            self.check_vertical_policy(!h.src.models.is_empty())?;
            self.note_missing_geoid(&self.src_geoid_grids, self.src.vertical(), p0);
        }
        if start.is_none() && self.src.vertical().is_some() {
            route::note(None, false, route::OpKind::Vertical);
        }
        // From a vertical CRS without a transformation, or whose geoid model's grid is missing,
        // heights are kept (PROJ's ballpark).
        let kept = !geoid && self.src.vertical().is_some();
        let (i, p) = start.unwrap_or((0, p0));
        let (sx, sy) = if i == 0 { (x, y) } else { (p.lon.to_degrees(), p.lat.to_degrees()) };
        // From ellipsoidal heights to the target.
        for (j, ops) in &h.dst.models {
            let Some(q) = h.middle(i, *j).apply(&h.src.defs[i], sx, sy, p, true) else { continue };
            let Some((r, applied)) = run_model(ops, q, false) else { continue };
            if !applied {
                self.check_vertical_policy(true)?;
                self.note_missing_geoid(&self.dst_geoid_grids, self.dst.vertical(), q);
            }
            let r = match &h.dst.routes[*j] {
                Some(route) => route.apply(&h.dst.defs[*j], r.lon.to_degrees(), r.lat.to_degrees(), r, false).ok_or_else(fail)?,
                None => r,
            };
            let (tx, ty) = self.dst.project(r.lon, r.lat).ok_or_else(fail)?;
            return Ok((tx, ty, self.dst.vertical().map_or(r.h, |v| v.height_of(r.h))));
        }
        if self.dst.vertical().is_some_and(|v| !v.ballpark) {
            return Err(fail());
        }
        if self.dst.vertical().is_some() {
            self.check_vertical_policy(!h.dst.models.is_empty())?;
            self.note_missing_geoid(&self.dst_geoid_grids, self.dst.vertical(), p);
            route::note(None, false, route::OpKind::Vertical);
        }
        let q = h.middle(i, 0).apply(&h.src.defs[i], sx, sy, p, true).ok_or_else(fail)?;
        let (tx, ty) = self.dst.project(q.lon, q.lat).ok_or_else(fail)?;
        // From or into a vertical CRS without a transformation: PROJ's ballpark vertical
        // transformation, which keeps the height, not the ellipsoidal height the horizontal datum
        // shift changes.
        let z = if kept || self.dst.vertical().is_some() { p.h } else { q.h };
        Ok((tx, ty, self.dst.vertical().map_or(z, |v| v.height_of(z))))
    }

    /// The height of a point (metres, in the source's axis direction) after the transformation
    /// between the vertical CRSs PROJ picks for it: the most accurate whose area holds the point,
    /// a transformation interpolating in another datum's geographic CRS counting the accuracy of
    /// the horizontal transformation to it both ways (as PROJ chains it); the next when it fails
    /// (outside its grid), up to three. `None` when none applies.
    fn vertical_height(&self, x: f64, y: f64, p: datum::Point) -> Option<f64> {
        match self.vertical_pick(&self.vertical_ops, x, y, p, |_| false)? {
            (i, Some(h)) => {
                route::note(Some(&self.vertical_ops[i].0), false, route::OpKind::Vertical);
                Some(h)
            }
            (_, None) => None,
        }
    }

    /// The transformation between the vertical CRSs [`Transformer::vertical_height`] picks for a
    /// point among `ops`, with the height it gives; or, the first `stop` holds for (by its index:
    /// one whose grid is missing), with none.
    fn vertical_pick(
        &self,
        ops: &[(std::sync::Arc<datum::Op>, Option<route::Route>)],
        x: f64,
        y: f64,
        p: datum::Point,
        stop: impl Fn(usize) -> bool,
    ) -> Option<(usize, Option<f64>)> {
        let horizontal = &self.src_horizontal;
        let (dlon, dlat) = (p.lon.to_degrees(), p.lat.to_degrees());
        let mut candidates: Vec<(usize, f64)> = ops
            .iter()
            .enumerate()
            .filter(|(_, (op, _))| op.contains(dlon, dlat))
            .map(|(i, (op, route))| {
                let accuracy = match route {
                    None => op.accuracy,
                    Some(r) => match r.accuracy_at(horizontal, x, y, p) {
                        h if h >= 0.0 && op.accuracy >= 0.0 => op.accuracy + 2.0 * h,
                        _ => -1.0,
                    },
                };
                (i, accuracy)
            })
            .collect();
        // PROJ sorts the chained transformations by their accuracies (known ones first), then picks
        // among them in that order (`pj_get_suggested_operation`), then the next best when one
        // fails.
        candidates.sort_by(|a, b| (a.1 < 0.0).cmp(&(b.1 < 0.0)).then(a.1.max(0.0).total_cmp(&b.1.max(0.0))));
        let mut excluded: Vec<usize> = Vec::new();
        for _ in 0..3 {
            let mut best: Option<(usize, f64)> = None;
            for &(i, accuracy) in candidates.iter().filter(|(i, _)| !excluded.contains(i)) {
                let op = &ops[i].0;
                if best.is_none_or(|(b, ba)| datum::better_by(op, accuracy, &ops[b].0, ba)) {
                    best = Some((i, accuracy));
                }
            }
            let (i, _) = best?;
            let (op, route) = &ops[i];
            if stop(i) {
                return Some((i, None));
            }
            // The point's position in the interpolation CRS; its height stays as it is.
            let q = match route {
                Some(r) => r.apply(horizontal, x, y, p, false).map(|q| datum::Point { h: p.h, ..q }),
                None => Some(p),
            };
            if let Some(h) = q.and_then(|q| datum::run_op(op, q, false, true)).map(|r| r.h) {
                return Some((i, Some(h)));
            }
            // A grid it failed for by this run (a failed download).
            if stop(i) {
                return Some((i, None));
            }
            excluded.push(i);
        }
        None
    }

    /// Transforms a bounding box `(min_x, min_y, max_x, max_y)`, densifying its edges so that
    /// curved edges in the target CRS are covered. Points that fail to transform are skipped.
    /// Uses 21 interior points per edge, as GDAL 3.12.1 does by default.
    pub fn transform_bounds(&self, b: (f64, f64, f64, f64)) -> Result<(f64, f64, f64, f64)> {
        self.transform_bounds_densified(b, 21)
    }

    /// Transforms bounds with `densify` interior points per edge. Projected boxes containing
    /// a pole include that pole and the full longitude range in a geographic target CRS.
    /// Geographic antimeridian crossings retain scalar longitude minima and maxima; wrapped
    /// bounds with `min_x > max_x` are not produced.
    /// Counts above 65,536 interior points per edge are refused to bound computation time.
    pub fn transform_bounds_densified(&self, b: (f64, f64, f64, f64), densify: usize) -> Result<(f64, f64, f64, f64)> {
        let steps = densify.checked_add(1).ok_or_else(|| Error::Crs("densify count is too large".into()))?;
        if densify > 65_536 {
            return Err(Error::Crs("densify count must not exceed 65536 interior points per edge".into()));
        }
        let (mut x0, mut y0, mut x1, mut y1) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for i in 0..=steps {
            let t = i as f64 / steps as f64;
            let x = b.0 + (b.2 - b.0) * t;
            let y = b.1 + (b.3 - b.1) * t;
            for (px, py) in [(x, b.1), (x, b.3), (b.0, y), (b.2, y)] {
                if let Ok((tx, ty)) = self.transform(px, py)
                    && tx.is_finite()
                    && ty.is_finite()
                {
                    x0 = x0.min(tx);
                    y0 = y0.min(ty);
                    x1 = x1.max(tx);
                    y1 = y1.max(ty);
                }
            }
        }
        if !x0.is_finite() {
            return Err(Error::Crs("no point of the bounds could be transformed".into()));
        }
        if self.src.is_projected() && self.dst.is_geographic() {
            // Synthetic probes must not record diagnostics or change operation reporting for
            // actual edge coordinates. In particular, UTM's projected pole is usually outside
            // the box and outside the datum operation's area of use.
            route::recording(|| {
                let reverse = self.bounds_reverse_route.get_or_init(|| route::Route::new(&self.dst, &self.src));
                for lat in [std::f64::consts::FRAC_PI_2, -std::f64::consts::FRAC_PI_2] {
                    let probe = || -> Option<()> {
                        let (px, py) = self.dst.project(0.0, lat)?;
                        let p = datum::Point { lon: 0.0, lat, h: 0.0, t: self.time };
                        let mut q = reverse.apply(&self.dst, px, py, p, false)?;
                        if let Some(steps) = &self.motion {
                            q = datum::run(steps, q, true, true)?;
                        }
                        let (sx, sy) = self.src.project(q.lon, q.lat)?;
                        if !(b.0 < sx && sx < b.2 && b.1 < sy && sy < b.3) {
                            return None;
                        }
                        let (lon, phi) = self.src.unproject(sx, sy)?;
                        let mut p = datum::Point { lon, lat: phi, h: 0.0, t: self.time };
                        if let Some(steps) = &self.motion {
                            p = datum::run(steps, p, false, true)?;
                        }
                        let q = self.bounds_probe_forward(sx, sy, p)?;
                        if !q.lat.is_finite() || (q.lat - lat).abs() > 1e-10 {
                            return None;
                        }
                        Some(())
                    };
                    if probe().is_some() {
                        for (x, y) in self.dst.pole_bounds(lat) {
                            x0 = x0.min(x);
                            y0 = y0.min(y);
                            x1 = x1.max(x);
                            y1 = y1.max(y);
                        }
                    }
                }
            });
        }
        Ok((x0, y0, x1, y1))
    }

    /// Apply the forward policy to a synthetic bounds probe without recording diagnostics.
    fn bounds_probe_forward(&self, x: f64, y: f64, p: datum::Point) -> Option<datum::Point> {
        if self.policy.only_best && !self.route.same_datum() {
            let route = self.best_route();
            let (selected, _) = route.selected(&self.src, x, y, p);
            if selected.is_some_and(|op| !op.available())
                || (self.policy.allow_ballpark == Some(false) && selected.is_none_or(|op| op.ballpark))
            {
                return None;
            }
            route.apply_selected(&self.src, x, y, p, false, selected)
        } else {
            let refused_ballpark = std::cell::Cell::new(false);
            let q = self.route.apply_reporting(&self.src, x, y, p, false, &|op, _| {
                if self.policy.allow_ballpark == Some(false) && op.is_none_or(|op| op.ballpark) {
                    refused_ballpark.set(true);
                }
            })?;
            (!refused_ballpark.get()).then_some(q)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Polar bounds from GDAL 3.12.1 include an interior pole and every longitude.
    #[test]
    fn transform_bounds_contains_interior_poles() {
        for (code, pole) in [(3413, 90.0), (3995, 90.0), (3031, -90.0), (3976, -90.0)] {
            let t = Transformer::new(&SpatialRef::from_epsg(code).unwrap(), &SpatialRef::from_epsg(4326).unwrap()).unwrap();
            let b = t.transform_bounds((-1e6, -1e6, 1e6, 1e6)).unwrap();
            assert_eq!((b.0, b.2), (-180.0, 180.0), "EPSG:{code}: {b:?}");
            assert_eq!(if pole > 0.0 { b.3 } else { b.1 }, pole, "EPSG:{code}: {b:?}");
        }
    }

    #[test]
    fn transform_bounds_probes_do_not_record_diagnostics() {
        let t = Transformer::new(&SpatialRef::from_epsg(25830).unwrap(), &SpatialRef::from_epsg(4326).unwrap()).unwrap();
        let b = t.transform_bounds((500_000.0, 4_000_000.0, 600_000.0, 4_100_000.0)).unwrap();
        assert!(b.0 > -4.0 && b.2 < -1.0 && b.1 > 35.0 && b.3 < 38.0, "{b:?}");
        assert_eq!(t.diagnostics.load(std::sync::atomic::Ordering::Relaxed), 0);
        // Real out-of-area coordinates must still record a warning.
        t.transform(500_000.0, -4_000_000.0).unwrap();
        assert_ne!(t.diagnostics.load(std::sync::atomic::Ordering::Relaxed) & 4, 0);
    }

    #[test]
    fn transform_bounds_probe_refuses_an_unavailable_best_operation() {
        let source = SpatialRef::parse("+proj=stere +lat_0=90 +datum=NAD27").unwrap();
        let target = SpatialRef::from_epsg(4326).unwrap();
        let mut t = Transformer::with_options(&source, &target, &TransformOptions { only_best: true, ..Default::default() }).unwrap();
        // A world-covering required grid whose file cannot exist, independent of installed
        // PROJ data. The ordinary NAD27 route remains available as a fallback.
        let directory = tempfile::tempdir().unwrap();
        let grid = directory.path().join("missing.gsb");
        let ideal = Def::from_proj(&format!("+proj=stere +lat_0=90 +ellps=clrk66 +nadgrids={}", grid.display())).unwrap();
        assert!(t.best_route.set(route::Route::all(&ideal, &t.dst)).is_ok());
        let p = datum::Point { lon: 0.0, lat: std::f64::consts::FRAC_PI_2, h: 0.0, t: None };
        assert!(t.bounds_probe_forward(0.0, 0.0, p).is_none());
        assert_eq!(t.diagnostics.load(std::sync::atomic::Ordering::Relaxed), 0);
        t.policy.only_best = false;
        assert!(t.bounds_probe_forward(0.0, 0.0, p).is_some());
        t.policy.only_best = true;
        assert!(matches!(t.transform(0.0, 0.0), Err(Error::MissingGrid(_))));
        assert_ne!(t.diagnostics.load(std::sync::atomic::Ordering::Relaxed) & 2, 0);
    }

    #[test]
    fn transform_bounds_uses_requested_densification() {
        let t = Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &SpatialRef::from_epsg(5070).unwrap()).unwrap();
        let b = (-100.0, 30.0, -90.0, 40.0);
        let corners = t.transform_bounds_densified(b, 0).unwrap();
        let dense = t.transform_bounds_densified(b, 21).unwrap();
        assert!(dense.1 < corners.1, "{dense:?} {corners:?}");
        assert_eq!(dense, t.transform_bounds(b).unwrap());
        assert!(t.transform_bounds_densified(b, usize::MAX).is_err());
        assert!(t.transform_bounds_densified(b, 65_537).is_err());
        let identity = Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &SpatialRef::from_epsg(4326).unwrap()).unwrap();
        assert_eq!(identity.transform_bounds_densified(b, 65_536).unwrap(), b);
        assert!(t.transform_bounds((f64::NAN, f64::NAN, f64::NAN, f64::NAN)).is_err());
    }

    /// GDAL 3.12.1 axis predicates: projected, geographic, 3D and non-NE controls.
    #[test]
    fn horizontal_axis_order_comes_from_declared_directions() {
        for (code, expected) in [(6708, true), (7792, false), (4326, true), (4979, true), (3035, true), (2065, false), (3413, false)] {
            assert_eq!(SpatialRef::from_epsg(code).unwrap().has_north_east_axis_order(), expected, "EPSG:{code}");
        }
        assert!(SpatialRef::compound(4326, 5773).unwrap().has_north_east_axis_order());
        assert!(!SpatialRef::default().has_north_east_axis_order());
    }

    #[test]
    fn metres_per_unit_of_tile_matrix_sets() {
        assert!((SpatialRef::from_epsg(3857).unwrap().metres_per_unit() - 1.0).abs() < 1e-12);
        assert!((SpatialRef::from_epsg(2263).unwrap().metres_per_unit() - 0.3048006096012192).abs() < 1e-12);
        let deg = 6_378_137.0 * std::f64::consts::PI / 180.0;
        assert!((SpatialRef::parse("OGC:CRS84").unwrap().metres_per_unit() - deg).abs() < 1e-6);
        // WKT 2: the LENGTHUNIT of the coordinate system's axes.
        let wkt2 = r#"PROJCRS["x",BASEGEOGCRS["WGS 84",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563]]],CONVERSION["c",METHOD["Transverse Mercator"],PARAMETER["False easting",500000,LENGTHUNIT["metre",1]]],CS[Cartesian,2],AXIS["easting",east,ORDER[1],LENGTHUNIT["foot",0.3048]],AXIS["northing",north,ORDER[2],LENGTHUNIT["foot",0.3048]]]"#;
        assert!((SpatialRef::from_wkt(wkt2).metres_per_unit() - 0.3048).abs() < 1e-12);
    }

    #[test]
    fn mercator_standard_parallel_survives_wkt() {
        // On another ellipsoid, and with a +k that the standard parallel overrides. (wbprojection
        // writes a spherical Mercator as Mercator_Auxiliary_Sphere, without its scale.)
        let wgs84 = SpatialRef::from_epsg(4326).unwrap();
        for p in ["+proj=merc +lat_ts=-41 +lon_0=20 +ellps=intl +units=m", "+proj=merc +lat_ts=30 +k=1 +datum=WGS84 +units=m"] {
            let crs = SpatialRef::from_proj4(p).unwrap();
            let from_wkt = SpatialRef::parse(&crs.to_wkt().unwrap()).unwrap();
            let (x, y) = Transformer::new(&wgs84, &crs).unwrap().transform(25.0, -50.0).unwrap();
            let (wx, wy) = Transformer::new(&wgs84, &from_wkt).unwrap().transform(25.0, -50.0).unwrap();
            assert!((x - wx).abs() < 1e-3 && (y - wy).abs() < 1e-3, "{p}: ({x}, {y}) vs ({wx}, {wy})");
        }
    }

    #[test]
    fn proj4_extension_describes_the_whole_crs() {
        // GDAL and PROJ read a WKT 1 with a PROJ4 extension as that PROJ string, whatever its
        // nodes say: here a central meridian of 10, not -75.
        let wkt = r#"PROJCS["unknown",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",-75],PARAMETER["scale_factor",1],PARAMETER["false_easting",0],PARAMETER["false_northing",0],UNIT["metre",1],EXTENSION["PROJ4","+proj=tmerc +lon_0=10 +datum=WGS84 +units=m +no_defs"]]"#;
        let own = Def::from_proj("+proj=tmerc +lon_0=10 +datum=WGS84 +units=m +no_defs").unwrap();
        let bare = SpatialRef { wkt: Some(wkt.into()), ..Default::default() };
        for d in [Def::new(&SpatialRef::from_wkt(wkt)).unwrap(), Def::new(&bare).unwrap(), Def::wkt_conversion(wkt).unwrap()] {
            assert!(d.same_conversion(&own));
        }
        // Over a declared code it does not convert as (UTM zone 18N's central meridian is -75).
        let declared = format!("{},AUTHORITY[\"EPSG\",\"32618\"]]", &wkt[..wkt.len() - 1]);
        let s = SpatialRef::from_wkt(&declared);
        assert_eq!(s.epsg, None);
        assert!(Def::new(&s).unwrap().same_conversion(&own));
        // A declared code it converts as is kept.
        let utm = r#"PROJCS["WGS 84 / UTM zone 18N",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["central_meridian",-75],UNIT["metre",1],EXTENSION["PROJ4","+proj=utm +zone=18 +datum=WGS84 +units=m +no_defs"],AUTHORITY["EPSG","32618"]]"#;
        assert_eq!(SpatialRef::from_wkt(utm).epsg, Some(32618));
        // Whose TOWGS84 does not override the extension's datum.
        let shifted = utm.replace("298.257223563]]", "298.257223563],TOWGS84[100,0,0,0,0,0,0]]");
        let s = SpatialRef::from_wkt(&shifted);
        let own = Def::from_proj("+proj=utm +zone=18 +datum=WGS84 +units=m +no_defs").unwrap();
        assert!(shifted.contains("TOWGS84") && Def::new(&s).unwrap().same_datum(&own));
        // EPSG:3857's WKT, whose extension gives the datum as a null grid.
        assert_eq!(SpatialRef::from_wkt(epsg_wkt(3857).unwrap()).epsg, Some(3857));
        // Nor one on another ellipsoid.
        assert_eq!(SpatialRef::from_wkt(&utm.replace("+datum=WGS84", "+ellps=intl")).epsg, None);
        let geog = r#"GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433],EXTENSION["PROJ4","+proj=longlat +ellps=intl +no_defs"],AUTHORITY["EPSG","4326"]]"#;
        assert_eq!(SpatialRef::from_wkt(geog).epsg, None);
        // Nor a declared ESRI code it does not convert as.
        let moll = SpatialRef::from_esri(54009).unwrap().to_wkt().unwrap();
        let i = moll.rfind(",AUTHORITY[").unwrap();
        let other = format!("{},EXTENSION[\"PROJ4\",\"+proj=moll +lon_0=10 +datum=WGS84 +units=m\"]{}", &moll[..i], &moll[i..]);
        assert_eq!(SpatialRef::from_wkt(&other).authority(), None);
        let same = other.replace("+lon_0=10", "+lon_0=0");
        assert_eq!(SpatialRef::from_wkt(&same).authority(), Some(("ESRI", 54009)));
        // Not a declared code on another datum than the extension's (ED50, which shifts).
        let ed50 = utm.replace("+zone=18 +datum=WGS84", "+zone=32 +ellps=intl").replace("-75", "9").replace("32618", "23032");
        assert_eq!(SpatialRef::from_wkt(&ed50).epsg, None);
    }

    /// Root identifiers other than the one declared (see [`declared_epsg`]) do not reach
    /// wbprojection, which would read the CRS by them: a geographic CRS declaring ESRI:102100 and
    /// EPSG:27700 (no code, as PROJ 9.9.0), or EPSG:4326 then EPSG:27700, stays geographic.
    #[cfg(feature = "wbprojection-fallback")]
    #[test]
    fn passed_over_root_codes_do_not_reach_wbprojection() {
        let wgs84 = |ids: &str| {
            format!(
                r#"GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433],{ids}]"#
            )
        };
        let all_passed_over = wgs84(r#"AUTHORITY["ESRI","102100"],AUTHORITY["EPSG","27700"]"#);
        assert_eq!(declared_epsg(&all_passed_over), None);
        // wbprojection reads the last of several root codes: only the declared one, which fits,
        // reaches it.
        let fitting_first = wgs84(r#"AUTHORITY["EPSG","4326"],AUTHORITY["EPSG","27700"]"#);
        assert_eq!(declared_epsg(&fitting_first), Some(4326));
        assert!(without_unfit_root_code(&fitting_first).is_some_and(|w| w.contains(r#"AUTHORITY["EPSG","4326"]"#) && !w.contains("27700")));
        let fitting_last = wgs84(r#"AUTHORITY["EPSG","27700"],AUTHORITY["EPSG","4326"]"#);
        assert_eq!(declared_epsg(&fitting_last), Some(4326));
        for wkt in [all_passed_over, fitting_first, fitting_last] {
            let s = SpatialRef { wkt: Some(wkt.clone()), ..Default::default() };
            assert!(s.to_wbp().unwrap().is_geographic(), "{wkt}");
        }
    }

    #[test]
    fn declared_codes_win() {
        let nad83 = r#"GEOGCS["NAD83",DATUM["North_American_Datum_1983",SPHEROID["GRS 1980",6378137,298.257222101,AUTHORITY["EPSG","7019"]],AUTHORITY["EPSG","6269"]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],AXIS["Latitude",NORTH],AXIS["Longitude",EAST],AUTHORITY["EPSG","4269"]]"#;
        assert_eq!(declared_epsg(nad83), Some(4269));
        assert_eq!(SpatialRef::from_wkt(nad83).epsg, Some(4269));
        assert_eq!(declared_epsg(r#"GEOGCRS["WGS 84",CS[ellipsoidal,2],ID["EPSG",4326]]"#), Some(4326));
        assert_eq!(declared_epsg(r#"GEOGCS["x",DATUM["d",SPHEROID["s",1,2]],UNIT["degree",0.01745,AUTHORITY["EPSG","9122"]]]"#), None);
        // Brackets of either kind: `projinfo -o PROJJSON` (PROJ 9.9.0) gives this WKT the `id`
        // EPSG 4326.
        let parentheses = r#"GEOGCS("WGS 84",DATUM("WGS_1984",SPHEROID("WGS 84",6378137,298.257223563)),PRIMEM("Greenwich",0),UNIT("degree",0.0174532925199433),AUTHORITY("EPSG","4326"))"#;
        assert_eq!(declared_epsg(parentheses), Some(4326));
        assert_eq!(SpatialRef::from_wkt(parentheses).epsg, Some(4326));
        assert_eq!(declared_epsg(r#"GEOGCS["x",UNIT["degree",0.0174532925199433],AUTHORITY[EPSG,4326]]"#), Some(4326));
        // The first identifier of the authority the registry has a CRS of the root's kind for, as
        // PROJ: there is no EPSG:1, EPSG:27700 is projected, and ESRI codes are passed over for an
        // EPSG one; another authority's identifier counts, and so does the last one.
        let ids = |ids: &str| declared_epsg(&format!(r#"GEOGCRS["x",CS[ellipsoidal,2],{ids}]"#));
        assert_eq!(ids(r#"ID["ESRI",4326],ID["EPSG",4326]"#), Some(4326));
        assert_eq!(ids(r#"ID["EPSG",1],ID["EPSG",4326]"#), Some(4326));
        assert_eq!(ids(r#"ID["EPSG","not-a-code"],ID["EPSG",4326]"#), Some(4326));
        assert_eq!(ids(r#"ID["ESRI",102100],ID["EPSG",4326]"#), Some(4326));
        assert_eq!(ids(r#"ID["EPSG",27700],ID["EPSG",4326]"#), Some(4326));
        assert_eq!(ids(r#"ID["EPSG",4277],ID["EPSG",4326]"#), Some(4277));
        assert_eq!(ids(r#"ID["ESRI",104000],ID["EPSG",4326]"#), Some(4326));
        assert_eq!(declared_code(r#"GEOGCRS["x",CS[ellipsoidal,2],ID["EPSG",4326],ID["ESRI",104000]]"#, "ESRI"), Some(104000));
        // ESRI's code first, as the ESRI path reads it (gated by `esri_fits` there); EPSG's code
        // read past it.
        let esri_first = r#"GEOGCRS["x",CS[ellipsoidal,2],ID["ESRI",104000],ID["EPSG",4326]]"#;
        assert_eq!((declared_code(esri_first, "ESRI"), declared_epsg(esri_first)), (Some(104000), Some(4326)));
        // An engineering root takes a registered code of any kind; a code the registry does not
        // have is still passed over.
        let local = |ids: &str| declared_epsg(&format!(r#"LOCAL_CS["x",LOCAL_DATUM["d",0],UNIT["metre",1],{ids}]"#));
        assert_eq!(local(r#"AUTHORITY["EPSG","1"],AUTHORITY["EPSG","27700"]"#), Some(27700));
        assert_eq!(local(r#"AUTHORITY["ESRI","102100"],AUTHORITY["EPSG","27700"]"#), Some(27700));
        assert_eq!(local(r#"AUTHORITY["EPSG","1"],AUTHORITY["EPSG","2"]"#), None);
        // All passed over: none, as PROJ (a projected code on a geographic CRS, after an ESRI one).
        assert_eq!(ids(r#"ID["ESRI",102100],ID["EPSG",27700]"#), None);
        assert_eq!(ids(r#"ID["IGNF","WGS84G"],ID["EPSG",4326]"#), None);
        assert_eq!(ids(r#"ID["EPSG",4326],ID["ESRI",4326]"#), Some(4326));
        assert_eq!(ids(r#"ID["EPSG",1]"#), Some(1));
        // A code is a whole number: `Node::epsg` (datums, units) reads none from 6326.5 or -1.
        let datum = |code: &str| wkt1::parse(&format!(r#"DATUM["d",SPHEROID["s",1,2],AUTHORITY["EPSG",{code}]]"#)).and_then(|d| d.epsg());
        assert_eq!((datum("6326"), datum(r#""6326""#), datum("6326.5"), datum("-1")), (Some(6326), Some(6326), None, None));
        // Without its root codes, written again from its nodes.
        let stripped = strip_root_code(r#"GEOGCRS["x",CS[ellipsoidal,2],ID["EPSG",4326,URI["urn:ogc:def:crs:EPSG::4326"]],REMARK["r"]]"#);
        assert_eq!(stripped.as_deref(), Some(r#"GEOGCRS["x",CS[ellipsoidal,2],REMARK["r"]]"#));
        assert_eq!(strip_root_code(r#"GEOGCRS["x",CS[ellipsoidal,2]]"#), None);
        // Codes of a datum or prime meridian are not the CRS's (GDAL wrote this one in a
        // GeoPackage, from a GeoParquet file's CRS).
        let wgs84 = r#"GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563],AUTHORITY["EPSG","6326"]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["Degree",0.0174532925199433],AXIS["Longitude",EAST],AXIS["Latitude",NORTH]]"#;
        assert_eq!(identify_epsg(wgs84), Some(4326));
    }

    #[test]
    fn unknown_datums_are_not_guessed_and_named_ones_are_identified_as_in_gdal() {
        // pyproj.CRS("+proj=tmerc +lon_0=10 +ellps=... +units=m +no_defs").to_wkt("WKT1_GDAL"):
        // a datum without code nor TOWGS84, which GDAL and PROJ leave unshifted and without
        // code. (These became Korean 1985, LUREF, Pulkovo 1942(58)... and took their shifts.)
        let wgs84 = SpatialRef::from_epsg(4326).unwrap();
        let ellipsoids = [
            ("bessel", "Bessel 1841", "6377397.155,299.1528128"),
            ("clrk66", "Clarke 1866", "6378206.4,294.978698213898"),
            ("intl", "International 1924 (Hayford 1909, 1910)", "6378388,297"),
            ("airy", "Airy 1830", "6377563.396,299.3249646"),
            ("krass", "Krassovsky, 1942", "6378245,298.3"),
            ("GRS80", "GRS 1980", "6378137,298.257222101,AUTHORITY[\"EPSG\",\"7019\"]"),
        ];
        for (ellps, name, spheroid) in ellipsoids {
            let geogcs = format!(
                r#"GEOGCS["unknown",DATUM["Unknown based on {name} ellipsoid",SPHEROID["{name}",{spheroid}]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]]]"#
            );
            let projcs = format!(
                r#"PROJCS["unknown",{geogcs},PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",10],PARAMETER["scale_factor",1],PARAMETER["false_easting",0],PARAMETER["false_northing",0],UNIT["metre",1,AUTHORITY["EPSG","9001"]],AXIS["Easting",EAST],AXIS["Northing",NORTH]]"#
            );
            let proj = |p: &str| SpatialRef::from_proj4(p).unwrap();
            let pairs = [
                (projcs, proj(&format!("+proj=tmerc +lon_0=10 +ellps={ellps} +units=m +no_defs"))),
                (geogcs, proj(&format!("+proj=longlat +ellps={ellps} +no_defs"))),
            ];
            for (wkt, string) in pairs {
                let crs = SpatialRef::from_wkt(&wkt);
                assert_eq!(crs.epsg, None, "{ellps}: {wkt}");
                let (x, y) = Transformer::new(&wgs84, &string).unwrap().transform(12.0, 48.0).unwrap();
                let (wx, wy) = Transformer::new(&wgs84, &crs).unwrap().transform(12.0, 48.0).unwrap();
                // Within 1 mm (degrees: 1e-8 is about 1 mm).
                let tol = if crs.is_geographic() { 1e-8 } else { 1e-3 };
                assert!((x - wx).abs() < tol && (y - wy).abs() < tol, "{ellps}: {x} {y} != {wx} {wy}");
            }
        }
        // ESRI WKT (pyproj's WKT1_ESRI) naming a datum by ESRI's name: the codes GDAL and pyproj
        // identify, ED50 rather than ED50(ED77) (EPSG:4154), ETRS89 by its ESRI alias.
        let esri = |code: u32, name: &str, datum: &str, spheroid: &str, rest: &str| {
            let geogcs = format!(
                r#"GEOGCS["GCS_{datum}",DATUM["D_{datum}",SPHEROID[{spheroid}]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#
            );
            let wkt = if name.is_empty() { geogcs } else { format!(r#"PROJCS["{name}",{geogcs},{rest}]"#) };
            assert_eq!(SpatialRef::from_wkt(&wkt).epsg, Some(code), "{wkt}");
            wkt
        };
        let utm = |lon: i32| {
            format!(
                r#"PROJECTION["Transverse_Mercator"],PARAMETER["False_Easting",500000.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",{lon}.0],PARAMETER["Scale_Factor",0.9996],PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]"#
            )
        };
        let intl = r#""International_1924",6378388.0,297.0"#;
        let grs80 = r#""GRS_1980",6378137.0,298.257222101"#;
        let ed50 = esri(4230, "", "European_1950", intl, "");
        let ed50_utm = esri(23032, "ED_1950_UTM_Zone_32N", "European_1950", intl, &utm(9));
        esri(4269, "", "North_American_1983", grs80, "");
        esri(26918, "NAD_1983_UTM_Zone_18N", "North_American_1983", grs80, &utm(-75));
        esri(4258, "", "ETRS_1989", grs80, "");
        esri(25832, "ETRS_1989_UTM_Zone_32N", "ETRS_1989", grs80, &utm(9));
        // On ETRS89-NOR, which ESRI writes D_ETRS_1989 (GDAL identifies it by its ESRI name).
        let ntm = r#"PROJECTION["Transverse_Mercator"],PARAMETER["False_Easting",100000.0],PARAMETER["False_Northing",1000000.0],PARAMETER["Central_Meridian",18.5],PARAMETER["Scale_Factor",1.0],PARAMETER["Latitude_Of_Origin",58.0],UNIT["Meter",1.0]"#;
        esri(5118, "ETRS_1989_NTM_Zone_18", "ETRS_1989", grs80, ntm);
        // With ED50's datum shift, as EPSG:4230 and EPSG:23032 (about 100 m in the Netherlands).
        for (wkt, code, tol) in [(&ed50, 4230, 1e-8), (&ed50_utm, 23032, 1e-3)] {
            let t = Transformer::new(&wgs84, &SpatialRef::from_wkt(wkt)).unwrap();
            let (x, y) = t.transform(5.0, 52.0).unwrap();
            let (ex, ey) = Transformer::new(&wgs84, &SpatialRef::from_epsg(code).unwrap()).unwrap().transform(5.0, 52.0).unwrap();
            assert!((x - ex).abs() < tol && (y - ey).abs() < tol, "{code}: {x} {y} != {ex} {ey}");
        }
        let (x, _) = Transformer::new(&wgs84, &SpatialRef::from_wkt(&ed50)).unwrap().transform(5.0, 52.0).unwrap();
        assert!((x - 5.0).abs() > 5e-4, "{x}");
        // A misspelled datum is not known by name, so it has no code, even on ED50's ellipsoid
        // and in its ESRI form: the registry is not asked to guess.
        let misspelled = ed50.replace("D_European_1950", "D_Europaen_1950");
        assert_eq!(SpatialRef::from_wkt(&misspelled).epsg, None);
        // A WKT 2 CRS on a datum ensemble without IDs: known by the ensemble's name.
        let wkt2 = r#"GEOGCRS["WGS 84",ENSEMBLE["World Geodetic System 1984 ensemble",MEMBER["World Geodetic System 1984 (G2139)"],ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]],ENSEMBLEACCURACY[2.0]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]],CS[ellipsoidal,2],AXIS["geodetic latitude (Lat)",north,ORDER[1],ANGLEUNIT["degree",0.0174532925199433]],AXIS["geodetic longitude (Lon)",east,ORDER[2],ANGLEUNIT["degree",0.0174532925199433]]]"#;
        assert_eq!(SpatialRef::from_wkt(wkt2).epsg, Some(4326));
    }

    #[test]
    fn conversions_in_other_units_differ() {
        // Nebraska in metres (EPSG:6516) and in US survey feet (EPSG:6880): the same positions on
        // the ground, not the same conversion (identification and proj_to_wkt's faithfulness).
        let lcc = "+proj=lcc +lat_0=39.8333333333333 +lon_0=-100 +lat_1=43 +lat_2=40 +x_0=500000 +y_0=0 +ellps=GRS80";
        let m = Def::from_proj(&format!("{lcc} +units=m")).unwrap();
        let ft = Def::from_proj(&format!("{lcc} +units=us-ft")).unwrap();
        assert!(m.same_conversion(&Def::from_proj(&format!("{lcc} +units=m")).unwrap()));
        assert!(!m.same_conversion(&ft) && !ft.same_conversion(&m));
    }

    #[test]
    fn datum_names_are_read_from_the_generated_table() {
        // As scripts/gen_datum_names.py normalizes them.
        assert_eq!(epsg::datum_key("D_European_1950"), "european1950");
        assert_eq!(epsg::datum_key("Militar-Geographische Institut"), "militargeographischeinstitut");
        assert_eq!(epsg::datum_key("CH1903+"), "ch1903+");
        // Official, GDAL's WKT 1, EPSG and ESRI names.
        for (name, code) in [
            ("European Datum 1950", 6230),
            ("European_Datum_1950", 6230),
            ("ED50", 6230),
            ("D_European_1950", 6230),
            ("D_ETRS_1989", 6258),
            ("WGS_1984", 6326),
            ("D_North_American_1983", 6269),
            ("CH1903+", 6150),
        ] {
            assert_eq!(epsg::datums_named(name), vec![code], "{name}");
        }
        // EPSG lists NAD83 as an alias of NAD83(HARN) too: identification takes the base datum.
        assert_eq!(epsg::datums_named("NAD83"), vec![6152, 6269]);
        let nad83 = r#"GEOGCS["unknown",DATUM["NAD83",SPHEROID["GRS 1980",6378137,298.257222101]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#;
        assert_eq!(identify_epsg(nad83), Some(4269));
        assert!(epsg::datums_named("Unknown based on Bessel 1841 ellipsoid").is_empty());
    }

    /// Compound OGC references retain horizontal and vertical components, as GDAL 3.13.3.
    #[test]
    fn compound_urns_preserve_both_components() {
        for (text, horizontal, vertical) in [
            ("urn:ogc:def:crs,crs:EPSG::2154,crs:EPSG::5720", 2154, 5720),
            ("URN:OGC:DEF:CRS,CRS:epsg:9.9:4269,CRS:epsg::5713", 4269, 5713),
            ("urn:ogc:def:crs,crs:EPSG::4326,crs:EPSG::5773", 4326, 5773),
        ] {
            let expected = SpatialRef::compound(horizontal, vertical).unwrap();
            for actual in [SpatialRef::parse(text), SpatialRef::parse_literal(text)] {
                let actual = actual.unwrap();
                assert_eq!(actual.epsg, None, "{text}");
                assert_eq!(actual.to_wkt(), expected.to_wkt(), "{text}");
                let json: serde_json::Value = serde_json::from_str(&actual.to_projjson().unwrap()).unwrap();
                assert_eq!(json["type"], "CompoundCRS");
            }
        }
    }

    #[test]
    fn unrelated_text_and_unsupported_compound_references_are_refused() {
        for text in [
            "not EPSG:4326",
            "description EPSG:4326 suffix",
            "PREFIX[ID[\"EPSG\",4326]]",
            "EPSG:2154+garbage",
            "EPSG:2154+5720junk",
            "EPSG:4326.0:3857",
            "EPSG:1/3857",
            "urn:ogc:def:crs:EPSG:4326",
            "urn:ogc:def:crs:EPSG::1/3857",
            "https://www.opengis.net/def/crs/EPSG//1/3857",
            "urn:ogc:def:crs:EPSG:4326:3857junk",
            "https://www.opengis.net/def/crs/EPSG/4326/3857junk",
            "urn:ogc:def:crs:EPSG::2154,crs:EPSG::5720",
            "urn:ogc:def:crs,crs:IGNF::LAMB93,crs:EPSG::5720",
            "urn:ogc:def:crs,crs:EPSG::2154",
            "urn:ogc:def:crs,crs:EPSG::2154,crs:EPSG::5720,crs:EPSG::5713",
            "urn:ogc:def:crs,cs:EPSG::2154,crs:EPSG::5720",
            "urn:ogc:def:crs,crs:EPSG::99999999,crs:EPSG::5720",
            "urn:ogc:def:crs,crs:EPSG::2154,crs:EPSG::99999999",
            "urn:ogc:def:crs,crs:EPSG::2154junk,crs:EPSG::5720",
        ] {
            assert!(SpatialRef::parse_literal(text).is_err(), "{text}");
        }
    }

    /// Reference syntax: GDAL 3.13.3. Terrio string references require 2D horizontal roles.
    #[test]
    fn compound_components_must_have_horizontal_and_vertical_roles() {
        for (horizontal, vertical) in [(5720, 5713), (4978, 5720), (4979, 5720), (7405, 5720), (2154, 4326)] {
            for text in [format!("EPSG:{horizontal}+{vertical}"), format!("urn:ogc:def:crs,crs:EPSG::{horizontal},crs:EPSG::{vertical}")] {
                assert!(SpatialRef::parse_literal(&text).is_err(), "{text}");
            }
        }
        for text in ["EPSG:+2154+5720", "EPSG:2154++5720"] {
            assert!(SpatialRef::parse_literal(text).is_err(), "{text}");
        }
    }

    /// Canonical references stay identical across feature configurations; legacy
    /// free-form acceptance is explicit, including the fallback's version-token bug.
    #[test]
    fn authority_reference_forms_are_feature_independent() {
        // input, canonical result, upstream legacy-parser result
        let cases = [
            ("4326", Some(4326), Some(4326)),
            (" epsg:4326 ", Some(4326), Some(4326)),
            ("EPSG::4326", Some(4326), Some(4326)),
            ("urn:ogc:def:crs:EPSG::4326", Some(4326), Some(4326)),
            ("urn:ogc:def:crs:EPSG:6.6:4326", Some(4326), Some(6)),
            ("urn:ogc:def:crs:EPSG:9.9:4326", Some(4326), Some(9)),
            ("http://www.opengis.net/def/crs/EPSG/0/4326", Some(4326), Some(4326)),
            ("https://opengis.net/def/crs/EPSG/6.6/4326", Some(4326), Some(6)),
            ("https://www.opengis.net/def/crs/EPSG/100/4326", Some(4326), Some(100)),
            ("+4326", None, Some(4326)),
            ("EPSG:+4326", None, None),
            ("EPSG:-4326", None, None),
            ("EPSG:::4326", None, Some(4326)),
            ("EPSG:4326junk", None, Some(4326)),
            ("4326junk", None, None),
            ("EPSG:4326/extra", None, Some(4326)),
            ("urn:ogc:def:crs:EPSG::4326:extra", None, Some(4326)),
            ("http://www.opengis.net/def/crs/EPSG/0/4326/extra", None, Some(4326)),
            ("http://www.opengis.net/def/crs/EPSG//4326", None, Some(4326)),
            ("urn:ogc:def:crs:EPSG::", None, None),
            ("EPSG:", None, None),
            ("4294967296", None, None),
            ("EPSG:4294967296", None, None),
            ("ESRI:4326", None, None),
            ("", None, None),
        ];
        for (input, canonical, legacy) in cases {
            assert_eq!(canonical_epsg_reference(input), canonical, "canonical: {input}");
            #[cfg(feature = "wbprojection-fallback")]
            {
                assert_eq!(wbp::epsg_from_srs_reference(input), legacy, "upstream: {input}");
                assert_eq!(epsg_reference(input), canonical.or(legacy), "with fallback: {input}");
            }
            #[cfg(not(feature = "wbprojection-fallback"))]
            {
                let _ = legacy;
                assert_eq!(epsg_reference(input), canonical, "native: {input}");
            }
            if canonical == Some(4326) {
                assert_eq!(SpatialRef::parse(input).unwrap().epsg, Some(4326), "public: {input}");
            }
        }
        // Public '+' inputs are PROJ definitions, even though the legacy helper
        // accepts a leading '+' on a bare number.
        assert!(SpatialRef::parse("+4326").is_err());
    }

    #[test]
    fn parses_common_forms() {
        for s in ["EPSG:4326", "4326", "urn:ogc:def:crs:EPSG::4326", "http://www.opengis.net/def/crs/EPSG/0/4326", "OGC:CRS84"] {
            assert_eq!(SpatialRef::parse(s).unwrap().epsg, Some(4326), "{s}");
        }
        for code in [54008, 54009, 54030] {
            assert_eq!(SpatialRef::from_epsg(code).unwrap(), SpatialRef::from_esri(code).unwrap());
        }
        assert!(SpatialRef::from_epsg(102003).is_err());
        assert_eq!(SpatialRef::parse("ESRI:102003").unwrap().esri, Some(102003));
        let wkt = SpatialRef::from_epsg(32633).unwrap().to_wkt().unwrap();
        assert_eq!(SpatialRef::parse(&wkt).unwrap().epsg, Some(32633));
        // An ESRI .prj (New York City boroughs): GDAL identifies EPSG:2263.
        let esri = r#"PROJCS["NAD_1983_StatePlane_New_York_Long_Island_FIPS_3104_Feet",GEOGCS["GCS_North_American_1983",DATUM["D_North_American_1983",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Lambert_Conformal_Conic"],PARAMETER["False_Easting",984250.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",-74.0],PARAMETER["Standard_Parallel_1",40.66666666666666],PARAMETER["Standard_Parallel_2",41.03333333333333],PARAMETER["Latitude_Of_Origin",40.16666666666666],UNIT["Foot_US",0.3048006096012192]]"#;
        assert_eq!(SpatialRef::parse(esri).unwrap().epsg, Some(2263));
        assert_eq!(SpatialRef::parse("EPSG:32633").unwrap().name().unwrap(), "WGS 84 / UTM zone 33N");
    }

    #[test]
    fn nad83_to_wgs84_is_a_null_transformation_as_in_gdal() {
        // echo "-106.001666667 40.001666667" | gdaltransform -s_srs EPSG:4269 -t_srs EPSG:32613
        let t = Transformer::new(&SpatialRef::from_epsg(4269).unwrap(), &SpatialRef::from_epsg(32613).unwrap()).unwrap();
        let (x, y) = t.transform(-106.001666667, 40.001666667).unwrap();
        assert!((x - 414499.347197613).abs() < 1e-3, "{x}");
        assert!((y - 4428422.65095327).abs() < 1e-3, "{y}");
        // echo "-100 40" | gdaltransform -s_srs EPSG:4269 -t_srs EPSG:4326 -> -100 40
        let t = Transformer::new(&SpatialRef::from_epsg(4269).unwrap(), &SpatialRef::from_epsg(4326).unwrap()).unwrap();
        assert_eq!(t.transform(-100.0, 40.0).unwrap(), (-100.0, 40.0));
        // ED50 keeps its datum shift: gdaltransform gives 9.99897347903267 44.9990669286146.
        let t = Transformer::new(&SpatialRef::from_epsg(4230).unwrap(), &SpatialRef::from_epsg(4326).unwrap()).unwrap();
        let (x, y) = t.transform(10.0, 45.0).unwrap();
        assert!((x - 9.99897347903267).abs() < 1e-4 && (y - 44.9990669286146).abs() < 1e-4, "{x} {y}");
    }

    #[test]
    fn transforms_to_web_mercator() {
        let t = Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &SpatialRef::from_epsg(3857).unwrap()).unwrap();
        let (x, y) = t.transform(-115.0, 36.0).unwrap();
        assert!((x - -12801741.0).abs() < 1.0, "{x}");
        assert!((y - 4300621.4).abs() < 1.0, "{y}");
    }

    /// A Lambert conformal conic (2SP, on the WGS 84 ellipsoid with an unnamed datum, as a
    /// gridded climate dataset declares it) gives GDAL's coordinates bit for bit: GDAL 3.12.1 with
    /// PROJ 9.7.1, through rasterio's `warp.transform`, both ways. Its constants and its forward
    /// projection go through `tsfn`; in the form PROJ used before 9 every latitude below was a
    /// few units in the last place off, and the extent of a grid warped from it with them. Bit
    /// for bit on the glibc the values were recorded on (2.39), whose `asinh` and `atanh` PROJ and
    /// Terrio share; other releases round those differently, so within 4 units in the last place.
    /// Exact parity on the installed glibc is checked against GDAL by `benchmark-compat`.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn lambert_conformal_conic_matches_proj() {
        const LCC: &str = r#"PROJCS["undefined",GEOGCS["undefined",DATUM["undefined",SPHEROID["undefined",6378137,298.257223563]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433]],PROJECTION["Lambert_Conformal_Conic_2SP"],PARAMETER["standard_parallel_1",25],PARAMETER["standard_parallel_2",60],PARAMETER["latitude_of_origin",42.5],PARAMETER["central_meridian",-100],PARAMETER["false_easting",0],PARAMETER["false_northing",0],UNIT["metre",1,AUTHORITY["EPSG","9001"]],AXIS["Easting",EAST],AXIS["Northing",NORTH]]"#;
        let (lcc, wgs84) = (SpatialRef::parse(LCC).unwrap(), SpatialRef::from_epsg(4326).unwrap());
        unsafe extern "C" {
            safe fn gnu_get_libc_version() -> *const std::ffi::c_char;
        }
        // SAFETY: glibc returns a static NUL-terminated string.
        let recorded_libc = unsafe { std::ffi::CStr::from_ptr(gnu_get_libc_version()) }.to_bytes() == b"2.39";
        let check = |got: (f64, f64), want: (f64, f64), label: String| {
            let ulps = |a: f64, b: f64| (a.to_bits() as i64).wrapping_sub(b.to_bits() as i64).unsigned_abs();
            let allowed = if recorded_libc { 0 } else { 4 };
            assert!(ulps(got.0, want.0) <= allowed && ulps(got.1, want.1) <= allowed, "{label}");
        };
        // A 1 km grid's corners, the projection's origin and points beyond the standard parallels.
        let inverse = [
            ((-5802750.0, -38500.0), (-160.3055519816432, 21.866370142929064)),
            ((-5518750.0, -38500.0), (-158.236286957958, 23.518637016668105)),
            ((-5802750.0, -622500.0), (-156.80763425972904, 17.953942068222393)),
            ((-5518750.0, -622500.0), (-154.77159676995768, 19.4872617759031)),
            ((0.0, 0.0), (-100.0, 42.5)),
            ((1000000.0, 2000000.0), (-81.90841925276249, 60.06731242458275)),
            ((-2500000.0, -1500000.0), (-125.15217207822917, 25.024669562373553)),
            ((3000000.0, -1000000.0), (-68.40599710933087, 27.79413628919135)),
        ];
        let t = Transformer::new(&lcc, &wgs84).unwrap();
        for ((x, y), want) in inverse {
            let got = t.transform(x, y).unwrap();
            check(got, want, format!("({x}, {y}) to {got:?}, GDAL {want:?}"));
        }
        let forward = [
            ((-157.5, 21.0), (-5640537.969442342, -305411.69883608964)),
            ((-100.0, 25.0), (0.0, -1882531.3839104532)),
            ((-80.5, 35.25), (1692969.5890082794, -571387.0049998641)),
            ((-120.0, 59.0), (-1131713.4059237528, 1908889.5129129528)),
            ((-60.0, 10.0), (4688828.012877845, -2478499.0432682936)),
            ((-155.0, 19.0), (-5572503.548581873, -651443.9024563333)),
        ];
        let t = Transformer::new(&wgs84, &lcc).unwrap();
        for ((lon, lat), want) in forward {
            let got = t.transform(lon, lat).unwrap();
            check(got, want, format!("({lon}, {lat}) to {got:?}, GDAL {want:?}"));
        }
    }

    #[test]
    fn reverse_degree_division_skips_web_mercator_and_wgs84() {
        let srs = |code| SpatialRef::from_epsg(code).unwrap();
        let pair = |a, b| {
            (Transformer::new(&srs(a), &srs(b)).unwrap(), Transformer::new(&srs(a), &srs(b)).unwrap().with_reverse_degree_division())
        };
        let bits = |p: (f64, f64)| (p.0.to_bits(), p.1.to_bits());
        let points = |x0: f64, y0: f64, step: f64| (0..2000).map(move |i| (x0 + f64::from(i) * step, y0 + f64::from(i) * step));

        let (plain, reverse) = pair(3857, 4326);
        for (x, y) in points(100_000.0, 200_000.0, 1234.5) {
            assert_eq!(bits(plain.transform(x, y).unwrap()), bits(reverse.transform(x, y).unwrap()));
        }
        let (plain, reverse) = pair(4326, 3857);
        for (lat, lon) in points(10.0, 20.0, 0.0123) {
            assert_eq!(bits(plain.transform(lat, lon).unwrap()), bits(reverse.transform(lat, lon).unwrap()));
        }
        // Geographic output of a pair GDAL inverts: some degrees round differently.
        let (plain, reverse) = pair(26913, 4269);
        let changed = points(400_000.0, 4_400_000.0, 12.5)
            .filter(|(x, y)| bits(plain.transform(*x, *y).unwrap()) != bits(reverse.transform(*x, *y).unwrap()))
            .count();
        assert!(changed > 0, "the reverse conversion never differed");
    }

    #[test]
    fn matches_gdal_for_national_grids() {
        // gdaltransform -s_srs EPSG:2263 -t_srs EPSG:4326 (traditional GIS order): US survey feet.
        let t = Transformer::new(&SpatialRef::from_epsg(2263).unwrap(), &SpatialRef::from_epsg(4326).unwrap()).unwrap();
        let (lon, lat) = t.transform(988000.0, 200000.0).unwrap();
        assert!((lon - -73.98647263351127).abs() < 1e-9 && (lat - 40.71562970330491).abs() < 1e-9, "{lon} {lat}");
        // From WGS 84, with GDAL's datum transformations, axis orders and units.
        let cases = [
            (27700, -1.5, 52.0, 434423.00723593397, 233624.3747866078),
            (2056, 8.0, 47.0, 2642695.4201549254, 1205590.522287224),
            (21781, 8.0, 47.0, 642695.4201549252, 205590.52228722392),
            (28992, 5.0, 52.0, 128410.08537081013, 445806.50883315015),
            (2154, 2.0, 47.0, 624015.5354680117, 6656012.00965832),
            (5514, 15.0, 50.0, -703011.8819030909, -1058147.274048979),
            (2065, 15.0, 50.0, 1058147.274048981, 703011.881903093),
            (2193, 174.0, -41.0, 1684102.1344457027, 5460761.410740666),
            (27200, 174.0, -41.0, 2594115.185056257, 6022476.222384517),
            (3035, 10.0, 50.0, 4321000.0, 2987510.566964945),
            (3031, 0.0, -75.0, 0.0, 1638783.2384072365),
            (3413, -45.0, 75.0, 0.0, -1633879.4974707854),
            (5070, -96.0, 40.0, 0.0, 1886977.5648988034),
            (3577, 135.0, -25.0, 299362.2208462456, -2706085.979530222),
        ];
        let wgs84 = SpatialRef::from_epsg(4326).unwrap();
        for (code, lon, lat, x, y) in cases {
            let crs = SpatialRef::from_epsg(code).unwrap();
            let (tx, ty) = Transformer::new(&wgs84, &crs).unwrap().transform(lon, lat).unwrap();
            assert!((tx - x).abs() < 1e-3 && (ty - y).abs() < 1e-3, "EPSG:{code}: {tx} {ty}");
            let (tlon, tlat) = Transformer::new(&crs, &wgs84).unwrap().transform(x, y).unwrap();
            // Within a millimetre: PROJ inverts Helmert rotations approximately.
            assert!((tlon - lon).abs() < 2e-8 && (tlat - lat).abs() < 2e-8, "EPSG:{code} inverse: {tlon} {tlat}");
        }
    }

    #[test]
    fn custom_wkt_units_and_towgs84_as_in_gdal() {
        let wgs84 = SpatialRef::from_epsg(4326).unwrap();
        // An ESRI .prj in US survey feet that matches no EPSG code.
        let esri = r#"PROJCS["Custom_Feet_LCC",GEOGCS["GCS_North_American_1983",DATUM["D_North_American_1983",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Lambert_Conformal_Conic"],PARAMETER["False_Easting",1000000.0],PARAMETER["False_Northing",50000.0],PARAMETER["Central_Meridian",-74.1],PARAMETER["Standard_Parallel_1",40.5],PARAMETER["Standard_Parallel_2",41.1],PARAMETER["Latitude_Of_Origin",40.0],UNIT["Foot_US",0.3048006096012192]]"#;
        let crs = SpatialRef::parse(esri).unwrap();
        assert_eq!(crs.epsg, None, "GDAL identifies no EPSG code");
        let (x, y) = Transformer::new(&wgs84, &crs).unwrap().transform(-73.95, 40.75).unwrap();
        assert!((x - 1041560.5074030993).abs() < 1e-3 && (y - 323273.5802968154).abs() < 1e-3, "{x} {y}");
        // A WKT 1 with TOWGS84 and no authority code.
        let osgb = r#"PROJCS["Custom BNG",GEOGCS["OSGB36",DATUM["OSGB_1936",SPHEROID["Airy 1830",6377563.396,299.3249646],TOWGS84[446.448,-125.157,542.06,0.15,0.247,0.842,-20.489]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",49],PARAMETER["central_meridian",-2],PARAMETER["scale_factor",0.9996012717],PARAMETER["false_easting",400000],PARAMETER["false_northing",-100000],UNIT["metre",1]]"#;
        let crs = SpatialRef::from_wkt(osgb);
        let (x, y) = Transformer::new(&wgs84, &crs).unwrap().transform(-1.5, 52.0).unwrap();
        assert!((x - 434423.00723593397).abs() < 1e-3 && (y - 233624.3747866078).abs() < 1e-3, "{x} {y}");
        // A PROJ string in feet.
        let p = SpatialRef::parse("+proj=lcc +lat_0=40.1666666666667 +lon_0=-74 +lat_1=41.0333333333333 +lat_2=40.6666666666667 +x_0=300000 +y_0=0 +datum=NAD83 +units=us-ft").unwrap();
        let (lon, lat) = Transformer::new(&p, &wgs84).unwrap().transform(988000.0, 200000.0).unwrap();
        assert!((lon - -73.98647263351127).abs() < 1e-9 && (lat - 40.71562970330491).abs() < 1e-9, "{lon} {lat}");
    }

    /// A one-subgrid NTv2 file over 0..1°E, 0..1°N with nodes 1° apart: latitude shifts of 1, 2,
    /// 3 and 4 arc-seconds (SW, SE, NW, NE) and longitude shifts of 2 arc-seconds east.
    fn tiny_ntv2() -> Vec<u8> {
        let mut b = Vec::new();
        let rec_i = |b: &mut Vec<u8>, name: &str, v: i32| {
            b.extend(format!("{name:<8}").bytes());
            b.extend(v.to_le_bytes());
            b.extend([0u8; 4]);
        };
        let rec_s = |b: &mut Vec<u8>, name: &str, v: &str| {
            b.extend(format!("{name:<8}").bytes());
            b.extend(format!("{v:<8}").bytes());
        };
        let rec_f = |b: &mut Vec<u8>, name: &str, v: f64| {
            b.extend(format!("{name:<8}").bytes());
            b.extend(v.to_le_bytes());
        };
        rec_i(&mut b, "NUM_OREC", 11);
        rec_i(&mut b, "NUM_SREC", 11);
        rec_i(&mut b, "NUM_FILE", 1);
        rec_s(&mut b, "GS_TYPE", "SECONDS");
        for (k, v) in [("VERSION", "NTv2.0"), ("SYSTEM_F", "TEST"), ("SYSTEM_T", "WGS84")] {
            rec_s(&mut b, k, v);
        }
        for k in ["MAJOR_F", "MINOR_F", "MAJOR_T", "MINOR_T"] {
            rec_f(&mut b, k, 6378137.0);
        }
        rec_s(&mut b, "SUB_NAME", "TEST");
        rec_s(&mut b, "PARENT", "NONE");
        rec_s(&mut b, "CREATED", "");
        rec_s(&mut b, "UPDATED", "");
        // Latitudes, then longitudes positive west, in arc-seconds.
        for (k, v) in [("S_LAT", 0.0), ("N_LAT", 3600.0), ("E_LONG", -3600.0), ("W_LONG", 0.0), ("LAT_INC", 3600.0), ("LONG_INC", 3600.0)] {
            rec_f(&mut b, k, v);
        }
        rec_i(&mut b, "GS_COUNT", 4);
        // Rows from the south, each from east to west: latitude and longitude shifts (west
        // positive) and their accuracies.
        for lat_shift in [2.0f32, 1.0, 4.0, 3.0] {
            for v in [lat_shift, -2.0, 0.0, 0.0] {
                b.extend(v.to_le_bytes());
            }
        }
        b
    }

    #[test]
    fn ntv2_grids_shift_as_proj() {
        register_grid("terrio_test_tiny.gsb", tiny_ntv2());
        let src = SpatialRef::parse("+proj=longlat +ellps=WGS84 +nadgrids=terrio_test_tiny.gsb").unwrap();
        let wgs84 = SpatialRef::from_epsg(4326).unwrap();
        let t = Transformer::new(&src, &wgs84).unwrap();
        let (lon, lat) = t.transform(0.25, 0.5).unwrap();
        // Bilinear interpolation: 0.375 * 1 + 0.125 * 2 + 0.375 * 3 + 0.125 * 4 = 2.25" (the
        // shifts are single precision in radians, as in PROJ).
        assert!((lon - (0.25 + 2.0 / 3600.0)).abs() < 1e-10 && (lat - (0.5 + 2.25 / 3600.0)).abs() < 1e-10, "{lon} {lat}");
        let (x, y) = Transformer::new(&wgs84, &src).unwrap().transform(lon, lat).unwrap();
        assert!((x - 0.25).abs() < 1e-12 && (y - 0.5).abs() < 1e-12, "{x} {y}");
        // Outside the grid, the only transformation fails, as in PROJ.
        assert!(t.transform(5.0, 5.0).is_err());
    }

    #[test]
    fn gtx_geoids_shift_heights_as_proj() {
        // A 2 x 2 GTX grid from (0°, 0°), 1° apart: 10, 20 (south row), 30, 40.
        let mut b = Vec::new();
        for v in [0.0f64, 0.0, 1.0, 1.0] {
            b.extend(v.to_be_bytes());
        }
        for v in [2i32, 2] {
            b.extend(v.to_be_bytes());
        }
        for v in [10.0f32, 20.0, 30.0, 40.0] {
            b.extend(v.to_be_bytes());
        }
        register_grid("terrio_test_tiny.gtx", b);
        let src = SpatialRef::parse("+proj=longlat +datum=WGS84 +geoidgrids=terrio_test_tiny.gtx +vunits=m").unwrap();
        let t = Transformer::new(&src, &SpatialRef::from_epsg(4979).unwrap()).unwrap();
        let (_, _, h) = t.transform_3d(0.25, 0.5, 100.0).unwrap();
        assert!((h - 122.5).abs() < 1e-9, "{h}");
        let (_, _, z) = Transformer::new(&SpatialRef::from_epsg(4979).unwrap(), &src).unwrap().transform_3d(0.25, 0.5, h).unwrap();
        assert!((z - 100.0).abs() < 1e-9, "{z}");
    }

    #[test]
    fn a_lon_0_is_no_prime_meridian_rotation() {
        // PROJ 9.9 (`projinfo -s A -t B`) gives the ballpark between CRSs differing by their prime
        // meridian 0 m, and between longlat CRSs with `+lon_0` no accuracy, whatever their prime
        // meridians.
        let def = |s: &str| Def::from_proj(s).unwrap();
        let intl = def("+proj=longlat +ellps=intl");
        assert!(def("+proj=longlat +pm=paris +ellps=intl").rotation_only_to(&intl));
        assert!(!def("+proj=longlat +lon_0=10 +ellps=intl").rotation_only_to(&intl));
        assert!(!def("+proj=longlat +lon_0=10 +pm=paris +ellps=intl").rotation_only_to(&intl));
        assert!(!def("+proj=longlat +lon_0=10 +pm=paris +ellps=intl").rotation_only_to(&def("+proj=longlat +lon_0=10 +ellps=intl")));
        assert!(!def("+proj=longlat +pm=10 +ellps=intl").rotation_only_to(&def("+proj=longlat +lon_0=10 +ellps=intl")));
        // A projected CRS's central meridian is no `+lon_0` of a longlat CRS.
        assert!(def("+proj=tmerc +lon_0=28 +pm=ferro +ellps=bessel").rotation_only_to(&def("+proj=longlat +ellps=bessel")));
    }

    #[test]
    fn latitudes_of_origin_beyond_the_poles_are_refused() {
        // PROJ 9.9: "Invalid value for lat_0: |lat_0| should be <= 90°", from a PROJ string and WKT 2.
        assert!(SpatialRef::parse("+proj=tmerc +lat_0=100 +lon_0=9 +k=0.9996 +x_0=500000 +datum=WGS84").is_err());
        let wkt2 = r#"PROJCRS["x",BASEGEOGCRS["WGS 84",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563]]],CONVERSION["c",METHOD["Transverse Mercator",ID["EPSG",9807]],PARAMETER["Latitude of natural origin",100,ANGLEUNIT["degree",0.0174532925199433]],PARAMETER["Longitude of natural origin",9,ANGLEUNIT["degree",0.0174532925199433]],PARAMETER["Scale factor at natural origin",0.9996,SCALEUNIT["unity",1]],PARAMETER["False easting",500000,LENGTHUNIT["metre",1]],PARAMETER["False northing",0,LENGTHUNIT["metre",1]]],CS[Cartesian,2],AXIS["(E)",east,LENGTHUNIT["metre",1]],AXIS["(N)",north,LENGTHUNIT["metre",1]]]"#;
        assert!(Transformer::new(&SpatialRef::parse(wkt2).unwrap(), &SpatialRef::from_epsg(4326).unwrap()).is_err());
        // The poles themselves are origins.
        assert!(SpatialRef::parse("+proj=stere +lat_0=-90 +lat_ts=-71 +datum=WGS84").is_ok());
        assert!(SpatialRef::parse("+proj=laea +lat_0=90 +datum=WGS84").is_ok());
    }

    #[test]
    fn compound_crss_parse_as_in_gdal() {
        let c = SpatialRef::parse("EPSG:4326+5773").unwrap();
        assert_eq!(c.epsg, None);
        assert_eq!(c.name().unwrap(), "WGS 84 + EGM96 height");
        assert!(c.to_wkt().unwrap().starts_with("COMPD_CS[\"WGS 84 + EGM96 height\",GEOGCS[\"WGS 84\""));
        // Its WKT is not identified as EPSG:4326, which has no heights, read back or identified.
        let wkt = c.to_wkt().unwrap();
        assert_eq!(identify_epsg(&wkt), None);
        let read = SpatialRef::from_wkt(&wkt);
        assert_eq!((read.epsg, read.name().as_deref()), (None, Some("WGS 84 + EGM96 height")));
        // A compound WKT declaring its code keeps it.
        let declared = format!("{},AUTHORITY[\"EPSG\",\"9707\"]]", wkt.strip_suffix(']').unwrap());
        assert_eq!(SpatialRef::from_wkt(&declared).epsg, Some(9707));
        assert!(c.is_geographic());
        let k = SpatialRef::from_epsg(5498).unwrap();
        assert_eq!(k.name().unwrap(), "NAD83 + NAVD88 height");
        // Without grids, heights between CRSs with heights are kept (PROJ's ballpark).
        let t = Transformer::new(&SpatialRef::from_epsg(9705).unwrap(), &SpatialRef::from_epsg(4326).unwrap()).unwrap();
        assert_eq!(t.transform_3d(10.0, 50.0, 100.0).unwrap(), (10.0, 50.0, 100.0));
    }

    #[test]
    fn names_and_wkt_come_from_the_epsg_table() {
        let s = SpatialRef::from_epsg(2263).unwrap();
        assert_eq!(s.name().unwrap(), "NAD83 / New York Long Island (ftUS)");
        let w = s.to_wkt().unwrap();
        assert!(w.contains("UNIT[\"US survey foot\",0.304800609601219") && w.ends_with("AUTHORITY[\"EPSG\",\"2263\"]]"), "{w}");
        assert_eq!(SpatialRef::from_wkt(&w).epsg, Some(2263));
    }

    /// GDAL's longitude reduction in its Web Mercator inverse (`ogrct.cpp`), one revolution per
    /// step, as terrio computed it before: the reference for [`wrap_longitude`].
    fn gdal_longitude(mut lon: f64) -> f64 {
        use std::f64::consts::PI;
        if lon > PI {
            if lon < PI + 1e-14 {
                lon = PI;
            } else {
                while lon > PI {
                    lon -= 2.0 * PI;
                }
            }
        } else if lon < -PI {
            if lon > -PI - 1e-14 {
                lon = -PI;
            } else {
                while lon < -PI {
                    lon += 2.0 * PI;
                }
            }
        }
        lon
    }

    /// `wrap_longitude(lon)` and GDAL's loop on it, bit for bit.
    fn same_as_gdal(lon: f64) {
        let (ours, gdal) = (wrap_longitude(lon), gdal_longitude(lon));
        assert_eq!(ours.to_bits(), gdal.to_bits(), "longitude {lon:e} ({:#x}): {ours:e}, GDAL {gdal:e}", lon.to_bits());
    }

    #[test]
    fn web_mercator_longitudes_are_gdals_bit_for_bit() {
        const R: f64 = 1.0 / 6378137.0;
        let up = |x: f64, n: usize| (0..n).fold(x, |x, _| x.next_up());
        let down = |x: f64, n: usize| (0..n).fold(x, |x, _| x.next_down());
        // Eastings: the antimeridian and a few ULPs around it, and GDAL's slower cases (10^16 m
        // takes it 2.5 x 10^8 steps).
        let seam = 20037508.342789244;
        let mut eastings = vec![seam, 1e7, 1e10, 1e13, 1e16, 3.14e15, 123456789.25];
        eastings.extend((1..=4).flat_map(|n| [up(seam, n), down(seam, n)]));
        for x in eastings.clone() {
            eastings.push(-x);
        }
        for x in eastings {
            let (lon, lat) = mercator_sphere_inverse(x, 1000.0).unwrap();
            assert_eq!(lon.to_bits(), gdal_longitude(x * R).to_bits(), "easting {x:e}");
            assert_eq!(lat, std::f64::consts::PI / 2.0 - 2.0 * (-1000.0 * R).exp().atan());
        }
        // Longitudes: around each power of two from 4 to 2^24 radians, where 2π is subtracted in
        // another rounding, ±π, exact multiples of 2π (a result of +0), and 4,000 others spread
        // over 2^2 to 2^22 radians.
        let two_pi = 2.0 * std::f64::consts::PI;
        let mut lons = vec![std::f64::consts::PI, 4.0 * std::f64::consts::PI, 1e-300, 0.0];
        for e in 2..=24 {
            let p = f64::from(1u32 << e);
            lons.extend((0..4).flat_map(|n| [up(p, n), down(p, n), up(p + two_pi, n), down(p + 4.0 * two_pi, n)]));
            lons.extend((1..=3).map(|k| p + f64::from(k) * two_pi));
        }
        lons.extend((1..=50).map(|k| f64::from(k) * two_pi));
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..4000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let exponent = 2.0 + 20.0 * (state >> 11) as f64 / (1u64 << 53) as f64;
            lons.push(2f64.powf(exponent));
        }
        for lon in lons {
            same_as_gdal(lon);
            same_as_gdal(-lon);
        }
        // NaN and infinities are refused, as before.
        for (x, y) in [(f64::NAN, 0.0), (f64::INFINITY, 0.0), (f64::NEG_INFINITY, 0.0), (0.0, f64::NAN), (0.0, f64::INFINITY)] {
            assert_eq!(mercator_sphere_inverse(x, y), None, "({x}, {y})");
        }
    }

    #[test]
    fn huge_web_mercator_eastings_return_at_once() {
        // GDAL's loop never ends for the first seven (subtracting 2π leaves their longitude
        // unchanged) and runs 2.5 x 10^10 and 2.5 x 10^12 times for the others: on a thread, so that a
        // loop that does not end fails the test rather than hangs it.
        let stuck = [1e30, -1e30, 1e300, f64::MAX, -f64::MAX, 1e24, -1e24];
        let slow = [1e18, -1e18, 1e20, -1e20];
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for x in stuck.into_iter().chain(slow) {
                let start = std::time::Instant::now();
                let lon = mercator_sphere_inverse(x, 0.0).map(|(lon, _)| lon);
                tx.send((x, lon, start.elapsed())).unwrap();
            }
        });
        let two_pi = 2.0 * std::f64::consts::PI;
        for _ in 0..stuck.len() + slow.len() {
            let (x, lon, took) = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("a huge easting did not return within 5 s");
            assert!(took < std::time::Duration::from_millis(10), "easting {x:e}: {took:?}");
            let lon = lon.unwrap_or_else(|| panic!("easting {x:e}: refused"));
            assert!(lon.abs() <= std::f64::consts::PI, "easting {x:e}: {lon}");
            // Where GDAL's loop never ends: the exact remainder of the longitude modulo 2π.
            let r = (x * (1.0 / 6378137.0)) % two_pi;
            assert!(!stuck.contains(&x) || lon == r || lon == r - two_pi.copysign(r), "easting {x:e}: {lon}, remainder {r}");
        }
    }
}

/// The authority and code of a WKT's first root `AUTHORITY` or `ID` (as GDAL 3.13.3 takes it,
/// whichever keyword comes first), wherever it is among the root's children (WKT 2 can give a
/// `REMARK` after it), but for a compound CRS's.
fn gdal_root_identifier(wkt: &str) -> Option<(String, u32)> {
    let root = wkt1::parse(wkt)?;
    if root.is(keywords::COMPOUND_CRS) {
        return None;
    }
    // The first only: GDAL takes `AUTHORITY["EPSG","abc"]` before an `ID["EPSG",4258]` as the
    // code `abc`, which is no code.
    let id = root.ids().next()?;
    match (id.args.first()?, id.args.get(1)?) {
        (wkt1::Arg::Str(authority), wkt1::Arg::Num(code)) if code.fract() == 0.0 && *code >= 0.0 && *code <= f64::from(u32::MAX) => {
            Some((authority.clone(), *code as u32))
        }
        (wkt1::Arg::Str(authority), wkt1::Arg::Str(code)) => Some((authority.clone(), code.trim().parse().ok()?)),
        _ => None,
    }
}

#[cfg(test)]
mod gdal_proj4_tests {
    /// GDAL 3.13.3 (PROJ 9.9.0) takes a WKT 2's first root identifier, `AUTHORITY` or `ID`:
    /// `osr.SpatialReference().ImportFromWkt(wkt)`, then `GetAuthorityCode(None)` gives 4326 with
    /// `AUTHORITY["EPSG","4326"]` first, 4258 with `ID["EPSG",4258]` first, and `abc` (no code)
    /// with `AUTHORITY["EPSG","abc"]` before `ID["EPSG",4258]`.
    #[test]
    fn the_first_root_identifier_is_gdals() {
        let wgs84 = r#"GEOGCRS["x",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],CS[ellipsoidal,2],AXIS["latitude",north,ANGLEUNIT["degree",0.0174532925199433]],AXIS["longitude",east,ANGLEUNIT["degree",0.0174532925199433]]"#;
        let first = format!(r#"{wgs84},AUTHORITY["EPSG","4326"],ID["EPSG",4258]]"#);
        assert_eq!(super::gdal_root_identifier(&first), Some(("EPSG".into(), 4326)));
        let first = format!(r#"{wgs84},ID["EPSG",4258],AUTHORITY["EPSG","4326"]]"#);
        assert_eq!(super::gdal_root_identifier(&first), Some(("EPSG".into(), 4258)));
        let unreadable = format!(r#"{wgs84},AUTHORITY["EPSG","abc"],ID["EPSG",4258]]"#);
        assert_eq!(super::gdal_root_identifier(&unreadable), None);
    }

    #[test]
    fn a_registry_string_has_nothing_the_table_adds() {
        // What `to_proj4_gdal` inserts is never already there: no EPSG or ESRI code GDAL adds a
        // `+towgs84` to has a `+towgs84` or `+nadgrids` of its own, none it adds a geoid to has
        // `+geoidgrids`.
        let text = crate::epsg::decompress(include_bytes!("../data/gdal_towgs84.tsv.zst"));
        let (mut checked, mut skipped) = (0, Vec::new());
        for line in text.lines() {
            let f: Vec<&str> = line.split('\t').collect();
            let code = f[1].parse().unwrap();
            let s = if f[0] == "EPSG" { crate::SpatialRef::from_epsg(code) } else { crate::SpatialRef::from_esri(code) };
            let Some(proj) = s.ok().and_then(|s| s.to_proj4()) else {
                skipped.push(format!("{}:{code}", f[0]));
                continue;
            };
            // Every row is read into the table.
            assert_eq!(crate::epsg::gdal_additions(f[0], code), Some((f[2], f[3])), "{}:{code}", f[0]);
            assert!(f[2].is_empty() || !(proj.contains("+towgs84=") || proj.contains("+nadgrids=")), "{}:{code} {proj}", f[0]);
            assert!(f[3].is_empty() || !proj.contains("+geoidgrids="), "{}:{code} {proj}", f[0]);
            checked += 1;
        }
        // Every one is in terrio's tables, ESRI:102151 and 102964 too.
        assert!(skipped.is_empty(), "{skipped:?}");
        assert_eq!(checked, 5130);
    }

    #[test]
    fn quoted_values_keep_their_spaces() {
        // `to_proj4_gdal` splits as PROJ strings are read and keeps values as written.
        let tokens = crate::proj_write::tokens(r#"+proj=longlat  +geoidgrids="@my  ""grid"".gtx" +no_defs"#, true);
        assert_eq!(tokens, ["+proj=longlat", r#"+geoidgrids="@my  ""grid"".gtx""#, "+no_defs"]);
        // DMS seconds are no quoted value.
        let tokens = crate::proj_write::tokens(r#"+proj=longlat +pm=2d20'14.025"E +type=crs"#, true);
        assert_eq!(tokens, ["+proj=longlat", r#"+pm=2d20'14.025"E"#, "+type=crs"]);
    }
}
