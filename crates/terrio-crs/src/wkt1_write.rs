// Written to match the behavior of:
//   PROJ 9.8.1:
//     src/iso19111/io.cpp
// No code is translated from these sources.

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
    "proj", "lat_0", "lon_0", "k", "k_0", "x_0", "y_0", "lat_1", "lat_2", "lon_1", "lon_2", "lat_ts", "lonc", "alpha", "gamma", "no_uoff",
    "no_off", "zone", "south", "h", "sweep", "azi", "ellps", "datum", "a", "b", "rf", "f", "R", "towgs84", "pm", "units", "to_meter",
    "axis", "no_defs", "wktext", "type", "nadgrids",
];

/// `(PROJ name, EPSG name, EPSG code, inverse flattening)` of the ellipsoids GDAL names. The
/// inverse flattening is the defining one, as PROJ writes it; `None` for an ellipsoid defined by
/// its semi-minor axis, whose inverse flattening PROJ computes.
const ELLIPSOIDS: &[(&str, &str, u32, Option<f64>)] = &[
    ("WGS84", "WGS 84", 7030, Some(298.257223563)),
    ("GRS80", "GRS 1980", 7019, Some(298.257222101)),
    ("WGS72", "WGS 72", 7043, Some(298.26)),
    ("bessel", "Bessel 1841", 7004, Some(299.1528128)),
    ("intl", "International 1924", 7022, Some(297.0)),
    ("clrk66", "Clarke 1866", 7008, None),
    ("clrk80", "Clarke 1880 (RGS)", 7012, Some(293.4663)),
    ("clrk80ign", "Clarke 1880 (IGN)", 7011, Some(293.4660212936269)),
    ("airy", "Airy 1830", 7001, Some(299.3249646)),
    ("mod_airy", "Airy Modified 1849", 7002, None),
    ("krass", "Krassowsky 1940", 7024, Some(298.3)),
    ("aust_SA", "Australian National Spheroid", 7003, Some(298.25)),
    ("helmert", "Helmert 1906", 7020, Some(298.3)),
    ("GRS67", "GRS 1967", 7036, Some(298.247167427)),
    ("evrst30", "Everest 1830 (1937 Adjustment)", 7015, Some(300.8017)),
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
    // Also described: `+R_C` (the conformal sphere of the ellipsoid) on a sphere, which changes
    // nothing; `+es` or `+e` beside `+a` alone, which gives the ellipsoid's shape (with another
    // shape parameter, terrio's order of precedence is not PROJ's).
    let shape = p.has("a") && ["ellps", "datum", "R", "b", "f", "rf"].iter().all(|k| !p.has(k)) && !(p.has("es") && p.has("e"));
    let described =
        datum_described && p.keys().all(|k| DESCRIBED.contains(&k) || (k == "R_C" && p.has("R")) || (matches!(k, "es" | "e") && shape));
    // Faithful when terrio reads it back as the PROJ string: the same conversion on the same
    // datum (its EPSG code identified, its geographic CRS).
    let faithful =
        |wkt: &str| described && Def::new(&SpatialRef::from_wkt(wkt)).is_ok_and(|read| read.same_conversion(&own) && read.same_datum(&own));
    if own.is_geocentric() {
        let root = super::wkt1::parse(&geogcs)?;
        let datum = root.child("DATUM")?.to_wkt();
        let pm = root.child("PRIMEM")?.to_wkt();
        let unit = unit(&p, own.to_meter());
        let w = format!(
            "GEOCCS[\"unknown\",{datum},{pm},{unit},AXIS[\"Geocentric X\",OTHER],AXIS[\"Geocentric Y\",OTHER],AXIS[\"Geocentric Z\",NORTH]]"
        );
        return Some(if faithful(&w) { w } else { extended(&w, &extension) });
    }
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
    Some(if faithful(&wkt) { wkt } else { extended(&wkt, &extension) })
}

