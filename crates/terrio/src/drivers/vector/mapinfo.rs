//! MapInfo Interchange Format (`.mif` + `.mid`).
//!
//! Reading skips the style clauses (`Pen`, `Brush`, `Symbol`...), turns regions into polygons by
//! ring containment, rectangles and ellipses into polygons, arcs into lines and text objects into
//! points, as GDAL does. The `CoordSys` clause gives the CRS for longitude/latitude and the common
//! projections (Transverse Mercator, Lambert Conformal Conic, Albers, Mercator, Stereographic,
//! Lambert Azimuthal Equal Area) on the common datums. Writing produces UTF-8 MIF/MID files.

use crate::crs::SpatialRef;
use crate::{Error, Result};
use std::fmt::Write as _;
use std::path::Path;
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, Layer, Ring};

/// Reads a MIF file and the MID file next to it.
pub fn read(path: &str) -> Result<Layer> {
    let mif = std::fs::read(path)?;
    let mid_path = ["mid", "MID"].iter().map(|e| Path::new(path).with_extension(e)).find(|p| p.exists());
    let mid = mid_path.map(std::fs::read).transpose()?;
    let name = Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("layer");
    parse(&mif, mid.as_deref(), name)
}

fn decode(bytes: &[u8], utf8: bool) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) if utf8 => String::from_utf8_lossy(bytes).into_owned(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

/// A token of the MIF data section, with the line it is on.
struct Tokens {
    toks: Vec<(usize, String)>,
    i: usize,
}

impl Tokens {
    fn new(lines: &[&str]) -> Self {
        let mut toks = Vec::new();
        for (n, line) in lines.iter().enumerate() {
            let mut chars = line.trim().chars().peekable();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() || c == ',' || c == '(' || c == ')' {
                    chars.next();
                } else if c == '"' {
                    chars.next();
                    let mut s = String::from("\"");
                    while let Some(c) = chars.next() {
                        if c == '"' {
                            if chars.peek() == Some(&'"') {
                                chars.next();
                                s.push('"');
                                continue;
                            }
                            break;
                        }
                        s.push(c);
                    }
                    toks.push((n, s));
                } else {
                    let mut s = String::new();
                    while let Some(&c) = chars.peek() {
                        if c.is_whitespace() || c == ',' || c == '(' || c == ')' {
                            break;
                        }
                        s.push(c);
                        chars.next();
                    }
                    toks.push((n, s));
                }
            }
        }
        Self { toks, i: 0 }
    }

    fn peek(&self) -> Option<&str> {
        self.toks.get(self.i).map(|(_, t)| t.as_str())
    }

    fn next(&mut self) -> Option<String> {
        let t = self.toks.get(self.i).map(|(_, t)| t.clone());
        self.i += 1;
        t
    }

    fn num(&mut self) -> Option<f64> {
        self.next()?.parse().ok()
    }

    fn count(&mut self) -> Option<usize> {
        self.next()?.parse().ok()
    }

    fn coord(&mut self) -> Option<Coord> {
        Some(Coord::xy(self.num()?, self.num()?))
    }

    fn coords(&mut self, n: usize) -> Option<Vec<Coord>> {
        (0..n).map(|_| self.coord()).collect()
    }

    /// Skips the rest of the current token's line.
    fn skip_line(&mut self) {
        if let Some(&(line, _)) = self.toks.get(self.i) {
            while self.toks.get(self.i).is_some_and(|(l, _)| *l == line) {
                self.i += 1;
            }
        }
    }
}

const STYLES: &[&str] = &["pen", "brush", "symbol", "center", "smooth", "font", "spacing", "justify", "angle", "label", "linespacing"];

