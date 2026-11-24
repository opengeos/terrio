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
//     src/init.cpp
//     src/iso19111/datum.cpp
//     src/iso19111/io.cpp
//     include/proj/internal/datum_internal.hpp
//     src/iso19111/operation/coordinateoperationfactory.cpp

//! A CRS as terrio transforms coordinates in it: a projection on an ellipsoid with linear and
//! angular units, axis order, prime meridian and its datum transformations to WGS 84, built from
//! the EPSG table, a PROJ string or a WKT definition.

use super::datum::{self, Op};
use super::math::*;
use super::params::{Params, parse_dms, parse_dms_prefix};
use super::projections::{self, Proj, Setup};
use super::rotation::{ObTran, Rotation};
use super::{SpatialRef, epsg, grid, keywords, route, wkt1, wkt2};
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
    /// The EPSG geographic CRS that a CRS on a datum no CRS is on is identified as by its name
    /// ([`Def::registered_datum`]). Its transformations are not those of that CRS.
    named_geog: Option<u32>,
    /// All the operations of a datum found by its name ([`Def::datum_by_name`]), those whose
    /// grids are missing included, as `geog`'s registry entry gives them otherwise.
    all_ops: Option<Vec<Arc<Op>>>,
    /// Whether it is a geographic 3D CRS (with ellipsoidal heights).
    three_d: bool,
    /// Whether it is a dynamic CRS (on a dynamic reference frame such as ITRF2014).
    dynamic: bool,
    /// The vertical CRS of a compound CRS.
    vertical: Option<Arc<Vertical>>,
    /// The area of use (west, south, east, north in degrees), when known.
    area: Option<[f64; 4]>,
    /// The parameters of the PROJ string the definition was read from; `None` for a WKT only
    /// wbprojection reads.
    params: Option<Arc<crate::ProjParams>>,
    /// What a WKT 2 `BOUNDCRS` with a Helmert transformation, or a coded CRS with a `TOWGS84` of
    /// its own (which PROJ reads as one), is bound to.
    bound: Option<Arc<Bound>>,
    /// Set for a WKT whose CRS PROJ does not resolve to the EPSG CRS terrio reads it as.
    unresolved: Option<Unresolved>,
    /// Why a `BOUNDCRS`'s transformation is not applied, warned of by the transformers using it
    /// (#1012 item 12).
    unapplied: Option<Arc<str>>,
}

/// A WKT whose CRS PROJ 9.8.1 does not resolve to an EPSG CRS (`CRS::getResolvedCRS`,
/// `buildCRSIds`): without the CRS's ID, its name (ESRI's `GCS_` names standing for the CRS they
/// are an ESRI alias of) is not that of a single CRS of PROJ's database, or that CRS differs, its
/// axis order too (an ESRI WKT 1 has no `AXIS`, so longitude first). For a projected CRS, its
/// geographic CRS (once the projected CRS is not resolved). PROJ then searches transformations
/// through other datums, as [`route`] follows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Unresolved {
    /// Whether the datum has an ID its name is not a name of (`DATUM["Local datum",...,
    /// ID["EPSG",6806]]`), which PROJ then reaches by its ballpark, of unknown accuracy.
    pub datum_renamed: bool,
}

/// A WKT 2 `BOUNDCRS` with a Helmert transformation, or a coded CRS with a `TOWGS84` of its own.
#[derive(Debug)]
struct Bound {
    /// The EPSG code of the source CRS's own datum, when terrio identifies it: into a CRS on it,
    /// PROJ's null transformation, no shift.
    source_datum: Option<u32>,
    /// For a target other than WGS 84, the target CRS and the Helmert transformation to it, which
    /// alone takes points into a CRS on the target's datum.
    target: Option<(Def, Arc<Op>)>,
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

/// The semi-major axis and inverse flattening of a WKT 1's `SPHEROID` (of its geographic CRS).
fn wkt_spheroid(w: &str) -> Option<(f64, f64)> {
    let root = crate::parsed::tree(w)?;
    let geodetic = ["GEOGCS", "GEOCCS"].iter().any(|k| root.key.eq_ignore_ascii_case(k));
    let geog = if geodetic { &root } else { root.child("GEOGCS")? };
    geog.child("DATUM")?.child("SPHEROID")?.ellipsoid()
}

/// The datum shift PROJ's `pj_datum_set` gives a pipeline step's `+datum=` (`datums.cpp`).
#[derive(Debug, Clone, Copy)]
pub(crate) enum StepShift {
    Towgs84(&'static [f64]),
    Nadgrids(&'static str),
}

/// The shift of PROJ's `+datum=` names in a pipeline step.
pub(crate) fn step_datum_shift(name: &str) -> Option<StepShift> {
    Some(match name {
        "WGS84" | "NAD83" => StepShift::Towgs84(&[0.0, 0.0, 0.0]),
        "GGRS87" => StepShift::Towgs84(&[-199.87, 74.79, 246.62]),
        "NAD27" => StepShift::Nadgrids("@conus,@alaska,@ntv2_0.gsb,@ntv1_can.dat"),
        "potsdam" => StepShift::Nadgrids("@BETA2007.gsb"),
        "carthage" => StepShift::Towgs84(&[-263.0, 6.0, 431.0]),
        "hermannskogel" => StepShift::Towgs84(&[577.326, 90.129, 463.919, 5.137, 1.474, 5.297, 2.4232]),
        "ire65" => StepShift::Towgs84(&[482.530, -130.596, 564.557, -1.042, -0.214, -0.631, 8.15]),
        "nzgd49" => StepShift::Towgs84(&[59.47, -5.04, 187.44, 0.47, -0.1, 1.024, -4.5993]),
        "OSGB36" => StepShift::Towgs84(&[446.448, -125.157, 542.060, 0.1502, 0.2470, 0.8421, -20.4894]),
        _ => return None,
    })
}

/// The ellipsoid of a CRS given by a PROJ string, as PROJ 9.8.1 reads it with `+type=crs`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProjEllipsoid {
    /// The CRS's ellipsoid (PROJ's `buildDatum`).
    pub crs: Ell,
    /// The figure the CRS's coordinates are computed on, a sphere under a spherification flag
    /// but `+R_C`.
    pub figure: Ell,
    /// Whether `+R_C` is the spherification flag, which the projection applies.
    pub conformal: bool,
    /// Whether PROJ computes on the string as a pipeline step reads it: for a spherification
    /// flag its CRS parser does not take, or for a WKT's `PROJ4` extension.
    pub as_step: bool,
    /// Whether `+R`, `+a` or a shape parameter is given. PROJ's CRS parser then drops `+datum`
    /// and puts the CRS on an unnamed datum of the resulting ellipsoid.
    pub reshaped: bool,
}

/// PROJ's spherification flags, in the order `pj_ellipsoid` looks for them: only the first
/// present applies.
const SPHERIFICATIONS: [&str; 8] = ["R_A", "R_V", "R_a", "R_g", "R_h", "R_lat_a", "R_lat_g", "R_C"];

/// The ellipsoid of a CRS's PROJ string `p` of method `method`, as PROJ 9.8.1 reads it with
/// `+type=crs`, or why PROJ refuses it.
///
/// PROJ first reads the string as a pipeline step (`pj_ellipsoid`): `+R` alone, else `+ellps`
/// (or `+datum`'s ellipsoid), whose size `+a` replaces, keeping its eccentricity, and whose shape
/// the first of `+rf`, `+f`, `+es`, `+e` and `+b` replaces. It drops the parameters it does not
/// use: the other shape parameters, `+ellps` and `+a` beside `+R`. Its CRS parser then reads what
/// is left (`buildDatum`): `+R`, else `+datum`'s ellipsoid (not `+ellps`), else `+ellps`', with
/// `+a`, which keeps the ellipsoid's inverse flattening or, for one PROJ defines by its
/// semi-minor axis (`clrk66`, `sphere`), that axis; only `+b`, or `+rf` on an ellipsoid of an
/// inverse flattening, changes its shape. Without a named ellipsoid, `+a` and the shape
/// parameter give the ellipsoid. That is the CRS's ellipsoid, on which the method's
/// coordinates are computed, but where a spherification flag that the parser does not take as
/// part of the method (all but `+R_A` for `laea` and `cea`) keeps the string as
/// the step reads it. `+f=0` as written makes orthographic spherical on the CRS's named
/// ellipsoid or its axes, not on one PROJ writes by its inverse flattening.
///
/// With `extension`, the string is a WKT's `PROJ4` extension, which PROJ computes on as a pipeline
/// step whatever its parameters: its figure is the step's.
pub(crate) fn proj_ellipsoid(p: &Params, method: &str, extension: bool) -> std::result::Result<ProjEllipsoid, String> {
    let positive = |key: &str| p.f64(key).filter(|v| v.is_finite() && *v > 0.0).ok_or_else(|| format!("invalid value for {key}"));
    // `+R` overrides everything else.
    if p.has("R") {
        let sphere = Ell::from_a_rf(positive("R")?, 0.0);
        return Ok(ProjEllipsoid { crs: sphere, figure: sphere, conformal: false, as_step: extension, reshaped: true });
    }
    let a = if p.has("a") { Some(positive("a")?) } else { None };
    let shape = ["rf", "f", "es", "e", "b"].into_iter().find(|k| p.has(k));
    let datum = p.str("datum").and_then(datum_ellps).and_then(Ell::named_definition);
    let ellps = p.str("ellps").and_then(Ell::named_definition);
    // PROJ's CRSs are on WGS 84 without ellipsoid parameters.
    let default = (a.is_none() && shape.is_none()).then_some((6_378_137.0, 298.257_223_563));
    let named = |(a, v): (f64, f64)| if v < 0.0 { Ell::from_a_b(a, -v) } else { Ell::from_a_rf(a, v) };
    // The step: `+datum` gives `+ellps` when there is none, and `pj_init` GRS 1980 without either.
    let step_base = ellps.or(datum).or(default.map(|_| (6_378_137.0, 298.257_222_101))).map(named);
    let Some(size) = a.or(step_base.map(|e| e.a)) else {
        return Err(format!("+{} without a semi-major axis", shape.unwrap_or("a")));
    };
    // The eccentricity squared of a shape parameter's value on a semi-major axis, as PROJ checks it.
    let eccentricity = |key: &str, v: f64, a: f64| -> std::result::Result<f64, String> {
        let flattening = |f: f64| 2.0 * f - f * f;
        let es = match key {
            "rf" if v.is_finite() && v > 0.0 => flattening(1.0 / v),
            "f" if v.is_finite() && v >= 0.0 => flattening(v),
            "es" if (0.0..1.0).contains(&v) => v,
            "e" if (0.0..1.0).contains(&v) => v * v,
            "b" if v.is_finite() && v > 0.0 => flattening((a - v) / a),
            _ => return Err(format!("invalid value for {key}")),
        };
        if es >= 0.0 { Ok(es) } else { Err("invalid eccentricity".into()) }
    };
    // The step reads values as C's `atof` does; the CRS parser, values it uses, as numbers.
    let atof = |key: &str| p.str(key).and_then(leading_number).unwrap_or(0.0);
    let number = |key: &str| p.f64(key).unwrap_or_else(|| atof(key));
    let step = match shape {
        Some(key) => Ell::from_a_es(size, eccentricity(key, atof(key), size)?),
        None => Ell::from_a_es(size, step_base.map_or(0.0, |e| e.es)),
    };
    // The CRS: a datum's ellipsoid, which PROJ defines by its inverse flattening (that of
    // `clrk66` for NAD27, not its semi-minor axis), else `+ellps`'.
    let rf = |e: Ell| if e.es == 0.0 { 0.0 } else { 1.0 / (1.0 - e.one_es.sqrt()) };
    // A WKT's extension has no CRS parser's reading (PROJ reports the WKT's own ellipsoid): its
    // step's is the CRS's (`+ellps=sphere +a=6370000`, which the parser refuses, is a sphere).
    let (crs, by_rf) = match datum.map(|d| (d, true)).or(ellps.map(|e| (e, false))).or(default.map(|d| (d, false))) {
        _ if extension => (step, false),
        Some(((base_a, v), of_datum)) => {
            let by_axis = v < 0.0 && !of_datum;
            // `+a` of the ellipsoid's own semi-major axis leaves it unchanged.
            match (shape, a.filter(|a| *a != base_a)) {
                (Some("b"), _) => (Ell::from_a_b(a.unwrap_or(base_a), number("b")), false),
                (Some("rf"), a) if !by_axis => (Ell::from_a_rf(a.unwrap_or(base_a), number("rf")), true),
                // Unchanged, the named ellipsoid itself, bit for bit.
                (_, None) => (named((base_a, v)), v > 0.0),
                (_, Some(a)) if by_axis => (Ell::from_a_b(a, -v), false),
                (_, Some(a)) => (Ell::from_a_rf(a, if v < 0.0 { rf(named((base_a, v))) } else { v }), true),
            }
        }
        None => match shape {
            // Its axes as PROJ's `createTwoAxis` keeps them.
            Some("b") => (eccentricity("b", number("b"), size).map(|_| Ell::from_a_b(size, number("b")))?, false),
            Some(key) => (Ell::from_a_es(size, eccentricity(key, number(key), size)?), true),
            None => (Ell::from_a_rf(size, 0.0), false),
        },
    };
    let flag = SPHERIFICATIONS.into_iter().find(|k| p.has(k));
    // The parser takes `+R_A` for the spherical variants of `laea` and `cea` (methods of two
    // variants), and reads parameter names regardless of case: `+R_a` is `+R_A` to it. Miller
    // and van der Grinten are spherical whatever, and a `+R_A` given makes it keep the extension.
    let in_method = !extension && matches!(flag, Some("R_A" | "R_a")) && matches!(method, "laea" | "cea");
    let flag = if in_method { Some("R_A") } else { flag };
    let as_step = extension || (flag.is_some() && !in_method);
    let mut figure = if as_step { step } else { crs };
    // Spherical orthographic (`+f=0`), on whatever ellipsoid its step is written with: by name,
    // by its axes or as a sphere it is a sphere; by its inverse flattening, which comes first
    // among shape parameters, it is not.
    let spherical_ortho =
        method == "ortho" && !as_step && shape == Some("f") && p.str("f") == Some("0") && !(by_rf && crs.es != 0.0 && !crs.is_proj_named());
    if spherical_ortho {
        figure = Ell::from_a_rf(crs.a, 0.0);
    }
    let (r, es, b) = (figure.a, figure.es, figure.a * figure.one_es.sqrt());
    let radius = match flag {
        // A sphere of the ellipsoid's area (`+R_A`) or volume (`+R_V`), by PROJ's series.
        Some("R_A") => r * (1.0 - es * (1.0 / 6.0 + es * (17.0 / 360.0 + es * 67.0 / 3024.0))),
        Some("R_V") => r * (1.0 - es * (1.0 / 6.0 + es * (5.0 / 72.0 + es * 55.0 / 1296.0))),
        Some("R_a") => (r + b) / 2.0,
        Some("R_g") => (r * b).sqrt(),
        Some("R_h") => 2.0 * r * b / (r + b),
        // The arithmetic or geometric mean of the radii of curvature at a latitude.
        Some(key @ ("R_lat_a" | "R_lat_g")) => {
            // Read as a prefix, as PROJ 9.8.1's `ell_set` reads it (`40.5NS` is 40.5°); an empty
            // value is refused, as by PROJ.
            let value = p.str(key);
            let lat = value.and_then(parse_dms_prefix).unwrap_or(0.0).to_radians();
            if value.is_some_and(|v| v.trim().is_empty()) || lat.abs() > HALFPI {
                return Err(format!("invalid value for {key}"));
            }
            let t = 1.0 - es * lat.sin().powi(2);
            if key == "R_lat_a" { r * (1.0 - es + t) / (2.0 * t * t.sqrt()) } else { r * (1.0 - es).sqrt() / t }
        }
        _ => r,
    };
    if radius != r {
        figure = Ell::from_a_rf(radius, 0.0);
    }
    let reshaped = a.is_some() || shape.is_some();
    Ok(ProjEllipsoid { crs, figure, conformal: flag == Some("R_C"), as_step, reshaped })
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

/// The definitions built, by the hash of their CRS (taken before the lock) and then the CRS, while
/// the grids found stay the same ([`grid::epoch`]), with the bytes they count against the memo's
/// bound.
#[derive(Default)]
struct Memo {
    epoch: (u64, u64),
    defs: std::collections::HashMap<u64, Vec<(SpatialRef, Arc<Def>)>>,
    bytes: usize,
}

/// The keys of the memo's hash, drawn once per process. Drawn after the host's `before_lock`,
/// and by [`hold_for_fork`] before a fork, so that no child starts while they are being drawn.
pub(crate) fn memo_keys() -> &'static std::collections::hash_map::RandomState {
    MEMO_KEYS.get().unwrap_or_else(|| {
        crate::host::before_lock();
        MEMO_KEYS.get_or_init(Default::default)
    })
}

static MEMO_KEYS: std::sync::OnceLock<std::collections::hash_map::RandomState> = std::sync::OnceLock::new();

/// The hash of a CRS in the memo.
fn memo_hash(s: &SpatialRef) -> u64 {
    use std::hash::BuildHasher;
    memo_keys().hash_one(s)
}

fn memo() -> &'static std::sync::Mutex<Memo> {
    crate::host::before_lock();
    static M: std::sync::OnceLock<std::sync::Mutex<Memo>> = std::sync::OnceLock::new();
    M.get_or_init(Default::default)
}

/// Forgets the definitions built, for [`crate::reset_grid_cache`] and [`crate::register_grid`]:
/// so that the grids they hold are let go, without waiting for the next definition built.
/// Dropped after the lock is released.
pub(crate) fn forget_definitions() {
    let old = std::mem::take(&mut *memo().lock().unwrap_or_else(std::sync::PoisonError::into_inner));
    drop(old);
}

/// Holds the memo of definitions across a fork, for [`crate::host::hold_for_fork`], with its hash
/// keys drawn first.
#[cfg(all(unix, not(target_family = "wasm")))]
pub(crate) fn hold_for_fork(held: &mut Vec<Box<dyn std::any::Any>>) {
    memo_keys();
    held.push(Box::new(memo().lock().unwrap_or_else(std::sync::PoisonError::into_inner)));
}

impl Def {
    /// The definition of a CRS: from the EPSG (or ESRI) table when it has a code there, else
    /// from its PROJ string or WKT. A copy of the one kept for the CRS ([`Def::shared`]).
    pub fn new(s: &SpatialRef) -> Result<Self> {
        Self::shared(s).map(|d| (*d).clone())
    }

