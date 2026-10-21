//! Coordinate reference systems and transformations between them.
//!
//! Coordinates are always in traditional GIS order: x/easting/longitude first, then
//! y/northing/latitude, whatever axis order the CRS's authority defines.
//!
//! ```no_run
//! use terrio::crs::{SpatialRef, Transformer};
//!
//! let nad27 = SpatialRef::parse("EPSG:4267")?;
//! let utm = SpatialRef::parse("+proj=utm +zone=13 +datum=WGS84")?;
//! println!("{} -> {}", nad27.label(), utm.label());
//! let t = Transformer::new(&nad27, &utm)?;
//! let (x, y) = t.transform(-105.5, 39.5)?;
//! let bounds = t.transform_bounds((-106.0, 39.0, -105.0, 40.0))?;
//! println!("{}", utm.to_wkt().unwrap_or_default());
//! # Ok::<(), terrio::Error>(())
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
//! use terrio::crs::{SpatialRef, TransformOptions, Transformer};
//!
//! // Ellipsoidal heights (WGS 84) to EGM96 heights.
//! let t = Transformer::new(&SpatialRef::parse("EPSG:4979")?, &SpatialRef::parse("EPSG:4326+5773")?)?;
//! let (lon, lat, h) = t.transform_3d(-100.0, 40.0, 100.0)?;
//! // ITRF2014 coordinates at epoch 2010.0 to GDA2020.
//! let opts = TransformOptions { source_coordinate_epoch: Some(2010.0), ..Default::default() };
//! let t = Transformer::with_options(&SpatialRef::parse("EPSG:7912")?, &SpatialRef::parse("EPSG:7843")?, &opts)?;
//! let (lon, lat, h) = t.transform_3d(135.0, -25.0, 0.0)?;
//! # Ok::<(), terrio::Error>(())
//! ```

use crate::{Error, Result};
use serde::Serialize;
use wbprojection as wbp;

mod datum;
mod def;
mod epsg;
mod grid;
mod math;
mod params;
mod projections;
mod rotation;
mod route;
mod wkt1;
mod wkt2;

pub(crate) use def::Def;
pub use grid::{register_grid, reset_grid_cache, set_grid_search_paths, set_network_enabled};

/// Starts loading terrio's EPSG table in the background, for work that will need it once it has
/// read its input.
pub(crate) fn prefetch() {
    epsg::prefetch();
}

/// The WKT GDAL writes for an EPSG code, from terrio's EPSG table.
pub(crate) fn epsg_wkt(code: u32) -> Option<&'static str> {
    epsg::get(code).map(|e| e.wkt.as_str())
}

/// A coordinate reference system, known by an EPSG code, a WKT definition, a PROJ string or a
/// combination of them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SpatialRef {
    /// The EPSG code, when the CRS has (or was identified as having) one.
    pub epsg: Option<u32>,
    /// The WKT definition.
    pub wkt: Option<String>,
    /// The PROJ string definition.
    pub proj4: Option<String>,
}

impl SpatialRef {
    /// A CRS from an EPSG code. Fails when the code is not in the EPSG registry terrio carries.
    pub fn from_epsg(code: u32) -> Result<Self> {
        let wkt = match epsg::get(code) {
            Some(e) => e.wkt.clone(),
            None => match (epsg::compound(code), epsg::vertical(code)) {
                (Some(c), _) => c.wkt.clone(),
                (None, Some(v)) => v.wkt.clone(),
                _ => wbp::to_ogc_wkt(code).map_err(|_| Error::Crs(format!("unknown EPSG code {code}")))?,
            },
        };
        Ok(Self { epsg: Some(code), wkt: Some(wkt), proj4: None })
    }

    /// A CRS from a WKT definition (WKT1 or WKT2). The EPSG code is identified when possible.
    pub fn from_wkt(wkt: &str) -> Self {
        // A WKT 1 extended with its PROJ string (`EXTENSION["PROJ4", ...]`, as GDAL writes CRSs
        // WKT 1 cannot describe) is that PROJ string, as GDAL reads it.
        if let Some(proj4) = proj4_extension(wkt)
            && let Ok(own) = Def::from_proj(&proj4)
        {
            // Its EPSG code: declared, or one whose definition converts as the PROJ string does.
            let epsg = declared_epsg(wkt).or_else(|| identify_epsg(wkt).filter(|code| is_registered_as(&own, *code)));
            return Self { epsg, wkt: Some(wkt.to_string()), proj4: Some(proj4) };
        }
        let epsg = if def::split_compound(wkt).is_some() { declared_epsg(wkt) } else { identify_epsg(wkt) };
        Self { epsg, wkt: Some(wkt.to_string()), proj4: None }
    }

