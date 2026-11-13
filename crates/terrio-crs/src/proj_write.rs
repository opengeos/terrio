//! PROJ strings of CRSs as PROJ 9.8.1 writes them (`proj_as_proj_string` with `PJ_PROJ_4`, which
//! pyproj's `CRS.to_proj4()` and `projinfo -o PROJ` call).
//!
//! The EPSG and ESRI tables hold each CRS's conversion as GDAL exports it, normalized for terrio
//! (`scripts/gen_epsg_table.py`): without the datum, its ellipsoid spelled out, the prime meridian
//! in degrees. This puts back what PROJ writes: `+datum=` for the three datums it names, prime
//! meridians by name, its order of the parameters, and its ending.

use crate::{SpatialRef, def, epsg};

/// The PROJ string of a CRS, or `None` when it has none.
pub(crate) fn write(s: &SpatialRef) -> Option<String> {
    if s.wkt.as_deref().is_some_and(is_engineering) {
        return None;
    }
    if let Some(mut tokens) = extension(s) {
        tokens.push("+type=crs".into());
        return Some(tokens.join(" "));
    }
    // A CRS given by a PROJ string (one given, an IAU CRS's) is written from it, as PROJ writes
    // a CRS from what it was read from, even when terrio identified its code.
    if let Some(p) = &s.proj4 {
        return custom(p, None);
    }
    // A compound CRS of a WKT (`EPSG:4326+5773`, a `.prj`'s `COMPD_CS`) that is not a registry
    // compound CRS: its horizontal CRS's string and its vertical CRS's unit.
    if s.epsg.and_then(epsg::compound).is_none()
        && let Some((horizontal, vertical)) = s.wkt.as_deref().and_then(def::split_compound)
    {
        let horizontal = SpatialRef::from_wkt(&horizontal);
        // A PROJ4 extension is kept whole, `+no_defs` and all, the vertical CRS after it.
        let (mut tokens, extended) = match extension(&horizontal) {
            Some(tokens) => (tokens, true),
            None => (strip_suffix(&write(&horizontal)?), false),
        };
        let to_meter = def::vertical_from_wkt(&vertical)?.to_meter;
        // A geoid model (GDAL's `PROJ4_GRIDS` extension) comes with the CRS of its heights.
        if let Some(grids) = wkt_geoid_grids(&vertical) {
            tokens.extend([format!("+geoidgrids={grids}"), "+geoid_crs=WGS84".into()]);
        }
        tokens.push(vertical_unit(to_meter));
        if extended {
            tokens.push("+type=crs".into());
            return Some(tokens.join(" "));
        }
        return Some(finish(tokens));
    }
    // A WKT terrio identified as an EPSG or ESRI code gets the code's string: PROJ writes the
    // same from the WKT (UTM zones as `+proj=utm`, Web Mercator with its null grid), which
    // terrio's own reading of the WKT does not reproduce; but for the order of parameters the
    // WKT gives in another order (standard parallels), which PROJ keeps.
    if s.epsg.is_none() && s.esri.is_none() {
        return s.wkt.as_deref().and_then(|w| crate::wkt_proj(w).and_then(|p| custom(&p, wkt_datum(w))));
    }
    // A WKT's shift to WGS 84 (`TOWGS84`, a WKT 2 `BOUNDCRS`) makes it a bound CRS, which PROJ
    // writes with the ellipsoid and `+towgs84` in place of the datum, even for a code.
    let shift = wkt_shift(s);
    if let Some(code) = s.epsg {
        if let Some(c) = epsg::compound(code) {
            // The horizontal CRS of the compound CRS's WKT, for its shift.
            let wkt = s.wkt.as_deref().and_then(def::split_compound).map(|(h, _)| h);
            let horizontal = write(&SpatialRef { epsg: Some(c.horizontal), wkt, ..Default::default() })?;
            let mut tokens = strip_suffix(&horizontal);
            tokens.push(vertical_unit(epsg::vertical(c.vertical)?.to_meter));
            return Some(finish(tokens));
        }
        if let Some(v) = epsg::vertical(code) {
            return Some(finish(vec![vertical_unit(v.to_meter)]));
        }
        // Polar Stereographic (variant C) has no PROJ method.
        if epsg::is_variant_c(code) {
            return None;
        }
        if let Some(e) = epsg::get(code) {
            return Some(registry(&e.proj, e.datum, &e.wkt, is_web_mercator(s), shift.as_deref()));
        }
        // An EPSG code outside the table is one whose method the table leaves out, which PROJ
        // cannot write either (Lambert Conic Near-Conformal...); the wbprojection fallback's
        // definition of it is not PROJ's.
        return None;
    }
    s.esri.and_then(epsg::esri).map(|e| registry(&e.proj, e.datum, &e.wkt, is_web_mercator(s), shift.as_deref()))
}