/// Parses MIF and MID contents.
pub fn parse(mif: &[u8], mid: Option<&[u8]>, name: &str) -> Result<Layer> {
    let head = String::from_utf8_lossy(&mif[..mif.len().min(4096)]).to_ascii_lowercase();
    let utf8 = head.contains("charset \"utf-8\"") || head.contains("charset \"utf8\"");
    let text = decode(mif, utf8);
    let lines: Vec<&str> = text.lines().collect();
    let mut layer = Layer::new(name);
    let mut delimiter = ',';
    let mut i = 0;
    let mut fields: Vec<(String, FieldType)> = Vec::new();
    while i < lines.len() {
        let line = lines[i].trim();
        let lower = line.to_ascii_lowercase();
        i += 1;
        if lower.starts_with("delimiter") {
            delimiter = line.split('"').nth(1).and_then(|d| d.chars().next()).unwrap_or(',');
        } else if lower.starts_with("coordsys") {
            if let Some(srs) = coordsys_to_srs(line) {
                srs.apply_to_layer(&mut layer);
            }
        } else if lower.starts_with("columns") {
            let n: usize = line.split_whitespace().nth(1).and_then(|n| n.parse().ok()).unwrap_or(0);
            for _ in 0..n {
                let Some(col) = lines.get(i) else { break };
                i += 1;
                let col = col.trim();
                let (fname, ftype) = col.split_once(char::is_whitespace).unwrap_or((col, "char"));
                let t = ftype.trim().to_ascii_lowercase();
                let t = if t.starts_with("char") {
                    FieldType::Text
                } else if t.starts_with("integer") || t.starts_with("smallint") || t.starts_with("largeint") {
                    FieldType::Integer
                } else if t.starts_with("float") || t.starts_with("decimal") {
                    FieldType::Float
                } else if t.starts_with("datetime") {
                    FieldType::DateTime
                } else if t.starts_with("date") {
                    FieldType::Date
                } else if t.starts_with("logical") {
                    FieldType::Boolean
                } else {
                    FieldType::Text
                };
                fields.push((fname.to_string(), t));
            }
        } else if lower == "data" {
            break;
        }
    }
    for (n, t) in &fields {
        layer.add_field(FieldDef::new(n, *t));
    }

    // Geometries.
    let mut toks = Tokens::new(&lines[i.min(lines.len())..]);
    let mut geometries = Vec::new();
    while let Some(t) = toks.peek() {
        let kw = t.to_ascii_lowercase();
        if STYLES.contains(&kw.as_str()) {
            toks.skip_line();
            continue;
        }
        match statement(&mut toks) {
            Some(g) => geometries.push(g),
            None => return Err(Error::InvalidData(format!("MIF: cannot read the geometry '{kw}'"))),
        }
    }

    // Attributes.
    let mid_text = mid.map(|m| decode(m, utf8)).unwrap_or_default();
    let mut rows = mid_text.lines().filter(|l| !l.trim().is_empty() || !fields.is_empty());
    for (fid, geometry) in geometries.into_iter().enumerate() {
        let values = rows.next().map(|r| split_mid(r, delimiter)).unwrap_or_default();
        let attributes = fields.iter().enumerate().map(|(k, (_, t))| values.get(k).map_or(FieldValue::Null, |v| value(v, *t))).collect();
        layer.push(Feature { fid: fid as u64, geometry, attributes });
    }
    Ok(layer)
}

/// Splits a MID line on the delimiter, honouring quotes (doubled inside quoted values).
fn split_mid(line: &str, delimiter: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cur.push('"');
                } else {
                    quoted = false;
                }
            } else {
                cur.push(c);
            }
        } else if c == '"' {
            quoted = true;
        } else if c == delimiter {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    out.push(cur);
    out
}

fn value(v: &str, t: FieldType) -> FieldValue {
    let s = v.trim();
    if s.is_empty() {
        return FieldValue::Null;
    }
    match t {
        FieldType::Integer => s.parse().map_or(FieldValue::Null, FieldValue::Integer),
        FieldType::Float => s.parse().map_or(FieldValue::Null, FieldValue::Float),
        FieldType::Boolean => FieldValue::Boolean(matches!(s, "T" | "t" | "1")),
        FieldType::Date if s.len() == 8 && s.bytes().all(|b| b.is_ascii_digit()) => {
            FieldValue::Date(format!("{}-{}-{}", &s[..4], &s[4..6], &s[6..]))
        }
        FieldType::DateTime if s.len() >= 14 && s.bytes().take(14).all(|b| b.is_ascii_digit()) => {
            FieldValue::DateTime(format!("{}-{}-{}T{}:{}:{}", &s[..4], &s[4..6], &s[6..8], &s[8..10], &s[10..12], &s[12..14]))
        }
        _ => FieldValue::Text(v.replace("\\n", "\n")),
    }
}

