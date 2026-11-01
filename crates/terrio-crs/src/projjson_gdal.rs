// Written to match the behavior of:
//   GDAL v3.12.1:
//     ogr/ogrspatialreference.cpp
//   PROJ 9.7.1:
//     src/iso19111/io.cpp
// No code is translated from these sources.

//! PROJJSON (<https://proj.org/specifications/projjson.html>) of a CRS as GDAL writes it
//! (`OSRExportToPROJJSON`, PROJ 9.7.1's `proj_as_projjson` in GDAL 3.12.1), without PROJ.
//!
//! GDAL builds a CRS given by an EPSG code from PROJ's database, and any other CRS (a dataset's,
//! one read from WKT) from its WKT. The latter is PROJ's reading of the WKT 1 (or WKT 2) terrio
//! carries for the CRS, which is GDAL's own WKT for an EPSG code; the former adds what the database
//! holds beyond that WKT (`projjson_db.tsv.zst`, generated from GDAL's `proj.db` by
//! `scripts/gen_projjson_table.py`): datum ensembles and their members, the coordinate systems'
//! axis names and abbreviations, conversion names, scopes, areas and bounding boxes.
//!
//! A CRS outside what this writer models (other projection methods, prime meridians other than
//! Greenwich, PROJ strings that WKT 1 cannot describe, compound, vertical and engineering CRSs)
//! is an error, never another encoding.

use super::wkt1::{self, Arg, Node, norm};
use super::{SpatialRef, epsg};
use crate::{Error, Result};
use serde_json::{Map, Value, json};
use std::collections::HashMap;
use std::sync::OnceLock;

/// The schema PROJ 9.7.1 writes.
const SCHEMA: &str = "https://proj.org/schemas/v0.7/projjson.schema.json";

static DB: &[u8] = include_bytes!("projjson_db.tsv.zst");

/// An axis of an EPSG coordinate system.
struct DbAxis {
    name: String,
    abbreviation: String,
    direction: String,
    /// The meridian a polar axis runs along, in degrees.
    meridian: Option<f64>,
    unit: Value,
}

/// A usage of an EPSG CRS: its scope, area and bounding box (south, west, north, east).
struct Usage {
    scope: String,
    area: String,
    bbox: [f64; 4],
}

/// What PROJ's database holds for an EPSG CRS beyond its WKT.
struct DbCrs {
    conversion: String,
    axes: Vec<DbAxis>,
    usages: Vec<Usage>,
    /// Whether the conversion's parameters are in the units of GDAL's WKT 1 (degrees, unity,
    /// the axes' unit), so the WKT's values are the database's.
    units: bool,
}

/// An EPSG datum ensemble.
struct Ensemble {
    name: String,
    accuracy: String,
    members: Vec<(u32, String)>,
}

/// An EPSG ellipsoid in metres: semi-major axis, and inverse flattening or semi-minor axis.
type DbEllipsoid = (f64, Option<f64>, Option<f64>);

struct Db {
    crs: HashMap<u32, DbCrs>,
    ensembles: HashMap<u32, Ensemble>,
    /// EPSG ellipsoids, by name, as the database defines them.
    ellipsoids: HashMap<String, Vec<DbEllipsoid>>,
    /// Datum anchor and frame reference epochs, by datum code.
    epochs: HashMap<u32, (Option<f64>, Option<f64>)>,
}

fn db() -> &'static Db {
    static TABLE: OnceLock<Db> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut out = Db { crs: HashMap::new(), ensembles: HashMap::new(), ellipsoids: HashMap::new(), epochs: HashMap::new() };
        for line in epsg::decompress(DB).lines() {
            let f: Vec<&str> = line.split('\t').collect();
            match f.as_slice() {
                ["C", code, conversion, axes, units] => {
                    let (Ok(code), Some(axes)) = (code.parse(), axes.split(';').map(db_axis).collect::<Option<Vec<_>>>()) else { continue };
                    out.crs.insert(code, DbCrs { conversion: conversion.to_string(), axes, usages: Vec::new(), units: *units == "1" });
                }
                ["D", code, anchor, frame] => {
                    let Ok(code) = code.parse() else { continue };
                    out.epochs.insert(code, (anchor.parse().ok(), frame.parse().ok()));
                }
                ["U", code, scope, area, s, w, n, e] => {
                    let bbox = [s, w, n, e].map(|v| v.parse().unwrap_or(f64::NAN));
                    if let Ok(code) = code.parse::<u32>()
                        && let Some(crs) = out.crs.get_mut(&code)
                    {
                        crs.usages.push(Usage { scope: scope.to_string(), area: area.to_string(), bbox });
                    }
                }
                ["L", name, a, rf, b] => {
                    let Ok(a) = a.parse() else { continue };
                    out.ellipsoids.entry(name.to_string()).or_default().push((a, rf.parse().ok(), b.parse().ok()));
                }
                ["E", code, name, accuracy, members] => {
                    let Ok(code) = code.parse() else { continue };
                    let members =
                        members.split(';').filter_map(|m| m.split_once('=')).filter_map(|(c, n)| Some((c.parse().ok()?, n.to_string())));
                    out.ensembles
                        .insert(code, Ensemble { name: name.to_string(), accuracy: accuracy.to_string(), members: members.collect() });
                }
                _ => {}
            }
        }
        out
    })
}