/// The shift to WGS 84 of a CRS's WKT, when it has one: `+nadgrids`, else `+towgs84`.
fn wkt_shift(s: &SpatialRef) -> Option<String> {
    let w = s.wkt.as_deref().filter(|w| ["TOWGS84", "ABRIDGEDTRANSFORMATION", "nadgrids"].iter().any(|k| w.contains(k)))?;
    let proj = crate::wkt_proj(w)?;
    let tokens: Vec<&str> = proj.split_whitespace().collect();
    tokens
        .iter()
        .find(|t| t.starts_with("+nadgrids="))
        .map(|t| t.to_string())
        .or_else(|| tokens.iter().find_map(|t| t.strip_prefix("+towgs84=")).map(towgs84))
}

/// The geoid grids of a vertical CRS's WKT (GDAL's `EXTENSION["PROJ4_GRIDS", ...]`).
fn wkt_geoid_grids(wkt: &str) -> Option<&str> {
    let rest = &wkt[wkt.find("\"PROJ4_GRIDS\"")? + 13..];
    let rest = &rest[rest.find('"')? + 1..];
    Some(&rest[..rest.find('"')?])
}

/// `+towgs84` values as PROJ writes them: seven of them.
fn towgs84(values: &str) -> String {
    let mut values: Vec<String> = values.split(',').map(|v| v.parse::<f64>().map_or_else(|_| v.to_string(), number)).collect();
    if values.len() == 3 {
        values.extend(std::iter::repeat_n("0".to_string(), 4));
    }
    format!("+towgs84={}", values.join(","))
}

/// The axis order PROJ writes: that of the methods defining one (south-orientated Transverse
/// Mercator, Krovak), not of other CRSs whose axes point south, west or north first.
fn kept_axis(method: &str, axis: &str) -> bool {
    matches!((method, axis), ("tmerc", "wsu") | ("krovak" | "mod_krovak", "swu"))
}

/// The tokens (without `+type=crs`) of a WKT's PROJ4 extension (GDAL's, for CRSs WKT 1 cannot
/// describe), which is the CRS's PROJ string: PROJ writes it as it is, but GDAL's Web Mercator,
/// which it writes as its own; not the WKT terrio builds for a PROJ string it is given, which
/// carries the string so.
fn extension(s: &SpatialRef) -> Option<Vec<String>> {
    let given = |p: &String| crate::proj_to_wkt(p) == s.wkt;
    let text = s.wkt.as_deref().and_then(crate::proj4_extension).filter(|_| !s.proj4.as_ref().is_some_and(given))?;
    let text = gdal_web_mercator(&text).unwrap_or(text);
    Some(text.split_whitespace().filter(|t| *t != "+type=crs").map(str::to_string).collect())
}

/// Whether a WKT is of an engineering CRS, which has no PROJ string.
fn is_engineering(wkt: &str) -> bool {
    let root = wkt.split(['[', '(']).next().unwrap_or_default().trim();
    ["LOCAL_CS", "ENGCRS", "ENGINEERINGCRS"].iter().any(|key| root.eq_ignore_ascii_case(key))
}

/// Web Mercator, which PROJ writes on its sphere with `+nadgrids=@null +wktext`, not with its
/// WGS 84 datum.
fn is_web_mercator(s: &SpatialRef) -> bool {
    matches!(s.epsg, Some(3857 | 3785 | 900913)) || s.esri == Some(102100)
}

/// The datums PROJ writes by name rather than by their ellipsoid.
fn datum_name(datum: u32) -> Option<&'static str> {
    match datum {
        6326 => Some("WGS84"),
        6269 => Some("NAD83"),
        6267 => Some("NAD27"),
        _ => None,
    }
}

/// The datum PROJ names (`WGS84`, `NAD83`, `NAD27`) of a WKT's datum name, in the spellings of
/// GDAL's, ESRI's and WKT 2's WKT.
fn named_datum(wkt_name: &str) -> Option<&'static str> {
    match wkt_name {
        "WGS_1984" | "D_WGS_1984" | "WGS 84" | "World Geodetic System 1984" | "World Geodetic System 1984 ensemble" => Some("WGS84"),
        "North_American_Datum_1983" | "D_North_American_1983" | "North American Datum 1983" | "NAD83" => Some("NAD83"),
        "North_American_Datum_1927" | "D_North_American_1927" | "North American Datum 1927" | "NAD27" => Some("NAD27"),
        _ => None,
    }
}

