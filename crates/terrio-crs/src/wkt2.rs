// SPDX-FileCopyrightText: Copyright (c) 2018, Even Rouault <even dot rouault at spatialys dot com>
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified), for a `BOUNDCRS`'s Helmert transformation:
//   PROJ 9.8.1:
//     src/proj_constants.h (the Helmert methods' EPSG codes and names, the parameters' codes)
//     src/iso19111/operation/singleoperation.cpp (exportToPROJStringGeneric: how a method is
//       recognized and applied)
//     src/iso19111/operation/transformation.cpp (getTOWGS84Parameters: which are written as
//       +towgs84)
//     src/iso19111/operation/oputils.cpp (isTimeDependent)
//     src/iso19111/operation/singleoperation.cpp (OperationMethod::getEPSGCode: a method's code
//       by its name) and parammappings.cpp (methodNameCodesList)
// Written to match the behavior of:
//   PROJ 9.8.1:
//     src/iso19111/io.cpp
// No code is translated from that source.

//! A WKT 2 (ISO 19162, 2015 and 2019) reader for geographic and projected CRSs, turning them
//! into PROJ strings as [`super::wkt1`] does for WKT 1, so they go through terrio's projections.
//!
//! In WKT 2 a parameter's value is in the unit of its own `ANGLEUNIT`, `LENGTHUNIT` or
//! `SCALEUNIT` node; without one, angles are in the base CRS's angular unit and lengths in the
//! projected CS's unit. Methods and parameters are named as in EPSG, and identified by their
//! EPSG codes when the WKT gives them.

use super::wkt1::{Arg, BoundTo, Converted, Helmert, Node, norm};
use crate::keywords::{BASE_GEODETIC_CRS, GEOGRAPHIC_OR_GEODETIC_CRS, PROJECTED_CRS};
use crate::{Error, Result};

/// Whether `root` is a WKT 2 CRS this reader converts.
pub(crate) fn handles(root: &Node) -> bool {
    let key = root.key.to_ascii_uppercase();
    if key == "BOUNDCRS" {
        // Only of a CRS this reader converts, or of a WKT 1 geographic or projected CRS.
        return root.child("SOURCECRS").and_then(first_node).is_some_and(|s| handles(s) || is_wkt1_source(s));
    }
    root.is(GEOGRAPHIC_OR_GEODETIC_CRS) || root.is(PROJECTED_CRS)
}

/// What a WKT 2 CRS the reader converts ([`handles`]) gives its PROJ string: a CRS's input, or a
/// `BOUNDCRS`'s.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Wkt2Root {
    Crs(Wkt2Input),
    Bound(Box<Wkt2Bound>),
}

/// What a `BOUNDCRS` gives its PROJ string, read from its model (`model::wkt2_root_input`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Wkt2Bound {
    pub source: Wkt2Source,
    pub transformation: Wkt2Transformation,
    /// Whether its target is WGS 84 (EPSG:4326 or 4979, or its datum WGS 84 by code or name).
    pub to_wgs84: bool,
    /// Its target's input, when the reader converts it.
    pub target: Option<Wkt2Root>,
}

/// A `BOUNDCRS`'s source CRS: a WKT 2 CRS, or a WKT 1 one as GDAL writes it, without the
/// `TOWGS84` of its datum.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Wkt2Source {
    Wkt2(Wkt2Root),
    Wkt1(super::wkt1::Wkt1Input),
}

/// A `BOUNDCRS`'s `ABRIDGEDTRANSFORMATION`: its method's name as written and EPSG code (its
/// `ID`), its parameters, and its number of `PARAMETERFILE`s.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Wkt2Transformation {
    pub method: String,
    pub epsg: Option<u32>,
    pub params: Vec<Wkt2TransformationParam>,
    pub files: usize,
}

/// A transformation's parameter: as a conversion's ([`Wkt2Param`]), its value `None` when not a
/// number, which only a Helmert transformation refuses.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Wkt2TransformationParam {
    pub name: String,
    pub epsg: Option<u32>,
    pub value: Option<f64>,
    pub units: Vec<(&'static str, Option<f64>)>,
}

impl Wkt2Transformation {
    fn read(t: &Node) -> Self {
        let method = t.child("METHOD");
        let params = t
            .children("PARAMETER")
            .map(|p| Wkt2TransformationParam {
                name: p.name().unwrap_or("").to_string(),
                epsg: p.epsg(),
                value: p.num(1),
                units: UNIT_KEYS.iter().filter_map(|k| p.child(k).map(|u| (*k, u.num(1)))).collect(),
            })
            .collect();
        Self {
            method: method.and_then(Node::name).unwrap_or("").to_string(),
            epsg: method.and_then(Node::epsg),
            params,
            files: t.children("PARAMETERFILE").count(),
        }
    }
}

/// The PROJ string of a WKT 2 CRS (`root`) from what it gives it ([`Wkt2Root`]), with a
/// `BOUNDCRS`'s Helmert transformation as `+towgs84` when it is to WGS 84, else as
/// [`Converted::bound`].
pub(crate) fn convert_root(input: &Wkt2Root, root: &Node) -> Result<Converted> {
    let b = match input {
        Wkt2Root::Crs(input) => {
            let base = if matches!(input.kind, Wkt2Kind::Geographic { .. } | Wkt2Kind::Geocentric { .. }) {
                root
            } else {
                root.first_in(BASE_GEODETIC_CRS).ok_or_else(|| bad("a CRS without its base CRS"))?
            };
            return convert(input, base);
        }
        Wkt2Root::Bound(b) => b,
    };
    let source = root.child("SOURCECRS").and_then(first_node).ok_or_else(|| bad("a BOUNDCRS without its SOURCECRS"))?;
    let mut c = match &b.source {
        Wkt2Source::Wkt1(input) => {
            let source = wkt1_source(source);
            let geogcs = if input.kind == super::wkt1::Wkt1Kind::Projected { source.child("GEOGCS") } else { Some(&source) };
            geogcs.and_then(|g| super::wkt1::convert(input, g)).ok_or_else(wkt1_source_refused)?
        }
        Wkt2Source::Wkt2(input) => convert_root(input, source)?,
    };
    // A Helmert transformation terrio cannot read fails, whatever its target; one to a CRS
    // terrio cannot convert is left out, as terrio did before it read those.
    // The exact or evaluation-point step only with the target it is to: not to WGS 84 when
    // the target is left out.
    match helmert_shift(&b.transformation, b.to_wgs84)? {
        Some((t, true, helmert)) => {
            c.proj = format!("{} +towgs84={}", c.proj, t.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(","));
            c.helmert = helmert;
        }
        // To another CRS (ED50): the shift to it, then its own transformations to WGS 84, as
        // PROJ chains them.
        Some((t, false, helmert)) => {
            let target = root.child("TARGETCRS").and_then(first_node);
            if let Some(target) = b.target.as_ref().zip(target).and_then(|(input, node)| convert_root(input, node).ok()) {
                c.bound = Some(Box::new(BoundTo { target, shift: t }));
                c.helmert = helmert;
            }
        }
        None => {}
    }
    Ok(c)
}

/// What a WKT 2 geographic, geocentric, projected or derived geographic CRS gives its PROJ string,
/// read from its model (`model::wkt2_root_input`, #1058 step 6).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Wkt2Input {
    /// The geodetic CRS: the CRS itself, or a projected or derived CRS's base CRS.
    pub geodetic: Wkt2Geodetic,
    pub kind: Wkt2Kind,
}

/// What the reader takes from a geodetic CRS.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Wkt2Geodetic {
    /// The CRS's name and EPSG code, its datum's name and code, and its ellipsoid's name, as written.
    pub name: Option<String>,
    pub epsg: Option<u32>,
    pub datum_name: Option<String>,
    pub datum_epsg: Option<u32>,
    pub ellipsoid_name: Option<String>,
    /// The semi-major axis in metres, and the inverse flattening (0 for a sphere).
    pub a: f64,
    pub rf: f64,
    /// The angular unit in degrees, WKT's rounded degree and grad made exact.
    pub ang: f64,
    /// The prime meridian in degrees, with PROJ's corrections.
    pub pm: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Wkt2Kind {
    /// A geographic CRS: whether planetocentric (a spherical CS), and its two axes as `+axis`
    /// letters when one is west or south.
    Geographic { spherical: bool, axes: Option<String> },
    /// A geocentric CRS, in its axes' length unit (metres).
    Geocentric { unit: f64 },
    /// A projected CRS: its CS's length unit (metres), its axes' directions in lower case (none for
    /// a polar CS, whose axes run along meridians), and its conversion.
    Projected { to_meter: f64, directions: Vec<String>, method: Wkt2Method },
    /// A derived geographic CRS (a rotated pole): its deriving conversion, and its grid's angular
    /// unit in degrees (the first unit its axes have, else degrees).
    Derived { method: Wkt2Method, grid_ang: f64 },
}