/// An axis of the table: `name|abbreviation|orientation|unit code`, the orientation being
/// `east` or `North along 90°E`.
fn db_axis(s: &str) -> Option<DbAxis> {
    let mut f = s.split('|');
    let (name, abbreviation, orientation, unit) = (f.next()?, f.next()?, f.next()?, f.next()?);
    let (direction, meridian) = match orientation.split_once(" along ") {
        Some((d, m)) => {
            let deg: f64 = m.trim_end_matches(['E', 'W']).trim_end_matches('°').parse().ok()?;
            (d, Some(if m.ends_with('W') { -deg } else { deg }))
        }
        None => (orientation, None),
    };
    let unit = match unit {
        "9001" => json!("metre"),
        "9122" => json!("degree"),
        "9002" => json!({"type": "LinearUnit", "name": "foot", "conversion_factor": 0.3048}),
        "9003" => json!({"type": "LinearUnit", "name": "US survey foot", "conversion_factor": 0.304800609601219}),
        _ => return None,
    };
    Some(DbAxis { name: name.into(), abbreviation: abbreviation.into(), direction: direction.to_ascii_lowercase(), meridian, unit })
}

fn unsupported(what: &str) -> Error {
    Error::Crs(format!("terrio cannot write this CRS as PROJJSON without pyproj: {what}"))
}

/// A number as PROJ writes it in PROJJSON: 15 significant digits (rasterio 1.5.0's
/// `CRS.from_wkt` of an inverse flattening of 294.97869821391095 gives 294.978698213911), and
/// integers without a fraction.
fn num(v: f64) -> Value {
    let v = if v.is_finite() { format!("{v:.14e}").parse().unwrap_or(v) } else { v };
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 9.0e15 { json!(v as i64) } else { json!(v) }
}

fn id(authority: &str, code: u32) -> Value {
    json!({"authority": authority, "code": code})
}

impl SpatialRef {
    /// The CRS as PROJJSON, as GDAL (3.12.1, with PROJ 9.7.1) writes it: with `database`, for the
    /// CRS GDAL builds from the CRS's EPSG code (`CRS.from_epsg`), else for the one it reads from
    /// the CRS's WKT (a dataset's CRS, `CRS.from_wkt`). Fails for a CRS this writer does not model.
    /// (`to_projjson` writes terrio's own PROJJSON, for more kinds of CRS, with documented
    /// differences from PROJ's.)
    pub fn to_gdal_projjson(&self, database: bool) -> Result<Value> {
        let code = if database { self.epsg } else { None };
        if database && code.is_none() {
            return Err(unsupported("the database object of a CRS without EPSG code"));
        }
        let wkt = self.to_wkt().ok_or_else(|| unsupported("the CRS has no WKT"))?;
        let root = wkt1::parse(&wkt).ok_or_else(|| unsupported("its WKT cannot be read"))?;
        let mut crs = from_wkt(&root)?;
        if let Some(code) = code {
            from_database(&mut crs, code)?;
        }
        let mut out = Map::new();
        out.insert("$schema".into(), json!(SCHEMA));
        out.extend(crs);
        Ok(Value::Object(out))
    }
}

/// The CRS PROJ reads from WKT.
fn from_wkt(root: &Node) -> Result<Map<String, Value>> {
    match root.key.to_ascii_uppercase().as_str() {
        "GEOGCS" => geographic1(root, false),
        "PROJCS" => projected1(root),
        "GEOGCRS" | "GEOGRAPHICCRS" | "GEODCRS" | "GEODETICCRS" => geographic2(root),
        k => Err(unsupported(&format!("a {k} CRS"))),
    }
}

/// An ellipsoid: a sphere has a radius; others an inverse flattening or a semi-minor axis.
fn ellipsoid_json(name: &str, a: f64, rf: Option<f64>, b: Option<f64>) -> Map<String, Value> {
    let mut e = Map::new();
    e.insert("name".into(), json!(name));
    match (rf, b) {
        (Some(rf), _) => {
            e.insert("semi_major_axis".into(), num(a));
            e.insert("inverse_flattening".into(), num(rf));
        }
        (None, Some(b)) if b != a => {
            e.insert("semi_major_axis".into(), num(a));
            e.insert("semi_minor_axis".into(), num(b));
        }
        _ => {
            e.insert("radius".into(), num(a));
        }
    }
    e
}

/// The authority and code of a WKT 1 node's `AUTHORITY` (EPSG, ESRI...).
fn authority(n: &Node) -> Option<Value> {
    let a = n.child("AUTHORITY")?;
    let code: u32 = match a.args.get(1)? {
        Arg::Str(s) => s.trim().parse().ok()?,
        Arg::Num(v) => *v as u32,
        _ => return None,
    };
    Some(id(a.name()?, code))
}

/// A WKT 1 name as PROJ keeps it: without GDAL's ` (deprecated)` mark.
fn crs_name(n: &Node) -> String {
    let name = n.name().unwrap_or("unnamed");
    name.strip_suffix(" (deprecated)").unwrap_or(name).to_string()
}