/// PROJ's prime meridians (`proj_list_prime_meridians`), by longitude in degrees, which it writes
/// by name.
const PRIME_MERIDIANS: [(&str, f64); 13] = [
    ("lisbon", -(9.0 + 7.0 / 60.0 + 54.862 / 3600.0)),
    ("paris", 2.0 + 20.0 / 60.0 + 14.025 / 3600.0),
    ("bogota", -(74.0 + 4.0 / 60.0 + 51.3 / 3600.0)),
    ("madrid", -(3.0 + 41.0 / 60.0 + 16.58 / 3600.0)),
    ("rome", 12.0 + 27.0 / 60.0 + 8.4 / 3600.0),
    ("bern", 7.0 + 26.0 / 60.0 + 22.5 / 3600.0),
    ("jakarta", 106.0 + 48.0 / 60.0 + 27.79 / 3600.0),
    ("ferro", -(17.0 + 40.0 / 60.0)),
    ("brussels", 4.0 + 22.0 / 60.0 + 4.71 / 3600.0),
    ("stockholm", 18.0 + 3.0 / 60.0 + 29.8 / 3600.0),
    ("athens", 23.0 + 42.0 / 60.0 + 58.815 / 3600.0),
    ("oslo", 10.0 + 43.0 / 60.0 + 22.5 / 3600.0),
    ("copenhagen", 12.0 + 34.0 / 60.0 + 40.35 / 3600.0),
];

/// PROJ's linear units (`proj_list_units`): name and metres.
const UNITS: [(&str, f64); 21] = [
    ("km", 1000.0),
    ("m", 1.0),
    ("dm", 0.1),
    ("cm", 0.01),
    ("mm", 0.001),
    ("kmi", 1852.0),
    ("in", 0.0254),
    ("ft", 0.3048),
    ("yd", 0.9144),
    ("mi", 1609.344),
    ("fath", 1.8288),
    ("ch", 20.1168),
    ("link", 0.201168),
    ("us-in", 1.0 / 39.37),
    ("us-ft", 1200.0 / 3937.0),
    ("us-yd", 3600.0 / 3937.0),
    ("us-ch", 79200.0 / 3937.0),
    ("us-mi", 6336000.0 / 3937.0),
    ("ind-yd", 0.91439523),
    ("ind-ft", 0.30479841),
    ("ind-ch", 20.11669506),
];

/// PROJ's name for a linear unit of `to_meter` metres, `None` for one it has no name for.
fn unit_name(to_meter: f64) -> Option<&'static str> {
    UNITS.iter().find(|(_, m)| (m - to_meter).abs() <= 1e-12 * m).map(|(n, _)| *n)
}

/// The vertical unit as PROJ writes it: `+vunits=` with its name, else `+vto_meter=`.
fn vertical_unit(to_meter: f64) -> String {
    unit_name(to_meter).map_or_else(|| format!("+vto_meter={}", number(to_meter)), |n| format!("+vunits={n}"))
}

/// A number as PROJ writes it: with 15 significant digits, without trailing zeros, with an
/// exponent below 1e-4 and from 1e15 (as C's `%.15g` formats them).
fn number(v: f64) -> String {
    if v == 0.0 {
        return "0".into();
    }
    let exponent = v.abs().log10().floor() as i32;
    if !(-4..15).contains(&exponent) {
        let s = format!("{v:.14e}");
        let (mantissa, exponent) = s.split_once('e').unwrap_or((&s, "0"));
        let mantissa = if mantissa.contains('.') { mantissa.trim_end_matches('0').trim_end_matches('.') } else { mantissa };
        let exponent: i32 = exponent.parse().unwrap_or_default();
        return format!("{mantissa}e{}{:02}", if exponent < 0 { '-' } else { '+' }, exponent.abs());
    }
    let digits = 15 - 1 - v.abs().log10().floor() as i32;
    let s = format!("{:.*}", digits.max(0) as usize, v);
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
}

/// The longitude of a WKT's prime meridian in degrees, to all its digits (the table's PROJ
/// strings round it to 12).
fn wkt_prime_meridian(wkt: &str) -> Option<f64> {
    let rest = &wkt[wkt.find("PRIMEM[")? + 7..];
    let rest = &rest[rest.find('"')? + 1..];
    let rest = &rest[rest.find('"')? + 1..];
    let rest = rest.trim_start_matches([',', ' ']);
    rest[..rest.find([',', ']'])?].trim().parse().ok()
}

