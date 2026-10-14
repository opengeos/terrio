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
        Self { epsg: wbp::identify_epsg_from_wkt(wkt), wkt: Some(wkt.to_string()), proj4: None }
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
            s.epsg = wbp::identify_epsg_from_wkt(w);
        }
        s
    }

    pub(crate) fn from_layer(layer: &wbvector::Layer) -> Self {
        let mut s = Self { epsg: layer.crs_epsg(), wkt: layer.crs_wkt().map(str::to_string), proj4: None };
        if s.epsg.is_none()
            && let Some(w) = &s.wkt
        {
            s.epsg = wbp::identify_epsg_from_wkt(w);
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
}

impl Transformer {
    /// A transformer from `src` to `dst`.
    pub fn new(src: &SpatialRef, dst: &SpatialRef) -> Result<Self> {
        Ok(Self { src: src.to_wbp()?, dst: dst.to_wbp()?, identity: src.same_as(dst) })
    }

    /// Transforms one point.
    pub fn transform(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        if self.identity {
            return Ok((x, y));
        }
        Ok(self.src.transform_to(x, y, &self.dst)?)
    }

    /// Transforms a point with a height.
    pub fn transform_3d(&self, x: f64, y: f64, z: f64) -> Result<(f64, f64, f64)> {
        if self.identity {
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
        assert_eq!(SpatialRef::parse("EPSG:32633").unwrap().name().unwrap(), "WGS 84 / UTM zone 33N");
    }

    #[test]
    fn transforms_to_web_mercator() {
        let t = Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &SpatialRef::from_epsg(3857).unwrap()).unwrap();
        let (x, y) = t.transform(-115.0, 36.0).unwrap();
        assert!((x - -12801741.0).abs() < 1.0, "{x}");
        assert!((y - 4300621.4).abs() < 1.0, "{y}");
    }
}
