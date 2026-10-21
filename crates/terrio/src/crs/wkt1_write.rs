//! The WKT 1 of a PROJ string, as PROJ writes it for GDAL (`WKT1_GDAL`): built from terrio's
//! own reading of the PROJ string, with PROJ's default for every parameter the string omits.
//! What WKT 1 cannot describe (a projection without a WKT 1 method, a datum grid, a parameter
//! or axis order its method has no node for) is kept in the PROJ string itself, in an
//! `EXTENSION["PROJ4", ...]` that GDAL and PROJ read instead of the nodes, with GDAL's
//! `custom_proj4` method when WKT 1 has none.

use super::def::{Def, datum_geog};
use super::math::{Ell, prime_meridian, unit_to_meter};
use super::params::Params;
use super::{SpatialRef, epsg};
use std::fmt::Write as _;

/// The parameters the nodes describe. Any other (`+approx`, `+no_rot`, `+czech`, `+guam`,
/// `+geoidgrids`...) is kept in the PROJ4 extension.
const DESCRIBED: &[&str] = &[
    "proj", "lat_0", "lon_0", "k", "k_0", "x_0", "y_0", "lat_1", "lat_2", "lat_ts", "lonc", "alpha", "gamma", "no_uoff", "no_off", "zone",
    "south", "h", "sweep", "azi", "ellps", "datum", "a", "b", "rf", "f", "R", "towgs84", "pm", "units", "to_meter", "axis", "no_defs",
    "wktext", "type", "nadgrids",
];

/// `(PROJ name, EPSG name, EPSG code)` of the ellipsoids GDAL names.
const ELLIPSOIDS: &[(&str, &str, u32)] = &[
    ("WGS84", "WGS 84", 7030),
    ("GRS80", "GRS 1980", 7019),
    ("WGS72", "WGS 72", 7043),
    ("bessel", "Bessel 1841", 7004),
    ("intl", "International 1924", 7022),
    ("clrk66", "Clarke 1866", 7008),
    ("clrk80", "Clarke 1880 (RGS)", 7012),
    ("clrk80ign", "Clarke 1880 (IGN)", 7011),
    ("airy", "Airy 1830", 7001),
    ("mod_airy", "Airy Modified 1849", 7002),
    ("krass", "Krassowsky 1940", 7024),
    ("aust_SA", "Australian National Spheroid", 7003),
    ("helmert", "Helmert 1906", 7020),
    ("GRS67", "GRS 1967", 7036),
    ("evrst30", "Everest 1830 (1937 Adjustment)", 7015),
];

/// The axes of an east-north projected CS.
const EAST_NORTH: &str = ",AXIS[\"Easting\",EAST],AXIS[\"Northing\",NORTH]";