/// The name of a WKT's datum (`DATUM` in WKT 1 and 2, `ENSEMBLE` in WKT 2).
fn wkt_datum(wkt: &str) -> Option<&str> {
    let at = ["DATUM[\"", "ENSEMBLE[\""].iter().filter_map(|k| wkt.find(k).map(|i| i + k.len())).min()?;
    let rest = &wkt[at..];
    Some(&rest[..rest.find('"')?])
}

/// A registry CRS's PROJ string (the table's, without its datum shift) as PROJ writes it.
fn registry(proj: &str, datum: u32, wkt: &str, web_mercator: bool, shift: Option<&str>) -> String {
    let key = |t: &str| t.split('=').next().unwrap_or_default().to_string();
    let is_ellipsoid = |k: &str| ["+a", "+b", "+rf", "+R", "+ellps"].contains(&k);
    let (mut params, mut ellipsoid, mut pm, mut units, mut axis) = (Vec::new(), Vec::new(), None, Vec::new(), None);
    let method = proj.split_whitespace().next().unwrap_or_default();
    for t in proj.split_whitespace() {
        match key(t).as_str() {
            "+to_deg" => {}
            // PROJ writes the axis order of the methods that define it (south-orientated
            // Transverse Mercator, Krovak), not of other CRSs whose axes point south or west.
            "+axis" => axis = Some(t.to_string()),
            "+pm" => pm = t[4..].parse::<f64>().ok(),
            "+units" | "+to_meter" => units.push(t.to_string()),
            k if is_ellipsoid(k) => ellipsoid.push(t.to_string()),
            _ => params.push(t.to_string()),
        }
    }
    // A table entry without a datum code (some ESRI CRSs) is on WGS 84 when its WKT says so.
    let name = datum_name(datum).or_else(|| (datum == 0).then(|| wkt_datum(wkt).and_then(named_datum)).flatten());
    let shift = shift.filter(|_| !web_mercator);
    if shift.is_some() {
        // A bound CRS: the ellipsoid, by PROJ's name for it, and the shift (after the prime
        // meridian).
        if let Some(named) = ellipsoid_of(&ellipsoid.join(" ")) {
            ellipsoid = named;
        }
    } else if let (Some(name), false) = (name, web_mercator) {
        ellipsoid = vec![format!("+datum={name}")];
    }
    let mut tokens = Vec::new();
    tokens.push(params.remove(0));
    // Web Mercator's sphere comes first, as PROJ writes it.
    if web_mercator {
        tokens.append(&mut ellipsoid);
    }
    if let Some(a) = axis.filter(|a| kept_axis(method.trim_start_matches("+proj="), a.trim_start_matches("+axis="))) {
        tokens.push(a);
    }
    tokens.extend(params);
    tokens.extend(ellipsoid);
    if let Some(pm) = pm {
        let value = wkt_prime_meridian(wkt).filter(|v| (v - pm).abs() < 1e-9).unwrap_or(pm);
        // The table rounds some longitudes to 8 decimals (Paris in grads): 5e-8° tells them
        // apart, the closest two of PROJ's being degrees apart.
        let name = PRIME_MERIDIANS.iter().find(|(_, v)| (v - value).abs() < 5e-8).map(|(n, _)| n.to_string());
        tokens.push(format!("+pm={}", name.unwrap_or_else(|| number(value))));
    }
    tokens.extend(shift.map(str::to_string));
    tokens.extend(units);
    if web_mercator {
        tokens.push("+nadgrids=@null".into());
        tokens.push("+wktext".into());
    }
    finish(tokens)
}

