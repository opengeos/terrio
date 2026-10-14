//! PROJJSON (<https://proj.org/specifications/projjson.html>), the CRS encoding GeoParquet asks
//! for, converted from and to OGC WKT1.
//!
//! WKT1 is what terrio carries for a CRS (built from the EPSG registry as GDAL writes it, or
//! stored as a file had it, including Esri's dialect), so PROJJSON is built from a WKT1 tree:
//! geographic and projected CRSs on the projection methods GDAL names in WKT1, with EPSG method
//! and parameter names and codes, so that PROJ (and GDAL, pyproj, GeoPandas) read the result as
//! the same CRS. The reverse conversion lets CRSs without an EPSG code round-trip.

use serde_json::{Map, Value, json};
use std::fmt::Write as _;

/// A WKT1 node: `KEYWORD[arg, ...]`.
#[derive(Debug, Clone, PartialEq)]
enum Arg {
    Str(String),
    Num(f64),
    Node(Node),
    /// A bare word, such as an axis direction (`NORTH`).
    Word(String),
}

#[derive(Debug, Clone, PartialEq)]
struct Node {
    key: String,
    args: Vec<Arg>,
}

impl Node {
    fn child<'a>(&'a self, key: &'a str) -> Option<&'a Node> {
        self.children(key).next()
    }