fn ellipse(x1: f64, y1: f64, x2: f64, y2: f64, from: f64, to: f64, n: usize) -> Vec<Coord> {
    let (cx, cy, rx, ry) = ((x1 + x2) / 2.0, (y1 + y2) / 2.0, (x2 - x1).abs() / 2.0, (y2 - y1).abs() / 2.0);
    let sweep = if to > from { to - from } else { to + 360.0 - from };
    (0..=n)
        .map(|k| {
            let a = (from + sweep * k as f64 / n as f64).to_radians();
            Coord::xy(cx + rx * a.cos(), cy + ry * a.sin())
        })
        .collect()
}

/// Reads one geometry statement. `Some(None)` is the `none` statement.
fn statement(t: &mut Tokens) -> Option<Option<Geometry>> {
    let kw = t.next()?.to_ascii_lowercase();
    Some(match kw.as_str() {
        "none" => None,
        "point" => Some(Geometry::Point(t.coord()?)),
        "line" => Some(Geometry::LineString(vec![t.coord()?, t.coord()?])),
        "pline" => {
            if t.peek().is_some_and(|w| w.eq_ignore_ascii_case("multiple")) {
                t.next();
                let n = t.count()?;
                let mut parts = Vec::with_capacity(n);
                for _ in 0..n {
                    let c = t.count()?;
                    parts.push(t.coords(c)?);
                }
                Some(Geometry::MultiLineString(parts))
            } else {
                let c = t.count()?;
                Some(Geometry::LineString(t.coords(c)?))
            }
        }
        "region" => {
            let n = t.count()?;
            let mut rings = Vec::with_capacity(n);
            for _ in 0..n {
                let c = t.count()?;
                rings.push(t.coords(c)?);
            }
            super::polygons_from_rings(rings)
        }
        "multipoint" => {
            let n = t.count()?;
            Some(Geometry::MultiPoint(t.coords(n)?))
        }
        "rect" | "roundrect" => {
            let (a, b) = (t.coord()?, t.coord()?);
            if kw == "roundrect" {
                t.num()?;
            }
            let ring = vec![Coord::xy(a.x, a.y), Coord::xy(b.x, a.y), Coord::xy(b.x, b.y), Coord::xy(a.x, b.y)];
            Some(Geometry::Polygon { exterior: Ring::new(ring), interiors: vec![] })
        }
        "ellipse" => {
            let (a, b) = (t.coord()?, t.coord()?);
            let mut ring = ellipse(a.x, a.y, b.x, b.y, 0.0, 360.0, 180);
            ring.pop();
            Some(Geometry::Polygon { exterior: Ring::new(ring), interiors: vec![] })
        }
        "arc" => {
            let (a, b) = (t.coord()?, t.coord()?);
            let (from, to) = (t.num()?, t.num()?);
            Some(Geometry::LineString(ellipse(a.x, a.y, b.x, b.y, from, to, 90)))
        }
        "text" => {
            // The text, then its bounding box: the point is the box's lower-left corner.
            if t.peek().is_some_and(|w| w.starts_with('"')) {
                t.next();
            }
            let a = t.coord()?;
            t.coord()?;
            Some(Geometry::Point(a))
        }
        "collection" => {
            let n = t.count()?;
            let mut parts = Vec::new();
            for _ in 0..n {
                while t.peek().is_some_and(|w| STYLES.contains(&w.to_ascii_lowercase().as_str())) {
                    t.skip_line();
                }
                if let Some(g) = statement(t)? {
                    parts.push(g);
                }
            }
            Some(Geometry::GeometryCollection(parts))
        }
        _ => return None,
    })
}

/// MapInfo datums: number, WKT datum name, ellipsoid name, semi-major axis, inverse flattening,
/// and the geographic EPSG code.
/// A MapInfo datum: number, WKT datum name, ellipsoid name, semi-major axis, inverse flattening,
/// and the geographic EPSG code.
type Datum = (u32, &'static str, &'static str, f64, f64, Option<u32>);

const DATUMS: &[Datum] = &[
    (104, "WGS_1984", "WGS 84", 6378137.0, 298.257223563, Some(4326)),
    (157, "WGS_1984", "WGS 84", 6378137.0, 298.257223563, Some(4326)),
    (74, "North_American_Datum_1983", "GRS 1980", 6378137.0, 298.257222101, Some(4269)),
    (62, "North_American_Datum_1927", "Clarke 1866", 6378206.4, 294.978698213898, Some(4267)),
    (116, "Geocentric_Datum_of_Australia_1994", "GRS 1980", 6378137.0, 298.257222101, Some(4283)),
    (115, "European_Terrestrial_Reference_System_1989", "GRS 1980", 6378137.0, 298.257222101, Some(4258)),
    (28, "European_Datum_1950", "International 1924", 6378388.0, 297.0, Some(4230)),
    (79, "OSGB_1936", "Airy 1830", 6377563.396, 299.3249646, Some(4277)),
    (33, "Reseau_Geodesique_Francais_1993", "GRS 1980", 6378137.0, 298.257222101, Some(4171)),
];