    /// A compound CRS from the EPSG codes of a horizontal and a vertical CRS, as GDAL builds
    /// `EPSG:4326+5773`.
    pub fn compound(horizontal: u32, vertical: u32) -> Result<Self> {
        let h = Self::from_epsg(horizontal)?;
        let v = epsg::vertical(vertical).ok_or_else(|| Error::Crs(format!("EPSG:{vertical} is not a vertical CRS")))?;
        let name = format!("{} + {}", h.name().unwrap_or_default(), v.name);
        let wkt = format!("COMPD_CS[\"{name}\",{},{}]", h.to_wkt().unwrap_or_default(), v.wkt);
        Ok(Self { epsg: None, wkt: Some(wkt), proj4: None })
    }

    /// A CRS from a PROJ string such as `+proj=utm +zone=33 +datum=WGS84`.
    pub fn from_proj4(proj4: &str) -> Result<Self> {
        let p = params::Params::parse(proj4);
        if let Some(code) = p.str("init").and_then(|i| i.to_ascii_lowercase().strip_prefix("epsg:").and_then(|c| c.parse().ok())) {
            return Self::from_epsg(code);
        }
        // Checks that terrio can transform with it.
        Def::from_proj(proj4)?;
        Ok(Self { epsg: None, wkt: proj_to_wkt(proj4), proj4: Some(proj4.to_string()) })
    }

    /// Parses a user-supplied CRS: `EPSG:4326`, `4326`, `urn:ogc:def:crs:EPSG::4326`, an OGC URL,
    /// `OGC:CRS84`, `WGS84`, a WKT definition, a PROJ string, or the path of a file holding one
    /// of these (such as a shapefile's `.prj`).
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
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
        // `EPSG:4326+5773`: a horizontal and a vertical CRS.
        if let Some(rest) = upper.strip_prefix("EPSG:")
            && let Some((h, v)) = rest.split_once('+')
            && let (Ok(h), Ok(v)) = (h.trim().parse::<u32>(), v.trim().parse::<u32>())
        {
            return Self::compound(h, v);
        }
        if looks_like_wkt(s) {
            return Ok(Self::from_wkt(s));
        }
        if let Some(code) = wbp::epsg_from_srs_reference(s) {
            return Self::from_epsg(code);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Ok(text) = std::fs::read_to_string(s) {
            return Self::parse(&text);
        }
        Err(Error::Crs(format!("cannot parse '{s}'")))
    }