    fn children<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.args.iter().filter_map(move |a| match a {
            Arg::Node(n) if n.key.eq_ignore_ascii_case(key) => Some(n),
            _ => None,
        })
    }

    fn str(&self, i: usize) -> Option<&str> {
        match self.args.get(i)? {
            Arg::Str(s) | Arg::Word(s) => Some(s),
            _ => None,
        }
    }

    fn num(&self, i: usize) -> Option<f64> {
        match self.args.get(i)? {
            Arg::Num(v) => Some(*v),
            Arg::Str(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    fn name(&self) -> &str {
        self.str(0).unwrap_or_default()
    }

    /// The `AUTHORITY["EPSG","code"]` code.
    fn epsg(&self) -> Option<u32> {
        let a = self.child("AUTHORITY")?;
        a.str(0).filter(|s| s.eq_ignore_ascii_case("EPSG"))?;
        a.str(1).and_then(|c| c.trim().parse().ok()).or_else(|| a.num(1).map(|v| v as u32))
    }
}

/// Parses WKT1 text into its root node.
fn parse(text: &str) -> Option<Node> {
    let mut p = Parser { s: text.as_bytes(), i: 0 };
    let n = p.node()?;
    p.ws();
    (p.i == p.s.len()).then_some(n)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn word(&mut self) -> String {
        self.ws();
        let start = self.i;
        while self.i < self.s.len() && (self.s[self.i].is_ascii_alphanumeric() || self.s[self.i] == b'_') {
            self.i += 1;
        }
        String::from_utf8_lossy(&self.s[start..self.i]).into_owned()
    }

    fn node(&mut self) -> Option<Node> {
        let key = self.word();
        if key.is_empty() {
            return None;
        }
        self.ws();
        if !matches!(self.s.get(self.i), Some(b'[' | b'(')) {
            return None;
        }
        self.i += 1;
        let mut args = Vec::new();
        loop {
            self.ws();
            match *self.s.get(self.i)? {
                b']' | b')' => {
                    self.i += 1;
                    return Some(Node { key, args });
                }
                b',' => self.i += 1,
                b'"' => {
                    self.i += 1;
                    let mut out = Vec::new();
                    loop {
                        let c = *self.s.get(self.i)?;
                        self.i += 1;
                        if c == b'"' {
                            if self.s.get(self.i) == Some(&b'"') {
                                out.push(b'"');
                                self.i += 1;
                                continue;
                            }
                            break;
                        }
                        out.push(c);
                    }
                    args.push(Arg::Str(String::from_utf8_lossy(&out).into_owned()));
                }
                c if c == b'-' || c == b'+' || c == b'.' || c.is_ascii_digit() => {
                    let start = self.i;
                    while self.i < self.s.len() && matches!(self.s[self.i], b'0'..=b'9' | b'.' | b'-' | b'+' | b'e' | b'E') {
                        self.i += 1;
                    }
                    args.push(Arg::Num(std::str::from_utf8(&self.s[start..self.i]).ok()?.parse().ok()?));
                }
                _ => {
                    let save = self.i;
                    let w = self.word();
                    self.ws();
                    if matches!(self.s.get(self.i), Some(b'[' | b'(')) {
                        self.i = save;
                        args.push(Arg::Node(self.node()?));
                    } else if w.is_empty() {
                        return None;
                    } else {
                        args.push(Arg::Word(w));
                    }
                }
            }
        }
    }
}

/// `(WKT1 datum name, EPSG datum name, datum code, ellipsoid EPSG name)` for datums whose WKT1
/// names (GDAL's and Esri's) differ from their EPSG names.
const DATUMS: &[(&[&str], &str, u32)] = &[
    (&["WGS_1984", "D_WGS_1984", "WGS84", "WGS 84", "World Geodetic System 1984"], "World Geodetic System 1984", 6326),
    (&["North_American_Datum_1983", "D_North_American_1983", "NAD83"], "North American Datum 1983", 6269),
    (&["North_American_Datum_1927", "D_North_American_1927", "NAD27"], "North American Datum 1927", 6267),
    (&["European_Terrestrial_Reference_System_1989", "D_ETRS_1989", "ETRS89"], "European Terrestrial Reference System 1989", 6258),
    (&["European_Datum_1950", "D_European_1950", "ED50"], "European Datum 1950", 6230),
    (&["Geocentric_Datum_of_Australia_1994", "D_GDA_1994", "GDA94"], "Geocentric Datum of Australia 1994", 6283),
    (&["OSGB_1936", "Ordnance_Survey_of_Great_Britain_1936", "D_OSGB_1936", "OSGB36"], "Ordnance Survey of Great Britain 1936", 6277),
];

/// Datum ensembles: `(datum code, accuracy, member codes, member names)`.
const ENSEMBLES: &[(u32, &str, &[u32], &[&str])] = &[
    (
        6326,
        "2.0",
        &[1166, 1152, 1153, 1154, 1155, 1156, 1309, 1383],
        &[
            "World Geodetic System 1984 (Transit)",
            "World Geodetic System 1984 (G730)",
            "World Geodetic System 1984 (G873)",
            "World Geodetic System 1984 (G1150)",
            "World Geodetic System 1984 (G1674)",
            "World Geodetic System 1984 (G1762)",
            "World Geodetic System 1984 (G2139)",
            "World Geodetic System 1984 (G2296)",
        ],
    ),
    (
        6258,
        "0.1",
        &[1178, 1179, 1180, 1181, 1182, 1183, 1184, 1185, 1186, 1204, 1206, 1382],
        &[
            "European Terrestrial Reference Frame 1989",
            "European Terrestrial Reference Frame 1990",
            "European Terrestrial Reference Frame 1991",
            "European Terrestrial Reference Frame 1992",
            "European Terrestrial Reference Frame 1993",
            "European Terrestrial Reference Frame 1994",
            "European Terrestrial Reference Frame 1996",
            "European Terrestrial Reference Frame 1997",
            "European Terrestrial Reference Frame 2000",
            "European Terrestrial Reference Frame 2005",
            "European Terrestrial Reference Frame 2014",
            "European Terrestrial Reference Frame 2020",
        ],
    ),
];

/// An EPSG parameter: `(name, code, kind)` where kind is `a`ngle, `s`cale or `l`ength.
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
const AZIMUTH: Param = ("Azimuth of initial line", 8813, 'a');
const GAMMA: Param = ("Angle from Rectified to Skew Grid", 8814, 'a');
const K_PC: Param = ("Scale factor on initial line", 8815, 's');
const E_PC: Param = ("Easting at projection centre", 8816, 'l');
const N_PC: Param = ("Northing at projection centre", 8817, 'l');
const LAT_SP: Param = ("Latitude of standard parallel", 8832, 'a');
const LON_ORIGIN: Param = ("Longitude of origin", 8833, 'a');

/// A projection method: `(EPSG name, code (0 for PROJ's own), [(WKT1 names, parameter)])`.
type Method = (&'static str, u32, &'static [(&'static [&'static str], Param)]);

const LAT0: &[&str] = &["latitude_of_origin", "latitude_of_center"];
const LON0: &[&str] = &["central_meridian", "longitude_of_center", "longitude_of_origin"];
const K: &[&str] = &["scale_factor"];
const X0: &[&str] = &["false_easting"];
const Y0: &[&str] = &["false_northing"];
const P1: &[&str] = &["standard_parallel_1"];
const P2: &[&str] = &["standard_parallel_2"];

fn method(wkt_name: &str, node: &Node) -> Option<Method> {
    let n = wkt_name.to_ascii_lowercase();
    let has = |p: &str| node.children("PARAMETER").any(|x| x.name().eq_ignore_ascii_case(p));
    let natural: &'static [(&[&str], Param)] = &[(LAT0, LAT_NAT), (LON0, LON_NAT), (K, K_NAT), (X0, FE), (Y0, FN)];
    let false_origin: &'static [(&[&str], Param)] = &[(LAT0, LAT_FO), (LON0, LON_FO), (P1, SP1), (P2, SP2), (X0, E_FO), (Y0, N_FO)];
    let azimuthal: &'static [(&[&str], Param)] = &[(LAT0, LAT_NAT), (LON0, LON_NAT), (X0, FE), (Y0, FN)];
    Some(match n.as_str() {
        "transverse_mercator" | "gauss_kruger" => ("Transverse Mercator", 9807, natural),
        "transverse_mercator_south_orientated" => ("Transverse Mercator (South Orientated)", 9808, natural),
        "mercator_1sp" => ("Mercator (variant A)", 9804, natural),
        "mercator" if !has("standard_parallel_1") => ("Mercator (variant A)", 9804, natural),
        "mercator_2sp" | "mercator" => ("Mercator (variant B)", 9805, &[(P1, SP1), (LON0, LON_NAT), (X0, FE), (Y0, FN)]),
        "lambert_conformal_conic_1sp" => ("Lambert Conic Conformal (1SP)", 9801, natural),
        "lambert_conformal_conic" if !has("standard_parallel_2") => ("Lambert Conic Conformal (1SP)", 9801, natural),
        "lambert_conformal_conic_2sp" | "lambert_conformal_conic" => ("Lambert Conic Conformal (2SP)", 9802, false_origin),
        "albers_conic_equal_area" | "albers" => ("Albers Equal Area", 9822, false_origin),
        "lambert_azimuthal_equal_area" => ("Lambert Azimuthal Equal Area", 9820, azimuthal),
        "azimuthal_equidistant" => ("Azimuthal Equidistant", 1125, azimuthal),
        "polar_stereographic" | "stereographic_north_pole" | "stereographic_south_pole" => {
            let lat = node
                .children("PARAMETER")
                .find(|p| ["latitude_of_origin", "standard_parallel_1"].iter().any(|k| p.name().eq_ignore_ascii_case(k)))
                .and_then(|p| p.num(1))
                .unwrap_or(90.0);
            if (lat.abs() - 90.0).abs() < 1e-9 {
                ("Polar Stereographic (variant A)", 9810, natural)
            } else {
                (
                    "Polar Stereographic (variant B)",
                    9829,
                    &[(&["latitude_of_origin", "standard_parallel_1"], LAT_SP), (LON0, LON_ORIGIN), (X0, FE), (Y0, FN)],
                )
            }
        }
        "oblique_stereographic" | "double_stereographic" => ("Oblique Stereographic", 9809, natural),
        "stereographic" => ("Stereographic", 0, natural),
        "equirectangular" | "equidistant_cylindrical" | "plate_carree" => {
            ("Equidistant Cylindrical", 1028, &[(P1, SP1), (LON0, LON_NAT), (X0, FE), (Y0, FN)])
        }
        "sinusoidal" => ("Sinusoidal", 0, &[(LON0, LON_NAT), (X0, FE), (Y0, FN)]),
        "cylindrical_equal_area" => ("Lambert Cylindrical Equal Area", 9835, &[(P1, SP1), (LON0, LON_NAT), (X0, FE), (Y0, FN)]),
        "cassini_soldner" | "cassini" => ("Cassini-Soldner", 9806, azimuthal),
        "hotine_oblique_mercator" => (
            "Hotine Oblique Mercator (variant A)",
            9812,
            &[(LAT0, LAT_PC), (LON0, LON_PC), (&["azimuth"], AZIMUTH), (&["rectified_grid_angle"], GAMMA), (K, K_PC), (X0, FE), (Y0, FN)],
        ),
        "hotine_oblique_mercator_azimuth_center" => (
            "Hotine Oblique Mercator (variant B)",
            9815,
            &[
                (LAT0, LAT_PC),
                (LON0, LON_PC),
                (&["azimuth"], AZIMUTH),
                (&["rectified_grid_angle"], GAMMA),
                (K, K_PC),
                (X0, E_PC),
                (Y0, N_PC),
            ],
        ),
        _ => return None,
    })
}

fn id(code: u32) -> Value {
    json!({"authority": "EPSG", "code": code})
}

/// A WKT1 `UNIT` node as a PROJJSON unit: the well-known names as strings, others as objects.
fn unit(u: Option<&Node>, kind: &str) -> Value {
    let Some(u) = u else {
        return json!(if kind == "LinearUnit" { "metre" } else { "degree" });
    };
    let factor = u.num(1).unwrap_or(1.0);
    let name = u.name().replace('_', " ");
    if kind == "LinearUnit" && (factor - 1.0).abs() < 1e-12 {
        return json!("metre");
    }
    if kind == "AngularUnit" && (factor - std::f64::consts::PI / 180.0).abs() < 1e-15 {
        return json!("degree");
    }
    let (name, code) = match (kind, u.epsg()) {
        ("LinearUnit", _) if (factor - 0.3048006096012192).abs() < 1e-12 => ("US survey foot".to_string(), Some(9003)),
        ("LinearUnit", _) if (factor - 0.3048).abs() < 1e-12 => ("foot".to_string(), Some(9002)),
        (_, code) => (name, code),
    };
    let mut o = json!({"type": kind, "name": name, "conversion_factor": factor});
    if let Some(c) = code {
        o["id"] = id(c);
    }
    o
}

/// The geographic CRS of a `GEOGCS` node.
fn geographic(g: &Node, lat_first_default: bool) -> Option<Value> {
    let datum = g.child("DATUM")?;
    let sph = datum.child("SPHEROID")?;
    let (a, rf) = (sph.num(1)?, sph.num(2)?);
    let known = DATUMS.iter().find(|d| d.0.iter().any(|n| n.eq_ignore_ascii_case(datum.name())) || datum.epsg() == Some(d.2));
    let mut ellipsoid = json!({"name": sph.name().replace('_', " "), "semi_major_axis": a});
    if rf == 0.0 {
        ellipsoid["semi_minor_axis"] = json!(a);
    } else {
        ellipsoid["inverse_flattening"] = json!(rf);
    }
    if let Some(c) = sph.epsg() {
        ellipsoid["id"] = id(c);
    }
    let datum_name = match known {
        Some(d) => d.1.to_string(),
        None => datum.name().trim_start_matches("D_").replace('_', " "),
    };
    let mut d = Map::new();
    // WGS 84 and ETRS89 are datum ensembles in EPSG, and PROJ only takes them as the same CRS
    // when they are written as such.
    let members = ENSEMBLES.iter().find(|e| known.is_some_and(|k| k.2 == e.0));
    let ensemble = members.is_some();
    match members {
        Some((_, _, first, names)) => {
            d.insert("name".into(), json!(format!("{datum_name} ensemble")));
            let m: Vec<Value> = names.iter().enumerate().map(|(i, n)| json!({"name": n, "id": id(first[i])})).collect();
            d.insert("members".into(), Value::Array(m));
        }
        None => {
            d.insert("type".into(), json!("GeodeticReferenceFrame"));
            d.insert("name".into(), json!(datum_name));
        }
    }
    d.insert("ellipsoid".into(), ellipsoid);
    if let Some((_, accuracy, _, _)) = members {
        d.insert("accuracy".into(), json!(accuracy));
    }
    if let Some(c) = datum.epsg().or(known.map(|k| k.2)) {
        d.insert("id".into(), id(c));
    }
    let angular = unit(g.child("UNIT"), "AngularUnit");
    let pm = g.child("PRIMEM").and_then(|p| p.num(1)).unwrap_or(0.0);
    let mut out = Map::new();
    out.insert("type".into(), json!("GeographicCRS"));
    out.insert("name".into(), json!(g.name()));
    if ensemble {
        out.insert("datum_ensemble".into(), Value::Object(d));
    } else {
        out.insert("datum".into(), Value::Object(d));
    }
    if pm != 0.0 {
        let pm_name = g.child("PRIMEM").map(Node::name).unwrap_or_default();
        out.insert("prime_meridian".into(), json!({"name": pm_name, "longitude": pm}));
    }
    // Axis order: as the WKT says, else latitude first for EPSG CRSs (as EPSG defines them).
    let axes: Vec<&Node> = g.children("AXIS").collect();
    let lat_first = match axes.first() {
        Some(a) => a.str(1).is_some_and(|d| d.eq_ignore_ascii_case("NORTH")),
        None => lat_first_default,
    };
    let lat = json!({"name": "Geodetic latitude", "abbreviation": "Lat", "direction": "north", "unit": angular});
    let lon = json!({"name": "Geodetic longitude", "abbreviation": "Lon", "direction": "east", "unit": angular});
    let axis = if lat_first { vec![lat, lon] } else { vec![lon, lat] };
    out.insert("coordinate_system".into(), json!({"subtype": "ellipsoidal", "axis": axis}));
    if let Some(c) = g.epsg() {
        out.insert("id".into(), id(c));
    }
    Some(Value::Object(out))
}

/// Converts WKT1 to PROJJSON. `code` is the EPSG code the CRS is known by, if any. `None` for
/// what cannot be converted (WKT2, unsupported projection methods).
pub(crate) fn from_wkt1(wkt: &str, code: Option<u32>) -> Option<Value> {
    let root = parse(wkt.trim())?;
    let mut out = match root.key.to_ascii_uppercase().as_str() {
        "GEOGCS" => geographic(&root, root.epsg().or(code).is_some())?,
        "PROJCS" => projected(&root)?,
        _ => return None,
    };
    let o = out.as_object_mut()?;
    if let Some(c) = code.or(root.epsg()) {
        o.insert("id".into(), id(c));
    }
    let mut full = Map::new();
    full.insert("$schema".into(), json!("https://proj.org/schemas/v0.7/projjson.schema.json"));
    full.extend(std::mem::take(o));
    Some(Value::Object(full))
}

fn projected(p: &Node) -> Option<Value> {
    let g = p.child("GEOGCS")?;
    let base = geographic(g, g.epsg().is_some())?;
    let linear = unit(p.child("UNIT"), "LinearUnit");
    let proj = p.child("PROJECTION")?;
    let ext = p.child("EXTENSION").and_then(|e| e.str(1)).unwrap_or_default();
    let web_mercator = ext.contains("+proj=merc") && ext.contains("+a=6378137") && ext.contains("+b=6378137");
    let param =
        |names: &[&str]| p.children("PARAMETER").find(|x| names.iter().any(|n| x.name().eq_ignore_ascii_case(n))).and_then(|x| x.num(1));
    let (mname, mcode, params): Method = if web_mercator {
        ("Popular Visualisation Pseudo Mercator", 1024, &[(LAT0, LAT_NAT), (LON0, LON_NAT), (X0, FE), (Y0, FN)])
    } else {
        method(proj.name(), p)?
    };
    let mut ps = Vec::new();
    for (names, (pname, pcode, kind)) in params {
        let v = param(names).unwrap_or(match *kind {
            's' => 1.0,
            _ => 0.0,
        });
        let u = match kind {
            'a' => json!("degree"),
            's' => json!("unity"),
            _ => linear.clone(),
        };
        ps.push(json!({"name": pname, "value": v, "unit": u, "id": id(*pcode)}));
    }
    let mut m = json!({"name": mname});
    if mcode != 0 {
        m["id"] = id(mcode);
    }
    // Axes as the WKT gives them (GDAL writes EPSG's), east/north otherwise. Polar axes run
    // along meridians: easting 90 degrees east of the origin's, northing along it (south pole)
    // or opposite it (north pole).
    let axes: Vec<&Node> = p.children("AXIS").collect();
    let polar = mname.starts_with("Polar Stereographic");
    let lon0 = param(&["central_meridian", "longitude_of_origin", "longitude_of_center"]).unwrap_or(0.0);
    let norm = |v: f64| {
        let v = (v + 180.0).rem_euclid(360.0) - 180.0;
        if v == -180.0 { 180.0 } else { v }
    };
    let axis_json = |name: &str, dir: &str| -> Value {
        let dir = dir.to_ascii_lowercase();
        let abbrev = match name {
            "Easting" => "E",
            "Northing" => "N",
            "Westing" => "W",
            "Southing" => "S",
            n => &n[..n.len().min(1)],
        };
        let mut a = json!({"name": name, "abbreviation": abbrev, "direction": dir, "unit": linear});
        if polar && (dir == "north" || dir == "south") {
            let m = match (name, dir.as_str()) {
                ("Easting", _) => lon0 + 90.0,
                (_, "south") => lon0 + 180.0,
                _ => lon0,
            };
            a["meridian"] = json!({"longitude": norm(m)});
        }
        a
    };
    let axis: Vec<Value> = if axes.len() == 2 && axes.iter().all(|a| a.str(1).is_some()) {
        axes.iter().map(|a| axis_json(a.name(), a.str(1).unwrap_or("east"))).collect()
    } else {
        vec![axis_json("Easting", "east"), axis_json("Northing", "north")]
    };
    let mut out = json!({
        "type": "ProjectedCRS",
        "name": p.name(),
        "base_crs": base,
        "conversion": {"name": format!("unnamed ({mname})"), "method": m, "parameters": ps},
        "coordinate_system": {"subtype": "Cartesian", "axis": axis},
    });
    if let Some(c) = p.epsg() {
        out["id"] = id(c);
    }
    Some(out)
}

/// The EPSG code a PROJJSON object names: its `id`, or a bound CRS's source.
pub(crate) fn epsg(v: &Value) -> Option<u32> {
    let id = v.get("id").or_else(|| v.get("ids").and_then(|ids| ids.get(0)))?;
    if id["authority"].as_str().is_some_and(|a| a.eq_ignore_ascii_case("EPSG")) {
        return id["code"].as_u64().map(|c| c as u32).or_else(|| id["code"].as_str()?.parse().ok());
    }
    None
}

fn unit_wkt(u: &Value, linear: bool) -> (String, f64) {
    match u {
        Value::String(s) if s == "metre" => ("metre".into(), 1.0),
        Value::String(s) if s == "degree" => ("degree".into(), 0.0174532925199433),
        Value::String(s) if s == "US survey foot" => ("US survey foot".into(), 0.304800609601219),
        Value::String(s) if s == "foot" => ("foot".into(), 0.3048),
        Value::Object(o) => (
            o.get("name").and_then(Value::as_str).unwrap_or("unknown").to_string(),
            o.get("conversion_factor").and_then(Value::as_f64).unwrap_or(1.0),
        ),
        _ if linear => ("metre".into(), 1.0),
        _ => ("degree".into(), 0.0174532925199433),
    }
}

fn wkt_str(s: &str) -> String {
    s.replace('"', "\"\"")
}

/// Converts a PROJJSON geographic or projected CRS to WKT1, for CRSs that have no EPSG code.
/// `None` for other CRS types and unknown methods.
pub(crate) fn to_wkt1(v: &Value) -> Option<String> {
    match v["type"].as_str()? {
        "GeographicCRS" => geog_wkt(v),
        "ProjectedCRS" => {
            let base = geog_wkt(&v["base_crs"])?;
            let conv = &v["conversion"];
            let mcode = conv["method"]["id"]["code"].as_u64().map(|c| c as u32);
            let mname = conv["method"]["name"].as_str().unwrap_or_default();
            let (wname, table) = WKT_METHODS
                .iter()
                .find(|(_, code, name, _)| Some(*code) == mcode || name.eq_ignore_ascii_case(mname))
                .map(|(w, _, _, t)| (*w, *t))?;
            let mut w = format!("PROJCS[\"{}\",{base},PROJECTION[\"{wname}\"]", wkt_str(v["name"].as_str().unwrap_or("unnamed")));
            let params = conv["parameters"].as_array()?;
            let lin_unit = v["coordinate_system"]["axis"][0]["unit"].clone();
            let (uname, ufactor) = unit_wkt(&lin_unit, true);
            for (wkt_param, code) in table.iter() {
                let Some(p) = params.iter().find(|p| p["id"]["code"].as_u64() == Some(*code as u64)) else { continue };
                let mut value = p["value"].as_f64()?;
                // Lengths in the CRS's unit; angles in degrees.
                if let Value::Object(u) = &p["unit"] {
                    let f = u.get("conversion_factor").and_then(Value::as_f64).unwrap_or(1.0);
                    let lin = u.get("type").and_then(Value::as_str) == Some("LinearUnit");
                    if lin {
                        value = value * f / ufactor;
                    } else if u.get("type").and_then(Value::as_str) == Some("AngularUnit") {
                        value = value * f / 0.0174532925199433;
                    }
                } else if p["unit"] == "metre" {
                    value /= ufactor;
                }
                let _ = write!(w, ",PARAMETER[\"{wkt_param}\",{value}]");
            }
            let _ = write!(w, ",UNIT[\"{}\",{ufactor}]", wkt_str(&uname));
            let axes = v["coordinate_system"]["axis"].as_array().cloned().unwrap_or_default();
            if axes.len() == 2 {
                for a in &axes {
                    let name = a["name"].as_str().unwrap_or("Easting");
                    let dir = a["direction"].as_str().unwrap_or("east").to_ascii_uppercase();
                    let _ = write!(w, ",AXIS[\"{}\",{dir}]", wkt_str(name));
                }
            } else {
                w.push_str(",AXIS[\"Easting\",EAST],AXIS[\"Northing\",NORTH]");
            }
            if let Some(c) = epsg(v) {
                let _ = write!(w, ",AUTHORITY[\"EPSG\",\"{c}\"]");
            }
            w.push(']');
            Some(w)
        }
        _ => None,
    }
}

/// WKT1 method names and parameters by EPSG code.
/// A WKT1 method: `(WKT1 name, EPSG code, EPSG name, [(WKT1 parameter, EPSG parameter code)])`.
type WktMethod = (&'static str, u32, &'static str, &'static [(&'static str, u32)]);