/// PROJ's built-in ellipsoids (`proj_list_ellps`), in its order: the name, the semi-major axis,
/// and the inverse flattening or (negative) the semi-minor axis. PROJ writes a custom CRS's
/// ellipsoid by the first name whose values it has.
const ELLIPSOIDS: [(&str, f64, f64); 46] = [
    ("MERIT", 6378137.0, 298.257),
    ("SGS85", 6378136.0, 298.257),
    ("GRS80", 6378137.0, 298.257222101),
    ("IAU76", 6378140.0, 298.257),
    ("airy", 6377563.396, 299.3249646),
    ("APL4.9", 6378137.0, 298.25),
    ("NWL9D", 6378145.0, 298.25),
    ("mod_airy", 6377340.189, -6356034.446),
    ("andrae", 6377104.43, 300.0),
    ("danish", 6377019.2563, 300.0),
    ("aust_SA", 6378160.0, 298.25),
    ("GRS67", 6378160.0, 298.247167427),
    ("GSK2011", 6378136.5, 298.2564151),
    ("bessel", 6377397.155, 299.1528128),
    ("bess_nam", 6377483.865, 299.1528128),
    ("clrk66", 6378206.4, -6356583.8),
    ("clrk80", 6378249.145, 293.4663),
    ("clrk80ign", 6378249.2, 293.4660212936269),
    ("CPM", 6375738.7, 334.29),
    ("delmbr", 6376428.0, 311.5),
    ("engelis", 6378136.05, 298.2566),
    ("evrst30", 6377276.345, 300.8017),
    ("evrst48", 6377304.063, 300.8017),
    ("evrst56", 6377301.243, 300.8017),
    ("evrst69", 6377295.664, 300.8017),
    ("evrstSS", 6377298.556, 300.8017),
    ("fschr60", 6378166.0, 298.3),
    ("fschr60m", 6378155.0, 298.3),
    ("fschr68", 6378150.0, 298.3),
    ("helmert", 6378200.0, 298.3),
    ("hough", 6378270.0, 297.0),
    ("intl", 6378388.0, 297.0),
    ("krass", 6378245.0, 298.3),
    ("kaula", 6378163.0, 298.24),
    ("lerch", 6378139.0, 298.257),
    ("mprts", 6397300.0, 191.0),
    ("new_intl", 6378157.5, -6356772.2),
    ("plessis", 6376523.0, -6355863.0),
    ("PZ90", 6378136.0, 298.25784),
    ("SEasia", 6378155.0, -6356773.3205),
    ("walbeck", 6376896.0, -6355834.8467),
    ("WGS60", 6378165.0, 298.3),
    ("WGS66", 6378145.0, 298.25),
    ("WGS72", 6378135.0, 298.26),
    ("WGS84", 6378137.0, 298.257223563),
    ("sphere", 6370997.0, -6370997.0),
];

/// The shape of an ellipsoid besides its semi-major axis, as it was given (PROJ writes it so).
#[derive(Clone, Copy)]
enum Shape {
    Sphere,
    Rf(f64),
    B(f64),
}

/// The ellipsoid of PROJ's list of a name, with its shape as PROJ defines it.
fn named_ellipsoid(name: &str) -> Option<(f64, Shape)> {
    ELLIPSOIDS.iter().find(|(n, ..)| *n == name).map(|(_, a, x)| (*a, if *x > 0.0 { Shape::Rf(*x) } else { Shape::B(-x) }))
}

/// An ellipsoid's tokens as PROJ writes them: by the name of the first of its list with its
/// axes, else a sphere's radius, else the semi-major axis and the inverse flattening or the
/// semi-minor axis, as given.
fn ellipsoid_tokens(a: f64, shape: Shape) -> Vec<String> {
    let b = match shape {
        Shape::Sphere => a,
        Shape::Rf(rf) => a - a / rf,
        Shape::B(b) => b,
    };
    let known = ELLIPSOIDS.iter().find(|(_, ea, x)| {
        (ea - a).abs() < 1e-6 && if *x > 0.0 { b != a && (x - a / (a - b)).abs() < 1e-9 * x } else { (-x - b).abs() < 1e-6 }
    });
    match (known, shape) {
        (Some((name, ..)), _) => vec![format!("+ellps={name}")],
        (None, _) if b == a => vec![format!("+R={}", number(a))],
        (None, Shape::Rf(rf)) => vec![format!("+a={}", number(a)), format!("+rf={}", number(rf))],
        (None, _) => vec![format!("+a={}", number(a)), format!("+b={}", number(b))],
    }
}

/// The ellipsoid of a PROJ string's ellipsoid parameters as PROJ writes it (`pj_ell_set`'s
/// reading of them): `None` for a datum, or a named ellipsoid outside PROJ's list.
fn ellipsoid_of(proj: &str) -> Option<Vec<String>> {
    let get = |k: &str| proj.split_whitespace().find_map(|t| t.strip_prefix(k)?.strip_prefix('=')).map(str::to_string);
    let num = |k: &str| get(k).and_then(|v| v.parse::<f64>().ok());
    if get("+datum").is_some() {
        return None;
    }
    if let Some(r) = num("+R") {
        return Some(ellipsoid_tokens(r, Shape::Sphere));
    }
    let flattening = num("+rf")
        .map(|rf| if rf == 0.0 { Shape::Sphere } else { Shape::Rf(rf) })
        .or_else(|| num("+f").map(|f| if f == 0.0 { Shape::Sphere } else { Shape::Rf(1.0 / f) }))
        .or_else(|| {
            num("+es")
                .or_else(|| num("+e").map(|e| e * e))
                .map(|es| if es == 0.0 { Shape::Sphere } else { Shape::Rf(1.0 / (1.0 - (1.0 - es).sqrt())) })
        });
    let b = num("+b").map(Shape::B);
    let a = num("+a");
    let (a, shape) = match get("+ellps") {
        Some(name) => {
            let (ea, eshape) = named_ellipsoid(&name).unzip();
            // PROJ's string keeps the flattening of an ellipsoid it defines by its semi-minor
            // axis (Clarke 1866...), whatever else is given, but `+b`.
            let shape = match eshape {
                Some(Shape::B(_)) => b,
                _ => flattening.or(b),
            };
            // A named ellipsoid keeps its name, unless its axes are changed.
            if a.is_none() && shape.is_none() {
                return Some(vec![format!("+ellps={name}")]);
            }
            (a.or(ea)?, shape.or(eshape)?)
        }
        None => (a?, flattening.or(b).unwrap_or(Shape::Sphere)),
    };
    Some(ellipsoid_tokens(a, shape))
}

