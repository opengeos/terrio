// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Written to match the behavior of:
//   PROJ 9.9.0:
//     src/iso19111/io.cpp (WKTParser: guessDialect, buildGeodeticCRS, buildCS, buildAxis,
//       buildPrimeMeridian, buildVerticalCRS, buildCompoundCRS, buildBoundCRS)
// No code is translated from this source.

//! A CRS definition as a typed model, read once from its WKT tree, as PROJ reads one into its
//! `crs::CRS`, `datum::*`, `cs::*` and `operation::Conversion` objects (#1058 step 2).
//!
//! The model holds what the text says, in the text's own terms: names as written (ESRI's
//! `GCS_WGS_1984`, GDAL's `Ordnance_Survey_of_Great_Britain_1936`), every identifier at every
//! level, methods and parameters by their written names and IDs, units as written. Lengths of
//! ellipsoids are in metres and prime meridians in degrees, as PROJ reads them (its corrections of
//! prime meridians included). What PROJ takes from its database is not here: the official names of
//! ESRI's and GDAL's spellings, a WKT 1 CRS's axes by its code, EPSG names of WKT 1 methods and
//! parameters. Those are later steps of #1058 (4: identification, 5a: one method table).
//!
//! Until #1058 step 3 reads terrio's helpers from it, only tests use the model; its unit test
//! checks it against PROJ 9.9.0's PROJJSON of the #1066 corpus (`oracle_tests.rs`).

use crate::keywords as kw;
use crate::wkt1::{self, Arg, Node};
use crate::wkt2;

#[cfg(test)]
pub(crate) mod oracle_tests;
#[cfg(test)]
mod proj_input_tests;

/// An identifier: WKT 1's `AUTHORITY`, WKT 2's `ID`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Id {
    pub authority: String,
    /// The code as written: a number's digits, else the text.
    pub code: String,
    pub version: Option<String>,
}

/// A name and the identifiers of an object.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Ident {
    pub name: String,
    pub ids: Vec<Id>,
}

/// What a unit measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnitKind {
    Length,
    Angle,
    Scale,
    Time,
    Unknown,
}

/// A unit: its name and its size in metres, radians, unity or seconds, as written.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Unit {
    pub name: String,
    pub factor: f64,
    pub kind: UnitKind,
}

impl Unit {
    pub fn metre() -> Self {
        Self { name: "metre".into(), factor: 1.0, kind: UnitKind::Length }
    }

    pub fn degree() -> Self {
        Self { name: "degree".into(), factor: std::f64::consts::PI / 180.0, kind: UnitKind::Angle }
    }

    pub fn unity() -> Self {
        Self { name: "unity".into(), factor: 1.0, kind: UnitKind::Scale }
    }

    /// The size of an angular unit in degrees, WKT's rounded degree and grad made exact.
    pub fn degrees(&self) -> f64 {
        crate::wkt2::degrees(self.factor)
    }
}

/// An ellipsoid: its semi-major axis in metres and its inverse flattening (0 for a sphere).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Ellipsoid {
    pub ident: Ident,
    pub a: f64,
    pub rf: f64,
}

/// A prime meridian, its longitude in degrees as PROJ reads it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PrimeMeridian {
    pub ident: Ident,
    pub longitude: f64,
}

/// A geodetic datum: a reference frame, or an ensemble of them.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Datum {
    Frame {
        ident: Ident,
        ellipsoid: Ellipsoid,
        prime_meridian: PrimeMeridian,
        /// WKT 1's `TOWGS84`, its 3 or 7 values as written.
        towgs84: Option<Vec<f64>>,
        anchor_epoch: Option<f64>,
    },
    Ensemble {
        ident: Ident,
        members: Vec<Ident>,
        ellipsoid: Ellipsoid,
        prime_meridian: PrimeMeridian,
        accuracy: Option<f64>,
    },
}

impl Datum {
    pub fn ident(&self) -> &Ident {
        match self {
            Datum::Frame { ident, .. } | Datum::Ensemble { ident, .. } => ident,
        }
    }

    pub fn ellipsoid(&self) -> &Ellipsoid {
        match self {
            Datum::Frame { ellipsoid, .. } | Datum::Ensemble { ellipsoid, .. } => ellipsoid,
        }
    }

    pub fn prime_meridian(&self) -> &PrimeMeridian {
        match self {
            Datum::Frame { prime_meridian, .. } | Datum::Ensemble { prime_meridian, .. } => prime_meridian,
        }
    }
}

/// The kind of a coordinate system.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum CsKind {
    Ellipsoidal,
    Cartesian,
    Spherical,
    Vertical,
    Other(String),
}

/// An axis: its name as written, its direction as PROJ spells it (`north`, `geocentricX`), its
/// unit, and for a polar axis the meridian it runs along, in degrees.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Axis {
    pub name: String,
    pub direction: String,
    pub unit: Option<Unit>,
    pub meridian: Option<f64>,
}

/// A coordinate system. Without `AXIS` (or WKT 2's `CS`), its axes are the dialect's default and
/// `explicit` is false.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Cs {
    pub kind: CsKind,
    pub axes: Vec<Axis>,
    pub explicit: bool,
}

/// A parameter of a conversion or transformation, with the unit written with it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Param {
    pub ident: Ident,
    pub value: f64,
    pub unit: Option<Unit>,
}

/// A map projection or other conversion, as written. A parameter without a unit of its own is in
/// `angular` or `linear` by its kind: WKT 1's `GEOGCS` and `PROJCS` units, WKT 2's base CRS's
/// angular unit and coordinate system's length unit.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Conversion {
    pub ident: Ident,
    pub method: Ident,
    pub params: Vec<Param>,
    pub angular: Unit,
    pub linear: Unit,
}

/// A bound CRS's (abridged) transformation, to its target CRS.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Transformation {
    pub ident: Ident,
    pub method: Ident,
    pub params: Vec<Param>,
    /// Its `PARAMETERFILE`s (an NTv2 grid's), by name and file name.
    pub files: Vec<(Ident, String)>,
    pub target: Box<Crs>,
}

/// The members of a vertical datum ensemble, and its accuracy in metres.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Members {
    pub members: Vec<Ident>,
    pub accuracy: Option<f64>,
}

/// A CRS.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Crs {
    /// A geographic (ellipsoidal) or geocentric (Cartesian, spherical) CRS.
    Geodetic {
        ident: Ident,
        datum: Datum,
        cs: Cs,
        /// The CRS's angular unit, as `wkt2::Geodetic::read` takes it (degrees for a WKT 1
        /// `GEOCCS`), whatever its axes' own.
        angular: Unit,
        frame_epoch: Option<f64>,
    },
    Projected {
        ident: Ident,
        base: Box<Crs>,
        conversion: Conversion,
        cs: Cs,
    },
    /// A vertical CRS, with the geoid grids of GDAL's `PROJ4_GRIDS` extension (its datum's, else
    /// its own).
    Vertical {
        ident: Ident,
        datum: Ident,
        /// A datum ensemble's members and accuracy.
        ensemble: Option<Members>,
        anchor_epoch: Option<f64>,
        frame_epoch: Option<f64>,
        cs: Cs,
        grids: Option<String>,
    },
    Compound {
        ident: Ident,
        parts: Vec<Crs>,
    },
    Bound {
        source: Box<Crs>,
        transformation: Transformation,
    },
    /// An engineering CRS, or another this model does not describe, by its keyword.
    Other {
        ident: Ident,
        key: String,
    },
}

/// The WKT dialect of a text, as PROJ guesses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dialect {
    Wkt1Gdal,
    Wkt1Esri,
    Wkt2,
}

/// A CRS definition: the CRS, the dialect it was written in, and the root's extensions (GDAL's
/// `EXTENSION["PROJ4", ...]`), as written.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Model {
    pub crs: Crs,
    pub dialect: Dialect,
    pub extensions: Vec<(String, String)>,
    /// Whether the CRS's last axis is the ellipsoidal height of ESRI's compound CRS on a geodetic
    /// `DATUM` (read as a 3D CRS), which its horizontal CRS's WKT does not declare (its base CRS's
    /// own height, of a projected CRS, is not counted).
    pub height_added: bool,
}

