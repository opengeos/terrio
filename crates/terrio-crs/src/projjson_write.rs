// Written to match the behavior of:
//   PROJ 9.8.1:
//     src/iso19111/io.cpp (JSONFormatter)
// No code is translated from these sources.

//! PROJJSON v0.7 of a CRS: geographic 2D and 3D, geocentric, projected, vertical, compound and
//! bound (`+towgs84`) CRSs, from the CRS's WKT (names, identifiers, axes) and its PROJ string
//! (conversion parameters), as PROJ writes them for EPSG CRSs.
//!
//! Deliberate differences from PROJ's output: no `scope`, `area`, `bbox` or `usages`; WGS 84 and
//! other datum ensembles are written as their datum (`World Geodetic System 1984`, EPSG:6326)
//! without members; parameters are in degrees and metres rather than the CRS's own units;
//! projection conversions are named `unknown` (UTM zones excepted); polar axes are east and
//! north rather than along meridians; axes are named after their direction. Planetary CRSs are
//! written as PROJ writes them (west axes, planetocentric spherical coordinate systems).
//!
//! What it would describe wrongly is refused rather than written: projections on an auxiliary
//! sphere of an ellipsoid (`+R_C`), and any parameter of the PROJ string the PROJJSON would not
//! carry, unless it has the value used when it is absent (see [`refuse_unwritten`]).

use serde_json::{Map, Value, json};

use crate::params::Params;
use crate::wkt1::{self, Node};
use crate::{Def, Error, Result, SpatialRef, epsg};

const SCHEMA: &str = "https://proj.org/schemas/v0.7/projjson.schema.json";
const DEGREE: f64 = std::f64::consts::PI / 180.0;

fn unsupported(what: impl std::fmt::Display) -> Error {
    Error::Crs(format!("no PROJJSON for {what}"))
}

/// The PROJJSON of a CRS.
pub(crate) fn write(s: &SpatialRef) -> Result<String> {
    let mut v = crs(s)?;
    if let Value::Object(o) = &mut v {
        // `$schema` first, as PROJ writes it.
        let mut out = Map::new();
        out.insert("$schema".into(), SCHEMA.into());
        out.append(o);
        v = Value::Object(out);
    }
    serde_json::to_string(&v).map_err(|e| Error::Crs(e.to_string()))
}

fn id(code: u32) -> Value {
    json!({"authority": "EPSG", "code": code})
}

/// Inserts an EPSG identifier, when there is one.
fn with_id(mut v: Value, code: Option<u32>) -> Value {
    if let (Value::Object(o), Some(code)) = (&mut v, code) {
        o.insert("id".into(), id(code));
    }
    v
}

fn crs(s: &SpatialRef) -> Result<Value> {
    if let Some(code) = s.epsg {
        if let Some(c) = epsg::compound(code) {
            let horizontal = crs(&SpatialRef::from_epsg(c.horizontal)?)?;
            return Ok(with_id(
                json!({"type": "CompoundCRS", "name": c.name, "components": [horizontal, vertical(c.vertical)?]}),
                Some(code),
            ));
        }
        if epsg::vertical(code).is_some() {
            return vertical(code);
        }
    }
    let wkt = s.to_wkt().ok_or_else(|| unsupported("a CRS without definition"))?;
    if let Some((h, v)) = crate::def::split_compound(&wkt) {
        let horizontal = crs(&SpatialRef::from_wkt(&h))?;
        let code = wkt1::parse(&v).and_then(|n| n.epsg()).ok_or_else(|| unsupported("a vertical CRS without EPSG code"))?;
        let name = wkt1::parse(&wkt).and_then(|n| n.name().map(str::to_string)).unwrap_or_else(|| "unknown".into());
        return Ok(with_id(
            json!({"type": "CompoundCRS", "name": name, "components": [horizontal, vertical(code)?]}),
            crate::declared_epsg(&wkt),
        ));
    }
    horizontal(s, &wkt)
}

/// An EPSG vertical CRS.
fn vertical(code: u32) -> Result<Value> {
    let v = epsg::vertical(code).ok_or_else(|| unsupported(format!("EPSG:{code}, not a vertical CRS terrio knows")))?;
    let datum_name = wkt1::parse(&v.wkt)
        .and_then(|n| n.child("VERT_DATUM").or_else(|| n.child("VDATUM")).and_then(|d| d.name().map(str::to_string)))
        .unwrap_or_else(|| v.name.clone());
    let (name, abbreviation, direction) =
        if v.down { ("Gravity-related depth", "D", "down") } else { ("Gravity-related height", "H", "up") };
    let axis = json!({"name": name, "abbreviation": abbreviation, "direction": direction, "unit": length_unit(v.to_meter, None)});
    Ok(json!({
        "type": "VerticalCRS",
        "name": v.name,
        "datum": with_id(json!({"type": "VerticalReferenceFrame", "name": datum_name}), (v.datum != 0).then_some(v.datum)),
        "coordinate_system": {"subtype": "vertical", "axis": [axis]},
        "id": id(code),
    }))
}

fn length_unit(to_meter: f64, name: Option<&str>) -> Value {
    if (to_meter - 1.0).abs() < 1e-15 {
        return "metre".into();
    }
    let name = name.map(str::to_string).unwrap_or_else(|| match to_meter {
        f if (f - 0.3048).abs() < 1e-15 => "foot".into(),
        f if (f - 1200.0 / 3937.0).abs() < 1e-15 => "US survey foot".into(),
        f => format!("{f} metre"),
    });
    json!({"type": "LinearUnit", "name": name, "conversion_factor": to_meter})
}