/// MapInfo unit names: their WKT name and size in metres.
const UNITS: &[(&str, &str, f64)] = &[
    ("m", "metre", 1.0),
    ("km", "kilometre", 1000.0),
    ("ft", "foot", 0.3048),
    ("survey ft", "US survey foot", 0.304800609601219),
    ("mi", "Statute mile", 1609.344),
    ("yd", "yard", 0.9144),
    ("cm", "centimetre", 0.01),
    ("mm", "millimetre", 0.001),
];

/// WKT 1 projection names and the parameters of MapInfo projections, in clause order.
const PROJECTIONS: &[(u32, &str, &[&str])] = &[
    (8, "Transverse_Mercator", &["central_meridian", "latitude_of_origin", "scale_factor", "false_easting", "false_northing"]),
    (
        3,
        "Lambert_Conformal_Conic_2SP",
        &["central_meridian", "latitude_of_origin", "standard_parallel_1", "standard_parallel_2", "false_easting", "false_northing"],
    ),
    (
        9,
        "Albers_Conic_Equal_Area",
        &["longitude_of_center", "latitude_of_center", "standard_parallel_1", "standard_parallel_2", "false_easting", "false_northing"],
    ),
    (10, "Mercator_1SP", &["central_meridian"]),
    (20, "Oblique_Stereographic", &["central_meridian", "latitude_of_origin", "scale_factor", "false_easting", "false_northing"]),
    // All origin latitudes (29) first, for writing; 4 is the polar aspect only.
    (29, "Lambert_Azimuthal_Equal_Area", &["longitude_of_center", "latitude_of_center"]),
    (4, "Lambert_Azimuthal_Equal_Area", &["longitude_of_center", "latitude_of_center"]),
];

fn geogcs(d: &Datum) -> String {
    format!(
        "GEOGCS[\"{0}\",DATUM[\"{0}\",SPHEROID[\"{1}\",{2},{3}]],PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433]]",
        d.1, d.2, d.3, d.4
    )
}

/// The CRS of a `CoordSys Earth Projection ...` clause, for the common projections and datums.
pub(super) fn coordsys_to_srs(line: &str) -> Option<SpatialRef> {
    let lower = line.to_ascii_lowercase();
    let rest = lower.split("projection").nth(1)?;
    let rest = rest.split("bounds").next()?.split("affine").next()?;
    let mut parts = rest.split(',').map(str::trim);
    let proj: u32 = parts.next()?.parse::<u32>().ok()? % 1000;
    let datum_code: u32 = parts.next()?.parse().ok()?;
    let datum = DATUMS.iter().find(|d| d.0 == datum_code)?;
    if proj == 1 {
        return match datum.5 {
            Some(code) => SpatialRef::from_epsg(code).ok(),
            None => Some(SpatialRef::from_wkt(&geogcs(datum))),
        };
    }
    if proj == 10 && datum_code == 157 {
        return SpatialRef::from_epsg(3857).ok();
    }
    let unit_name = parts.next()?.trim_matches('"');
    let unit = UNITS.iter().find(|u| u.0 == unit_name)?;
    let values: Vec<f64> = parts.filter_map(|v| v.parse().ok()).collect();
    let (_, name, params) = PROJECTIONS.iter().find(|p| p.0 == proj)?;
    if values.len() < params.len() {
        return None;
    }
    // UTM zones are known by their EPSG codes.
    if proj == 8 && unit.2 == 1.0 && values[1] == 0.0 && values[2] == 0.9996 && values[3] == 500000.0 {
        let zone = (values[0] + 183.0) / 6.0;
        let north = values[4] == 0.0;
        if zone.fract() == 0.0 && (1.0..=60.0).contains(&zone) {
            let zone = zone as u32;
            let code = match (datum_code, north) {
                (104, true) => Some(32600 + zone),
                (104, false) => Some(32700 + zone),
                (74, true) if zone <= 23 => Some(26900 + zone),
                _ => None,
            };
            if let Some(code) = code {
                return SpatialRef::from_epsg(code).ok();
            }
        }
    }
    // Lambert-93, the one Lambert Conformal Conic CRS GDAL identifies from MapInfo parameters.
    let near = |a: f64, b: f64| (a - b).abs() < 1e-10;
    if proj == 3
        && datum_code == 33
        && near(values[0], 3.0)
        && near(values[1], 46.5)
        && near(values[2], 44.0)
        && [49.0, 49.00000000001, 49.00000000002].iter().any(|v| near(values[3], *v))
    {
        return SpatialRef::from_epsg(2154).ok();
    }
    let mut wkt = format!("PROJCS[\"unnamed\",{},PROJECTION[\"{name}\"]", geogcs(datum));
    for (p, v) in params.iter().zip(&values) {
        let _ = write!(wkt, ",PARAMETER[\"{p}\",{v}]");
    }
    if matches!(proj, 10 | 4 | 29) {
        wkt.push_str(",PARAMETER[\"false_easting\",0],PARAMETER[\"false_northing\",0]");
    }
    let _ = write!(wkt, ",UNIT[\"{}\",{}]]", unit.1, unit.2);
    Some(SpatialRef::from_wkt(&wkt))
}