/// The model of a WKT definition (WKT 1 GDAL, ESRI or WKT 2), or `None` when it is not one this
/// model reads: text that is not WKT (see [`wkt1::parse`]), or a CRS without what it needs (a
/// geodetic CRS without its datum or ellipsoid, a projected CRS without its base CRS or its
/// method, which PROJ 9.9.0 refuses).
pub(crate) fn read(text: &str) -> Option<Model> {
    // ESRI's compound CRS, `GEOGCS[...],VERTCS[...]`, as PROJ reads it: named after its parts.
    if let Some((h, v)) = wkt1::esri_compound(text) {
        // A `VERTCS` on a geodetic `DATUM` is an ellipsoidal height: PROJ 9.9.0 reads the two as
        // a 3D CRS (ESRI's LUREF / Luxembourg TM (3D), EPSG:9895), its base CRS 3D too.
        // The horizontal CRS's dialect as PROJ guesses it (a GDAL `PROJCS` with its `AUTHORITY`
        // stays GDAL's).
        let d = dialect(text, &h);
        if v.child("DATUM").is_some() {
            // Its height in the `VERTCS`'s unit and `Direction`; the base CRS's up in metres.
            let length = v.child("UNIT").and_then(|u| unit(u, UnitKind::Length)).unwrap_or_else(Unit::metre);
            let direction = if esri_down(&v) { "down" } else { "up" };
            let mut crs = crs(&h, d)?;
            // Whether the height was added (to a CS of two axes).
            let height = |cs: &mut Cs, unit: Unit, direction: &str| {
                let two = cs.axes.len() == 2;
                if two {
                    cs.axes.push(Axis { name: String::new(), direction: direction.into(), unit: Some(unit), meridian: None });
                }
                two
            };
            let height_added = match &mut crs {
                Crs::Projected { base, cs, .. } => {
                    if let Crs::Geodetic { cs, .. } = base.as_mut() {
                        height(cs, Unit::metre(), "up");
                    }
                    height(cs, length, direction)
                }
                Crs::Geodetic { cs, .. } => height(cs, length, direction),
                _ => false,
            };
            return Some(Model { crs, dialect: d, extensions: extensions(&h), height_added });
        }
        let parts = vec![crs(&h, d)?, crs(&v, Dialect::Wkt1Esri)?];
        let name = format!("{} + {}", h.name().unwrap_or(""), v.name().unwrap_or(""));
        let crs = Crs::Compound { ident: Ident { name, ids: Vec::new() }, parts };
        return Some(Model { crs, dialect: d, extensions: extensions(&h), height_added: false });
    }
    let root = wkt1::parse(text)?;
    let d = dialect(text, &root);
    Some(Model { crs: crs(&root, d)?, dialect: d, extensions: extensions(&root), height_added: false })
}

/// A WKT 1 CRS's input to its PROJ string, read from its model (#1058 step 6): what
/// [`wkt1::Wkt1Input::read`] reads from its tree (`root`, the horizontal CRS's of ESRI's compound),
/// but for the unusual WKT 1 `where_the_model_reads_unusual_wkt1_otherwise` pins, to settle in step
/// 6d as PROJ reads it.
pub(crate) fn wkt1_input(m: &Model, root: &Node) -> Option<wkt1::Wkt1Input> {
    use wkt1::{Wkt1Input, Wkt1Kind};
    if m.dialect == Dialect::Wkt2 {
        return None;
    }
    // The kind by the root's keyword, as the tree reader takes it.
    let kind = match root.key.to_ascii_uppercase().as_str() {
        "GEOGCS" => Wkt1Kind::Geographic,
        "GEOCCS" => Wkt1Kind::Geocentric,
        "PROJCS" => Wkt1Kind::Projected,
        _ => return None,
    };
    // ESRI's compound CRS, `GEOGCS[...],VERTCS[...]`: its horizontal CRS, which the tree's root is.
    let crs = match &m.crs {
        Crs::Compound { parts, .. } => parts.first()?,
        crs => crs,
    };
    let (base, conversion, cs) = match crs {
        Crs::Geodetic { cs, .. } if kind != Wkt1Kind::Projected => (crs, None, cs),
        Crs::Projected { base, conversion, cs, .. } if kind == Wkt1Kind::Projected => (base.as_ref(), Some(conversion), cs),
        _ => return None,
    };
    let Crs::Geodetic { ident: base_ident, datum, angular: base_angular, .. } = base else { return None };
    let ellipsoid = datum.ellipsoid();
    // The EPSG code of an object's first identifier, as `Node::epsg` reads it.
    let epsg =
        |i: &Ident| i.ids.first().filter(|id| id.authority.eq_ignore_ascii_case("EPSG")).and_then(|id| id.code.trim().parse::<u32>().ok());
    // The angular unit as WKT 1 rounds it (a degree and a grad, within 1e-9).
    let angular = match (kind, conversion) {
        (Wkt1Kind::Geocentric, _) => None,
        (_, Some(c)) => Some(&c.angular),
        _ => Some(base_angular),
    };
    let ang = angular.map_or(1.0, |u| u.factor.to_degrees());
    let ang = [1.0, 0.9].into_iter().find(|u| (ang - u).abs() < 1e-9).unwrap_or(ang);
    let towgs84 = match datum {
        Datum::Frame { towgs84, .. } => towgs84.clone(),
        Datum::Ensemble { .. } => None,
    };
    let unit = match (kind, conversion) {
        (Wkt1Kind::Geographic, _) => 1.0,
        (_, Some(c)) => c.linear.factor,
        _ => cs.axes.first().and_then(|a| a.unit.as_ref()).map_or(1.0, |u| u.factor),
    };
    let esri_names = base_ident.name.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("gcs_"))
        || datum.ident().name.get(..2).is_some_and(|p| p.eq_ignore_ascii_case("d_"));
    let mut input = Wkt1Input {
        kind,
        a: ellipsoid.a,
        rf: ellipsoid.rf,
        ang,
        pm: datum.prime_meridian().longitude,
        towgs84,
        unit,
        datum_epsg: epsg(datum.ident()),
        params: Vec::new(),
        method: None,
        method_epsg: None,
        esri_names,
        axes: None,
        has_axis: false,
    };
    let Some(c) = conversion else { return Some(input) };
    input.params = c.params.iter().map(|p| (wkt1::norm(&p.ident.name), p.value)).filter(|(k, _)| !k.is_empty()).collect();
    input.method = Some(c.method.name.clone()).filter(|n| !n.is_empty());
    input.method_epsg = epsg(&c.method);
    // A projection without a name, unless EPSG's variant C by its code.
    wkt1::wkt1_method(input.method.as_deref(), input.method_epsg)?;
    if cs.explicit {
        // Not the ellipsoidal height ESRI's 3D compound CRS adds.
        let axes = if m.height_added { &cs.axes[..cs.axes.len() - 1] } else { &cs.axes[..] };
        let letter = |a: &Axis| match a.direction.as_str() {
            "east" => Some(b'e'),
            "west" => Some(b'w'),
            "north" => Some(b'n'),
            "south" => Some(b's'),
            _ => None,
        };
        let letters: Option<Vec<u8>> = axes.iter().map(letter).collect();
        input.axes = letters.and_then(|l| match l[..] {
            [a, b] if matches!(a, b'e' | b'w') != matches!(b, b'e' | b'w') => Some([a, b]),
            _ => None,
        });
    }
    input.has_axis = cs.explicit;
    Some(input)
}