/// The name PROJ gives a WKT 1 datum: EPSG's for GDAL's and ESRI's spellings (`WGS_1984`,
/// `D_North_American_1983`), the name itself otherwise.
fn datum_name(name: &str) -> String {
    match name {
        "WGS_1984" => return "World Geodetic System 1984".into(),
        "European_Terrestrial_Reference_System_1989" => return "European Terrestrial Reference System 1989".into(),
        _ => {}
    }
    if name.contains('_')
        && let [code] = epsg::datums_named(name).as_slice()
        && let Some(official) = epsg::datum_name(*code)
    {
        return official;
    }
    name.to_string()
}

/// A WKT 1 `UNIT` as PROJJSON: `degree` or `metre` by name, else an object.
fn unit1(u: Option<&Node>, kind: &str) -> Result<(Value, f64)> {
    let Some(u) = u else {
        return Ok(if kind == "AngularUnit" { (json!("degree"), std::f64::consts::PI / 180.0) } else { (json!("metre"), 1.0) });
    };
    let (name, factor) = (u.name().unwrap_or_default(), u.num(1).ok_or_else(|| unsupported("a UNIT without its size"))?);
    Ok(match (kind, name) {
        ("AngularUnit", "degree") | ("LinearUnit", "metre") => (json!(name), factor),
        _ => (json!({"type": kind, "name": name, "conversion_factor": num(factor)}), factor),
    })
}

fn axis(name: &str, abbreviation: &str, direction: &str, unit: &Value) -> Value {
    json!({"name": name, "abbreviation": abbreviation, "direction": direction, "unit": unit})
}

/// The axes of a WKT 1 node: `(name, direction)`.
fn axes1(n: &Node) -> Vec<(String, String)> {
    n.children("AXIS")
        .filter_map(|a| match a.args.get(1) {
            Some(Arg::Ident(d)) => Some((a.name().unwrap_or_default().to_string(), d.to_ascii_lowercase())),
            _ => None,
        })
        .collect()
}

/// The coordinate system of an EPSG geographic CRS, from the database.
fn db_cs(code: u32) -> Option<Value> {
    let crs = db().crs.get(&code)?;
    let axes: Vec<Value> = crs.axes.iter().map(|a| axis(&a.name, &a.abbreviation, &a.direction, &a.unit)).collect();
    Some(json!({"subtype": "ellipsoidal", "axis": axes}))
}

/// A WKT 1 `GEOGCS`. PROJ writes its identifier, and those of the objects inside it only when
/// neither it nor the projected CRS it is the base of (`outer_id`) has one.
fn geographic1(g: &Node, outer_id: bool) -> Result<Map<String, Value>> {
    let code = g.epsg();
    let inner = !outer_id && code.is_none();
    // GDAL extends WKT 1 with the PROJ string of what it cannot describe (a rotated pole).
    if g.children("EXTENSION").next().is_some() {
        return Err(unsupported("a WKT 1 extended with a PROJ string"));
    }
    let d = g.child("DATUM").ok_or_else(|| unsupported("a GEOGCS without DATUM"))?;
    if g.child("PRIMEM").and_then(|p| p.num(1)).is_some_and(|pm| pm != 0.0) {
        return Err(unsupported("a prime meridian other than Greenwich"));
    }
    let sph = d.child("SPHEROID").ok_or_else(|| unsupported("a DATUM without SPHEROID"))?;
    let (a, rf) = (sph.num(1).ok_or_else(|| unsupported("a SPHEROID without axis"))?, sph.num(2).unwrap_or(0.0));
    let mut ellipsoid = ellipsoid_json(sph.name().unwrap_or("unnamed"), a, (rf != 0.0).then_some(rf), None);
    if inner
        && d.epsg().is_none()
        && let Some(c) = sph.epsg()
    {
        ellipsoid.insert("id".into(), id("EPSG", c));
    }
    let mut datum = Map::new();
    datum.insert("type".into(), json!("GeodeticReferenceFrame"));
    datum.insert("name".into(), json!(datum_name(d.name().unwrap_or("unnamed"))));
    datum.insert("ellipsoid".into(), Value::Object(ellipsoid));
    if inner && let Some(c) = d.epsg() {
        datum.insert("id".into(), id("EPSG", c));
    }
    let (unit, _) = unit1(g.child("UNIT"), "AngularUnit")?;
    let axes = axes1(g);
    // A CRS with an EPSG code takes the database's axes when the WKT gives none or the same
    // directions, as PROJ reads WKT 1.
    let known = code.and_then(|c| db().crs.get(&c)).filter(|k| {
        unit == json!("degree") && k.axes.len() == 2 && (axes.is_empty() || axes.iter().zip(&k.axes).all(|((_, d), a)| *d == a.direction))
    });
    let cs = match (known, code) {
        (Some(_), Some(c)) => db_cs(c).unwrap_or(Value::Null),
        _ => {
            let named = |(n, d): &(String, String)| match n.to_ascii_lowercase().as_str() {
                "latitude" | "lat" => axis("Latitude", "lat", d, &unit),
                "longitude" | "lon" | "long" => axis("Longitude", "lon", d, &unit),
                _ => axis(n, "", d, &unit),
            };
            let list: Vec<Value> = if axes.is_empty() {
                vec![axis("Longitude", "lon", "east", &unit), axis("Latitude", "lat", "north", &unit)]
            } else {
                axes.iter().map(named).collect()
            };
            json!({"subtype": "ellipsoidal", "axis": list})
        }
    };
    let mut m = Map::new();
    m.insert("type".into(), json!("GeographicCRS"));
    m.insert("name".into(), json!(crs_name(g)));
    m.insert("datum".into(), Value::Object(datum));
    m.insert("coordinate_system".into(), cs);
    if let Some(i) = authority(g) {
        m.insert("id".into(), i);
    }
    Ok(m)
}

