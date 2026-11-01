// SPDX-FileCopyrightText: Copyright (c) 2000, Frank Warmerdam
// SPDX-FileCopyrightText: PROJ contributors
// SPDX-FileCopyrightText: Copyright (c) 2018, Even Rouault <even dot rouault at spatialys dot com>
// SPDX-FileCopyrightText: Copyright (c) 2023, Even Rouault <even dot rouault at spatialys dot com>
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   PROJ 9.8.1:
//     src/datums.cpp
//     src/ell_set.cpp
//     src/iso19111/datum.cpp
//     include/proj/internal/datum_internal.hpp
//     src/iso19111/operation/coordinateoperationfactory.cpp

//! A CRS as terrio transforms coordinates in it: a projection on an ellipsoid with linear and
//! angular units, axis order, prime meridian and its datum transformations to WGS 84, built from
//! the EPSG table, a PROJ string or a WKT definition.

use super::datum::{self, Op};
use super::math::*;
use super::params::Params;
use super::projections::{self, Proj, Setup};
use super::rotation::{ObTran, Rotation};
use super::{SpatialRef, epsg, wkt1, wkt2};
use crate::{Error, Result};
use std::sync::Arc;

/// A CRS ready for coordinate transformations.
#[derive(Debug, Clone)]
pub(crate) struct Def {
    /// The projection, `None` for a geographic CRS.
    proj: Option<Arc<dyn Proj>>,
    /// Converts degrees as the inverse of a PROJ `unitconvert` step does, dividing by the factor
    /// of the forward step. See [`crate::Transformer::with_reverse_degree_division`].
    pub(crate) inverted_degrees: bool,
    geocentric: bool,
    /// Geographic planetocentric latitude rather than ellipsoidal geodetic latitude.
    ocentric: bool,
    /// NAIF celestial-body identifier when declared by the IAU registry.
    body: Option<u32>,
    iau_datum: Option<u32>,
    z_down: bool,
    /// The pole rotation of a rotated-pole grid (`+proj=ob_tran`), a CRS in degrees without a
    /// projection, whose longitudes are relative to `lam0`.
    rot: Option<Rotation>,
    pub(crate) ell: Ell,
    /// Semi-major axis used by the projection, including auxiliary-sphere modifiers.
    projection_a: f64,
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
    /// Whether it is a geographic 3D CRS (with ellipsoidal heights).
    three_d: bool,
    /// Whether it is a dynamic CRS (on a dynamic reference frame such as ITRF2014).
    dynamic: bool,
    /// The vertical CRS of a compound CRS.
    vertical: Option<Arc<Vertical>>,
    /// The area of use (west, south, east, north in degrees), when known.
    area: Option<[f64; 4]>,
}

/// The vertical part of a compound CRS: its unit and direction, and its geoid models.
#[derive(Debug)]
pub(crate) struct Vertical {
    /// The EPSG code, when known.
    pub code: Option<u32>,
    /// Metres per unit.
    pub to_meter: f64,
    /// Whether heights are depths (positive down).
    pub down: bool,
    /// Transformations from ellipsoidal heights (of a geographic 3D CRS, by EPSG code, or of the
    /// horizontal CRS's datum when `None`) to heights in this CRS.
    pub models: Vec<(Option<u32>, Vec<Arc<Op>>)>,
    /// Whether heights are kept as they are when no model applies (PROJ's ballpark vertical
    /// transformation, which it adds for EPSG vertical CRSs but not for `+geoidgrids=`).
    pub ballpark: bool,
}

impl Vertical {
    /// The vertical CRS of an EPSG code.
    pub fn from_epsg(code: u32) -> Option<Self> {
        let v = epsg::vertical(code)?;
        // The table does not name geoid models: `WGS 84 to EGM96 height`, as PROJ names them
        // without the EPSG index (`(1)`). A model borrowed from another vertical CRS of the datum
        // is named after that CRS: NAVD88 height (ftUS) uses `NAD83(2011) to NAVD88 height`.
        let named = |g: u32, vc: u32, ops: Vec<Arc<datum::Op>>| -> Vec<Arc<datum::Op>> {
            let Some(geog) = epsg::get(g) else { return ops };
            let Some(to) = epsg::vertical(vc) else { return ops };
            ops.into_iter()
                .map(|op| {
                    if !op.name.is_empty() || op.steps.is_empty() {
                        return op;
                    }
                    Arc::new(datum::Op { name: format!("{} to {}", geog.name, to.name), ..(*op).clone() })
                })
                .collect()
        };
        let models = epsg::geoid_models(code).into_iter().map(|(g, vc, ops)| (Some(g), named(g, vc, ops))).collect();
        Some(Self { code: Some(code), to_meter: v.to_meter, down: v.down, models, ballpark: true })
    }

    /// A vertical CRS whose heights are ellipsoidal heights minus a geoid grid's (PROJ's
    /// `+geoidgrids=`, GDAL's `PROJ4_GRIDS` extension).
    pub fn from_geoid_grids(spec: &str, to_meter: f64) -> Self {
        let op = Op::world(vec![datum::vgridshift_step(spec, -1.0)]);
        Self { code: None, to_meter, down: false, models: vec![(None, vec![Arc::new(op)])], ballpark: false }
    }

    /// Metres from a height in this CRS's unit and direction.
    pub fn to_metres(&self, z: f64) -> f64 {
        if self.down { -z * self.to_meter } else { z * self.to_meter }
    }