/// A PROJ string as its WKT 1 describes it, for formats that describe CRSs by parameters
/// (GeoTIFF's GeoKeys): its method and parameters (in degrees and its linear unit), its linear
/// unit and datum.
pub struct Described {
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

/// The root keywords of WKT 1.
const WKT1_ROOTS: [&str; 6] = ["GEOGCS", "PROJCS", "GEOCCS", "COMPD_CS", "VERT_CS", "LOCAL_CS"];

/// The WKT 1 of a CRS that a reader ignoring `EXTENSION["PROJ4", ...]` reads as the CRS, as
/// PROJ's `WKT1_GDAL` export: [`SpatialRef::to_wkt`] when it is such a WKT 1, a stored WKT 2
/// (or PROJJSON) as its registry CRS's WKT 1 or as the WKT 1 of its PROJ string. `None` for
/// what PROJ cannot write in WKT 1 (geographic 3D CRSs, Equal Earth, the registry CRSs whose
/// method has no WKT 1 name), and for a WKT 1 that needs an extension, unless its root's EPSG or
/// ESRI `AUTHORITY` is the CRS's own code (GDAL's Web Mercator, EPSG:3857).
pub(crate) fn strict(sr: &SpatialRef) -> Option<String> {
    // An EPSG code outside terrio's tables is read through the wbprojection fallback, whose WKT
    // is not the registry's.
    let outside = |c: u32| epsg::get(c).is_none() && epsg::compound(c).is_none() && epsg::vertical(c).is_none();
    let fallback = |c: u32| sr.wkt.is_none() || sr.wkt == super::registry_wkt(c);
    if sr.esri.is_none() && sr.epsg.is_some_and(|c| outside(c) && fallback(c)) {
        return None;
    }
    let wkt = sr.to_wkt()?;
    let root = super::wkt1::parse(&wkt);
    if let Some(root) = root.as_ref().filter(|r| WKT1_ROOTS.iter().any(|k| r.key.eq_ignore_ascii_case(k))) {
        return plain(root, sr.authority()).then_some(wkt);
    }
    // A registry CRS whose table WKT is WKT 2 is one PROJ writes no WKT 1 for; nor is a 3D CRS
    // (a bound CRS's source or a compound CRS's horizontal CRS).
    let horizontal = root.as_ref().and_then(wkt2_horizontal);
    if sr.wkt.is_none() || horizontal.is_some_and(is_3d) {
        return None;
    }
    // A datum shift by a grid (a bound CRS's `PARAMETERFILE`): WKT 1 keeps it only in an
    // extension, and the PROJ string terrio reads leaves it out.
    // By its nodes, not its text (a name may hold the words); by its text only when it does not
    // parse (PROJJSON is stored converted to WKT 2).
    let has = |key: &str| match &root {
        Some(r) => has_node(r, key),
        None => wkt.to_ascii_uppercase().contains(&key.to_ascii_uppercase()),
    };
    if has("PARAMETERFILE") {
        return None;
    }
    // A stored WKT 2 of a registry CRS, without a datum shift of its own and with its axes, is its
    // WKT 1 (`OGC:CRS84`, identified as EPSG:4326, is not: its axes are east-north).
    let bound = has("BOUNDCRS") || has("TOWGS84");
    let axes = root.as_ref().and_then(horizontal_axes);
    let geocentric = horizontal.is_some_and(is_geocentric);
    if !bound && (sr.epsg.is_some() || sr.esri.is_some()) {
        let registry = strict(&SpatialRef { epsg: sr.epsg, esri: sr.esri, wkt: None, proj4: None });
        let registry_axes = registry.as_deref().and_then(wkt1_axes);
        let own_axes = root.as_ref().and_then(wkt2_horizontal).and_then(directions);
        if geocentric || own_axes.is_none() || registry_axes.is_none() || registry_axes == own_axes {
            return registry;
        }
        // Its axes swapped (EPSG:4258 in longitude and latitude): the registry's WKT 1, with its
        // datum, the axes swapped and without the codes that give their order.
        if let (Some(r), Some(o)) = (&registry_axes, &own_axes)
            && r.len() == 2
            && o.len() == 2
            && r[0] == o[1]
            && r[1] == o[0]
        {
            // Without the code, an extension (Web Mercator's) is no longer the CRS's own.
            let swapped = registry.as_deref().and_then(swap_axes)?;
            return super::wkt1::parse(&swapped).is_some_and(|root| plain(&root, None)).then_some(swapped);
        }
    }
    // A bound CRS's shift reaches the PROJ string only as `+towgs84` (a Helmert): another
    // (Molodensky, geographic offsets) would be lost.
    let proj = sr.to_proj4()?;
    if bound && !proj.split_whitespace().any(|t| t.starts_with("+towgs84=")) {
        return None;
    }
    // Else the WKT 1 of its PROJ string, which names only WGS 84, NAD83 and NAD27: on a datum
    // terrio identifies (OSGB36), on that EPSG geographic CRS, which keeps the datum's name and
    // transformations.
    let own = Def::from_wkt(&wkt).ok()?;
    let geog = own.geog().filter(|_| !bound && !geocentric);
    let mut w = match geog {
        // Without the code it was not identified as.
        Some(g) if own.is_geographic() => without_root_authority(&strict(&SpatialRef { epsg: Some(g), ..Default::default() })?),
        Some(g) => from_proj_on(&proj, g)?,
        None => from_proj(&proj)?,
    };
    // The WKT 2's axes, east-north ones, or a geographic CRS's north-east ones, as WKT 1 writes
    // them. A registry geographic CRS's WKT 1 (`+datum=WGS84`) loses its code when its axes are
    // swapped.
    if !geocentric {
        const LON_LAT: &str = ",AXIS[\"Longitude\",EAST],AXIS[\"Latitude\",NORTH]";
        const LAT_LON: &str = ",AXIS[\"Latitude\",NORTH],AXIS[\"Longitude\",EAST]";
        let geographic = w.starts_with("GEOGCS[");
        let (from, to) = match axes?.as_slice() {
            [e, n] if e == "east" && n == "north" => (LAT_LON, LON_LAT),
            [n, e] if n == "north" && e == "east" && geographic => (LON_LAT, LAT_LON),
            _ => return None,
        };
        if geographic && w.contains(from) {
            w = without_root_authority(&w.replacen(from, to, 1));
        }
    }
    // A PROJ string can lose a conversion (EPSG's ellipsoidal Equidistant Cylindrical becomes
    // `+proj=eqc`, which WKT 1's Equirectangular is read as): the WKT 1 must read back as the CRS.
    let same = Def::new(&SpatialRef::from_wkt(&w)).is_ok_and(|read| read.same_conversion(&own));
    (same && super::wkt1::parse(&w).is_some_and(|root| plain(&root, None))).then_some(w)
}

/// The directions of the axes of a WKT 1 CRS's horizontal CRS (a compound CRS's first),
/// lowercase; east and north when it gives none.
fn wkt1_axes(wkt: &str) -> Option<Vec<String>> {
    let root = super::wkt1::parse(wkt)?;
    let root = match root.key.eq_ignore_ascii_case("COMPD_CS") {
        true => first_node(&root)?.clone(),
        false => root,
    };
    if !["GEOGCS", "PROJCS"].iter().any(|k| root.key.eq_ignore_ascii_case(k)) {
        return None;
    }
    let axes = directions(&root)?;
    Some(if axes.is_empty() { vec!["east".into(), "north".into()] } else { axes })
}

/// Whether a node or one of its descendants has a keyword.
fn has_node(n: &super::wkt1::Node, key: &str) -> bool {
    n.key.eq_ignore_ascii_case(key) || n.args.iter().any(|a| matches!(a, super::wkt1::Arg::Node(c) if has_node(c, key)))
}

/// The first child node of a node.
fn first_node(n: &super::wkt1::Node) -> Option<&super::wkt1::Node> {
    n.args.iter().find_map(|a| match a {
        super::wkt1::Arg::Node(c) => Some(c),
        _ => None,
    })
}

/// The horizontal CRS of a WKT 2 CRS: itself, a bound CRS's source CRS or a compound CRS's first.
fn wkt2_horizontal(root: &super::wkt1::Node) -> Option<&super::wkt1::Node> {
    match root.key.to_ascii_uppercase().as_str() {
        "BOUNDCRS" => first_node(root.child("SOURCECRS")?),
        "COMPOUNDCRS" => first_node(root),
        _ => Some(root),
    }
}

/// The directions of a WKT 2 CRS's axes, lowercase.
fn directions(crs: &super::wkt1::Node) -> Option<Vec<String>> {
    crs.children("AXIS")
        .map(|a| match a.args.get(1) {
            Some(super::wkt1::Arg::Ident(d)) => Some(d.to_ascii_lowercase()),
            _ => None,
        })
        .collect()
}

/// A registry CRS's WKT 1 with the two axes of its horizontal CRS swapped, and without the codes
/// of its horizontal CRS and root, which give their order. `None` when it has no two axes.
fn swap_axes(wkt: &str) -> Option<String> {
    // The horizontal CRS's text: the root, or a compound CRS's first node.
    let (start, end) = if wkt.starts_with("COMPD_CS[") {
        let start = ["PROJCS[", "GEOGCS["].iter().filter_map(|k| wkt.find(k)).min()?;
        let mut depth = 0;
        let len = wkt[start..].char_indices().find_map(|(i, c)| {
            depth += match c {
                '[' => 1,
                ']' => -1,
                _ => 0,
            };
            (c == ']' && depth == 0).then_some(i + 1)
        })?;
        (start, start + len)
    } else {
        (0, wkt.len())
    };
    let horizontal = &wkt[start..end];
    // Its own axes: its last two `AXIS` nodes (a base GEOGCS's come first).
    let at: Vec<usize> = horizontal.match_indices(",AXIS[").map(|(i, _)| i).collect();
    let [.., a, b] = at[..] else { return None };
    let a_end = a + horizontal[a..].find(']')? + 1;
    let b_end = b + horizontal[b..].find(']')? + 1;
    if a_end != b {
        return None;
    }
    let swapped = format!("{}{}{}{}", &horizontal[..a], &horizontal[b..b_end], &horizontal[a..a_end], &horizontal[b_end..]);
    let swapped = without_root_authority(&swapped);
    let out = format!("{}{swapped}{}", &wkt[..start], &wkt[end..]);
    Some(if start > 0 { without_root_authority(&out) } else { out })
}

/// A WKT 1 without the `AUTHORITY` of its root (its last node).
fn without_root_authority(wkt: &str) -> String {
    match wkt.rfind(",AUTHORITY[") {
        Some(i) if wkt[i..].matches(']').count() == 2 => format!("{}]", &wkt[..i]),
        _ => wkt.to_string(),
    }
}

/// Whether a WKT 2 CRS is geocentric: a geodetic CRS of a Cartesian CS.
fn is_geocentric(root: &super::wkt1::Node) -> bool {
    let cartesian =
        root.child("CS").is_some_and(|c| matches!(c.args.first(), Some(super::wkt1::Arg::Ident(t)) if t.eq_ignore_ascii_case("Cartesian")));
    ["GEODCRS", "GEODETICCRS"].iter().any(|k| root.key.eq_ignore_ascii_case(k)) && cartesian
}

/// The directions of the axes of a WKT 2 CRS's horizontal CRS (a bound CRS's source CRS),
/// lowercase. `None` for a compound or other CRS.
fn horizontal_axes(root: &super::wkt1::Node) -> Option<Vec<String>> {
    if root.key.eq_ignore_ascii_case("COMPOUNDCRS") {
        return None;
    }
    let crs = wkt2_horizontal(root)?;
    let horizontal = ["GEOGCRS", "GEOGRAPHICCRS", "GEODCRS", "GEODETICCRS", "PROJCRS", "PROJECTEDCRS"];
    if !horizontal.iter().any(|k| crs.key.eq_ignore_ascii_case(k)) {
        return None;
    }
    let axes = directions(crs);
    // A polar CRS's axes along meridians (south along 90°E and 180°E at the north pole, north
    // along 90°E and 0°E at the south pole), the easting and northing of its projection.
    let along = |a: &super::wkt1::Node, lon: f64| a.child("MERIDIAN").and_then(|m| m.num(0)) == Some(lon);
    let meridians: Vec<&super::wkt1::Node> = crs.children("AXIS").collect();
    let polar = match (axes.as_deref(), meridians.as_slice()) {
        (Some([x, y]), [ax, ay]) if x == "south" && y == "south" => along(ax, 90.0) && along(ay, 180.0),
        (Some([x, y]), [ax, ay]) if x == "north" && y == "north" => along(ax, 90.0) && along(ay, 0.0),
        _ => false,
    };
    if polar { Some(vec!["east".into(), "north".into()]) } else { axes }
}

/// Whether a WKT 1 describes its CRS without any `EXTENSION` node (GDAL's `PROJ4`, a datum's
/// `PROJ4_GRIDS`, or another), or with one under the CRS's own EPSG or ESRI code (`code`), in a
/// method PROJ writes in WKT 1.
fn plain(root: &super::wkt1::Node, code: Option<(&str, u32)>) -> bool {
    fn any(n: &super::wkt1::Node, f: &dyn Fn(&super::wkt1::Node) -> bool) -> bool {
        f(n) || n.args.iter().any(|a| matches!(a, super::wkt1::Arg::Node(c) if any(c, f)))
    }
    let is = |n: &super::wkt1::Node, key: &str, name: &str| {
        n.key.eq_ignore_ascii_case(key) && n.name().is_some_and(|v| v.eq_ignore_ascii_case(name))
    };
    // PROJ has no WKT 1 name for Equal Earth (EPSG:8857 has no WKT 1).
    if any(root, &|n| is(n, "PROJECTION", "Equal_Earth")) {
        return false;
    }
    let coded = root.child("AUTHORITY").is_some_and(|a| {
        let value = match a.args.get(1) {
            Some(super::wkt1::Arg::Str(v)) => v.trim().parse().ok(),
            Some(super::wkt1::Arg::Num(v)) => Some(*v as u32),
            _ => None,
        };
        code.is_some_and(|(name, c)| {
            (name == "EPSG" || name == "ESRI") && a.name().is_some_and(|n| n.eq_ignore_ascii_case(name)) && value == Some(c)
        })
    });
    coded || !any(root, &|n| n.key.eq_ignore_ascii_case("EXTENSION"))
}

/// Whether a WKT 2 CRS is three-dimensional and not geocentric: WKT 1 has no 3D geographic or
/// projected CRS.
fn is_3d(root: &super::wkt1::Node) -> bool {
    let cs = root.child("CS");
    let dims = cs.and_then(|c| c.num(1));
    let cartesian = cs.is_some_and(|c| matches!(c.args.first(), Some(super::wkt1::Arg::Ident(t)) if t.eq_ignore_ascii_case("Cartesian")));
    let geodetic = ["GEODCRS", "GEODETICCRS"].iter().any(|k| root.key.eq_ignore_ascii_case(k));
    dims == Some(3.0) && !(geodetic && cartesian)
}

/// The description of a PROJ string whose WKT 1 describes it whole (without the PROJ4
/// extension), with east-north axes. `None` otherwise.
pub fn describe(s: &str) -> Option<Described> {
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
        rf: num(inverse_flattening(&p, &ell, ellipsoid.and_then(|e| e.3))).parse().unwrap_or(0.0),
        pm: p.str("pm").map(|v| v.parse().ok().or_else(|| prime_meridian(v)).unwrap_or(0.0)).unwrap_or(0.0),
        towgs84: p.list("towgs84").filter(|v| v.len() == 3 || v.len() == 7),
    })
}