/// A WKT 2 geographic, geocentric or projected CRS's input to its PROJ string, read from its model
/// (#1058 step 6): what [`wkt2::Wkt2Input::read`] reads from its tree (`root`). `None` for what
/// the reader refuses, and for a derived or bound CRS (#1058 step 6c-2).
pub(crate) fn wkt2_input(m: &Model, root: &Node) -> Option<wkt2::Wkt2Input> {
    use wkt2::{Wkt2Geodetic, Wkt2Input, Wkt2Kind, Wkt2Method, Wkt2Param};
    if m.dialect != Dialect::Wkt2 || root.child("DERIVINGCONVERSION").is_some() {
        return None;
    }
    // The EPSG code of an object's first identifier, as `Node::epsg` reads it.
    let epsg =
        |i: &Ident| i.ids.first().filter(|id| id.authority.eq_ignore_ascii_case("EPSG")).and_then(|id| id.code.trim().parse::<u32>().ok());
    let geodetic = |crs: &Crs| {
        let Crs::Geodetic { ident, datum, angular, .. } = crs else { return None };
        let ellipsoid = datum.ellipsoid();
        Some(Wkt2Geodetic {
            name: Some(ident.name.clone()),
            epsg: epsg(ident),
            datum_name: Some(datum.ident().name.clone()),
            datum_epsg: epsg(datum.ident()),
            ellipsoid_name: Some(ellipsoid.ident.name.clone()),
            a: ellipsoid.a,
            rf: ellipsoid.rf,
            ang: angular.degrees(),
            pm: datum.prime_meridian().longitude,
        })
    };
    let lower = |a: &Axis| a.direction.to_ascii_lowercase();
    let (geodetic, kind) = match &m.crs {
        crs @ Crs::Geodetic { cs, .. } => {
            let kind = match cs.kind {
                CsKind::Cartesian => {
                    // Three axes, X, Y and Z in that order, in one length unit.
                    let directions = ["geocentricX", "geocentricY", "geocentricZ"];
                    let canonical = cs.explicit
                        && cs.axes.len() == 3
                        && cs.axes.iter().zip(directions).all(|(a, d)| a.direction.eq_ignore_ascii_case(d));
                    let units: Vec<f64> = cs.axes.iter().map(|a| a.unit.as_ref().map(|u| u.factor)).collect::<Option<_>>()?;
                    if !canonical || units.iter().any(|u| *u != units[0]) {
                        return None;
                    }
                    Wkt2Kind::Geocentric { unit: units[0] }
                }
                CsKind::Ellipsoidal | CsKind::Spherical => {
                    let spherical = cs.kind == CsKind::Spherical;
                    if spherical && cs.axes.len() != 2 {
                        return None;
                    }
                    // Two axes, one of them west or south: as `+axis` letters.
                    let directions: Vec<String> = if cs.explicit { cs.axes.iter().take(2).map(lower).collect() } else { Vec::new() };
                    let axes = if directions.len() == 2 && directions.iter().any(|d| d == "west" || d == "south") {
                        let letter = |d: &String| match d.as_str() {
                            "east" => Some('e'),
                            "west" => Some('w'),
                            "north" => Some('n'),
                            "south" => Some('s'),
                            _ => None,
                        };
                        Some(directions.iter().map(letter).collect::<Option<String>>()?)
                    } else {
                        None
                    };
                    Wkt2Kind::Geographic { spherical, axes }
                }
                _ => return None,
            };
            (geodetic(crs)?, kind)
        }
        Crs::Projected { base, conversion, cs, .. } => {
            // A polar CS's axes, which run along meridians, give no directions.
            let polar = cs.axes.iter().any(|a| a.meridian.is_some());
            let directions = if cs.explicit && !polar { cs.axes.iter().map(lower).collect() } else { Vec::new() };
            let key = |u: &Unit| match u.kind {
                UnitKind::Angle => "ANGLEUNIT",
                UnitKind::Length => "LENGTHUNIT",
                UnitKind::Scale => "SCALEUNIT",
                _ => "UNIT",
            };
            let params = conversion
                .params
                .iter()
                .map(|p| Wkt2Param {
                    name: p.ident.name.clone(),
                    epsg: epsg(&p.ident),
                    value: p.value,
                    units: p.unit.iter().map(|u| (key(u), Some(u.factor))).collect(),
                })
                .collect();
            let method = Wkt2Method { name: conversion.method.name.clone(), epsg: epsg(&conversion.method), params };
            (geodetic(base)?, Wkt2Kind::Projected { to_meter: conversion.linear.factor, directions, method })
        }
        _ => return None,
    };
    Some(Wkt2Input { geodetic, kind })
}

/// The `EXTENSION`s of a root node (of ESRI's sequence, its horizontal CRS's), as written.
fn extensions(root: &Node) -> Vec<(String, String)> {
    root.children("EXTENSION")
        .filter_map(|e| match e.args.get(1) {
            Some(Arg::Str(s)) => Some((e.name()?.to_string(), s.clone())),
            _ => None,
        })
        .collect()
}

/// The WKT 1 root keywords.
const WKT1_ROOTS: [&str; 7] = ["GEOGCS", "PROJCS", "GEOCCS", "VERT_CS", "VERTCS", "COMPD_CS", "LOCAL_CS"];

/// The dialect of a WKT, as PROJ 9.9.0 guesses it (`WKTParser::guessDialect`): WKT 1 by its root
/// keyword. ESRI's for a `VERTCS`; for a WKT 1 with a `GEOGCS["GCS_`; and for a WKT 1 without
/// `AXIS[` or `AUTHORITY[`, except a `LOCAL_CS` (GDAL's). In the last two cases, GDAL's instead
/// when it has a `rectified_grid_angle` parameter (GDAL's Hotine Oblique Mercator, which ESRI's has
/// not).
pub(crate) fn dialect(text: &str, root: &Node) -> Dialect {
    if !WKT1_ROOTS.iter().any(|k| root.key.eq_ignore_ascii_case(k)) {
        return Dialect::Wkt2;
    }
    if root.key.eq_ignore_ascii_case("VERTCS") {
        return Dialect::Wkt1Esri;
    }
    let lower = text.to_ascii_lowercase();
    let local = root.key.eq_ignore_ascii_case("LOCAL_CS");
    let esri = lower.contains("geogcs[\"gcs_") || (!local && !lower.contains("axis[") && !lower.contains("authority["));
    if esri && !lower.contains("parameter[\"rectified_grid_angle") { Dialect::Wkt1Esri } else { Dialect::Wkt1Gdal }
}

/// The CRS of a node, `None` for a node that is not a CRS.
fn crs(n: &Node, d: Dialect) -> Option<Crs> {
    if n.is(kw::WKT1_PROJECTED) || n.is(kw::PROJECTED_CRS) {
        projected(n, d)
    } else if n.is(kw::WKT1_GEOGRAPHIC) || n.is(kw::WKT1_GEOCENTRIC) || n.is(kw::GEOGRAPHIC_OR_GEODETIC_CRS) || n.is(kw::BASE_GEODETIC_CRS)
    {
        geodetic(n, d)
    } else if n.is(kw::VERTICAL_CRS) {
        vertical(n, d)
    } else if n.is(kw::COMPOUND_CRS) {
        // Every component, or none: a component without what it needs, or of a kind the model does
        // not read (WKT 2's `TIMECRS`), leaves the compound CRS unread.
        let parts =
            nodes(n).filter(|c| !METADATA.iter().any(|k| c.key.eq_ignore_ascii_case(k))).map(|c| crs(c, d)).collect::<Option<Vec<_>>>()?;
        Some(Crs::Compound { ident: ident(n), parts })
    } else if n.key.eq_ignore_ascii_case("BOUNDCRS") {
        bound(n, d)
    } else if n.is(kw::ENGINEERING_CRS) {
        Some(Crs::Other { ident: ident(n), key: n.key.to_ascii_uppercase() })
    } else {
        None
    }
}

/// The keywords of a CRS's metadata (WKT 2's identifiers, usages and remarks, WKT 1's
/// `AUTHORITY` and `EXTENSION`), which a compound CRS has beside its components.
const METADATA: [&str; 10] = ["ID", "AUTHORITY", "EXTENSION", "USAGE", "SCOPE", "AREA", "BBOX", "VERTICALEXTENT", "TIMEEXTENT", "REMARK"];

/// The prime meridian node of a geodetic CRS node: `PRIMEM`, or WKT 2's `PRIMEMERIDIAN`.
fn prime_meridian_node(n: &Node) -> Option<&Node> {
    n.child("PRIMEM").or_else(|| n.child("PRIMEMERIDIAN"))
}