/// A projected or derived CRS's conversion: its method's name as written and EPSG code, and its
/// parameters.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Wkt2Method {
    pub name: String,
    pub epsg: Option<u32>,
    pub params: Vec<Wkt2Param>,
}

/// A conversion's parameter: its name as written, its EPSG code, its value (as written, in its
/// unit), and its unit nodes' sizes as written (in radians, metres or unity), by keyword
/// (`ANGLEUNIT`, `LENGTHUNIT`, `SCALEUNIT`, `UNIT`), the first of each.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Wkt2Param {
    pub name: String,
    pub epsg: Option<u32>,
    pub value: f64,
    pub units: Vec<(&'static str, Option<f64>)>,
}

/// A parameter's unit keywords.
pub(crate) const UNIT_KEYS: [&str; 4] = ["ANGLEUNIT", "LENGTHUNIT", "SCALEUNIT", "UNIT"];

impl Wkt2Param {
    /// Whether the parameter has a unit node of a keyword.
    fn has(&self, key: &str) -> bool {
        self.units.iter().any(|u| u.0 == key)
    }

    /// The size of the parameter's unit node of the first of `keys` it has one of.
    fn size(&self, keys: &[&str]) -> Option<f64> {
        keys.iter().find_map(|k| self.units.iter().find(|u| u.0 == *k)).and_then(|u| u.1)
    }
}

/// The length unit (metres) of a WKT 2 geodetic CRS's Cartesian CS (`cs`): three axes, X, Y and
/// Z in that order, in one unit. Fails otherwise, as PROJ 9.9.0 refuses such a CS.
pub(crate) fn geocentric_unit(root: &Node, cs: &Node) -> Result<f64> {
    let axes: Vec<_> = root.children("AXIS").collect();
    if cs.num(1) != Some(3.0) || axes.len() != 3 {
        return Err(bad("geocentric Cartesian CS must declare three axes"));
    }
    let directions = ["geocentricX", "geocentricY", "geocentricZ"];
    for (i, axis) in axes.iter().enumerate() {
        if !matches!(axis.args.get(1), Some(Arg::Ident(d)) if d.eq_ignore_ascii_case(directions[i]))
            || axis.child("ORDER").is_some_and(|o| o.num(0) != Some((i + 1) as f64))
        {
            return Err(bad("geocentric WKT axes must be canonical X/Y/Z"));
        }
    }
    let units = axes
        .iter()
        .map(|a| {
            // Or the CS's, after the axes: `LENGTHUNIT`, or a bare `UNIT` as PROJ's
            // simplified WKT 2 writes it.
            a.child("LENGTHUNIT")
                .or_else(|| root.child("LENGTHUNIT"))
                .or_else(|| root.child("UNIT"))
                .and_then(|u| u.num(1))
                .ok_or_else(|| bad("geocentric axis needs a linear unit"))
        })
        .collect::<Result<Vec<_>>>()?;
    let unit = units[0];
    if units.iter().any(|u| *u != unit) {
        return Err(bad("geocentric axes must use one linear unit"));
    }
    Ok(unit)
}