/// A WKT 1 projection method: its name, its parameters (in degrees and the CRS's linear unit),
/// the projected CS's axes as GDAL writes them, and whether the nodes describe the PROJ
/// string's axis order.
struct Method {
    name: &'static str,
    params: Vec<(&'static str, f64)>,
    axes: &'static str,
    axis_described: bool,
}

/// The GDAL WKT 1 of a PROJ string. `None` when terrio cannot read the PROJ string.
pub(crate) fn from_proj(s: &str) -> Option<String> {
    let own = Def::from_proj(s).ok()?;
    let p = Params::parse(s);
    let extension = format!("EXTENSION[\"PROJ4\",\"{}\"]", s.trim().replace('"', ""));
    let (geogcs, datum_described) = geogcs(&p, &own);
    let described = datum_described && p.keys().all(|k| DESCRIBED.contains(&k));
    if own.is_rotated_pole() {
        // Its base geographic CRS extended with the PROJ string, as GDAL 2 wrote it. Unnamed and
        // without its code, so as not to be taken for the base CRS.
        let body = geogcs.strip_prefix("GEOGCS[\"")?;
        let body = body[body.find('"')? + 1..].strip_suffix(']')?;
        // The root's own AUTHORITY: the last node, closed by the root's bracket.
        let body = match body.rfind(",AUTHORITY[") {
            Some(i) if body[i..].matches(']').count() == 1 => &body[..i],
            _ => body,
        };
        return Some(format!("GEOGCS[\"unnamed\"{body},{extension}]"));
    }
    let wkt = if own.is_geographic() {
        match epsg_geographic(&p) {
            Some(e) => e.wkt.clone(),
            None => format!("{},AXIS[\"Longitude\",EAST],AXIS[\"Latitude\",NORTH]]", geogcs.strip_suffix(']')?),
        }
    } else {
        let unit = unit(&p, own.to_meter());
        match method(&p, own.to_meter()) {
            Some(m) => {
                let mut w = format!("PROJCS[\"unknown\",{geogcs},PROJECTION[\"{}\"]", m.name);
                for (k, v) in &m.params {
                    let _ = write!(w, ",PARAMETER[\"{k}\",{}]", num(*v));
                }
                let _ = write!(w, ",{unit}");
                w.push_str(m.axes);
                w.push(']');
                if !m.axis_described {
                    return Some(extended(&w, &extension));
                }
                w
            }
            // GDAL's `custom_proj4`: the PROJ string on its datum.
            None => {
                let ext = format!("EXTENSION[\"PROJ4\",\"{}\"]", custom(s));
                return Some(format!("PROJCS[\"unknown\",{geogcs},PROJECTION[\"custom_proj4\"],{unit}{EAST_NORTH},{ext}]"));
            }
        }
    };
    // Faithful when terrio reads it back as the PROJ string: the same conversion on the same
    // datum (its EPSG code identified, its geographic CRS).
    let faithful = described && Def::new(&SpatialRef::from_wkt(&wkt)).is_ok_and(|read| read.same_conversion(&own) && read.same_datum(&own));
    Some(if faithful { wkt } else { extended(&wkt, &extension) })
}

/// A PROJ string as its WKT 1 describes it, for formats that describe CRSs by parameters
/// (GeoTIFF's GeoKeys): its method and parameters (in degrees and its linear unit), its linear
/// unit and datum.
pub(crate) struct Described {
    /// GDAL's WKT 1 method; `None` for a geographic CRS.
    pub method: Option<&'static str>,
    pub params: Vec<(&'static str, f64)>,
    /// Metres per linear unit, and its EPSG code when GeoTIFF has one.
    pub to_meter: f64,
    pub unit_code: Option<u32>,
    /// The EPSG geographic CRS, else the ellipsoid (its EPSG code when GDAL names it, semi-major
    /// axis and inverse flattening, 0 for a sphere), prime meridian and datum shift.
    pub geog: Option<u32>,
    pub ellipsoid: Option<u32>,
    pub a: f64,
    pub rf: f64,
    pub pm: f64,
    pub towgs84: Option<Vec<f64>>,
}

/// The description of a PROJ string whose WKT 1 describes it whole (without the PROJ4
/// extension), with east-north axes. `None` otherwise.
pub(crate) fn describe(s: &str) -> Option<Described> {
    let wkt = from_proj(s)?;
    if wkt.contains("EXTENSION[") {
        return None;
    }
    let own = Def::from_proj(s).ok()?;
    let p = Params::parse(s);
    let method = match own.is_geographic() {
        true => None,
        false => Some(method(&p, own.to_meter()).filter(|m| m.axis_described && p.str("axis").is_none_or(|a| a.starts_with("en")))?),
    };
    let ell = own.ell;
    let named = p.str("ellps").or_else(|| p.str("datum").and_then(super::def::datum_ellps));
    let ellipsoid = named.and_then(|n| ELLIPSOIDS.iter().find(|e| e.0 == n)).filter(|e| Ell::named(e.0).is_some_and(|n| n == ell));
    // The units GDAL reads in GeoKeys, by size.
    let to_meter = own.to_meter();
    const UNITS: [(f64, u32); 4] = [(1.0, 9001), (0.3048, 9002), (1200.0 / 3937.0, 9003), (1000.0, 9036)];
    let unit_code = UNITS.iter().find(|(m, _)| (to_meter / m - 1.0).abs() < 1e-12).map(|(_, c)| *c);
    Some(Described {
        method: method.as_ref().map(|m| m.name),
        params: method.map(|m| m.params).unwrap_or_default(),
        to_meter: own.to_meter(),
        unit_code,
        geog: epsg_geographic(&p).and(p.str("datum")).and_then(datum_geog),
        ellipsoid: ellipsoid.map(|e| e.2),
        a: ell.a,
        rf: if ell.es == 0.0 { 0.0 } else { num(1.0 / (1.0 - ell.one_es.sqrt())).parse().unwrap_or(0.0) },
        pm: p.str("pm").map(|v| v.parse().ok().or_else(|| prime_meridian(v)).unwrap_or(0.0)).unwrap_or(0.0),
        towgs84: p.list("towgs84").filter(|v| v.len() == 3 || v.len() == 7),
    })
}

/// The GDAL WKT 1 of a projected PROJ string on an EPSG geographic CRS (its `GEOGCS`, which
/// identifies the datum), as GDAL reads GeoKeys that give the geographic CRS by its code. `None`
/// when the code is not a geographic CRS of the EPSG table, or the WKT 1 needs the PROJ string to
/// describe the projection.
pub(crate) fn from_proj_on(s: &str, geog: u32) -> Option<String> {
    let e = epsg::get(geog).filter(|e| e.proj.starts_with("+proj=longlat"))?;
    let wkt = from_proj(s).filter(|w| w.starts_with("PROJCS[") && !w.contains("EXTENSION["))?;
    let start = wkt.find("GEOGCS[")?;
    let mut depth = 0;
    let end = start
        + wkt[start..]
            .char_indices()
            .find(|&(_, c)| {
                depth += match c {
                    '[' => 1,
                    ']' => -1,
                    _ => 0,
                };
                c == ']' && depth == 0
            })?
            .0;
    let mut geogcs = e.wkt.clone();
    while let Some(i) = geogcs.find(",AXIS[") {
        let len = geogcs[i..].find(']')?;
        geogcs.replace_range(i..i + len + 1, "");
    }
    Some(format!("{}{geogcs}{}", &wkt[..start], &wkt[end + 1..]))
}

/// The ellipsoid of an EPSG geographic CRS, as PROJ parameters.
pub(crate) fn epsg_ellipsoid(geog: u32) -> Option<String> {
    let e = epsg::get(geog).filter(|e| e.proj.starts_with("+proj=longlat"))?;
    const ELLIPSOID: [&str; 6] = ["+ellps=", "+a=", "+b=", "+rf=", "+f=", "+R="];
    let tokens: Vec<&str> = e.proj.split_whitespace().filter(|t| ELLIPSOID.iter().any(|p| t.starts_with(p))).collect();
    (!tokens.is_empty()).then(|| tokens.join(" "))
}

/// A WKT 1 with an extension node added to its root.
fn extended(wkt: &str, extension: &str) -> String {
    format!("{},{extension}]", &wkt[..wkt.len() - 1])
}

/// The PROJ string of a `custom_proj4` extension, with `+wktext` and `+no_defs` as GDAL writes
/// it.
fn custom(s: &str) -> String {
    let mut out = s.trim().replace('"', "");
    for flag in ["+wktext", "+no_defs"] {
        if !out.split_whitespace().any(|t| t == flag) {
            out.push(' ');
            out.push_str(flag);
        }
    }
    out
}

/// The `GEOGCS` of a PROJ string, and whether it describes the datum: an EPSG geographic CRS
/// for PROJ's `+datum=` names, else a datum named after its ellipsoid, with its `TOWGS84`. A
/// datum grid (`+nadgrids=`) is not described.
fn geogcs(p: &Params, own: &Def) -> (String, bool) {
    let towgs84 = p.list("towgs84").filter(|v| v.len() == 3 || v.len() == 7);
    let grids = p.str("nadgrids").is_some();
    let pm = match p.str("pm") {
        None => "PRIMEM[\"Greenwich\",0,AUTHORITY[\"EPSG\",\"8901\"]]".to_string(),
        Some(v) => match (v.parse::<f64>(), prime_meridian(v)) {
            (Ok(deg), _) => format!("PRIMEM[\"unknown\",{}]", num(deg)),
            (_, Some(deg)) => {
                let mut name = v.to_string();
                name[..1].make_ascii_uppercase();
                format!("PRIMEM[\"{name}\",{}]", num(deg))
            }
            _ => "PRIMEM[\"Greenwich\",0,AUTHORITY[\"EPSG\",\"8901\"]]".to_string(),
        },
    };
    const DEGREE: &str = "UNIT[\"degree\",0.0174532925199433,AUTHORITY[\"EPSG\",\"9122\"]]";
    if let Some(e) = epsg_geographic(p) {
        let mut w = e.wkt.clone();
        while let Some(i) = w.find(",AXIS[") {
            let Some(len) = w[i..].find(']') else { break };
            w.replace_range(i..i + len + 1, "");
        }
        return (w, true);
    }
    let ell = own.ell;
    let rf = if ell.es == 0.0 { 0.0 } else { 1.0 / (1.0 - ell.one_es.sqrt()) };
    let named = p.str("ellps").or_else(|| p.str("datum").and_then(super::def::datum_ellps));
    let known = named.and_then(|n| ELLIPSOIDS.iter().find(|e| e.0 == n)).filter(|e| Ell::named(e.0).is_some_and(|n| n == ell));
    let (spheroid, datum) = match known {
        Some((_, name, code)) => (
            format!("SPHEROID[\"{name}\",{},{},AUTHORITY[\"EPSG\",\"{code}\"]]", num(ell.a), num(rf)),
            format!("Unknown based on {name} ellipsoid"),
        ),
        None => (format!("SPHEROID[\"unknown\",{},{}]", num(ell.a), num(rf)), "unknown".to_string()),
    };
    let (datum, shift) = match &towgs84 {
        Some(t) => {
            let list = |v: &[f64]| v.iter().map(|x| num(*x)).collect::<Vec<_>>().join(",");
            let mut seven = t.clone();
            seven.resize(7, 0.0);
            (format!("{datum} using towgs84={}", list(t)), format!(",TOWGS84[{}]", list(&seven)))
        }
        None => (datum, String::new()),
    };
    (format!("GEOGCS[\"unknown\",DATUM[\"{datum}\",{spheroid}{shift}],{pm},{DEGREE}]"), !grids)
}

/// The EPSG geographic CRS of a PROJ string's `+datum=`, on Greenwich and without another
/// datum shift.
fn epsg_geographic(p: &Params) -> Option<&'static epsg::Entry> {
    let other = p.has("towgs84") || p.has("nadgrids") || p.has("pm");
    p.str("datum").and_then(datum_geog).filter(|_| !other).and_then(epsg::get)
}

/// A number as GDAL writes it: to 15 significant digits.
fn num(v: f64) -> String {
    format!("{:.14e}", v).parse::<f64>().map(|r| r.to_string()).unwrap_or_else(|_| v.to_string())
}

/// The `UNIT` of a projected CRS's linear unit, as GDAL names it.
fn unit(p: &Params, to_meter: f64) -> String {
    let named = |name: &str, size: &str, code: u32| format!("UNIT[\"{name}\",{size},AUTHORITY[\"EPSG\",\"{code}\"]]");
    let units = p.str("units").filter(|u| p.str("to_meter").is_none() && unit_to_meter(u) == Some(to_meter));
    match units {
        Some("m") | None if to_meter == 1.0 => named("metre", "1", 9001),
        Some("us-ft") => named("US survey foot", "0.304800609601219", 9003),
        Some("ft") => named("foot", "0.3048", 9002),
        Some("km") => named("kilometre", "1000", 9036),
        _ => format!("UNIT[\"unknown\",{}]", num(to_meter)),
    }
}

/// The WKT 1 method of a PROJ string's projection, with PROJ's defaults. `None` when WKT 1 has
/// none, or when PROJ's CRSs read the PROJ string otherwise than PROJ's projection does.
fn method(p: &Params, to_meter: f64) -> Option<Method> {
    let deg = |k: &str| p.angle(k).map(f64::to_degrees);
    let lat_0 = deg("lat_0").unwrap_or(0.0);
    let lon_0 = deg("lon_0").unwrap_or(0.0);
    let k = p.f64("k_0").or_else(|| p.f64("k")).unwrap_or(1.0);
    let fe = p.f64("x_0").unwrap_or(0.0) / to_meter;
    let fn_ = p.f64("y_0").unwrap_or(0.0) / to_meter;
    let axis = p.str("axis").map(|a| a.get(..2).unwrap_or(a)).unwrap_or("en");
    let en = axis == "en";
    let m = |name: &'static str, params: Vec<(&'static str, f64)>| Method { name, params, axes: EAST_NORTH, axis_described: en };
    let origin = |lat: &'static str, lon: &'static str| vec![(lat, lat_0), (lon, lon_0), ("false_easting", fe), ("false_northing", fn_)];
    let lon_only = |lon: &'static str| vec![(lon, lon_0), ("false_easting", fe), ("false_northing", fn_)];
    let tm = |lat: f64, lon: f64, k: f64, fe: f64, fn_: f64| {
        vec![("latitude_of_origin", lat), ("central_meridian", lon), ("scale_factor", k), ("false_easting", fe), ("false_northing", fn_)]
    };
    Some(match p.str("proj")? {
        "tmerc" | "etmerc" if axis == "ws" => {
            Method { name: "Transverse_Mercator_South_Orientated", params: tm(lat_0, lon_0, k, fe, fn_), axes: "", axis_described: true }
        }
        "tmerc" | "etmerc" => m("Transverse_Mercator", tm(lat_0, lon_0, k, fe, fn_)),
        "utm" => {
            let zone = p.f64("zone")?.trunc();
            let fn_ = if p.has("south") { 10_000_000.0 } else { 0.0 };
            m("Transverse_Mercator", tm(0.0, zone * 6.0 - 183.0, 0.9996, 500_000.0 / to_meter, fn_ / to_meter))
        }
        "lcc" => {
            let lat_1 = deg("lat_1").unwrap_or(0.0);
            // With one standard parallel and no latitude of origin, PROJ's `lcc` (and terrio) puts
            // the origin on the parallel, PROJ's CRSs on the equator with a second parallel there:
            // written as the PROJ string.
            let lat_2 = match deg("lat_2") {
                Some(v) => v,
                None if p.has("lat_0") => lat_1,
                None => return None,
            };
            if lat_1 == lat_2 && lat_0 == lat_1 {
                m("Lambert_Conformal_Conic_1SP", tm(lat_0, lon_0, k, fe, fn_))
            } else if k == 1.0 {
                m(
                    "Lambert_Conformal_Conic_2SP",
                    vec![
                        ("latitude_of_origin", lat_0),
                        ("central_meridian", lon_0),
                        ("standard_parallel_1", lat_1),
                        ("standard_parallel_2", lat_2),
                        ("false_easting", fe),
                        ("false_northing", fn_),
                    ],
                )
            } else {
                return None;
            }
        }
        "merc" => match deg("lat_ts") {
            Some(ts) => m(
                "Mercator_2SP",
                vec![("standard_parallel_1", ts), ("central_meridian", lon_0), ("false_easting", fe), ("false_northing", fn_)],
            ),
            None => {
                m("Mercator_1SP", vec![("central_meridian", lon_0), ("scale_factor", k), ("false_easting", fe), ("false_northing", fn_)])
            }
        },
        name @ ("aea" | "eqdc") => m(
            if name == "aea" { "Albers_Conic_Equal_Area" } else { "Equidistant_Conic" },
            vec![
                ("latitude_of_center", lat_0),
                ("longitude_of_center", lon_0),
                ("standard_parallel_1", deg("lat_1").unwrap_or(0.0)),
                ("standard_parallel_2", deg("lat_2").unwrap_or(0.0)),
                ("false_easting", fe),
                ("false_northing", fn_),
            ],
        ),
        "laea" => m("Lambert_Azimuthal_Equal_Area", origin("latitude_of_center", "longitude_of_center")),
        "aeqd" => m("Azimuthal_Equidistant", origin("latitude_of_center", "longitude_of_center")),
        "stere" if (lat_0.abs() - 90.0).abs() < 1e-10 => {
            let params = match deg("lat_ts").filter(|ts| (ts.abs() - 90.0).abs() > 1e-10) {
                Some(ts) => vec![("latitude_of_origin", ts), ("central_meridian", lon_0), ("false_easting", fe), ("false_northing", fn_)],
                None => tm(lat_0, lon_0, k, fe, fn_),
            };
            Method { name: "Polar_Stereographic", params, axes: "", axis_described: en }
        }
        "ups" => {
            let lat = if p.has("south") { -90.0 } else { 90.0 };
            let params = tm(lat, 0.0, 0.994, 2_000_000.0 / to_meter, 2_000_000.0 / to_meter);
            Method { name: "Polar_Stereographic", params, axes: "", axis_described: en }
        }
        "stere" => m("Stereographic", tm(lat_0, lon_0, k, fe, fn_)),
        "sterea" => m("Oblique_Stereographic", tm(lat_0, lon_0, k, fe, fn_)),
        "gstmerc" => m("Gauss_Schreiber_Transverse_Mercator", tm(lat_0, lon_0, k, fe, fn_)),
        // GDAL's Swiss oblique Mercator: an oblique Mercator of azimuth 90°.
        "somerc" => m(
            "Hotine_Oblique_Mercator_Azimuth_Center",
            vec![
                ("latitude_of_center", lat_0),
                ("longitude_of_center", lon_0),
                ("azimuth", 90.0),
                ("rectified_grid_angle", 90.0),
                ("scale_factor", k),
                ("false_easting", fe),
                ("false_northing", fn_),
            ],
        ),
        "omerc" => {
            let alpha = deg("alpha")?;
            let variant_a = p.has("no_uoff") || p.has("no_off");
            m(
                if variant_a { "Hotine_Oblique_Mercator" } else { "Hotine_Oblique_Mercator_Azimuth_Center" },
                vec![
                    ("latitude_of_center", lat_0),
                    ("longitude_of_center", deg("lonc").unwrap_or(0.0)),
                    ("azimuth", alpha),
                    ("rectified_grid_angle", deg("gamma").unwrap_or(alpha)),
                    ("scale_factor", k),
                    ("false_easting", fe),
                    ("false_northing", fn_),
                ],
            )
        }
        "labrd" => m(
            "Laborde_Oblique_Mercator",
            vec![
                ("latitude_of_center", lat_0),
                ("longitude_of_center", lon_0),
                ("azimuth", deg("azi").unwrap_or(0.0)),
                ("scale_factor", k),
                ("false_easting", fe),
                ("false_northing", fn_),
            ],
        ),
        // Krovak with PROJ's defaults, east-north or south-west (`+axis=swu`).
        "krovak" => Method {
            name: "Krovak",
            params: vec![
                ("latitude_of_center", deg("lat_0").unwrap_or(49.5)),
                ("longitude_of_center", deg("lon_0").unwrap_or(24.833333333333332)),
                ("azimuth", deg("alpha").unwrap_or(30.28813975277778)),
                ("pseudo_standard_parallel_1", deg("lat_ts").unwrap_or(78.5)),
                ("scale_factor", p.f64("k_0").or_else(|| p.f64("k")).unwrap_or(0.9999)),
                ("false_easting", fe),
                ("false_northing", fn_),
            ],
            axes: if axis == "sw" { ",AXIS[\"Southing\",SOUTH],AXIS[\"Westing\",WEST]" } else { EAST_NORTH },
            axis_described: en || axis == "sw",
        },
        // PROJ's New Zealand Map Grid has a fixed origin.
        "nzmg" => m(
            "New_Zealand_Map_Grid",
            vec![
                ("latitude_of_origin", -41.0),
                ("central_meridian", 173.0),
                ("false_easting", 2_510_000.0 / to_meter),
                ("false_northing", 6_023_150.0 / to_meter),
            ],
        ),
        "cass" => m("Cassini_Soldner", origin("latitude_of_origin", "central_meridian")),
        "poly" => m("Polyconic", origin("latitude_of_origin", "central_meridian")),
        "gnom" => m("Gnomonic", origin("latitude_of_origin", "central_meridian")),
        "ortho" => m("Orthographic", origin("latitude_of_origin", "central_meridian")),
        "eqc" => m(
            "Equirectangular",
            vec![
                ("standard_parallel_1", deg("lat_ts").unwrap_or(0.0)),
                ("latitude_of_origin", lat_0),
                ("central_meridian", lon_0),
                ("false_easting", fe),
                ("false_northing", fn_),
            ],
        ),
        // A cylindrical equal-area projection given by its scale instead has no WKT 1 method.
        "cea" if p.has("lat_ts") || k == 1.0 => m(
            "Cylindrical_Equal_Area",
            vec![
                ("standard_parallel_1", deg("lat_ts").unwrap_or(0.0)),
                ("central_meridian", lon_0),
                ("false_easting", fe),
                ("false_northing", fn_),
            ],
        ),
        "bonne" => m(
            "Bonne",
            vec![
                ("standard_parallel_1", deg("lat_1").unwrap_or(0.0)),
                ("central_meridian", lon_0),
                ("false_easting", fe),
                ("false_northing", fn_),
            ],
        ),
        "loxim" => m(
            "Loximuthal",
            vec![
                ("latitude_of_origin", deg("lat_1").unwrap_or(0.0)),
                ("central_meridian", lon_0),
                ("false_easting", fe),
                ("false_northing", fn_),
            ],
        ),
        "geos" => Method {
            name: "Geostationary_Satellite",
            params: vec![("central_meridian", lon_0), ("satellite_height", p.f64("h")?), ("false_easting", fe), ("false_northing", fn_)],
            axes: EAST_NORTH,
            // The sweep axis is only in the PROJ string.
            axis_described: en && p.str("sweep").is_none_or(|s| s == "y"),
        },
        name @ ("wintri" | "wink1" | "wink2") => {
            let (method, parallel) = match name {
                "wintri" => ("Winkel_Tripel", "lat_1"),
                "wink1" => ("Winkel_I", "lat_ts"),
                _ => ("Winkel_II", "lat_1"),
            };
            m(
                method,
                vec![
                    ("central_meridian", lon_0),
                    ("standard_parallel_1", deg(parallel).unwrap_or(0.0)),
                    ("false_easting", fe),
                    ("false_northing", fn_),
                ],
            )
        }
        "sinu" => m("Sinusoidal", lon_only("longitude_of_center")),
        "mill" => m("Miller_Cylindrical", lon_only("longitude_of_center")),
        "robin" => m("Robinson", lon_only("longitude_of_center")),
        name => {
            let (_, method) = WORLD.iter().find(|(n, _)| *n == name)?;
            m(method, lon_only("central_meridian"))
        }
    })
}