/// The value of a WKT 1 `PARAMETER`.
fn wkt_param(wkt: &str, name: &str) -> Option<f64> {
    let lower = wkt.to_ascii_lowercase();
    let at = lower.find(&format!("parameter[\"{name}\""))?;
    let rest = &wkt[at..];
    rest.split(',').nth(1)?.trim_end_matches(']').trim().parse().ok()
}

/// The `CoordSys` clause for a CRS: longitude/latitude, UTM zones, Web Mercator, and the
/// projections of [`PROJECTIONS`] on the datums of [`DATUMS`]; other CRSs are written as
/// non-earth coordinates.
pub(super) fn srs_to_coordsys(srs: &SpatialRef, extent: Option<(f64, f64, f64, f64)>) -> String {
    let nonearth = || {
        let (x0, y0, x1, y1) = extent.unwrap_or((0.0, 0.0, 1.0, 1.0));
        format!("NonEarth Units \"m\" Bounds ({x0}, {y0}) ({x1}, {y1})")
    };
    let code = srs.epsg.unwrap_or(0);
    if code == 2154 {
        // Lambert-93 on RGF93, as GDAL writes it (the registry puts it on ETRS89).
        return "Earth Projection 3, 33, \"m\", 3, 46.5, 44, 49, 700000, 6600000".into();
    }
    if code == 3857 {
        return "Earth Projection 10, 157, \"m\", 0".into();
    }
    if let Some(d) = DATUMS.iter().find(|d| d.5 == Some(code) && d.0 != 157) {
        return format!("Earth Projection 1, {}", d.0);
    }
    let Some(wkt) = srs.to_wkt() else { return nonearth() };
    let lower = wkt.to_ascii_lowercase().replace(['_', ' '], "");
    let datum = DATUMS.iter().filter(|d| d.0 != 157).find(|d| {
        let n = d.1.to_ascii_lowercase().replace('_', "");
        lower.contains(&n)
            || (d.0 == 104 && lower.contains("wgs84"))
            || (d.0 == 74 && (lower.contains("nad83") || lower.contains("northamerican1983")))
            || (d.0 == 62 && (lower.contains("nad27") || lower.contains("northamerican1927")))
            || (d.0 == 115 && lower.contains("etrs89"))
            || (d.0 == 33 && lower.contains("rgf93"))
    });
    let Some(datum) = datum else { return nonearth() };
    let projection = PROJECTIONS.iter().find(|(_, name, _)| {
        let n = name.to_ascii_lowercase().replace('_', "");
        lower.contains(&format!("projection[\"{n}"))
            || (*name == "Lambert_Conformal_Conic_2SP" && lower.contains("projection[\"lambertconformalconic\""))
    });
    let Some((proj, _, params)) = projection else { return nonearth() };
    // The linear unit: the last UNIT of the definition.
    let unit_size = wkt
        .rfind("UNIT[")
        .and_then(|i| wkt[i..].split(',').nth(1))
        .and_then(|v| v.trim_end_matches(']').trim().parse::<f64>().ok())
        .unwrap_or(1.0);
    let Some(unit) = UNITS.iter().find(|u| (u.2 - unit_size).abs() < 1e-9 * u.2.max(1.0)) else { return nonearth() };
    fn alias(p: &str) -> [&str; 3] {
        match p {
            "longitude_of_center" => ["longitude_of_center", "central_meridian", "longitude_of_origin"],
            "latitude_of_center" => ["latitude_of_center", "latitude_of_origin", "standard_parallel_1"],
            "central_meridian" => ["central_meridian", "longitude_of_center", "longitude_of_origin"],
            "latitude_of_origin" => ["latitude_of_origin", "latitude_of_center", "latitude_of_origin"],
            other => [other, other, other],
        }
    }
    let mut values = Vec::new();
    for p in *params {
        let v = alias(p).iter().find_map(|a| wkt_param(&wkt, a)).or(match *p {
            "latitude_of_origin" | "latitude_of_center" | "false_easting" | "false_northing" => Some(0.0),
            "scale_factor" => Some(1.0),
            _ => None,
        });
        match v {
            Some(v) => values.push(v.to_string()),
            None => return nonearth(),
        }
    }
    format!("Earth Projection {proj}, {}, \"{}\", {}", datum.0, unit.0, values.join(", "))
}