/// An EPSG parameter: name, code, and kind (`a`ngle, `l`ength or `s`cale).
type Param = (&'static str, u32, char);
const LAT_NAT: Param = ("Latitude of natural origin", 8801, 'a');
const LON_NAT: Param = ("Longitude of natural origin", 8802, 'a');
const K_NAT: Param = ("Scale factor at natural origin", 8805, 's');
const FE: Param = ("False easting", 8806, 'l');
const FN: Param = ("False northing", 8807, 'l');
const LAT_FO: Param = ("Latitude of false origin", 8821, 'a');
const LON_FO: Param = ("Longitude of false origin", 8822, 'a');
const SP1: Param = ("Latitude of 1st standard parallel", 8823, 'a');
const SP2: Param = ("Latitude of 2nd standard parallel", 8824, 'a');
const E_FO: Param = ("Easting at false origin", 8826, 'l');
const N_FO: Param = ("Northing at false origin", 8827, 'l');
const LAT_PC: Param = ("Latitude of projection centre", 8811, 'a');
const LON_PC: Param = ("Longitude of projection centre", 8812, 'a');
const AZIMUTH: Param = ("Azimuth at projection centre", 8813, 'a');
const GAMMA: Param = ("Angle from Rectified to Skew Grid", 8814, 'a');
const K_PC: Param = ("Scale factor at projection centre", 8815, 's');
const E_PC: Param = ("Easting at projection centre", 8816, 'l');
const N_PC: Param = ("Northing at projection centre", 8817, 'l');
const LAT_SP: Param = ("Latitude of standard parallel", 8832, 'a');
const LON_ORIGIN: Param = ("Longitude of origin", 8833, 'a');

/// GDAL's WKT 1 parameter names (normalized) for an EPSG parameter.
fn wkt1_names(code: u32) -> &'static [&'static str] {
    match code {
        8801 | 8821 | 8811 => &["latitudeoforigin", "latitudeofcenter"],
        8802 | 8822 | 8812 | 8833 => &["centralmeridian", "longitudeofcenter", "longitudeoforigin"],
        8805 | 8815 => &["scalefactor"],
        8806 | 8826 | 8816 => &["falseeasting"],
        8807 | 8827 | 8817 => &["falsenorthing"],
        8823 => &["standardparallel1"],
        8824 => &["standardparallel2"],
        8813 => &["azimuth"],
        8814 => &["rectifiedgridangle"],
        8832 => &["latitudeoforigin", "standardparallel1"],
        _ => &[],
    }
}

/// The EPSG method (name and code, 0 for PROJ's own) and parameters of a WKT 1 projection.
fn method1(name: &str, param: &dyn Fn(&[&str]) -> Option<f64>) -> Option<(&'static str, u32, &'static [Param])> {
    const NAT: &[Param] = &[LAT_NAT, LON_NAT, K_NAT, FE, FN];
    const FO: &[Param] = &[LAT_FO, LON_FO, SP1, SP2, E_FO, N_FO];
    const AZ: &[Param] = &[LAT_NAT, LON_NAT, FE, FN];
    const WORLD: &[Param] = &[LON_NAT, FE, FN];
    Some(match norm(name).as_str() {
        "transversemercator" => ("Transverse Mercator", 9807, NAT),
        "transversemercatorsouthorientated" => ("Transverse Mercator (South Orientated)", 9808, NAT),
        "mercator1sp" => ("Mercator (variant A)", 9804, NAT),
        "mercator2sp" => ("Mercator (variant B)", 9805, &[SP1, LON_NAT, FE, FN]),
        "lambertconformalconic1sp" => ("Lambert Conic Conformal (1SP)", 9801, NAT),
        "lambertconformalconic2sp" => ("Lambert Conic Conformal (2SP)", 9802, FO),
        "albersconicequalarea" => ("Albers Equal Area", 9822, FO),
        "lambertazimuthalequalarea" => ("Lambert Azimuthal Equal Area", 9820, AZ),
        "polarstereographic" if param(&["latitudeoforigin"]).is_some_and(|l| (l.abs() - 90.0).abs() < 1e-12) => {
            ("Polar Stereographic (variant A)", 9810, NAT)
        }
        "polarstereographic" => ("Polar Stereographic (variant B)", 9829, &[LAT_SP, LON_ORIGIN, FE, FN]),
        "obliquestereographic" => ("Oblique Stereographic", 9809, NAT),
        "hotineobliquemercatorazimuthcenter" => {
            ("Hotine Oblique Mercator (variant B)", 9815, &[LAT_PC, LON_PC, AZIMUTH, GAMMA, K_PC, E_PC, N_PC])
        }
        "hotineobliquemercator" => ("Hotine Oblique Mercator (variant A)", 9812, &[LAT_PC, LON_PC, AZIMUTH, GAMMA, K_PC, FE, FN]),
        "cassinisoldner" => ("Cassini-Soldner", 9806, AZ),
        "cylindricalequalarea" => ("Lambert Cylindrical Equal Area", 9835, &[SP1, LON_NAT, FE, FN]),
        "mollweide" => ("Mollweide", 0, WORLD),
        "sinusoidal" => ("Sinusoidal", 0, WORLD),
        "robinson" => ("Robinson", 0, WORLD),
        "equalearth" => ("Equal Earth", 1078, WORLD),
        _ => return None,
    })
}