fn angular_unit(node: Option<&Node>) -> Value {
    match node.and_then(|u| Some((u.name()?, u.num(1)?))) {
        Some((name, f)) if (f - DEGREE).abs() > 1e-15 => json!({"type": "AngularUnit", "name": name, "conversion_factor": f}),
        _ => "degree".into(),
    }
}

/// The geographic (or geodetic) CRS node of a WKT tree: the root, or a projected CRS's base.
fn base_node(root: &Node) -> Option<&Node> {
    ["GEOGCS", "BASEGEOGCRS", "BASEGEODCRS", "GEOGCRS", "GEODCRS", "GEOCCS"]
        .iter()
        .find_map(|k| root.child(k))
        .or_else(|| ["GEOGCS", "GEOGCRS", "GEODCRS", "GEOCCS", "GEOGRAPHICCRS", "GEODETICCRS"].contains(&root.key.as_str()).then_some(root))
}

fn horizontal(s: &SpatialRef, wkt: &str) -> Result<Value> {
    let root = wkt1::parse(wkt).ok_or_else(|| unsupported("an unreadable WKT"))?;
    let def = Def::new(s)?;
    let entry = s.epsg.and_then(epsg::get);
    let base = base_node(&root).ok_or_else(|| unsupported("a CRS without geodetic datum"))?;
    // The WGS 84 and ETRS89 ensembles are static in PROJJSON. A CRS without code on a dynamic
    // base CRS (by its code), or a WKT 2 with a DYNAMIC node, would lose its frame epoch too.
    let dynamic = |e: &epsg::Entry| e.dynamic && !matches!(e.datum, 6326 | 6258);
    let dynamic_node = root.child("DYNAMIC").is_some() || base.child("DYNAMIC").is_some();
    if dynamic_node || entry.is_some_and(dynamic) || base.epsg().and_then(epsg::get).is_some_and(dynamic) {
        return Err(unsupported("a dynamic CRS (its frame reference epoch)"));
    }
    let proj = match (entry, s.proj4.as_deref()) {
        (_, Some(p)) => p.to_string(),
        (Some(e), None) => e.proj.clone(),
        (None, None) => crate::wkt_proj(wkt).ok_or_else(|| unsupported("a WKT terrio reads otherwise"))?,
    };
    let p = Params::parse(&proj);
    refuse_misdescribed(&root, &p, &def)?;
    let name = root.name().unwrap_or("unknown").to_string();
    let code = s.epsg.or_else(|| crate::declared_epsg(wkt));
    // A datum shift the definition gives (`+towgs84`, WKT's TOWGS84) makes a bound CRS, as PROJ
    // reads it; a CRS made from an EPSG code (its table WKT) is written as such, whatever that
    // WKT holds: its datum shift is EPSG's.
    let from_table = s.proj4.is_none() && entry.is_some_and(|e| s.wkt.as_deref() == Some(e.wkt.as_str()));
    let given = if from_table { None } else { s.proj4.clone().or_else(|| s.wkt.as_deref().and_then(crate::wkt_proj)) };
    let towgs84 = match given.and_then(|g| Params::parse(&g).list("towgs84")) {
        Some(t) if !matches!(t.len(), 3 | 7) => return Err(unsupported(format!("a +towgs84 of {} values", t.len()))),
        Some(t) if t.iter().any(|v| !v.is_finite()) => return Err(unsupported("a +towgs84 with a non-finite value")),
        t => t,
    };
    let mut written = vec!["towgs84"];
    let datum = datum(base, &p, &def)?;
    let iau = match (code, s.authority()) {
        (None, Some(("IAU_2015", n))) => Some(json!({"authority": "IAU", "code": n, "version": 2015})),
        _ => None,
    };
    let with_code = |mut o: Value| match &iau {
        Some(id) => {
            o["id"] = id.clone();
            o
        }
        None => with_id(o, code),
    };

    let out = if def.is_geocentric() {
        let unit = length_unit(def.to_meter(), None);
        let axes: Vec<Value> = ["X", "Y", "Z"]
            .iter()
            .map(|a| json!({"name": format!("Geocentric {a}"), "abbreviation": a, "direction": format!("geocentric{a}"), "unit": unit}))
            .collect();
        let mut o = json!({"type": "GeodeticCRS", "name": name});
        o["datum"] = datum;
        o["coordinate_system"] = json!({"subtype": "Cartesian", "axis": axes});
        with_id(o, code)
    } else if def.is_geographic() {
        // Planetocentric latitudes: PROJ's geodetic CRS with a spherical coordinate system.
        let planetocentric = p.has("geoc") || spherical_cs(&root);
        let kind = if planetocentric { "GeodeticCRS" } else { "GeographicCRS" };
        let mut o = json!({"type": kind, "name": name});
        o["datum"] = datum;
        o["coordinate_system"] = geographic_cs(&root, base, def.has_height(), Some(&p), planetocentric);
        with_code(o)
    } else {
        let base_name = base.name().unwrap_or("unknown");
        let mut b = json!({"type": "GeographicCRS", "name": base_name});
        b["datum"] = datum;
        b["coordinate_system"] = geographic_cs(base, base, false, None, false);
        let b = with_id(b, base.epsg());
        let mut o = json!({"type": "ProjectedCRS", "name": name, "base_crs": b});
        let (c, keys) = conversion(&p)?;
        o["conversion"] = c;
        written.extend(keys);
        o["coordinate_system"] = projected_cs(&root, def.to_meter(), p.str("axis"));
        with_code(o)
    };
    refuse_unwritten(&p, &written, entry.is_some())?;
    match towgs84 {
        Some(t) => Ok(bound(out, &t, base.name().unwrap_or("unknown"), def.is_geocentric(), def.is_geographic() && def.has_height())),
        None => Ok(out),
    }
}