/// Web Mercator as PROJ writes it, from `+proj=webmerc`.
const WEB_MERCATOR: &str =
    "+proj=merc +a=6378137 +b=6378137 +lat_ts=0 +lon_0=0 +x_0=0 +y_0=0 +k=1 +units=m +nadgrids=@null +wktext +no_defs +type=crs";

/// A PROJ string in the form of GDAL's of Web Mercator, as PROJ writes it: Mercator on the
/// sphere of WGS 84's semi-major axis (given as `+a` and `+b`) with a null grid and a scale
/// factor of 1 given, which PROJ reads as Web Mercator, keeping the origin and the unit.
fn gdal_web_mercator(proj: &str) -> Option<String> {
    let (mut a, mut b, mut k, mut null) = (false, false, false, false);
    let (mut lon_0, mut x_0, mut y_0, mut units) = (0.0, 0.0, 0.0, "m".to_string());
    for t in strip_suffix(proj) {
        let (key, v) = t.split_once('=').unwrap_or((&t, ""));
        let n = v.parse::<f64>().ok();
        match (key, n) {
            ("+proj", _) if v == "merc" => {}
            ("+a", Some(6378137.0)) => a = true,
            ("+b", Some(6378137.0)) => b = true,
            ("+k" | "+k_0", Some(1.0)) => k = true,
            ("+lat_ts", Some(0.0)) => {}
            ("+lon_0", Some(n)) => lon_0 = n,
            ("+x_0", Some(n)) => x_0 = n,
            ("+y_0", Some(n)) => y_0 = n,
            ("+units", _) => units = v.to_string(),
            ("+nadgrids", _) if v == "@null" => null = true,
            ("+wktext", _) => {}
            _ => return None,
        }
    }
    (a && b && k && null).then(|| {
        format!(
            "+proj=merc +a=6378137 +b=6378137 +lat_ts=0 +lon_0={} +x_0={} +y_0={} +k=1 +units={units} +nadgrids=@null +wktext +no_defs +type=crs",
            number(lon_0),
            number(x_0),
            number(y_0)
        )
    })
}

/// PROJ's datums (`proj_list_datums`) and their ellipsoids: PROJ writes the three it names
/// (`datum_name`) by name, and the ellipsoid of the others.
const DATUMS: [(&str, &str); 10] = [
    ("WGS84", "+a=6378137 +rf=298.257223563"),
    ("GGRS87", "+a=6378137 +rf=298.257222101"),
    ("NAD83", "+a=6378137 +rf=298.257222101"),
    ("NAD27", "+a=6378206.4 +b=6356583.8"),
    ("potsdam", "+a=6377397.155 +rf=299.1528128"),
    ("carthage", "+a=6378249.2 +rf=293.4660212936269"),
    ("hermannskogel", "+a=6377397.155 +rf=299.1528128"),
    ("ire65", "+a=6377340.189 +rf=299.3249646"),
    ("nzgd49", "+a=6378388 +rf=297"),
    ("OSGB36", "+a=6377563.396 +rf=299.3249646"),
];