/// World projections of a central meridian, by PROJ name, and their WKT 1 methods.
pub(crate) const WORLD: [(&str, &str); 12] = [
    ("vandg", "VanDerGrinten"),
    ("eqearth", "Equal_Earth"),
    ("moll", "Mollweide"),
    ("eck1", "Eckert_I"),
    ("eck2", "Eckert_II"),
    ("eck3", "Eckert_III"),
    ("eck4", "Eckert_IV"),
    ("eck5", "Eckert_V"),
    ("eck6", "Eckert_VI"),
    ("gall", "Gall_Stereographic"),
    ("aitoff", "Aitoff"),
    ("natearth", "Natural_Earth"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_gdal_methods_and_parameters() {
        let w = from_proj("+proj=merc +lat_ts=33 +lon_0=-100 +ellps=WGS84 +units=m").unwrap();
        assert!(w.contains("PROJECTION[\"Mercator_2SP\"],PARAMETER[\"standard_parallel_1\",33]"), "{w}");
        assert!(w.contains("DATUM[\"Unknown based on WGS 84 ellipsoid\""), "{w}");
        let w = from_proj("+proj=krovak +axis=swu +ellps=bessel +units=m").unwrap();
        assert!(
            w.contains("PARAMETER[\"latitude_of_center\",49.5]") && w.contains("AXIS[\"Southing\",SOUTH]") && !w.contains("EXTENSION"),
            "{w}"
        );
        let w = from_proj("+proj=tmerc +lon_0=-100 +ellps=WGS84 +towgs84=100,50,-30 +pm=ferro +units=us-ft").unwrap();
        assert!(w.contains("TOWGS84[100,50,-30,0,0,0,0]") && w.contains("PRIMEM[\"Ferro\",-17.666666666667]"), "{w}");
        assert!(w.contains("UNIT[\"US survey foot\",0.304800609601219"), "{w}");
        let w = from_proj("+proj=hammer +ellps=WGS84 +units=m").unwrap();
        assert!(w.contains("PROJECTION[\"custom_proj4\"]") && w.contains("+proj=hammer +ellps=WGS84 +units=m +wktext +no_defs"), "{w}");
    }
}
