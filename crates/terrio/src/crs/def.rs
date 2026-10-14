//! A CRS as terrio transforms coordinates in it: a projection on an ellipsoid with linear and
//! angular units, axis order, prime meridian and its datum transformations to WGS 84, built from
//! the EPSG table, a PROJ string or a WKT definition.

use super::datum::{self, Op};
use super::math::*;
use super::params::Params;
use super::projections::{self, Proj, Setup};
use super::{SpatialRef, epsg, wkt1};
use crate::{Error, Result};
use std::sync::Arc;

/// A CRS ready for coordinate transformations.
#[derive(Debug, Clone)]
pub(crate) struct Def {
    /// The projection, `None` for a geographic CRS.
    proj: Option<Arc<dyn Proj>>,
    ell: Ell,
    lam0: f64,
    /// The latitude of origin (for comparing definitions).
    phi0: f64,
    x0: f64,
    y0: f64,
    /// Metres per linear unit.
    to_meter: f64,
    /// Degrees per angular unit (geographic CRSs).
    to_deg: f64,
    /// The prime meridian, in radians east of Greenwich.
    pm: f64,
    /// The axis directions (`enu`, `wsu`, `neu`...).
    axis: [u8; 2],
    /// Datum transformations to WGS 84, in the order they are tried; empty for WGS 84 itself or a
    /// datum treated as equivalent to it.
    ops: Vec<Arc<Op>>,
    /// The EPSG code of the datum, when known.
    datum: Option<u32>,
    /// The EPSG code of the geographic CRS, when known.
    geog: Option<u32>,
}

/// The EPSG geographic CRSs PROJ's `+datum=` names stand for.
fn datum_geog(name: &str) -> Option<u32> {
    Some(match name {
        "WGS84" => 4326,
        "NAD83" => 4269,
        "NAD27" => 4267,
        "GGRS87" => 4121,
        "potsdam" => 4314,
        "carthage" => 4223,
        "hermannskogel" => 4312,
        "ire65" => 4299,
        "nzgd49" => 4272,
        "OSGB36" => 4277,
        _ => return None,
    })
}

fn datum_ellps(name: &str) -> Option<&'static str> {
    Some(match name {
        "WGS84" => "WGS84",
        "NAD83" => "GRS80",
        "NAD27" => "clrk66",
        "GGRS87" => "GRS80",
        "potsdam" => "bessel",
        "carthage" => "clrk80ign",
        "hermannskogel" => "bessel",
        "ire65" => "mod_airy",
        "nzgd49" => "intl",
        "OSGB36" => "airy",
        _ => return None,
    })
}

/// Wraps a wbprojection projection, for projections terrio does not implement itself.
struct WbProj {
    crs: wbprojection::Crs,
    a: f64,
}

impl std::fmt::Debug for WbProj {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WbProj({})", self.crs.name)
    }
}

impl Proj for WbProj {
    fn fwd(&self, lam: f64, phi: f64) -> Option<(f64, f64)> {
        let (x, y) = self.crs.forward(lam.to_degrees(), phi.to_degrees()).ok()?;
        Some((x / self.a, y / self.a))
    }

    fn inv(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (lon, lat) = self.crs.inverse(x * self.a, y * self.a).ok()?;
        Some((lon.to_radians(), lat.to_radians()))
    }
}

impl Def {
    /// The definition of a CRS: from the EPSG table when it has a code there, else from its
    /// PROJ string or WKT.
    pub fn new(s: &SpatialRef) -> Result<Self> {
        if let Some(code) = s.epsg
            && let Some(e) = epsg::get(code)
        {
            let mut d = Self::from_proj(&e.proj)?;
            d.ops = e.ops.clone();
            d.datum = (e.datum != 0).then_some(e.datum);
            d.geog = (e.geog != 0).then_some(e.geog);
            // A WKT identified as this code but with its own TOWGS84: GDAL uses that shift.
            if let Some(w) = s.wkt.as_deref().filter(|w| *w != e.wkt)
                && let Some(t) = wkt_towgs84(w)
            {
                d.ops = vec![Arc::new(Op::world(datum::towgs84_steps(d.ell, &t)))];
                d.datum = None;
                d.geog = None;
            }
            return Ok(d);
        }
        if let Some(p) = &s.proj4 {
            return Self::from_proj(p);
        }
        if let Some(w) = &s.wkt {
            return Self::from_wkt(w);
        }
        if let Some(code) = s.epsg {
            return Err(Error::Crs(format!("unknown EPSG code {code}")));
        }
        Err(Error::Crs("the CRS is unknown".into()))
    }

