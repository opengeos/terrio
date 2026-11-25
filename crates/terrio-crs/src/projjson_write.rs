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

use serde_json::{Value, json};

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
    let mut v = crs(s, anchors(s))?;
    if let Value::Object(o) = &mut v {
        o.insert("$schema".into(), SCHEMA.into());
    }
    Ok(crate::projjson_order::to_string(&v))
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

/// Whether the WKT 2 a CRS was read from gives its geodetic and its vertical datum an
/// `ANCHOREPOCH`, which PROJ 9.9.0 writes (#1012 item 65): then the epoch is written also where
/// this writer rebuilds the datum from the registry (an identified code, the parts of a compound
/// one), from PROJ's database. PROJ writes none for a datum without one, nor for WKT 1 or a PROJ
/// string; nor does this for an EPSG code, whose registry WKT is GDAL's WKT 1, though PROJ writes
/// them for the code from its database.
#[derive(Clone, Copy)]
struct Anchors {
    geodetic: bool,
    vertical: bool,
}

fn anchors(s: &SpatialRef) -> Anchors {
    let none = Anchors { geodetic: false, vertical: false };
    let Some(w) = s.wkt.as_deref().filter(|_| s.proj4.is_none()) else { return none };
    if !crate::parsed::tree(w).is_some_and(|root| crate::model::dialect(w, &root) == crate::model::Dialect::Wkt2) {
        return none;
    }
    let upper = w.to_ascii_uppercase();
    let mut out = none;
    // Each epoch belongs to the datum node opened last before it.
    for (i, _) in upper.match_indices("ANCHOREPOCH[") {
        let before = &upper[..i];
        let last = |keys: &[&str]| keys.iter().filter_map(|k| before.rfind(k)).max();
        let vertical = last(&["VDATUM[", "VERTICALDATUM[", "VRF["]);
        let geodetic = last(&[",DATUM[", "[DATUM[", "GEODETICDATUM[", "TRF["]);
        match (geodetic, vertical) {
            (Some(g), Some(v)) if v > g => out.vertical = true,
            (_, Some(_)) if geodetic.is_none() => out.vertical = true,
            (Some(_), _) => out.geodetic = true,
            _ => {}
        }
    }
    out
}

fn crs(s: &SpatialRef, anchors: Anchors) -> Result<Value> {
    if let Some(code) = s.epsg {
        // A compound code, given or declared, by the registry; a compound CRS identified by its
        // parts (opengeos/terrio#1012 item 47) by its own text below, as PROJ 9.9.0 writes it.
        let declared = s.wkt.as_deref().is_none_or(|w| crate::declared_epsg(w) == Some(code));
        if let Some(c) = epsg::compound(code).filter(|_| declared) {
            let horizontal = crs(&SpatialRef::from_epsg(c.horizontal)?, anchors)?;
            return Ok(with_id(
                json!({"type": "CompoundCRS", "name": c.name, "components": [horizontal, vertical(c.vertical, anchors)?]}),
                Some(code),
            ));
        }
        if epsg::vertical(code).is_some() {
            return vertical(code, anchors);
        }
    }
    let wkt = s.to_wkt().ok_or_else(|| unsupported("a CRS without definition"))?;
    if let Some((h, v)) = crate::def::split_compound(&wkt) {
        // uncut: stored or built WKT
        let horizontal = crs(&SpatialRef::from_wkt_text(&h), anchors)?;
        // Its EPSG code only when its unit and direction are the code's: not heights in feet as
        // the code's metres.
        let code =
            crate::def::vertical_code(&v).ok_or_else(|| unsupported("a vertical CRS without an EPSG code of its unit and direction"))?;
        let name = wkt1::parse(&wkt).and_then(|n| n.name().map(str::to_string)).unwrap_or_else(|| "unknown".into());
        return Ok(with_id(
            json!({"type": "CompoundCRS", "name": name, "components": [horizontal, vertical(code, anchors)?]}),
            crate::declared_epsg(&wkt).filter(|c| crate::declared_fits(&wkt, *c)),
        ));
    }
    horizontal(s, &wkt, anchors)
}

