//! A WKT 2 (ISO 19162, 2015 and 2019) reader for geographic and projected CRSs, turning them
//! into PROJ strings as [`super::wkt1`] does for WKT 1, so they go through terrio's projections.
//!
//! In WKT 2 a parameter's value is in the unit of its own `ANGLEUNIT`, `LENGTHUNIT` or
//! `SCALEUNIT` node; without one, angles are in the base CRS's angular unit and lengths in the
//! projected CS's unit. Methods and parameters are named as in EPSG, and identified by their
//! EPSG codes when the WKT gives them.

use super::wkt1::{Arg, Converted, Node, norm};
use crate::{Error, Result};

/// The keywords of the WKT 2 CRSs this reader converts.
const GEOGRAPHIC: [&str; 4] = ["GEOGCRS", "GEOGRAPHICCRS", "GEODCRS", "GEODETICCRS"];
const PROJECTED: [&str; 2] = ["PROJCRS", "PROJECTEDCRS"];
const BASE: [&str; 4] = ["BASEGEOGCRS", "BASEGEODCRS", "BASEGEOGRAPHICCRS", "BASEGEODETICCRS"];

/// Whether `root` is a WKT 2 CRS this reader converts.
pub(crate) fn handles(root: &Node) -> bool {
    let key = root.key.to_ascii_uppercase();
    if key == "BOUNDCRS" {
        // Only of a CRS this reader converts.
        return root.child("SOURCECRS").and_then(first_node).is_some_and(handles);
    }
    if GEOGRAPHIC.contains(&key.as_str()) {
        // A geocentric GEODCRS (a Cartesian CS) is not a longitude-latitude CRS.
        return !root
            .child("CS")
            .is_some_and(|cs| matches!(cs.args.first(), Some(Arg::Ident(t)) if !t.eq_ignore_ascii_case("ellipsoidal")));
    }
    PROJECTED.contains(&key.as_str())
}