/// The GDAL WKT 1 of a projected PROJ string on an EPSG geographic CRS (its `GEOGCS`, which
/// identifies the datum), as GDAL reads GeoKeys that give the geographic CRS by its code. `None`
/// when the code is not a geographic CRS of the EPSG table, or the WKT 1 needs the PROJ string to
/// describe the projection.
pub fn from_proj_on(s: &str, geog: u32) -> Option<String> {
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
pub fn epsg_ellipsoid(geog: u32) -> Option<String> {
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
    // R_C changes the runtime conversion radius, while PROJ's CRS metadata
    // keeps the original ellipsoid. The PROJ4 extension retains the modifier.
    let ell = if p.has("R_C") && !p.has("R") && !p.has("R_A") {
        super::datum::ellipsoid(p)
            .or_else(|| p.str("datum").and_then(super::def::datum_ellps).and_then(Ell::named))
            .unwrap_or_else(Ell::wgs84)
    } else {
        own.ell
    };
    let named = p.str("ellps").or_else(|| p.str("datum").and_then(super::def::datum_ellps));
    let known = named.and_then(|n| ELLIPSOIDS.iter().find(|e| e.0 == n)).filter(|e| Ell::named(e.0).is_some_and(|n| n == ell));
    let rf = inverse_flattening(p, &ell, known.and_then(|e| e.3));
    let (spheroid, datum) = match known {
        Some((_, name, code, _)) => (
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

/// The inverse flattening of an ellipsoid as PROJ writes it (0 for a sphere): the defining one of
/// a named ellipsoid, else the string's `+rf` when it gives this ellipsoid, else computed from its
/// eccentricity. Computing the defining value back (298.257223563002 for WGS 84) would make
/// another ellipsoid of it.
fn inverse_flattening(p: &Params, ell: &Ell, defining: Option<f64>) -> f64 {
    if ell.es == 0.0 {
        return 0.0;
    }
    let same = |rf: &f64| Ell::from_a_rf(ell.a, *rf) == *ell;
    defining.or_else(|| p.f64("rf")).filter(same).unwrap_or_else(|| 1.0 / (1.0 - ell.one_es.sqrt()))
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
            // With one standard parallel and no latitude of origin, the origin and a second
            // parallel are on the equator, as in PROJ's CRSs.
            let lat_2 = match deg("lat_2") {
                Some(v) => v,
                None if p.has("lat_0") => lat_1,
                None => 0.0,
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
            // The pole of `lat_ts`'s hemisphere, as the CRS is read.
            let pole = deg("lat_ts").map_or(lat_0, |ts| if ts < 0.0 { -90.0 } else { 90.0 });
            let params = match deg("lat_ts").filter(|ts| (ts.abs() - 90.0).abs() > 1e-10) {
                Some(ts) => vec![("latitude_of_origin", ts), ("central_meridian", lon_0), ("false_easting", fe), ("false_northing", fn_)],
                None => tm(pole, lon_0, k, fe, fn_),
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
        // The centre line through two points, as GDAL writes PROJ's two-point form.
        "omerc" if !p.has("alpha") && !p.has("gamma") => m(
            "Hotine_Oblique_Mercator_Two_Point_Natural_Origin",
            vec![
                ("latitude_of_center", lat_0),
                ("latitude_of_point_1", deg("lat_1").unwrap_or(0.0)),
                ("longitude_of_point_1", deg("lon_1").unwrap_or(0.0)),
                ("latitude_of_point_2", deg("lat_2").unwrap_or(0.0)),
                ("longitude_of_point_2", deg("lon_2").unwrap_or(0.0)),
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
        "qsc" => m("Quadrilateralized_Spherical_Cube", origin("latitude_of_origin", "central_meridian")),
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
pub(crate) const WORLD: [(&str, &str); 13] = [
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
    ("igh", "Interrupted_Goode_Homolosine"),
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
    }

    #[test]
    fn fallback_methods_require_the_feature() {
        let result = from_proj("+proj=hammer +ellps=WGS84 +units=m");
        if cfg!(feature = "wbprojection-fallback") {
            let w = result.unwrap();
            assert!(w.contains("PROJECTION[\"custom_proj4\"]") && w.contains("+proj=hammer +ellps=WGS84 +units=m +wktext +no_defs"), "{w}");
        } else {
            assert!(result.is_none());
        }
    }
}