/// An EPSG vertical CRS.
fn vertical(code: u32, anchors: Anchors) -> Result<Value> {
    let v = epsg::vertical(code).ok_or_else(|| unsupported(format!("EPSG:{code}, not a vertical CRS terrio knows")))?;
    let datum_name = wkt1::parse(&v.wkt)
        .and_then(|n| n.child("VERT_DATUM").or_else(|| n.child("VDATUM")).and_then(|d| d.name().map(str::to_string)))
        .unwrap_or_else(|| v.name.clone());
    let (name, abbreviation, direction) =
        if v.down { ("Gravity-related depth", "D", "down") } else { ("Gravity-related height", "H", "up") };
    let axis = json!({"name": name, "abbreviation": abbreviation, "direction": direction, "unit": length_unit(v.to_meter, None)});
    let mut datum = json!({"type": "VerticalReferenceFrame", "name": datum_name});
    // Its anchor epoch, which PROJ 9.9.0 writes (#1012 item 65).
    if let Some(epoch) = epsg::anchor_epoch(v.datum).filter(|_| anchors.vertical) {
        datum["anchor_epoch"] = epoch.into();
    }
    Ok(json!({
        "type": "VerticalCRS",
        "name": v.name,
        "datum": with_id(datum, (v.datum != 0).then_some(v.datum)),
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

/// The geographic (or geodetic) CRS node of a WKT tree: a projected CRS's base, the first in its
/// order (see [`Node::geodetic_base`]), or the root.
fn base_node(root: &Node) -> Option<&Node> {
    use crate::keywords::{GEOGRAPHIC_OR_GEODETIC_CRS, WKT1_GEOCENTRIC, WKT1_GEOGRAPHIC};
    root.geodetic_base()
        .or_else(|| [GEOGRAPHIC_OR_GEODETIC_CRS, WKT1_GEOGRAPHIC, WKT1_GEOCENTRIC].iter().any(|keys| root.is(keys)).then_some(root))
}

fn horizontal(s: &SpatialRef, wkt: &str, anchors: Anchors) -> Result<Value> {
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
    // A WKT's PROJ4 extension is computed on as a pipeline step; it is described by the CRS rules
    // of its PROJ string only where those read it the same way.
    // Not a registry CRS's own WKT (ESRI:102100's extension is Web Mercator's, written as such).
    let extension = s.proj4.is_none() && entry.is_none() && s.esri.is_none() && crate::proj4_extension(wkt).is_some();
    if extension && !crate::extension_places_as_string(&proj) {
        return Err(unsupported("a PROJ4 extension PROJ computes on otherwise than its PROJ string"));
    }
    // Nor one on another figure than the WKT's ellipsoid, which this writer writes (PROJ 9.8.1
    // writes the WKT's, `+R=6400000` beside a WGS 84 `SPHEROID` as WGS 84).
    let spheroid = base.datum().and_then(|d| d.first_in(crate::keywords::ELLIPSOID));
    // But Web Mercator on its WGS 84 (GDAL's, GeoServer's EPSG:900913), which this writer writes
    // as Popular Visualisation Pseudo Mercator, as PROJ does.
    let web_mercator = pseudo_mercator(&Params::parse(&proj));
    if extension
        && !web_mercator
        && let Some((a, rf)) = spheroid.and_then(|e| Some((e.num(1)?, e.num(2)?)))
    {
        let f = if rf == 0.0 { 0.0 } else { 1.0 / rf };
        if (a - def.ell.a).abs() > 1e-3 || (2.0 * f - f * f - def.ell.es).abs() > 1e-10 {
            return Err(unsupported("a PROJ4 extension on another ellipsoid than its WKT's"));
        }
    }
    let p = Params::parse(&proj);
    refuse_misdescribed(&root, &p, &def)?;
    // `+nadgrids=@null` beside a datum whose transformations to WGS 84 shift points (OSGB36):
    // PROJ writes a bound CRS with the null grid, which terrio's reader does not take; written
    // as the datum alone, it would be read back with them.
    if p.has("nadgrids") && def.ops().iter().all(|op| op.is_null_shift()) && !def.ops().is_empty() {
        let shifting = def
            .datum_code()
            .and_then(|d| p.str("datum").and_then(crate::def::datum_geog).filter(|_| d != 6326))
            .and_then(epsg::get)
            .is_some_and(|e| !e.ops.iter().all(|op| op.is_null_shift()));
        if shifting {
            return Err(unsupported("a datum given no shift by +nadgrids=@null (PROJ's bound CRS with the null grid)"));
        }
    }
    let name = root.name().unwrap_or("unknown").to_string();
    // A root code only when it is the CRS's (`identify_epsg` refuses another's).
    let code = s.epsg.or_else(|| crate::declared_epsg(wkt).filter(|c| crate::declared_fits(wkt, *c)));
    // A datum shift the definition gives (`+towgs84`, WKT's TOWGS84) makes a bound CRS, as PROJ
    // reads it; a CRS made from an EPSG code (its table WKT) is written as such, whatever that
    // WKT holds: its datum shift is EPSG's.
    let from_table = s.proj4.is_none() && entry.is_some_and(|e| s.wkt.as_deref() == Some(e.wkt.as_str()));
    let given = if from_table { None } else { s.proj4.clone().or_else(|| s.wkt.as_deref().and_then(crate::wkt_proj)) };
    // `+nadgrids` wins over `+towgs84` (`@null` alone: no shift, written as none; others refused
    // as unwritten), as PROJ reads them.
    let towgs84 = match given.map(|g| Params::parse(&g)).filter(|g| !g.has("nadgrids")).and_then(|g| g.list("towgs84")) {
        Some(t) if !matches!(t.len(), 3 | 7) => return Err(unsupported(format!("a +towgs84 of {} values", t.len()))),
        Some(t) if t.iter().any(|v| !v.is_finite()) => return Err(unsupported("a +towgs84 with a non-finite value")),
        t => t,
    };
    let mut written = vec!["towgs84"];
    let datum = datum(base, &p, &def, anchors.geodetic)?;
    let iau = match (code, s.authority()) {
        (None, Some(("IAU_2015", n))) => Some(json!({"authority": "IAU", "code": n, "version": 2015})),
        _ => None,
    };
    let with_code = |mut o: Value| {
        let mut o = match &iau {
            Some(id) => {
                o["id"] = id.clone();
                o
            }
            None => with_id(o, code),
        };
        // A conversion's own code (a UTM zone's) only in a CRS without one, as PROJ 9.9.0 writes
        // it: EPSG:32733's conversion has none, `+proj=utm +zone=33 +south`'s is EPSG:16133.
        if o.get("id").is_some()
            && let Some(c) = o.get_mut("conversion").and_then(Value::as_object_mut)
        {
            c.remove("id");
        }
        o
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
    // The figure computed on, as PROJ reads the string, against the CRS's ellipsoid written:
    // misdescribed when a flag makes another sphere of it (`+R_C` on an ellipsoid).
    let shape = crate::def::proj_ellipsoid(p, p.str("proj").unwrap_or_default(), false).ok();
    let ellipsoidal = shape.is_some_and(|e| if e.conformal { e.figure.es != 0.0 } else { e.figure != e.crs });
    if ellipsoidal {
        return Err(match ["R_A", "R_C", "R_V", "R_a", "R_g", "R_h", "R_lat_a", "R_lat_g"].iter().find(|k| p.has(k)) {
            Some(k) => unsupported(format!("a projection on an auxiliary sphere (+{k})")),
            // Spherical orthographic (`+f=0`), which PROJ writes as its own "Orthographic
            // (Spherical)", a method terrio's readers do not take.
            None => unsupported("spherical orthographic on an ellipsoid (+f=0)"),
        });
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
fn datum(base: &Node, p: &Params, def: &Def, anchored: bool) -> Result<Value> {
    let node = base.datum();
    let code = node.and_then(Node::epsg);
    let name =
        code.and_then(epsg::datum_name).or_else(|| node.and_then(Node::name).map(str::to_string)).unwrap_or_else(|| "unknown".into());
    let e = node.and_then(|d| d.first_in(crate::keywords::ELLIPSOID));
    // The WKT's ellipsoid as written (EPSG:3857's datum is WGS 84, projected on a sphere), else
    // the PROJ string's.
    let es_rf = |es: f64| if es == 0.0 { 0.0 } else { 1.0 / (1.0 - (1.0 - es).sqrt()) };
    // In metres, from its `LENGTHUNIT` (PROJ's WKT 2 of EPSG:4157 gives Clarke 1858 in feet).
    let (a, rf) = match e.and_then(Node::ellipsoid) {
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
    // A WKT 2 datum's `ANCHOREPOCH`, which PROJ 9.9.0 writes (#1012 item 65), else the EPSG
    // datum's where the WKT 2 read had one (`anchored`) but the datum is the registry's here; not
    // for a WKT without one, which PROJ leaves without.
    if let Some(epoch) =
        node.and_then(|n| n.child("ANCHOREPOCH")).and_then(|a| a.num(0)).or_else(|| code.filter(|_| anchored).and_then(epsg::anchor_epoch))
    {
        d["anchor_epoch"] = epoch.into();
    }
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

use crate::methods::{Kind, conversion_from_proj, pseudo_mercator};

/// The conversion of a projected CRS's PROJ string, by its EPSG method, and the PROJ string's
/// parameters it carries (see [`refuse_unwritten`]).
fn conversion(p: &Params) -> Result<(Value, Vec<&'static str>)> {
    let c = conversion_from_proj(p).map_err(unsupported)?;
    let parameters: Vec<Value> = c
        .values
        .into_iter()
        .map(|(p, value)| {
            let unit = match p.kind {
                Kind::Angle => "degree",
                Kind::Length => "metre",
                Kind::Scale => "unity",
            };
            json!({"name": p.name, "value": value, "unit": unit, "id": id(p.code)})
        })
        .collect();
    let name = c.name.unwrap_or_else(|| "unknown".to_string());
    Ok((
        with_id(
            json!({"name": name, "method": with_id(json!({"name": c.method}), c.method_code), "parameters": parameters}),
            c.conversion_code,
        ),
        c.written.to_vec(),
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