/// Converts a WKT 2 geographic or projected CRS, or a `BOUNDCRS` to WGS 84 of one, into a PROJ
/// string. Fails on what it cannot convert faithfully: an unknown method, or an unknown
/// parameter.
pub(crate) fn to_proj(root: &Node) -> Result<Converted> {
    let key = root.key.to_ascii_uppercase();
    if key == "BOUNDCRS" {
        let source = root.child("SOURCECRS").and_then(first_node).ok_or_else(|| bad("a BOUNDCRS without its SOURCECRS"))?;
        let mut c = to_proj(source)?;
        if let Some(t) = towgs84(root)? {
            c.proj = format!("{} +towgs84={}", c.proj, t.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(","));
            c.geog = None;
        }
        return Ok(c);
    }
    let projected = PROJECTED.contains(&key.as_str());
    if !projected && let Some(conv) = root.child("DERIVINGCONVERSION") {
        return derived(root, conv);
    }
    let base = if projected { BASE.iter().find_map(|k| root.child(k)).ok_or_else(|| bad("a PROJCRS without its base CRS"))? } else { root };
    let geog = Geodetic::read(base, !projected)?;
    let mut tail = geog.tail();
    if !projected {
        let mut out = vec!["+proj=longlat".to_string()];
        if (geog.ang - 1.0).abs() > 1e-12 {
            out.push(format!("+to_deg={}", geog.ang));
        }
        out.append(&mut tail);
        return Ok(geog.converted(out.join(" ")));
    }

    // The projected CS's linear unit, and its axes' directions.
    let to_meter = root
        .child("LENGTHUNIT")
        .or_else(|| root.child("UNIT"))
        .or_else(|| root.children("AXIS").find_map(|a| a.child("LENGTHUNIT").or_else(|| a.child("UNIT"))))
        .map(|u| u.num(1).ok_or_else(|| bad("a unit without its size")))
        .transpose()?
        .unwrap_or(1.0);
    // Polar CSs give their axes' directions along meridians ("south" along 90°E): those are
    // read east-north, as GDAL does.
    let polar = root.children("AXIS").any(|a| a.child("MERIDIAN").is_some());
    let directions: Vec<String> = root
        .children("AXIS")
        .filter(|_| !polar)
        .filter_map(|a| a.args.get(1).and_then(|d| if let Arg::Ident(s) = d { Some(s.to_ascii_lowercase()) } else { None }))
        .collect();

    let conv = root.child("CONVERSION").ok_or_else(|| bad("a PROJCRS without its CONVERSION"))?;
    let method = conv.child("METHOD").ok_or_else(|| bad("a CONVERSION without its METHOD"))?;
    // PROJ's own methods (`METHOD["PROJ hammer"]`), with its parameters by their PROJ names.
    let generic = proj_method(conv, method, geog.ang, to_meter)?;
    let p = if generic.is_some() { Params::default() } else { Params::read(conv, geog.ang, to_meter)? };
    let (name, code) = (norm(method.name().unwrap_or("")), method.epsg());
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
    let mut sweep = None;
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
        push("lat_2", p.get(SP2).or_else(|| p.get(SP1)));
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
    } else if is(&["hotineobliquemercatorvariantb"], &[9815])
        && p.get(AZIMUTH).is_some_and(|a| (a - 90.0).abs() < 1e-9)
        && p.get(GAMMA).is_some_and(|g| (g - 90.0).abs() < 1e-9)
    {
        // With an azimuth and a rectified grid angle of 90°, the Swiss oblique Mercator, as PROJ
        // reads it (and writes it, from `+proj=somerc`).
        fe = p.get(FE_CENTRE).unwrap_or(fe);
        fn_ = p.get(FN_CENTRE).unwrap_or(fn_);
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
    } else if is(&["cassinisoldner"], &[9806]) {
        push("lat_0", lat_0());
        push("lon_0", lon_0());
        "cass"
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
    } else if is(&["krovaknorthorientated"], &[1041]) {
        // As PROJ, whose Krovak fixes the pseudo standard parallel and writes none.
        p.get(LAT_PSEUDO_SP);
        push("lat_0", p.get(LAT_CENTRE).or_else(lat_0));
        push("lon_0", lon_0());
        push("alpha", p.get(CO_LAT_CONE));
        push("k", Some(p.get(K_PSEUDO_SP).or_else(|| p.get(K_NAT)).unwrap_or(0.9999)));
        "krovak"
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
    } else if let Some(proj) = WORLD.iter().find(|(n, _)| *n == name).map(|(_, p)| *p) {
        push("lon_0", lon_0());
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
                // The figure, datum, units and axes come from the CRS, not from the method.
                "proj" | "ellps" | "datum" | "a" | "b" | "rf" | "f" | "es" | "e" | "R" | "R_A" | "units" | "to_meter" | "towgs84"
                | "nadgrids" | "geoidgrids" | "init" | "type" | "pm" | "axis" | "no_defs" | "wktext" | "over" => {
                    return Err(bad(&format!("parameter '{k}' of the method 'PROJ {proj}'")));
                }
                _ => push(k, Some(*v)),
            }
        }
        proj.as_str()
    } else {
        return Err(Error::Crs(format!("unsupported WKT 2 projection method '{}'", method.name().unwrap_or(""))));
    };
    // Parameters the method does not use would be ignored: refuse them, as they change the
    // coordinates.
    if let Some(unused) = p.unused() {
        return Err(bad(&format!("parameter '{unused}' of the method '{}'", method.name().unwrap_or(""))));
    }
    let mut proj_str = vec![format!("+proj={proj}")];
    if no_uoff {
        proj_str.push("+no_uoff".into());
    }
    if let Some(s) = sweep {
        proj_str.push(format!("+sweep={s}"));
    }
    proj_str.append(&mut out);
    if south_orientated {
        proj_str.push("+axis=wsu".into());
    } else if let Some(axis) = axis(&directions) {
        proj_str.push(format!("+axis={axis}"));
    }
    proj_str.push(format!("+x_0={fe}"));
    proj_str.push(format!("+y_0={fn_}"));
    proj_str.push(format!("+to_meter={to_meter}"));
    proj_str.append(&mut tail);
    Ok(geog.converted(proj_str.join(" ")))
}