/// The zone and hemisphere of a Transverse Mercator that is a UTM zone, which PROJ writes as
/// `+proj=utm`: its parameters (but `+proj`) those of a zone, in metres.
fn utm_zone(params: &[String], unit_m: bool) -> Option<(u32, bool)> {
    let (mut lon_0, mut south, mut k, mut x_0) = (None, false, false, false);
    for t in &params[1..] {
        let (key, v) = t.split_once('=')?;
        match (key, v.parse::<f64>().ok()?) {
            ("+lat_0", 0.0) => {}
            ("+lon_0", n) => lon_0 = Some(n),
            ("+k" | "+k_0", 0.9996) => k = true,
            ("+x_0", 500000.0) => x_0 = true,
            ("+y_0", 0.0) => {}
            ("+y_0", 10000000.0) => south = true,
            _ => return None,
        }
    }
    let zone = (lon_0? + 183.0) / 6.0;
    (unit_m && k && x_0 && zone.fract() == 0.0 && (1.0..=60.0).contains(&zone)).then_some((zone as u32, south))
}

/// The parameters of a PROJ string's ellipsoid.
const ELLIPSOID_KEYS: [&str; 8] = ["+ellps", "+a", "+b", "+rf", "+f", "+es", "+e", "+R"];

/// A custom CRS's PROJ string (one it was given, or its WKT's) as PROJ writes it: its ellipsoid
/// by name when PROJ has the name, `+towgs84` with seven values, the prime meridian by name, the
/// unit of projected CRSs always given, and its order of these. `None` for a planetocentric
/// geographic CRS on an ellipsoid (`+geoc`), which PROJ cannot write.
fn custom(proj: &str, wkt_datum: Option<&str>) -> Option<String> {
    let key = |t: &str| t.split('=').next().unwrap_or_default().to_string();
    let value = |t: &str| t.split_once('=').map(|(_, v)| v.to_string()).unwrap_or_default();
    let method = proj.split_whitespace().find_map(|t| t.strip_prefix("+proj=")).unwrap_or_default().to_string();
    if method == "webmerc" {
        return Some(WEB_MERCATOR.into());
    }
    if let Some(web_mercator) = gdal_web_mercator(proj) {
        return Some(web_mercator);
    }
    let (mut params, mut datum, mut pm, mut towgs, mut nadgrids) = (Vec::new(), None, None, None, None);
    let (mut ellipsoid, mut units, mut to_meter, mut axis, mut geoc): (Vec<String>, _, _, _, _) = (Vec::new(), None, None, None, false);
    let (mut geoidgrids, mut geoid_crs, mut vertical) = (None, None, None);
    for t in strip_suffix(proj) {
        let v = value(&t);
        match key(&t).as_str() {
            "+datum" => datum = Some(t),
            k if ELLIPSOID_KEYS.contains(&k) => ellipsoid.push(t),
            "+towgs84" => towgs = Some(v),
            "+nadgrids" => nadgrids = Some(t),
            "+pm" => pm = Some(v),
            "+units" => units = Some(v),
            "+to_meter" => to_meter = v.parse::<f64>().ok(),
            "+axis" => axis = Some(v),
            "+geoc" => geoc = true,
            "+geoidgrids" => geoidgrids = Some(t),
            "+geoid_crs" => geoid_crs = Some(t),
            "+vunits" | "+vto_meter" => vertical = Some(t),
            // GDAL's flag to keep the PROJ string in a WKT, which PROJ drops.
            "+wktext" => {}
            _ => params.push(t),
        }
    }
    // Numbers as PROJ writes them (a WKT's in feet converted to metres: 152398.6327).
    let mut tokens: Vec<String> = params
        .into_iter()
        .map(|t| match t.split_once('=') {
            Some((k, v)) if k != "+proj" && v.parse::<f64>().is_ok_and(f64::is_finite) => {
                format!("{k}={}", number(v.parse().unwrap_or_default()))
            }
            // PROJ's name of geographic coordinates, of all their aliases.
            Some(("+proj", "latlong" | "lonlat" | "latlon")) => "+proj=longlat".into(),
            _ => t,
        })
        .collect();
    let unit_m = units.as_deref().is_none_or(|u| u == "m") && to_meter.is_none_or(|m| m == 1.0);
    if method == "tmerc"
        && let Some((zone, south)) = utm_zone(&tokens, unit_m)
    {
        tokens = vec!["+proj=utm".into(), format!("+zone={zone}")];
        if south {
            tokens.push("+south".into());
        }
    }
    if let Some(axis) = axis.filter(|a| kept_axis(&method, a)) {
        tokens.insert(1.min(tokens.len()), format!("+axis={axis}"));
    }
    // `+ellps` beside `+datum` is ignored, as PROJ ignores it.
    if datum.is_some() {
        ellipsoid.retain(|t| !t.starts_with("+ellps="));
    }
    // PROJ names a datum it names (WGS84, NAD83, NAD27: given, or a WKT's) when nothing changes
    // it: no shift (a shift makes it a bound CRS), no other shape of its ellipsoid, no other
    // prime meridian; else it writes the datum's ellipsoid. (A WKT's PROJ string gives its
    // datum's ellipsoid, too.)
    let unchanged = towgs.is_none() && nadgrids.is_none() && pm.is_none() && (ellipsoid.is_empty() || datum.is_none());
    let datum = datum.as_deref().map(value).or_else(|| wkt_datum.and_then(named_datum).map(str::to_string));
    // On a named ellipsoid or a datum's, PROJ's string drops a flattening given as `+f`, `+es`
    // or `+e` (keeping orthographic's `+f=0`, ESRI's sphere, as a parameter of its own).
    if datum.is_some() || ellipsoid.iter().any(|t| t.starts_with("+ellps=")) {
        let flattening = |t: &String| ["+f", "+es", "+e"].contains(&key(t).as_str());
        if method == "ortho" && ellipsoid.iter().any(|t| t == "+f=0") {
            tokens.insert(1.min(tokens.len()), "+f=0".into());
        }
        ellipsoid.retain(|t| !flattening(t));
    }
    let ellipsoid = match datum {
        Some(d) if unchanged && matches!(d.as_str(), "WGS84" | "NAD83" | "NAD27") => vec![format!("+datum={d}")],
        Some(d) => match DATUMS.iter().find(|(n, _)| *n == d) {
            // The ellipsoid's parameters given first win over the datum's.
            Some((_, axes)) => {
                // A shape given (`+rf`, `+b`) replaces the datum's, as its `+a` does.
                let shaped = ellipsoid.iter().any(|t| t.starts_with("+rf=") || t.starts_with("+b="));
                let axes = if shaped { axes.split_whitespace().next().unwrap_or_default() } else { axes };
                ellipsoid_of(&format!("{} {axes}", ellipsoid.join(" "))).unwrap_or(ellipsoid)
            }
            None if ellipsoid.is_empty() => vec![format!("+datum={d}")],
            None => ellipsoid_of(&ellipsoid.join(" ")).unwrap_or(ellipsoid),
        },
        None => ellipsoid_of(&ellipsoid.join(" ")).unwrap_or(ellipsoid),
    };
    // Planetocentric latitudes are written for a sphere, where they are geodetic ones.
    if geoc && !ellipsoid.iter().any(|t| t.starts_with("+R=") || t == "+ellps=sphere") {
        return None;
    }
    tokens.extend(ellipsoid);
    if let Some(pm) = pm {
        let name =
            pm.parse::<f64>().ok().and_then(|v| PRIME_MERIDIANS.iter().find(|(_, m)| (m - v).abs() < 5e-8)).map(|(n, _)| n.to_string());
        tokens.push(format!("+pm={}", name.unwrap_or(pm)));
    }
    // A grid shift wins over `+towgs84`.
    match (nadgrids, towgs) {
        (Some(grids), _) => tokens.push(grids),
        (None, Some(t)) => tokens.push(towgs84(&t)),
        (None, None) => {}
    }
    // A projected (or geocentric) CRS's unit is always written; a geographic CRS has none.
    if !matches!(method.as_str(), "longlat" | "latlong" | "lonlat" | "latlon") {
        let unit = units.or_else(|| to_meter.and_then(unit_name).map(str::to_string));
        tokens.push(match (unit, to_meter) {
            (Some(u), _) => format!("+units={u}"),
            (None, Some(m)) => format!("+to_meter={}", number(m)),
            (None, None) => "+units=m".into(),
        });
    }
    // A vertical unit is that of geoid heights, which only a geoid model gives.
    if let Some(grids) = geoidgrids {
        tokens.push(grids);
        tokens.push(geoid_crs.unwrap_or_else(|| "+geoid_crs=WGS84".into()));
        let to_meter = vertical.as_deref().and_then(|t| t.strip_prefix("+vto_meter=")).and_then(|v| v.parse::<f64>().ok());
        tokens.push(match to_meter {
            Some(m) => vertical_unit(m),
            None => vertical.unwrap_or_else(|| "+vunits=m".into()),
        });
    }
    Some(finish(tokens))
}

/// The tokens of a PROJ string without `+no_defs` and `+type=crs`.
fn strip_suffix(proj: &str) -> Vec<String> {
    proj.split_whitespace().filter(|t| *t != "+no_defs" && *t != "+type=crs").map(str::to_string).collect()
}

/// A PROJ string from its tokens, ended as PROJ ends a CRS's.
fn finish(mut tokens: Vec<String>) -> String {
    tokens.push("+no_defs".into());
    tokens.push("+type=crs".into());
    tokens.join(" ")
}