/// A WKT 1 `PROJCS`.
fn projected1(p: &Node) -> Result<Map<String, Value>> {
    let inner = p.child("AUTHORITY").is_none();
    let g = p.child("GEOGCS").ok_or_else(|| unsupported("a PROJCS without GEOGCS"))?;
    let mut base = geographic1(g, !inner)?;
    let (angular, _) = unit1(g.child("UNIT"), "AngularUnit")?;
    let (linear, to_meter) = unit1(p.children("UNIT").last(), "LinearUnit")?;
    let params: Vec<(String, f64)> = p.children("PARAMETER").filter_map(|x| Some((norm(x.name()?), x.num(1)?))).collect();
    let param = |names: &[&str]| names.iter().find_map(|n| params.iter().find(|(k, _)| k == n).map(|(_, v)| *v));
    // A WKT 1 GDAL extends with a PROJ string: only Web Mercator's, as PROJ reads it.
    let extension = p.children("EXTENSION").find(|e| e.name().is_some_and(|k| k.eq_ignore_ascii_case("PROJ4")));
    let (method, mcode, list): (&str, u32, &[Param]) = match extension.and_then(|e| e.args.get(1)) {
        Some(Arg::Str(s)) if web_mercator(s) => {
            // PROJ builds the base CRS from the PROJ string's axes.
            let unit = json!("degree");
            base.insert(
                "coordinate_system".into(),
                json!({"subtype": "ellipsoidal", "axis": [axis("Latitude", "lat", "north", &unit), axis("Longitude", "lon", "east", &unit)]}),
            );
            ("Popular Visualisation Pseudo Mercator", 1024, &[LAT_NAT, LON_NAT, FE, FN])
        }
        Some(_) => return Err(unsupported("a WKT 1 extended with a PROJ string")),
        None => {
            let name = p.child("PROJECTION").and_then(Node::name).ok_or_else(|| unsupported("a PROJCS without PROJECTION"))?;
            method1(name, &param).ok_or_else(|| unsupported(&format!("the {name} projection")))?
        }
    };
    let mut values = Vec::new();
    let mut parameters = Vec::new();
    for &(pname, pcode, kind) in list {
        let v = param(wkt1_names(pcode)).unwrap_or(if kind == 's' { 1.0 } else { 0.0 });
        values.push(v);
        let unit = match kind {
            'a' => angular.clone(),
            's' => json!("unity"),
            _ => linear.clone(),
        };
        parameters.push(json!({"name": pname, "value": num(v), "unit": unit, "id": id("EPSG", pcode)}));
    }
    let mut m = json!({"name": method});
    if mcode != 0 {
        m["id"] = id("EPSG", mcode);
    }
    let mut conversion = json!({"name": "unnamed", "method": m, "parameters": parameters});
    // PROJ names a Transverse Mercator with UTM's parameters after its zone.
    if mcode == 9807 && to_meter == 1.0 && angular == json!("degree") {
        let [lat0, lon0, k, fe, fnorth] = [values[0], values[1], values[2], values[3], values[4]];
        let zone = (lon0 + 183.0) / 6.0;
        if lat0 == 0.0
            && k == 0.9996
            && fe == 500_000.0
            && (fnorth == 0.0 || fnorth == 10_000_000.0)
            && zone.fract() == 0.0
            && (1.0..=60.0).contains(&zone)
        {
            let north = fnorth == 0.0;
            conversion["name"] = json!(format!("UTM zone {}{}", zone as u32, if north { 'N' } else { 'S' }));
            if inner {
                conversion["id"] = id("EPSG", if north { 16000 } else { 17000 } + zone as u32);
            }
        }
    }
    let axes = axes1(p);
    let list: Vec<Value> = if axes.is_empty() {
        vec![axis("Easting", "E", "east", &linear), axis("Northing", "N", "north", &linear)]
    } else {
        axes.iter().map(|(n, d)| axis(n, "", d, &linear)).collect()
    };
    let mut out = Map::new();
    out.insert("type".into(), json!("ProjectedCRS"));
    out.insert("name".into(), json!(crs_name(p)));
    out.insert("base_crs".into(), Value::Object(base));
    out.insert("conversion".into(), conversion);
    out.insert("coordinate_system".into(), json!({"subtype": "Cartesian", "axis": list}));
    if let Some(i) = authority(p) {
        out.insert("id".into(), i);
    }
    Ok(out)
}

/// Whether a PROJ string is Web Mercator as GDAL writes it in EPSG:3857's WKT 1.
fn web_mercator(p: &str) -> bool {
    let t: Vec<&str> = p.split_whitespace().collect();
    ["+proj=merc", "+a=6378137", "+b=6378137", "+nadgrids=@null"].iter().all(|k| t.contains(k))
        && t.iter().all(|k| !k.starts_with("+lat_ts=") || *k == "+lat_ts=0")
        && t.iter().all(|k| !k.starts_with("+k=") || *k == "+k=1")
}