/// Whether an ESRI `VERTCS` is down (`PARAMETER["Direction",-1]`), as PROJ 9.9.0 reads it.
fn esri_down(n: &Node) -> bool {
    n.children("PARAMETER").any(|p| p.name().is_some_and(|s| s.eq_ignore_ascii_case("Direction")) && p.num(1) == Some(-1.0))
}

/// The child nodes of a node.
fn nodes(n: &Node) -> impl Iterator<Item = &Node> {
    n.args.iter().filter_map(|a| match a {
        Arg::Node(c) => Some(c),
        _ => None,
    })
}

/// The name and identifiers of a node.
fn ident(n: &Node) -> Ident {
    Ident { name: n.name().unwrap_or("").to_string(), ids: n.ids().filter_map(id).collect() }
}

/// An `AUTHORITY` or `ID` node's identifier.
fn id(n: &Node) -> Option<Id> {
    let text = |a: &Arg| match a {
        Arg::Str(s) => Some(s.clone()),
        Arg::Num(v) if v.fract() == 0.0 && v.abs() < 1e15 => Some(format!("{}", *v as i64)),
        Arg::Num(v) => Some(v.to_string()),
        Arg::Ident(s) => Some(s.clone()),
        Arg::Node(_) => None,
    };
    let version = n.args.get(2).and_then(text);
    Some(Id { authority: n.name()?.to_string(), code: text(n.args.get(1)?)?, version })
}

/// A unit node: WKT 2's `LENGTHUNIT`, `ANGLEUNIT`, `SCALEUNIT`, `TIMEUNIT`, ESRI's `LINUNIT`, or a `UNIT` of the
/// kind its place gives it. `None` without its size, which PROJ refuses.
fn unit(n: &Node, kind: UnitKind) -> Option<Unit> {
    let kind = match n.key.to_ascii_uppercase().as_str() {
        // ESRI's `LINUNIT`, a 3D geographic CRS's height unit.
        "LENGTHUNIT" | "LINUNIT" => UnitKind::Length,
        "ANGLEUNIT" => UnitKind::Angle,
        "SCALEUNIT" => UnitKind::Scale,
        "TIMEUNIT" | "TEMPORALQUANTITY" => UnitKind::Time,
        "UNIT" => kind,
        _ => return None,
    };
    let factor = n.num(1).filter(|v| v.is_finite() && *v > 0.0)?;
    Some(Unit { name: n.name().unwrap_or("").to_string(), factor, kind })
}

/// The unit node of a node: its first of `LENGTHUNIT`, `ANGLEUNIT`, `SCALEUNIT`, `TIMEUNIT` or
/// `UNIT`, in its order.
fn unit_node(n: &Node) -> Option<&Node> {
    n.first_in(&["LENGTHUNIT", "ANGLEUNIT", "SCALEUNIT", "TIMEUNIT", "UNIT"])
}

/// The angular unit of a geodetic CRS node, as `wkt2::Geodetic::read` takes it: its own
/// `ANGLEUNIT` or `UNIT`, else (a WKT 2 CRS) its axes', else its prime meridian's, else degrees.
fn angular_unit(n: &Node, d: Dialect) -> Unit {
    let own = n.child("ANGLEUNIT").or_else(|| n.child("UNIT"));
    let found = own
        .or_else(
            || if d == Dialect::Wkt2 { n.children("AXIS").find_map(|a| a.child("ANGLEUNIT").or_else(|| a.child("UNIT"))) } else { None },
        )
        .or_else(|| prime_meridian_node(n).and_then(|p| p.child("ANGLEUNIT").or_else(|| p.child("UNIT"))));
    found.and_then(|u| unit(u, UnitKind::Angle)).unwrap_or_else(Unit::degree)
}

/// A geodetic CRS: a geographic or geocentric CRS, or the base CRS of a projected CRS.
fn geodetic(n: &Node, d: Dialect) -> Option<Crs> {
    let geocentric = n.is(kw::WKT1_GEOCENTRIC);
    let datum_node = n.datum()?;
    let ell = datum_node.first_in(kw::ELLIPSOID)?;
    let (a, rf) = ell.ellipsoid()?;
    let ellipsoid = Ellipsoid { ident: ident(ell), a, rf };
    // A WKT 1 `GEOCCS`'s `UNIT` is its length unit, and so is a WKT 2 Cartesian CS's; its prime
    // meridian is in degrees.
    let cartesian = n.child("CS").is_some_and(|c| matches!(c.args.first(), Some(Arg::Ident(k)) if k.eq_ignore_ascii_case("Cartesian")));
    let ang = if geocentric { Unit::degree() } else { angular_unit(n, d) };
    let prime_meridian = prime_meridian(n, d, geocentric)?;
    let datum = if datum_node.is(kw::GEODETIC_DATUM_ENSEMBLE) {
        Datum::Ensemble {
            ident: ident(datum_node),
            members: datum_node.children("MEMBER").map(ident).collect(),
            ellipsoid,
            prime_meridian,
            accuracy: datum_node.child("ENSEMBLEACCURACY").and_then(|a| a.num(0)),
        }
    } else {
        let towgs84 = datum_node.child("TOWGS84").map(|t| (0..t.args.len()).filter_map(|i| t.num(i)).collect());
        let anchor_epoch = datum_node.child("ANCHOREPOCH").and_then(|e| e.num(0));
        Datum::Frame { ident: ident(datum_node), ellipsoid, prime_meridian, towgs84, anchor_epoch }
    };
    let frame_epoch = n.child("DYNAMIC").and_then(|dy| dy.child("FRAMEEPOCH")).and_then(|e| e.num(0));
    let cs = if geocentric {
        // WKT 1's geocentric axes (`OTHER`, `EAST`, `NORTH` as GDAL writes them) are PROJ's X, Y
        // and Z.
        let length = n.child("UNIT").and_then(|u| unit(u, UnitKind::Length)).unwrap_or_else(Unit::metre);
        let axes = ["geocentricX", "geocentricY", "geocentricZ"];
        Cs {
            kind: CsKind::Cartesian,
            axes: axes
                .iter()
                .map(|dir| Axis { name: String::new(), direction: dir.to_string(), unit: Some(length.clone()), meridian: None })
                .collect(),
            explicit: n.child("AXIS").is_some(),
        }
    } else {
        // Latitude first in WKT 2, longitude first in WKT 1 (OGC 01-009). PROJ 9.9.0 gives a WKT 1
        // `GEOGCS` without `AXIS` the axes of the geographic CRS its root `AUTHORITY` names in its
        // database (#1058 step 4), else longitude first.
        let default = if d == Dialect::Wkt2 { ["north", "east"] } else { ["east", "north"] };
        let default_unit = if cartesian {
            n.child("LENGTHUNIT").or_else(|| n.child("UNIT")).and_then(|u| unit(u, UnitKind::Length)).unwrap_or_else(Unit::metre)
        } else {
            ang.clone()
        };
        // A WKT 2 Cartesian CS that is not three axes, X, Y and Z in that order, each in a length
        // unit, one for all, which PROJ 9.9.0 and the WKT 2 reader refuse.
        if d == Dialect::Wkt2
            && let Some(c) = n.child("CS").filter(|_| cartesian)
            && wkt2::geocentric_unit(n, c).is_err()
        {
            return None;
        }
        let mut cs = cs(n, d, CsKind::Ellipsoidal, &default, &default_unit);
        // ESRI's 3D geographic CRS: its `LINUNIT` is its ellipsoidal height's, as PROJ reads it.
        if !cs.explicit
            && let Some(length) = n.child("LINUNIT").and_then(|u| unit(u, UnitKind::Length))
        {
            cs.axes.push(Axis { name: String::new(), direction: "up".into(), unit: Some(length), meridian: None });
        }
        cs
    };
    Some(Crs::Geodetic { ident: ident(n), datum, cs, angular: ang, frame_epoch })
}