/// The PROJ string of a WKT 2 geographic, geocentric, projected or derived geographic CRS, from
/// what it gives it
/// ([`Wkt2Input`]); `base` is the geodetic CRS's node. Fails on an unknown method or parameter, or
/// a method's values PROJ refuses.
pub(crate) fn convert(input: &Wkt2Input, base: &Node) -> Result<Converted> {
    let geog = Geodetic::new(&input.geodetic, base);
    let mut tail = geog.tail();
    let (to_meter, directions, method) = match &input.kind {
        Wkt2Kind::Geocentric { unit } => {
            let mut out = vec![format!("+proj=geocent +to_meter={unit}")];
            out.append(&mut tail);
            return Ok(geog.converted(out.join(" ")));
        }
        Wkt2Kind::Geographic { spherical, axes } => {
            let mut out = vec!["+proj=longlat".to_string()];
            if *spherical {
                out.push("+geoc".into());
            }
            if let Some(axes) = axes {
                out.push(format!("+axis={axes}u"));
            }
            if (geog.ang - 1.0).abs() > 1e-12 {
                out.push(format!("+to_deg={}", geog.ang));
            }
            out.append(&mut tail);
            return Ok(geog.converted(out.join(" ")));
        }
        Wkt2Kind::Projected { to_meter, directions, method } => (*to_meter, directions, method),
        Wkt2Kind::Derived { method, grid_ang } => return derived(geog, method, *grid_ang),
    };
    let generic = proj_method(method, geog.ang, to_meter)?;
    let p = if generic.is_some() { Params::default() } else { Params::read(&method.params, geog.ang, to_meter)? };
    let (name, code) = (norm(&method.name), method.epsg);
    let is = |names: &[&str], codes: &[u32]| names.contains(&name.as_str()) || code.is_some_and(|c| codes.contains(&c));

    let mut out: Vec<String> = Vec::new();
    let mut push = |k: &str, v: Option<f64>| {
        if let Some(v) = v {
            out.push(format!("+{k}={v}"));
        }
    };
    // Natural origin, false origin and projection centre: the origin of most methods.
    let lat_0 = || p.get(LAT_NAT).or_else(|| p.get(LAT_FALSE)).or_else(|| p.get(LAT_CENTRE));
    let lon_0 = || p.get(LON_NAT).or_else(|| p.get(LON_FALSE)).or_else(|| p.get(LON_CENTRE)).or_else(|| p.get(LON_ORIGIN));
    let (mut fe, mut fn_) = (p.get(FE).unwrap_or(0.0), p.get(FN).unwrap_or(0.0));
    let mut south_orientated = false;
    let mut no_uoff = false;
    let mut conformal_sphere = false;
    let mut sweep = None;
    // A flag of the PROJ method (`+hyperbolic`, `+guam`), written after it as PROJ writes it.
    let mut flag = None;
    let proj = if is(&["transversemercator"], &[9807]) {
        push("lat_0", Some(lat_0().unwrap_or(0.0)));
        push("lon_0", lon_0());
        push("k", Some(p.get(K_NAT).unwrap_or(1.0)));
        "tmerc"
    } else if is(&["transversemercatorsouthorientated"], &[9808]) {
        push("lat_0", Some(lat_0().unwrap_or(0.0)));
        push("lon_0", lon_0());
        push("k", Some(p.get(K_NAT).unwrap_or(1.0)));
        south_orientated = true;
        "tmerc"
    } else if is(&["transversemercatorzonedgridsystem"], &[9824]) {
        // PROJ 9.9.0's `tmerczoned`, with its initial longitude as `+lon_0`.
        push("lat_0", Some(lat_0().unwrap_or(0.0)));
        push("lon_0", Some(p.get(LON_INITIAL).unwrap_or(0.0)));
        push("width", p.get(ZONE_WIDTH));
        push("k_0", Some(p.get(K_NAT).unwrap_or(1.0)));
        "tmerczoned"
    } else if is(&["lambertconicconformal1sp"], &[9801]) {
        push("lat_1", lat_0());
        push("lat_0", lat_0());
        push("lon_0", lon_0());
        push("k_0", Some(p.get(K_NAT).unwrap_or(1.0)));
        "lcc"
    } else if is(&["lambertconicconformal2sp"], &[9802]) {
        push("lat_0", lat_0());
        push("lon_0", lon_0());
        push("lat_1", p.get(SP1));
        // Without its second parallel, 0, as PROJ reads it.
        push("lat_2", Some(p.get(SP2).unwrap_or(0.0)));
        "lcc"
    } else if is(&["albersequalarea"], &[9822]) {
        push("lat_0", lat_0());
        push("lon_0", lon_0());
        push("lat_1", p.get(SP1));
        push("lat_2", p.get(SP2));
        "aea"
    } else if is(&["mercatorvarianta", "mercator1sp"], &[9804]) {
        if lat_0().is_some_and(|v| v != 0.0) {
            return Err(bad("Mercator (variant A) with a latitude of natural origin other than 0"));
        }
        push("lon_0", lon_0());
        push("k", Some(p.get(K_NAT).unwrap_or(1.0)));
        "merc"
    } else if is(&["mercatorspherical"], &[1026]) {
        push("lat_0", Some(lat_0().unwrap_or(0.0)));
        push("lon_0", lon_0());
        conformal_sphere = true;
        "merc"
    } else if is(&["mercatorvariantb", "mercator2sp"], &[9805]) {
        push("lon_0", lon_0());
        push("lat_ts", Some(p.get(SP1).unwrap_or(0.0)));
        "merc"
    } else if is(&["popularvisualisationpseudomercator"], &[1024]) {
        push("lon_0", lon_0());
        // Spherical formulas on the semi-major axis.
        tail.retain(|t| !t.starts_with("+rf=") && !t.starts_with("+b="));
        tail.push(format!("+b={}", geog.a));
        "merc"
    } else if is(&["polarstereographicvarianta"], &[9810]) {
        let lat = lat_0().unwrap_or(90.0);
        if (lat.abs() - 90.0).abs() > 1e-9 {
            return Err(bad("Polar Stereographic (variant A) with a latitude of natural origin other than ±90°"));
        }
        push("lat_0", Some(lat.signum() * 90.0));
        push("lon_0", lon_0());
        push("k", Some(p.get(K_NAT).unwrap_or(1.0)));
        "stere"
    } else if is(&["polarstereographicvariantb"], &[9829]) {
        let ts = p.get(LAT_TS).ok_or_else(|| bad("Polar Stereographic (variant B) without its standard parallel"))?;
        push("lat_0", Some(if ts < 0.0 { -90.0 } else { 90.0 }));
        push("lat_ts", Some(ts));
        push("lon_0", lon_0());
        "stere"
    } else if is(&["polarstereographicvariantc"], &[9830]) {
        // Variant B on the same standard parallel, with its false origin moved to the pole.
        let ts = p
            .get(LAT_TS)
            .filter(|v| v.is_finite() && v.abs() <= 90.0)
            .ok_or_else(|| bad("Polar Stereographic (variant C) without a valid standard parallel"))?;
        let lon =
            lon_0().filter(|v| v.is_finite()).ok_or_else(|| bad("Polar Stereographic (variant C) without its longitude of origin"))?;
        fe = p
            .get(FE)
            .filter(|v| v.is_finite())
            .ok_or_else(|| bad("Polar Stereographic (variant C) without its easting at false origin"))?;
        fn_ = p
            .get(FN)
            .filter(|v| v.is_finite())
            .ok_or_else(|| bad("Polar Stereographic (variant C) without its northing at false origin"))?;
        push("lat_0", Some(if ts < 0.0 { -90.0 } else { 90.0 }));
        push("lat_ts", Some(ts));
        push("lon_0", Some(lon));
        fn_ = super::wkt1::variant_c_pole_northing(geog.a, geog.rf, ts, fn_);
        "stere"
    } else if is(&["obliquestereographic"], &[9809]) {
        push("lat_0", lat_0());
        push("lon_0", lon_0());
        push("k", Some(p.get(K_NAT).unwrap_or(1.0)));
        "sterea"
    } else if is(&["stereographic"], &[]) {
        push("lat_0", lat_0());
        push("lon_0", lon_0());
        push("k", Some(p.get(K_NAT).unwrap_or(1.0)));
        "stere"
    } else if is(&["lambertazimuthalequalarea"], &[9820]) || (geog.rf == 0.0 && is(&["lambertazimuthalequalareaspherical"], &[1027])) {
        push("lat_0", lat_0());
        push("lon_0", lon_0());
        "laea"
    } else if (is(&["hotineobliquemercatorvarianta"], &[9812]) || is(&["hotineobliquemercatorvariantb"], &[9815]))
        && p.get(AZIMUTH).is_some_and(|a| (a - 90.0).abs() < 1e-9)
        && p.get(GAMMA).is_some_and(|g| (g - 90.0).abs() < 1e-9)
    {
        // With an azimuth and a rectified grid angle of 90°, the Swiss oblique Mercator, as PROJ
        // 9.9.0 reads variant A as well as B (and writes it, from `+proj=somerc`; opengeos/terrio#1012
        // item 28). Variant B's false origin is at the centre, variant A's at the natural origin.
        if is(&["hotineobliquemercatorvariantb"], &[9815]) {
            fe = p.get(FE_CENTRE).unwrap_or(fe);
            fn_ = p.get(FN_CENTRE).unwrap_or(fn_);
        }
        push("lat_0", p.get(LAT_CENTRE).or_else(lat_0));
        push("lon_0", p.get(LON_CENTRE).or_else(lon_0));
        push("k_0", Some(p.get(K_CENTRE).or_else(|| p.get(K_NAT)).unwrap_or(1.0)));
        "somerc"
    } else if is(&["hotineobliquemercatorvarianta"], &[9812]) || is(&["hotineobliquemercatorvariantb"], &[9815]) {
        let variant_b = is(&["hotineobliquemercatorvariantb"], &[9815]);
        if variant_b {
            fe = p.get(FE_CENTRE).unwrap_or(fe);
            fn_ = p.get(FN_CENTRE).unwrap_or(fn_);
        } else {
            no_uoff = true;
        }
        let alpha = p.get(AZIMUTH);
        push("lat_0", p.get(LAT_CENTRE).or_else(lat_0));
        push("lonc", p.get(LON_CENTRE).or_else(lon_0));
        push("alpha", alpha);
        push("gamma", p.get(GAMMA).or(alpha));
        push("k", Some(p.get(K_CENTRE).or_else(|| p.get(K_NAT)).unwrap_or(1.0)));
        "omerc"
    } else if is(&["hotineobliquemercatortwopointnaturalorigin"], &[]) {
        // PROJ's two-point form (`+proj=omerc +lat_1 +lon_1 +lat_2 +lon_2`), as GDAL writes it.
        fe = p.get(FE_CENTRE).unwrap_or(fe);
        fn_ = p.get(FN_CENTRE).unwrap_or(fn_);
        push("lat_0", p.get(LAT_CENTRE).or_else(lat_0));
        push("lat_1", p.get(LAT_P1));
        push("lon_1", p.get(LON_P1));
        push("lat_2", p.get(LAT_P2));
        push("lon_2", p.get(LON_P2));
        push("k", Some(p.get(K_CENTRE).or_else(|| p.get(K_NAT)).unwrap_or(1.0)));
        "omerc"
    } else if is(&["cassinisoldner"], &[9806]) {
        push("lat_0", lat_0());
        push("lon_0", lon_0());
        "cass"
    } else if is(&["hyperboliccassinisoldner"], &[9833]) {
        push("lat_0", lat_0());
        push("lon_0", lon_0());
        flag = Some("+hyperbolic");
        "cass"
    } else if is(&["guamprojection"], &[9831]) {
        push("lat_0", lat_0());
        push("lon_0", lon_0());
        flag = Some("+guam");
        "aeqd"
    } else if is(&["lambertconicconformal2spmichigan"], &[1051]) {
        // Its ellipsoid scaling factor is PROJ's `k_0`.
        push("lat_0", lat_0());
        push("lon_0", lon_0());
        push("lat_1", p.get(SP1));
        push("lat_2", Some(p.get(SP2).unwrap_or(0.0)));
        push("k_0", Some(p.get(K_ELLIPSOID).unwrap_or(1.0)));
        "lcc"
    } else if is(&["colombiaurban"], &[1052]) {
        push("lat_0", lat_0());
        push("lon_0", lon_0());
        push("h_0", p.get(H_PLANE_ORIGIN));
        "col_urban"
    } else if is(&["americanpolyconic"], &[9818]) {
        push("lat_0", lat_0());
        push("lon_0", lon_0());
        "poly"
    } else if is(&["equidistantcylindrical"], &[1028]) || (geog.rf == 0.0 && is(&["equidistantcylindricalspherical"], &[1029])) {
        push("lat_ts", p.get(SP1));
        push("lat_0", lat_0());
        push("lon_0", lon_0());
        "eqc"
    } else if is(&["lambertcylindricalequalarea"], &[9835]) || (geog.rf == 0.0 && is(&["lambertcylindricalequalareaspherical"], &[9834])) {
        push("lat_ts", p.get(SP1));
        push("lon_0", lon_0());
        "cea"
    } else if is(&["azimuthalequidistant"], &[1125]) {
        push("lat_0", lat_0());
        push("lon_0", lon_0());
        "aeqd"
    } else if is(&["newzealandmapgrid"], &[9811]) {
        push("lat_0", lat_0());
        push("lon_0", lon_0());
        "nzmg"
    } else if is(&["equalearth"], &[1078]) {
        push("lon_0", lon_0());
        "eqearth"
    } else if is(&["verticalperspective"], &[9838]) {
        if p.get(HEIGHT_TOPO).unwrap_or(0.0) != 0.0 {
            return Err(bad("Vertical Perspective with a nonzero topocentric height is not supported"));
        }
        push("lat_0", p.get(LAT_TOPO));
        push("lon_0", p.get(LON_TOPO));
        push("h", Some(p.get(VIEW_HEIGHT).ok_or_else(|| bad("Vertical Perspective without its Viewpoint Height"))?));
        "nsper"
    } else if is(&["orthographic"], &[9840]) {
        push("lat_0", lat_0());
        push("lon_0", lon_0());
        "ortho"
    } else if is(&["equidistantconic"], &[1119]) {
        push("lat_0", lat_0());
        push("lon_0", lon_0());
        push("lat_1", p.get(SP1));
        push("lat_2", p.get(SP2));
        "eqdc"
    } else if is(&["quadrilateralizedsphericalcube"], &[]) {
        push("lat_0", lat_0());
        push("lon_0", lon_0());
        "qsc"
    } else if is(&["gnomonic"], &[]) {
        push("lat_0", lat_0());
        push("lon_0", lon_0());
        "gnom"
    } else if is(&["bonne"], &[9827]) {
        // PROJ's `lat_1` is EPSG's latitude of natural origin.
        push("lat_1", lat_0());
        push("lon_0", lon_0());
        "bonne"
    } else if is(&["geostationarysatellitesweepx", "geostationarysatellitesweepy"], &[]) {
        push("lon_0", lon_0());
        // PROJ refuses a geostationary CRS without a height.
        push("h", Some(p.get(SAT_HEIGHT).ok_or_else(|| bad("Geostationary Satellite without its Satellite Height"))?));
        sweep = Some(if name == "geostationarysatellitesweepx" { "x" } else { "y" });
        "geos"
    } else if is(&["krovaknorthorientated", "krovak"], &[1041, 9819]) {
        // As PROJ, whose Krovak fixes the pseudo standard parallel and writes none. Krovak
        // (EPSG 9819) and its north-orientated form differ in their axes, which the CS gives.
        p.get(LAT_PSEUDO_SP);
        push("lat_0", p.get(LAT_CENTRE).or_else(lat_0));
        push("lon_0", lon_0());
        push("alpha", p.get(CO_LAT_CONE));
        push("k", Some(p.get(K_PSEUDO_SP).or_else(|| p.get(K_NAT)).unwrap_or(0.9999)));
        "krovak"
    } else if is(&["krovakmodifiednorthorientated", "krovakmodified"], &[1043, 1042]) {
        // As PROJ's `mod_krovak`, which fixes the pseudo standard parallel, the evaluation point
        // and the coefficients, and ignores them. Its axes, as Krovak's, the CS gives.
        for fixed in [LAT_PSEUDO_SP, EVAL_1, EVAL_2].iter().chain(&KROVAK_C) {
            p.ignore(*fixed);
        }
        push("lat_0", p.get(LAT_CENTRE).or_else(lat_0));
        push("lon_0", lon_0());
        push("alpha", p.get(CO_LAT_CONE));
        push("k", Some(p.get(K_PSEUDO_SP).or_else(|| p.get(K_NAT)).unwrap_or(0.9999)));
        "mod_krovak"
    } else if is(&["loximuthal"], &[]) {
        // PROJ's `lat_1` is the latitude of natural origin.
        push("lat_1", lat_0());
        push("lon_0", lon_0());
        "loxim"
    } else if is(&["transversecylindricalequalarea"], &[]) {
        push("lat_0", lat_0());
        push("lon_0", lon_0());
        push("k", Some(p.get(K_NAT).unwrap_or(1.0)));
        "tcea"
    } else if is(&["twopointequidistant"], &[]) {
        push("lat_1", p.get(LAT_P1));
        push("lon_1", p.get(LON_P1));
        push("lat_2", p.get(LAT_P2));
        push("lon_2", p.get(LON_P2));
        "tpeqd"
    } else if is(&["wagneriii"], &[]) {
        push("lat_ts", p.get(LAT_TRUE_SCALE));
        push("lon_0", lon_0());
        "wag3"
    } else if let Some(proj) = crate::methods::WORLD.iter().find(|w| w.wkt2 == Some(name.as_ref())).map(|w| w.proj) {
        push("lon_0", lon_0());
        // A latitude of natural origin is ignored, as PROJ 9.8.1 ignores it (its mapping of these
        // methods has none): Robinson and Sinusoidal with one are read.
        p.ignore(LAT_NAT);
        proj
    } else if let Some((proj, parallel)) =
        [("winkeli", "wink1", "lat_ts"), ("winkelii", "wink2", "lat_1"), ("winkeltripel", "wintri", "lat_1")]
            .iter()
            .find(|(n, _, _)| *n == name)
            .map(|(_, p, k)| (*p, *k))
    {
        push("lon_0", lon_0());
        push(parallel, p.get(SP1));
        proj
    } else if let Some((proj, params)) = &generic {
        let mut seen: Vec<&str> = Vec::new();
        for (k, v) in params {
            if seen.contains(&k.as_str()) {
                return Err(bad(&format!("parameter '{k}' given twice in the method 'PROJ {proj}'")));
            }
            seen.push(k);
            match k.as_str() {
                "x_0" => fe = *v,
                "y_0" => fn_ = *v,
                k if RESERVED.contains(&k) => return Err(bad(&format!("parameter '{k}' of the method 'PROJ {proj}'"))),
                _ => push(k, Some(*v)),
            }
        }
        proj.as_str()
    } else {
        return Err(Error::Crs(format!("unsupported WKT 2 projection method '{}'", method.name)));
    };
    // Parameters the method does not use would be ignored: refuse them, as they change the
    // coordinates.
    if let Some(unused) = p.unused() {
        return Err(bad(&format!("parameter '{unused}' of the method '{}'", method.name)));
    }
    let mut proj_str = vec![format!("+proj={proj}")];
    proj_str.extend(flag.map(String::from));
    if conformal_sphere {
        proj_str.push("+R_C".into());
    }
    if no_uoff {
        proj_str.push("+no_uoff".into());
    }
    if let Some(s) = sweep {
        proj_str.push(format!("+sweep={s}"));
    }
    proj_str.append(&mut out);
    // South-orientated Transverse Mercator is west and south but for a north-east CS, which GDAL
    // and PROJ read south then west (as for its WKT 1).
    if south_orientated {
        let north_east = directions.len() >= 2 && directions[0] == "north" && directions[1] == "east";
        proj_str.push(if north_east { "+axis=swu" } else { "+axis=wsu" }.into());
    } else if let Some(axis) = axis(directions) {
        proj_str.push(format!("+axis={axis}"));
    }
    // EPSG method 9808 subtracts the false easting and northing (W = FE - x, S = FN - y), where
    // `+axis=wsu` negates `+x_0`/`+y_0` with x and y.
    if south_orientated {
        (fe, fn_) = (super::wkt1::negated(fe), super::wkt1::negated(fn_));
    }
    proj_str.push(format!("+x_0={fe}"));
    proj_str.push(format!("+y_0={fn_}"));
    proj_str.push(format!("+to_meter={to_meter}"));
    proj_str.append(&mut tail);
    Ok(geog.converted(proj_str.join(" ")))
}