/// A WKT 2 geographic CRS (GDAL's WKT of EPSG's geographic 3D CRSs).
fn geographic2(g: &Node) -> Result<Map<String, Value>> {
    let ids = g.child("ID").is_none();
    let datum_node = g
        .child("ENSEMBLE")
        .or_else(|| g.child("DATUM"))
        .or_else(|| g.child("TRF"))
        .or_else(|| g.child("GEODETICDATUM"))
        .ok_or_else(|| unsupported("a WKT 2 CRS without datum"))?;
    if g.child("PRIMEM").and_then(|p| p.num(1)).is_some_and(|pm| pm != 0.0) {
        return Err(unsupported("a prime meridian other than Greenwich"));
    }
    let e = datum_node.child("ELLIPSOID").ok_or_else(|| unsupported("a datum without ELLIPSOID"))?;
    let (a, rf) = (e.num(1).ok_or_else(|| unsupported("an ELLIPSOID without axis"))?, e.num(2).unwrap_or(0.0));
    if e.child("LENGTHUNIT").and_then(|u| u.num(1)).is_some_and(|f| f != 1.0) {
        return Err(unsupported("an ellipsoid in another unit than metres"));
    }
    let mut ellipsoid = Map::new();
    ellipsoid.insert("name".into(), json!(e.name().unwrap_or("unnamed")));
    if rf == 0.0 {
        ellipsoid.insert("radius".into(), num(a));
    } else {
        ellipsoid.insert("semi_major_axis".into(), num(a));
        ellipsoid.insert("inverse_flattening".into(), num(rf));
    }
    let mut datum = Map::new();
    let ensemble = datum_node.key.eq_ignore_ascii_case("ENSEMBLE");
    if !ensemble {
        datum.insert("type".into(), json!("GeodeticReferenceFrame"));
    }
    // A dynamic datum's frame reference epoch, and a datum's anchor epoch.
    if !ensemble && let Some(f) = g.child("DYNAMIC").and_then(|d| d.child("FRAMEEPOCH")).and_then(|f| f.num(0)) {
        datum.insert("type".into(), json!("DynamicGeodeticReferenceFrame"));
        datum.insert("frame_reference_epoch".into(), num(f));
    }
    if let Some(a) = datum_node.child("ANCHOREPOCH").and_then(|a| a.num(0)) {
        datum.insert("anchor_epoch".into(), num(a));
    }
    datum.insert("name".into(), json!(datum_node.name().unwrap_or("unnamed")));
    if ensemble {
        let members: Vec<Value> = datum_node.children("MEMBER").map(|m| json!({"name": m.name().unwrap_or_default()})).collect();
        datum.insert("members".into(), Value::Array(members));
    }
    datum.insert("ellipsoid".into(), Value::Object(ellipsoid));
    if let Some(acc) = datum_node.child("ENSEMBLEACCURACY").and_then(|a| a.num(0)) {
        datum.insert("accuracy".into(), json!(format!("{acc:?}")));
    }
    if ids && let Some(c) = wkt2_id(datum_node) {
        datum.insert("id".into(), id("EPSG", c));
    }
    let cs = g.child("CS").ok_or_else(|| unsupported("a WKT 2 CRS without CS"))?;
    if !matches!(cs.args.first(), Some(Arg::Ident(t)) if t.eq_ignore_ascii_case("ellipsoidal")) {
        return Err(unsupported("a geodetic CRS that is not geographic"));
    }
    let mut axes = Vec::new();
    for a in g.children("AXIS") {
        let full = a.name().unwrap_or_default();
        let (name, abbreviation) = match full.rsplit_once(" (") {
            Some((n, ab)) => (n, ab.trim_end_matches(')')),
            None => (full, ""),
        };
        let mut name = name.to_string();
        if let Some(first) = name.get(..1) {
            name = first.to_ascii_uppercase() + &name[1..];
        }
        let direction = match a.args.get(1) {
            Some(Arg::Ident(d)) => d.to_ascii_lowercase(),
            _ => return Err(unsupported("an axis without direction")),
        };
        let unit = match (a.child("ANGLEUNIT"), a.child("LENGTHUNIT")) {
            (Some(u), _) if u.name() == Some("degree") => json!("degree"),
            (_, Some(u)) if u.name() == Some("metre") => json!("metre"),
            _ => return Err(unsupported("an axis unit other than degree or metre")),
        };
        axes.push(axis(&name, abbreviation, &direction, &unit));
    }
    let mut m = Map::new();
    m.insert("type".into(), json!("GeographicCRS"));
    m.insert("name".into(), json!(g.name().unwrap_or("unnamed")));
    m.insert(if ensemble { "datum_ensemble" } else { "datum" }.into(), Value::Object(datum));
    m.insert("coordinate_system".into(), json!({"subtype": "ellipsoidal", "axis": axes}));
    if let Some(u) = g.child("USAGE") {
        if let Some(s) = u.child("SCOPE").and_then(Node::name) {
            m.insert("scope".into(), json!(s));
        }
        if let Some(a) = u.child("AREA").and_then(Node::name) {
            m.insert("area".into(), json!(a));
        }
        if let Some(b) = u.child("BBOX") {
            let v: Vec<f64> = (0..4).filter_map(|i| b.num(i)).collect();
            if let [s, w, n, e] = v[..] {
                m.insert("bbox".into(), bbox([s, w, n, e]));
            }
        }
    }
    if let Some(c) = wkt2_id(g) {
        m.insert("id".into(), id("EPSG", c));
    }
    Ok(m)
}