/// The prime meridian of a geodetic CRS node, as `wkt2::Geodetic::read` reads it: in its own unit,
/// else the CRS's (its `ANGLEUNIT`, or a `UNIT` named `degree` or `grad` in WKT 2; WKT 1's
/// `GEOGCS` unit), else degrees, with PROJ's corrections (`wkt1::prime_meridian_degrees`).
/// Greenwich without one. `None` for a WKT 2 prime meridian's unit without its size, which PROJ
/// 9.9.0 and the WKT 2 reader refuse.
fn prime_meridian(n: &Node, d: Dialect, geocentric: bool) -> Option<PrimeMeridian> {
    let Some(p) = prime_meridian_node(n) else {
        return Some(PrimeMeridian { ident: Ident { name: "Greenwich".into(), ids: Vec::new() }, longitude: 0.0 });
    };
    let crs_unit = if geocentric {
        None
    } else if d == Dialect::Wkt2 {
        if n.child("LENGTHUNIT").is_some() {
            None
        } else {
            n.child("ANGLEUNIT").or_else(|| {
                n.child("UNIT").filter(|u| u.name().is_some_and(|s| s.eq_ignore_ascii_case("degree") || s.eq_ignore_ascii_case("grad")))
            })
        }
    } else {
        n.child("UNIT")
    };
    let own = p.child("ANGLEUNIT").or_else(|| p.child("UNIT"));
    if d == Dialect::Wkt2 && own.is_some_and(|u| u.num(1).is_none()) {
        return None;
    }
    let size = own.or(crs_unit).and_then(|u| unit(u, UnitKind::Angle)).map_or(1.0, |u| u.degrees());
    let longitude = wkt1::prime_meridian_degrees(p.name().unwrap_or(""), p.num(1).unwrap_or(0.0), size);
    Some(PrimeMeridian { ident: ident(p), longitude })
}

/// The axis directions PROJ knows, as it spells them (ISO 19111, `AxisDirection`).
const DIRECTIONS: [&str; 41] = [
    "north",
    "northNorthEast",
    "northEast",
    "eastNorthEast",
    "east",
    "eastSouthEast",
    "southEast",
    "southSouthEast",
    "south",
    "southSouthWest",
    "southWest",
    "westSouthWest",
    "west",
    "westNorthWest",
    "northWest",
    "northNorthWest",
    "up",
    "down",
    "geocentricX",
    "geocentricY",
    "geocentricZ",
    "columnPositive",
    "columnNegative",
    "rowPositive",
    "rowNegative",
    "displayRight",
    "displayLeft",
    "displayUp",
    "displayDown",
    "forward",
    "aft",
    "port",
    "starboard",
    "clockwise",
    "counterClockwise",
    "towards",
    "awayFrom",
    "future",
    "past",
    "unspecified",
    "other",
];

/// A direction as PROJ spells it, whatever its case in the WKT; another lowercased.
fn direction(s: &str) -> String {
    DIRECTIONS.iter().find(|d| d.eq_ignore_ascii_case(s)).map_or_else(|| s.to_ascii_lowercase(), |d| d.to_string())
}

/// The coordinate system of a CRS node: its `AXIS` nodes (sorted by their WKT 2 `ORDER`, which
/// PROJ 9.9.0 refuses out of sequence and terrio reads), each in its own
/// unit, else the CRS's (`default_unit`, or WKT 2's unit after the axes); WKT 2's `CS` kind. Without
/// axes, `default` in `default_unit`, not explicit.
fn cs(n: &Node, d: Dialect, kind: CsKind, default: &[&str], default_unit: &Unit) -> Cs {
    let kind = match n.child("CS").and_then(|c| match c.args.first() {
        Some(Arg::Ident(k)) => Some(k.to_ascii_lowercase()),
        _ => None,
    }) {
        Some(k) => match k.as_str() {
            "ellipsoidal" => CsKind::Ellipsoidal,
            "cartesian" => CsKind::Cartesian,
            "spherical" => CsKind::Spherical,
            "vertical" => CsKind::Vertical,
            _ => CsKind::Other(k),
        },
        None => kind,
    };
    let unit_kind = default_unit.kind;
    // WKT 2's unit of the CS, after its axes.
    let cs_unit = if d == Dialect::Wkt2 { unit_node(n).and_then(|u| unit(u, unit_kind)) } else { None };
    let fallback = cs_unit.unwrap_or_else(|| default_unit.clone());
    let mut axes: Vec<(f64, Axis)> = n
        .children("AXIS")
        .enumerate()
        .map(|(i, a)| {
            let axis_unit = unit_node(a).and_then(|u| unit(u, unit_kind)).unwrap_or_else(|| fallback.clone());
            let meridian = a.child("MERIDIAN").and_then(|m| {
                let size =
                    m.child("ANGLEUNIT").or_else(|| m.child("UNIT")).and_then(|u| unit(u, UnitKind::Angle)).map_or(1.0, |u| u.degrees());
                Some(m.num(0)? * size)
            });
            let dir = match a.args.get(1) {
                Some(Arg::Ident(s) | Arg::Str(s)) => direction(s),
                _ => String::new(),
            };
            let order = a.child("ORDER").and_then(|o| o.num(0)).unwrap_or(i as f64 + 1.0);
            (order, Axis { name: a.name().unwrap_or("").to_string(), direction: dir, unit: Some(axis_unit), meridian })
        })
        .collect();
    if axes.is_empty() {
        let axes = default
            .iter()
            .map(|dir| Axis { name: String::new(), direction: dir.to_string(), unit: Some(fallback.clone()), meridian: None })
            .collect();
        return Cs { kind, axes, explicit: false };
    }
    axes.sort_by(|x, y| x.0.total_cmp(&y.0));
    Cs { kind, axes: axes.into_iter().map(|(_, a)| a).collect(), explicit: true }
}

/// The parameters of a conversion or transformation node, each with its own unit.
fn params(n: &Node) -> Vec<Param> {
    n.children("PARAMETER")
        .filter_map(|p| {
            let unit = unit_node(p).and_then(|u| unit(u, UnitKind::Unknown));
            Some(Param { ident: ident(p), value: p.num(1)?, unit })
        })
        .collect()
}