    /// A height in this CRS's unit and direction from metres.
    pub fn height_of(&self, h: f64) -> f64 {
        if self.down { -h / self.to_meter } else { h / self.to_meter }
    }
}

/// The EPSG geographic CRSs PROJ's `+datum=` names stand for.
pub(crate) fn datum_geog(name: &str) -> Option<u32> {
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

/// The ellipsoids of PROJ's `+datum=` names.
pub(crate) fn datum_ellps(name: &str) -> Option<&'static str> {
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
#[cfg(feature = "wbprojection-fallback")]
struct WbProj {
    crs: wbprojection::Crs,
    a: f64,
}

#[cfg(feature = "wbprojection-fallback")]
impl std::fmt::Debug for WbProj {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WbProj({})", self.crs.name)
    }
}

#[cfg(feature = "wbprojection-fallback")]
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
    /// The definition of a CRS: from the EPSG (or ESRI) table when it has a code there, else
    /// from its PROJ string or WKT.
    pub fn new(s: &SpatialRef) -> Result<Self> {
        if let Some(code) = s.epsg
            && let Some(c) = epsg::compound(code)
        {
            let mut d = Self::new(&SpatialRef { epsg: Some(c.horizontal), ..Default::default() })?;
            d.vertical = Vertical::from_epsg(c.vertical).map(Arc::new);
            return Ok(d);
        }
        // An ESRI CRS whose projection terrio computes otherwise than PROJ goes by its WKT, as
        // before terrio knew ESRI codes.
        let entry = match s.epsg {
            Some(code) => epsg::get(code),
            None => s.esri.filter(|c| !super::ESRI_UNLIKE_PROJ.contains(c)).and_then(epsg::esri),
        };
        if let Some(e) = entry {
            let mut d = Self::from_proj(&e.proj)?;
            d.ops = epsg::usable(&e.ops, &e.grid_ops);
            d.datum = (e.datum != 0).then_some(e.datum);
            d.geog = (e.geog != 0).then_some(e.geog);
            d.three_d = e.three_d;
            d.dynamic = e.dynamic;
            d.area = e.area;
            d.body = Some(399);
            // A WKT identified as this code but with its own TOWGS84: GDAL uses that shift (not
            // over a PROJ4 extension, which describes the whole CRS).
            if s.proj4.is_none()
                && let Some(w) = s.wkt.as_deref().filter(|w| *w != e.wkt)
                && let Some(t) = wkt_towgs84(w)
            {
                d.ops = vec![Arc::new(Op::world(datum::towgs84_steps(d.ell, &t)))];
                d.datum = None;
                d.geog = None;
            }
            return Ok(d);
        }
        if let Some(p) = &s.proj4 {
            let mut d = Self::from_proj(p)?;
            d.tag_iau(s)?;
            return Ok(d);
        }
        if let Some(w) = &s.wkt {
            let mut d = Self::from_wkt(w)?;
            d.tag_iau(s)?;
            return Ok(d);
        }
        if let Some(code) = s.epsg {
            return Err(Error::Crs(format!("unknown EPSG code {code}")));
        }
        Err(Error::Crs("the CRS is unknown".into()))
    }

    fn tag_iau(&mut self, s: &SpatialRef) -> Result<()> {
        if let Some(entry) = s.wkt.as_deref().and_then(super::iau::declared).and_then(super::iau::get) {
            if entry.body != 399 && (self.datum.is_some() || self.geog.is_some() || !self.ops.is_empty()) {
                return Err(Error::Crs("Earth datum operations cannot be applied to an IAU planetary CRS".into()));
            }
            self.body = Some(entry.body);
            // Both effective definitions exactly match immutable registry data.
            // Avoid reparsing the datum and rebuilding the base on repeated setup.
            if s.wkt.as_deref() == Some(entry.wkt.as_str()) && s.proj4.as_deref() == Some(entry.proj.as_str()) {
                self.iau_datum = Some(entry.datum);
                self.ocentric = entry.geographic && entry.ocentric;
                return Ok(());
            }
            // A CRS identifier must not override an edited datum definition.
            let datum_node = |w: &str| {
                let root = super::wkt1::parse(w)?;
                let base = root.child("BASEGEOGCRS").or_else(|| root.child("BASEGEODCRS")).unwrap_or(&root);
                base.child("DATUM").cloned()
            };
            let base = super::iau::get(entry.base).ok_or_else(|| Error::Crs(format!("unknown IAU base CRS {}", entry.base)))?;
            let canonical = Self::from_proj(&base.proj)?;
            if (self.ell.a - canonical.ell.a).abs() < 1e-6
                && (self.ell.es - canonical.ell.es).abs() < 1e-14
                && s.wkt.as_deref().and_then(datum_node).is_some_and(|d| Some(d) == datum_node(&entry.wkt))
                && s.proj4.as_deref().is_none_or(|p| p == entry.proj)
            {
                self.iau_datum = Some(entry.datum);
            }
        }
        Ok(())
    }

    /// A declared body, or PROJ's Earth-radius heuristic for custom ellipsoids.
    pub fn celestial_body(&self) -> Option<u32> {
        self.body.or_else(|| ((self.ell.a - 6_375_000.0).abs() < 0.007 * 6_375_000.0).then_some(399))
    }

    /// PROJ 9.8.1's compatibility rule: known body names take precedence over radius;
    /// otherwise semi-major axes must agree within 0.7% of the destination's.
    pub fn compatible_body(&self, other: &Self) -> Result<()> {
        let (a, b) = (self.celestial_body(), other.celestial_body());
        if (a.is_some() && b.is_some() && a != b)
            || ((a.is_none() || b.is_none()) && (self.ell.a - other.ell.a).abs() > 0.007 * other.ell.a)
        {
            return Err(Error::Crs(format!(
                "source and target ellipsoids do not belong to the same celestial body (source semi-major axis {} m, target {} m)",
                self.ell.a, other.ell.a
            )));
        }
        if (a != Some(399) && (self.datum.is_some() || !self.ops.is_empty()))
            || (b != Some(399) && (other.datum.is_some() || !other.ops.is_empty()))
        {
            return Err(Error::Crs("Earth datum operations cannot be applied to planetary coordinates".into()));
        }
        Ok(())
    }

    /// A definition from a PROJ string. `+towgs84=` and `+datum=` give its datum
    /// transformation.
    pub fn from_proj(s: &str) -> Result<Self> {
        let p = Params::parse(s);
        p.validate_quoting()?;
        if let Some(init) = p.str("init") {
            let code = init.to_ascii_lowercase().strip_prefix("epsg:").and_then(|c| c.parse().ok());
            if let Some(code) = code {
                return Self::new(&SpatialRef { epsg: Some(code), ..Default::default() });
            }
        }
        let name = p.str("proj").ok_or_else(|| Error::Crs(format!("no +proj in '{s}'")))?;
        let datum_name = p.str("datum");
        if name == "geocent" {
            for key in ["a", "b", "rf", "f", "es", "e", "R"] {
                if p.has(key) && p.f64(key).is_none_or(|v| !v.is_finite()) {
                    return Err(Error::Crs(format!("invalid geocentric ellipsoid parameter {key}")));
                }
            }
            if (p.has("ellps") && p.str("ellps").and_then(Ell::named).is_none())
                || (p.has("datum") && datum_name.and_then(datum_geog).is_none())
            {
                return Err(Error::Crs("unknown geocentric ellipsoid or datum".into()));
            }
            if p.has("towgs84") {
                let values = p.str("towgs84").unwrap_or("").split(',').collect::<Vec<_>>();
                if !matches!(values.len(), 3 | 7) || values.iter().any(|v| v.trim().parse::<f64>().ok().is_none_or(|n| !n.is_finite())) {
                    return Err(Error::Crs("geocentric towgs84 needs 3 or 7 finite parameters".into()));
                }
            }
        }

        let mut ell = datum::ellipsoid(&p).or_else(|| datum_name.and_then(datum_ellps).and_then(Ell::named)).unwrap_or_else(Ell::wgs84);
        // `+R_A`: a sphere of the ellipsoid's area.
        if p.has("R_A") && ell.es != 0.0 {
            let e = ell.e;
            let r = ell.a * (0.5 * qsfn(1.0, e, ell.one_es)).sqrt();
            ell = Ell::from_a_rf(r, 0.0);
        }
        let explicit_to_meter = p.str("to_meter").and_then(|v| match v.split_once('/') {
            Some((n, d)) => Some(n.parse::<f64>().ok()? / d.parse::<f64>().ok()?),
            None => v.parse().ok(),
        });
        let to_meter = explicit_to_meter.or_else(|| p.str("units").and_then(unit_to_meter)).unwrap_or(1.0);
        let explicit_pm = p.str("pm").and_then(|v| {
            v.parse::<f64>()
                .ok()
                .or_else(|| prime_meridian(v))
                .or_else(|| if name == "geocent" { p.angle("pm").map(f64::to_degrees) } else { None })
        });
        if name == "geocent" && p.has("pm") && explicit_pm.is_none_or(|v| !v.is_finite()) {
            return Err(Error::Crs("invalid geocentric prime meridian".into()));
        }
        let pm = explicit_pm.unwrap_or(0.0);
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
        // As PROJ refuses it ("Invalid value for lat_0: |lat_0| should be <= 90°").
        if setup.phi0.abs() > HALFPI + 1e-12 || !setup.phi0.is_finite() {
            return Err(Error::Crs(format!("invalid lat_0: {} degrees", setup.phi0.to_degrees())));
        }
        let is_longlat = |n: &str| matches!(n, "longlat" | "latlong" | "lonlat" | "latlon");
        // `ob_tran`'s rotation, of a rotated-pole grid (`+o_proj=longlat`) or of a projection.
        let ob_tran = if name == "ob_tran" { Some(Rotation::from_params(&p)?) } else { None };
        let o_proj = p.str("o_proj").filter(|_| ob_tran.is_some());
        let rot = ob_tran.filter(|_| o_proj.is_some_and(is_longlat));
        let geocentric = name == "geocent";
        if geocentric && p.has("to_meter") && explicit_to_meter.is_none() {
            return Err(Error::Crs("invalid geocentric to_meter".into()));
        }
        if geocentric && p.has("axis") {
            let b = p.str("axis").unwrap_or("").as_bytes();
            let valid = b.len() == 3
                && matches!(b[2], b'u' | b'd')
                && ((matches!(b[0], b'e' | b'w') && matches!(b[1], b'n' | b's'))
                    || (matches!(b[0], b'n' | b's') && matches!(b[1], b'e' | b'w')));
            if !valid {
                return Err(Error::Crs("geocentric axis must span east/west, north/south and up/down".into()));
            }
        }

        if geocentric && p.has("units") && p.str("units").and_then(unit_to_meter).is_none() && !p.has("to_meter") {
            return Err(Error::Crs("unknown geocentric linear unit".into()));
        }
        if geocentric
            && (!to_meter.is_finite() || to_meter <= 0.0 || !ell.a.is_finite() || ell.a <= 0.0 || !ell.es.is_finite() || ell.es >= 1.0)
        {
            return Err(Error::Crs("invalid geocentric unit or ellipsoid".into()));
        }
        let geographic = is_longlat(name) || rot.is_some();
        // PROJ 9.8.1 evaluates R_C before parsing lat_0 (init.cpp), so its
        // conformal radius is equatorial even with a nonzero latitude parameter.
        // Projected endpoints retain their geodetic ellipsoid separately from this
        // projection radius. Geographic/geocentric conversions use the sphere,
        // matching PROJ; their stored CRS metadata still describes the original ellipsoid.
        let conformal_ellipsoid = (p.has("R_C") && !p.has("R") && !p.has("R_A")).then_some(ell);
        if let Some(base) = conformal_ellipsoid {
            let radius = base.a * base.one_es.sqrt();
            if !radius.is_finite() || radius <= 0.0 {
                return Err(Error::Crs("invalid conformal sphere radius".into()));
            }
            setup.ell = Ell::from_a_rf(radius, 0.0);
        }
        let proj: Option<Arc<dyn Proj>> = if geographic || geocentric {
            None
        } else if let (Some(rot), Some(o_proj)) = (ob_tran, o_proj) {
            if o_proj == "ob_tran" {
                return Err(Error::Crs("invalid +proj=ob_tran: +o_proj=ob_tran".into()));
            }
            // A projected ob_tran (+o_proj other than longlat) applies the CRS's figure, central
            // meridian and false easting and northing, as PROJ does. The rotated projection is built
            // on a copy of the setup, so what its formulas read (+lat_0, +lat_ts, scale) and change
            // stays with it.
            let mut inner =
                Setup { p: setup.p, ell: setup.ell, phi0: setup.phi0, lam0: setup.lam0, k0: setup.k0, x0: setup.x0, y0: setup.y0 };
            let link = projections::build(o_proj, &mut inner)?
                .ok_or_else(|| Error::Crs(format!("unsupported projection: +proj=ob_tran with +o_proj={o_proj}")))?;
            Some(Arc::new(ObTran { rot, link }))
        } else {
            match projections::build(name, &mut setup)? {
                Some(b) => Some(Arc::from(b)),
                None => {
                    #[cfg(feature = "wbprojection-fallback")]
                    {
                        // Fall back on wbprojection, which applies the origin and false easting itself.
                        let crs = wbprojection::from_proj_string(s).map_err(|e| Error::Crs(format!("{e}")))?;
                        setup.lam0 = 0.0;
                        setup.x0 = 0.0;
                        setup.y0 = 0.0;
                        Some(Arc::new(WbProj { crs, a: setup.ell.a }))
                    }
                    #[cfg(not(feature = "wbprojection-fallback"))]
                    return Err(Error::Crs(format!("unsupported projection: +proj={name}; enable the wbprojection-fallback feature")));
                }
            }
        };
        let projection_a = setup.ell.a;
        let ell = if proj.is_some() { conformal_ellipsoid.unwrap_or(setup.ell) } else { setup.ell };
        let (lam0, phi0, x0, y0) = (setup.lam0, setup.phi0, setup.x0, setup.y0);
        let mut d = Self {
            proj,
            inverted_degrees: false,
            geocentric,
            ocentric: geographic && rot.is_none() && p.has("geoc"),
            body: None,
            iau_datum: None,
            z_down: p.str("axis").is_some_and(|a| a.as_bytes().get(2) == Some(&b'd')),
            rot,
            ell,
            projection_a,
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
            three_d: false,
            dynamic: false,
            vertical: None,
            area: None,
        };
        // The datum transformation.
        let towgs84 = p.list("towgs84").filter(|v| v.len() == 3 || v.len() == 7);
        let nadgrids = p.str("nadgrids").filter(|g| !g.split(',').all(|n| n.trim_start_matches('@') == "null"));
        if let Some(g) = nadgrids {
            d.ops = vec![Arc::new(Op::world(datum::nadgrids_steps(g)))];
        } else if let Some(t) = towgs84 {
            // Zero translation still converts between different ellipsoids. On
            // WGS 84 itself it is exactly the WGS 84 datum, without a Cartesian round trip.
            let wgs84 = Ell::wgs84();
            if t.iter().all(|v| *v == 0.0) && ell.a == wgs84.a && ell.es == wgs84.es {
                d.datum = Some(6326);
                d.geog = Some(4326);
            } else {
                d.ops = vec![Arc::new(Op::world(datum::towgs84_steps(ell, &t)))];
            }
        } else if let Some(code) = datum_name.and_then(datum_geog)
            && let Some(e) = epsg::get(code)
        {
            d.ops = epsg::usable(&e.ops, &e.grid_ops);
            d.datum = (e.datum != 0).then_some(e.datum);
            d.geog = Some(code);
            d.dynamic = e.dynamic;
        }
        if let Some(g) = p.str("geoidgrids") {
            let to_meter =
                p.str("vto_meter").and_then(|v| v.parse().ok()).or_else(|| p.str("vunits").and_then(unit_to_meter)).unwrap_or(1.0);
            d.vertical = Some(Arc::new(Vertical::from_geoid_grids(g, to_meter)));
        }
        Ok(d)
    }

    /// The coordinate conversion of a WKT definition (without its datum), when terrio reads it: that
    /// of its `PROJ4` extension when it has one.
    pub fn wkt_conversion(w: &str) -> Option<Self> {
        if let Some(d) = super::proj4_extension(w).and_then(|p| Self::from_proj(&p).ok()) {
            return Some(d);
        }
        let root = wkt1::parse(w)?;
        let c = if wkt2::handles(&root) { wkt2::to_proj(&root).ok()? } else { wkt1::to_proj(&root)? };
        Self::from_proj(&c.proj).ok()
    }

    /// Whether both CRSs have the same coordinate conversion (projection, units, axes and prime
    /// meridian), whatever their datums: compared on points around the projection's origin.
    pub fn same_conversion(&self, other: &Def) -> bool {
        if self.geocentric != other.geocentric
            || self.ocentric != other.ocentric
            || self.ocentric && (self.ell.es - other.ell.es).abs() > 1e-15
        {
            return false;
        }
        if self.geocentric {
            return self.axis == other.axis
                && self.z_down == other.z_down
                && (self.to_meter / other.to_meter - 1.0).abs() < 1e-12
                && (self.pm - other.pm).abs() < 1e-12;
        }
        match (&self.proj, &other.proj) {
            (None, None) => {
                (self.to_deg - other.to_deg).abs() < 1e-12
                    && (self.pm - other.pm).abs() < 1e-9
                    && self.axis == other.axis
                    && self.rot == other.rot
                    && (self.lam0 - other.lam0).abs() < 1e-12
            }
            (Some(_), Some(_)) => {
                // In the same unit (a State Plane zone in US survey feet is not the one in metres).
                let ratio = self.to_meter / other.to_meter;
                if !ratio.is_finite() || (ratio - 1.0).abs() > 1e-9 {
                    return false;
                }
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
        if let Some((horizontal, vertical)) = split_compound(w) {
            let h = SpatialRef::from_wkt(&horizontal);
            let mut d = Self::new(&h)?;
            d.vertical = vertical_from_wkt(&vertical).map(Arc::new);
            return Ok(d);
        }
        // A WKT 1 extended with its PROJ string (`EXTENSION["PROJ4", ...]`) is that PROJ string,
        // whatever its nodes say, as GDAL and PROJ read it.
        if let Some(d) = super::proj4_extension(w).and_then(|p| Self::from_proj(&p).ok()) {
            return Ok(d);
        }
        // Text the WKT reader refuses (not WKT, nested deeper or longer than GDAL reads) is not
        // handed to wbprojection's parser either, whose recursion has no bound.
        let root = wkt1::parse(w).ok_or_else(|| Error::Crs("not a WKT definition GDAL reads".into()))?;
        let converted = if wkt2::handles(&root) { Some(wkt2::to_proj(&root)?) } else { wkt1::to_proj(&root) };
        if let Some(c) = converted {
            let mut d = Self::from_proj(&c.proj)?;
            if d.ops.is_empty() && !c.proj.contains("+towgs84=") {
                let geog = c.geog.or_else(|| c.geogcs.epsg()).or_else(|| match &c.geogcs_wkt1 {
                    // WKT 2: identified loosely, so on the same ellipsoid only.
                    Some(text) => super::identify_epsg(text).filter(|g| wkt2::same_ellipsoid(*g, d.ell.a, d.ell.es)),
                    None => super::identify_epsg(geogcs_text(w)?),
                });
                let geog = geog.map(|g| epsg::get(g).filter(|e| e.proj.starts_with("+proj=geocent")).map_or(g, |e| e.geog));
                if let Some(e) = geog.and_then(epsg::get)
                    && e.proj.starts_with("+proj=longlat")
                {
                    d.ops = epsg::usable(&e.ops, &e.grid_ops);
                    d.datum = (e.datum != 0).then_some(e.datum);
                    d.geog = if e.three_d { Some(e.geog) } else { geog };
                    d.dynamic = e.dynamic;
                }
            }
            return Ok(d);
        }
        // Other kinds of WKT 2 CRS and WKT 1 projections the readers do not know: wbprojection's
        // parser. A WKT 2 geographic or projected CRS the reader refuses is not handed to it: its
        // WKT 2 parameters and method names would be read wrongly, silently (#123).
        #[cfg(feature = "wbprojection-fallback")]
        {
            let crs = wbprojection::Crs::from_wkt(w).map_err(|e| Error::Crs(format!("{e}")))?;
            let geographic = crs.is_geographic();
            let e = &crs.datum.ellipsoid;
            let ell = Ell::from_a_es(e.a, 2.0 * e.f - e.f * e.f);
            let mut d = Self {
                proj: None,
                inverted_degrees: false,
                geocentric: false,
                ocentric: false,
                body: None,
                iau_datum: None,
                z_down: false,
                rot: None,
                ell,
                projection_a: ell.a,
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
                three_d: false,
                dynamic: false,
                vertical: None,
                area: None,
            };
            if !geographic {
                d.proj = Some(Arc::new(WbProj { crs, a: ell.a }));
            }
            Ok(d)
        }
        #[cfg(not(feature = "wbprojection-fallback"))]
        Err(Error::Crs("unsupported WKT definition; enable the wbprojection-fallback feature".into()))
    }

    /// Metres per linear unit.
    pub fn to_meter(&self) -> f64 {
        self.to_meter
    }

    /// Whether this is a geographic CRS. A rotated-pole grid is not: its coordinates are in
    /// degrees, but are not longitudes and latitudes.
    pub fn is_geographic(&self) -> bool {
        self.proj.is_none() && self.rot.is_none() && !self.geocentric
    }

    pub fn is_geocentric(&self) -> bool {
        self.geocentric
    }

    /// The internal geographic coordinate space used to select and apply datum operations.
    pub fn geographic_endpoint(&self) -> Self {
        let mut d = self.clone();
        d.geocentric = false;
        d.ocentric = false;
        d.three_d = true;
        d.to_meter = 1.0;
        d.to_deg = 1.0;
        d.pm = 0.0;
        d.lam0 = 0.0;
        d.axis = *b"en";
        d.z_down = false;
        d
    }

    pub fn unproject_geocentric(&self, x: f64, y: f64, z: f64) -> Option<(f64, f64, f64)> {
        let (x, y) = self.unaxis(x, y);
        let z = if self.z_down { -z } else { z };
        let [lon, lat, h] = datum::from_cart(&self.ell, x * self.to_meter, y * self.to_meter, z * self.to_meter);
        (lon.is_finite() && lat.is_finite() && h.is_finite()).then_some((adjlon(lon + self.pm).to_degrees(), lat.to_degrees(), h))
    }

    pub fn project_geocentric(&self, lon: f64, lat: f64, h: f64) -> Option<(f64, f64, f64)> {
        if lat.abs() > 90.0 + 1e-10 {
            return None;
        }
        let [x, y, z] = datum::to_cart(&self.ell, lon.to_radians() - self.pm, lat.clamp(-90.0, 90.0).to_radians(), h);
        let (x, y) = self.axis(x / self.to_meter, y / self.to_meter);
        let z = z / self.to_meter * if self.z_down { -1.0 } else { 1.0 };
        (x.is_finite() && y.is_finite() && z.is_finite()).then_some((x, y, z))
    }

    /// Whether this is a rotated-pole grid (`+proj=ob_tran`).
    pub fn is_rotated_pole(&self) -> bool {
        self.rot.is_some()
    }

    /// A rotated-pole grid's CF parameters, in degrees: `grid_north_pole_latitude`,
    /// `grid_north_pole_longitude` and `north_pole_grid_longitude`.
    pub fn rotated_pole(&self) -> Option<[f64; 3]> {
        let (lat, o_lon) = self.rot?.pole();
        let lon = adjlon(self.lam0 + self.pm - std::f64::consts::PI);
        Some([lat.to_degrees(), lon.to_degrees(), o_lon.to_degrees()])
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
            None => {
                let (lam, phi) = if self.inverted_degrees {
                    // The forward step went from radians to degrees, with this factor.
                    let factor = 1.0 / 1.0_f64.to_radians();
                    (e * self.to_deg / factor, n * self.to_deg / factor)
                } else {
                    ((e * self.to_deg).to_radians(), (n * self.to_deg).to_radians())
                };
                let phi = if self.ocentric { phi.sin().atan2((1.0 - self.ell.es) * phi.cos()) } else { phi };
                // Longitudes are relative to `+lon_0` too, as PROJ's `+proj=longlat +lon_0=10`.
                let (lam, phi) = match &self.rot {
                    Some(r) => r.inv(lam, phi),
                    None => (lam, phi),
                };
                (lam + self.lam0, phi)
            }
            Some(p) => {
                // PROJ's `inv_prepare()` multiplies by `ra = 1 / a`.
                let inverse_a = 1.0 / self.projection_a;
                let x = (e * self.to_meter - self.x0) * inverse_a;
                let y = (n * self.to_meter - self.y0) * inverse_a;
                let (lam, phi) = p.inv(x, y)?;
                (lam + self.lam0, phi)
            }
        };
        if !lam.is_finite() || !phi.is_finite() {
            return None;
        }
        Some((adjlon(lam + self.pm), phi))
    }

    /// The longitude and latitude in degrees (east of Greenwich) of coordinates in a geographic
    /// CRS, as given (without the round trip through radians); `None` for a projected CRS.
    pub fn source_degrees(&self, u: f64, v: f64) -> Option<(f64, f64)> {
        if self.proj.is_some() || self.rot.is_some() {
            return None;
        }
        let (e, n) = self.unaxis(u, v);
        let pm = (self.pm + self.lam0).to_degrees();
        if self.to_deg == 1.0 && pm == 0.0 {
            return Some((e, n));
        }
        Some((e * self.to_deg + pm, n * self.to_deg))
    }

    /// The coordinates in this CRS of a geodetic point (radians, east of Greenwich).
    pub fn project(&self, lon: f64, lat: f64) -> Option<(f64, f64)> {
        let lon = lon - self.pm;
        let (e, n) = match &self.proj {
            None => {
                let (lam, phi) = match &self.rot {
                    Some(r) => r.fwd(adjlon(lon - self.lam0), lat),
                    None => (adjlon(lon - self.lam0), if self.ocentric { ((1.0 - self.ell.es) * lat.sin()).atan2(lat.cos()) } else { lat }),
                };
                if self.inverted_degrees {
                    // The forward step went from degrees to radians, with this factor.
                    let factor = 1.0_f64.to_radians();
                    (lam / factor / self.to_deg, phi / factor / self.to_deg)
                } else {
                    (lam.to_degrees() / self.to_deg, phi.to_degrees() / self.to_deg)
                }
            }
            Some(p) => {
                if lat.abs() > HALFPI + 1e-12 {
                    return None;
                }
                // PROJ's `fwd_prepare()` clamps the latitude to the poles.
                let lat = lat.clamp(-HALFPI, HALFPI);
                let (x, y) = p.fwd(adjlon(lon - self.lam0), lat)?;
                ((x * self.projection_a + self.x0) / self.to_meter, (y * self.projection_a + self.y0) / self.to_meter)
            }
        };
        if !e.is_finite() || !n.is_finite() {
            return None;
        }
        Some(self.axis(e, n))
    }

    /// Whether both CRSs are on the same datum, so that no datum transformation is needed.
    pub fn same_datum(&self, other: &Def) -> bool {
        if self.iau_datum.is_some() || other.iau_datum.is_some() {
            return self.iau_datum == other.iau_datum;
        }
        match (self.datum, other.datum) {
            (Some(a), Some(b)) => a == b,
            _ => self.ops == other.ops,
        }
    }

    /// Whether PROJ's ballpark transformation to `other` is a longitude rotation alone (both on
    /// one ellipsoid, with different prime meridians), which PROJ gives an accuracy of 0 m. Not
    /// for a geographic CRS with `+lon_0`, a PROJ-string CRS PROJ gives no accuracy to.
    pub fn rotation_only_to(&self, other: &Def) -> bool {
        let lon_0 = |d: &Def| d.is_geographic() && d.lam0 != 0.0;
        self.ell.a == other.ell.a && self.ell.es == other.ell.es && self.pm != other.pm && !lon_0(self) && !lon_0(other)
    }

    /// The datum transformations to WGS 84, in the order they are tried.
    pub fn ops(&self) -> &[Arc<Op>] {
        &self.ops
    }

    /// A definition with all registered grid operations, including unavailable grids.
    pub fn with_all_operations(&self) -> Self {
        let mut out = self.clone();
        if let Some(e) = self.geog.and_then(epsg::get)
            && !e.grid_ops.is_empty()
        {
            out.ops = e.grid_ops.clone();
        }
        out
    }

    /// The area of use, when known.
    pub fn area(&self) -> Option<[f64; 4]> {
        self.area
    }

    /// The EPSG code of the datum.
    pub fn datum_code(&self) -> Option<u32> {
        self.datum
    }

    /// The EPSG code of the geographic CRS.
    pub fn geog(&self) -> Option<u32> {
        self.geog
    }

    /// Whether the CRS is on WGS 84 itself.
    pub fn is_wgs84(&self) -> bool {
        self.datum == Some(6326)
    }

    /// Makes a 2D CRS 3D, with ellipsoidal heights (GDAL's `PromoteTo3D`).
    pub fn promote_to_3d(&mut self) {
        if !self.geocentric && self.vertical.is_none() {
            self.three_d = true;
        }
    }

    /// Whether coordinates have a height: a geographic 3D or a compound CRS.
    pub fn has_height(&self) -> bool {
        self.three_d || self.geocentric || self.vertical.is_some()
    }

    /// Whether it is a dynamic CRS.
    pub fn is_dynamic(&self) -> bool {
        self.dynamic
    }

    /// The vertical CRS of a compound CRS.
    pub fn vertical(&self) -> Option<&Arc<Vertical>> {
        self.vertical.as_ref()
    }

    /// The same CRS without its vertical part.
    pub fn horizontal(&self) -> Def {
        let mut d = self.clone();
        d.vertical = None;
        d.three_d = false;
        d
    }

    /// The bounding boxes in this CRS of a transformation's area (two across the antimeridian),
    /// from 21 points along each edge as PROJ's `proj_trans_bounds` samples them. Empty for a
    /// geographic CRS, a world-wide area or one that does not project.
    pub fn areas(&self, op: &Op) -> Vec<[f64; 4]> {
        let [w, s, e, n] = op.bounds;
        if self.is_geographic() || (e - w >= 360.0 && n - s >= 180.0) {
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

/// The horizontal and vertical parts of a compound CRS's WKT (`COMPD_CS` or `COMPOUNDCRS`).
pub(crate) fn split_compound(w: &str) -> Option<(String, String)> {
    let w = w.trim();
    let open = w.find(['[', '('])?;
    let key = w[..open].trim().to_ascii_uppercase();
    if key != "COMPD_CS" && key != "COMPOUNDCRS" {
        return None;
    }
    // The top-level arguments of the node.
    let body = &w[open + 1..w.len().checked_sub(1)?];
    let mut args = Vec::new();
    let (mut depth, mut start, mut quoted) = (0i32, 0usize, false);
    for (i, c) in body.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '[' | '(' if !quoted => depth += 1,
            ']' | ')' if !quoted => depth -= 1,
            ',' if !quoted && depth == 0 => {
                args.push(body[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    args.push(body[start..].trim());
    let head = |a: &str| a.split(['[', '(']).next().unwrap_or("").trim().to_ascii_uppercase();
    let horizontal = args.iter().find(|a| {
        matches!(
            head(a).as_str(),
            "GEOGCS"
                | "PROJCS"
                | "GEOCCS"
                | "GEOGCRS"
                | "GEOGRAPHICCRS"
                | "PROJCRS"
                | "PROJECTEDCRS"
                | "GEODCRS"
                | "GEODETICCRS"
                | "BOUNDCRS"
        )
    })?;
    let vertical = args.iter().find(|a| matches!(head(a).as_str(), "VERT_CS" | "VERTCRS" | "VERTICALCRS"))?;
    Some((horizontal.to_string(), vertical.to_string()))
}

/// The vertical CRS of a `VERT_CS` (or WKT 2 `VERTCRS`) definition: its EPSG code when it has
/// one, else its unit and the geoid grids of GDAL's `PROJ4_GRIDS` extension.
pub(crate) fn vertical_from_wkt(w: &str) -> Option<Vertical> {
    let declared = super::declared_epsg(w);
    if let Some(v) = declared.and_then(Vertical::from_epsg) {
        return Some(v);
    }
    let root = wkt1::parse(w)?;
    let to_meter = root.child("UNIT").or_else(|| root.child("LENGTHUNIT")).and_then(|u| u.num(1)).unwrap_or(1.0);
    let grids = root.children("EXTENSION").find(|e| e.name() == Some("PROJ4_GRIDS")).and_then(|e| match e.args.get(1) {
        Some(wkt1::Arg::Str(s)) => Some(s.clone()),
        _ => None,
    });
    Some(match grids {
        Some(g) => Vertical::from_geoid_grids(&g, to_meter),
        None => Vertical { code: declared, to_meter, down: false, models: Vec::new(), ballpark: true },
    })
}

/// The `TOWGS84` parameters of a WKT 1 definition, when they shift.
fn wkt_towgs84(w: &str) -> Option<Vec<f64>> {
    let root = wkt1::parse(w)?;
    let geogcs =
        if root.key.eq_ignore_ascii_case("GEOGCS") || root.key.eq_ignore_ascii_case("GEOCCS") { &root } else { root.child("GEOGCS")? };
    let t = geogcs.child("DATUM")?.child("TOWGS84")?;
    let v: Vec<f64> = (0..t.args.len()).filter_map(|i| t.num(i)).collect();
    (v.len() == 3 || v.len() == 7).then_some(v)
}

/// The `GEOGCS[...]` part of a WKT 1 definition.
fn geogcs_text(w: &str) -> Option<&str> {
    // WKT 1 delimits nodes with brackets or parentheses, and either closer ends a node, as the
    // WKT reader (and PROJ) reads it; brackets in quoted names do not count.
    let start = w.find("GEOGCS[").or_else(|| w.find("GEOGCS("))?;
    let (mut depth, mut quoted) = (0usize, false);
    for (i, c) in w[start..].char_indices() {
        match c {
            '"' => quoted = !quoted,
            '[' | '(' if !quoted => depth += 1,
            ']' | ')' if !quoted => {
                depth = depth.checked_sub(1)?;
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
#[cfg(feature = "wbprojection-fallback")]
pub(crate) fn wkt_linear_unit(w: &str) -> Option<f64> {
    let root = wkt1::parse(w)?;
    if let Some(u) = root.children("UNIT").last().or_else(|| root.children("LENGTHUNIT").last()) {
        return u.num(1);
    }
    root.children("AXIS").find_map(|a| a.child("LENGTHUNIT")).and_then(|u| u.num(1))
}

#[cfg(test)]
mod inverted_degrees_tests {
    use super::*;

    /// Radians that dividing by π/180 and multiplying by 180/π round differently.
    #[test]
    fn inverted_pipelines_divide_by_the_forward_factor() {
        let mut def = Def::new(&SpatialRef { epsg: Some(4269), ..Default::default() }).unwrap();
        let factor = 1.0_f64.to_radians();
        let radians: Vec<f64> = (1..2000).map(|i| f64::from(i) * 7.3e-4).collect();
        let plain: Vec<_> = radians.iter().map(|r| def.project(*r, 0.25).unwrap()).collect();
        def.inverted_degrees = true;
        let mut changed = 0;
        for (r, before) in radians.iter().zip(&plain) {
            let (u, v) = def.project(*r, 0.25).unwrap();
            let (lon, lat) = if def.axis[0] == b'e' { (u, v) } else { (v, u) };
            assert_eq!(lon.to_bits(), (r / factor).to_bits());
            assert_eq!(lat.to_bits(), (0.25 / factor).to_bits());
            changed += usize::from((u, v) != *before);
        }
        assert!(changed > 0, "the two conversions never differed");
        let reciprocal = 1.0 / factor;
        let (mut unprojected, mut changed) = (def.clone(), 0);
        unprojected.inverted_degrees = true;
        def.inverted_degrees = false;
        for degrees in (1..2000).map(|i| f64::from(i) * 0.05) {
            let (u, v) = def.axis(degrees, 30.0);
            let (lon, lat) = unprojected.unproject(u, v).unwrap();
            assert_eq!(lon.to_bits(), (degrees / reciprocal).to_bits());
            assert_eq!(lat.to_bits(), (30.0 / reciprocal).to_bits());
            changed += usize::from((lon, lat) != def.unproject(u, v).unwrap());
        }
        assert!(changed > 0, "the two conversions never differed");
    }
}