const WKT_METHODS: &[WktMethod] = &[
    ("Transverse_Mercator", 9807, "Transverse Mercator", NAT),
    ("Transverse_Mercator_South_Orientated", 9808, "Transverse Mercator (South Orientated)", NAT),
    ("Mercator_1SP", 9804, "Mercator (variant A)", NAT),
    ("Mercator_2SP", 9805, "Mercator (variant B)", &[("standard_parallel_1", 8823), ("central_meridian", 8802), FE_W, FN_W]),
    ("Lambert_Conformal_Conic_1SP", 9801, "Lambert Conic Conformal (1SP)", NAT),
    ("Lambert_Conformal_Conic_2SP", 9802, "Lambert Conic Conformal (2SP)", FO),
    ("Albers_Conic_Equal_Area", 9822, "Albers Equal Area", FO_CENTER),
    ("Lambert_Azimuthal_Equal_Area", 9820, "Lambert Azimuthal Equal Area", AZ),
    ("Azimuthal_Equidistant", 1125, "Azimuthal Equidistant", AZ),
    ("Polar_Stereographic", 9810, "Polar Stereographic (variant A)", NAT),
    (
        "Polar_Stereographic",
        9829,
        "Polar Stereographic (variant B)",
        &[("latitude_of_origin", 8832), ("central_meridian", 8833), FE_W, FN_W],
    ),
    ("Oblique_Stereographic", 9809, "Oblique Stereographic", NAT),
    ("Stereographic", 0, "Stereographic", NAT),
    ("Equirectangular", 1028, "Equidistant Cylindrical", &[("standard_parallel_1", 8823), ("central_meridian", 8802), FE_W, FN_W]),
    ("Sinusoidal", 0, "Sinusoidal", &[("longitude_of_center", 8802), FE_W, FN_W]),
    (
        "Cylindrical_Equal_Area",
        9835,
        "Lambert Cylindrical Equal Area",
        &[("standard_parallel_1", 8823), ("central_meridian", 8802), FE_W, FN_W],
    ),
    ("Cassini_Soldner", 9806, "Cassini-Soldner", &[("latitude_of_origin", 8801), ("central_meridian", 8802), FE_W, FN_W]),
];
const FE_W: (&str, u32) = ("false_easting", 8806);
const FN_W: (&str, u32) = ("false_northing", 8807);
const NAT: &[(&str, u32)] = &[("latitude_of_origin", 8801), ("central_meridian", 8802), ("scale_factor", 8805), FE_W, FN_W];
const FO: &[(&str, u32)] = &[
    ("standard_parallel_1", 8823),
    ("standard_parallel_2", 8824),
    ("latitude_of_origin", 8821),
    ("central_meridian", 8822),
    ("false_easting", 8826),
    ("false_northing", 8827),
];
const FO_CENTER: &[(&str, u32)] = &[
    ("standard_parallel_1", 8823),
    ("standard_parallel_2", 8824),
    ("latitude_of_center", 8821),
    ("longitude_of_center", 8822),
    ("false_easting", 8826),
    ("false_northing", 8827),
];
const AZ: &[(&str, u32)] = &[("latitude_of_center", 8801), ("longitude_of_center", 8802), FE_W, FN_W];