    /// A definition from a PROJ string. `+towgs84=` and `+datum=` give its datum
    /// transformation.
    pub fn from_proj(s: &str) -> Result<Self> {
        let p = Params::parse(s);
        if let Some(init) = p.str("init") {
            let code = init.to_ascii_lowercase().strip_prefix("epsg:").and_then(|c| c.parse().ok());
            if let Some(code) = code {
                return Self::new(&SpatialRef { epsg: Some(code), wkt: None, proj4: None });
            }
        }
        let name = p.str("proj").ok_or_else(|| Error::Crs(format!("no +proj in '{s}'")))?;
        let datum_name = p.str("datum");
        let mut ell = datum::ellipsoid(&p).or_else(|| datum_name.and_then(datum_ellps).and_then(Ell::named)).unwrap_or_else(Ell::wgs84);
        // `+R_A`: a sphere of the ellipsoid's area.
        if p.has("R_A") && ell.es != 0.0 {
            let e = ell.e;
            let r = ell.a * (0.5 * qsfn(1.0, e, ell.one_es)).sqrt();
            ell = Ell::from_a_rf(r, 0.0);
        }
        let to_meter = p
            .str("to_meter")
            .and_then(|v| match v.split_once('/') {
                Some((n, d)) => Some(n.parse::<f64>().ok()? / d.parse::<f64>().ok()?),
                None => v.parse().ok(),
            })
            .or_else(|| p.str("units").and_then(unit_to_meter))
            .unwrap_or(1.0);
        let pm = p.str("pm").map(|v| v.parse::<f64>().ok().or_else(|| prime_meridian(v)).unwrap_or(0.0)).unwrap_or(0.0);
        let axis = match p.str("axis") {
            Some(a) if a.len() >= 2 && a.bytes().take(2).all(|c| b"enws".contains(&c)) => [a.as_bytes()[0], a.as_bytes()[1]],
            _ => *b"en",
        };
        let mut setup = Setup {
            p: &p,
            ell,
            phi0: p.angle("lat_0").unwrap_or(0.0),
            lam0: p.angle("lon_0").unwrap_or(0.0),
            k0: p.f64("k_0").or_else(|| p.f64("k")).unwrap_or(1.0),
            x0: p.f64("x_0").unwrap_or(0.0),
            y0: p.f64("y_0").unwrap_or(0.0),
        };
        let geographic = matches!(name, "longlat" | "latlong" | "lonlat" | "latlon");
        let proj: Option<Arc<dyn Proj>> = if geographic {
            None
        } else {
            match projections::build(name, &mut setup)? {
                Some(b) => Some(Arc::from(b)),
                None => {
                    // Fall back on wbprojection, which applies the origin and false easting itself.
                    let crs = wbprojection::from_proj_string(s).map_err(|e| Error::Crs(format!("{e}")))?;
                    setup.lam0 = 0.0;
                    setup.x0 = 0.0;
                    setup.y0 = 0.0;
                    Some(Arc::new(WbProj { crs, a: setup.ell.a }))
                }
            }
        };
        let (ell, lam0, phi0, x0, y0) = (setup.ell, setup.lam0, setup.phi0, setup.x0, setup.y0);
        let mut d = Self {
            proj,
            ell,
            lam0,
            phi0,
            x0,
            y0,
            to_meter,
            to_deg: p.f64("to_deg").unwrap_or(1.0),
            pm: pm.to_radians(),
            axis,
            ops: Vec::new(),
            datum: None,
            geog: None,
        };
        // The datum transformation.
        let towgs84 = p.list("towgs84").filter(|v| v.len() == 3 || v.len() == 7);
        if let Some(t) = towgs84 {
            if t.iter().any(|v| *v != 0.0) {
                d.ops = vec![Arc::new(Op::world(datum::towgs84_steps(ell, &t)))];
            }
        } else if let Some(code) = datum_name.and_then(datum_geog)
            && let Some(e) = epsg::get(code)
        {
            d.ops = e.ops.clone();
            d.datum = (e.datum != 0).then_some(e.datum);
            d.geog = Some(code);
        }
        Ok(d)
    }

    /// The coordinate conversion of a WKT 1 definition (without its datum), when terrio reads it.
    pub fn wkt_conversion(w: &str) -> Option<Self> {
        let c = wkt1::to_proj(&wkt1::parse(w)?)?;
        Self::from_proj(&c.proj).ok()
    }