/// The EPSG code of a WKT 2 node's `ID`.
fn wkt2_id(n: &Node) -> Option<u32> {
    let i = n.child("ID")?;
    if !i.name()?.eq_ignore_ascii_case("EPSG") {
        return None;
    }
    i.num(1).map(|v| v as u32)
}

fn bbox([s, w, n, e]: [f64; 4]) -> Value {
    json!({"south_latitude": num(s), "west_longitude": num(w), "north_latitude": num(n), "east_longitude": num(e)})
}

/// A datum ensemble of the database, around the ellipsoid of the CRS's WKT.
fn ensemble_of(code: u32, datum: &Map<String, Value>) -> Option<Value> {
    let e = db().ensembles.get(&code)?;
    let members: Vec<Value> = e.members.iter().map(|(c, n)| json!({"name": n, "id": id("EPSG", *c)})).collect();
    Some(
        json!({"name": e.name, "members": members, "ellipsoid": datum.get("ellipsoid")?.clone(), "accuracy": e.accuracy, "id": id("EPSG", code)}),
    )
}

/// The geographic CRS of `code` as PROJ's database gives it, from the CRS read from its WKT.
fn database_geographic(crs: &mut Map<String, Value>, code: u32) -> Result<()> {
    let datum = epsg::get(code).map(|e| e.datum).filter(|d| *d != 0);
    let key = if crs.contains_key("datum") { "datum" } else { "datum_ensemble" };
    let Some(Value::Object(d)) = crs.get_mut(key) else { return Err(unsupported("a CRS without datum")) };
    // The ellipsoid as the database defines it (Clarke 1866 by its semi-minor axis); those in
    // other units than metres are not in the table.
    let e = d.get("ellipsoid").and_then(Value::as_object).ok_or_else(|| unsupported("a datum without ellipsoid"))?;
    let name = e.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
    let a = e.get("semi_major_axis").or_else(|| e.get("radius")).and_then(Value::as_f64).unwrap_or(f64::NAN);
    let &(da, rf, b) = db()
        .ellipsoids
        .get(&name)
        .and_then(|l| l.iter().find(|(da, _, _)| (da - a).abs() < 1e-3))
        .ok_or_else(|| unsupported(&format!("the ellipsoid {name} as the database defines it")))?;
    d.insert("ellipsoid".into(), Value::Object(ellipsoid_json(&name, da, rf, b)));
    // A datum's anchor epoch, and a dynamic datum's frame reference epoch.
    if key == "datum"
        && let Some(&(anchor, frame)) = datum.and_then(|c| db().epochs.get(&c))
    {
        if let Some(f) = frame {
            d.insert("type".into(), json!("DynamicGeodeticReferenceFrame"));
            d.insert("frame_reference_epoch".into(), num(f));
        }
        if let Some(a) = anchor {
            d.insert("anchor_epoch".into(), num(a));
        }
    }
    if let Some(d) = datum
        && let Some(Value::Object(old)) = crs.get(key)
        && let Some(ensemble) = ensemble_of(d, old)
    {
        crs.remove(key);
        crs.insert("datum_ensemble".into(), ensemble);
    }
    crs.insert("coordinate_system".into(), db_cs(code).ok_or_else(|| unsupported("a CRS the database table lacks"))?);
    Ok(())
}