/// A derived geographic CRS (a `GEOGCRS` with a `DERIVINGCONVERSION`) as a PROJ string: a
/// rotated-pole grid, in one of the three forms PROJ writes. Fails on any other conversion, which
/// would otherwise be dropped.
fn derived(root: &Node, conv: &Node) -> Result<Converted> {
    let base = BASE.iter().find_map(|k| root.child(k)).ok_or_else(|| bad("a derived geographic CRS without its base CRS"))?;
    let geog = Geodetic::read(base, false)?;
    let method = conv.child("METHOD").ok_or_else(|| bad("a DERIVINGCONVERSION without its METHOD"))?;
    let method_name = method.name().unwrap_or("");
    // The parameters, in degrees, by normalised name.
    let mut values = Vec::new();
    for n in conv.children("PARAMETER") {
        let name = n.name().unwrap_or("");
        let v = n.num(1).ok_or_else(|| bad(&format!("parameter '{name}' without a value")))?;
        let unit = n.child("ANGLEUNIT").or_else(|| n.child("UNIT")).and_then(|u| u.num(1));
        values.push((norm(name), name.to_string(), v * unit.map(degrees).unwrap_or(geog.ang)));
    }
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
    let unit = root.children("AXIS").find_map(|a| a.child("ANGLEUNIT").or_else(|| a.child("UNIT"))).and_then(|u| u.num(1));
    if let Some(ang) = unit.map(degrees).filter(|a| (a - 1.0).abs() > 1e-12) {
        out.push(format!("+to_deg={ang}"));
    }
    out.append(&mut geog.tail());
    Ok(geog.converted(out.join(" ")))
}

/// A PROJ projection and its parameters by their PROJ names.
type ProjMethod = (String, Vec<(String, f64)>);

/// A method PROJ writes as `PROJ name` (a projection without a WKT 2 method of its own), with
/// its parameters, named as in PROJ strings: angles in degrees and lengths in metres. `None` for
/// other methods.
fn proj_method(conv: &Node, method: &Node, ang: f64, to_meter: f64) -> Result<Option<ProjMethod>> {
    let Some(name) = method.name().and_then(|n| n.strip_prefix("PROJ ")) else { return Ok(None) };
    if name.is_empty() || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
        return Ok(None);
    }
    let mut params = Vec::new();
    for n in conv.children("PARAMETER") {
        let key = n.name().unwrap_or("");
        if key.is_empty() || !key.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
            return Err(bad(&format!("parameter '{key}' of the method 'PROJ {name}'")));
        }
        let v = n.num(1).ok_or_else(|| bad(&format!("parameter '{key}' without a value")))?;
        let v = if let Some(u) = n.child("ANGLEUNIT").and_then(|u| u.num(1)) {
            v * degrees(u)
        } else if let Some(u) = n.child("LENGTHUNIT").and_then(|u| u.num(1)) {
            v * u
        } else if n.child("SCALEUNIT").is_some() {
            v
        } else if key.starts_with("lat") || key.starts_with("lon") || matches!(key, "alpha" | "gamma" | "azi" | "theta" | "tilt") {
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
    Ok(Some((name.to_string(), params)))
}

/// Pseudocylindrical and other world projections PROJ names without an EPSG method, which take a
/// central meridian only.
const WORLD: [(&str, &str); 26] = [
    ("mollweide", "moll"),
    ("robinson", "robin"),
    ("sinusoidal", "sinu"),
    ("eckerti", "eck1"),
    ("eckertii", "eck2"),
    ("eckertiii", "eck3"),
    ("eckertiv", "eck4"),
    ("eckertv", "eck5"),
    ("eckertvi", "eck6"),
    ("millercylindrical", "mill"),
    ("vandergrinten", "vandg"),
    ("naturalearth", "natearth"),
    ("quarticauthalic", "qua_aut"),
    ("goodehomolosine", "igh"),
    ("aitoff", "aitoff"),
    ("crasterparabolic", "crast"),
    ("naturalearthii", "natearth2"),
    ("patterson", "patterson"),
    ("times", "times"),
    ("flatpolarquartic", "mbtfpq"),
    ("wagneri", "wag1"),
    ("wagnerii", "wag2"),
    ("wagneriv", "wag4"),
    ("wagnerv", "wag5"),
    ("wagnervi", "wag6"),
    ("gallstereographic", "gall"),
];

