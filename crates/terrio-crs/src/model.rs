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

#[cfg(test)]
mod oracle_tests;

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
    pub target: Box<Crs>,
}

/// A CRS.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Crs {
    /// A geographic (ellipsoidal) or geocentric (Cartesian, spherical) CRS.
    Geodetic {
        ident: Ident,
        datum: Datum,
        cs: Cs,
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
        ensemble: bool,
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
}

/// The model of a WKT definition (WKT 1 GDAL, ESRI or WKT 2), or `None` when it is not one this
/// model reads: text that is not WKT (see [`wkt1::parse`]), or a CRS without what it needs (a
/// geodetic CRS without its datum or ellipsoid, a projected CRS without its base CRS).
pub(crate) fn read(text: &str) -> Option<Model> {
    // ESRI's compound CRS, `GEOGCS[...],VERTCS[...]`, as PROJ reads it: named after its parts.
    if let Some((h, v)) = wkt1::esri_compound(text) {
        // A `VERTCS` on a geodetic `DATUM` is an ellipsoidal height: PROJ 9.9.0 reads the two as
        // a 3D CRS (ESRI's LUREF / Luxembourg TM (3D), EPSG:9895), its base CRS 3D too.
        if v.child("DATUM").is_some() {
            let length = v.child("UNIT").and_then(|u| unit(u, UnitKind::Length)).unwrap_or_else(Unit::metre);
            let mut crs = crs(&h, Dialect::Wkt1Esri)?;
            let up = |cs: &mut Cs| {
                cs.axes.push(Axis { name: String::new(), direction: "up".into(), unit: Some(length.clone()), meridian: None })
            };
            match &mut crs {
                Crs::Projected { base, cs, .. } => {
                    up(cs);
                    if let Crs::Geodetic { cs, .. } = base.as_mut() {
                        up(cs);
                    }
                }
                Crs::Geodetic { cs, .. } => up(cs),
                _ => {}
            }
            return Some(Model { crs, dialect: Dialect::Wkt1Esri, extensions: Vec::new() });
        }
        let parts = vec![crs(&h, Dialect::Wkt1Esri)?, crs(&v, Dialect::Wkt1Esri)?];
        let name = format!("{} + {}", h.name().unwrap_or(""), v.name().unwrap_or(""));
        let crs = Crs::Compound { ident: Ident { name, ids: Vec::new() }, parts };
        return Some(Model { crs, dialect: Dialect::Wkt1Esri, extensions: Vec::new() });
    }
    let root = wkt1::parse(text)?;
    let d = dialect(text, &root);
    let extensions = root
        .children("EXTENSION")
        .filter_map(|e| match e.args.get(1) {
            Some(Arg::Str(s)) => Some((e.name()?.to_string(), s.clone())),
            _ => None,
        })
        .collect();
    Some(Model { crs: crs(&root, d)?, dialect: d, extensions })
}

/// The WKT 1 root keywords.
const WKT1_ROOTS: [&str; 7] = ["GEOGCS", "PROJCS", "GEOCCS", "VERT_CS", "VERTCS", "COMPD_CS", "LOCAL_CS"];

/// The dialect of a WKT, as PROJ 9.9.0 guesses it (`WKTParser::guessDialect`): WKT 1 by its root
/// keyword; ESRI's for a `VERTCS`, or a WKT 1 with a `GEOGCS["GCS_` or, but for a `LOCAL_CS`,
/// without any `AXIS[` or `AUTHORITY[`, unless it has a `rectified_grid_angle` parameter (GDAL's
/// Hotine Oblique Mercator, which ESRI's has not).
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
        let parts = nodes(n).filter_map(|c| crs(c, d)).collect();
        Some(Crs::Compound { ident: ident(n), parts })
    } else if n.key.eq_ignore_ascii_case("BOUNDCRS") {
        bound(n, d)
    } else if n.is(kw::ENGINEERING_CRS) {
        Some(Crs::Other { ident: ident(n), key: n.key.to_ascii_uppercase() })
    } else {
        None
    }
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
        .or_else(|| n.child("PRIMEM").and_then(|p| p.child("ANGLEUNIT").or_else(|| p.child("UNIT"))));
    found.and_then(|u| unit(u, UnitKind::Angle)).unwrap_or_else(Unit::degree)
}

/// A geodetic CRS: a geographic or geocentric CRS, or the base CRS of a projected CRS.
fn geodetic(n: &Node, d: Dialect) -> Option<Crs> {
    let geocentric = n.is(kw::WKT1_GEOCENTRIC);
    let datum_node = n.datum()?;
    let ell = datum_node.first_in(kw::ELLIPSOID)?;
    let (a, rf) = ell.ellipsoid()?;
    let ellipsoid = Ellipsoid { ident: ident(ell), a, rf };
    // A WKT 1 `GEOCCS`'s `UNIT` is its length unit; its prime meridian is in degrees.
    let ang = if geocentric { Unit::degree() } else { angular_unit(n, d) };
    let prime_meridian = prime_meridian(n, d, geocentric);
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
        let mut cs = cs(n, d, CsKind::Ellipsoidal, &default, &ang);
        // ESRI's 3D geographic CRS: its `LINUNIT` is its ellipsoidal height's, as PROJ reads it.
        if !cs.explicit
            && let Some(length) = n.child("LINUNIT").and_then(|u| unit(u, UnitKind::Length))
        {
            cs.axes.push(Axis { name: String::new(), direction: "up".into(), unit: Some(length), meridian: None });
        }
        cs
    };
    Some(Crs::Geodetic { ident: ident(n), datum, cs, frame_epoch })
}