/// The CRS of an EPSG code as PROJ's database gives it, from the CRS read from its WKT.
fn from_database(crs: &mut Map<String, Value>, code: u32) -> Result<()> {
    let entry = epsg::get(code);
    let kind = crs.get("type").and_then(Value::as_str).map(str::to_string);
    let known = db().crs.get(&code).ok_or_else(|| unsupported("a CRS the database table lacks"))?;
    match kind.as_deref() {
        Some("GeographicCRS") => database_geographic(crs, code)?,
        Some("ProjectedCRS") => {
            if !known.units {
                return Err(unsupported("a conversion the database gives in other units"));
            }
            let base_id =
                crs.get("base_crs").and_then(|b| b.get("id")).filter(|i| i["authority"] == "EPSG").and_then(|i| i["code"].as_u64());
            let geog = entry.map(|e| e.geog).filter(|g| *g != 0).or(base_id.map(|c| c as u32));
            match (geog, crs.get_mut("base_crs")) {
                (Some(g), Some(Value::Object(base))) => database_geographic(base, g)?,
                _ => return Err(unsupported("a projected CRS without its base CRS's code")),
            }
            if !known.conversion.is_empty()
                && let Some(c) = crs.get_mut("conversion")
            {
                c["name"] = json!(known.conversion);
            }
            let axes: Vec<Value> = known
                .axes
                .iter()
                .map(|a| {
                    let mut v = axis(&a.name, &a.abbreviation, &a.direction, &a.unit);
                    if let Some(m) = a.meridian {
                        v["meridian"] = json!({"longitude": num(m)});
                    }
                    v
                })
                .collect();
            crs.insert("coordinate_system".into(), json!({"subtype": "Cartesian", "axis": axes}));
        }
        _ => return Err(unsupported("a CRS of another type")),
    }
    let usage = |u: &Usage| json!({"scope": u.scope, "area": u.area, "bbox": bbox(u.bbox)});
    match known.usages.as_slice() {
        [] => {}
        [u] => {
            if let Value::Object(one) = usage(u) {
                crs.extend(one);
            }
        }
        several => {
            crs.insert("usages".into(), Value::Array(several.iter().map(usage).collect()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pj(s: &SpatialRef, database: bool) -> Value {
        s.to_gdal_projjson(database).unwrap()
    }

    #[test]
    fn utm_from_its_code_and_from_its_wkt_as_gdal_writes_them() {
        let s = SpatialRef::from_epsg(32633).unwrap();
        let wkt = pj(&s, false);
        // rasterio 1.5.0's `CRS.from_wkt(<GDAL's WKT of 32633>).to_dict(projjson=True)`.
        assert_eq!(wkt["base_crs"]["datum"]["name"], "World Geodetic System 1984");
        assert_eq!(wkt["base_crs"]["coordinate_system"]["axis"][0]["name"], "Geodetic latitude");
        assert_eq!(wkt["conversion"]["name"], "UTM zone 33N");
        assert!(wkt["conversion"].get("id").is_none());
        assert_eq!(wkt["coordinate_system"]["axis"][0]["abbreviation"], "");
        let db = pj(&s, true);
        // `CRS.from_epsg(32633)`: the database's ensemble, axes and usage.
        assert_eq!(db["base_crs"]["datum_ensemble"]["members"].as_array().unwrap().len(), 8);
        assert_eq!(db["base_crs"]["datum_ensemble"]["id"]["code"], 6326);
        assert_eq!(db["coordinate_system"]["axis"][0]["abbreviation"], "E");
        assert_eq!(db["scope"], "Navigation and medium accuracy spatial referencing.");
        assert_eq!(db["bbox"], json!({"south_latitude": 0, "west_longitude": 12, "north_latitude": 84, "east_longitude": 18}));
    }

    #[test]
    fn the_database_form_holds_what_the_database_defines() {
        // rasterio 1.5.0's `CRS.from_epsg(...).to_dict(projjson=True)`.
        let nad27 = pj(&SpatialRef::from_epsg(4267).unwrap(), true);
        assert_eq!(nad27["datum"]["ellipsoid"], json!({"name": "Clarke 1866", "semi_major_axis": 6378206.4, "semi_minor_axis": 6356583.8}));
        let etrs = pj(&SpatialRef::from_epsg(25832).unwrap(), true);
        assert_eq!(etrs["usages"].as_array().map(Vec::len), Some(2));
        assert_eq!(etrs["base_crs"]["datum_ensemble"]["name"], "European Terrestrial Reference System 1989 ensemble");
        let polar = pj(&SpatialRef::from_epsg(3031).unwrap(), true);
        assert_eq!(polar["coordinate_system"]["axis"][0]["meridian"], json!({"longitude": 90}));
        // From its WKT, the same CRSs carry what the WKT holds.
        assert_eq!(pj(&SpatialRef::from_epsg(4267).unwrap(), false)["datum"]["ellipsoid"]["inverse_flattening"], json!(294.978698213898));
        // The database gives Nord Maroc's parameters in grads, GDAL's WKT in degrees: refused.
        let grads = SpatialRef::from_epsg(26191).unwrap();
        assert!(grads.to_gdal_projjson(false).is_ok());
        assert!(grads.to_gdal_projjson(true).is_err());
    }

    #[test]
    fn numbers_have_proj_s_15_significant_digits() {
        // terrio's GRIB driver writes 17 digits; rasterio 1.5.0 gives 294.978698213911 for both
        // the GRIB file and `CRS.from_wkt` of this WKT.
        let wkt = r#"PROJCS["unnamed",GEOGCS["Coordinate System imported from GRIB file",DATUM["unnamed",SPHEROID["Spheroid imported from GRIB file",6378206.4,294.97869821391095]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",-117],PARAMETER["scale_factor",0.9996],PARAMETER["false_easting",500000],PARAMETER["false_northing",0],UNIT["Metre",1],AXIS["Easting",EAST],AXIS["Northing",NORTH]]"#;
        let j = pj(&SpatialRef::from_wkt(wkt), false);
        assert_eq!(j["base_crs"]["datum"]["ellipsoid"]["inverse_flattening"], json!(294.978698213911));
        assert_eq!(j["conversion"]["id"], json!({"authority": "EPSG", "code": 16011}));
        assert_eq!(j["conversion"]["parameters"][3]["value"], json!(500000));
    }

    #[test]
    fn unmodelled_crss_are_refused() {
        // Krovak, a PROJ string WKT 1 cannot describe, and a non-Greenwich prime meridian.
        for def in ["EPSG:5514", "+proj=ob_tran +o_proj=longlat +o_lat_p=40 +o_lon_p=0 +lon_0=0 +ellps=WGS84", "EPSG:4807"] {
            let s = SpatialRef::parse(def).unwrap();
            assert!(s.to_gdal_projjson(false).is_err(), "{def}");
            assert!(s.to_gdal_projjson(true).is_err(), "{def}");
        }
    }
}