/// The axis order of a projected CS as PROJ's `+axis`, when not east then north. As GDAL's
/// traditional GIS order, a north-east CS is read east-north: only west and south change the
/// coordinates.
fn axis(directions: &[String]) -> Option<String> {
    let has = |d: &str| directions.iter().any(|x| x == d);
    let (e, n) = (if has("west") { 'w' } else { 'e' }, if has("south") { 's' } else { 'n' });
    (e != 'e' || n != 'n').then(|| format!("{e}{n}u"))
}

/// The first child node of a node.
fn first_node(n: &Node) -> Option<&Node> {
    n.args.iter().find_map(|a| if let Arg::Node(n) = a { Some(n) } else { None })
}

fn bad(what: &str) -> Error {
    Error::Crs(format!("unsupported WKT 2: {what}"))
}

/// A geodetic CRS's ellipsoid, prime meridian, angular unit and datum.
struct Geodetic {
    node: Node,
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
    fn read(n: &Node, root: bool) -> Result<Self> {
        let datum = ["DATUM", "GEODETICDATUM", "TRF", "ENSEMBLE"]
            .iter()
            .find_map(|k| n.child(k))
            .ok_or_else(|| bad("a geodetic CRS without its datum"))?;
        let ell = datum.child("ELLIPSOID").or_else(|| datum.child("SPHEROID")).ok_or_else(|| bad("a datum without its ellipsoid"))?;
        let ell_unit = ell.child("LENGTHUNIT").or_else(|| ell.child("UNIT")).and_then(|u| u.num(1)).unwrap_or(1.0);
        let a = ell.num(1).ok_or_else(|| bad("an ellipsoid without its semi-major axis"))? * ell_unit;
        let rf = ell.num(2).ok_or_else(|| bad("an ellipsoid without its inverse flattening"))?;
        // The CRS's angular unit: its own node, else its CS axes' (a geographic CRS), else the
        // prime meridian's, else degrees.
        let unit = n
            .child("ANGLEUNIT")
            .or_else(|| n.child("UNIT"))
            .or_else(|| if root { n.children("AXIS").find_map(|a| a.child("ANGLEUNIT").or_else(|| a.child("UNIT"))) } else { None })
            .or_else(|| n.child("PRIMEM").and_then(|p| p.child("ANGLEUNIT").or_else(|| p.child("UNIT"))));
        let ang = unit.and_then(|u| u.num(1)).map(degrees).unwrap_or(1.0);
        let pm = match n.child("PRIMEM") {
            Some(p) => {
                let v = p.num(1).unwrap_or(0.0);
                match p.child("ANGLEUNIT").or_else(|| p.child("UNIT")).and_then(|u| u.num(1)) {
                    Some(r) => v * degrees(r),
                    None => v * ang,
                }
            }
            None => 0.0,
        };
        // The EPSG geographic CRS: by the CRS's code, else its datum's (6xxx for 4xxx), else its
        // name, on the same ellipsoid.
        let geog = n.epsg().or_else(|| {
            let d = datum.epsg()?;
            let code = d.checked_sub(2000)?;
            super::epsg::get(code).filter(|e| e.datum == d && e.proj.starts_with("+proj=longlat")).map(|_| code)
        });
        let geog = geog.or_else(|| {
            let name = n.name().filter(|s| !s.eq_ignore_ascii_case("unknown"))?;
            // The official name is exact (realizations have their own, "NAD83(HARN)"); still,
            // an ambiguous name identifies nothing.
            let es = if rf == 0.0 { 0.0 } else { (2.0 - 1.0 / rf) / rf };
            let found: Vec<u32> = super::epsg::named(name).into_iter().filter(|c| same_ellipsoid(*c, a, es)).collect();
            (found.len() == 1).then(|| found[0])
        });
        Ok(Self {
            node: n.clone(),
            a,
            rf,
            ang,
            pm,
            geog,
            datum_name: datum.name().unwrap_or("unknown").to_string(),
            ellipsoid_name: ell.name().unwrap_or("unknown").to_string(),
        })
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
        // A WKT 1 `GEOGCS` of the datum, for identifying it by name when it has no code.
        let q = |s: &str| s.replace('"', "\"\"");
        let geogcs_wkt1 = format!(
            "GEOGCS[\"{}\",DATUM[\"{}\",SPHEROID[\"{}\",{},{}]],PRIMEM[\"Greenwich\",{}],UNIT[\"degree\",0.0174532925199433]]",
            q(self.node.name().unwrap_or("unknown")),
            q(&self.datum_name),
            q(&self.ellipsoid_name),
            self.a,
            self.rf,
            self.pm
        );
        Converted { proj, geogcs: self.node.clone(), geog: self.geog, geogcs_wkt1: Some(geogcs_wkt1) }
    }
}