    /// The definition of a CRS, kept for the whole `SpatialRef`, as about 25 of its accessors
    /// need it.
    ///
    /// It is built once while the grids found stay the same: until [`crate::reset_grid_cache`],
    /// the settings that call it, or [`crate::register_grid`]. The transformations it holds
    /// depend on those grids. A change of `TERRIO_NETWORK` or `PROJ_NETWORK` is seen at the next
    /// reset, as by the grids' availability cache.
    ///
    /// The memo holds up to 1 MiB, counting the WKT and PROJ text, the definition's own size and
    /// its lists of transformations. It does not count what definitions share (transformations,
    /// projections) nor a projection's parameters. When full, it is emptied and fills again. A
    /// definition larger than that, one built under `grid::without_grids` (where no grid counts
    /// as found), and a failure are not kept.
    pub(crate) fn shared(s: &SpatialRef) -> Result<Arc<Self>> {
        if grid::without_grids_active() {
            return Self::build(s).map(Arc::new);
        }
        let (epoch, hash) = (grid::epoch(), memo_hash(s));
        {
            let m = memo().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if m.epoch == epoch
                && let Some((_, d)) = m.defs.get(&hash).and_then(|v| v.iter().find(|(k, _)| k == s))
            {
                return Ok(d.clone());
            }
        }
        // Built without the lock: a definition may need others (a compound CRS's horizontal one).
        let d = Arc::new(Self::build(s)?);
        let ops = d.ops.len() + d.all_ops.as_ref().map_or(0, Vec::len);
        let size = s.wkt.as_ref().map_or(0, String::len)
            + s.proj4.as_ref().map_or(0, String::len)
            + std::mem::size_of::<Self>()
            + ops * std::mem::size_of::<Arc<Op>>();
        if size > 1 << 20 {
            return Ok(d);
        }
        // The definitions forgotten are dropped after the lock is released.
        let mut forgotten = None;
        {
            let mut m = memo().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            // Not kept when the grids changed meanwhile: it may hold transformations they no
            // longer give.
            if epoch == grid::epoch() {
                if m.epoch != epoch || m.bytes + size > 1 << 20 {
                    forgotten = Some(std::mem::replace(&mut *m, Memo { epoch, ..Default::default() }));
                }
                let same = m.defs.entry(hash).or_default();
                if !same.iter().any(|(k, _)| k == s) {
                    same.push((s.clone(), d.clone()));
                    m.bytes += size;
                }
            }
        }
        drop(forgotten);
        Ok(d)
    }