/// A derived geographic CRS (a `GEOGCRS` with a `DERIVINGCONVERSION`) as a PROJ string, from its
/// base CRS (`geog`), its deriving conversion and grid unit: a rotated-pole grid, in one of the
/// three forms PROJ writes. Fails on any other conversion, which would otherwise be dropped.
fn derived(geog: Geodetic, method: &Wkt2Method, grid_ang: f64) -> Result<Converted> {
    let method_name = method.name.as_str();
    // The parameters, in degrees, by normalised name.
    let values: Vec<_> = method
        .params
        .iter()
        .map(|p| (norm(&p.name), p.name.clone(), p.value * p.size(&["ANGLEUNIT", "UNIT"]).map(degrees).unwrap_or(geog.ang)))
        .collect();
    let used = std::cell::RefCell::new(Vec::new());
    let get = |key: &str| {
        let v = values.iter().find(|v| v.0 == key)?;
        used.borrow_mut().push(key.to_string());
        Some(v.2)
    };
    let need = |key: &str| get(key).ok_or_else(|| bad(&format!("the method '{method_name}' without its parameter '{key}'")));
    // `+o_lat_p`, `+o_lon_p` and `+lon_0`.
    let (lat_p, lon_p, lon_0) = match norm(method_name).as_str() {
        "projobtranoprojlonglat" => (need("olatp")?, get("olonp").unwrap_or(0.0), get("lon0").unwrap_or(0.0)),
        // The grid's north pole, and the grid longitude of the geographic north pole.
        "polerotationnetcdfcfconvention" => {
            let lat = need("gridnorthpolelatitudenetcdfcfconvention")?;
            let lon = need("gridnorthpolelongitudenetcdfcfconvention")?;
            (lat, get("northpolegridlongitudenetcdfcfconvention").unwrap_or(0.0), lon + 180.0)
        }
        // The grid's south pole, and the rotation about its axis.
        "polerotationgribconvention" => {
            let lat = need("latitudeofthesouthernpolegribconvention")?;
            let lon = need("longitudeofthesouthernpolegribconvention")?;
            (-lat, -get("axisrotationgribconvention").unwrap_or(0.0), lon)
        }
        _ => return Err(Error::Crs(format!("unsupported WKT 2 deriving conversion '{method_name}'"))),
    };
    if let Some(unused) = values.iter().find(|v| !used.borrow().contains(&v.0)) {
        return Err(bad(&format!("parameter '{}' of the method '{method_name}'", unused.1)));
    }
    let mut out = vec![format!("+proj=ob_tran +o_proj=longlat +o_lat_p={lat_p} +o_lon_p={lon_p} +lon_0={lon_0}")];
    // The grid's angular unit: its CS axes'.
    if (grid_ang - 1.0).abs() > 1e-12 {
        out.push(format!("+to_deg={grid_ang}"));
    }
    out.append(&mut geog.tail());
    Ok(geog.converted(out.join(" ")))
}