/// Whether an EPSG geographic 2D CRS is on an ellipsoid (its semi-major axis and eccentricity
/// squared).
pub(crate) fn same_ellipsoid(code: u32, a: f64, es: f64) -> bool {
    let Some(e) = super::epsg::get(code).filter(|e| e.proj.starts_with("+proj=longlat") && !e.three_d) else { return false };
    let Ok(d) = super::def::Def::from_proj(&e.proj) else { return false };
    (d.ell.a - a).abs() < 1e-3 && (d.ell.es - es).abs() < 1e-9
}

/// The size of an angular unit given in radians, in degrees, with WKT's rounded degree and grad
/// made exact.
fn degrees(radians: f64) -> f64 {
    let d = radians.to_degrees();
    [1.0, 0.9, 1.0 / 3600.0].into_iter().find(|u| (d - u).abs() < 1e-9 * u).unwrap_or(d)
}

/// The kind of a parameter's value.
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Angle,
    Length,
    Scale,
}

/// A parameter: its normalised EPSG names and its EPSG code.
type Param = (&'static [&'static str], u32, Kind);

const LAT_NAT: Param = (&["latitudeofnaturalorigin", "latitudeoforigin"], 8801, Kind::Angle);
const LON_NAT: Param = (&["longitudeofnaturalorigin", "centralmeridian"], 8802, Kind::Angle);
const K_NAT: Param = (&["scalefactoratnaturalorigin"], 8805, Kind::Scale);
const FE: Param = (&["falseeasting"], 8806, Kind::Length);
const FN: Param = (&["falsenorthing"], 8807, Kind::Length);
const LAT_CENTRE: Param = (&["latitudeofprojectioncentre"], 8811, Kind::Angle);
const LON_CENTRE: Param = (&["longitudeofprojectioncentre"], 8812, Kind::Angle);
const AZIMUTH: Param = (&["azimuthatprojectioncentre", "azimuthofinitialline"], 8813, Kind::Angle);
const GAMMA: Param = (&["anglefromrectifiedtoskewgrid"], 8814, Kind::Angle);
const K_CENTRE: Param = (&["scalefactoratprojectioncentre", "scalefactoroninitialline"], 8815, Kind::Scale);
const FE_CENTRE: Param = (&["eastingatprojectioncentre"], 8816, Kind::Length);
const FN_CENTRE: Param = (&["northingatprojectioncentre"], 8817, Kind::Length);
const LAT_FALSE: Param = (&["latitudeoffalseorigin"], 8821, Kind::Angle);
const LON_FALSE: Param = (&["longitudeoffalseorigin"], 8822, Kind::Angle);
const SP1: Param = (&["latitudeof1ststandardparallel"], 8823, Kind::Angle);
const SP2: Param = (&["latitudeof2ndstandardparallel"], 8824, Kind::Angle);
const FE_FALSE: Param = (&["eastingatfalseorigin"], 8826, Kind::Length);
const FN_FALSE: Param = (&["northingatfalseorigin"], 8827, Kind::Length);
const LAT_TS: Param = (&["latitudeofstandardparallel"], 8832, Kind::Angle);
const LON_ORIGIN: Param = (&["longitudeoforigin"], 8833, Kind::Angle);