/// The prime meridian of a geodetic CRS node, as `wkt2::Geodetic::read` reads it: in its own unit,
/// else the CRS's (its `ANGLEUNIT`, or a `UNIT` named `degree` or `grad` in WKT 2; WKT 1's
/// `GEOGCS` unit), else degrees, with PROJ's corrections (`wkt1::prime_meridian_degrees`).
/// Greenwich without one.
fn prime_meridian(n: &Node, d: Dialect, geocentric: bool) -> PrimeMeridian {
    let Some(p) = n.child("PRIMEM") else {
        return PrimeMeridian { ident: Ident { name: "Greenwich".into(), ids: Vec::new() }, longitude: 0.0 };
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
    let size = own.or(crs_unit).and_then(|u| unit(u, UnitKind::Angle)).map_or(1.0, |u| u.degrees());
    let longitude = wkt1::prime_meridian_degrees(p.name().unwrap_or(""), p.num(1).unwrap_or(0.0), size);
    PrimeMeridian { ident: ident(p), longitude }
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

/// The coordinate system of a CRS node: its `AXIS` nodes (in their `ORDER`, WKT 2), each in its own
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
    let linear = linear.and_then(|u| unit(u, UnitKind::Length)).unwrap_or_else(Unit::metre);
    let conversion = match n.child("CONVERSION") {
        Some(c) => Conversion {
            ident: ident(c),
            method: c.child("METHOD").or_else(|| c.child("PROJECTION")).map(ident).unwrap_or_default(),
            params: params(c),
            angular,
            linear: linear.clone(),
        },
        None => Conversion {
            ident: Ident::default(),
            method: n.child("PROJECTION").map(ident).unwrap_or_default(),
            params: params(n),
            angular,
            linear: linear.clone(),
        },
    };
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
    // A 3D projected CRS's base CRS without its own axes is 3D, its height in the projected CRS's
    // height unit, as PROJ 9.9.0 reads it.
    let mut base = base;
    if let (Some(h), Crs::Geodetic { cs: base_cs, .. }) = (cs.axes.get(2), &mut base)
        && !base_cs.explicit
        && base_cs.axes.len() == 2
    {
        base_cs.axes.push(Axis { name: String::new(), direction: "up".into(), unit: h.unit.clone(), meridian: None });
    }
    Some(Crs::Projected { ident: ident(n), base: Box::new(base), conversion, cs })
}

/// A vertical CRS: WKT 1's `VERT_CS`, ESRI's `VERTCS` (down with its `Direction` -1, as PROJ
/// reads it), WKT 2's `VERTCRS`.
fn vertical(n: &Node, d: Dialect) -> Option<Crs> {
    // Its vertical datum, or WKT 2's datum ensemble (DVR90's).
    let datum = n.first_in(kw::VERTICAL_DATUM).or_else(|| n.first_in(kw::GEODETIC_DATUM_ENSEMBLE))?;
    let ensemble = datum.is(kw::GEODETIC_DATUM_ENSEMBLE);
    let length = n
        .child("UNIT")
        .or_else(|| n.child("LENGTHUNIT"))
        .or_else(|| n.child("AXIS").and_then(|a| a.child("LENGTHUNIT").or_else(|| a.child("UNIT"))))
        .and_then(|u| unit(u, UnitKind::Length))
        .unwrap_or_else(Unit::metre);
    let down = n.children("PARAMETER").any(|p| p.name().is_some_and(|s| s.eq_ignore_ascii_case("Direction")) && p.num(1) == Some(-1.0));
    let mut cs = cs(n, d, CsKind::Vertical, &[if down { "down" } else { "up" }], &length);
    if d != Dialect::Wkt2 {
        cs.kind = CsKind::Vertical;
    }
    let grids = |x: &Node| {
        x.children("EXTENSION").find(|e| e.name() == Some("PROJ4_GRIDS")).and_then(|e| match e.args.get(1) {
            Some(Arg::Str(s)) => Some(s.clone()),
            _ => None,
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
        // A WKT 1 CRS inside a WKT 2 `BOUNDCRS` is read as WKT 1.
        let d = if WKT1_ROOTS.iter().any(|k| c.key.eq_ignore_ascii_case(k)) { Dialect::Wkt1Gdal } else { d };
        crs(c, d)
    };
    let (source, target) = (inner("SOURCECRS")?, inner("TARGETCRS")?);
    let t = n.child("ABRIDGEDTRANSFORMATION")?;
    let transformation = Transformation {
        ident: ident(t),
        method: t.child("METHOD").map(ident).unwrap_or_default(),
        params: params(t),
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
        let base = Model { crs: (**base).clone(), dialect: Dialect::Wkt2, extensions: Vec::new() };
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
}