/// Refuses what this writer would describe wrongly: projections on an auxiliary sphere of an
/// ellipsoid (`+R_C`, `+R_A`...: PROJ's spherical method variants), and planetocentric
/// latitudes with heights. A projected CRS's coordinates do not depend on how its base CRS's
/// latitudes are given, so a planetocentric base is written.
fn refuse_misdescribed(root: &Node, p: &Params, def: &Def) -> Result<()> {
    // The ellipsoid the PROJ string names, before an auxiliary sphere replaces it: its own
    // parameters, its datum's, else WGS 84 (as `Def::from_proj` defaults).
    let named = crate::datum::ellipsoid(p)
        .or_else(|| p.str("datum").and_then(crate::def::datum_ellps).and_then(crate::math::Ell::named))
        .unwrap_or_else(crate::math::Ell::wgs84);
    let ellipsoidal = named.es != 0.0;
    if ellipsoidal && let Some(k) = ["R_A", "R_C", "R_V", "R_a", "R_g", "R_h", "R_lat_a", "R_lat_g"].iter().find(|k| p.has(k)) {
        return Err(unsupported(format!("a projection on an auxiliary sphere (+{k})")));
    }
    if def.is_geographic() && def.has_height() && (p.has("geoc") || spherical_cs(root)) {
        return Err(unsupported("planetocentric latitudes with heights"));
    }
    Ok(())
}

/// The parameters written by other parts than the conversion (ellipsoid, datum, units, axes, prime
/// meridian), or decided before (auxiliary spheres, planetocentric latitudes).
const DESCRIBED: &[&str] = &[
    "proj", "ellps", "datum", "a", "b", "rf", "f", "R", "es", "e", "units", "to_meter", "to_deg", "axis", "pm", "geoc", "no_defs",
    "wktext", "type", "R_A", "R_C", "R_V", "R_a", "R_g", "R_h", "R_lat_a", "R_lat_g",
];

/// Refuses a parameter of the PROJ string the PROJJSON does not carry (`written` by the
/// conversion, or [`DESCRIBED`]), so that nothing is dropped silently, unless it has the value
/// used when it is absent: 0 for `lat_0`, `lon_0`, `x_0` and `y_0`, 1 for `k` and `k_0`.
/// `+nadgrids=@null` keeps a datum as it is; an EPSG CRS's datum shift grids are EPSG's.
fn refuse_unwritten(p: &Params, written: &[&str], epsg: bool) -> Result<()> {
    for key in p.keys() {
        let value = p.str(key);
        let irrelevant = written.contains(&key)
            || DESCRIBED.contains(&key)
            || match key {
                "lat_0" | "lon_0" => p.angle(key) == Some(0.0),
                "x_0" | "y_0" => p.f64(key) == Some(0.0),
                "k" | "k_0" => p.f64(key) == Some(1.0),
                "nadgrids" => epsg || value.is_some_and(|v| v.split(',').all(|g| g.trim_start_matches('@') == "null")),
                "geoidgrids" => epsg,
                _ => false,
            };
        if !irrelevant {
            return Err(unsupported(format!("the parameter +{key}{}", value.map(|v| format!("={v}")).unwrap_or_default())));
        }
    }
    Ok(())
}

/// Whether the WKT's coordinate system is spherical (planetocentric latitudes, in WKT 2).
fn spherical_cs(root: &Node) -> bool {
    root.child("CS").is_some_and(|cs| matches!(cs.args.first(), Some(wkt1::Arg::Ident(t)) if t.eq_ignore_ascii_case("spherical")))
}