const CO_LAT_CONE: Param = (&["colatitudeofconeaxis"], 1036, Kind::Angle);
const LAT_PSEUDO_SP: Param = (&["latitudeofpseudostandardparallel"], 8818, Kind::Angle);
const K_PSEUDO_SP: Param = (&["scalefactoronpseudostandardparallel"], 8819, Kind::Scale);

/// PROJ's parameters without an EPSG code: codes down from `u32::MAX`, which match no `ID`.
const SAT_HEIGHT: Param = (&["satelliteheight"], u32::MAX, Kind::Length);
const LAT_P1: Param = (&["latitudeof1stpoint"], u32::MAX - 1, Kind::Angle);
const LON_P1: Param = (&["longitudeof1stpoint"], u32::MAX - 2, Kind::Angle);
const LAT_P2: Param = (&["latitudeof2ndpoint"], u32::MAX - 3, Kind::Angle);
const LON_P2: Param = (&["longitudeof2ndpoint"], u32::MAX - 4, Kind::Angle);
const LAT_TRUE_SCALE: Param = (&["latitudeoftruescale"], u32::MAX - 5, Kind::Angle);

const PARAMS: [Param; 29] = [
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
    CO_LAT_CONE,
    LAT_PSEUDO_SP,
    K_PSEUDO_SP,
];

/// A conversion's parameters, in degrees, metres and unity, and which were used.
#[derive(Default)]
struct Params {
    values: Vec<(u32, String, f64)>,
    used: std::cell::RefCell<Vec<u32>>,
}

impl Params {
    fn read(conv: &Node, ang: f64, to_meter: f64) -> Result<Self> {
        let mut values = Vec::new();
        for n in conv.children("PARAMETER") {
            let name = n.name().unwrap_or("");
            let key = norm(name);
            let param = PARAMS
                .iter()
                .find(|(names, code, _)| names.contains(&key.as_str()) || n.epsg() == Some(*code))
                .ok_or_else(|| bad(&format!("parameter '{name}'")))?;
            let v = n.num(1).ok_or_else(|| bad(&format!("parameter '{name}' without a value")))?;
            let unit = ["ANGLEUNIT", "LENGTHUNIT", "SCALEUNIT", "UNIT"].iter().find_map(|k| n.child(k)).and_then(|u| u.num(1));
            let v = match param.2 {
                Kind::Angle => v * unit.map(degrees).unwrap_or(ang),
                Kind::Length => v * unit.unwrap_or(to_meter),
                Kind::Scale => v * unit.unwrap_or(1.0),
            };
            values.push((param.1, name.to_string(), v));
        }
        // Easting and northing at the false origin are the false easting and northing.
        for v in &mut values {
            if v.0 == FE_FALSE.1 {
                v.0 = FE.1;
            } else if v.0 == FN_FALSE.1 {
                v.0 = FN.1;
            }
        }
        Ok(Self { values, used: Default::default() })
    }

    /// The value of a parameter, marked used.
    fn get(&self, p: Param) -> Option<f64> {
        let v = self.values.iter().find(|v| v.0 == p.1)?;
        self.used.borrow_mut().push(p.1);
        Some(v.2)
    }

    /// The name of a parameter no method arm used, with a value that matters (not 0).
    fn unused(&self) -> Option<&str> {
        let used = self.used.borrow();
        self.values.iter().find(|v| !used.contains(&v.0) && v.2 != 0.0).map(|v| v.1.as_str())
    }
}