/// PROJ parameters a `PROJ name` method may not take as PARAMETERs: the figure, datum, units and
/// axes come from the CRS, not from the method.
const RESERVED: [&str; 27] = [
    "proj",
    "ellps",
    "datum",
    "a",
    "b",
    "rf",
    "f",
    "es",
    "e",
    "R",
    "R_A",
    "R_C",
    "units",
    "to_meter",
    "towgs84",
    "nadgrids",
    "geoidgrids",
    "vunits",
    "vto_meter",
    "geoc",
    "init",
    "type",
    "pm",
    "axis",
    "no_defs",
    "wktext",
    "over",
];

/// A PROJ projection and its parameters by their PROJ names.
type ProjMethod = (String, Vec<(String, f64)>);

/// A method PROJ writes as `PROJ name` (a projection without a WKT 2 method of its own), with
/// its parameters, named as in PROJ strings: angles in degrees and lengths in metres. `None` for
/// other methods.
fn proj_method(method: &Wkt2Method, ang: f64, to_meter: f64) -> Result<Option<ProjMethod>> {
    let Some(name) = method.name.strip_prefix("PROJ ") else { return Ok(None) };
    let word = |w: &str| !w.is_empty() && w.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_');
    // PROJ writes a projection's string-valued parameters and flags in the method's name:
    // `PROJ ob_tran o_proj=moll`, `PROJ isea orient=pole`, `PROJ isea dual net=a5`.
    let mut words = name.split(' ');
    let mut name = match words.next() {
        Some(w) if word(w) => w.to_string(),
        _ => return Ok(None),
    };
    // The string-valued parameters each projection takes; the others are PARAMETERs.
    let (strings, flags): (&[&str], &[&str]) = match name.as_str() {
        "ob_tran" => (&["o_proj"], &[]),
        "isea" => (&["orient", "mode", "net"], &["dual"]),
        _ => (&[], &[]),
    };
    let mut keys: Vec<&str> = Vec::new();
    for w in words {
        match w.split_once('=') {
            Some((k, v)) if word(k) && word(v) => {
                if !strings.contains(&k) || keys.contains(&k) {
                    return Err(bad(&format!("parameter '{k}' of the method 'PROJ {}'", method.name)));
                }
                keys.push(k);
                name.push_str(&format!(" +{k}={v}"));
            }
            None if flags.contains(&w) && !keys.contains(&w) => {
                keys.push(w);
                name.push_str(&format!(" +{w}"));
            }
            _ => return Ok(None),
        }
    }
    let mut params = Vec::new();
    for n in &method.params {
        let key = n.name.as_str();
        if key.is_empty() || !key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') || keys.contains(&key) {
            return Err(bad(&format!("parameter '{key}' of the method 'PROJ {name}'")));
        }
        let v = n.value;
        let v = if let Some(u) = n.size(&["ANGLEUNIT"]) {
            v * degrees(u)
        } else if let Some(u) = n.size(&["LENGTHUNIT"]) {
            v * u
        } else if n.has("SCALEUNIT") {
            v
        } else if ["lat", "lon", "o_lat", "o_lon"].iter().any(|k| key.starts_with(k))
            || matches!(key, "alpha" | "gamma" | "azi" | "theta" | "tilt" | "o_alpha")
        {
            // Without a unit: angles in the CRS's angular unit, lengths in its linear unit, and
            // the others (scale factors, shape parameters) as they are.
            v * ang
        } else if matches!(key, "x_0" | "y_0" | "h") {
            v * to_meter
        } else {
            v
        };
        params.push((key.to_string(), v));
    }
    Ok(Some((name, params)))
}

/// The axis order of a projected CS as PROJ's `+axis`, when not east then north. As GDAL's
/// traditional GIS order, north-east is read east-north. Other direction pairs retain their
/// declared order, including north-west and south-east. A CS with an axis along neither
/// (`south, up`) is read east-north, as PROJ 9.9.0 reads it.
fn axis(directions: &[String]) -> Option<String> {
    let letter = |d: &str| match d {
        "east" => Some('e'),
        "west" => Some('w'),
        "north" => Some('n'),
        "south" => Some('s'),
        _ => None,
    };
    // Two axes along east-west and north-south: in their order, as GDAL 3.10 and PROJ 9.8.1
    // (`always_xy`) read them, but for northing then easting, read as easting then northing.
    if let [a, b] = directions.get(..2).unwrap_or_default()
        && let (Some(a), Some(b)) = (letter(a), letter(b))
        && matches!(a, 'e' | 'w') != matches!(b, 'e' | 'w')
    {
        return (!matches!((a, b), ('e', 'n') | ('n', 'e'))).then(|| format!("{a}{b}u"));
    }
    // Two axes along one line, east and west or north and south, which PROJ 9.9.0 refuses: west or
    // south, as terrio read them before. Other axes (`up`, `northNorthEast`, one direction twice)
    // are read east then north, as PROJ 9.9.0 reads them.
    let [a, b] = directions.get(..2).unwrap_or_default() else { return None };
    if a == b || letter(a).is_none() || letter(b).is_none() {
        return None;
    }
    let has = |d: &str| directions.iter().any(|x| x == d);
    let (e, n) = (if has("west") { 'w' } else { 'e' }, if has("south") { 's' } else { 'n' });
    (e != 'e' || n != 'n').then(|| format!("{e}{n}u"))
}

/// Whether a `BOUNDCRS`'s source CRS is a WKT 1 geographic or projected CRS (`GEOGCS`,
/// `PROJCS`), as GDAL writes a bound CRS's source when it has only WKT 1 for it.
fn is_wkt1_source(n: &Node) -> bool {
    n.key.eq_ignore_ascii_case("GEOGCS") || n.key.eq_ignore_ascii_case("PROJCS")
}

/// A `BOUNDCRS`'s WKT 1 source CRS without the `TOWGS84` of its datum: the bound transformation
/// replaces it, as PROJ 9.8.1 transforms such a CRS.
fn wkt1_source(source: &Node) -> Node {
    let mut source = source.clone();
    let geogcs = if source.key.eq_ignore_ascii_case("PROJCS") { source.child_mut("GEOGCS") } else { Some(&mut source) };
    if let Some(datum) = geogcs.and_then(|g| g.child_mut("DATUM")) {
        datum.args.retain(|a| !matches!(a, Arg::Node(n) if n.key.eq_ignore_ascii_case("TOWGS84")));
    }
    source
}

/// The refusal of a `BOUNDCRS` whose WKT 1 source CRS the WKT 1 reader refuses.
pub(crate) fn wkt1_source_refused() -> Error {
    bad("a BOUNDCRS whose WKT 1 source CRS terrio cannot read")
}

/// The first child node of a node.
pub(crate) fn first_node(n: &Node) -> Option<&Node> {
    n.args.iter().find_map(|a| if let Arg::Node(n) = a { Some(n) } else { None })
}

pub(crate) fn bad(what: &str) -> Error {
    Error::Crs(format!("unsupported WKT 2: {what}"))
}