/// A geodetic reference frame, from the WKT's datum and ellipsoid nodes and the PROJ string.
fn datum(base: &Node, p: &Params, def: &Def) -> Result<Value> {
    let node = base.child("DATUM").or_else(|| base.child("ENSEMBLE"));
    let code = node.and_then(Node::epsg);
    let name =
        code.and_then(epsg::datum_name).or_else(|| node.and_then(Node::name).map(str::to_string)).unwrap_or_else(|| "unknown".into());
    let e = node.and_then(|d| d.child("SPHEROID").or_else(|| d.child("ELLIPSOID")));
    // The WKT's ellipsoid as written (EPSG:3857's datum is WGS 84, projected on a sphere), else
    // the PROJ string's.
    let es_rf = |es: f64| if es == 0.0 { 0.0 } else { 1.0 / (1.0 - (1.0 - es).sqrt()) };
    let (a, rf) = match e.and_then(|e| Some((e.num(1)?, e.num(2)?))) {
        Some((a, rf)) if a > 0.0 && (rf == 0.0 || rf > 1.0) => (a, rf),
        _ => (def.ell.a, es_rf(def.ell.es)),
    };
    let mut ellipsoid = json!({"name": e.and_then(Node::name).unwrap_or("unknown")});
    // A semi-minor axis given as such (`+b`, as IAU ellipsoids are) is written as such, as PROJ
    // writes it: a rounded inverse flattening moves points on very flat ellipsoids (Eros).
    let b = p.f64("b").filter(|b| e.and_then(Node::epsg).is_none() && rf != 0.0 && *b > 0.0 && *b < a && p.f64("a") == Some(a));
    if rf == 0.0 {
        ellipsoid["radius"] = a.into();
    } else if let Some(b) = b {
        ellipsoid["semi_major_axis"] = a.into();
        ellipsoid["semi_minor_axis"] = b.into();
    } else {
        ellipsoid["semi_major_axis"] = a.into();
        ellipsoid["inverse_flattening"] = rf.into();
    }
    let ellipsoid = with_id(ellipsoid, e.and_then(Node::epsg));
    let mut d = json!({"type": "GeodeticReferenceFrame", "name": name, "ellipsoid": ellipsoid});
    if let Some(pm) = prime_meridian(base, p)? {
        d["prime_meridian"] = pm;
    }
    Ok(with_id(d, code))
}

/// The prime meridian, unless Greenwich: its longitude in degrees. One the WKT or `+pm` gives
/// but this cannot read is refused rather than written as Greenwich.
fn prime_meridian(base: &Node, p: &Params) -> Result<Option<Value>> {
    let node = base.child("PRIMEM");
    // The PROJ string's `+pm` first: the WKT terrio writes for one it cannot read says Greenwich.
    let longitude = match (p.str("pm"), node) {
        (Some(v), _) => v
            .parse::<f64>()
            .ok()
            .or_else(|| crate::math::prime_meridian(v))
            .ok_or_else(|| unsupported(format!("the prime meridian +pm={v}")))?,
        // WKT 2 gives its unit; GDAL's WKT 1 writes degrees.
        (None, Some(n)) => n
            .num(1)
            .map(|l| l * n.child("ANGLEUNIT").and_then(|u| u.num(1)).map_or(1.0, |f| f / DEGREE))
            .ok_or_else(|| unsupported("an unreadable PRIMEM"))?,
        (None, None) => return Ok(None),
    };
    if !longitude.is_finite() {
        return Err(unsupported("a non-finite prime meridian"));
    }
    if longitude == 0.0 {
        return Ok(None);
    }
    let name = node.and_then(Node::name).map(str::to_string).or_else(|| p.str("pm").map(str::to_string)).unwrap_or_default();
    Ok(Some(with_id(json!({"name": name, "longitude": longitude}), node.and_then(Node::epsg))))
}

/// The axes of a node, as `(name, direction)` in lower case.
fn axes(node: &Node) -> Vec<(String, String)> {
    let cs = node.children("AXIS").collect::<Vec<_>>();
    cs.iter()
        .filter_map(|a| {
            let direction = match a.args.get(1)? {
                wkt1::Arg::Ident(d) | wkt1::Arg::Str(d) => d.to_ascii_lowercase(),
                _ => return None,
            };
            Some((a.name().unwrap_or("").to_ascii_lowercase(), direction))
        })
        .collect()
}