/// The 3 or 7 `+towgs84` parameters of a `BOUNDCRS`'s transformation to WGS 84: `None` when
/// it is to another CRS, or does not shift.
fn towgs84(root: &Node) -> Result<Option<Vec<f64>>> {
    let target = root.child("TARGETCRS").and_then(first_node);
    let to_wgs84 = target.is_some_and(|t| {
        t.epsg() == Some(4326)
            || ["DATUM", "ENSEMBLE"]
                .iter()
                .filter_map(|k| t.child(k))
                .any(|d| norm(d.name().unwrap_or("")).starts_with("worldgeodeticsystem1984"))
    });
    let Some(t) = root.child("ABRIDGEDTRANSFORMATION") else { return Ok(None) };
    // A transformation to another CRS has no `+towgs84`: the source CRS is kept, and its datum
    // identified as without a BOUNDCRS (GDAL, too, reads the source CRS of such a BOUNDCRS).
    if !to_wgs84 {
        return Ok(None);
    }
    let method = t.child("METHOD").map(|m| (norm(m.name().unwrap_or("")), m.epsg())).unwrap_or_default();
    // So is one that is not a 3 or 7 parameter Helmert transformation (a grid's, NTv2 or
    // NADCON, whose file is a PARAMETERFILE): geocentric translations, position vector and
    // coordinate frame, in the geog2D, geocentric and geog3D domains.
    let helmert = ["geocentrictranslations", "positionvector", "coordinateframe"].iter().any(|m| method.0.starts_with(m))
        || matches!(method.1, Some(9603 | 1031 | 1035 | 9606 | 1033 | 1037 | 9607 | 1032 | 1038));
    if !helmert {
        return Ok(None);
    }
    // Coordinate frame rotations turn the other way from PROJ's (position vector) `+towgs84`.
    let frame = method.0.contains("coordinateframe") || matches!(method.1, Some(9607 | 1032 | 1038));
    let mut v = [0.0; 7];
    let mut seven = false;
    for p in t.children("PARAMETER") {
        let name = norm(p.name().unwrap_or(""));
        let x = p.num(1).ok_or_else(|| bad("a transformation parameter without a value"))?;
        let unit = ["ANGLEUNIT", "LENGTHUNIT", "SCALEUNIT", "UNIT"].iter().find_map(|k| p.child(k)).and_then(|u| u.num(1));
        let code = p.epsg();
        let i = match (name.as_str(), code) {
            ("xaxistranslation", _) | (_, Some(8605)) => 0,
            ("yaxistranslation", _) | (_, Some(8606)) => 1,
            ("zaxistranslation", _) | (_, Some(8607)) => 2,
            ("xaxisrotation", _) | (_, Some(8608)) => 3,
            ("yaxisrotation", _) | (_, Some(8609)) => 4,
            ("zaxisrotation", _) | (_, Some(8610)) => 5,
            ("scaledifference", _) | (_, Some(8611)) => 6,
            _ => return Err(bad(&format!("transformation parameter '{}'", p.name().unwrap_or("")))),
        };
        v[i] = match i {
            0..=2 => x * unit.unwrap_or(1.0),
            // Arc-seconds, and parts per million.
            3..=5 => x * unit.map(|r| r.to_degrees() * 3600.0).unwrap_or(1.0) * if frame { -1.0 } else { 1.0 },
            // A scale difference in its unit, or without one as the ratio PROJ writes
            // (1.0000067 for 6.7 ppm): any other unit-less value is ambiguous.
            _ => match unit {
                Some(u) => x * u * 1e6,
                None if (x - 1.0).abs() < 0.01 => (x - 1.0) * 1e6,
                None => return Err(bad("a scale difference without its unit")),
            },
        };
        seven |= i >= 3;
    }
    if v.iter().all(|x| *x == 0.0) {
        return Ok(None);
    }
    Ok(Some(if seven { v.to_vec() } else { v[..3].to_vec() }))
}