/// A geodetic CRS's ellipsoid, prime meridian, angular unit and datum, and its node.
struct Geodetic {
    node: Node,
    name: Option<String>,
    a: f64,
    rf: f64,
    /// The angular unit, in degrees.
    ang: f64,
    /// The prime meridian, in degrees.
    pm: f64,
    /// The EPSG geographic CRS this one is, by its code or its datum's.
    geog: Option<u32>,
    datum_name: String,
    ellipsoid_name: String,
}

impl Geodetic {
    fn new(g: &Wkt2Geodetic, n: &Node) -> Self {
        let (a, rf) = (g.a, g.rf);
        // The EPSG geographic CRS: by the CRS's code, else its datum's (6xxx for 4xxx), else its
        // name, on the same ellipsoid.
        let geog = g.epsg.or_else(|| {
            let d = g.datum_epsg?;
            let code = d.checked_sub(2000)?;
            super::epsg::get(code).filter(|e| e.datum == d && e.proj.starts_with("+proj=longlat")).map(|_| code)
        });
        let geog = geog.or_else(|| {
            let name = g.name.as_deref().filter(|s| !s.eq_ignore_ascii_case("unknown"))?;
            // The official name is exact (realizations have their own, "NAD83(HARN)"); still,
            // an ambiguous name identifies nothing.
            let es = if rf == 0.0 { 0.0 } else { (2.0 - 1.0 / rf) / rf };
            let found: Vec<u32> = super::epsg::named(name).into_iter().filter(|c| same_ellipsoid(*c, a, es)).collect();
            (found.len() == 1).then(|| found[0])
        });
        Self {
            node: n.clone(),
            name: g.name.clone(),
            a,
            rf,
            ang: g.ang,
            pm: g.pm,
            geog,
            datum_name: g.datum_name.clone().unwrap_or_else(|| "unknown".into()),
            ellipsoid_name: g.ellipsoid_name.clone().unwrap_or_else(|| "unknown".into()),
        }
    }

    /// The ellipsoid and prime meridian, as the end of a PROJ string.
    fn tail(&self) -> Vec<String> {
        let mut t = vec![format!("+a={}", self.a), if self.rf == 0.0 { format!("+b={}", self.a) } else { format!("+rf={}", self.rf) }];
        if self.pm != 0.0 {
            t.push(format!("+pm={}", self.pm));
        }
        t
    }

    fn converted(&self, proj: String) -> Converted {
        let datum = DatumOf {
            crs_name: self.name.clone().unwrap_or_else(|| "unknown".into()),
            datum_name: self.datum_name.clone(),
            ellipsoid_name: self.ellipsoid_name.clone(),
            a: self.a,
            rf: self.rf,
            pm: self.pm,
        };
        Converted { proj, geogcs: self.node.clone(), geog: self.geog, datum: Some(datum), bound: None, helmert: None }
    }
}

/// A WKT 2 geodetic CRS's datum, as [`crate::identify_datum`] identifies it when it has no code:
/// the CRS's, datum's and ellipsoid's names (`unknown` when unnamed), the ellipsoid, and the prime
/// meridian in degrees.
#[derive(Debug, Clone)]
pub(crate) struct DatumOf {
    pub crs_name: String,
    pub datum_name: String,
    pub ellipsoid_name: String,
    pub a: f64,
    /// The inverse flattening, 0 for a sphere.
    pub rf: f64,
    pub pm: f64,
}

impl DatumOf {
    /// A geographic CRS in degrees on the datum, as a PROJ string.
    pub(crate) fn proj(&self) -> String {
        let shape = if self.rf == 0.0 { format!("+b={}", self.a) } else { format!("+rf={}", self.rf) };
        let pm = if self.pm != 0.0 { format!(" +pm={}", self.pm) } else { String::new() };
        format!("+proj=longlat +a={} {shape}{pm}", self.a)
    }
}

/// Whether an EPSG geographic 2D CRS is on an ellipsoid (its semi-major axis and eccentricity
/// squared).
pub(crate) fn same_ellipsoid(code: u32, a: f64, es: f64) -> bool {
    let Some(e) = super::epsg::get(code).filter(|e| e.proj.starts_with("+proj=longlat") && !e.three_d) else { return false };
    let Ok(d) = super::def::Def::from_proj(&e.proj) else { return false };
    crate::fits::same_figure(d.ell.a, d.ell.es, a, es)
}

/// The size of an angular unit given in radians, in degrees, with WKT's rounded degree and grad
/// made exact.
pub(crate) fn degrees(radians: f64) -> f64 {
    let d = radians.to_degrees();
    [1.0, 0.9, 1.0 / 3600.0].into_iter().find(|u| (d - u).abs() < 1e-9 * u).unwrap_or(d)
}

use crate::methods::{
    AZIMUTH, CO_LAT_CONE, EVAL_1, EVAL_2, FE, FE_CENTRE, FE_FALSE, FN, FN_CENTRE, FN_FALSE, GAMMA, H_PLANE_ORIGIN, HEIGHT_TOPO, K_CENTRE,
    K_ELLIPSOID, K_NAT, K_PSEUDO_SP, KROVAK_C, Kind, LAT_CENTRE, LAT_FALSE, LAT_NAT, LAT_P1, LAT_P2, LAT_PSEUDO_SP, LAT_TOPO,
    LAT_TRUE_SCALE, LAT_TS, LON_CENTRE, LON_FALSE, LON_INITIAL, LON_NAT, LON_ORIGIN, LON_P1, LON_P2, LON_TOPO, Param, SAT_HEIGHT, SP1, SP2,
    VIEW_HEIGHT, ZONE_WIDTH,
};

/// The parameters the WKT 2 reader takes.
const PARAMS: [Param; 49] = [
    LAT_TOPO,
    LON_TOPO,
    HEIGHT_TOPO,
    VIEW_HEIGHT,
    SAT_HEIGHT,
    LAT_P1,
    LON_P1,
    LAT_P2,
    LON_P2,
    LAT_TRUE_SCALE,
    LAT_NAT,
    LON_NAT,
    K_NAT,
    FE,
    FN,
    LAT_CENTRE,
    LON_CENTRE,
    AZIMUTH,
    GAMMA,
    K_CENTRE,
    FE_CENTRE,
    FN_CENTRE,
    LAT_FALSE,
    LON_FALSE,
    SP1,
    SP2,
    FE_FALSE,
    FN_FALSE,
    LAT_TS,
    LON_ORIGIN,
    LON_INITIAL,
    ZONE_WIDTH,
    CO_LAT_CONE,
    LAT_PSEUDO_SP,
    K_PSEUDO_SP,
    K_ELLIPSOID,
    H_PLANE_ORIGIN,
    EVAL_1,
    EVAL_2,
    KROVAK_C[0],
    KROVAK_C[1],
    KROVAK_C[2],
    KROVAK_C[3],
    KROVAK_C[4],
    KROVAK_C[5],
    KROVAK_C[6],
    KROVAK_C[7],
    KROVAK_C[8],
    KROVAK_C[9],
];

/// A conversion's parameters, in degrees, metres and unity, and which were used.
#[derive(Default)]
struct Params {
    values: Vec<(u32, String, f64)>,
    used: std::cell::RefCell<Vec<u32>>,
}

/// Canonical identity and SI unit kind of a supported projection parameter.
pub(crate) fn parameter_info(name: &str, code: Option<u32>) -> Option<(u32, &'static str)> {
    let key = norm(name);
    let by_name = PARAMS.iter().find(|p| p.named(&key));
    let by_code = code.and_then(|c| PARAMS.iter().find(|p| p.code == c));
    if let (Some(a), Some(b)) = (by_name, by_code)
        && a.code != b.code
    {
        return None;
    }
    let Param { code: id, kind, .. } = *by_code.or(by_name)?;
    let id = if id == FE_FALSE.code {
        FE.code
    } else if id == FN_FALSE.code {
        FN.code
    } else {
        id
    };
    Some((
        id,
        match kind {
            Kind::Angle => "ANGLEUNIT",
            Kind::Length => "LENGTHUNIT",
            Kind::Scale => "SCALEUNIT",
        },
    ))
}

impl Params {
    fn read(params: &[Wkt2Param], ang: f64, to_meter: f64) -> Result<Self> {
        let mut values = Vec::new();
        for n in params {
            let name = n.name.as_str();
            let key = norm(name);
            let param =
                PARAMS.iter().find(|p| p.named(&key) || n.epsg == Some(p.code)).ok_or_else(|| bad(&format!("parameter '{name}'")))?;
            let v = n.value;
            let unit = n.size(&UNIT_KEYS);
            let v = match param.kind {
                Kind::Angle => v * unit.map(degrees).unwrap_or(ang),
                Kind::Length => v * unit.unwrap_or(to_meter),
                Kind::Scale => v * unit.unwrap_or(1.0),
            };
            values.push((param.code, name.to_string(), v));
        }
        // Easting and northing at the false origin are the false easting and northing.
        for v in &mut values {
            if v.0 == FE_FALSE.code {
                v.0 = FE.code;
            } else if v.0 == FN_FALSE.code {
                v.0 = FN.code;
            }
        }
        Ok(Self { values, used: Default::default() })
    }

