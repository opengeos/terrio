//! Coordinate reference systems and transformations between them.
//!
//! Coordinates are always in traditional GIS order: x/easting/longitude first, then
//! y/northing/latitude, whatever axis order the CRS's authority defines.

use crate::{Error, Result};
use serde::Serialize;
use wbprojection as wbp;

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
        let wkt = wbp::to_ogc_wkt(code).map_err(|_| Error::Crs(format!("unknown EPSG code {code}")))?;
        Ok(Self { epsg: Some(code), wkt: Some(wkt), proj4: None })
    }

    /// A CRS from a WKT definition (WKT1 or WKT2). The EPSG code is identified when possible.
    pub fn from_wkt(wkt: &str) -> Self {
        Self { epsg: identify_epsg(wkt), wkt: Some(wkt.to_string()), proj4: None }
    }

    /// A CRS from a PROJ string such as `+proj=utm +zone=33 +datum=WGS84`.
    pub fn from_proj4(proj4: &str) -> Result<Self> {
        let crs = wbp::from_proj_string(proj4)?;
        let epsg = wbp::identify_epsg_from_crs(&crs);
        Ok(Self { epsg, wkt: Some(wbp::crs_to_wkt(&crs)), proj4: Some(proj4.to_string()) })
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
            return wbp::to_ogc_wkt(code).ok();
        }
        self.proj4.as_deref().and_then(|p| wbp::from_proj_string(p).ok()).map(|c| wbp::crs_to_wkt(&c))
    }

    /// The CRS's name, such as `WGS 84 / UTM zone 33N`.
    pub fn name(&self) -> Option<String> {
        // The registry's name first: a stored WKT may carry another vendor's name (ESRI `.prj`).
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
        self.to_wbp().map(|c| c.is_geographic()).unwrap_or(false)
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

    pub(crate) fn to_crs_info(&self) -> wbraster::CrsInfo {
        wbraster::CrsInfo { epsg: self.epsg, wkt: self.to_wkt(), proj4: self.proj4.clone() }
    }

    pub(crate) fn from_layer(layer: &wbvector::Layer) -> Self {
        let mut s = Self { epsg: layer.crs_epsg(), wkt: layer.crs_wkt().map(str::to_string), proj4: None };
        if s.epsg.is_none()
            && let Some(w) = &s.wkt
        {
            s.epsg = identify_epsg(w);
        }
        s
    }

    pub(crate) fn apply_to_layer(&self, layer: &mut wbvector::Layer) {
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
    let report = wbp::identify_epsg_from_wkt_report(wkt, wbp::EpsgIdentifyPolicy::Lenient)?;
    if report.used_embedded_epsg || !report.ambiguous {
        return report.resolved_code;
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
        .filter(|c| c.total_score >= best)
        .min_by_key(|c| (!wkt_names_realization && names_realization(c.code), c.code))
        .map(|c| c.code)
        .or(report.resolved_code)
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

/// Transforms coordinates from one CRS to another.
pub struct Transformer {
    src: wbp::Crs,
    dst: wbp::Crs,
    identity: bool,
    /// Whether the datum change is a null ("ballpark") transformation, as PROJ uses for GDAL.
    ballpark: bool,
}

/// Datums that PROJ (and so GDAL and `ogr2ogr`) transforms between with a null transformation
/// by default: NAD83 and WGS 84 (EPSG transformation 1188 and PROJ's ballpark). wbprojection
/// applies a Helmert shift of about a metre between them instead.
const NULL_SHIFT_DATUMS: &[&str] = &["WGS 84", "NAD 83", "NAD83(NSRS2007)"];

/// The projection engine's CRSs for a transformation from `src` to `dst`, with the source's datum
/// replaced by the target's when GDAL would use a null datum transformation between them.
pub(crate) fn wbp_pair(src: &SpatialRef, dst: &SpatialRef) -> Result<(wbp::Crs, wbp::Crs)> {
    let (mut s, d) = (src.to_wbp()?, dst.to_wbp()?);
    if is_ballpark_pair(&s, &d) {
        s.datum = d.datum.clone();
    }
    Ok((s, d))
}

fn is_ballpark_pair(a: &wbp::Crs, b: &wbp::Crs) -> bool {
    let (a, b) = (a.datum.name, b.datum.name);
    a != b && NULL_SHIFT_DATUMS.contains(&a) && NULL_SHIFT_DATUMS.contains(&b)
}

impl Transformer {
    /// A transformer from `src` to `dst`.
    pub fn new(src: &SpatialRef, dst: &SpatialRef) -> Result<Self> {
        let (s, d) = (src.to_wbp()?, dst.to_wbp()?);
        let ballpark = is_ballpark_pair(&s, &d);
        Ok(Self { src: s, dst: d, identity: src.same_as(dst), ballpark })
    }

    /// Transforms one point.
    pub fn transform(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        if self.identity {
            return Ok((x, y));
        }
        if self.ballpark {
            let (lon, lat) = self.src.inverse(x, y)?;
            return Ok(self.dst.forward(lon, lat)?);
        }
        Ok(self.src.transform_to(x, y, &self.dst)?)
    }

    /// Transforms a point with a height.
    pub fn transform_3d(&self, x: f64, y: f64, z: f64) -> Result<(f64, f64, f64)> {
        if self.identity {
            return Ok((x, y, z));
        }
        if self.ballpark {
            let (x, y) = self.transform(x, y)?;
            return Ok((x, y, z));
        }
        Ok(self.src.transform_to_3d(x, y, z, &self.dst)?)
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
}