    /// Whether both CRSs have the same coordinate conversion (projection, units, axes and prime
    /// meridian), whatever their datums: compared on points around the projection's origin.
    pub fn same_conversion(&self, other: &Def) -> bool {
        match (&self.proj, &other.proj) {
            (None, None) => (self.to_deg - other.to_deg).abs() < 1e-12 && (self.pm - other.pm).abs() < 1e-9 && self.axis == other.axis,
            (Some(_), Some(_)) => {
                let lat0 = self.phi0.to_degrees().clamp(-89.0, 89.0);
                let lon0 = (self.lam0 + self.pm).to_degrees();
                for dlon in [-0.5, 0.0, 0.5] {
                    for dlat in [-0.5, 0.0, 0.5] {
                        let (lon, lat) = ((lon0 + dlon).to_radians(), (lat0 + dlat).clamp(-89.9, 89.9).to_radians());
                        match (self.project(lon, lat), other.project(lon, lat)) {
                            (Some(a), Some(b)) => {
                                let d = ((a.0 * self.to_meter - b.0 * other.to_meter).powi(2)
                                    + (a.1 * self.to_meter - b.1 * other.to_meter).powi(2))
                                .sqrt();
                                if d.is_nan() || d >= 0.05 {
                                    return false;
                                }
                            }
                            (None, None) => {}
                            _ => return false,
                        }
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// A definition from WKT. Datum transformations come from `TOWGS84`, else from the EPSG
    /// geographic CRS the WKT's `GEOGCS` is identified as.
    pub fn from_wkt(w: &str) -> Result<Self> {
        if let Some(root) = wkt1::parse(w)
            && let Some(c) = wkt1::to_proj(&root)
        {
            let mut d = Self::from_proj(&c.proj)?;
            if d.ops.is_empty() && !c.proj.contains("+towgs84=") {
                let geog = c.geogcs.epsg().or_else(|| {
                    let text = geogcs_text(w)?;
                    super::identify_epsg(text)
                });
                if let Some(e) = geog.and_then(epsg::get)
                    && e.proj.starts_with("+proj=longlat")
                {
                    d.ops = e.ops.clone();
                    d.datum = (e.datum != 0).then_some(e.datum);
                    d.geog = geog;
                }
            }
            return Ok(d);
        }
        // WKT 2 and projections the reader does not know: wbprojection's parser.
        let crs = wbprojection::Crs::from_wkt(w).map_err(|e| Error::Crs(format!("{e}")))?;
        let geographic = crs.is_geographic();
        let e = &crs.datum.ellipsoid;
        let ell = Ell::from_a_es(e.a, 2.0 * e.f - e.f * e.f);
        let mut d = Self {
            proj: None,
            ell,
            lam0: 0.0,
            phi0: 0.0,
            x0: 0.0,
            y0: 0.0,
            to_meter: wkt_linear_unit(w).filter(|_| !geographic).unwrap_or(1.0),
            to_deg: 1.0,
            pm: 0.0,
            axis: *b"en",
            ops: Vec::new(),
            datum: None,
            geog: None,
        };
        if !geographic {
            d.proj = Some(Arc::new(WbProj { crs, a: ell.a }));
        }
        Ok(d)
    }

    /// Whether this is a geographic CRS.
    pub fn is_geographic(&self) -> bool {
        self.proj.is_none()
    }

    /// The coordinates `(east, north)` of axis-ordered ones.
    fn unaxis(&self, u: f64, v: f64) -> (f64, f64) {
        if self.axis == *b"en" {
            return (u, v);
        }
        let (mut e, mut n) = (0.0, 0.0);
        for (c, val) in self.axis.iter().zip([u, v]) {
            match c {
                b'e' => e = val,
                b'w' => e = -val,
                b'n' => n = val,
                b's' => n = -val,
                _ => {}
            }
        }
        (e, n)
    }

    fn axis(&self, e: f64, n: f64) -> (f64, f64) {
        if self.axis == *b"en" {
            return (e, n);
        }
        let pick = |c: u8| match c {
            b'e' => e,
            b'w' => -e,
            b'n' => n,
            _ => -n,
        };
        (pick(self.axis[0]), pick(self.axis[1]))
    }

    /// Geodetic coordinates (radians, east of Greenwich) of a point in this CRS.
    pub fn unproject(&self, u: f64, v: f64) -> Option<(f64, f64)> {
        let (e, n) = self.unaxis(u, v);
        let (lam, phi) = match &self.proj {
            None => ((e * self.to_deg).to_radians(), (n * self.to_deg).to_radians()),
            Some(p) => {
                let x = (e * self.to_meter - self.x0) / self.ell.a;
                let y = (n * self.to_meter - self.y0) / self.ell.a;
                let (lam, phi) = p.inv(x, y)?;
                (lam + self.lam0, phi)
            }
        };
        if !lam.is_finite() || !phi.is_finite() {
            return None;
        }
        Some((adjlon(lam + self.pm), phi))
    }

    /// The coordinates in this CRS of a geodetic point (radians, east of Greenwich).
    pub fn project(&self, lon: f64, lat: f64) -> Option<(f64, f64)> {
        let lon = lon - self.pm;
        let (e, n) = match &self.proj {
            None => (adjlon(lon).to_degrees() / self.to_deg, lat.to_degrees() / self.to_deg),
            Some(p) => {
                if lat.abs() > HALFPI + 1e-12 {
                    return None;
                }
                let (x, y) = p.fwd(adjlon(lon - self.lam0), lat)?;
                ((x * self.ell.a + self.x0) / self.to_meter, (y * self.ell.a + self.y0) / self.to_meter)
            }
        };
        if !e.is_finite() || !n.is_finite() {
            return None;
        }
        Some(self.axis(e, n))
    }

    /// Whether both CRSs are on the same datum, so that no datum transformation is needed.
    pub fn same_datum(&self, other: &Def) -> bool {
        match (self.datum, other.datum) {
            (Some(a), Some(b)) => a == b,
            _ => self.ops == other.ops,
        }
    }

    /// The datum transformations to WGS 84, in the order they are tried.
    pub fn ops(&self) -> &[Arc<Op>] {
        &self.ops
    }

    /// The EPSG code of the geographic CRS.
    pub fn geog(&self) -> Option<u32> {
        self.geog
    }

    /// Whether the CRS is on WGS 84 itself.
    pub fn is_wgs84(&self) -> bool {
        self.datum == Some(6326)
    }

    /// The bounding boxes in this CRS of a transformation's area (two across the antimeridian),
    /// from 21 points along each edge as PROJ's `proj_trans_bounds` samples them. Empty for a
    /// geographic CRS, a world-wide area or one that does not project.
    pub fn areas(&self, op: &Op) -> Vec<[f64; 4]> {
        let [w, s, e, n] = op.bounds;
        if self.proj.is_none() || (e - w >= 360.0 && n - s >= 180.0) {
            return Vec::new();
        }
        let parts = if w <= e { vec![[w, s, e, n]] } else { vec![[w, s, 180.0, n], [-180.0, s, e, n]] };
        parts
            .into_iter()
            .filter_map(|[w, s, e, n]| {
                let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
                const STEPS: usize = 20;
                for i in 0..=STEPS {
                    let t = i as f64 / STEPS as f64;
                    let (lon, lat) = (w + (e - w) * t, s + (n - s) * t);
                    for (x, y) in [(lon, s), (lon, n), (w, lat), (e, lat)] {
                        if let Some((u, v)) = self.project(x.to_radians(), y.to_radians()) {
                            b = [b[0].min(u), b[1].min(v), b[2].max(u), b[3].max(v)];
                        }
                    }
                }
                b[0].is_finite().then_some(b)
            })
            .collect()
    }
}

/// The `TOWGS84` parameters of a WKT 1 definition, when they shift.
fn wkt_towgs84(w: &str) -> Option<Vec<f64>> {
    let root = wkt1::parse(w)?;
    let geogcs = if root.key.eq_ignore_ascii_case("GEOGCS") { &root } else { root.child("GEOGCS")? };
    let t = geogcs.child("DATUM")?.child("TOWGS84")?;
    let v: Vec<f64> = (0..t.args.len()).filter_map(|i| t.num(i)).collect();
    ((v.len() == 3 || v.len() == 7) && v.iter().any(|x| *x != 0.0)).then_some(v)
}

/// The `GEOGCS[...]` part of a WKT 1 definition.
fn geogcs_text(w: &str) -> Option<&str> {
    let start = w.find("GEOGCS[")?;
    let mut depth = 0;
    for (i, c) in w[start..].char_indices() {
        match c {
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&w[start..start + i + 1]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The size in metres of a WKT definition's linear unit: the last `UNIT` (WKT 1) or
/// `LENGTHUNIT` (WKT 2) that is not angular.
pub(crate) fn wkt_linear_unit(w: &str) -> Option<f64> {
    let root = wkt1::parse(w)?;
    if let Some(u) = root.children("UNIT").last().or_else(|| root.children("LENGTHUNIT").last()) {
        return u.num(1);
    }
    root.children("AXIS").find_map(|a| a.child("LENGTHUNIT")).and_then(|u| u.num(1))
}