    /// The value of a parameter, marked used.
    fn get(&self, p: Param) -> Option<f64> {
        let v = self.values.iter().find(|v| v.0 == p.code)?;
        self.used.borrow_mut().push(p.code);
        Some(v.2)
    }

    /// Marks a parameter used without reading it: one PROJ ignores for the method.
    fn ignore(&self, p: Param) {
        self.used.borrow_mut().push(p.code);
    }

    /// The name of a parameter no method arm used, with a value that matters (not 0).
    fn unused(&self) -> Option<&str> {
        let used = self.used.borrow();
        self.values.iter().find(|v| !used.contains(&v.0) && v.2 != 0.0).map(|v| v.1.as_str())
    }
}

/// Whether a `BOUNDCRS`'s target is WGS 84.
pub(crate) fn bound_to_wgs84(root: &Node) -> bool {
    root.child("TARGETCRS").and_then(first_node).is_some_and(|t| {
        matches!(t.epsg(), Some(4326 | 4979))
            || t.datum().is_some_and(|d| d.epsg() == Some(6326) || norm(d.name().unwrap_or("")).starts_with("worldgeodeticsystem1984"))
    })
}

// The EPSG codes of the Helmert transformation methods PROJ 9.8.1 applies (`proj_constants.h`).
const CF_FULL_MATRIX: [u32; 3] = [1132, 1133, 1140];
const CF: [u32; 4] = [1032, 9607, 1038, 1149];
const CF_TIME_DEPENDENT: [u32; 3] = [1056, 1057, 1058];
const PV: [u32; 3] = [1033, 9606, 1037];
const PV_TIME_DEPENDENT: [u32; 3] = [1053, 1054, 1055];
const TRANSLATIONS: [u32; 3] = [1031, 9603, 1035];
const BADEKAS_CF: [u32; 3] = [1034, 1039, 9636];
const BADEKAS_PV: [u32; 3] = [1061, 1062, 1063];

/// The EPSG names of those methods (`proj_constants.h`), by which PROJ 9.8.1 gives a method
/// without an `ID` its code (`OperationMethod::getEPSGCode` in `singleoperation.cpp`, which looks
/// the name, less a " (3D)" suffix, up in `parammappings.cpp`'s `methodNameCodesList`, compared
/// ignoring case and punctuation).
const HELMERT_NAMES: [(u32, &str); 25] = [
    (1031, "Geocentric translations (geocentric domain)"),
    (1032, "Coordinate Frame rotation (geocentric domain)"),
    (1033, "Position Vector transformation (geocentric domain)"),
    (1034, "Molodensky-Badekas (CF geocentric domain)"),
    (1035, "Geocentric translations (geog3D domain)"),
    (1037, "Position Vector transformation (geog3D domain)"),
    (1038, "Coordinate Frame rotation (geog3D domain)"),
    (1039, "Molodensky-Badekas (CF geog3D domain)"),
    (1053, "Time-dependent Position Vector tfm (geocentric)"),
    (1054, "Time-dependent Position Vector tfm (geog2D)"),
    (1055, "Time-dependent Position Vector tfm (geog3D)"),
    (1056, "Time-dependent Coordinate Frame rotation (geocen)"),
    (1057, "Time-dependent Coordinate Frame rotation (geog2D)"),
    (1058, "Time-dependent Coordinate Frame rotation (geog3D)"),
    (1061, "Molodensky-Badekas (PV geocentric domain)"),
    (1062, "Molodensky-Badekas (PV geog3D domain)"),
    (1063, "Molodensky-Badekas (PV geog2D domain)"),
    (1132, "Coordinate Frame rotation full matrix (geocen)"),
    (1133, "Coordinate Frame rotation full matrix (geog2D)"),
    (1140, "Coordinate Frame rotation full matrix (geog3D)"),
    (1149, "Coordinate Frame rotation (geog3D to compound)"),
    (9603, "Geocentric translations (geog2D domain)"),
    (9606, "Position Vector transformation (geog2D domain)"),
    (9607, "Coordinate Frame rotation (geog2D domain)"),
    (9636, "Molodensky-Badekas (CF geog2D domain)"),
];

/// How PROJ applies a Helmert transformation method.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HelmertKind {
    /// Geocentric translations: three parameters, whatever else is given.
    Translations,
    /// Position vector or coordinate frame: seven.
    Seven,
    /// Time-dependent: seven, with rates and a reference epoch that a `BOUNDCRS`, which has no
    /// coordinate epoch, does not apply.
    TimeDependent,
    /// Molodensky-Badekas: seven, about an evaluation point.
    Badekas,
}

/// A Helmert transformation method, as PROJ 9.8.1 recognizes it.
struct HelmertMethod {
    kind: HelmertKind,
    /// Coordinate frame, whose rotations turn the other way from position vector's.
    frame: bool,
    /// Coordinate frame's full matrix: the rotation exact, not the small-angle approximation.
    exact: bool,
}

/// A transformation's method name (lower case), its EPSG code (its `ID`, else that of its EPSG
/// name), and its number of parameters.
fn method_of(t: &Wkt2Transformation) -> (String, Option<u32>, usize) {
    let name = t.method.to_ascii_lowercase();
    let code = t.epsg.or_else(|| {
        // Its EPSG name, less a " (3D)" suffix, as `getEPSGCode` looks it up.
        let key = norm(name.strip_suffix(" (3d)").unwrap_or(&name));
        HELMERT_NAMES.iter().find(|(_, n)| norm(n) == key).map(|(c, _)| *c)
    });
    // A grid's file counts as a parameter, as in PROJ.
    (name, code, t.params.len() + t.files)
}

/// The Helmert method of a `BOUNDCRS`'s `ABRIDGEDTRANSFORMATION`, as PROJ 9.8.1 recognizes it
/// (`singleoperation.cpp`'s `exportToPROJStringGeneric`): by its EPSG code, else by its name with
/// the number of parameters its kind has (seven, fifteen for a time-dependent one, three for
/// geocentric translations). Molodensky-Badekas only by its code. `None` for another method.
fn helmert_method(t: &Wkt2Transformation) -> Option<HelmertMethod> {
    let (name, code, count) = method_of(t);
    let is = |codes: &[u32]| code.is_some_and(|c| codes.contains(&c));
    // `oputils.cpp`'s `isTimeDependent`, and PROJ's tests of the name's convention.
    let time_dependent = name.contains("time dependent") || name.contains("time-dependent");
    let vector = name.contains("position vector") || name.contains("pv");
    let frame = name.contains("coordinate frame") || name.contains("cf");
    let method = |kind, frame, exact| Some(HelmertMethod { kind, frame, exact });
    use HelmertKind::*;
    if is(&CF_FULL_MATRIX) {
        method(Seven, true, true)
    } else if (count == 7 && frame && !time_dependent) || is(&CF) {
        method(Seven, true, false)
    } else if (count == 15 && frame && time_dependent) || is(&CF_TIME_DEPENDENT) {
        method(TimeDependent, true, false)
    } else if (count == 7 && vector && !time_dependent) || is(&PV) {
        method(Seven, false, false)
    } else if (count == 15 && vector && time_dependent) || is(&PV_TIME_DEPENDENT) {
        method(TimeDependent, false, false)
    } else if (count == 3 && name.contains("geocentric translations")) || is(&TRANSLATIONS) {
        method(Translations, false, false)
    } else if is(&BADEKAS_CF) || is(&BADEKAS_PV) {
        method(Badekas, !(vector || is(&BADEKAS_PV)), false)
    } else {
        None
    }
}

/// Whether a transformation method PROJ 9.8.1 does not apply is one of the Helmert family (a
/// time-specific one, which needs a coordinate epoch; one given by name without its kind's
/// number of parameters, as geocentric translations without three), which terrio refuses rather than
/// reading the `BOUNDCRS` without its shift.
fn helmert_family(t: &Wkt2Transformation) -> bool {
    let name = t.method.to_ascii_lowercase();
    ["position vector", "coordinate frame", "geocentric translations", "helmert", "molodensky-badekas", "time-specific"]
        .iter()
        .any(|k| name.contains(k))
        || matches!(t.epsg, Some(1065 | 1066))
}