/// Writes a layer as MIF/MID: `path` names the `.mif` file.
pub fn write(layer: &Layer, path: &str) -> Result<()> {
    let (mif, mid) = to_strings(layer)?;
    std::fs::write(path, mif)?;
    std::fs::write(Path::new(path).with_extension("mid"), mid)?;
    Ok(())
}

pub(super) fn column_name(name: &str) -> String {
    let mut s: String = name.chars().map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' }).collect();
    if !s.starts_with(|c: char| c.is_alphabetic() || c == '_') {
        s.insert(0, '_');
    }
    s.chars().take(31).collect()
}

fn to_strings(layer: &Layer) -> Result<(String, String)> {
    let srs = SpatialRef::from_layer(layer);
    let mut mif = String::from("Version 300\nCharset \"UTF-8\"\nDelimiter \",\"\n");
    let _ = writeln!(mif, "CoordSys {}", srs_to_coordsys(&srs, crate::vector::layer_extent(layer)));
    let _ = writeln!(mif, "Columns {}", layer.schema.len());
    for (i, f) in layer.schema.fields().iter().enumerate() {
        let t = match f.field_type {
            FieldType::Integer => {
                let large = layer.features.iter().filter_map(|ft| ft.attributes.get(i)?.as_i64()).any(|v| i32::try_from(v).is_err());
                if large { "LargeInt".to_string() } else { "Integer".to_string() }
            }
            FieldType::Float => "Float".into(),
            FieldType::Date => "Date".into(),
            FieldType::DateTime => "DateTime".into(),
            FieldType::Boolean => "Logical".into(),
            _ => {
                let longest = layer.features.iter().filter_map(|ft| ft.attributes.get(i)?.as_str().map(str::len)).max().unwrap_or(1);
                format!("Char({})", if f.width > 0 { f.width.max(longest) } else { 254.max(longest) }.min(254))
            }
        };
        let _ = writeln!(mif, "  {} {t}", column_name(&f.name));
    }
    mif.push_str("Data\n\n");
    let mut mid = String::new();
    for f in &layer.features {
        match &f.geometry {
            Some(g) => write_geometry(&mut mif, g),
            None => mif.push_str("none\n"),
        }
        let cells: Vec<String> = layer
            .schema
            .fields()
            .iter()
            .zip(&f.attributes)
            .map(|(d, v)| match v {
                FieldValue::Null if d.field_type == FieldType::Text => "\"\"".into(),
                FieldValue::Null => String::new(),
                FieldValue::Integer(x) => x.to_string(),
                FieldValue::Float(x) => x.to_string(),
                FieldValue::Boolean(b) => (if *b { "T" } else { "F" }).into(),
                FieldValue::Date(s) if d.field_type == FieldType::Date => s.chars().filter(char::is_ascii_digit).take(8).collect(),
                FieldValue::DateTime(s) if d.field_type == FieldType::DateTime => {
                    let digits: String = s.chars().filter(char::is_ascii_digit).take(17).collect();
                    format!("{digits:0<17}")
                }
                FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => {
                    format!("\"{}\"", s.replace('"', "\"\"").replace('\n', "\\n"))
                }
                FieldValue::Blob(_) => String::new(),
            })
            .collect();
        mid.push_str(&cells.join(","));
        mid.push('\n');
    }
    Ok((mif, mid))
}

