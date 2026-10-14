//! Coordinate reference systems and transformations between them.
//!
//! Coordinates are always in traditional GIS order: x/easting/longitude first, then
//! y/northing/latitude, whatever axis order the CRS's authority defines.

use crate::{Error, Result};
use serde::Serialize;
use wbprojection as wbp;

mod datum;
mod def;
mod epsg;
mod math;
mod params;
mod projections;
mod route;
mod wkt1;

pub(crate) use def::Def;

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
            None => wbp::to_ogc_wkt(code).map_err(|_| Error::Crs(format!("unknown EPSG code {code}")))?,
        };
        Ok(Self { epsg: Some(code), wkt: Some(wkt), proj4: None })
    }

    /// A CRS from a WKT definition (WKT1 or WKT2). The EPSG code is identified when possible.
    pub fn from_wkt(wkt: &str) -> Self {
        Self { epsg: identify_epsg(wkt), wkt: Some(wkt.to_string()), proj4: None }
    }

    /// A CRS from a PROJ string such as `+proj=utm +zone=33 +datum=WGS84`.
    pub fn from_proj4(proj4: &str) -> Result<Self> {
        let p = params::Params::parse(proj4);
        if let Some(code) = p.str("init").and_then(|i| i.to_ascii_lowercase().strip_prefix("epsg:").and_then(|c| c.parse().ok())) {
            return Self::from_epsg(code);
        }
        // Checks that terrio can transform with it.
        Def::from_proj(proj4)?;
        let wkt = wbp::from_proj_string(proj4).ok().map(|c| wbp::crs_to_wkt(&c));
        Ok(Self { epsg: None, wkt, proj4: Some(proj4.to_string()) })
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
            return epsg::get(code).map(|e| e.wkt.clone()).or_else(|| wbp::to_ogc_wkt(code).ok());
        }
        self.proj4.as_deref().and_then(|p| wbp::from_proj_string(p).ok()).map(|c| wbp::crs_to_wkt(&c))
    }

    /// The CRS's name, such as `WGS 84 / UTM zone 33N`.
    pub fn name(&self) -> Option<String> {
        // The registry's name first: a stored WKT may carry another vendor's name (ESRI `.prj`).
        if let Some(e) = self.epsg.and_then(epsg::get) {
            return Some(e.name.clone());
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
        if s.epsg.is_none()
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

impl std::fmt::Display for SpatialRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label())
    }
}

/// Datum realizations, which a WKT definition names when it means them.
const REALIZATIONS: &[&str] = &["2011", "HARN", "NSRS", "CSRS", "CORS", "MA11", "PA11", "NAD83(", "ITRF", "GDA2020", "ETRF"];

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
        let Some(own) = &own else { return true };
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

/// Transforms coordinates from one CRS to another, as GDAL does: linear and angular units, axis
/// order and prime meridians as the CRSs define them, and, when the datums differ, the datum
/// transformation GDAL (PROJ) picks for each point among those it lists for the pair, or chains
/// through an intermediate datum such as WGS 84.
pub struct Transformer {
    src: Def,
    dst: Def,
    identity: bool,
    route: route::Route,
}

impl Transformer {
    /// A transformer from `src` to `dst`.
    pub fn new(src: &SpatialRef, dst: &SpatialRef) -> Result<Self> {
        let (s, d) = (Def::new(src)?, Def::new(dst)?);
        let route = route::Route::new(&s, &d);
        Ok(Self { src: s, dst: d, identity: src.same_as(dst), route })
    }

    /// Transforms one point.
    pub fn transform(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        if self.identity {
            return Ok((x, y));
        }
        let fail = || Error::Crs(format!("cannot transform ({x}, {y})"));
        let (lon, lat) = self.src.unproject(x, y).ok_or_else(fail)?;
        let (lon, lat) = self.route.apply(&self.src, &self.dst, x, y, lon, lat);
        self.dst.project(lon, lat).ok_or_else(fail)
    }

    /// Transforms a point with a height, which is kept (as GDAL does between 2D CRSs).
    pub fn transform_3d(&self, x: f64, y: f64, z: f64) -> Result<(f64, f64, f64)> {
        let (x, y) = self.transform(x, y)?;
        Ok((x, y, z))
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

    #[test]
    fn names_and_wkt_come_from_the_epsg_table() {
        let s = SpatialRef::from_epsg(2263).unwrap();
        assert_eq!(s.name().unwrap(), "NAD83 / New York Long Island (ftUS)");
        let w = s.to_wkt().unwrap();
        assert!(w.contains("UNIT[\"US survey foot\",0.304800609601219") && w.ends_with("AUTHORITY[\"EPSG\",\"2263\"]]"), "{w}");
        assert_eq!(SpatialRef::from_wkt(&w).epsg, Some(2263));
    }
}