/// A projected CRS: its base CRS, its conversion (WKT 2's `CONVERSION`, WKT 1's `PROJECTION` and
/// `PARAMETER`s), and its coordinate system.
fn projected(n: &Node, d: Dialect) -> Option<Crs> {
    let base_node = n.geodetic_base()?;
    let base = geodetic(base_node, d)?;
    let angular = angular_unit(base_node, d);
    // Its length unit: WKT 1's first `UNIT`, as PROJ takes it; WKT 2's CS unit, after its axes,
    // else its first axis's.
    let linear = if d == Dialect::Wkt2 {
        n.child("LENGTHUNIT")
            .or_else(|| n.child("UNIT"))
            .or_else(|| n.children("AXIS").find_map(|a| a.child("LENGTHUNIT").or_else(|| a.child("UNIT"))))
    } else {
        n.child("UNIT")
    };
    // A WKT 2 unit without its size, which PROJ 9.9.0 and the WKT 2 reader refuse.
    if d == Dialect::Wkt2 && linear.is_some_and(|u| u.num(1).is_none()) {
        return None;
    }
    let linear = linear.and_then(|u| unit(u, UnitKind::Length)).unwrap_or_else(Unit::metre);
    // A WKT 2 parameter without its value, which PROJ 9.9.0 and the WKT 2 reader refuse.
    if d == Dialect::Wkt2 && n.child("CONVERSION").is_some_and(|c| c.children("PARAMETER").any(|p| p.num(1).is_none())) {
        return None;
    }
    let conversion = match n.child("CONVERSION") {
        Some(c) => Conversion {
            ident: ident(c),
            method: ident(c.child("METHOD").or_else(|| c.child("PROJECTION"))?),
            params: params(c),
            angular,
            linear: linear.clone(),
        },
        None => Conversion {
            ident: Ident::default(),
            method: ident(n.child("PROJECTION")?),
            params: params(n),
            angular,
            linear: linear.clone(),
        },
    };
    // What terrio's WKT 1 reader refuses: a Hotine Oblique Mercator (two point) parameter that
    // is not a finite number, which would otherwise be dropped for a zero default, and a polar
    // stereographic without the latitude it needs (`wkt1::polar_latitude_refused`).
    if n.is(kw::WKT1_PROJECTED)
        && let Some(projection) = n.child("PROJECTION")
        && let Some(method) = wkt1::wkt1_method(projection.name(), projection.epsg())
    {
        if method == "hotineobliquemercatortwopointnaturalorigin" && wkt1::hotine_two_point_refused(n) {
            return None;
        }
        if matches!(method.as_str(), "polarstereographic" | "polarstereographicvarianta" | "polarstereographicvariantb")
            && wkt1::polar_latitude_refused(n)
        {
            return None;
        }
    }
    // Without `AXIS`, east and north, but as PROJ 9.9.0 reads a WKT 1 Transverse Mercator (South
    // Orientated), west and south, and ESRI's Krovak, south and west (east and north for its East
    // North form, `X_Scale` -1 and `XY_Plane_Rotation` 90).
    let method = wkt1::norm(&conversion.method.name);
    let param = |name: &str| conversion.params.iter().find(|p| p.ident.name.eq_ignore_ascii_case(name)).map(|p| p.value);
    let east_north = param("X_Scale") == Some(-1.0) && param("XY_Plane_Rotation") == Some(90.0);
    let default = if method == "transversemercatorsouthorientated" {
        ["west", "south"]
    } else if d == Dialect::Wkt1Esri && method == "krovak" && !east_north {
        ["south", "west"]
    } else {
        ["east", "north"]
    };
    let cs = cs(n, d, CsKind::Cartesian, &default, &linear);
    // A 3D projected CRS's 2D base CRS is 3D, its ellipsoidal height up in metres whatever the
    // projected CRS's unit, as PROJ 9.9.0 reads it.
    let mut base = base;
    if let (3, Crs::Geodetic { cs: base_cs, .. }) = (cs.axes.len(), &mut base)
        && base_cs.axes.len() == 2
    {
        base_cs.axes.push(Axis { name: String::new(), direction: "up".into(), unit: Some(Unit::metre()), meridian: None });
    }
    Some(Crs::Projected { ident: ident(n), base: Box::new(base), conversion, cs })
}

/// A vertical CRS: WKT 1's `VERT_CS`, ESRI's `VERTCS` (down with its `Direction` -1, as PROJ
/// reads it), WKT 2's `VERTCRS`.
fn vertical(n: &Node, d: Dialect) -> Option<Crs> {
    // Its vertical datum, or WKT 2's datum ensemble (DVR90's), or an ESRI `VERTCS`'s geodetic
    // `DATUM` (an ellipsoidal height, read alone as a vertical CRS on that datum, as by PROJ).
    let datum = n
        .first_in(kw::VERTICAL_DATUM)
        .or_else(|| n.first_in(kw::GEODETIC_DATUM_ENSEMBLE))
        .or_else(|| if n.key.eq_ignore_ascii_case("VERTCS") { n.child("DATUM") } else { None })?;
    let ensemble = datum.is(kw::GEODETIC_DATUM_ENSEMBLE).then(|| Members {
        members: datum.children("MEMBER").map(ident).collect(),
        accuracy: datum.child("ENSEMBLEACCURACY").and_then(|a| a.num(0)),
    });
    let length = n
        .child("UNIT")
        .or_else(|| n.child("LENGTHUNIT"))
        .or_else(|| n.child("AXIS").and_then(|a| a.child("LENGTHUNIT").or_else(|| a.child("UNIT"))))
        .and_then(|u| unit(u, UnitKind::Length))
        .unwrap_or_else(Unit::metre);
    let down = esri_down(n);
    let cs = cs(n, d, CsKind::Vertical, &[if down { "down" } else { "up" }], &length);
    let grids = |x: &Node| {
        x.children("EXTENSION").find(|e| e.name().is_some_and(|s| s.eq_ignore_ascii_case("PROJ4_GRIDS"))).and_then(|e| {
            match e.args.get(1) {
                Some(Arg::Str(s)) => Some(s.clone()),
                _ => None,
            }
        })
    };
    let anchor_epoch = datum.child("ANCHOREPOCH").and_then(|e| e.num(0));
    let frame_epoch = n.child("DYNAMIC").and_then(|dy| dy.child("FRAMEEPOCH")).and_then(|e| e.num(0));
    let grids = grids(datum).or_else(|| grids(n));
    Some(Crs::Vertical { ident: ident(n), datum: ident(datum), ensemble, anchor_epoch, frame_epoch, cs, grids })
}