fn write_coords(s: &mut String, cs: &[Coord], close: bool) {
    let first = cs.first().filter(|f| close && cs.len() > 1 && cs.last().map(|l| (l.x, l.y)) != Some((f.x, f.y)));
    let _ = writeln!(s, "  {}", cs.len() + usize::from(first.is_some()));
    for c in cs.iter().chain(first) {
        let _ = writeln!(s, "{} {}", c.x, c.y);
    }
}

fn write_geometry(s: &mut String, g: &Geometry) {
    match g {
        Geometry::Point(c) => {
            let _ = writeln!(s, "Point {} {}", c.x, c.y);
        }
        Geometry::MultiPoint(cs) => {
            let _ = writeln!(s, "Multipoint {}", cs.len());
            for c in cs {
                let _ = writeln!(s, "{} {}", c.x, c.y);
            }
        }
        Geometry::LineString(cs) => {
            s.push_str("Pline");
            write_coords(s, cs, false);
        }
        Geometry::MultiLineString(ls) => {
            let _ = writeln!(s, "Pline Multiple {}", ls.len());
            for l in ls {
                write_coords(s, l, false);
            }
        }
        Geometry::Polygon { exterior, interiors } => {
            let _ = writeln!(s, "Region {}", 1 + interiors.len());
            for r in std::iter::once(exterior).chain(interiors) {
                write_coords(s, &r.0, true);
            }
        }
        Geometry::MultiPolygon(ps) => {
            let _ = writeln!(s, "Region {}", ps.iter().map(|(_, h)| 1 + h.len()).sum::<usize>());
            for (e, hs) in ps {
                for r in std::iter::once(e).chain(hs) {
                    write_coords(s, &r.0, true);
                }
            }
        }
        Geometry::GeometryCollection(gs) => {
            let _ = writeln!(s, "Collection {}", gs.len());
            for g in gs {
                write_geometry(s, g);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_styles_regions_and_crs() {
        let mif = b"Version 300\nCharset \"WindowsLatin1\"\nDelimiter \",\"\nCoordSys Earth Projection 8, 104, \"m\", -75, 0, 0.9996, 500000, 0\nColumns 3\n  NAME Char (50)\n  N Integer\n  D Date\nData\n\nRegion 2\n5\n0 0\n10 0\n10 10\n0 10\n0 0\n4\n2 2\n4 2\n4 4\n2 2\n    Pen (1,2,0)\n    Brush (1,0,16777215)\nPoint 1 2\n    Symbol (35,0,12)\nPline Multiple 2\n2\n0 0\n1 1\n2\n2 2\n3 3\nnone\n";
        let mid = b"\"Gu\xe9rin \"\"A\"\"\",4,20200102\n\"b\",,\n\"c\",1,\n\"d\",2,\n";
        let l = parse(mif, Some(mid), "t").unwrap();
        assert_eq!(l.crs_epsg(), Some(32618));
        assert_eq!(l.len(), 4);
        let Some(Geometry::Polygon { interiors, .. }) = &l.features[0].geometry else { panic!("no polygon") };
        assert_eq!(interiors.len(), 1);
        assert_eq!(l.features[1].geometry, Some(Geometry::point(1.0, 2.0)));
        assert!(matches!(&l.features[2].geometry, Some(Geometry::MultiLineString(p)) if p.len() == 2));
        assert_eq!(l.features[3].geometry, None);
        assert_eq!(l.features[0].attributes[0], FieldValue::Text("Guérin \"A\"".into()));
        assert_eq!(l.features[0].attributes[2], FieldValue::Date("2020-01-02".into()));
        assert_eq!(l.features[1].attributes[1], FieldValue::Null);

        let (mif, mid) = to_strings(&l).unwrap();
        let back = parse(mif.as_bytes(), Some(mid.as_bytes()), "t").unwrap();
        assert_eq!(back.crs_epsg(), Some(32618));
        for (a, b) in back.features.iter().zip(&l.features) {
            assert_eq!(a.geometry, b.geometry);
            assert_eq!(a.attributes, b.attributes);
        }
    }
}