fn geog_wkt(v: &Value) -> Option<String> {
    let datum = v.get("datum").or_else(|| v.get("datum_ensemble"))?;
    let e = &datum["ellipsoid"];
    let a = e["semi_major_axis"].as_f64()?;
    let rf = e["inverse_flattening"].as_f64().unwrap_or_else(|| match e["semi_minor_axis"].as_f64() {
        Some(b) if b != a => a / (a - b),
        _ => 0.0,
    });
    let dname = datum["name"].as_str().unwrap_or("unknown").trim_end_matches(" ensemble");
    let wkt_datum = DATUMS.iter().find(|d| d.1 == dname).map_or_else(|| dname.replace(' ', "_"), |d| d.0[0].to_string());
    let (uname, ufactor) = unit_wkt(&v["coordinate_system"]["axis"][0]["unit"], false);
    let pm = v["prime_meridian"]["longitude"].as_f64().unwrap_or(0.0);
    let pm_name = v["prime_meridian"]["name"].as_str().unwrap_or("Greenwich");
    let mut w = format!(
        "GEOGCS[\"{}\",DATUM[\"{}\",SPHEROID[\"{}\",{a},{rf}]],PRIMEM[\"{}\",{pm}],UNIT[\"{}\",{ufactor}]",
        wkt_str(v["name"].as_str().unwrap_or("unknown")),
        wkt_str(&wkt_datum),
        wkt_str(e["name"].as_str().unwrap_or("unknown")),
        wkt_str(pm_name),
        wkt_str(&uname)
    );
    if let Some(c) = epsg(v) {
        let _ = write!(w, ",AUTHORITY[\"EPSG\",\"{c}\"]");
    }
    w.push(']');
    Some(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_utm() {
        let wkt = super::super::super::wkt::gdal_wkt(&crate::crs::SpatialRef::from_epsg(32633).unwrap()).unwrap();
        let j = from_wkt1(&wkt, Some(32633)).unwrap();
        assert_eq!(j["type"], "ProjectedCRS");
        assert_eq!(j["conversion"]["method"]["id"]["code"], 9807);
        assert_eq!(j["conversion"]["parameters"][1]["value"], 15.0);
        assert_eq!(j["id"]["code"], 32633);
        let back = to_wkt1(&j).unwrap();
        assert!(back.contains("PARAMETER[\"central_meridian\",15]"), "{back}");
    }

    #[test]
    fn parses_esri_wkt() {
        let wkt = r#"PROJCS["NAD_1983_StatePlane_New_York_Long_Island_FIPS_3104_Feet",GEOGCS["GCS_North_American_1983",DATUM["D_North_American_1983",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Lambert_Conformal_Conic"],PARAMETER["False_Easting",984250.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",-74.0],PARAMETER["Standard_Parallel_1",40.66666666666666],PARAMETER["Standard_Parallel_2",41.03333333333333],PARAMETER["Latitude_Of_Origin",40.16666666666666],UNIT["Foot_US",0.3048006096012192]]"#;
        let j = from_wkt1(wkt, None).unwrap();
        assert_eq!(j["conversion"]["method"]["id"]["code"], 9802);
        assert_eq!(j["coordinate_system"]["axis"][0]["unit"]["name"], "US survey foot");
        assert_eq!(j["base_crs"]["datum"]["id"]["code"], 6269);
    }
}