/// A WKT 2 `BOUNDCRS`: its source and target CRSs and its abridged transformation.
fn bound(n: &Node, d: Dialect) -> Option<Crs> {
    let inner = |key: &str| {
        let c = crate::wkt2::first_node(n.child(key)?)?;
        // A WKT 1 CRS inside a WKT 2 `BOUNDCRS` is read in its own dialect.
        let d = if WKT1_ROOTS.iter().any(|k| c.key.eq_ignore_ascii_case(k)) { dialect(&c.to_wkt(), c) } else { d };
        crs(c, d)
    };
    let (source, target) = (inner("SOURCECRS")?, inner("TARGETCRS")?);
    let t = n.child("ABRIDGEDTRANSFORMATION")?;
    let transformation = Transformation {
        ident: ident(t),
        method: t.child("METHOD").map(ident).unwrap_or_default(),
        params: params(t),
        files: t
            .children("PARAMETERFILE")
            .filter_map(|p| match p.args.get(1) {
                Some(Arg::Str(s)) => Some((ident(p), s.clone())),
                _ => None,
            })
            .collect(),
        target: Box::new(target),
    };
    Some(Crs::Bound { source: Box::new(source), transformation })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geodetic_parts(m: &Model) -> (&Ident, &Datum, &Cs) {
        match &m.crs {
            Crs::Geodetic { ident, datum, cs, .. } => (ident, datum, cs),
            other => panic!("not geodetic: {other:?}"),
        }
    }

    /// An ellipsoid in Clarke's feet (PROJ's WKT 2 of EPSG:4157) is in metres, and NTF (Paris)'s
    /// prime meridian in grads is in degrees: PROJ 9.9.0 gives 6378293.645 m and 2.5969213 grads.
    #[test]
    fn figures_in_metres_and_degrees() {
        let m = read(r#"GEOGCRS["Mount Dillon",DATUM["Mount Dillon",ELLIPSOID["Clarke 1858",20926348,294.260676369261,LENGTHUNIT["Clarke's foot",0.3047972654]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]],CS[ellipsoidal,2],AXIS["geodetic latitude (Lat)",north,ORDER[1],ANGLEUNIT["degree",0.0174532925199433]],AXIS["geodetic longitude (Lon)",east,ORDER[2],ANGLEUNIT["degree",0.0174532925199433]],ID["EPSG",4157]]"#).unwrap();
        let (ident, datum, cs) = geodetic_parts(&m);
        assert_eq!(ident.ids, vec![Id { authority: "EPSG".into(), code: "4157".into(), version: None }]);
        assert!((datum.ellipsoid().a - 6378293.645).abs() < 1e-3, "{}", datum.ellipsoid().a);
        assert_eq!(cs.axes.iter().map(|a| a.direction.as_str()).collect::<Vec<_>>(), ["north", "east"]);
        assert_eq!(m.dialect, Dialect::Wkt2);
        let esri = read(r#"GEOGCS["GCS_NTF_Paris",DATUM["Nouvelle_Triangulation_Francaise_(Paris)",SPHEROID["Clarke_1880_IGN",6378249.2,293.466021293627]],PRIMEM["Paris",2.33722917],UNIT["Grad",0.0157079632679489]]"#).unwrap();
        let (_, datum, cs) = geodetic_parts(&esri);
        assert!((datum.prime_meridian().longitude - 2.5969213 * 0.9).abs() < 1e-12);
        assert_eq!((esri.dialect, cs.explicit), (Dialect::Wkt1Esri, false));
        assert_eq!(cs.axes.iter().map(|a| a.direction.as_str()).collect::<Vec<_>>(), ["east", "north"]);
    }

    /// terrio's keywords PROJ refuses (`DATUMENSEMBLE`, `BASEGEOGRAPHICCRS`) and `(`/`)` brackets
    /// are read, as terrio reads them (backward compatibility); a datum with an `ENSEMBLE` by the
    /// first of the two.
    #[test]
    fn terrio_extensions_and_brackets() {
        let m = read(r#"PROJCRS("x",BASEGEOGRAPHICCRS("WGS 84",DATUMENSEMBLE("World Geodetic System 1984 ensemble",MEMBER("World Geodetic System 1984 (G2296)"),ELLIPSOID("WGS 84",6378137,298.257223563)),PRIMEM("Greenwich",0)),CONVERSION("UTM zone 31N",METHOD("Transverse Mercator",ID("EPSG",9807)),PARAMETER("Longitude of natural origin",3,ANGLEUNIT("degree",0.0174532925199433),ID("EPSG",8802))),CS(Cartesian,2),AXIS("E",east),AXIS("N",north),LENGTHUNIT("metre",1))"#).unwrap();
        let Crs::Projected { base, conversion, cs, .. } = &m.crs else { panic!("{m:?}") };
        let base = Model { crs: (**base).clone(), dialect: Dialect::Wkt2, extensions: Vec::new(), height_added: false };
        let (_, datum, _) = geodetic_parts(&base);
        assert!(matches!(datum, Datum::Ensemble { members, .. } if members.len() == 1));
        assert_eq!(conversion.method.ids[0].code, "9807");
        assert_eq!(conversion.params[0].unit.as_ref().map(|u| u.kind), Some(UnitKind::Angle));
        assert_eq!(cs.axes[0].unit, Some(Unit::metre()));
        let both = read(r#"GEOGCRS["x",DATUM["first",ELLIPSOID["GRS 1980",6378137,298.257222101]],ENSEMBLE["second",MEMBER["m"],ELLIPSOID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],CS[ellipsoidal,2],AXIS["lat",north],AXIS["lon",east],ANGLEUNIT["degree",0.0174532925199433]]"#).unwrap();
        assert_eq!(geodetic_parts(&both).1.ident().name, "first");
    }

    /// `TOWGS84` keeps its 3 or 7 values; a WKT 2 `BOUNDCRS` its target and transformation, over
    /// a WKT 1 source too; a root's extension is kept.
    #[test]
    fn shifts_and_extensions() {
        for (t, n) in [("TOWGS84[375,-111,431]", 3), ("TOWGS84[446.448,-125.157,542.06,0.15,0.247,0.842,-20.489]", 7)] {
            let w = format!(
                r#"GEOGCS["x",DATUM["y",SPHEROID["Airy 1830",6377563.396,299.3249646],{t}],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433],AUTHORITY["EPSG","4277"]]"#
            );
            let m = read(&w).unwrap();
            assert!(matches!(geodetic_parts(&m).1, Datum::Frame { towgs84: Some(v), .. } if v.len() == n));
            assert_eq!(m.dialect, Dialect::Wkt1Gdal);
        }
        let b = read(r#"BOUNDCRS[SOURCECRS[GEOGCS["OSGB 1936",DATUM["OSGB_1936",SPHEROID["Airy 1830",6377563.396,299.3249646]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]],TARGETCRS[GEOGCRS["WGS 84",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563]],CS[ellipsoidal,2],AXIS["lat",north],AXIS["lon",east],ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",4326]]],ABRIDGEDTRANSFORMATION["OSGB36 to WGS 84",METHOD["Position Vector transformation (geog2D domain)",ID["EPSG",9606]],PARAMETER["X-axis translation",446.448,LENGTHUNIT["metre",1],ID["EPSG",8605]]]]"#).unwrap();
        let Crs::Bound { source, transformation } = &b.crs else { panic!("{b:?}") };
        assert!(matches!(**source, Crs::Geodetic { ref cs, .. } if cs.axes[0].direction == "east"));
        assert!(matches!(*transformation.target, Crs::Geodetic { ref ident, .. } if ident.ids[0].code == "4326"));
        assert_eq!((transformation.method.ids[0].code.as_str(), transformation.params.len()), ("9606", 1));
        let e = read(r#"PROJCS["WGS 84 / Pseudo-Mercator",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Mercator_1SP"],PARAMETER["central_meridian",0],UNIT["metre",1],EXTENSION["PROJ4","+proj=merc +a=6378137 +b=6378137"],AUTHORITY["EPSG","3857"]]"#).unwrap();
        assert_eq!(e.extensions, vec![("PROJ4".to_string(), "+proj=merc +a=6378137 +b=6378137".to_string())]);
    }

    /// ESRI's `VERTCS` with `Direction` -1 is down; ESRI's `GEOGCS[...],VERTCS[...]` sequence is
    /// a compound CRS named after its parts, as PROJ 9.9.0 reads both; a vertical datum's grids win
    /// over the root's.
    #[test]
    fn vertical_and_compound() {
        let v =
            r#"VERTCS["MSL_Depth",VDATUM["Mean_Sea_Level"],PARAMETER["Vertical_Shift",0.0],PARAMETER["Direction",-1.0],UNIT["Meter",1.0]]"#;
        let m = read(v).unwrap();
        assert!(matches!(&m.crs, Crs::Vertical { cs, .. } if cs.axes[0].direction == "down" && !cs.explicit));
        let g = r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;
        let c = read(&format!("{g},{v}")).unwrap();
        assert!(matches!(&c.crs, Crs::Compound { ident, parts } if ident.name == "GCS_WGS_1984 + MSL_Depth" && parts.len() == 2));
        let grids = read(r#"VERT_CS["h",VERT_DATUM["d",2005,EXTENSION["PROJ4_GRIDS","datum.gtx"]],UNIT["metre",1],EXTENSION["PROJ4_GRIDS","root.gtx"],AXIS["Up",UP]]"#).unwrap();
        assert!(matches!(&grids.crs, Crs::Vertical { grids: Some(g), cs, .. } if g == "datum.gtx" && cs.axes[0].direction == "up"));
    }

    /// A dynamic CRS's frame epoch and its datum's anchor epoch; an axis along a meridian.
    #[test]
    fn epochs_and_meridians() {
        let m = read(r#"GEOGCRS["WGS 84 (G1762)",DYNAMIC[FRAMEEPOCH[2005]],DATUM["World Geodetic System 1984 (G1762)",ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]],ANCHOREPOCH[1984]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]],CS[ellipsoidal,2],AXIS["lat",north,ORDER[1],ANGLEUNIT["degree",0.0174532925199433]],AXIS["lon",east,ORDER[2],ANGLEUNIT["degree",0.0174532925199433]]]"#).unwrap();
        assert!(
            matches!(&m.crs, Crs::Geodetic { frame_epoch: Some(e), datum: Datum::Frame { anchor_epoch: Some(1984.0), .. }, .. } if *e == 2005.0)
        );
        let p = read(r#"PROJCRS["x",BASEGEOGCRS["WGS 84",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563]]],CONVERSION["Polar",METHOD["Polar Stereographic (variant A)",ID["EPSG",9810]]],CS[Cartesian,2],AXIS["(E)",south,MERIDIAN[90,ANGLEUNIT["degree",0.0174532925199433]],ORDER[2]],AXIS["(N)",south,MERIDIAN[180,ANGLEUNIT["degree",0.0174532925199433]],ORDER[1]],LENGTHUNIT["metre",1]]"#).unwrap();
        let Crs::Projected { cs, .. } = &p.crs else { panic!() };
        assert_eq!(cs.axes.iter().map(|a| a.meridian).collect::<Vec<_>>(), [Some(180.0), Some(90.0)]);
    }

    fn directions(cs: &Cs) -> Vec<(&str, f64)> {
        cs.axes.iter().map(|a| (a.direction.as_str(), a.unit.as_ref().map_or(f64::NAN, |u| u.factor))).collect()
    }

    /// Heights as PROJ 9.9.0 (pyproj 3.8.0) reads them:
    /// - a 3D projected CRS's 2D base CRS is 3D, up in metres, whatever the projected CRS's unit and
    ///   whether its axes are explicit;
    /// - ESRI's `VERTCS` on a geodetic `DATUM` after a `GEOGCS` is a 3D geographic CRS, its height
    ///   in the `VERTCS`'s unit and `Direction` (down, in feet), and alone a vertical CRS on that
    ///   datum (up).
    #[test]
    fn heights_as_proj_reads_them() {
        let g = r#"GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#;
        let tail = r#"PROJECTION["Transverse_Mercator"],PARAMETER["central_meridian",0],UNIT["foot",0.3048],AXIS["Easting",EAST],AXIS["Northing",NORTH],AXIS["h",UP]]"#;
        let explicit = g.replace("]]", r#"],AXIS["Lat",NORTH],AXIS["Lon",EAST]]"#);
        for base in [g.to_string(), explicit] {
            let m = read(&format!(r#"PROJCS["x",{base},{tail}"#)).unwrap();
            let Crs::Projected { base, cs, .. } = &m.crs else { panic!("{m:?}") };
            assert_eq!(directions(cs)[2], ("up", 0.3048));
            let Crs::Geodetic { cs, .. } = base.as_ref() else { panic!() };
            assert_eq!(directions(cs)[2], ("up", 1.0));
        }
        let e = r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;
        let v = r#"VERTCS["WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PARAMETER["Vertical_Shift",0.0],PARAMETER["Direction",-1.0],UNIT["Foot",0.3048]]"#;
        let m = read(&format!("{e},{v}")).unwrap();
        let Crs::Geodetic { cs, .. } = &m.crs else { panic!("{m:?}") };
        assert_eq!(directions(cs).iter().map(|d| d.0).collect::<Vec<_>>(), ["east", "north", "down"]);
        assert_eq!(directions(cs)[2].1, 0.3048);
        let alone = read(&v.replace("-1.0", "1.0")).unwrap();
        assert!(matches!(&alone.crs, Crs::Vertical { datum, cs, .. } if datum.name == "D_WGS_1984" && cs.axes[0].direction == "up"));
    }

    /// A WKT 2 Cartesian CS's bare `UNIT` is a length (PROJ 9.9.0: geocentric X, Y, Z in metres); a
    /// GDAL Krovak with its `AUTHORITY` keeps east and north (only ESRI's is south and west); a
    /// `COMPD_CS` with a component it cannot read is unread.
    #[test]
    fn units_dialects_and_whole_compounds() {
        let m = read(r#"GEODCRS["x",DATUM["WGS 84",ELLIPSOID["WGS 84",6378137,298.257223563]],CS[Cartesian,3],AXIS["X",geocentricX],AXIS["Y",geocentricY],AXIS["Z",geocentricZ],UNIT["metre",1]]"#).unwrap();
        let (_, _, cs) = geodetic_parts(&m);
        assert!(cs.axes.iter().all(|a| a.unit.as_ref().is_some_and(|u| u.kind == UnitKind::Length && u.factor == 1.0)), "{cs:?}");
        let k = r#"PROJCS["S-JTSK / Krovak",GEOGCS["S-JTSK",DATUM["S_JTSK",SPHEROID["Bessel 1841",6377397.155,299.1528128]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Krovak"],PARAMETER["latitude_of_center",49.5],UNIT["metre",1],AUTHORITY["EPSG","5513"]]"#;
        let m = read(k).unwrap();
        let Crs::Projected { cs, .. } = &m.crs else { panic!() };
        assert_eq!((m.dialect, cs.axes[0].direction.as_str()), (Dialect::Wkt1Gdal, "east"));
        assert!(read(r#"COMPD_CS["c",GEOGCS["g",PRIMEM["Greenwich",0]],VERT_CS["h",VERT_DATUM["d",2005],UNIT["metre",1]]]"#).is_none());
        // Without a method, as PROJ 9.9.0 refuses it ("Invalid projection").
        let g = r#"GEOGCS["g",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#;
        assert!(read(&format!(r#"PROJCS["p",{g},PARAMETER["false_easting",0],UNIT["metre",1]]"#)).is_none());
        assert!(read(r#"PROJCRS["p",BASEGEOGCRS["g",DATUM["WGS 84",ELLIPSOID["WGS 84",6378137,298.257223563]]],CONVERSION["c",PARAMETER["False easting",0]],CS[Cartesian,2],AXIS["E",east],AXIS["N",north],LENGTHUNIT["metre",1]]"#).is_none());
        // GDAL's `PROJ4_GRIDS` in any case, as PROJ 9.9.0 reads it (a bound CRS).
        let m = read(r#"VERT_CS["v",VERT_DATUM["d",2005,EXTENSION["proj4_grids","g.gtx"]],UNIT["metre",1]]"#).unwrap();
        assert!(matches!(&m.crs, Crs::Vertical { grids: Some(g), .. } if g == "g.gtx"));
    }

    /// WKT 2's `PRIMEMERIDIAN` (PROJ 9.9.0: longitude 2), a compound CRS with a `TIMECRS` (which
    /// the model does not read), the extensions of ESRI's sequence's horizontal CRS, a vertical
    /// ensemble's members and accuracy, and a bound CRS's `PARAMETERFILE`.
    #[test]
    fn meridians_ensembles_files_and_unread_components() {
        let g = r#"GEOGCRS["x",DATUM["WGS 84",ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]]],%PM%CS[ellipsoidal,2],AXIS["lat",north,ORDER[1],ANGLEUNIT["degree",0.0174532925199433]],AXIS["lon",east,ORDER[2],ANGLEUNIT["degree",0.0174532925199433]]]"#;
        let m = read(&g.replace("%PM%", r#"PRIMEMERIDIAN["Local",2,ANGLEUNIT["degree",0.0174532925199433]],"#)).unwrap();
        let (_, datum, _) = geodetic_parts(&m);
        assert_eq!((datum.prime_meridian().ident.name.as_str(), datum.prime_meridian().longitude), ("Local", 2.0));
        let g = g.replace("%PM%", "");
        let time = r#"TIMECRS["t",TDATUM["Gregorian",TIMEORIGIN[0000-01-01]],CS[TemporalDateTime,1],AXIS["time (T)",future]]"#;
        assert!(read(&format!(r#"COMPOUNDCRS["c",{g},{time}]"#)).is_none());
        let e = r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433],EXTENSION["vendor","value"]]"#;
        let v =
            r#"VERTCS["EGM96_Geoid",VDATUM["EGM96_Geoid"],PARAMETER["Vertical_Shift",0.0],PARAMETER["Direction",1.0],UNIT["Meter",1.0]]"#;
        let ellipsoidal = r#"VERTCS["WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PARAMETER["Vertical_Shift",0.0],PARAMETER["Direction",1.0],UNIT["Meter",1.0]]"#;
        for text in [e.to_string(), format!("{e},{v}"), format!("{e},{ellipsoidal}")] {
            assert_eq!(read(&text).unwrap().extensions, [("vendor".to_string(), "value".to_string())], "{text}");
        }
        let m = read(
            r#"VERTCRS["v",ENSEMBLE["e",MEMBER["a"],MEMBER["b"],ENSEMBLEACCURACY[0.1]],CS[vertical,1],AXIS["H",up,LENGTHUNIT["metre",1]]]"#,
        )
        .unwrap();
        let Crs::Vertical { ensemble: Some(e), .. } = &m.crs else { panic!("{m:?}") };
        assert_eq!((e.members.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), e.accuracy), (vec!["a", "b"], Some(0.1)));
        let ntv2 = format!(
            r#"BOUNDCRS[SOURCECRS[{g}],TARGETCRS[{g}],ABRIDGEDTRANSFORMATION["t",METHOD["NTv2",ID["EPSG",9615]],PARAMETERFILE["Latitude and longitude difference file","ntv2_0.gsb",ID["EPSG",8656]]]]"#
        );
        let Crs::Bound { transformation: t, .. } = read(&ntv2).unwrap().crs else { panic!() };
        assert_eq!((t.files[0].0.name.as_str(), t.files[0].1.as_str()), ("Latitude and longitude difference file", "ntv2_0.gsb"));
    }
}