    /// The definition of a CRS, built anew (see [`Def::new`]).
    fn build(s: &SpatialRef) -> Result<Self> {
        // A retained engineering definition must not become an Earth CRS just because
        // its root carries a registered identifier. Keep explicit PROJ extensions handled
        // by the WKT path, before considering the authority registry.
        if let Some(wkt) = &s.wkt
            && keywords::is(keywords::root_key(wkt), keywords::ENGINEERING_CRS)
        {
            if let Some(proj) = &s.proj4 {
                let mut definition = Self::from_proj(proj)?;
                definition.tag_iau(s)?;
                return Ok(definition);
            }
            return Self::from_wkt(wkt);
        }
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
            // over a PROJ4 extension, which describes the whole CRS). Not over Web Mercator's
            // `+nadgrids=@null` (its registry WKT's extension), which wins over `+towgs84` as in a
            // PROJ string: PROJ 9.8.1 places ESRI's WGS_1984_Web_Mercator_Auxiliary_Sphere with a
            // TOWGS84, zero or not, as EPSG:3857 (a shift on the projection's sphere put 45°N 30 km
            // away).
            if s.proj4.is_none()
                && let Some(w) = s.wkt.as_deref().filter(|w| *w != e.wkt && super::proj4_extension(w).is_none())
                && let Some(t) = wkt_towgs84(w)
                && !super::proj4_extension(&e.wkt).is_some_and(|p| p.split_whitespace().any(|t| t == "+nadgrids=@null"))
            {
                d.ops = vec![Arc::new(Op::world(datum::towgs84_steps(d.ell, &t)))];
                // PROJ reads it as a `BOUNDCRS` to WGS 84 over the code's datum, into which it
                // applies no shift: NAD83 / UTM zone 16N with `TOWGS84[0,0,0]` to NAD83
                // (EPSG:4269) is the inverse projection alone in PROJ 9.7.1, not a way through
                // WGS 84 that may pick a grid (us_noaa_ohhpgn.tif, 0.2 m in Ohio). Likewise on WGS
                // 84 itself into EPSG:4326, and OSGB36 into EPSG:27700. A code without a datum has
                // no source datum to go into.
                if d.datum.is_some() {
                    d.bound = Some(Arc::new(Bound { source_datum: d.datum, target: None }));
                }
                d.datum = None;
                d.geog = None;
            }
            // Likewise a WKT 2 `BOUNDCRS` over the code with a Helmert transformation, to WGS 84
            // or to another CRS, as PROJ uses it (not a grid's, whose datum stays the code's).
            // One the reader refuses (a Helmert transformation PROJ does not apply, none at all)
            // is refused here too, not read as the code without its shift.
            let bound = match s.wkt.as_deref().filter(|w| s.proj4.is_none() && *w != e.wkt && is_bound_crs(w)) {
                Some(w) => Some(Self::from_wkt(w)?),
                None => None,
            };
            if let Some(bound) = bound
                && bound.datum.is_none()
            {
                // The bound's transformations replace the code's, unless its target has none to
                // WGS 84: the bound CRS then has the source's own, with its geographic CRS (so
                // `geog` is set), and the code's stay, as in PROJ.
                if !bound.ops.is_empty() && bound.geog.is_none() {
                    d.ops = bound.ops;
                    d.datum = None;
                    d.geog = None;
                }
                d.bound = bound.bound;
            }
            // Not the registry's own WKT, nor one with the CRS's ID, which PROJ resolves. Nor one
            // with a PROJ4 extension, which GDAL hands PROJ as its PROJ string, so that no name is
            // resolved.
            if s.proj4.is_none()
                && s.epsg.is_some()
                && d.geog.is_some()
                && let Some(w) =
                    s.wkt.as_deref().filter(|w| *w != e.wkt && super::declared_epsg(w).is_none() && super::proj4_extension(w).is_none())
            {
                d.record_proj_resolution(w, s.epsg);
            }
            return Ok(d);
        }
        if let Some(p) = &s.proj4 {
            // A PROJ string given is read by PROJ's CRS rules; a WKT's PROJ4 extension, kept
            // through the WKT, by a pipeline step's (`Self::from_wkt`).
            let mut d = Self::from_proj(p)?;
            d.tag_iau(s)?;
            return Ok(d);
        }
        if let Some(w) = &s.wkt {
            let mut d = Self::from_wkt(w)?;
            d.tag_iau(s)?;
            // A WKT without EPSG code on the datum of one, which PROJ resolves by its name only.
            if d.geog.is_some() && d.bound.is_none() && super::declared_epsg(w).is_none() {
                d.record_proj_resolution(w, None);
            }
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
                let root = crate::parsed::tree(w)?;
                root.geodetic_base().unwrap_or(&root).datum().cloned()
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

    /// The NAIF code of the body the definition declares: the Earth for an EPSG or ESRI code,
    /// that of an IAU_2015 CRS. (`Def::new` declares every table entry on the Earth: the ESRI table
    /// has no CRS of another body, as `scripts/gen_esri_table.py` leaves out those PROJ can't
    /// transform from WGS 84.)
    pub fn declared_body(&self) -> Option<u32> {
        self.body
    }

    /// A declared body, or PROJ's Earth-radius heuristic for custom ellipsoids.
    pub fn celestial_body(&self) -> Option<u32> {
        self.body.or_else(|| earth_sized(self.ell.a).then_some(399))
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
        let d = Self::from_params(s, &p, false);
        // As PROJ refuses a value it cannot read, where terrio would take the parameter's default.
        match p.invalid().into_iter().find(|key| !unchecked(&p, key)) {
            Some(key) => Err(Error::Crs(format!("invalid value for {key} in '{s}'"))),
            None => d,
        }
    }

    /// The definition of a WKT's `PROJ4` extension. PROJ takes the extension as it is, without
    /// refusing the values it cannot read (`+lat_ts=abc` is read as 0), so this does not either.
    /// Numeric scale/unit checks still apply. Fully numeric datum lists require three or seven
    /// elements; unreadable lists retain the legacy behavior of ignoring the shift.
    ///
    /// PROJ 9.8.1 computes on the extension as a pipeline step, so its ellipsoid is the step's:
    /// `+ellps=WGS84 +f=0` is a sphere, `+ellps=clrk66 +a=6400000` keeps Clarke 1866's
    /// eccentricity. An ellipsoid the step refuses (`+rf=abc` or `+rf=0`, read as 0, `+es=1`, a
    /// shape without a size) is refused, as PROJ refuses to transform with it.
    pub fn from_proj_extension(s: &str) -> Result<Self> {
        Self::from_params(s, &Params::parse_extension(s), true)
    }

    /// [`Self::from_proj`] of parsed parameters.
    fn from_params(s: &str, p: &Params, extension: bool) -> Result<Self> {
        p.validate_quoting()?;
        p.validate_flags()?;
        if let Some(init) = p.str("init") {
            let code = init.to_ascii_lowercase().strip_prefix("epsg:").and_then(|c| c.parse().ok());
            if let Some(code) = code {
                return Self::new(&SpatialRef { epsg: Some(code), ..Default::default() });
            }
        }
        let name = p.str("proj").ok_or_else(|| Error::Crs(format!("no +proj in '{s}'")))?;
        let datum_name = p.str("datum");
        // Names PROJ 9.8.1 does not know are refused, as it refuses them, in a WKT's `PROJ4`
        // extension too, rather than read as a default: `+ellps=foo` is not WGS 84, nor
        // `+units=feet` metres. Names are compared as written, so `wgs84`, `meter` and `M` are
        // unknown, and a `+to_meter` beside an unknown `+units` does not help.
        // Not with `+R`, which PROJ reads instead of `+ellps`.
        if let Some(e) = p.str("ellps").filter(|e| Ell::named(e).is_none() && !p.has("R")) {
            return Err(Error::Crs(format!("unknown ellipsoid {e} in '{s}'")));
        }
        if let Some(d) = datum_name.filter(|d| datum_ellps(d).is_none()) {
            return Err(Error::Crs(format!("unknown datum {d} in '{s}'")));
        }
        for key in ["units", "vunits"] {
            if let Some(u) = p.str(key).filter(|u| unit_to_meter(u).is_none()) {
                return Err(Error::Crs(format!("invalid value for {key} ({u}) in '{s}'")));
            }
        }
        // `+lat_0`, `+lon_0`, `+x_0` and `+y_0` that are not finite numbers (or angles) are
        // refused for the methods PROJ 9.8.1's CRS parser maps them for, and ignored for the
        // others: `+proj=tmerc +lat_0=abc` is refused, `+proj=robin +lat_0=abc` read without it.
        // A WKT's `PROJ4` extension is not checked, as PROJ takes it as it is.
        if !extension {
            for key in ["lat_0", "lon_0", "x_0", "y_0"] {
                let unreadable = p.str(key).is_some_and(|v| match key {
                    "lat_0" | "lon_0" => parse_dms(v).is_none(),
                    _ => v.trim().parse::<f64>().ok().is_none_or(|v| !v.is_finite()),
                });
                if unreadable && proj_maps_origin(name, key) {
                    return Err(Error::Crs(format!("invalid value for {key} in '{s}'")));
                }
            }
        }
        // Without `+vunits`, `+vto_meter` gives the vertical unit and must be a positive number.
        // A WKT's `PROJ4` extension may give it as a fraction (`1200/3937`), as PROJ's pipeline
        // reads it; its CRS parser refuses one.
        if !p.has("vunits") && p.has("vto_meter") && vto_meter(p, extension).is_none() {
            return Err(Error::Crs(format!("invalid value for vto_meter in '{s}'")));
        }
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

        let shape = proj_ellipsoid(p, name, extension).map_err(|e| Error::Crs(format!("invalid ellipsoid in '{s}': {e}")))?;
        let ell = shape.figure;
        // An ellipsoid PROJ refuses (`+b` above `+a`, `+es` of 1 or more) would give NaN
        // coordinates.
        for e in [ell, shape.crs] {
            if !(e.a.is_finite() && e.a > 0.0 && (0.0..1.0).contains(&e.es)) {
                return Err(Error::Crs(format!("invalid ellipsoid in '{s}': semi-major axis {}, eccentricity squared {}", e.a, e.es)));
            }
        }
        let explicit_to_meter = p.str("to_meter").and_then(|v| match v.split_once('/') {
            Some((n, d)) => Some(n.parse::<f64>().ok()? / d.parse::<f64>().ok()?),
            None => v.parse().ok(),
        });
        // A unit terrio knows (`+units`) first, as PROJ reads them: `+to_meter` only without it,
        // and then a positive number ("Invalid value for to_meter").
        let units = p.str("units").and_then(unit_to_meter);
        if units.is_none() && p.has("to_meter") && explicit_to_meter.is_none_or(|v| !v.is_finite() || v <= 0.0) {
            return Err(Error::Crs(format!("invalid value for to_meter in '{s}'")));
        }
        let to_meter = units.or(explicit_to_meter).unwrap_or(1.0);
        let explicit_pm = p.str("pm").and_then(|v| {
            v.parse::<f64>()
                .ok()
                .or_else(|| prime_meridian(v))
                // In degrees, minutes and seconds (`2d20'14.025"E`), as PROJ reads it.
                .or_else(|| p.angle("pm").map(f64::to_degrees))
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
            p,
            ell,
            phi0: p.angle("lat_0").unwrap_or(0.0),
            lam0: p.angle("lon_0").unwrap_or(0.0),
            k0: 1.0,
            x0: p.f64("x_0").unwrap_or(0.0),
            y0: p.f64("y_0").unwrap_or(0.0),
        };
        // Check the first present scale key as PROJ's `pj_init` does: its numeric prefix (`atof`)
        // must be above 0 ("Invalid value for k"); NaN passes that test. The value used is the
        // first of `+k_0` and `+k` that is a number, as the writers read it, and must also pass
        // that check. Without a numeric `+k`, `+k_0=0.5r` is 1, where PROJ takes 0.5 or refuses it.
        let k = p.str("k_0").or_else(|| p.str("k"));
        if k.is_some_and(|k| !leading_number(k).is_some_and(|k| k > 0.0 || k.is_nan())) {
            return Err(Error::Crs(format!("invalid value for k_0 in '{s}'")));
        }
        setup.k0 = p.f64("k_0").or_else(|| p.f64("k")).unwrap_or(1.0);
        if setup.k0 <= 0.0 {
            return Err(Error::Crs(format!("invalid value for k_0 in '{s}'")));
        }
        // As PROJ refuses it ("Invalid value for lat_0: |lat_0| should be <= 90°").
        if setup.phi0.abs() > HALFPI + 1e-12 || !setup.phi0.is_finite() {
            return Err(Error::Crs(format!("invalid lat_0: {} degrees", setup.phi0.to_degrees())));
        }
        let is_longlat = |n: &str| matches!(n, "longlat" | "latlong" | "lonlat" | "latlon");
        // `ob_tran`'s rotation, of a rotated-pole grid (`+o_proj=longlat`) or of a projection.
        let ob_tran = if name == "ob_tran" { Some(Rotation::from_params(p)?) } else { None };
        let o_proj = p.str("o_proj").filter(|_| ob_tran.is_some());
        let rot = ob_tran.filter(|_| o_proj.is_some_and(is_longlat));
        let geocentric = name == "geocent";
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
        let conformal_ellipsoid = shape.conformal.then_some(ell);
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
        // As the PROJ string gives them, with PROJ's defaults. UTM and UPS have theirs from the
        // zone and the hemisphere, as PROJ's conversion does.
        let zoned = matches!(name, "utm" | "ups");
        let degrees = |key: &str, set: f64| if zoned { set.to_degrees() } else { p.degrees(key).unwrap_or(0.0) };
        let params = crate::ProjParams {
            // PROJ's name of the aliases it writes under another (`latlong`, `lonlat` and `latlon`
            // are `longlat`, `etmerc` is `tmerc` and `webmerc` is `merc`).
            method: match name {
                n if is_longlat(n) => "longlat",
                "etmerc" => "tmerc",
                "webmerc" => "merc",
                n => n,
            }
            .to_string(),
            lon_0: if name == "utm" {
                // The zone's central meridian in whole degrees, which its radians don't convert
                // back to exactly: zone z (from 1) is at 6z - 183°.
                let zone = ((setup.lam0 + std::f64::consts::PI) * 30.0 / std::f64::consts::PI + 0.5).round();
                zone * 6.0 - 183.0
            } else {
                degrees("lon_0", setup.lam0)
            },
            lat_0: degrees("lat_0", setup.phi0),
            k_0: if zoned { setup.k0 } else { p.f64("k_0").or_else(|| p.f64("k")).unwrap_or(1.0) },
            x_0: if zoned { setup.x0 } else { p.f64("x_0").unwrap_or(0.0) },
            y_0: if zoned { setup.y0 } else { p.f64("y_0").unwrap_or(0.0) },
            prime_meridian: pm,
            // The ellipsoid PROJ 9.8.1 reports for the CRS, not the sphere a spherification flag
            // computes on.
            a: shape.crs.a,
            b: shape.crs.a * shape.crs.one_es.sqrt(),
            // The coordinates of a geographic CRS are angles, whatever `+units` says.
            to_meter: if geographic { 1.0 } else { to_meter },
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
            pm: proj_prime_meridian(pm).to_radians(),
            axis,
            ops: Vec::new(),
            datum: None,
            geog: None,
            named_geog: None,
            all_ops: None,
            three_d: false,
            dynamic: false,
            vertical: None,
            area: None,
            params: Some(Arc::new(params)),
            bound: None,
            unresolved: None,
            unapplied: None,
        };
        // The datum transformation.
        // A pipeline step's `+datum=` is the shift PROJ's `pj_datum_set` gives it, from WGS 84 to
        // the step's figure, unless the string gives its own; none for a geocentric step, nor
        // under `+R_C` in a PROJ string (PROJ 9.8.1 then keeps the datum's transformations, but
        // not in a WKT's extension).
        // `pj_datum_set` takes the first `+nadgrids` of the string with the datum's appended,
        // else the first `+towgs84`: the string's own `+nadgrids` wins, its `+towgs84` only over a
        // datum defined by one (NAD27's grids win over `+towgs84=0,0,0`).
        let step_shift = datum_name
            .filter(|_| shape.as_step && (extension || !shape.conformal) && !p.has("nadgrids") && name != "geocent")
            .and_then(step_datum_shift)
            .filter(|d| matches!(d, StepShift::Nadgrids(_)) || !p.has("towgs84"));
        // A step's `+R_C` makes its figure the conformal sphere at the equator, onto which it is
        // shifted.
        let shift_figure = if extension && shape.conformal { Ell::from_a_rf(ell.a * ell.one_es.sqrt(), 0.0) } else { ell };
        // `pj_init` takes a null shift on WGS 84 or GRS 1980 (its test of `a` and `es`) as WGS 84.
        let wgs84_figure = shift_figure.a == 6_378_137.0 && (shift_figure.es - 0.006_694_379_990).abs() < 0.000_000_000_050;
        let (step_towgs84, step_nadgrids) = match step_shift {
            Some(StepShift::Towgs84(t)) => (Some(t.to_vec()), None),
            Some(StepShift::Nadgrids(g)) => (None, Some(g)),
            None => (None, None),
        };
        let step_on_wgs84 = step_towgs84.as_ref().is_some_and(|t| wgs84_figure && t.iter().all(|v| *v == 0.0));
        let towgs84 = p.list("towgs84").or(step_towgs84);
        if towgs84.as_ref().is_some_and(|v| v.len() != 3 && v.len() != 7) {
            return Err(Error::Crs(format!("towgs84 needs 3 or 7 values in '{s}'")));
        }
        // `+nadgrids` wins over `+towgs84` and the datum's transformations (`pj_datum_set`, and a
        // bound CRS of PROJ's CRS parser), `@null` alone being no shift at all.
        let grids = p.str("nadgrids").or(step_nadgrids);
        let null_grids = grids.is_some_and(|g| g.split(',').all(|n| n.trim_start_matches('@') == "null"));
        let nadgrids = grids.filter(|_| !null_grids);
        if step_on_wgs84 {
            d.datum = Some(6326);
            d.geog = Some(4326);
        } else if null_grids {
            // No shift to WGS 84, on the datum still: a CRS on it is reached without one (PROJ's
            // bound CRS over the datum, `+datum=OSGB36 +nadgrids=@null` into EPSG:4277), another
            // through WGS 84 by the null shift, not by the datum's own transformations (so its
            // datum's code is kept, not its geographic CRS's, by which routes take those).
            // Without a datum, no operation is no shift already (EPSG:3857's null grid).
            if let Some(code) = datum_name.and_then(datum_geog).filter(|_| !shape.reshaped)
                && let Some(e) = epsg::get(code)
            {
                d.ops = vec![Arc::new(Op::world(datum::nadgrids_steps("@null")))];
                d.datum = (e.datum != 0).then_some(e.datum);
            }
        } else if let Some(g) = nadgrids {
            d.ops = vec![Arc::new(Op::world(datum::nadgrids_steps(g)))];
        } else if let Some(t) = towgs84 {
            // Zero translation still converts between different ellipsoids. On
            // WGS 84 itself it is exactly the WGS 84 datum, without a Cartesian round trip.
            let wgs84 = Ell::wgs84();
            if t.iter().all(|v| *v == 0.0) && shift_figure.a == wgs84.a && shift_figure.es == wgs84.es {
                d.datum = Some(6326);
                d.geog = Some(4326);
            } else {
                d.ops = vec![Arc::new(Op::world(datum::towgs84_steps(shift_figure, &t)))];
            }
        } else if let Some(code) = datum_name.and_then(datum_geog).filter(|_| !shape.reshaped && step_shift.is_none())
            && let Some(e) = epsg::get(code)
        {
            d.ops = epsg::usable(&e.ops, &e.grid_ops);
            d.datum = (e.datum != 0).then_some(e.datum);
            d.geog = Some(code);
            d.dynamic = e.dynamic;
        }
        if let Some(g) = p.str("geoidgrids") {
            // `+vunits` first, then `+vto_meter` (both checked above), as PROJ reads them.
            let to_meter = p.str("vunits").and_then(unit_to_meter).or_else(|| vto_meter(p, extension)).unwrap_or(1.0);
            d.vertical = Some(Arc::new(Vertical::from_geoid_grids(g, to_meter)));
        }
        Ok(d)
    }

    /// The datum transformations of the EPSG geographic CRS a WKT's datum is identified as
    /// (its code, else its name), for a definition without any of its own.
    fn registered_datum(&mut self, c: &wkt1::Converted, geogcs_text: Option<&str>) {
        if !self.ops.is_empty() {
            return;
        }
        // The geographic CRS's own code only when its content is that CRS (a local datum under
        // OSGB36's code does not get OSGB36's transformations), as a root code.
        // WKT 2: its base CRS node, whose datum keeps its ID (`BASEGEOGCRS` read as `GEOGCRS`).
        // Else the node as written back (a WKT 1 `geogcs` or `GEOGCS [` the text search misses):
        // never no comparison at all.
        let own = match geogcs_text {
            Some(t) => Some(t.to_string()),
            None => {
                let t = c.geogcs.to_wkt();
                Some(t.strip_prefix("BASE").map(str::to_string).unwrap_or(t))
            }
        };
        // Or on its datum but another prime meridian, which the definition applies itself.
        let fits = |g: &u32| own.as_deref().is_none_or(|t| super::declared_fits(t, *g) || super::declared_fits_but_meridian(t, *g));
        let geog = c.geog.filter(fits).or_else(|| c.geogcs.epsg().filter(fits)).or_else(|| match &c.datum {
            // WKT 2: identified loosely, so on the same ellipsoid only.
            Some(datum) => super::identify_datum(datum).filter(|g| wkt2::same_ellipsoid(*g, self.ell.a, self.ell.es)),
            None => super::identify_epsg(geogcs_text?),
        });
        let geog = geog.map(|g| epsg::get(g).filter(|e| e.proj.starts_with("+proj=geocent")).map_or(g, |e| e.geog));
        if let Some(e) = geog.and_then(epsg::get)
            && e.proj.starts_with("+proj=longlat")
        {
            self.ops = epsg::usable(&e.ops, &e.grid_ops);
            self.datum = (e.datum != 0).then_some(e.datum);
            self.geog = if e.three_d { Some(e.geog) } else { geog };
            self.dynamic = e.dynamic;
        } else if geog.is_none()
            && let Some(own) = own.as_deref()
        {
            self.datum_by_name(own);
            // A WKT 1 GEOGCS on a datum no CRS is on keeps its datum's transformations, but gives
            // the CRS PROJ 9.8.1 identifies it as by name (ESRI's GCS_Greenland_1996, GR96) as its
            // geographic CRS (`own`: the text as written, else the node as written back).
            if c.datum.is_none() {
                self.named_geog = super::identify_by_shared_name(own);
            }
        }
    }

    /// Takes the transformations of the datum a geographic CRS names when the CRS is on another
    /// ellipsoid than the datum's, and runs them on the CRS's ellipsoid, as PROJ 9.8.1 does: it
    /// finds Esri's `D_Monte_Mario` on an International 1924 ellipsoid of another axis by its name
    /// and applies "Monte Mario to WGS 84 (4)" with its geocentric step on the file's ellipsoid.
    ///
    /// The datum is found as PROJ finds it: by its `ID`; by `D_` before any name of the datum,
    /// spaces and former names included (`D_OSGB_1936`, `D_Monte Mario`); or, without `D_`, by a
    /// name written with underscores that is the datum's official name or an EPSG alias once they
    /// are spaces (`Monte_Mario`, `OSGB_1936`). Not by a name with spaces and no `D_` (`Monte
    /// Mario`, `ED50`, as PROJ's own WKT writes them), which PROJ places by a ballpark on another
    /// ellipsoid, nor by an ESRI name without its `D_` (`European_1950`).
    ///
    /// The transformations are a current geographic 2D CRS's of the datum on Greenwich (not
    /// Monte Mario (Rome)'s on the Rome meridian): CRSs on another meridian keep the route's
    /// search, which places them as PROJ does. Only those starting with a geocentric step on the
    /// datum's ellipsoid are moved onto the CRS's, as PROJ replaces the first operation's source,
    /// and a null transformation becomes the geocentric change from the CRS's ellipsoid to WGS 84's;
    /// others (grids, a concatenation starting with an offset, one registered between geocentric
    /// CRSs) are kept as registered.
    fn datum_by_name(&mut self, geogcs: &str) {
        if self.pm != 0.0 {
            return;
        }
        let Some((name, code)) = super::wkt_datum_name_and_code(geogcs) else { return };
        let by_name = if name.starts_with("D_") {
            epsg::datum_name_rank(&name).is_some()
        } else {
            name.contains('_') && epsg::datum_name_rank(&name).is_some_and(|r| r <= 1)
        };
        // A name PROJ finds wins over the datum's ID: `Greenland_1996` with `AUTHORITY["EPSG",
        // "1421"]` is the former Greenland 1996 datum (EPSG:6747), which no current CRS has, so
        // PROJ places it by a ballpark; under a name it does not find, the ID's datum.
        let datums = if by_name {
            epsg::datums_named(&name)
        } else if let Some(code) = code {
            vec![code]
        } else {
            return;
        };
        let found = datums.iter().flat_map(|d| epsg::geographic_on(*d)).copied().filter(|g| !epsg::is_deprecated(*g)).find_map(|g| {
            let e = epsg::get(g).filter(|e| e.proj.starts_with("+proj=longlat") && !e.three_d && !e.ops.is_empty())?;
            let registered = Def::new(&SpatialRef { epsg: Some(g), ..Default::default() }).ok()?;
            (registered.pm == 0.0).then_some((e, registered.ell))
        });
        let Some((e, registered)) = found else { return };
        let same = crate::fits::same_ellipsoid;
        // The geographic CRS's own ellipsoid, which a projection may not use (Esri's
        // `Mercator_Auxiliary_Sphere` projects on a sphere): on the datum's own, the CRS is
        // identified by its content.
        let Some(own) = Def::wkt_conversion(geogcs).map(|d| d.ell) else { return };
        if same(&registered, &own) {
            return;
        }
        // One registered between geocentric CRSs (ONGD14 to WGS 84 (1), NAD83(CSRS)v6 through
        // NAD83(2011)) is kept: PROJ puts a ballpark into the datum's CRS before it, which keeps the
        // point, and runs it on the datum's own ellipsoid.
        let rebase = |op: &Arc<Op>| {
            let mut op = (**op).clone();
            if !op.geocentric {
                if let Some(datum::Step::Cart { ell, inv: false }) = op.steps.first_mut()
                    && same(ell, &registered)
                {
                    *ell = own;
                } else if op.steps.is_empty() && !op.ballpark {
                    // A null transformation ("TWD97 to WGS 84 (1)"), or a concatenation of them
                    // (ETRF89 through ETRS89), is still geocentric: PROJ runs it from the file's
                    // ellipsoid to WGS 84's, which moves latitudes, and keeps heights between 2D
                    // CRSs around it.
                    op.steps = vec![datum::Step::Cart { ell: own, inv: false }, datum::Step::Cart { ell: Ell::wgs84(), inv: true }];
                    op.keeps_height = true;
                }
            }
            Arc::new(op)
        };
        self.ops = epsg::usable(&e.ops, &e.grid_ops).iter().map(rebase).collect();
        // Those with missing grids too, for the routes of every operation (`ONLY_BEST`).
        self.all_ops = (!e.grid_ops.is_empty()).then(|| e.grid_ops.iter().map(rebase).collect());
        self.datum = (e.datum != 0).then_some(e.datum);
        self.dynamic = e.dynamic;
    }

    /// The coordinate conversion of a WKT definition (without its datum), when terrio reads it: that
    /// of its `PROJ4` extension when it has one.
    pub fn wkt_conversion(w: &str) -> Option<Self> {
        if let Some(p) = super::proj4_extension(w) {
            match Self::from_proj_extension(&p) {
                Ok(d) => return Some(d),
                Err(Error::Crs(message)) if message.starts_with("unsupported projection:") => {}
                Err(_) => return None,
            }
        }
        let root = crate::parsed::tree(w)?;
        let c = if wkt2::handles(&root) { crate::model::wkt2_to_proj(w).ok()? } else { crate::model::wkt1_to_proj(w)? };
        Self::from_proj(&c.proj).ok()
    }

    /// Whether both CRSs have the same coordinate conversion (projection, units, axes and prime
    /// meridian), whatever their datums: compared on points around the projection's origin.
    pub fn same_conversion(&self, other: &Def) -> bool {
        self.same_conversion_within(other, 0.05)
    }

    /// Whether both CRSs have the same coordinate conversion, projected points within `tolerance`
    /// metres of each other (see [`Def::same_conversion`]).
    pub fn same_conversion_within(&self, other: &Def, tolerance: f64) -> bool {
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
                                if d.is_nan() || d >= tolerance {
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

    /// A definition from WKT. Datum transformations come from `TOWGS84` (or a WKT 2 `BOUNDCRS`'s
    /// Helmert transformation to WGS 84), from a `BOUNDCRS`'s Helmert transformation to another
    /// CRS followed by that CRS's transformations, else from the EPSG geographic CRS the WKT's
    /// `GEOGCS` is identified as.
    pub fn from_wkt(w: &str) -> Result<Self> {
        if let Some(compound) = super::esri_compound_wkt(w) {
            return Self::from_wkt(&compound);
        }
        if let Some((horizontal, vertical)) = split_compound(w) {
            // uncut: stored or built WKT
            let h = SpatialRef::from_wkt_text(&horizontal);
            let mut d = Self::new(&h)?;
            d.vertical = vertical_from_wkt(&vertical).map(Arc::new);
            return Ok(d);
        }
        // A WKT 1 extended with its PROJ string (`EXTENSION["PROJ4", ...]`) is that PROJ string,
        // whatever its nodes say, as GDAL and PROJ read it. Unsupported methods retain the
        // WKT-node fallback, but invalid values in a supported conversion must not fall through.
        if let Some(p) = super::proj4_extension(w) {
            match Self::from_proj_extension(&p) {
                Ok(mut d) => {
                    // The ellipsoid it reports is the WKT's, as PROJ reports it, whatever its
                    // extension computes on.
                    if let (Some(params), Some((a, rf))) = (d.params.as_mut(), wkt_spheroid(w)) {
                        let params = Arc::make_mut(params);
                        params.a = a;
                        params.b = if rf == 0.0 { a } else { a - a / rf };
                    }
                    return Ok(d);
                }
                Err(Error::Crs(message)) if message.starts_with("unsupported projection:") => {}
                Err(error) => return Err(error),
            }
        }
        // Text the WKT reader refuses (not WKT, nested deeper or longer than GDAL reads) is not
        // handed to wbprojection's parser either, whose recursion has no bound.
        let root = crate::parsed::tree(w).ok_or_else(|| Error::Crs("not a WKT definition GDAL reads".into()))?;
        // Engineering roots carry metadata, not an Earth projection. Do not send their IDs
        // through wbprojection's EPSG builder; its large frame can exhaust a WASM warp stack.
        if root.is(keywords::ENGINEERING_CRS) {
            let kind = root.key.to_ascii_lowercase().replace('_', "");
            return Err(Error::Crs(format!("unsupported projection: unsupported WKT root '{kind}'")));
        }
        let converted = if wkt2::handles(&root) { Some(crate::model::wkt2_to_proj(w)?) } else { crate::model::wkt1_to_proj(w) };
        if let Some(c) = converted {
            let mut d = Self::from_proj(&c.proj)?;
            d.unapplied = c.unapplied.as_deref().map(Arc::from);
            // A `BOUNDCRS`'s Helmert transformation that `+towgs84` cannot stand for (a full
            // rotation matrix, a Molodensky-Badekas evaluation point), as PROJ applies it.
            let helmert = c.helmert.as_ref().map(|h| datum::helmert(h.t, h.r, h.s, h.position_vector, h.exact, h.pivot));
            if let Some(step) = &helmert
                && c.bound.is_none()
            {
                d.ops = vec![Arc::new(Op::world(datum::helmert_steps_with(d.ell, Ell::wgs84(), step.clone())))];
            }
            if let Some(bound) = &c.bound {
                // A WKT 2 `BOUNDCRS` to another CRS: its Helmert transformation to that CRS, then
                // each of that CRS's transformations to WGS 84, as PROJ concatenates them. With
                // none, the source CRS's own, as PROJ uses them (else its ballpark to WGS 84).
                let wkt1::BoundTo { target, shift } = &**bound;
                let mut source = d.clone();
                source.registered_datum(&c, own_geogcs_text(w, &c));
                // A target terrio cannot read is left out, as one it cannot convert is.
                let Ok(mut t) = Self::from_proj(&target.proj) else {
                    let name = target.geogcs.name().unwrap_or("");
                    source.unapplied =
                        Some(Arc::from(format!("the BOUNDCRS transformation is not applied: its target CRS \"{name}\" is not read")));
                    return Ok(source);
                };
                t.registered_datum(target, None);
                let to_target = match helmert {
                    Some(step) => datum::helmert_steps_with(d.ell, t.ell, step),
                    None => datum::helmert_steps(d.ell, t.ell, shift),
                };
                let mut ops: Vec<Arc<Op>> = t
                    .ops
                    .iter()
                    .map(|op| {
                        // Of unknown accuracy, as the bound transformation is: PROJ then picks
                        // the smallest area holding the point.
                        let mut op = (**op).clone();
                        op.steps = to_target.iter().cloned().chain(op.steps).collect();
                        (op.accuracy, op.chain_accuracy) = (-1.0, -1.0);
                        Arc::new(op)
                    })
                    .collect();
                // In PROJ's order, which decides between those of one area ("OSGB36 to WGS 84
                // (3)" before "(2)").
                route::sort_as_proj(&mut ops);
                // Then the shift and PROJ's ballpark from the target CRS, which PROJ's nested
                // search keeps even where its search from the target code drops it (DHDN, CH1903,
                // Tokyo, NTF): it drops the ballpark only when a transformation needing no grid
                // holds the area of interest, here the world (the bound CRS has no area in
                // terrio). Where only that ballpark is left for the area of interest, the route
                // drops it, as PROJ does (`route::nested`).
                if !ops.is_empty() && !ops.iter().any(|op| op.ballpark) {
                    let accuracy = -1.0;
                    ops.push(Arc::new(Op { accuracy, chain_accuracy: accuracy, ballpark: true, ..Op::world(to_target.clone()) }));
                }
                if ops.is_empty() {
                    // The datum stays `None`: `Def::new` tells by it that a bound CRS over a code
                    // has the bound's transformations, and `on_bound_source` covers the source's.
                    (d.ops, d.geog, d.dynamic, d.all_ops) = (source.ops.clone(), source.geog, source.dynamic, source.all_ops.clone());
                } else {
                    d.ops = ops;
                }
                let helmert = Arc::new(Op { name: "bound transformation".into(), ..Op::world(to_target) });
                d.bound = Some(Arc::new(Bound { source_datum: source.datum, target: Some((t, helmert)) }));
                return Ok(d);
            }
            if c.proj.contains("+towgs84=") {
                // To WGS 84 (a `BOUNDCRS`, or a WKT 1 `TOWGS84`, which PROJ reads as one): the
                // source CRS's own datum, into which PROJ applies no shift. A WKT 1 whose datum
                // resolves to none keeps the shift into every CRS.
                let mut source = d.clone();
                source.ops.clear();
                source.registered_datum(&c, own_geogcs_text(w, &c));
                if is_bound_crs(w) || source.datum.is_some() {
                    d.bound = Some(Arc::new(Bound { source_datum: source.datum, target: None }));
                }
                return Ok(d);
            }
            if !c.proj.contains("+towgs84=") {
                d.registered_datum(&c, own_geogcs_text(w, &c));
            }
            return Ok(d);
        }
        // A WKT 1 on Polar Stereographic (variant C) the reader cannot convert (without its
        // standard parallel, geographic CRS or ellipsoid) is refused: wbprojection reads it as
        // variant B, with the false origin at the pole.
        if root
            .child("PROJECTION")
            .is_some_and(|p| p.epsg() == Some(9830) || p.name().is_some_and(|m| wkt1::norm(m) == "polarstereographicvariantc"))
        {
            return Err(Error::Crs("an incomplete Polar Stereographic (variant C) WKT".into()));
        }
        // Nor a polar stereographic WKT the reader refuses for its latitude (GDAL's without its
        // latitude of origin, a latitude that is not finite), which wbprojection would read at the
        // north pole (PROJ 9.8.1 refuses it).
        if wkt1::polar_latitude_refused(&root) {
            return Err(Error::Crs("a polar stereographic WKT without a finite latitude".into()));
        }
        // Other kinds of WKT 2 CRS and WKT 1 projections the readers do not know: wbprojection's
        // parser. A WKT 2 geographic or projected CRS the reader refuses is not handed to it: its
        // WKT 2 parameters and method names would be read wrongly, silently (#123).
        #[cfg(feature = "wbprojection-fallback")]
        {
            // Without a root code that is not its content's, nor root codes other than the one it
            // declares, which wbprojection's parser would read as the CRS (an unknown method
            // declaring EPSG:27700).
            let stripped = super::without_unfit_root_code(w);
            let crs = wbprojection::Crs::from_wkt(stripped.as_deref().unwrap_or(w)).map_err(|e| Error::Crs(format!("{e}")))?;
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
                named_geog: None,
                all_ops: None,
                three_d: false,
                dynamic: false,
                vertical: None,
                area: None,
                params: None,
                bound: None,
                unresolved: None,
                unapplied: None,
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

    /// Longitude revolution in the native angular unit, for a geographic CRS.
    pub fn longitude_period(&self) -> Option<f64> {
        self.is_geographic().then_some(360.0 / self.to_deg.abs()).filter(|v| v.is_finite() && *v > 0.0)
    }

    /// Endpoints of the full longitude range at a pole, in this CRS's units and axis order.
    pub fn pole_bounds(&self, latitude: f64) -> [(f64, f64); 2] {
        let half = 180.0 / self.to_deg;
        let y = latitude.to_degrees() / self.to_deg;
        [self.axis(-half, y), self.axis(half, y)]
    }
    /// The prime meridian's longitude east of Greenwich, in radians.
    pub(crate) fn pm(&self) -> f64 {
        self.pm
    }

    pub(crate) fn set_pm(&mut self, pm: f64) {
        self.pm = pm;
    }

    /// Whether this is a geographic CRS. A rotated-pole grid is not: its coordinates are in
    /// degrees, but are not longitudes and latitudes.
    pub fn is_geographic(&self) -> bool {
        self.proj.is_none() && self.rot.is_none() && !self.geocentric
    }

    /// Whether this definition has a map projection.
    pub fn is_projected(&self) -> bool {
        self.proj.is_some()
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

    /// Geodetic coordinates (radians, east of Greenwich) of a point in this CRS; `None` when the
    /// coordinates are not finite or the projection has no inverse for them.
    pub fn unproject(&self, u: f64, v: f64) -> Option<(f64, f64)> {
        if !u.is_finite() || !v.is_finite() {
            return None;
        }
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
                let normalize = |value: f64, origin: f64| {
                    let scaled = (value * self.to_meter - origin) * inverse_a;
                    if scaled.is_finite() {
                        scaled
                    } else {
                        // Preserve the ordinary arithmetic above. Only an overflowing
                        // intermediate needs the smaller, combined unit/radius factor.
                        value * (self.to_meter * inverse_a) - origin * inverse_a
                    }
                };
                let x = normalize(e, self.x0);
                let y = normalize(n, self.y0);
                if !x.is_finite() || !y.is_finite() {
                    return None;
                }
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

    /// The Helmert transformation of a WKT 2 `BOUNDCRS` to `other`'s datum, when it is bound to
    /// a CRS on that datum (one with an EPSG code) other than WGS 84.
    pub fn bound_to(&self, other: &Def) -> Option<&Arc<Op>> {
        if self.on_bound_source(other) {
            return None;
        }
        // Only a target terrio identifies: PROJ tells datums without codes apart by name, which
        // terrio does not keep (a bound CRS to one is shifted into it alone in PROJ, and not into
        // another CRS of that ellipsoid).
        self.bound
            .as_deref()
            .and_then(|b| b.target.as_ref())
            .filter(|(target, _)| {
                target.datum.is_some() && target.datum == other.datum && other.bound.as_ref().is_none_or(|b| b.target.is_none())
            })
            .map(|(_, helmert)| helmert)
    }

    /// Whether `other` is on the datum of the source CRS of this WKT 2 `BOUNDCRS`, which PROJ
    /// transforms into with no shift.
    pub fn on_bound_source(&self, other: &Def) -> bool {
        self.bound.as_deref().is_some_and(|b| b.source_datum.is_some() && b.source_datum == other.datum)
    }

    /// Why a `BOUNDCRS`'s transformation is not applied, if it is not.
    pub(crate) fn unapplied(&self) -> Option<&str> {
        self.unapplied.as_deref()
    }

    /// Whether this is a WKT 2 `BOUNDCRS` to a CRS other than WGS 84, whose transformations to
    /// WGS 84 are its shift followed by each of that CRS's, then by that CRS's ballpark.
    pub fn bound_elsewhere(&self) -> bool {
        self.bound.as_deref().is_some_and(|b| b.target.is_some())
    }

    /// For a CRS PROJ reads as bound to a hub CRS (a WKT 2 `BOUNDCRS`, a `+towgs84` or
    /// `+nadgrids` shift to WGS 84) and that has no geographic CRS of its own: the hub CRS, and the
    /// shift into it. The hub has no area of its own, as PROJ gives the bound CRS none
    /// (`getExtent`), so the search from it is for the other CRS's area.
    pub fn hub(&self) -> Option<(Def, Arc<Op>)> {
        if self.geog.is_some() {
            return None;
        }
        if let Some((target, shift)) = self.bound.as_deref().and_then(|b| b.target.as_ref()) {
            let mut hub = target.clone();
            hub.area = None;
            return Some((hub, shift.clone()));
        }
        match self.ops.as_slice() {
            [op] if !op.ballpark && !op.steps.is_empty() => {
                let mut wgs84 = Def::new(&SpatialRef { epsg: Some(4326), ..Default::default() }).ok()?;
                wgs84.area = None;
                Some((wgs84, op.clone()))
            }
            _ => None,
        }
    }

    /// The datum transformations to WGS 84, in the order they are tried.
    pub fn ops(&self) -> &[Arc<Op>] {
        &self.ops
    }

    /// A definition with all registered grid operations, including unavailable grids.
    pub fn with_all_operations(&self) -> Self {
        let mut out = self.clone();
        if let Some(all) = &self.all_ops {
            out.ops = all.clone();
        } else if let Some(e) = self.geog.and_then(epsg::get)
            && !e.grid_ops.is_empty()
        {
            out.ops = e.grid_ops.clone();
        }
        out
    }

    /// The parameters of the PROJ string the definition was read from.
    pub fn params(&self) -> Option<&crate::ProjParams> {
        self.params.as_deref()
    }

    /// The first two axes' directions (`e`, `n`, `w`, `s`), as `+axis` gives them.
    pub fn axis_directions(&self) -> [u8; 2] {
        self.axis
    }

    /// The area of use, when known.
    pub fn area(&self) -> Option<[f64; 4]> {
        self.area
    }

    /// Whether PROJ does not resolve the WKT the CRS was read from to its EPSG CRS.
    pub fn unresolved(&self) -> Option<Unresolved> {
        self.unresolved
    }

    /// The EPSG code of the datum.
    pub fn datum_code(&self) -> Option<u32> {
        self.datum
    }

    /// The EPSG code of the geographic CRS.
    pub fn geog(&self) -> Option<u32> {
        self.geog
    }

    /// The EPSG geographic CRS, also when only its name identifies it on a datum no CRS is on
    /// (ESRI's GCS_Greenland_1996, GR96).
    pub fn geographic_code(&self) -> Option<u32> {
        self.geog.or(self.named_geog)
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

    /// Whether it is a geographic 3D or a geocentric CRS.
    pub fn geodetic_3d(&self) -> bool {
        self.three_d || self.geocentric
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

impl Def {
    /// Records whether PROJ resolves the CRS of WKT `w` (read as EPSG `code`, if any) and the area
    /// it then uses: that of the CRS it finds by the WKT's name, even one that differs, else none.
    /// A CRS whose datum is renamed keeps the code's area, as PROJ's way through its ballpark is
    /// not followed yet: routes with it are those of the code. Remembered by WKT, as `Def::new`
    /// runs for many accessors, up to 1 MiB of WKT text: then all is forgotten, and the cache fills
    /// again.
    fn record_proj_resolution(&mut self, w: &str, code: Option<u32>) {
        // By code, then by WKT (looked up without allocating), with the bytes of WKT text held.
        type Seen = (std::collections::HashMap<Option<u32>, std::collections::HashMap<String, Option<Resolution>>>, usize);
        static SEEN: std::sync::Mutex<Option<Seen>> = std::sync::Mutex::new(None);
        let known = SEEN.lock().unwrap_or_else(|e| e.into_inner()).as_ref().and_then(|s| s.0.get(&code)?.get(w).copied());
        let r = known.unwrap_or_else(|| {
            // A CRS PROJ finds by the name of a WKT terrio reads as no code: the WKT matches it
            // when terrio reads them alike (conversion, ellipsoid, datum).
            let alike = |found: u32| {
                Def::new(&SpatialRef { epsg: Some(found), ..Default::default() }).is_ok_and(|f| {
                    self.same_conversion_within(&f, 1e-6)
                        && crate::fits::same_ellipsoid(&self.ell, &f.ell)
                        && self.datum.is_some()
                        && self.datum == f.datum
                })
            };
            let r = crate::parsed::tree(w).and_then(|root| proj_resolution(&root, code, self.geog, &alike));
            let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
            let seen = seen.get_or_insert_with(Default::default);
            // At most 1 MiB of WKT text kept.
            if seen.1 + w.len() > 1 << 20 {
                *seen = Default::default();
            }
            if seen.0.entry(code).or_default().insert(w.to_string(), r).is_none() {
                seen.1 += w.len();
            }
            r
        });
        if let Some(r) = r
            && !r.resolved
        {
            self.unresolved = Some(Unresolved { datum_renamed: r.datum_renamed });
            if !r.datum_renamed {
                self.area = r.area;
            }
        }
    }
}

/// The directions of a CRS node's axes (`north`, `east`...): without `AXIS`, those of its implicit
/// axes, latitude first (`north_first`) or longitude (or easting) first.
fn axis_directions(n: &wkt1::Node, north_first: bool) -> Vec<String> {
    let given: Vec<String> = n
        .children("AXIS")
        .filter_map(|a| match a.args.get(1) {
            Some(wkt1::Arg::Ident(d)) => Some(d.to_ascii_lowercase()),
            _ => None,
        })
        .collect();
    match (given.is_empty(), north_first) {
        (false, _) => given,
        (true, true) => vec!["north".into(), "east".into()],
        (true, false) => vec!["east".into(), "north".into()],
    }
}

/// The directions of an EPSG CRS's axes, parsed once per code.
fn registered_axes(code: u32) -> Option<Vec<String>> {
    type Seen = std::collections::HashMap<u32, Option<Vec<String>>>;
    static SEEN: std::sync::Mutex<Option<Seen>> = std::sync::Mutex::new(None);
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    seen.get_or_insert_with(Default::default)
        .entry(code)
        .or_insert_with(|| epsg::get(code).and_then(|e| wkt1::parse(&e.wkt)).map(|r| axis_directions(&r, false)))
        .clone()
}

/// How PROJ 9.8.1 resolves the geographic or projected CRS of WKT `root`, which terrio reads as
/// EPSG `code`, on the EPSG geographic CRS `geog` (the code's, else `own_geog`, terrio's reading of
/// the WKT's): whether it resolves it (for a projected CRS, the CRS or else its geographic CRS),
/// the area it uses for it, and whether the datum is renamed (see [`Unresolved`]). `alike` tells
/// whether the WKT matches an EPSG CRS but for its name and axes, for a WKT terrio reads as no
/// code. `None` for other kinds of CRS (the horizontal CRS of a compound CRS is resolved on its
/// own, as PROJ does).
///
/// A CRS with an ID is resolved (PROJ's `buildCRSIds` takes it whatever its axis order; one PROJ
/// does not know leads to the same transformations, see below). Without, PROJ looks its name up
/// (`createObjectsFromName`, with `data/crs_names.tsv.zst`), and resolves it when it finds the CRS
/// alone and the WKT is equivalent to it, axes included (but those of an implicit geographic CRS of
/// a projected one). A geographic CRS of WKT 1 without `AXIS` is longitude first; one of WKT 2
/// without `CS`, latitude first.
fn proj_resolution(root: &wkt1::Node, code: Option<u32>, own_geog: Option<u32>, alike: &dyn Fn(u32) -> bool) -> Option<Resolution> {
    let key = root.key.to_ascii_uppercase();
    let is_wkt1 = key == "GEOGCS" || key == "PROJCS";
    // A WKT 2 geodetic CRS with an ellipsoidal CS is a geographic CRS to PROJ.
    let ellipsoidal = || {
        root.child("CS")
            .and_then(|cs| cs.args.first())
            .is_some_and(|a| matches!(a, wkt1::Arg::Ident(t) if t.eq_ignore_ascii_case("ellipsoidal")))
    };
    let projected = match key.as_str() {
        "GEOGCS" | "GEOGCRS" | "GEOGRAPHICCRS" => false,
        "GEODCRS" | "GEODETICCRS" if ellipsoidal() => false,
        "PROJCS" | "PROJCRS" | "PROJECTEDCRS" => true,
        _ => return None,
    };
    // A geographic 3D CRS, which PROJ looks up among those.
    let three_d = !projected && root.child("CS").and_then(|cs| cs.num(1)).is_some_and(|n| n == 3.0);
    let base = if projected { root.geodetic_base()? } else { root };
    let datum = base.datum();
    let datum_renamed = datum.is_some_and(|d| d.epsg().is_some_and(|c| d.name().is_none_or(|n| !epsg::datums_named(n).contains(&c))));
    let has_id = |n: &wkt1::Node| n.ids().next().is_some();
    let gcs = |n: &wkt1::Node| is_wkt1 && n.name().is_some_and(|n| n.starts_with("GCS_"));
    let area = |c: Option<u32>| c.and_then(epsg::get).and_then(|e| e.area);
    // With an ID, resolved. PROJ skips an ID of an authority or code it does not know (looking
    // the CRS up by its name instead), but then searches only among the CRSs of that authority
    // (`findsOpsInRegistryWithIntermediate`), so finds no chain through another datum: the CRS's
    // own transformations, as for a resolved CRS.
    if has_id(root) {
        return Some(Resolution { resolved: true, area: area(code), datum_renamed });
    }
    // ESRI's WKT reader maps a `PROJCS` name after a `GCS_` geographic CRS or a `D_` datum.
    let esri =
        if projected { gcs(base) || (is_wkt1 && datum.and_then(|d| d.name()).is_some_and(|n| n.starts_with("D_"))) } else { gcs(root) };
    let found = root.name().and_then(|n| epsg::crs_named_as_proj(n, projected, three_d, esri));
    let axes = |n: &wkt1::Node, implicit: bool, c: u32| registered_axes(c) == Some(axis_directions(n, implicit));
    let geog = code.and_then(epsg::get).map(|e| e.geog).filter(|&g| g != 0).or(own_geog);
    // The WKT's axes are the CRS's, and those of its geographic CRS when explicit.
    let same_axes = |c: u32| {
        axes(root, false, c)
            && (!projected
                || base.child("AXIS").is_none()
                || epsg::get(c).map(|e| e.geog).filter(|&g| g != 0).is_some_and(|g| axes(base, !is_wkt1, g)))
    };
    let equivalent = match (found, code) {
        // A renamed datum is not the code's, as PROJ compares datum names.
        (Some(f), Some(c)) => !datum_renamed && f == c && same_axes(c),
        // PROJ finds a CRS terrio has not read the WKT as: resolved when the WKT matches it, its
        // datum name and axis order too.
        (Some(f), None) => !datum_renamed && same_axes(f) && alike(f),
        _ => false,
    };
    let resolved = equivalent
        || projected && {
            // The geographic CRS of a projected CRS PROJ does not resolve, which PROJ looks its
            // transformations up for.
            has_id(base)
                || geog.is_some_and(|g| {
                    let f = base.name().and_then(|n| epsg::crs_named_as_proj(n, false, false, gcs(base)));
                    f == Some(g) && axes(base, !is_wkt1, g)
                })
        };
    Some(Resolution { resolved, area: area(found), datum_renamed })
}

/// What [`proj_resolution`] tells of a WKT.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Resolution {
    /// Whether PROJ resolves the CRS (for a projected CRS, the CRS or else its geographic CRS).
    resolved: bool,
    /// The area PROJ uses for it: that of the CRS it finds by the WKT's name, if any.
    area: Option<[f64; 4]>,
    /// Whether the datum is renamed (see [`Unresolved`]).
    datum_renamed: bool,
}

/// PROJ's rule for an ellipsoid of the Earth: a semi-major axis within 0.7% of its mean radius
/// (`Ellipsoid::guessBodyName`).
pub(crate) fn earth_sized(a: f64) -> bool {
    (a - 6_375_000.0).abs() < 0.007 * 6_375_000.0
}

/// The horizontal and vertical parts of a compound CRS's WKT (`COMPD_CS` or `COMPOUNDCRS`).
pub(crate) fn split_compound(w: &str) -> Option<(String, String)> {
    let w = w.trim();
    let open = w.find(['[', '('])?;
    if !keywords::is(keywords::root_key(w), keywords::COMPOUND_CRS) {
        return None;
    }
    // The top-level arguments of the node.
    let body = w.get(open + 1..w.len().checked_sub(1)?)?;
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
    let horizontal = args.iter().find(|a| keywords::is(keywords::root_key(a), keywords::HORIZONTAL_CRS))?;
    let vertical = args.iter().find(|a| keywords::is(keywords::root_key(a), keywords::VERTICAL_CRS))?;
    Some((horizontal.to_string(), vertical.to_string()))
}

/// The geoid grids of a node's GDAL `PROJ4_GRIDS` extension.
fn grids_extension(n: &wkt1::Node) -> Option<String> {
    let e = n.children("EXTENSION").find(|e| e.name() == Some("PROJ4_GRIDS"))?;
    match e.args.get(1) {
        Some(wkt1::Arg::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

/// The geoid grids of a vertical CRS's vertical datum (GDAL's `PROJ4_GRIDS`, where GDAL writes
/// it), which PROJ 9.9.0 reads as a bound CRS of no code, a declared code or not.
fn datum_geoid_grids(root: &wkt1::Node) -> Option<String> {
    root.first_in(keywords::VERTICAL_DATUM).and_then(grids_extension)
}

/// The code and the geoid grids of a vertical CRS's WKT, of one parse: grids in its vertical
/// datum (GDAL's `PROJ4_GRIDS`, where GDAL writes it), which PROJ 9.9.0 reads as a bound CRS of no
/// code, a declared code or not; else its code ([`vertical_code`]), declared or identified by its
/// name, with that code's geoid models; else its root's grids, as terrio read them before (PROJ
/// ignores them, and identifies the CRS by its name).
fn code_and_grids(w: &str) -> (Option<u32>, Option<String>) {
    let root = crate::parsed::tree(w);
    if let Some(grids) = root.as_deref().and_then(datum_geoid_grids) {
        return (None, Some(grids));
    }
    let code = code_of(w, root.as_deref());
    let grids = if code.is_none() { root.as_deref().and_then(grids_extension) } else { None };
    (code, grids)
}

/// The geoid grids a vertical CRS's WKT is of ([`code_and_grids`]), which its definition and its
/// PROJ string (`+geoidgrids`) read.
pub(crate) fn wkt_geoid_grids(w: &str) -> Option<String> {
    code_and_grids(w).1
}

/// The vertical CRS of a `VERT_CS` (ESRI's `VERTCS`, WKT 2's `VERTCRS`) definition: its EPSG code
/// when it has one or is identified ([`vertical_code`]), else its unit and its geoid grids
/// ([`wkt_geoid_grids`]).
pub(crate) fn vertical_from_wkt(w: &str) -> Option<Vertical> {
    let (code, grids) = code_and_grids(w);
    // A declared or identified code's definition.
    if let Some(v) = code.and_then(Vertical::from_epsg) {
        return Some(v);
    }
    let (to_meter, down) = super::vertical_axis(w)?;
    Some(match grids {
        // Its own grids, in its direction (a depth CRS's, by `AXIS` or ESRI's `Direction`).
        Some(g) => Vertical { down, ..Vertical::from_geoid_grids(&g, to_meter) },
        None => Vertical { code, to_meter, down, models: Vec::new(), ballpark: true },
    })
}

/// The EPSG code a vertical CRS's WKT declares, when its unit and direction are the code's
/// (heights in feet declaring EGM96 height in metres are not EGM96 height); without one, the code
/// PROJ 9.9.0 identifies by its name, datum, unit and direction. None when its vertical datum has
/// geoid grids of its own (GDAL's `PROJ4_GRIDS`), which PROJ reads as a bound CRS of no code even
/// with a declared code; nor, without a declared code, when its root has (as terrio read them).
pub(crate) fn vertical_code(w: &str) -> Option<u32> {
    code_and_grids(w).0
}

/// [`vertical_code`] of a WKT whose vertical datum has no geoid grids, and its parse.
fn code_of(w: &str, root: Option<&wkt1::Node>) -> Option<u32> {
    let (to_meter, down) = super::vertical_axis(w)?;
    let fits = |code: u32| epsg::vertical(code).is_some_and(|r| (to_meter - r.to_meter).abs() <= 1e-9 * r.to_meter && down == r.down);
    if let Some(code) = super::declared_epsg(w) {
        return fits(code).then_some(code);
    }
    // Without a code, the CRS its name finds on the datum its datum's name is, as PROJ 9.9.0
    // identifies it (`VerticalCRS::identify`): ESRI's `VERTCS["EGM96_height",
    // VDATUM["EGM96_Geoid"],...]` is EPSG:5773. Only a single such CRS.
    let root = root?;
    if !root.is(keywords::VERTICAL_CRS) || grids_extension(root).is_some() {
        return None;
    }
    let esri = root.key.eq_ignore_ascii_case("VERTCS");
    let on_datum = epsg::verticals_on_datum_named(root.first_in(keywords::VERTICAL_DATUM)?.name()?);
    let found: Vec<u32> = epsg::verticals_named(root.name()?, esri).into_iter().filter(|c| fits(*c) && on_datum.contains(c)).collect();
    match found[..] {
        [code] => Some(code),
        _ => None,
    }
}

/// Whether a WKT is a WKT 2 `BOUNDCRS`.
pub(crate) fn is_bound_crs(w: &str) -> bool {
    w.trim_start().get(..8).is_some_and(|k| k.eq_ignore_ascii_case("BOUNDCRS"))
}

/// The `TOWGS84` parameters of a WKT 1 definition, when they shift.
fn wkt_towgs84(w: &str) -> Option<Vec<f64>> {
    let root = crate::parsed::tree(w)?;
    let geogcs =
        if root.key.eq_ignore_ascii_case("GEOGCS") || root.key.eq_ignore_ascii_case("GEOCCS") { &root } else { root.child("GEOGCS")? };
    let t = geogcs.child("DATUM")?.child("TOWGS84")?;
    let v: Vec<f64> = (0..t.args.len()).filter_map(|i| t.num(i)).collect();
    (v.len() == 3 || v.len() == 7).then_some(v)
}

/// A PROJ string's `+vto_meter`, a positive number: a fraction (`1200/3937`) only in a WKT's
/// `PROJ4` extension, as PROJ 9.8.1's pipeline reads it (its CRS parser refuses one).
fn vto_meter(p: &Params, extension: bool) -> Option<f64> {
    let v = p.str("vto_meter")?.trim();
    let value = match v.split_once('/').filter(|_| extension) {
        Some((n, d)) => n.trim().parse::<f64>().ok()? / d.trim().parse::<f64>().ok()?,
        None => v.parse::<f64>().ok()?,
    };
    (value.is_finite() && value > 0.0).then_some(value)
}

/// Whether PROJ 9.8.1 maps a PROJ string's `+lat_0`, `+lon_0`, `+x_0` or `+y_0` into the CRS of a
/// method, and so refuses a value that is not a number: as probed with pyproj 3.8.0
/// (`CRS("+proj=<method> ... +<key>=abc")` refused where the same string without it is read),
/// and Laborde by `data/proj_param_values.tsv`; `tmerczoned`, new in PROJ 9.9.0, as PROJ 9.9.0
/// with pyproj 3.8.0 maps them.
fn proj_maps_origin(method: &str, key: &str) -> bool {
    const ALL: [&str; 25] = [
        "aea",
        "aeqd",
        "cass",
        "col_urban",
        "eqc",
        "eqdc",
        "etmerc",
        "gnom",
        "gstmerc",
        "krovak",
        "labrd",
        "laea",
        "lcc",
        "mod_krovak",
        "nsper",
        "nzmg",
        "ortho",
        "poly",
        "qsc",
        "somerc",
        "stere",
        "sterea",
        "tmerc",
        "tmerczoned",
        "webmerc",
    ];
    // Methods without a latitude of origin.
    const NO_LATITUDE: [&str; 19] = [
        "bonne", "cea", "eck4", "eqearth", "gall", "geos", "igh", "merc", "mill", "moll", "natearth", "robin", "sinu", "vandg", "wag4",
        "wag5", "wink1", "wink2", "wintri",
    ];
    ALL.contains(&method)
        || (NO_LATITUDE.contains(&method) && key != "lat_0")
        // Hotine's longitude is `+lonc`.
        || (method == "omerc" && key != "lon_0")
}

/// The text of a WKT 1 definition's own geographic CRS: none for a CRS read as WKT 2 (its
/// `GEOGCRS` the text search does not find), whose search would find a WKT 1 `BOUNDCRS` target's
/// `GEOGCS` instead (ED50 with its own `TOWGS84` under a Bessel source), so that its datum would
/// be taken for the source's. The source's node is read then.
fn own_geogcs_text<'a>(w: &'a str, c: &wkt1::Converted) -> Option<&'a str> {
    geogcs_text(w).filter(|_| c.datum.is_none())
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

/// The size in metres of a WKT definition's linear unit: the root's first `UNIT` (WKT 1, as PROJ
/// 9.8.1 takes it) or last `LENGTHUNIT` (WKT 2).
#[cfg(feature = "wbprojection-fallback")]
pub(crate) fn wkt_linear_unit(w: &str) -> Option<f64> {
    let root = crate::parsed::tree(w)?;
    if let Some(u) = root.children("UNIT").next().or_else(|| root.children("LENGTHUNIT").last()) {
        return u.num(1);
    }
    root.children("AXIS").find_map(|a| a.child("LENGTHUNIT")).and_then(|u| u.num(1))
}

/// Whether PROJ 9.9.0 leaves a parameter's value unchecked for a CRS's method, though terrio
/// reads it: the methods its CRS parser keeps as PROJ strings (`murd2`, `tissot`; terrio
/// checks `tpers`'s itself), parameters outside the method's EPSG definition (`ortho`'s `alpha`, `ups`'s
/// `lat_ts`), and `stere`'s `lat_ts` unless polar.
fn unchecked(p: &Params, key: &str) -> bool {
    let polar = || p.str("lat_0").and_then(parse_dms).is_some_and(|l| l.abs() == 90.0);
    match p.str("proj").unwrap_or("") {
        "murd2" | "tissot" => matches!(key, "lat_1" | "lat_2"),
        "ortho" => key == "alpha",
        "ups" => key == "lat_ts",
        "stere" => key == "lat_ts" && !polar(),
        _ => false,
    }
}

/// The number a value starts with, as C's `atof` reads it (`0.5r` is 0.5); `None` without one.
pub(crate) fn leading_number(v: &str) -> Option<f64> {
    let v = v.trim_start();
    let b = v.as_bytes();
    let mut end = usize::from(matches!(b.first(), Some(b'+' | b'-')));
    for word in ["infinity", "inf", "nan"] {
        if v.get(end..end + word.len()).is_some_and(|s| s.eq_ignore_ascii_case(word)) {
            return v[..end + word.len()].parse().ok();
        }
    }
    while b.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    if b.get(end) == Some(&b'.') {
        end += 1;
        while b.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
    }
    if matches!(b.get(end), Some(b'e' | b'E')) {
        let exponent = end;
        end += 1;
        if matches!(b.get(end), Some(b'+' | b'-')) {
            end += 1;
        }
        let digits = end;
        while b.get(end).is_some_and(u8::is_ascii_digit) {
            end += 1;
        }
        if end == digits {
            end = exponent;
        }
    }
    v[..end].parse().ok()
}

#[cfg(test)]
mod inverse_input_tests {
    use super::*;

    #[test]
    fn leading_numbers_allow_suffixes_and_special_values() {
        for (value, expected) in [("0.5r", 0.5), (" -2.5e+2suffix", -250.0), ("1e+suffix", 1.0), (".5°", 0.5), ("2.3.4", 2.3)] {
            assert_eq!(leading_number(value), Some(expected), "{value}");
        }
        for value in ["", "abc", "+", ".", "-e2", "é"] {
            assert_eq!(leading_number(value), None, "{value}");
        }
        assert!(leading_number("-NaNtail").unwrap().is_nan());
        assert_eq!(leading_number("+INFtail"), Some(f64::INFINITY));
        assert_eq!(leading_number("-Infinitytail"), Some(f64::NEG_INFINITY));
        assert_eq!(leading_number(&format!("0.5{}", "x".repeat(100_000))), Some(0.5));
        assert_eq!(leading_number(&format!("{}x", "1".repeat(100_000))), Some(f64::INFINITY));
    }

    #[test]
    fn inverse_refuses_nonfinite_coordinates_across_projection_kinds() {
        for epsg in [4326, 3577, 3857, 3035] {
            let def = Def::new(&SpatialRef { epsg: Some(epsg), ..Default::default() }).unwrap();
            for (u, v) in [
                (f64::INFINITY, f64::INFINITY),
                (f64::INFINITY, 0.0),
                (f64::NEG_INFINITY, f64::INFINITY),
                (0.0, f64::NEG_INFINITY),
                (f64::NAN, f64::NAN),
                (0.0, f64::NAN),
            ] {
                assert!(def.unproject(u, v).is_none(), "EPSG:{epsg}: ({u}, {v})");
            }
        }
    }

    #[test]
    fn inverse_preserves_direction_when_finite_unit_conversion_overflows() {
        let crs = SpatialRef::from_proj4("+proj=stere +lat_0=90 +R=6371000 +units=km").unwrap();
        let def = Def::new(&crs).unwrap();
        let (u, v) = (1e306_f64, 2e306_f64);
        assert!(!(u * 1000.0).is_finite());
        let (lon, lat) = def.unproject(u, v).unwrap();
        // Analytic polar stereographic limit, independent of ellipsoid or distance units.
        assert!((lon - u.atan2(-v)).abs() < 1e-14, "{lon}");
        assert!((lat + std::f64::consts::FRAC_PI_2).abs() < 1e-14, "{lat}");
    }

    #[test]
    fn inverse_refuses_unrepresentable_normalized_coordinates() {
        let crs = SpatialRef::from_proj4("+proj=stere +lat_0=90 +R=6371000 +to_meter=1e300").unwrap();
        let def = Def::new(&crs).unwrap();
        assert!(def.unproject(1e306, 2e306).is_none());
    }
}

#[cfg(test)]
mod inverted_degrees_tests {
    use super::*;

    fn nad83(inverted_degrees: bool) -> Def {
        let mut def = Def::new(&SpatialRef { epsg: Some(4269), ..Default::default() }).unwrap();
        def.inverted_degrees = inverted_degrees;
        def
    }

    #[test]
    fn project_divides_by_the_forward_factor() {
        let (plain, inverted) = (nad83(false), nad83(true));
        let factor = 1.0_f64.to_radians();
        let mut changed = 0;
        for radians in (1..2000).map(|i| f64::from(i) * 7.3e-4) {
            let (u, v) = inverted.project(radians, 0.25).unwrap();
            assert_eq!((u, v), inverted.axis(radians / factor, 0.25 / factor));
            changed += usize::from((u, v) != plain.project(radians, 0.25).unwrap());
        }
        assert!(changed > 0, "the two conversions never differed");
    }

    #[test]
    fn unproject_divides_by_the_forward_factor() {
        let (plain, inverted) = (nad83(false), nad83(true));
        let factor = 1.0 / 1.0_f64.to_radians();
        let mut changed = 0;
        for degrees in (1..2000).map(|i| f64::from(i) * 0.05) {
            let (u, v) = plain.axis(degrees, 30.0);
            let (lon, lat) = inverted.unproject(u, v).unwrap();
            assert_eq!(lon.to_bits(), (degrees / factor).to_bits());
            assert_eq!(lat.to_bits(), (30.0 / factor).to_bits());
            changed += usize::from((lon, lat) != plain.unproject(u, v).unwrap());
        }
        assert!(changed > 0, "the two conversions never differed");
    }
}

#[cfg(test)]
mod resolution_tests {
    use super::*;

    /// [`proj_resolution`] of a WKT, as a tuple.
    fn resolution(w: &str, code: Option<u32>) -> Option<(bool, Option<[f64; 4]>, bool)> {
        let r = proj_resolution(&wkt1::parse(w).unwrap(), code, None, &|_| false)?;
        Some((r.resolved, r.area, r.datum_renamed))
    }

    /// A renamed geographic CRS on a datum or ensemble with an ID, as WKT 2.
    fn renamed(datum: &str) -> String {
        let deg = r#"ANGLEUNIT["degree",0.0174532925199433]"#;
        format!(
            r#"GEOGCRS["Local",{datum},PRIMEM["Greenwich",0,{deg}],CS[ellipsoidal,2],AXIS["geodetic latitude (Lat)",north,ORDER[1],{deg}],AXIS["geodetic longitude (Lon)",east,ORDER[2],{deg}]]"#
        )
    }

    #[test]
    fn ensembles_named_as_their_id_are_not_renamed_datums() {
        let wgs84 = renamed(
            r#"ENSEMBLE["World Geodetic System 1984 ensemble",MEMBER["World Geodetic System 1984 (Transit)"],ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]],ENSEMBLEACCURACY[2.0],ID["EPSG",6326]]"#,
        );
        let etrs89 = renamed(
            r#"ENSEMBLE["European Terrestrial Reference System 1989 ensemble",MEMBER["European Terrestrial Reference Frame 1989"],ELLIPSOID["GRS 1980",6378137,298.257222101,LENGTHUNIT["metre",1]],ENSEMBLEACCURACY[0.1],ID["EPSG",6258]]"#,
        );
        // Renamed, so not resolved, but on the ensemble the ID names.
        assert_eq!(resolution(&wgs84, Some(4326)), Some((false, None, false)));
        assert_eq!(resolution(&etrs89, Some(4258)), Some((false, None, false)));
        let local = renamed(r#"DATUM["Local datum",ELLIPSOID["International 1924",6378388,297,LENGTHUNIT["metre",1]],ID["EPSG",6806]]"#);
        assert_eq!(resolution(&local, Some(4806)), Some((false, None, true)));
    }

    #[test]
    fn a_renamed_datum_under_the_codes_name_is_not_the_code() {
        // Monte Mario (Rome) under its own name, on a datum of another name with its ID: PROJ
        // compares datum names, so it does not resolve it (73 m off EPSG:4806 in PROJ 9.8.1).
        let renamed = renamed(r#"DATUM["Local datum",ELLIPSOID["International 1924",6378388,297,LENGTHUNIT["metre",1]],ID["EPSG",6806]]"#)
            .replacen(r#"GEOGCRS["Local""#, r#"GEOGCRS["Monte Mario (Rome)""#, 1);
        let area = epsg::get(4806).unwrap().area;
        assert_eq!(resolution(&renamed, Some(4806)), Some((false, area, true)));
        // On its datum's own name, it is the code.
        let own = renamed.replacen(r#"DATUM["Local datum""#, r#"DATUM["Monte Mario (Rome)""#, 1);
        assert_eq!(resolution(&own, Some(4806)).map(|r| (r.0, r.2)), Some((true, false)));
    }

    #[test]
    fn a_projected_crs_on_a_geographic_crs_with_its_id_is_resolved() {
        // A custom Transverse Mercator without code: PROJ takes its geographic CRS by its ID,
        // without `AXIS` too; without the ID, a WKT 1 `GEOGCS` without `AXIS` (longitude first)
        // is not resolved.
        let tm = |geogcs: &str| {
            format!(
                r#"PROJCS["Custom TM",{geogcs},PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",10],PARAMETER["scale_factor",0.9996],PARAMETER["false_easting",500000],PARAMETER["false_northing",0],UNIT["metre",1],AXIS["Easting",EAST],AXIS["Northing",NORTH]]"#
            )
        };
        let geogcs = r#"GEOGCS["DHDN",DATUM["Deutsches_Hauptdreiecksnetz",SPHEROID["Bessel 1841",6377397.155,299.1528128]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]"#;
        let with_id = tm(&format!(r#"{geogcs},AUTHORITY["EPSG","4314"]]"#));
        let without = tm(&format!("{geogcs}]"));
        let resolved = |w: &str| proj_resolution(&wkt1::parse(w).unwrap(), None, Some(4314), &|_| false).map(|r| r.resolved);
        assert_eq!(resolved(&with_id), Some(true));
        assert_eq!(resolved(&without), Some(false));
    }

    #[test]
    fn code_less_wkts_found_by_name_are_resolved_when_alike() {
        // Monte Mario (Rome) by an alias, latitude first, on its datum: PROJ resolves it when it
        // matches the CRS it finds (`alike`), else it uses that CRS's area.
        let w = renamed(r#"DATUM["Monte Mario (Rome)",ELLIPSOID["International 1924",6378388,297,LENGTHUNIT["metre",1]],ID["EPSG",6806]]"#)
            .replacen(r#"GEOGCRS["Local""#, r#"GEOGCRS["Rome 1940 (Rome)""#, 1);
        let root = wkt1::parse(&w).unwrap();
        let alike = proj_resolution(&root, None, Some(4806), &|c| c == 4806).unwrap();
        assert!(alike.resolved);
        let other = proj_resolution(&root, None, Some(4806), &|_| false).unwrap();
        assert!(!other.resolved);
        assert_eq!(other.area, epsg::get(4806).unwrap().area);
    }

    #[test]
    fn names_stand_for_crss_of_the_kind_proj_looks_up() {
        // The name of a geographic 3D or projected CRS does not stand for it among geographic 2D
        // CRSs, nor that of a geographic CRS among projected ones.
        let named = |name, projected, three_d| epsg::crs_named_as_proj(name, projected, three_d, false);
        assert_eq!(named("WGS 84", false, false), Some(4326));
        assert_eq!(named("WGS 84", false, true), Some(4979));
        assert_eq!(named("WGS 84", true, false), None);
        assert_eq!(named("WGS 84 / UTM zone 33N", true, false), Some(32633));
        assert_eq!(named("WGS 84 / UTM zone 33N", false, false), None);
        // A name PROJ finds among several CRSs.
        assert_eq!(named("DHDN", false, false), Some(4314));
        assert_eq!(named("ETRF2000", false, false), Some(9067));
        // Geographic 3D CRSs of one name, one of them deprecated: the current one.
        for (name, current) in [("RRAF 1991", 4557), ("IGD05", 7135), ("IGD05/12", 7138)] {
            assert_eq!(named(name, false, true), Some(current), "{name}");
        }
        // A geographic 3D WKT of its official name and axes is resolved; renamed, it is not.
        let deg = r#"ANGLEUNIT["degree",0.0174532925199433]"#;
        let wgs84_3d = format!(
            r#"GEOGCRS["WGS 84",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,{deg}],CS[ellipsoidal,3],AXIS["latitude",north,ORDER[1],{deg}],AXIS["longitude",east,ORDER[2],{deg}],AXIS["height",up,ORDER[3],LENGTHUNIT["metre",1]]]"#
        );
        assert_eq!(resolution(&wgs84_3d, Some(4979)).map(|r| r.0), Some(true));
        let renamed_3d = wgs84_3d.replacen(r#"GEOGCRS["WGS 84""#, r#"GEOGCRS["Local""#, 1);
        assert_eq!(resolution(&renamed_3d, Some(4979)), Some((false, None, false)));
    }
}

#[cfg(test)]
mod memo_tests {
    use super::*;

    /// A CRS's definition is built once and shared, until the grids found may change: a reset of
    /// the grid caches or a grid registered.
    #[test]
    fn a_definition_is_kept_until_the_grids_change() {
        let s = SpatialRef { epsg: Some(4267), ..Default::default() };
        // Kept within an epoch: tried again when another test changes the grids meanwhile.
        assert!((0..100).any(|_| {
            let epoch = grid::epoch();
            let (a, b) = (Def::shared(&s).unwrap(), Def::shared(&s).unwrap());
            Arc::ptr_eq(&a, &b) && epoch == grid::epoch()
        }));
        // The definition kept is the one built: the same transformations, in the same order.
        assert!((0..100).any(|_| {
            let epoch = grid::epoch();
            let (kept, built) = (Def::shared(&s).unwrap(), Def::build(&s).unwrap());
            let same = kept.ops.len() == built.ops.len() && kept.ops.iter().zip(&built.ops).all(|(a, b)| Arc::ptr_eq(a, b));
            same && epoch == grid::epoch()
        }));
        let first = Def::shared(&s).unwrap();
        crate::reset_grid_cache();
        let after_reset = Def::shared(&s).unwrap();
        assert!(!Arc::ptr_eq(&first, &after_reset));
        // A name no CRS uses, so that no other test's grids change.
        crate::register_grid("terrio-memo-test-unused.gsb", vec![0; 16]);
        assert!(!Arc::ptr_eq(&after_reset, &Def::shared(&s).unwrap()));
        // Not kept while no grid counts as found.
        let kept = Def::shared(&s).unwrap();
        grid::without_grids(|| assert!(!Arc::ptr_eq(&kept, &Def::shared(&s).unwrap())));
    }

    /// A definition larger than the memo's bound is built each time, not kept.
    #[test]
    fn a_definition_larger_than_the_bound_is_not_kept() {
        let s = SpatialRef { proj4: Some(format!("+proj=longlat +datum=WGS84{}", " ".repeat(1 << 20))), ..Default::default() };
        let first = Def::shared(&s).unwrap();
        assert!(!Arc::ptr_eq(&first, &Def::shared(&s).unwrap()));
    }

    /// The memo's lock is one `host::hold_for_fork` takes across a fork, and its hash keys are
    /// drawn before it.
    #[cfg(all(unix, not(target_family = "wasm")))]
    #[test]
    fn hold_for_fork_takes_the_memo_lock() {
        let mut held = Vec::new();
        hold_for_fork(&mut held);
        assert!(MEMO_KEYS.get().is_some());
        drop(held);
        crate::host::assert_held_for_fork("definitions", |wait| {
            let _g = memo().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            wait();
        });
    }
}

#[cfg(test)]
mod vertical_wkt_tests {
    /// A vertical CRS identified by its name is its code's, but one with geoid grids of its own
    /// (GDAL's `PROJ4_GRIDS`, in its vertical datum or at its root) is not identified: it is its
    /// grids, of no code, as the PROJ string writes it. PROJ 9.9.0 (pyproj 3.8.0) reads the
    /// extension in the datum as a bound CRS of no code (`to_epsg()` None).
    #[test]
    fn own_geoid_grids_win_over_an_identified_code() {
        let bare = r#"VERT_CS["EGM96 height",VERT_DATUM["EGM96 geoid",2005],UNIT["metre",1],AXIS["Gravity-related height",UP]]"#;
        assert_eq!(super::vertical_from_wkt(bare).and_then(|v| v.code), Some(5773));
        let in_datum = bare.replace("2005]", r#"2005,EXTENSION["PROJ4_GRIDS","custom.gtx"]]"#);
        let at_root = bare.replace("AXIS[", r#"EXTENSION["PROJ4_GRIDS","custom.gtx"],AXIS["#);
        for grids in [&in_datum, &at_root] {
            assert_eq!(super::vertical_code(grids), None, "{grids}");
            assert_eq!(crate::identify_epsg(grids), None, "{grids}");
            let v = super::vertical_from_wkt(grids).unwrap();
            assert_eq!((v.code, v.models.len()), (None, 1), "{grids}");
        }
        // A declared code keeps its definition, as before.
        let declared = at_root.replace("UP]]", r#"UP],AUTHORITY["EPSG","5773"]]"#);
        assert_eq!(super::vertical_from_wkt(&declared).and_then(|v| v.code), Some(5773));
        // Its own grids keep its direction: an axis down, or ESRI's `Direction` -1.
        assert!(super::vertical_from_wkt(&at_root.replace("UP]", "DOWN]")).unwrap().down);
        let esri = r#"VERTCS["x",VDATUM["y"],PARAMETER["Vertical_Shift",0.0],PARAMETER["Direction",-1.0],UNIT["Meter",1.0],EXTENSION["PROJ4_GRIDS","custom.gtx"]]"#;
        let v = super::vertical_from_wkt(esri).unwrap();
        assert!(v.down && v.models.len() == 1);
        assert_eq!(v.to_metres(10.0), -10.0);
    }
}