/// A geographic CRS's coordinate system, its axes in the order and directions of the PROJ
/// string's `+axis` (west-positive longitudes of planetographic CRSs too), else the WKT's (the
/// WKT terrio writes for a PROJ string gives the default axes), else EPSG's (latitude first) for
/// an EPSG CRS, else longitude first as PROJ strings give them.
/// Planetocentric latitudes have PROJ's spherical coordinate system.
fn geographic_cs(root: &Node, base: &Node, three_d: bool, p: Option<&Params>, planetocentric: bool) -> Value {
    let unit = angular_unit(
        base.child("UNIT").or_else(|| base.child("ANGLEUNIT")).or_else(|| root.children("AXIS").find_map(|a| a.child("ANGLEUNIT"))),
    );
    let (kind, lat, lon) = if planetocentric { ("Planetocentric", "U", "V") } else { ("Geodetic", "Lat", "Lon") };
    let latitude = |direction: &str| json!({"name": format!("{kind} latitude"), "abbreviation": lat, "direction": direction, "unit": unit});
    let longitude =
        |direction: &str| json!({"name": format!("{kind} longitude"), "abbreviation": lon, "direction": direction, "unit": unit});
    let axis_of = |d: &str| match d {
        "north" | "south" => Some(latitude(d)),
        "east" | "west" => Some(longitude(d)),
        _ => None,
    };
    let from_wkt: Vec<Value> = axes(root).iter().take(2).filter_map(|(_, d)| axis_of(d)).collect();
    let from_proj: Vec<Value> = p
        .and_then(|p| p.str("axis"))
        .map(|a| {
            a.bytes()
                .take(2)
                .filter_map(|c| match c {
                    b'n' => axis_of("north"),
                    b's' => axis_of("south"),
                    b'e' => axis_of("east"),
                    b'w' => axis_of("west"),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let mut axis = if from_proj.len() == 2 {
        from_proj
    } else if from_wkt.len() == 2 {
        from_wkt
    } else if base.epsg().is_some() {
        vec![latitude("north"), longitude("east")]
    } else {
        vec![longitude("east"), latitude("north")]
    };
    if three_d {
        axis.push(json!({"name": "Ellipsoidal height", "abbreviation": "h", "direction": "up", "unit": "metre"}));
    }
    json!({"subtype": if planetocentric { "spherical" } else { "ellipsoidal" }, "axis": axis})
}

fn projected_cs(root: &Node, to_meter: f64, axis: Option<&str>) -> Value {
    let unit_node = root.child("UNIT").or_else(|| root.child("LENGTHUNIT"));
    let unit = length_unit(to_meter, unit_node.and_then(Node::name).filter(|n| *n != "metre" && *n != "meter"));
    let mut directions: Vec<String> = axes(root).into_iter().map(|(_, d)| d).collect();
    // The PROJ string's `+axis` (`+axis=wnu`), as terrio transforms: the WKT terrio writes for
    // a PROJ string keeps its axes in its PROJ4 extension.
    if let Some(a) = axis.filter(|a| a.len() >= 2) {
        directions = a
            .bytes()
            .take(2)
            .map(|c| match c {
                b'w' => "west",
                b'n' => "north",
                b's' => "south",
                _ => "east",
            })
            .map(String::from)
            .collect();
    }
    // Polar CSs give both axes along meridians ("north" along 90°E): east and north, as terrio
    // and GDAL read them, rather than PROJ's axes with a `meridian`.
    if directions.len() != 2 || directions[0] == directions[1] {
        directions = vec!["east".into(), "north".into()];
    }
    let axis: Vec<Value> = directions
        .iter()
        .map(|d| {
            let (name, abbreviation) = match d.as_str() {
                "north" => ("Northing", "N"),
                "south" => ("Southing", "S"),
                "west" => ("Westing", "W"),
                _ => ("Easting", "E"),
            };
            json!({"name": name, "abbreviation": abbreviation, "direction": d, "unit": unit})
        })
        .collect();
    json!({"subtype": "Cartesian", "axis": axis})
}

/// An EPSG operation parameter: its name, code, and whether it is an angle (degrees), a length
/// (metres) or a scale.
type Param = (&'static str, u32, char);

const LAT_NAT: Param = ("Latitude of natural origin", 8801, 'a');
const LON_NAT: Param = ("Longitude of natural origin", 8802, 'a');
const K_NAT: Param = ("Scale factor at natural origin", 8805, 's');
const FE: Param = ("False easting", 8806, 'l');
const FN: Param = ("False northing", 8807, 'l');
const LAT_CENTRE: Param = ("Latitude of projection centre", 8811, 'a');
const LON_CENTRE: Param = ("Longitude of projection centre", 8812, 'a');
const AZIMUTH: Param = ("Azimuth at projection centre", 8813, 'a');
const GAMMA: Param = ("Angle from Rectified to Skew Grid", 8814, 'a');
const K_CENTRE: Param = ("Scale factor at projection centre", 8815, 's');
const FE_CENTRE: Param = ("Easting at projection centre", 8816, 'l');
const FN_CENTRE: Param = ("Northing at projection centre", 8817, 'l');
const LAT_FALSE: Param = ("Latitude of false origin", 8821, 'a');
const LON_FALSE: Param = ("Longitude of false origin", 8822, 'a');
const SP1: Param = ("Latitude of 1st standard parallel", 8823, 'a');
const SP2: Param = ("Latitude of 2nd standard parallel", 8824, 'a');
const FE_FALSE: Param = ("Easting at false origin", 8826, 'l');
const FN_FALSE: Param = ("Northing at false origin", 8827, 'l');
const LAT_TS: Param = ("Latitude of standard parallel", 8832, 'a');
const LON_ORIGIN: Param = ("Longitude of origin", 8833, 'a');
const CO_LAT: Param = ("Co-latitude of cone axis", 1036, 'a');
const LAT_PSEUDO: Param = ("Latitude of pseudo standard parallel", 8818, 'a');
const K_PSEUDO: Param = ("Scale factor on pseudo standard parallel", 8819, 's');
const K_ELLIPSOID: Param = ("Ellipsoid scaling factor", 1038, 's');

/// The conversion of a projected CRS's PROJ string, by its EPSG method, and the PROJ string's
/// parameters it carries (see [`refuse_unwritten`]).
fn conversion(p: &Params) -> Result<(Value, Vec<&'static str>)> {
    let proj = p.str("proj").unwrap_or("");
    let deg = |k: &str| p.angle(k).map(f64::to_degrees);
    let num = |k: &str| p.f64(k);
    let k0 = num("k_0").or_else(|| num("k")).unwrap_or(1.0);
    let (lat0, lon0) = (deg("lat_0").unwrap_or(0.0), deg("lon_0").unwrap_or(0.0));
    let (x0, y0) = (num("x_0").unwrap_or(0.0), num("y_0").unwrap_or(0.0));
    let sphere = !p.has("ellps") && !p.has("datum") && (p.has("R") || (p.has("a") && p.f64("a") == p.f64("b")));
    let mut name = "unknown".to_string();
    let mut conversion_id = None;
    // The parameters most methods carry.
    const ORIGIN: &[&str] = &["lat_0", "lon_0", "x_0", "y_0"];
    const ORIGIN_K: &[&str] = &["lat_0", "lon_0", "k", "k_0", "x_0", "y_0"];
    const CONIC: &[&str] = &["lat_0", "lon_0", "lat_1", "lat_2", "x_0", "y_0"];
    let (method, code, values, written): (&str, u32, Vec<(Param, f64)>, &[&str]) = match proj {
        "utm" => {
            let zone = num("zone").ok_or_else(|| unsupported("a UTM projection without zone"))? as u32;
            if !(1..=60).contains(&zone) {
                return Err(unsupported(format!("a UTM projection of zone {zone}")));
            }
            let south = p.has("south");
            name = format!("UTM zone {zone}{}", if south { "S" } else { "N" });
            conversion_id = Some(if south { 17000 } else { 16000 } + zone);
            let lon = f64::from(zone) * 6.0 - 183.0;
            let values =
                vec![(LAT_NAT, 0.0), (LON_NAT, lon), (K_NAT, 0.9996), (FE, 500000.0), (FN, if south { 10_000_000.0 } else { 0.0 })];
            ("Transverse Mercator", 9807, values, &["zone", "south"])
        }
        "tmerc" => ("Transverse Mercator", 9807, vec![(LAT_NAT, lat0), (LON_NAT, lon0), (K_NAT, k0), (FE, x0), (FN, y0)], ORIGIN_K),
        "lcc" => {
            // As PROJ (and terrio) read the CRS: with `lat_0` and no `lat_2`, one standard parallel
            // (1SP, or variant B away from the origin); else two, the second on the equator
            // without `lat_2` (2SP, or 2SP Michigan with a scale).
            let sp1 = deg("lat_1").unwrap_or(0.0);
            let one = !p.has("lat_2") && p.has("lat_0");
            if one && lat0 == sp1 {
                let values = vec![(LAT_NAT, sp1), (LON_NAT, lon0), (K_NAT, k0), (FE, x0), (FN, y0)];
                ("Lambert Conic Conformal (1SP)", 9801, values, &["lat_0", "lat_1", "lon_0", "k", "k_0", "x_0", "y_0"])
            } else if one {
                let values = vec![(LAT_NAT, sp1), (K_NAT, k0), (LAT_FALSE, lat0), (LON_FALSE, lon0), (FE_FALSE, x0), (FN_FALSE, y0)];
                ("Lambert Conic Conformal (1SP variant B)", 1102, values, &["lat_0", "lat_1", "lon_0", "k", "k_0", "x_0", "y_0"])
            } else {
                let mut values = vec![
                    (LAT_FALSE, lat0),
                    (LON_FALSE, lon0),
                    (SP1, sp1),
                    (SP2, deg("lat_2").unwrap_or(0.0)),
                    (FE_FALSE, x0),
                    (FN_FALSE, y0),
                ];
                if k0 == 1.0 {
                    ("Lambert Conic Conformal (2SP)", 9802, values, &["lat_0", "lat_1", "lat_2", "lon_0", "k", "k_0", "x_0", "y_0"])
                } else {
                    values.push((K_ELLIPSOID, k0));
                    (
                        "Lambert Conic Conformal (2SP Michigan)",
                        1051,
                        values,
                        &["lat_0", "lat_1", "lat_2", "lon_0", "k", "k_0", "x_0", "y_0"],
                    )
                }
            }
        }
        "aea" | "eqdc" => {
            let values = vec![
                (LAT_FALSE, lat0),
                (LON_FALSE, lon0),
                (SP1, deg("lat_1").unwrap_or(0.0)),
                (SP2, deg("lat_2").unwrap_or(0.0)),
                (FE_FALSE, x0),
                (FN_FALSE, y0),
            ];
            if proj == "aea" { ("Albers Equal Area", 9822, values, CONIC) } else { ("Equidistant Conic", 1119, values, CONIC) }
        }
        "laea" if sphere => {
            ("Lambert Azimuthal Equal Area (Spherical)", 1027, vec![(LAT_NAT, lat0), (LON_NAT, lon0), (FE, x0), (FN, y0)], ORIGIN)
        }
        "laea" => ("Lambert Azimuthal Equal Area", 9820, vec![(LAT_NAT, lat0), (LON_NAT, lon0), (FE, x0), (FN, y0)], ORIGIN),
        // EPSG:3857: WGS 84 coordinates projected on its semi-major axis's sphere.
        "merc" if sphere && num("a") == Some(6378137.0) && deg("lat_ts").unwrap_or(0.0) == 0.0 && k0 == 1.0 => {
            let values = vec![(LAT_NAT, 0.0), (LON_NAT, lon0), (FE, x0), (FN, y0)];
            ("Popular Visualisation Pseudo Mercator", 1024, values, &["lat_ts", "lon_0", "x_0", "y_0"])
        }
        "merc" => match deg("lat_ts").filter(|t| *t != 0.0) {
            Some(ts) => {
                ("Mercator (variant B)", 9805, vec![(SP1, ts), (LON_NAT, lon0), (FE, x0), (FN, y0)], &["lat_ts", "lon_0", "x_0", "y_0"])
            }
            None => {
                let values = vec![(LAT_NAT, 0.0), (LON_NAT, lon0), (K_NAT, k0), (FE, x0), (FN, y0)];
                ("Mercator (variant A)", 9804, values, &["lat_ts", "lon_0", "k", "k_0", "x_0", "y_0"])
            }
        },
        "stere" if lat0.abs() == 90.0 => match deg("lat_ts").filter(|_| !p.has("k") && !p.has("k_0")) {
            Some(ts) => (
                "Polar Stereographic (variant B)",
                9829,
                vec![(LAT_TS, ts), (LON_ORIGIN, lon0), (FE, x0), (FN, y0)],
                &["lat_0", "lat_ts", "lon_0", "x_0", "y_0"],
            ),
            None => {
                ("Polar Stereographic (variant A)", 9810, vec![(LAT_NAT, lat0), (LON_NAT, lon0), (K_NAT, k0), (FE, x0), (FN, y0)], ORIGIN_K)
            }
        },
        "sterea" => ("Oblique Stereographic", 9809, vec![(LAT_NAT, lat0), (LON_NAT, lon0), (K_NAT, k0), (FE, x0), (FN, y0)], ORIGIN_K),
        "somerc" => {
            let values = vec![
                (LAT_CENTRE, lat0),
                (LON_CENTRE, lon0),
                (AZIMUTH, 90.0),
                (GAMMA, 90.0),
                (K_CENTRE, k0),
                (FE_CENTRE, x0),
                (FN_CENTRE, y0),
            ];
            ("Hotine Oblique Mercator (variant B)", 9815, values, ORIGIN_K)
        }
        "omerc" => {
            let alpha = deg("alpha").ok_or_else(|| unsupported("an oblique Mercator without azimuth"))?;
            let gamma = deg("gamma").unwrap_or(alpha);
            let lonc = deg("lonc").unwrap_or(lon0);
            // `lon_0` is the centre's longitude only without `lonc`.
            let written: &[&str] = if p.has("lonc") {
                &["lat_0", "lonc", "alpha", "gamma", "k", "k_0", "x_0", "y_0", "no_uoff", "no_off"]
            } else {
                &["lat_0", "lon_0", "alpha", "gamma", "k", "k_0", "x_0", "y_0", "no_uoff", "no_off"]
            };
            if p.has("no_uoff") || p.has("no_off") {
                let values =
                    vec![(LAT_CENTRE, lat0), (LON_CENTRE, lonc), (AZIMUTH, alpha), (GAMMA, gamma), (K_CENTRE, k0), (FE, x0), (FN, y0)];
                ("Hotine Oblique Mercator (variant A)", 9812, values, written)
            } else {
                let values = vec![
                    (LAT_CENTRE, lat0),
                    (LON_CENTRE, lonc),
                    (AZIMUTH, alpha),
                    (GAMMA, gamma),
                    (K_CENTRE, k0),
                    (FE_CENTRE, x0),
                    (FN_CENTRE, y0),
                ];
                ("Hotine Oblique Mercator (variant B)", 9815, values, written)
            }
        }
        "krovak" => {
            // As terrio and PROJ compute it: their defaults, and the pseudo standard parallel fixed
            // (PROJ ignores `lat_1` and `lat_ts`). The co-latitude of the cone axis is fixed too:
            // another `alpha` (which PROJ reads) is refused.
            if let Some(alpha) = deg("alpha").filter(|a| (a - 30.288_139_752_777_8).abs() > 1e-6) {
                return Err(unsupported(format!("a Krovak projection with +alpha={alpha}")));
            }
            let values = vec![
                (LAT_CENTRE, deg("lat_0").unwrap_or(49.5)),
                (LON_ORIGIN, deg("lon_0").unwrap_or(24.0 + 50.0 / 60.0)),
                (CO_LAT, 30.288_139_752_777_8),
                (LAT_PSEUDO, 78.5),
                (K_PSEUDO, num("k_0").or_else(|| num("k")).unwrap_or(0.9999)),
                (FE, x0),
                (FN, y0),
            ];
            let written: &[&str] = &["lat_0", "lon_0", "alpha", "lat_1", "lat_ts", "k", "k_0", "x_0", "y_0"];
            if p.str("axis") == Some("swu") {
                ("Krovak", 9819, values, written)
            } else {
                ("Krovak (North Orientated)", 1041, values, written)
            }
        }
        // Its latitude of origin changes nothing (PROJ writes none); a `k_0` without `lat_ts` is
        // not written.
        "cea" => {
            let values = vec![(SP1, deg("lat_ts").unwrap_or(0.0)), (LON_NAT, lon0), (FE, x0), (FN, y0)];
            ("Lambert Cylindrical Equal Area", 9835, values, &["lat_0", "lat_ts", "lon_0", "x_0", "y_0"])
        }
        "eqc" => {
            // Its latitude of origin as PROJ writes it: only when not 0.
            let mut values = vec![(SP1, deg("lat_ts").unwrap_or(0.0)), (LON_NAT, lon0), (FE, x0), (FN, y0)];
            if lat0 != 0.0 {
                values.insert(1, (LAT_NAT, lat0));
            }
            ("Equidistant Cylindrical", 1028, values, &["lat_0", "lat_ts", "lon_0", "x_0", "y_0"])
        }
        "cass" => ("Cassini-Soldner", 9806, vec![(LAT_NAT, lat0), (LON_NAT, lon0), (FE, x0), (FN, y0)], ORIGIN),
        "poly" => ("American Polyconic", 9818, vec![(LAT_NAT, lat0), (LON_NAT, lon0), (FE, x0), (FN, y0)], ORIGIN),
        "nzmg" => ("New Zealand Map Grid", 9811, vec![(LAT_NAT, lat0), (LON_NAT, lon0), (FE, x0), (FN, y0)], ORIGIN),
        "aeqd" => ("Azimuthal Equidistant", 1125, vec![(LAT_NAT, lat0), (LON_NAT, lon0), (FE, x0), (FN, y0)], ORIGIN),
        "ortho" => ("Orthographic", 9840, vec![(LAT_NAT, lat0), (LON_NAT, lon0), (FE, x0), (FN, y0)], ORIGIN),
        "eqearth" => ("Equal Earth", 1078, vec![(LON_NAT, lon0), (FE, x0), (FN, y0)], &["lon_0", "x_0", "y_0"]),
        "bonne" => {
            // Its parallel is `lat_1`; `lat_0` changes nothing then.
            let values = vec![(LAT_NAT, deg("lat_1").unwrap_or(lat0)), (LON_NAT, lon0), (FE, x0), (FN, y0)];
            ("Bonne", 9827, values, if p.has("lat_1") { &["lat_0", "lat_1", "lon_0", "x_0", "y_0"] } else { ORIGIN })
        }
        other => return Err(unsupported(format!("the projection +proj={other}"))),
    };
    // `str::parse` reads `nan` and `inf`, which JSON cannot carry.
    if let Some(((pname, ..), _)) = values.iter().find(|(_, v)| !v.is_finite()) {
        return Err(unsupported(format!("a non-finite {}", pname.to_lowercase())));
    }
    let parameters: Vec<Value> = values
        .into_iter()
        .map(|((pname, pcode, kind), value)| {
            let unit = match kind {
                'a' => "degree",
                'l' => "metre",
                _ => "unity",
            };
            json!({"name": pname, "value": value, "unit": unit, "id": id(pcode)})
        })
        .collect();
    Ok((
        with_id(json!({"name": name, "method": {"name": method, "id": id(code)}, "parameters": parameters}), conversion_id),
        written.to_vec(),
    ))
}

/// A bound CRS: `source` with its transformation to WGS 84, as PROJ writes `+towgs84`: in the
/// geocentric domain to WGS 84's geocentric CRS for a geocentric source, else in the geog2D
/// domain to WGS 84's geographic 2D or (for a source with heights) 3D CRS.
fn bound(source: Value, t: &[f64], geographic: &str, geocentric: bool, three_d: bool) -> Value {
    // PROJ knows "metre", "degree" and "unity" by name; other units are objects.
    let arc_second = json!({"type": "AngularUnit", "name": "arc-second", "conversion_factor": DEGREE / 3600.0});
    let ppm = json!({"type": "ScaleUnit", "name": "parts per million", "conversion_factor": 1e-6});
    let names: [(&str, u32, Value); 7] = [
        ("X-axis translation", 8605, "metre".into()),
        ("Y-axis translation", 8606, "metre".into()),
        ("Z-axis translation", 8607, "metre".into()),
        ("X-axis rotation", 8608, arc_second.clone()),
        ("Y-axis rotation", 8609, arc_second.clone()),
        ("Z-axis rotation", 8610, arc_second),
        ("Scale difference", 8611, ppm),
    ];
    let (method, code) = match (t.len() == 3, geocentric) {
        (true, false) => ("Geocentric translations (geog2D domain)", 9603),
        (false, false) => ("Position Vector transformation (geog2D domain)", 9606),
        (true, true) => ("Geocentric translations (geocentric domain)", 1031),
        (false, true) => ("Position Vector transformation (geocentric domain)", 1033),
    };
    let parameters: Vec<Value> =
        t.iter().zip(names).map(|(v, (name, code, unit))| json!({"name": name, "value": v, "unit": unit, "id": id(code)})).collect();
    let datum = json!({"type": "GeodeticReferenceFrame", "name": "World Geodetic System 1984",
                       "ellipsoid": {"name": "WGS 84", "semi_major_axis": 6378137, "inverse_flattening": 298.257223563, "id": id(7030)},
                       "id": id(6326)});
    let target = if geocentric {
        let axes: Vec<Value> = ["X", "Y", "Z"]
            .iter()
            .map(|a| json!({"name": format!("Geocentric {a}"), "abbreviation": a, "direction": format!("geocentric{a}"), "unit": "metre"}))
            .collect();
        json!({"type": "GeodeticCRS", "name": "WGS 84", "datum": datum,
               "coordinate_system": {"subtype": "Cartesian", "axis": axes}, "id": id(4978)})
    } else {
        let mut axes = vec![
            json!({"name": "Geodetic latitude", "abbreviation": "Lat", "direction": "north", "unit": "degree"}),
            json!({"name": "Geodetic longitude", "abbreviation": "Lon", "direction": "east", "unit": "degree"}),
        ];
        if three_d {
            axes.push(json!({"name": "Ellipsoidal height", "abbreviation": "h", "direction": "up", "unit": "metre"}));
        }
        json!({"type": "GeographicCRS", "name": "WGS 84", "datum": datum,
               "coordinate_system": {"subtype": "ellipsoidal", "axis": axes}, "id": id(if three_d { 4979 } else { 4326 })})
    };
    json!({
        "type": "BoundCRS",
        "source_crs": source,
        "target_crs": target,
        "transformation": {"name": format!("Transformation from {geographic} to WGS84"), "method": {"name": method, "id": id(code)}, "parameters": parameters},
    })
}