    /// The WKT of the geographic CRS this CRS is based on (its own when geographic): from the
    /// EPSG table for an EPSG CRS, else the `GEOGCS` node of its WKT 1. `None` when neither is
    /// known.
    pub(crate) fn geographic_wkt(&self) -> Option<String> {
        if let Some(e) = self.epsg.and_then(epsg::get)
            && let Some(g) = epsg::get(e.geog)
        {
            return Some(g.wkt.clone());
        }
        let wkt = self.to_wkt()?;
        let start = wkt.find("GEOGCS[")?;
        let mut depth = 0usize;
        for (i, c) in wkt[start..].char_indices() {
            match c {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(wkt[start..=start + i].to_string());
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Whether nothing is known about the CRS.
    pub fn is_unknown(&self) -> bool {
        self.epsg.is_none() && self.wkt.is_none() && self.proj4.is_none()
    }

    /// The WKT definition, built from the EPSG code or PROJ string when no WKT is stored.
    pub fn to_wkt(&self) -> Option<String> {
        if let Some(w) = &self.wkt {
            return Some(w.clone());
        }
        if let Some(code) = self.epsg {
            return epsg::get(code)
                .map(|e| e.wkt.clone())
                .or_else(|| epsg::compound(code).map(|c| c.wkt.clone()))
                .or_else(|| epsg::vertical(code).map(|v| v.wkt.clone()))
                .or_else(|| wbp::to_ogc_wkt(code).ok());
        }
        self.proj4.as_deref().and_then(proj_to_wkt)
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
        let registry = self.epsg.and_then(|code| wbp::to_ogc_wkt(code).ok());
        registry
            .as_deref()
            .and_then(wkt_name)
            .or_else(|| self.to_wkt().as_deref().and_then(wkt_name))
            .or_else(|| self.epsg.and_then(wbp::epsg::epsg_info).map(|i| i.name.to_string()))
    }

    /// A short label: `EPSG:4326` when the code is known, otherwise the name.
    pub fn label(&self) -> String {
        match (self.epsg, self.name()) {
            (Some(code), _) => format!("EPSG:{code}"),
            (None, Some(name)) => name,
            (None, None) => "unknown".into(),
        }
    }

    /// Whether this is a geographic (longitude/latitude) CRS.
    pub fn is_geographic(&self) -> bool {
        match Def::new(self) {
            Ok(d) => d.is_geographic(),
            Err(_) => self.to_wbp().map(|c| c.is_geographic()).unwrap_or(false),
        }
    }

    /// Whether this is a rotated-pole grid (PROJ's `+proj=ob_tran`, CF's
    /// `rotated_latitude_longitude`): its coordinates are in degrees but are not longitudes and
    /// latitudes, so it is not geographic.
    pub fn is_rotated_pole(&self) -> bool {
        self.epsg.is_none() && Def::new(self).is_ok_and(|d| d.is_rotated_pole())
    }

    /// Whether coordinates are in degrees: a geographic CRS or a rotated-pole grid.
    pub(crate) fn is_angular(&self) -> bool {
        self.is_geographic() || self.is_rotated_pole()
    }

    /// Whether both describe the same CRS, as far as can be told.
    pub fn same_as(&self, other: &SpatialRef) -> bool {
        match (self.epsg, other.epsg) {
            (Some(a), Some(b)) => a == b,
            _ => self.to_wkt().is_some() && self.to_wkt() == other.to_wkt(),
        }
    }

    /// Converts to the projection engine's CRS.
    pub(crate) fn to_wbp(&self) -> Result<wbp::Crs> {
        if let Some(code) = self.epsg
            && let Ok(c) = wbp::Crs::from_epsg(code)
        {
            return Ok(c);
        }
        // wbprojection would read a rotated-pole grid as its geographic base CRS.
        if self.is_rotated_pole() {
            return Err(Error::Crs("a rotated-pole CRS has no wbprojection equivalent".into()));
        }
        if let Some(w) = &self.wkt {
            return Ok(wbp::Crs::from_wkt(w)?);
        }
        if let Some(p) = &self.proj4 {
            return Ok(wbp::from_proj_string(p)?);
        }
        Err(Error::Crs("the CRS is unknown".into()))
    }

    pub(crate) fn from_raster_crs(c: &wbraster::CrsInfo) -> Self {
        let mut s = Self { epsg: c.epsg, wkt: c.wkt.clone(), proj4: c.proj4.clone() };
        // A WKT made from a PROJ string is not identified: it can describe less than the PROJ
        // string (the registry's loose matches took a Robinson on another meridian for ESRI's).
        if s.epsg.is_none()
            && s.proj4.is_none()
            && let Some(w) = &s.wkt
        {
            s.epsg = identify_epsg(w);
        }
        s
    }

    /// The CRS as the raster model stores it.
    pub fn to_crs_info(&self) -> wbraster::CrsInfo {
        wbraster::CrsInfo { epsg: self.epsg, wkt: self.to_wkt(), proj4: self.proj4.clone() }
    }

    /// The CRS a vector layer declares.
    pub fn from_layer(layer: &wbvector::Layer) -> Self {
        let mut s = Self { epsg: layer.crs_epsg(), wkt: layer.crs_wkt().map(str::to_string), proj4: None };
        if s.epsg.is_none()
            && let Some(w) = &s.wkt
        {
            s.epsg = identify_epsg(w);
        }
        s
    }

    /// Sets a vector layer's CRS to this one.
    pub fn apply_to_layer(&self, layer: &mut wbvector::Layer) {
        layer.set_crs_epsg(self.epsg);
        layer.set_crs_wkt(self.to_wkt());
    }
}

/// The WKT 1 of a PROJ string. When the WKT describes less than the PROJ string (a
/// south-orientated transverse Mercator, an oblique Mercator variant), it is extended with the
/// PROJ string itself (`EXTENSION["PROJ4", ...]`), as GDAL writes it.
fn proj_to_wkt(p: &str) -> Option<String> {
    let extension = format!("EXTENSION[\"PROJ4\",\"{}\"]", p.trim().replace('"', ""));
    // A rotated-pole grid: its base geographic CRS extended with the PROJ string, as GDAL 2 wrote
    // it (wbprojection does not know `ob_tran`). Unnamed and without its code, so as not to be
    // taken for the base CRS.
    if Def::from_proj(p).ok()?.is_rotated_pole() {
        const DATUM: [&str; 10] = ["+datum=", "+ellps=", "+a=", "+b=", "+rf=", "+f=", "+R=", "+towgs84=", "+nadgrids=", "+pm="];
        let datum: Vec<&str> = p.split_whitespace().filter(|t| DATUM.iter().any(|d| t.starts_with(d))).collect();
        let geogcs = wbp::crs_to_wkt(&wbp::from_proj_string(&format!("+proj=longlat {}", datum.join(" "))).ok()?);
        let body = geogcs.trim_end().strip_prefix("GEOGCS[\"")?;
        let body = body[body.find('"')? + 1..].strip_suffix(']')?;
        // The root's own AUTHORITY: the last node, closed by the root's bracket.
        let body = match body.rfind(",AUTHORITY[") {
            Some(i) if body[i..].matches(']').count() == 1 => &body[..i],
            _ => body,
        };
        return Some(format!("GEOGCS[\"unnamed\"{body},{extension}]"));
    }
    if let Ok(crs) = wbp::from_proj_string(&mercator_lat_ts_as_k(p)) {
        let wkt = wbp::crs_to_wkt(&crs);
        // Faithful when terrio reads it back (identifying its EPSG code, its geographic CRS) as
        // the PROJ string: the same conversion on the same datum. A Bessel stereographic without
        // TOWGS84 is not Amersfoort / RD New, which shifts its datum.
        let faithful = Def::from_proj(p)
            .is_ok_and(|own| Def::new(&SpatialRef::from_wkt(&wkt)).is_ok_and(|w| w.same_conversion(&own) && w.same_datum(&own)));
        if faithful {
            return Some(wkt);
        }
        return Some(format!("{},{extension}]", wkt.trim_end().strip_suffix(']')?));
    }
    // A projection wbprojection does not know: GDAL's `custom_proj4`, on the PROJ string's datum.
    Def::from_proj(p).ok()?;
    const DATUM: [&str; 9] = ["+datum=", "+ellps=", "+a=", "+b=", "+rf=", "+f=", "+R=", "+towgs84=", "+nadgrids="];
    let datum: Vec<&str> = p.split_whitespace().filter(|t| DATUM.iter().any(|d| t.starts_with(d))).collect();
    let geogcs = wbp::crs_to_wkt(&wbp::from_proj_string(&format!("+proj=longlat {}", datum.join(" "))).ok()?);
    Some(format!("PROJCS[\"unknown\",{geogcs},PROJECTION[\"custom_proj4\"],UNIT[\"metre\",1],{extension}]"))
}

/// Whether the EPSG CRS `code` is the CRS of `own`: the same conversion on the same datum.
fn is_registered_as(own: &Def, code: u32) -> bool {
    Def::new(&SpatialRef { epsg: Some(code), wkt: None, proj4: None }).is_ok_and(|r| own.same_conversion(&r) && own.same_datum(&r))
}

/// The PROJ string of a WKT 1's root `EXTENSION["PROJ4", ...]`.
fn proj4_extension(wkt: &str) -> Option<String> {
    let root = wkt1::parse(wkt)?;
    let ext = root.children("EXTENSION").find(|n| n.name().is_some_and(|k| k.eq_ignore_ascii_case("PROJ4")))?;
    match ext.args.get(1)? {
        wkt1::Arg::Str(p) if p.trim_start().starts_with('+') => Some(p.trim().to_string()),
        _ => None,
    }
}

/// A PROJ string whose Mercator standard parallel (`+lat_ts`) is given as the scale at the
/// equator it implies (`+k`): wbprojection's Mercator has no standard parallel, so its WKT would
/// otherwise describe the equator's Mercator (EPSG:3395's projection).
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

/// The semi-major axis of the first `SPHEROID` or `ELLIPSOID` of a WKT definition.
fn semi_major_axis(wkt: &str) -> Option<f64> {
    let upper = wkt.to_ascii_uppercase();
    let start = upper.find("SPHEROID[").or_else(|| upper.find("ELLIPSOID["))?;
    let body = &wkt[start..];
    let rest = &body[body.find('[')? + 1..];
    // Skip the quoted name, then read the number after it.
    let after_name = rest.strip_prefix('"').and_then(|r| r.find('"').map(|i| &r[i + 1..]))?;
    after_name.trim_start_matches([',', ' ']).split([',', ']']).next()?.trim().parse().ok()
}

/// Identifies the EPSG code of a WKT definition. Among equally good matches, prefers the base
/// datum over its later realizations (NAD83 over NAD83(HARN) and NAD83(2011)) unless the WKT names
/// one, then the lowest code: what GDAL picks for ESRI `.prj` files.
pub(crate) fn identify_epsg(wkt: &str) -> Option<u32> {
    if let Some(code) = declared_epsg(wkt) {
        return Some(code);
    }
    let report = wbp::identify_epsg_from_wkt_report(wkt, wbp::EpsgIdentifyPolicy::Lenient)?;
    // The registry matches loosely: keep a code only when its definition converts coordinates
    // as the WKT does (GDAL leaves a custom CRS without code).
    let own = Def::wkt_conversion(wkt);
    let fits = |code: u32| {
        // A projection terrio cannot compare: at least the same ellipsoid (a sinusoidal
        // projection on MODIS's sphere is not ESRI's World Sinusoidal on WGS 84).
        let Some(own) = &own else {
            let candidate = wbp::to_ogc_wkt(code).ok();
            return match (semi_major_axis(wkt), candidate.as_deref().and_then(semi_major_axis)) {
                (Some(a), Some(b)) => (a - b).abs() < 1e-3,
                _ => true,
            };
        };
        let registered = SpatialRef { epsg: Some(code), wkt: None, proj4: None };
        Def::new(&registered).is_ok_and(|d| own.same_conversion(&d))
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
    let upper = wkt.to_ascii_uppercase();
    let wkt_names_realization = REALIZATIONS.iter().any(|r| upper.contains(r));
    let names_realization = |code: u32| {
        let name = wbp::epsg::epsg_info(code).map(|i| i.name.to_ascii_uppercase()).unwrap_or_default();
        REALIZATIONS.iter().any(|r| name.contains(r))
    };
    report
        .top_candidates
        .iter()
        .filter(|c| c.total_score >= best && fits(c.code))
        .min_by_key(|c| (!wkt_names_realization && names_realization(c.code), c.code))
        .map(|c| c.code)
        .or(report.resolved_code.filter(|c| fits(*c)))
}

/// A WKT definition without its `AUTHORITY[...]` nodes.
fn strip_authorities(wkt: &str) -> String {
    let mut out = wkt.to_string();
    while let Some(start) = out.find(",AUTHORITY[") {
        let Some(len) = out[start..].find(']') else { break };
        out.replace_range(start..start + len + 1, "");
    }
    out
}

/// The EPSG code a WKT definition declares for itself: the `AUTHORITY["EPSG","4269"]` (WKT 1) or
/// `ID["EPSG",4269]` (WKT 2) closing its outermost node, not those of its datum or units.
fn declared_epsg(wkt: &str) -> Option<u32> {
    let body = wkt.trim_end().strip_suffix(']')?.trim_end();
    // The last child node must be the identifier itself, not a node holding one.
    let body = body.strip_suffix(']')?;
    if body.ends_with(']') {
        return None;
    }
    let start = body.rfind('[')?;
    let keyword = body[..start].trim_end().rsplit([',', '[']).next()?.trim().to_ascii_uppercase();
    if keyword != "AUTHORITY" && keyword != "ID" {
        return None;
    }
    let mut parts = body[start + 1..].split(',').map(|p| p.trim().trim_matches('"'));
    if !parts.next()?.eq_ignore_ascii_case("EPSG") {
        return None;
    }
    parts.next()?.parse().ok()
}

fn looks_like_wkt(s: &str) -> bool {
    let head: String = s.chars().take_while(|c| *c != '[' && *c != '(').collect::<String>().trim().to_ascii_uppercase();
    matches!(
        head.as_str(),
        "GEOGCS"
            | "PROJCS"
            | "GEOCCS"
            | "COMPD_CS"
            | "GEOGCRS"
            | "GEOGRAPHICCRS"
            | "PROJCRS"
            | "PROJECTEDCRS"
            | "GEODCRS"
            | "GEODETICCRS"
            | "BOUNDCRS"
            | "COMPOUNDCRS"
            | "VERTCRS"
            | "VERT_CS"
    )
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
}

/// Transforms coordinates from one CRS to another, as GDAL does: linear and angular units, axis
/// order and prime meridians as the CRSs define them, and, when the datums differ, the datum
/// transformation GDAL (PROJ) picks for each point among those it lists for the pair, or chains
/// through an intermediate datum such as WGS 84. Datum shift grids are used when they can be
/// found, as GDAL uses them; heights of geographic 3D and compound CRSs go through geoid
/// models; time-dependent transformations use the coordinate epoch (see [`TransformOptions`]).
pub struct Transformer {
    src: Def,
    dst: Def,
    identity: bool,
    route: route::Route,
    /// The time of time-dependent transformations, as GDAL sets it from the coordinate epochs.
    time: Option<f64>,
    /// The height transformation, between CRSs that both have heights.
    heights: Option<Box<Heights>>,
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

/// The definition of an EPSG geographic 3D CRS.
fn def_of(code: u32) -> Option<Def> {
    Def::new(&SpatialRef { epsg: Some(code), wkt: None, proj4: None }).ok()
}

impl Heights {
    fn middle(&self, i: usize, j: usize) -> std::sync::Arc<route::Route> {
        let mut m = self.middle.lock().unwrap_or_else(|e| e.into_inner());
        m.entry((i, j)).or_insert_with(|| std::sync::Arc::new(route::Route::new(&self.src.defs[i], &self.dst.defs[j]))).clone()
    }
}

/// Picks and runs a geoid model for a point (degrees for the areas), `reverse` from heights to
/// ellipsoidal heights.
fn run_model(ops: &[std::sync::Arc<datum::Op>], p: datum::Point, reverse: bool) -> Option<datum::Point> {
    let (lon, lat) = (p.lon.to_degrees(), p.lat.to_degrees());
    datum::transform_with(ops, |op, _| op.contains(lon, lat), |i| datum::run_op(&ops[i], p, reverse, true))
}

impl Transformer {
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
        let (s, d) = (Def::new(src)?, Def::new(dst)?);
        let time = match (s.is_dynamic(), d.is_dynamic(), opts.source_coordinate_epoch, opts.target_coordinate_epoch) {
            (true, false, Some(e), _) => Some(e),
            (false, true, _, Some(e)) => Some(e),
            (true, true, Some(e), Some(_)) => Some(e),
            _ => None,
        };
        let route = route::Route::new(&s, &d);
        let heights = (s.has_height() && d.has_height()).then(|| {
            Box::new(Heights {
                src: Side::new(&s, true),
                dst: Side::new(&d, false),
                middle: std::sync::Mutex::new(std::collections::HashMap::new()),
            })
        });
        Ok(Self { identity: src.same_as(dst), src: s, dst: d, route, time, heights })
    }

    /// Transforms one point.
    pub fn transform(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        if self.identity {
            return Ok((x, y));
        }
        let (x, y, _) = self.transform_2d(x, y, 0.0, self.time)?;
        Ok((x, y))
    }

    /// The 2D transformation of a point, with its height kept (or, for transformations PROJ runs
    /// without keeping heights, transformed).
    fn transform_2d(&self, x: f64, y: f64, z: f64, t: Option<f64>) -> Result<(f64, f64, f64)> {
        let fail = || Error::Crs(format!("cannot transform ({x}, {y})"));
        let (lon, lat) = self.src.unproject(x, y).ok_or_else(fail)?;
        let p = datum::Point { lon, lat, h: z, t };
        let q = self.route.apply(&self.src, x, y, p, false).ok_or_else(fail)?;
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

    fn transform_at(&self, x: f64, y: f64, z: f64, t: Option<f64>) -> Result<(f64, f64, f64)> {
        if self.identity {
            return Ok((x, y, z));
        }
        let Some(h) = &self.heights else { return self.transform_2d(x, y, z, t) };
        // Compound CRSs with the same vertical CRS: heights only change unit.
        if let (Some(a), Some(b)) = (self.src.vertical(), self.dst.vertical())
            && a.code.is_some()
            && a.code == b.code
        {
            let (tx, ty, _) = self.transform_2d(x, y, 0.0, t)?;
            return Ok((tx, ty, b.height_of(a.to_metres(z))));
        }
        let fail = || Error::Crs(format!("cannot transform ({x}, {y}, {z})"));
        let (lon, lat) = self.src.unproject(x, y).ok_or_else(fail)?;
        let p0 = datum::Point { lon, lat, h: self.src.vertical().map_or(z, |v| v.to_metres(z)), t };
        // To ellipsoidal heights, on the source CRS or a geoid model's CRS.
        let mut start = None;
        for (i, ops) in &h.src.models {
            let q = match &h.src.routes[*i] {
                Some(r) => r.apply(&self.src, x, y, p0, false),
                None => Some(p0),
            };
            if let Some(r) = q.and_then(|q| run_model(ops, q, true)) {
                start = Some((*i, r));
                break;
            }
        }
        // No geoid model: heights are kept (PROJ's ballpark vertical transformation).
        if start.is_none() && self.src.vertical().is_some_and(|v| !v.ballpark) {
            return Err(fail());
        }
        let (i, p) = start.unwrap_or((0, p0));
        let (sx, sy) = if i == 0 { (x, y) } else { (p.lon.to_degrees(), p.lat.to_degrees()) };
        // From ellipsoidal heights to the target.
        for (j, ops) in &h.dst.models {
            let Some(q) = h.middle(i, *j).apply(&h.src.defs[i], sx, sy, p, true) else { continue };
            let Some(r) = run_model(ops, q, false) else { continue };
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
        let q = h.middle(i, 0).apply(&h.src.defs[i], sx, sy, p, true).ok_or_else(fail)?;
        let (tx, ty) = self.dst.project(q.lon, q.lat).ok_or_else(fail)?;
        Ok((tx, ty, self.dst.vertical().map_or(q.h, |v| v.height_of(q.h))))
    }

    /// Transforms a bounding box `(min_x, min_y, max_x, max_y)`, densifying its edges so that
    /// curved edges in the target CRS are covered. Points that fail to transform are skipped.
    pub fn transform_bounds(&self, b: (f64, f64, f64, f64)) -> Result<(f64, f64, f64, f64)> {
        const STEPS: usize = 20;
        let (mut x0, mut y0, mut x1, mut y1) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
        for i in 0..=STEPS {
            let t = i as f64 / STEPS as f64;
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
        Ok((x0, y0, x1, y1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn declared_codes_win() {
        let nad83 = r#"GEOGCS["NAD83",DATUM["North_American_Datum_1983",SPHEROID["GRS 1980",6378137,298.257222101,AUTHORITY["EPSG","7019"]],AUTHORITY["EPSG","6269"]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],AXIS["Latitude",NORTH],AXIS["Longitude",EAST],AUTHORITY["EPSG","4269"]]"#;
        assert_eq!(declared_epsg(nad83), Some(4269));
        assert_eq!(SpatialRef::from_wkt(nad83).epsg, Some(4269));
        assert_eq!(declared_epsg(r#"GEOGCRS["WGS 84",CS[ellipsoidal,2],ID["EPSG",4326]]"#), Some(4326));
        assert_eq!(declared_epsg(r#"GEOGCS["x",DATUM["d",SPHEROID["s",1,2]],UNIT["degree",0.01745,AUTHORITY["EPSG","9122"]]]"#), None);
        // Codes of a datum or prime meridian are not the CRS's (GDAL wrote this one in a
        // GeoPackage, from a GeoParquet file's CRS).
        let wgs84 = r#"GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563],AUTHORITY["EPSG","6326"]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["Degree",0.0174532925199433],AXIS["Longitude",EAST],AXIS["Latitude",NORTH]]"#;
        assert_eq!(identify_epsg(wgs84), Some(4326));
    }

    #[test]
    fn parses_common_forms() {
        for s in ["EPSG:4326", "4326", "urn:ogc:def:crs:EPSG::4326", "http://www.opengis.net/def/crs/EPSG/0/4326", "OGC:CRS84"] {
            assert_eq!(SpatialRef::parse(s).unwrap().epsg, Some(4326), "{s}");
        }
        let wkt = wbp::to_ogc_wkt(32633).unwrap();
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
    fn compound_crss_parse_as_in_gdal() {
        let c = SpatialRef::parse("EPSG:4326+5773").unwrap();
        assert_eq!(c.epsg, None);
        assert_eq!(c.name().unwrap(), "WGS 84 + EGM96 height");
        assert!(c.to_wkt().unwrap().starts_with("COMPD_CS[\"WGS 84 + EGM96 height\",GEOGCS[\"WGS 84\""));
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
}