/// A `BOUNDCRS`'s Helmert transformation, read: its method, its parameters in the method's own
/// convention, and which of the seven it gives (a bit each, translations first).
struct HelmertValues {
    method: HelmertMethod,
    t: [f64; 3],
    r: [f64; 3],
    s: f64,
    pivot: [f64; 3],
    given: u8,
    /// Geocentric translations given a non-zero rotation or scale, which PROJ does not apply.
    unapplied: bool,
}

impl HelmertValues {
    /// The `+towgs84` values (position vector convention, small-angle), 3 or 7 of them.
    fn towgs84(&self) -> Vec<f64> {
        if self.method.kind == HelmertKind::Translations || self.given & !0b111 == 0 {
            return self.t.to_vec();
        }
        let sign = if self.method.frame { -1.0 } else { 1.0 };
        vec![self.t[0], self.t[1], self.t[2], self.r[0] * sign, self.r[1] * sign, self.r[2] * sign, self.s]
    }

    /// The transformation, where `+towgs84` cannot stand for it: a full rotation matrix, or a
    /// Molodensky-Badekas evaluation point.
    fn beyond_towgs84(&self) -> Option<Helmert> {
        (self.method.exact || self.pivot != [0.0; 3]).then_some(Helmert {
            t: self.t,
            r: self.r,
            s: self.s,
            position_vector: !self.method.frame,
            exact: self.method.exact,
            pivot: self.pivot,
        })
    }
}

/// The Helmert transformation of a `BOUNDCRS`'s `ABRIDGEDTRANSFORMATION`. `None` for a grid's
/// (NTv2 or NADCON, whose file is a `PARAMETERFILE`), which terrio does not read. An error for any
/// other method terrio does not apply: one of the Helmert family PROJ 9.8.1 does not apply, a name
/// PROJ does not know ("PositionVector", "Input is not a transformation"), or one PROJ applies and
/// terrio does not (Molodensky, EPSG 9604 and 9605); and for an unknown parameter.
fn helmert_values(t: &Wkt2Transformation) -> Result<Option<HelmertValues>> {
    let Some(method) = helmert_method(t) else {
        if helmert_family(t) {
            return Err(bad("a Helmert transformation PROJ 9.8.1 does not apply"));
        }
        if t.files == 0 {
            return Err(bad("a BOUNDCRS transformation method terrio does not apply"));
        }
        return Ok(None);
    };
    let (mut tr, mut r, mut s, mut pivot, mut given) = ([0.0; 3], [0.0; 3], 0.0, [0.0; 3], 0u8);
    for p in &t.params {
        let name = norm(&p.name);
        let x = p.value.ok_or_else(|| bad("a transformation parameter without a value"))?;
        let unit = UNIT_KEYS.iter().find_map(|k| p.units.iter().find(|u| u.0 == *k)).and_then(|u| u.1);
        let code = p.epsg;
        // A time-dependent method's rates and reference epoch, which PROJ 9.8.1 does not apply
        // without a coordinate epoch.
        if method.kind == HelmertKind::TimeDependent
            && (name.starts_with("rateofchange") || name.ends_with("referenceepoch") || matches!(code, Some(1040..=1047 | 1049)))
        {
            continue;
        }
        let i = match (name.as_str(), code) {
            ("xaxistranslation", _) | (_, Some(8605)) => 0,
            ("yaxistranslation", _) | (_, Some(8606)) => 1,
            ("zaxistranslation", _) | (_, Some(8607)) => 2,
            ("xaxisrotation", _) | (_, Some(8608)) => 3,
            ("yaxisrotation", _) | (_, Some(8609)) => 4,
            ("zaxisrotation", _) | (_, Some(8610)) => 5,
            ("scaledifference", _) | (_, Some(8611)) => 6,
            ("ordinate1ofevaluationpoint", _) | (_, Some(8617)) if method.kind == HelmertKind::Badekas => 7,
            ("ordinate2ofevaluationpoint", _) | (_, Some(8618)) if method.kind == HelmertKind::Badekas => 8,
            ("ordinate3ofevaluationpoint", _) | (_, Some(8667)) if method.kind == HelmertKind::Badekas => 9,
            // Another parameter is refused, not dropped.
            _ => return Err(bad(&format!("transformation parameter '{}'", p.name))),
        };
        match i {
            0..=2 => tr[i] = x * unit.unwrap_or(1.0),
            // Arc-seconds, and parts per million.
            3..=5 => r[i - 3] = x * unit.map(|u| u.to_degrees() * 3600.0).unwrap_or(1.0),
            // A scale difference as the ratio PROJ writes, in unity (1.0000067 for 6.7 ppm), or
            // without a unit; in another unit (parts per million), in that unit, which PROJ 9.8.1
            // would read as a ratio too. Any other unit-less value is ambiguous.
            6 => {
                s = match unit {
                    Some(1.0) => (x - 1.0) * 1e6,
                    Some(u) => x * u * 1e6,
                    None if (x - 1.0).abs() < 0.01 => (x - 1.0) * 1e6,
                    None => return Err(bad("a scale difference without its unit")),
                }
            }
            _ => pivot[i - 7] = x * unit.unwrap_or(1.0),
        }
        if i < 7 {
            given |= 1 << i;
        }
    }
    // Geocentric translations are three, whatever else is given, as PROJ applies them.
    let unapplied = method.kind == HelmertKind::Translations && (r != [0.0; 3] || s != 0.0);
    if method.kind == HelmertKind::Translations {
        (r, s) = ([0.0; 3], 0.0);
    }
    Ok(Some(HelmertValues { method, t: tr, r, s, pivot, given, unapplied }))
}

/// A `BOUNDCRS`'s Helmert transformation: its `+towgs84` values, whether it is to WGS 84, and the
/// transformation itself where `+towgs84` cannot stand for it.
type BoundShift = (Vec<f64>, bool, Option<Helmert>);

/// The 3 or 7 parameters (as `+towgs84`'s) of a `BOUNDCRS`'s Helmert transformation, whether it
/// is to WGS 84, and the transformation itself where `+towgs84` cannot stand for it: `None` when
/// it is not a supported Helmert operation.
fn helmert_shift(t: &Wkt2Transformation, to_wgs84: bool) -> Result<Option<BoundShift>> {
    let Some(v) = helmert_values(t)? else { return Ok(None) };
    // Even zero parameters declare a Cartesian datum operation. Keeping it is
    // necessary when the source and destination ellipsoids differ.
    Ok(Some((v.towgs84(), to_wgs84, v.beyond_towgs84())))
}

/// Whether a `BOUNDCRS`'s Helmert transformation has a PROJ string that places points where
/// PROJ 9.8.1 transforms them (`transformation.cpp`'s `getTOWGS84Parameters`): position vector
/// and coordinate frame with each of their seven parameters (one given twice leaves another out),
/// and geocentric translations with their three and without a rotation or scale. Not the
/// time-dependent methods by code and the Molodensky-Badekas ones, which PROJ writes no string
/// for (a time-dependent one named with its convention's words and seven parameters PROJ writes,
/// as terrio does). Nor coordinate frame's full matrix, or geocentric translations given a
/// rotation or scale, which PROJ writes as a `+towgs84` placing points elsewhere.
pub(crate) fn helmert_writable(t: &Node) -> bool {
    let t = &Wkt2Transformation::read(t);
    // The methods `getTOWGS84Parameters` writes: by code, or by a name with the whole words of
    // its convention and its number of parameters (not a Molodensky-Badekas "CF" one).
    let (name, code, count) = method_of(t);
    let is = |codes: &[u32]| code.is_some_and(|c| codes.contains(&c));
    let towgs84 = is(&CF)
        || is(&CF_FULL_MATRIX)
        || is(&PV)
        || is(&TRANSLATIONS)
        || (count == 7 && (name.contains("coordinate frame") || name.contains("position vector")))
        || (count == 3 && name.contains("geocentric translations"));
    if !towgs84 {
        return helmert_method(t).is_none() && !helmert_family(t);
    }
    let Ok(values) = helmert_values(t) else { return false };
    let Some(v) = values else { return true };
    match v.method.kind {
        // A time-dependent one gets here only by the name of its convention with seven
        // parameters, which `getTOWGS84Parameters` writes as it is.
        HelmertKind::Seven | HelmertKind::TimeDependent => !v.method.exact && v.given == 0b111_1111,
        HelmertKind::Translations => !v.unapplied && v.given & 0b111 == 0b111,
        HelmertKind::Badekas => false,
    }
}
