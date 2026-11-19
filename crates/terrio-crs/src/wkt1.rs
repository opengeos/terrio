// Written to match the behavior of:
//   PROJ 9.8.1:
//     src/iso19111/io.cpp
//   GDAL v3.13.3:
//     ogr/ogr_srs_esri.cpp
// No code is translated from these sources.

//! A WKT 1 (OGC and ESRI) reader that turns a CRS definition into a PROJ string, as GDAL does,
//! with its linear and angular units.

/// A WKT node: `KEYWORD[arg, arg, ...]`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Arg {
    Str(String),
    Num(f64),
    Ident(String),
    Node(Node),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Node {
    pub key: String,
    pub args: Vec<Arg>,
}

impl Node {
    pub fn to_wkt(&self) -> String {
        let args: Vec<String> = self
            .args
            .iter()
            .map(|a| match a {
                Arg::Str(s) => format!("\"{}\"", s.replace('"', "\"\"")),
                Arg::Num(v) => v.to_string(),
                Arg::Ident(s) => s.clone(),
                Arg::Node(n) => n.to_wkt(),
            })
            .collect();
        format!("{}[{}]", self.key, args.join(","))
    }

    /// The first child node with a keyword.
    pub fn child(&self, key: &str) -> Option<&Node> {
        self.args.iter().find_map(|a| match a {
            Arg::Node(n) if n.key.eq_ignore_ascii_case(key) => Some(n),
            _ => None,
        })
    }

    /// The first child node with a keyword, to change.
    pub fn child_mut(&mut self, key: &str) -> Option<&mut Node> {
        self.args.iter_mut().find_map(|a| match a {
            Arg::Node(n) if n.key.eq_ignore_ascii_case(key) => Some(n),
            _ => None,
        })
    }

    /// The child nodes with a keyword.
    pub fn children<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.args.iter().filter_map(move |a| match a {
            Arg::Node(n) if n.key.eq_ignore_ascii_case(key) => Some(n),
            _ => None,
        })
    }

    /// Whether the node's keyword is one of `keys` (see [`crate::keywords`]).
    pub fn is(&self, keys: &[&str]) -> bool {
        crate::keywords::is(&self.key, keys)
    }

    /// The first child node with the first of `keys` the node has one with.
    pub fn child_in(&self, keys: &[&str]) -> Option<&Node> {
        keys.iter().find_map(|k| self.child(k))
    }

    /// The first child node, in the node's order, whose keyword is one of `keys`: PROJ 9.9.0 reads
    /// the first of a CRS's datum nodes (`DATUM`, `TRF` or `GEODETICDATUM`), of a datum's
    /// ellipsoid nodes and of a projected CRS's base CRS nodes, whatever their keyword.
    pub fn first_in(&self, keys: &[&str]) -> Option<&Node> {
        self.args.iter().find_map(|a| match a {
            Arg::Node(n) if n.is(keys) => Some(n),
            _ => None,
        })
    }

    /// The geodetic datum or datum ensemble of a geodetic CRS node, the first in its order (see
    /// [`Self::first_in`]; PROJ refuses a CRS with both a datum and an ensemble, which terrio
    /// reads by the first).
    pub fn datum(&self) -> Option<&Node> {
        self.first_in(crate::keywords::GEODETIC_DATUM)
    }

    /// The base geodetic CRS of a projected (or derived) CRS node: WKT 1's `GEOGCS`, WKT 2's
    /// `BASEGEOGCRS` or `BASEGEODCRS` (terrio's two included), the first in its order.
    pub fn geodetic_base(&self) -> Option<&Node> {
        self.first_in(crate::keywords::GEODETIC_BASE)
    }

    /// The node itself or the first node below it, depth first, whose keyword is one of `keys`.
    pub fn find(&self, keys: &[&str]) -> Option<&Node> {
        if self.is(keys) {
            return Some(self);
        }
        self.args.iter().find_map(|a| match a {
            Arg::Node(c) => c.find(keys),
            _ => None,
        })
    }

    /// The node's identifiers (its `AUTHORITY` and `ID` children), in the order the node has them.
    pub fn ids(&self) -> impl Iterator<Item = &Node> {
        self.args.iter().filter_map(|a| match a {
            Arg::Node(n) if n.is(crate::keywords::ID) => Some(n),
            _ => None,
        })
    }

    /// The node's name (its first argument, when a string).
    pub fn name(&self) -> Option<&str> {
        match self.args.first() {
            Some(Arg::Str(s)) => Some(s),
            _ => None,
        }
    }

    /// The `i`th argument as a number.
    pub fn num(&self, i: usize) -> Option<f64> {
        match self.args.get(i) {
            Some(Arg::Num(v)) => Some(*v),
            Some(Arg::Str(s)) => s.trim().parse().ok(),
            _ => None,
        }
    }

    /// The code of an `ID` or `AUTHORITY` node: its second argument, a whole number or the text of
    /// one.
    pub fn code(&self) -> Option<u32> {
        match self.args.get(1)? {
            Arg::Num(v) if v.fract() == 0.0 && *v >= 0.0 && *v <= f64::from(u32::MAX) => Some(*v as u32),
            Arg::Str(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    /// The semi-major axis in metres and the inverse flattening (0 for a sphere) of an ellipsoid
    /// node (`SPHEROID` or `ELLIPSOID`): its axis in its `LENGTHUNIT` (WKT 2, as PROJ reads
    /// `LENGTHUNIT["Clarke's foot",0.3047972654]`), else in metres.
    pub fn ellipsoid(&self) -> Option<(f64, f64)> {
        let unit = match self.child_in(&["LENGTHUNIT", "UNIT"]) {
            Some(u) => u.num(1).filter(|v| v.is_finite() && *v > 0.0)?,
            None => 1.0,
        };
        let (a, rf) = (self.num(1)? * unit, self.num(2)?);
        (a.is_finite() && a > 0.0 && rf.is_finite() && rf >= 0.0).then_some((a, rf))
    }

    /// The EPSG code of the node's `AUTHORITY` or `ID`.
    pub fn epsg(&self) -> Option<u32> {
        let n = self.child_in(crate::keywords::ID)?;
        if !n.name()?.eq_ignore_ascii_case("EPSG") {
            return None;
        }
        n.code()
    }
}

/// The nesting PROJ's WKT parser allows (`WKTNode::createFrom` refuses a value at `recLevel`
/// 16), which GDAL reads WKT with: the root is at level 0 and every value (a node, a string, a
/// number or a word) inside a node at level `n` is at level `n + 1`.
pub(crate) const MAX_LEVELS: usize = 16;

/// The longest WKT GDAL imports (`OGRSpatialReference::importFromWkt` refuses more than 100,000
/// bytes unless `OSR_IMPORT_FROM_WKT_LIMIT=NO`).
pub(crate) const MAX_LEN: usize = 100_000;

/// Parses WKT into its tree, as PROJ 9.8.1's WKT parser (and so GDAL) accepts it: `[` and `(` open a
/// node, and either `]` or `)` closes it whichever opened it (`SPHEROID["WGS 84",6378137,
/// 298.257223563)` is WGS 84's spheroid for GDAL). `None` for text that is not WKT, WKT nested
/// deeper than PROJ allows, or longer than GDAL imports. Every step of the parser consumes
/// input or fails, and its recursion is bounded by [`MAX_LEVELS`].
pub(crate) fn parse(s: &str) -> Option<Node> {
    if s.len() > MAX_LEN {
        return None;
    }
    let b = s.as_bytes();
    let mut i = 0;
    node(b, &mut i, 0)
}

/// The horizontal and vertical CRS of ESRI's compound CRS, a WKT 1 `GEOGCS` or `PROJCS` followed
/// by a `VERTCS` (`GEOGCS[...],VERTCS[...]`, as ESRI writes EPSG:9707), which PROJ 9.9.0 reads as
/// a compound CRS; anything after the `VERTCS` is ignored, as by PROJ. `None` for another text.
pub(crate) fn esri_compound(s: &str) -> Option<(Node, Node)> {
    if s.len() > MAX_LEN {
        return None;
    }
    let b = s.as_bytes();
    let mut i = 0;
    let horizontal = node(b, &mut i, 0)?;
    if !["GEOGCS", "PROJCS"].iter().any(|k| horizontal.key.eq_ignore_ascii_case(k)) {
        return None;
    }
    skip_ws(b, &mut i);
    if b.get(i) != Some(&b',') {
        return None;
    }
    i += 1;
    skip_ws(b, &mut i);
    let vertical = node(b, &mut i, 0)?;
    vertical.key.eq_ignore_ascii_case("VERTCS").then_some((horizontal, vertical))
}

/// Whether `c` is a space as C's `isspace` reads it, which PROJ 9.8.1's parser skips: ASCII
/// whitespace and the vertical tab.
fn space(c: u8) -> bool {
    c.is_ascii_whitespace() || c == 0x0b
}

fn skip_ws(b: &[u8], i: &mut usize) {
    while *i < b.len() && space(b[*i]) {
        *i += 1;
    }
}

/// Moves `*i` past the quoted run that opens at `b[*i]`, a doubled quote inside it a quote.
/// `None` when the quote is not closed.
fn skip_quoted(b: &[u8], i: &mut usize) -> Option<()> {
    *i += 1;
    loop {
        match *b.get(*i)? {
            b'"' if b.get(*i + 1) == Some(&b'"') => *i += 2,
            b'"' => {
                *i += 1;
                return Some(());
            }
            _ => *i += 1,
        }
    }
}

/// The node at `b[*i..]`, at nesting `level` (see [`MAX_LEVELS`]); `*i` ends after its closer.
fn node(b: &[u8], i: &mut usize, level: usize) -> Option<Node> {
    if level >= MAX_LEVELS {
        return None;
    }
    skip_ws(b, i);
    // A keyword is any run of characters up to a space or a delimiter, a quoted run in it
    // included, as PROJ 9.8.1's `WKTNode::createFrom` reads it: an unknown node is skipped
    // whatever its keyword (`vendor:mark[...]`, `a-b[...]`, `a"b c"[...]`). A NUL outside quotes
    // is refused: PROJ's text ends there, which leaves the WKT unclosed.
    let start = *i;
    while *i < b.len() && !space(b[*i]) && !matches!(b[*i], b',' | b'[' | b'(' | b']' | b')') {
        match b[*i] {
            b'"' => skip_quoted(b, i)?,
            0 => return None,
            _ => *i += 1,
        }
    }
    let key = std::str::from_utf8(&b[start..*i]).ok()?.to_string();
    skip_ws(b, i);
    if *i >= b.len() || !(b[*i] == b'[' || b[*i] == b'(') {
        return None;
    }
    *i += 1;
    let mut args = Vec::new();
    loop {
        skip_ws(b, i);
        let before = *i;
        match b.get(*i)? {
            // Either closer ends the node, as in PROJ 9.8.1.
            b']' | b')' => {
                *i += 1;
                break;
            }
            b',' => {
                *i += 1;
                continue;
            }
            _ => {}
        }
        // A value of this node, one level deeper.
        if level + 1 >= MAX_LEVELS {
            return None;
        }
        if b[*i] == b'"' {
            *i += 1;
            let mut s = Vec::new();
            loop {
                match *b.get(*i)? {
                    b'"' if b.get(*i + 1) == Some(&b'"') => {
                        s.push(b'"');
                        *i += 2;
                    }
                    b'"' => {
                        *i += 1;
                        break;
                    }
                    c => {
                        s.push(c);
                        *i += 1;
                    }
                }
            }
            args.push(Arg::Str(String::from_utf8_lossy(&s).into_owned()));
        } else {
            // A number, a word or a nested node: the scan stops at a delimiter outside quotes,
            // which is not the first byte (handled above) unless it opens a nested node. As in
            // PROJ 9.8.1, a quote in a value runs to the next quote, delimiters included:
            // `b"c,d"e` is one value and `b"],y["c` too, and a quote left unclosed refuses the
            // WKT. A NUL outside quotes is refused, as in a keyword.
            let start = *i;
            while *i < b.len() && !matches!(b[*i], b',' | b'[' | b'(' | b']' | b')') {
                match b[*i] {
                    b'"' => skip_quoted(b, i)?,
                    0 => return None,
                    _ => *i += 1,
                }
            }
            let tok = std::str::from_utf8(&b[start..*i]).ok()?.trim();
            if *i < b.len() && (b[*i] == b'[' || b[*i] == b'(') {
                *i = start;
                args.push(Arg::Node(node(b, i, level + 1)?));
            } else if let Ok(v) = tok.parse::<f64>() {
                args.push(Arg::Num(v));
            } else {
                args.push(Arg::Ident(tok.to_string()));
            }
        }
        // Each value consumed input: a parser that stops advancing fails rather than looping.
        if *i <= before {
            return None;
        }
    }
    Some(Node { key, args })
}

/// Whether a WKT 1 is on a polar stereographic method the reader refuses for its latitude: GDAL's
/// `Polar_Stereographic` without a finite `latitude_of_origin` (the first, as the reader takes
/// it), which PROJ 9.8.1 refuses rather than read at the north pole, or EPSG's variant B with a
/// standard parallel given that is not a finite number.
pub(crate) fn polar_latitude_refused(root: &Node) -> bool {
    // The first parameter of a name, as the reader takes it: `None` when absent, `Some(None)`
    // when not a number.
    let first = |name: &str| root.children("PARAMETER").find(|p| p.name().is_some_and(|n| norm(n) == name)).map(|p| p.num(1));
    match root.child("PROJECTION").and_then(Node::name).map(norm).as_deref() {
        Some("polarstereographic") => !first("latitudeoforigin").flatten().is_some_and(f64::is_finite),
        // Given but not a finite number (PROJ 9.8.1 refuses "abc" too).
        Some("polarstereographicvariantb") => first("latitudeofstandardparallel").is_some_and(|v| !v.is_some_and(f64::is_finite)),
        _ => false,
    }
}

pub(crate) fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase()
}

/// The prime meridians PROJ 9.8.1 reads in degrees by name whatever the WKT's unit
/// (`primeMeridiansDMS`), by degrees, minutes and seconds. Not its `+pm=` names
/// (`math::PRIME_MERIDIANS`): it has no Paris or Copenhagen (which stay in the CRS's unit), adds
/// Paris RGS, and its Madrid is at 3°41'14.55"W.
const PRIME_MERIDIANS_DMS: [(&str, f64, f64, f64); 13] = [
    ("Lisbon", -9.0, 7.0, 54.862),
    ("Bogota", -74.0, 4.0, 51.3),
    ("Madrid", -3.0, 41.0, 14.55),
    ("Rome", 12.0, 27.0, 8.4),
    ("Bern", 7.0, 26.0, 22.5),
    ("Jakarta", 106.0, 48.0, 27.79),
    ("Ferro", -17.0, 40.0, 0.0),
    ("Brussels", 4.0, 22.0, 4.71),
    ("Stockholm", 18.0, 3.0, 29.8),
    ("Athens", 23.0, 42.0, 58.815),
    ("Oslo", 10.0, 43.0, 22.5),
    ("Paris RGS", 2.0, 20.0, 13.95),
    ("Paris_RGS", 2.0, 20.0, 13.95),
];

/// The longitude in degrees of a WKT `PRIMEM` named `name` with the value `value`, in a CRS
/// whose angular unit is `ang` degrees, as PROJ 9.8.1 reads it (`buildPrimeMeridian`): in that
/// unit, but for Paris in a CRS in grads, which GDAL and ESRI write in degrees (2.33722917,
/// Paris's 2.5969213 grads, to within 1e-8), and for the meridians PROJ knows by name and value
/// in degrees (`Paris RGS`, 2.33720833, in ATF (Paris), also in grads) or in EPSG's sexagesimal
/// degrees (`Ferro` at -17.40, 17°40'W), whatever the unit.
pub(crate) fn prime_meridian_degrees(name: &str, value: f64, ang: f64) -> f64 {
    // Within 1e-8 of it, Paris is 2.5969213 grads exactly, as PROJ replaces it.
    if name == "Paris" && (ang - 0.9).abs() < 1e-9 && (value - 2.33722917).abs() < 1e-8 {
        return 2.5969213 * 0.9;
    }
    if let Some(&(_, d, m, s)) = PRIME_MERIDIANS_DMS.iter().find(|(n, ..)| *n == name) {
        let sign = d.signum();
        let sexagesimal = sign * (d.abs() + m / 100.0 + s / 10000.0);
        let decimal = sign * (d.abs() + m / 60.0 + s / 3600.0);
        if (value - sexagesimal).abs() < 1e-8 || (value - decimal).abs() < 1e-8 {
            return decimal;
        }
    }
    value * ang
}

/// A WKT 1 CRS as a PROJ string, with the code of its geographic CRS when it declares one.
pub(crate) struct Converted {
    pub proj: String,
    /// The `GEOGCS` node, for identifying the datum.
    pub geogcs: Node,
    /// The EPSG geographic CRS, when the reader knows it.
    pub geog: Option<u32>,
    /// A WKT 1 `GEOGCS` to identify the datum by, when the WKT has none of its own (WKT 2).
    pub geogcs_wkt1: Option<String>,
    /// A WKT 2 `BOUNDCRS` to another CRS than WGS 84: that CRS and the shift to it.
    pub bound: Option<Box<BoundTo>>,
    /// A `BOUNDCRS`'s Helmert transformation that `+towgs84` cannot stand for: a full rotation
    /// matrix, or a Molodensky-Badekas evaluation point.
    pub helmert: Option<Helmert>,
}

/// A Helmert transformation as PROJ's `helmert` and `molobadekas` steps take it: translations in
/// metres, rotations in arc-seconds and the scale difference in parts per million, in the
/// method's own convention, the rotation matrix exact or approximate, about a pivot point.
#[derive(Clone, Debug)]
pub(crate) struct Helmert {
    pub t: [f64; 3],
    pub r: [f64; 3],
    pub s: f64,
    pub position_vector: bool,
    pub exact: bool,
    pub pivot: [f64; 3],
}

/// The target CRS of a WKT 2 `BOUNDCRS` to a CRS other than WGS 84, and its Helmert
/// transformation to it.
pub(crate) struct BoundTo {
    /// The target CRS.
    pub target: Converted,
    /// The 3 or 7 parameters of the Helmert transformation to it, as `+towgs84`'s.
    pub shift: Vec<f64>,
}

/// Converts a WKT 1 `GEOGCS` or `PROJCS` (ESRI or OGC flavour) into a PROJ string. `None` for
/// other kinds of CRS and projections terrio does not know.
pub(crate) fn to_proj(root: &Node) -> Option<Converted> {
    let key = root.key.to_ascii_uppercase();
    let geogcs = match key.as_str() {
        "GEOGCS" | "GEOCCS" => root,
        "PROJCS" => root.child("GEOGCS")?,
        _ => return None,
    };
    let datum = geogcs.child("DATUM")?;
    let sph = datum.child_in(crate::keywords::ELLIPSOID)?;
    let a = sph.num(1)?;
    let rf = sph.num(2)?;
    // Angular unit of the geographic CRS, in degrees.
    let ang = if key == "GEOCCS" { 1.0 } else { geogcs.child("UNIT").and_then(|u| u.num(1)).map(|r| r.to_degrees()).unwrap_or(1.0) };
    // WKT rounds the size of a degree (0.0174532925199433) and of a grad.
    let ang = [1.0, 0.9].into_iter().find(|u| (ang - u).abs() < 1e-9).unwrap_or(ang);
    let primem = geogcs.child("PRIMEM");
    let pm = primem.and_then(|p| p.num(1)).unwrap_or(0.0);
    let pm = prime_meridian_degrees(primem.and_then(Node::name).unwrap_or(""), pm, ang);
    let mut out = Vec::new();
    let mut tail = vec![format!("+a={a}"), format!("+rf={rf}")];
    if rf == 0.0 {
        tail[1] = format!("+b={a}");
    }
    if pm != 0.0 {
        tail.push(format!("+pm={pm}"));
    }
    if let Some(t) = datum.child("TOWGS84") {
        let v: Vec<String> = (0..t.args.len()).filter_map(|i| t.num(i)).map(|v| v.to_string()).collect();
        if !v.is_empty() {
            tail.push(format!("+towgs84={}", v.join(",")));
        }
    }
    if key == "GEOCCS" {
        let unit = root.child("UNIT").and_then(|u| u.num(1)).unwrap_or(1.0);
        out.push(format!("+proj=geocent +to_meter={unit}"));
        out.extend(tail);
        let geog = datum.epsg().and_then(|d| super::epsg::geographic_on(d).iter().copied().find(|c| !super::epsg::is_deprecated(*c)));
        return Some(Converted { proj: out.join(" "), geogcs: root.clone(), geog, geogcs_wkt1: None, bound: None, helmert: None });
    }
    if key == "GEOGCS" {
        out.push("+proj=longlat".to_string());
        if (ang - 1.0).abs() > 1e-12 {
            tail.push(format!("+to_deg={ang}"));
        }
        out.extend(tail);
        return Some(Converted { proj: out.join(" "), geogcs: geogcs.clone(), geog: None, geogcs_wkt1: None, bound: None, helmert: None });
    }

    // The first `UNIT`, as PROJ 9.8.1 and GDAL take it when a `PROJCS` has two.
    let to_meter = root.children("UNIT").next().and_then(|u| u.num(1)).unwrap_or(1.0);
    let params: Vec<(String, f64)> = root.children("PARAMETER").filter_map(|p| Some((norm(p.name()?), p.num(1)?))).collect();
    let get = |names: &[&str]| names.iter().find_map(|n| params.iter().find(|(k, _)| k == n).map(|(_, v)| *v));
    let angle = |names: &[&str]| get(names).map(|v| v * ang);
    // GDAL's WKT 1 names, else EPSG's (`Latitude of false origin`, `Longitude of projection
    // centre`), which PROJ 9.8.1 also reads in WKT 1.
    let lon_0 = angle(&[
        "centralmeridian",
        "longitudeofcenter",
        "longitudeoforigin",
        "longitudeofnaturalorigin",
        "longitudeofcentre",
        "longitudeoffalseorigin",
        "longitudeofprojectioncentre",
    ]);
    let lat_0 = angle(&[
        "latitudeoforigin",
        "latitudeofcenter",
        "latitudeofnaturalorigin",
        "latitudeofcentre",
        "latitudeoffalseorigin",
        "latitudeofprojectioncentre",
    ]);
    let k = get(&["scalefactor", "scalefactoratnaturalorigin", "scalefactoroninitialline"]);
    let sp1 = angle(&["standardparallel1", "standardparallel", "latitudeof1ststandardparallel"]);
    let sp2 = angle(&["standardparallel2", "latitudeof2ndstandardparallel"]);
    let projection = root.child("PROJECTION")?;
    let method = if projection.epsg() == Some(9830) { "polarstereographicvariantc".to_string() } else { norm(projection.name()?) };
    if method == "hotineobliquemercatortwopointnaturalorigin" {
        // A present but malformed value must not be dropped and replaced by a zero default.
        for parameter in root.children("PARAMETER") {
            let name = norm(parameter.name()?);
            if matches!(
                name.as_str(),
                "latitudeofpoint1"
                    | "latitudeof1stpoint"
                    | "longitudeofpoint1"
                    | "longitudeof1stpoint"
                    | "latitudeofpoint2"
                    | "latitudeof2ndpoint"
                    | "longitudeofpoint2"
                    | "longitudeof2ndpoint"
                    | "latitudeofcenter"
                    | "latitudeoforigin"
                    | "latitudeofnaturalorigin"
                    | "latitudeofcentre"
                    | "latitudeoffalseorigin"
                    | "latitudeofprojectioncentre"
                    | "scalefactor"
                    | "scalefactoratnaturalorigin"
                    | "scalefactoroninitialline"
                    | "falseeasting"
                    | "eastingatfalseorigin"
                    | "eastingatprojectioncentre"
                    | "falsenorthing"
                    | "northingatfalseorigin"
                    | "northingatprojectioncentre"
            ) && !parameter.num(1)?.is_finite()
            {
                return None;
            }
        }
    }

    // `x_0` and `y_0` are ESRI's names for them in `PROJ_healpix`, `PROJ_rhealpix` and `PROJ_isea`.
    let proj_method = matches!(method.as_str(), "projhealpix" | "projrhealpix") || method.starts_with("projisea");
    let easting = get(&["falseeasting", "eastingatfalseorigin", "eastingatprojectioncentre"])
        .or_else(|| if proj_method { get(&["x0"]) } else { None });
    let northing = get(&["falsenorthing", "northingatfalseorigin", "northingatprojectioncentre"])
        .or_else(|| if proj_method { get(&["y0"]) } else { None });
    let fe = easting.unwrap_or(0.0) * to_meter;
    let mut fn_ = northing.unwrap_or(0.0) * to_meter;
    let azimuth = angle(&["azimuth", "azimuthofinitialline"]);
    let gamma = angle(&["rectifiedgridangle", "angleofrectifiedtoskewedgrid", "anglefromrectifiedtoskewgrid"]);
    let push = |out: &mut Vec<String>, k: &str, v: Option<f64>| {
        if let Some(v) = v {
            out.push(format!("+{k}={v}"));
        }
    };
    match method.as_str() {
        "transversemercator" | "gausskruger" | "transversemercatorcomplex" => {
            out.push("+proj=tmerc".into());
            push(&mut out, "lat_0", lat_0.or(Some(0.0)));
            push(&mut out, "lon_0", lon_0);
            push(&mut out, "k", k.or(Some(1.0)));
        }
        "transversemercatorsouthorientated" => {
            // West and south, but for a north-east CS, which PROJ reads south then west.
            let north_east = projected_axes(root) == Some(*b"ne");
            out.push(if north_east { "+proj=tmerc +axis=swu" } else { "+proj=tmerc +axis=wsu" }.into());
            push(&mut out, "lat_0", lat_0.or(Some(0.0)));
            push(&mut out, "lon_0", lon_0);
            push(&mut out, "k", k.or(Some(1.0)));
        }
        "lambertconformalconic1sp" => {
            out.push("+proj=lcc".into());
            push(&mut out, "lat_1", lat_0);
            push(&mut out, "lat_0", lat_0);
            push(&mut out, "lon_0", lon_0);
            push(&mut out, "k_0", k.or(Some(1.0)));
        }
        "lambertconformalconic2sp" | "lambertconformalconic" | "lambertconformalconic2spbelgium" => {
            out.push("+proj=lcc".into());
            match (sp1, sp2) {
                (Some(s1), Some(s2)) => {
                    push(&mut out, "lat_1", Some(s1));
                    push(&mut out, "lat_2", Some(s2));
                }
                // ESRI's single-parallel form.
                (Some(s1), None) if method == "lambertconformalconic" => push(&mut out, "lat_1", Some(s1)),
                // A 2SP method without its second parallel: 0, as GDAL reads it.
                (Some(s1), None) => {
                    push(&mut out, "lat_1", Some(s1));
                    push(&mut out, "lat_2", Some(0.0));
                }
                _ => push(&mut out, "lat_1", lat_0),
            }
            push(&mut out, "lat_0", lat_0);
            push(&mut out, "lon_0", lon_0);
            push(&mut out, "k_0", k);
        }
        "albersconicequalarea" | "albers" | "albersequalarea" => {
            out.push("+proj=aea".into());
            push(&mut out, "lat_0", lat_0);
            push(&mut out, "lon_0", lon_0);
            push(&mut out, "lat_1", sp1);
            push(&mut out, "lat_2", sp2);
        }
        // ESRI's `Mercator_Variant_A` and EPSG's `Mercator (variant A)` are Mercator (1SP), as
        // PROJ 9.8.1 reads them. PROJ leaves a latitude of origin out of ESRI's, and refuses EPSG's
        // with one other than 0, which `+proj=merc` has no parameter for.
        "mercator1sp" | "mercatorvarianta" => {
            let origin = get(&["latitudeoforigin", "latitudeofnaturalorigin"]);
            let esri = projection.name().is_some_and(|n| n.eq_ignore_ascii_case("Mercator_Variant_A"));
            if method == "mercatorvarianta" && !esri && origin.is_some_and(|v| v != 0.0) {
                return None;
            }
            out.push("+proj=merc".into());
            push(&mut out, "lon_0", lon_0);
            push(&mut out, "k", k.or(Some(1.0)));
        }
        "mercator2sp" | "mercator" => {
            out.push("+proj=merc".into());
            push(&mut out, "lon_0", lon_0);
            if sp1.is_some() {
                push(&mut out, "lat_ts", sp1);
            } else {
                push(&mut out, "k", k.or(Some(1.0)));
            }
        }
        "mercatorauxiliarysphere" | "popularvisualisationpseudomercator" => {
            out.push("+proj=merc".into());
            push(&mut out, "lon_0", lon_0);
            // Spherical formulas on the semi-major axis.
            tail[1] = format!("+b={a}");
        }
        "polarstereographic" | "polarstereographicvarianta" | "polarstereographicvariantb" => {
            out.push("+proj=stere".into());
            // EPSG's variant B gives its standard parallel as `Latitude of standard parallel`.
            let parallel = if method == "polarstereographicvariantb" { angle(&["latitudeofstandardparallel"]) } else { None };
            // GDAL's name gives its latitude only as `latitude_of_origin`; PROJ 9.8.1 refuses it
            // without, rather than read it at the north pole.
            if polar_latitude_refused(root) {
                return None;
            }
            // Not a latitude of NaN, read at the north pole.
            let lat = lat_0.or(sp1).or(parallel).unwrap_or(90.0);
            if !lat.is_finite() {
                return None;
            }
            push(&mut out, "lat_0", Some(if lat < 0.0 { -90.0 } else { 90.0 }));
            if (lat.abs() - 90.0).abs() > 1e-9 {
                push(&mut out, "lat_ts", Some(lat));
            }
            push(&mut out, "lon_0", lon_0);
            push(&mut out, "k", k.or(Some(1.0)));
        }
        "stereographicnorthpole" | "stereographicsouthpole" => {
            out.push("+proj=stere".into());
            push(&mut out, "lat_0", Some(if method.ends_with("southpole") { -90.0 } else { 90.0 }));
            push(&mut out, "lat_ts", sp1);
            push(&mut out, "lon_0", lon_0);
            push(&mut out, "k", k.or(Some(1.0)));
        }
        // Variant B on the same standard parallel, with its false origin moved to the pole.
        "polarstereographicvariantc" => {
            let lat = angle(&["latitudeofstandardparallel"]).filter(|v| v.is_finite() && v.abs() <= 90.0)?;
            let lon = lon_0.filter(|v| v.is_finite())?;
            easting.filter(|v| v.is_finite())?;
            northing.filter(|v| v.is_finite())?;
            out.push("+proj=stere".into());
            push(&mut out, "lat_0", Some(if lat < 0.0 { -90.0 } else { 90.0 }));
            push(&mut out, "lat_ts", Some(lat));
            push(&mut out, "lon_0", Some(lon));
            fn_ = variant_c_pole_northing(a, rf, lat, fn_);
        }
        "obliquestereographic" | "doublestereographic" => {
            out.push("+proj=sterea".into());
            push(&mut out, "lat_0", lat_0);
            push(&mut out, "lon_0", lon_0);
            push(&mut out, "k", k.or(Some(1.0)));
        }
        "stereographic" => {
            out.push("+proj=stere".into());
            push(&mut out, "lat_0", lat_0);
            push(&mut out, "lon_0", lon_0);
            push(&mut out, "k", k.or(Some(1.0)));
        }
        "lambertazimuthalequalarea" => {
            out.push("+proj=laea".into());
            push(&mut out, "lat_0", lat_0);
            push(&mut out, "lon_0", lon_0);
        }
        // PROJ's two-point form, with GDAL's and ESRI's parameter names.
        "hotineobliquemercatortwopointnaturalorigin" => {
            out.push("+proj=omerc".into());
            push(&mut out, "lat_0", lat_0);
            push(&mut out, "lat_1", angle(&["latitudeofpoint1", "latitudeof1stpoint"]));
            push(&mut out, "lon_1", angle(&["longitudeofpoint1", "longitudeof1stpoint"]));
            push(&mut out, "lat_2", angle(&["latitudeofpoint2", "latitudeof2ndpoint"]));
            push(&mut out, "lon_2", angle(&["longitudeofpoint2", "longitudeof2ndpoint"]));
            push(&mut out, "k", k.or(Some(1.0)));
        }
        "hotineobliquemercator" | "hotineobliquemercatorazimuthnaturalorigin" | "hotineobliquemercatorvarianta" => {
            out.push("+proj=omerc +no_uoff".into());
            push(&mut out, "lat_0", lat_0);
            push(&mut out, "lonc", lon_0);
            push(&mut out, "alpha", azimuth);
            push(&mut out, "gamma", gamma.or(azimuth));
            push(&mut out, "k", k.or(Some(1.0)));
        }
        // With an azimuth (and a rectified grid angle, ESRI's being the azimuth) of 90°, the Swiss
        // oblique Mercator, as PROJ reads it (and writes it, from `+proj=somerc`).
        "hotineobliquemercatorazimuthcenter" | "hotineobliquemercatorvariantb"
            if azimuth.is_some_and(|a| (a - 90.0).abs() < 1e-9) && gamma.is_none_or(|g| (g - 90.0).abs() < 1e-9) =>
        {
            out.push("+proj=somerc".into());
            push(&mut out, "lat_0", lat_0);
            push(&mut out, "lon_0", lon_0);
            push(&mut out, "k_0", k.or(Some(1.0)));
        }
        "hotineobliquemercatorazimuthcenter" | "obliquemercator" | "hotineobliquemercatorvariantb" => {
            out.push("+proj=omerc".into());
            push(&mut out, "lat_0", lat_0);
            push(&mut out, "lonc", lon_0);
            push(&mut out, "alpha", azimuth);
            push(&mut out, "gamma", gamma.or(azimuth));
            push(&mut out, "k", k.or(Some(1.0)));
        }
        "swissobliquecylindrical" | "swissobliquemercator" | "hotineobliquemercatorazimuthcenterswiss" => {
            out.push("+proj=somerc".into());
            push(&mut out, "lat_0", lat_0);
            push(&mut out, "lon_0", lon_0);
            push(&mut out, "k_0", k.or(Some(1.0)));
        }
        "krovak" => {
            // Its axes: ESRI's scale and rotation of the plane (-1 and 90° for east-north, else
            // south-west; the rotation in the CRS's angular unit, 100 grads for 90°), else the
            // WKT's, south-west without them, as GDAL 3.10 and PROJ 9.8.1 read them (northing then
            // easting read as easting then northing).
            let axes = match (get(&["xscale"]), angle(&["xyplanerotation"])) {
                (Some(sx), Some(rot)) => {
                    if sx < 0.0 && (rot - 90.0).abs() < 1e-10 {
                        *b"en"
                    } else {
                        *b"sw"
                    }
                }
                _ => projected_axes(root).unwrap_or(*b"sw"),
            };
            out.push(match axes {
                [b'e', b'n'] | [b'n', b'e'] => "+proj=krovak".into(),
                [a, b] => format!("+proj=krovak +axis={}{}u", a as char, b as char),
            });
            push(&mut out, "lat_0", lat_0);
            push(&mut out, "lon_0", lon_0);
            push(&mut out, "alpha", azimuth);
            push(&mut out, "lat_ts", angle(&["pseudostandardparallel1"]));
            push(&mut out, "k", k.or(Some(0.9999)));
        }
        "labordeobliquemercator" => {
            out.push("+proj=labrd".into());
            push(&mut out, "lat_0", lat_0);
            push(&mut out, "lon_0", lon_0);
            push(&mut out, "azi", azimuth);
            push(&mut out, "k_0", k.or(Some(1.0)));
        }
        "equidistantconic" => {
            out.push("+proj=eqdc".into());
            push(&mut out, "lat_0", lat_0);
            push(&mut out, "lon_0", lon_0);
            push(&mut out, "lat_1", sp1);
            push(&mut out, "lat_2", sp2);
        }
        "gaussschreibertransversemercator" => {
            out.push("+proj=gstmerc".into());
            push(&mut out, "lat_0", lat_0);
            push(&mut out, "lon_0", lon_0);
            push(&mut out, "k_0", k.or(Some(1.0)));
        }
        // GDAL's latitude of origin, ESRI's central parallel.
        "loximuthal" => {
            out.push("+proj=loxim".into());
            push(&mut out, "lat_1", lat_0.or_else(|| angle(&["centralparallel"])));
            push(&mut out, "lon_0", lon_0);
        }
        "winkeltripel" | "winkeli" | "winkelii" => {
            let (name, parallel) = match method.as_str() {
                "winkeltripel" => ("wintri", "lat_1"),
                "winkeli" => ("wink1", "lat_ts"),
                _ => ("wink2", "lat_1"),
            };
            out.push(format!("+proj={name}"));
            push(&mut out, parallel, sp1.or(Some(0.0)));
            push(&mut out, "lon_0", lon_0);
        }
        "cassinisoldner" | "cassini" => {
            out.push("+proj=cass".into());
            push(&mut out, "lat_0", lat_0);
            push(&mut out, "lon_0", lon_0);
        }
        "polyconic" | "americanpolyconic" => {
            out.push("+proj=poly".into());
            push(&mut out, "lat_0", lat_0);
            push(&mut out, "lon_0", lon_0);
        }
        "equirectangular" | "equidistantcylindrical" | "platecarree" => {
            out.push("+proj=eqc".into());
            push(&mut out, "lat_ts", sp1);
            push(&mut out, "lat_0", lat_0);
            push(&mut out, "lon_0", lon_0);
        }
        "cylindricalequalarea" | "lambertcylindricalequalarea" => {
            out.push("+proj=cea".into());
            push(&mut out, "lat_ts", sp1);
            push(&mut out, "lon_0", lon_0);
        }
        // ESRI's Behrmann: cylindrical equal area true at 30°, or at the standard parallel it
        // gives (EASE-Grid 2.0, EPSG:6933, as ESRI writes it), as PROJ reads it.
        "behrmann" => {
            out.push("+proj=cea".into());
            push(&mut out, "lat_ts", sp1.or(Some(30.0)));
            push(&mut out, "lon_0", lon_0);
        }
        "azimuthalequidistant" | "modifiedazimuthalequidistant" => {
            out.push("+proj=aeqd".into());
            push(&mut out, "lat_0", lat_0);
            push(&mut out, "lon_0", lon_0);
        }
        "newzealandmapgrid" => out.push("+proj=nzmg".into()),
        "equalearth" => {
            out.push("+proj=eqearth".into());
            push(&mut out, "lon_0", lon_0);
        }
        "robinson" => {
            out.push("+proj=robin".into());
            push(&mut out, "lon_0", lon_0);
        }
        // GDAL's `satellite_height` and ESRI's `Height` (ESRI's `Option` changes nothing, and
        // its WKT cannot give a sweep axis: sweep y). Without a height, PROJ refuses the CRS.
        "geostationarysatellite" => {
            out.push("+proj=geos".into());
            push(&mut out, "lon_0", lon_0);
            push(&mut out, "h", get(&["satelliteheight", "height"]).map(|h| h * to_meter));
        }
        // GDAL's and ESRI's quadrilateralized spherical cube.
        "quadrilateralizedsphericalcube" => {
            out.push("+proj=qsc".into());
            push(&mut out, "lat_0", lat_0);
            push(&mut out, "lon_0", lon_0);
        }
        "orthographic" => {
            out.push("+proj=ortho".into());
            push(&mut out, "lat_0", lat_0);
            push(&mut out, "lon_0", lon_0);
        }
        // Projections terrio computes on an ellipsoid as PROJ does (#126), not through wbprojection.
        "sinusoidal" | "millercylindrical" | "vandergrinten" | "vandergrinteni" => {
            let name = match method.as_str() {
                "sinusoidal" => "sinu",
                "millercylindrical" => "mill",
                _ => "vandg",
            };
            out.push(format!("+proj={name}"));
            push(&mut out, "lon_0", lon_0);
        }
        "wagneriv" | "wagnerv" => {
            let name = if method == "wagneriv" { "wag4" } else { "wag5" };
            out.push(format!("+proj={name}"));
            push(&mut out, "lon_0", lon_0);
        }
        "gnomonic" => {
            out.push("+proj=gnom".into());
            push(&mut out, "lat_0", lat_0);
            push(&mut out, "lon_0", lon_0);
        }
        "bonne" => {
            out.push("+proj=bonne".into());
            push(&mut out, "lat_1", sp1.or(lat_0));
            push(&mut out, "lon_0", lon_0);
        }
        // World projections terrio ports from PROJ, as GDAL names them (wbprojection reads them as
        // others).
        "gallstereographic" | "flatpolarquartic" | "werenskioldi" => {
            let name = match method.as_str() {
                "gallstereographic" => "gall",
                "flatpolarquartic" => "mbtfpq",
                _ => "weren",
            };
            out.push(format!("+proj={name}"));
            push(&mut out, "lon_0", lon_0);
        }
        // ESRI's Goode homolosine: interrupted with `Option` 1 (2 is PROJ's `igh_o`, and without
        // it PROJ's uninterrupted `goode`, which terrio does not compute).
        "goodehomolosine" if get(&["option"]) == Some(1.0) => {
            out.push("+proj=igh".into());
            push(&mut out, "lon_0", lon_0);
        }
        "goodehomolosine" => return None,
        // HEALPix and rHEALPix in ESRI's form of PROJ's own methods, with their PROJ parameters.
        "projhealpix" => {
            out.push("+proj=healpix".into());
            push(&mut out, "lon_0", angle(&["lon0"]));
            push(&mut out, "rot_xy", angle(&["rotxy"]));
        }
        "projrhealpix" => {
            out.push("+proj=rhealpix".into());
            push(&mut out, "lon_0", angle(&["lon0"]));
            push(&mut out, "north_square", get(&["northsquare"]));
            push(&mut out, "south_square", get(&["southsquare"]));
        }
        // ISEA, with its string-valued parameters in the method's name: `PROJ_isea_orient=pole`.
        m if m.starts_with("projisea") => {
            let raw = root.child("PROJECTION")?.name()?;
            let flags = raw.get(..9)?.eq_ignore_ascii_case("PROJ_isea").then(|| &raw[9..])?;
            out.push("+proj=isea".into());
            let mut keys: Vec<&str> = Vec::new();
            for flag in flags.split('_').skip(1) {
                // ISEA's flag (`PROJ_isea_dual`), once.
                if flag == "dual" && !keys.contains(&flag) {
                    keys.push(flag);
                    out.push("+dual".into());
                    continue;
                }
                let (k, v) = flag.split_once('=')?;
                if k.is_empty() || v.is_empty() || !k.bytes().chain(v.bytes()).all(|c| c.is_ascii_alphanumeric()) {
                    return None;
                }
                // ISEA's string-valued parameters, each once; the others are PARAMETERs.
                if !matches!(k, "orient" | "mode" | "net") || keys.contains(&k) {
                    return None;
                }
                keys.push(k);
                out.push(format!("+{k}={v}"));
            }
            if !flags.is_empty() && !flags.starts_with('_') {
                return None;
            }
            push(&mut out, "lon_0", angle(&["lon0"]));
            push(&mut out, "lat_0", angle(&["lat0"]));
            push(&mut out, "azi", angle(&["azi"]));
            push(&mut out, "orient_lat", angle(&["orientlat"]));
            push(&mut out, "orient_lon", angle(&["orientlon"]));
            push(&mut out, "resolution", get(&["resolution"]));
            push(&mut out, "aperture", get(&["aperture"]));
        }
        // World projections of a central meridian, as terrio writes them.
        m => {
            let proj = crate::methods::WORLD.iter().find(|w| w.wkt1.is_some_and(|n| norm(n) == m))?.proj;
            out.push(format!("+proj={proj}"));
            push(&mut out, "lon_0", lon_0);
        }
    }
    // The CRS's axes, as GDAL 3.10 and PROJ 9.8.1 (`always_xy`) read them: the directions of its
    // `AXIS` nodes, in their order but for northing then easting, which is read as easting then
    // northing. South-orientated Transverse Mercator and Krovak have their own axes.
    match (method.as_str(), projected_axes(root)) {
        ("transversemercatorsouthorientated" | "krovak", _) => {}
        (_, Some([a, b])) if [a, b] != *b"ne" && [a, b] != *b"en" => out.push(format!("+axis={}{}u", a as char, b as char)),
        _ => {}
    }
    // South-orientated Transverse Mercator's false easting and northing are subtracted (EPSG
    // method 9808: W = FE - x, S = FN - y), where `+axis=wsu` negates `+x_0`/`+y_0` with x and y.
    let (fe, fn_) = if method == "transversemercatorsouthorientated" { (negated(fe), negated(fn_)) } else { (fe, fn_) };
    out.push(format!("+x_0={fe}"));
    out.push(format!("+y_0={fn_}"));
    out.push(format!("+to_meter={to_meter}"));
    out.extend(tail);
    Some(Converted { proj: out.join(" "), geogcs: geogcs.clone(), geog: None, geogcs_wkt1: None, bound: None, helmert: None })
}

/// The directions of a projected CRS's two `AXIS` nodes as PROJ's `+axis` letters (`e`, `w`,
/// `n`, `s`), when it has two along east-west and north-south.
fn projected_axes(root: &Node) -> Option<[u8; 2]> {
    let letters: Vec<u8> = root
        .children("AXIS")
        .map(|a| match a.args.get(1) {
            Some(Arg::Ident(d)) => match d.to_ascii_lowercase().as_str() {
                "east" => Some(b'e'),
                "west" => Some(b'w'),
                "north" => Some(b'n'),
                "south" => Some(b's'),
                _ => None,
            },
            _ => None,
        })
        .collect::<Option<_>>()?;
    let [a, b] = letters[..] else { return None };
    let east_west = |c: u8| matches!(c, b'e' | b'w');
    (east_west(a) != east_west(b)).then_some([a, b])
}

/// The northing of the pole in Polar Stereographic (variant C), EPSG method 9830, whose false
/// origin lies on the standard parallel φF rather than at the pole: NF − ρF for the south pole case
/// (φF < 0) and NF + ρF for the north, from the northing NF at the false origin, with ρF = a mF and
/// mF = cos φF / (1 − e² sin² φF)^0.5 (IOGP Guidance Note 7-2), on an ellipsoid of semi-major
/// axis `a` and inverse flattening `rf` (0 for a sphere). A point lies at ρ = ρF t / tF from the
/// pole, as in variant B with the same standard parallel, so the method is variant B with this
/// false northing.
pub(crate) fn variant_c_pole_northing(a: f64, rf: f64, lat_ts: f64, northing: f64) -> f64 {
    let es = if rf == 0.0 { 0.0 } else { (2.0 - 1.0 / rf) / rf };
    let phi = lat_ts.to_radians();
    let rho_f = a * phi.cos() / (1.0 - es * phi.sin().powi(2)).sqrt();
    if lat_ts < 0.0 { northing - rho_f } else { northing + rho_f }
}

/// `-v`, without a negative zero.
pub(crate) fn negated(v: f64) -> f64 {
    if v == 0.0 { 0.0 } else { -v }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SpatialRef, Transformer};
    use std::time::Duration;

    /// PROJ's two tables of named meridians agree on every name they share but Madrid, which
    /// `primeMeridiansDMS` has at 3°41'14.55"W and `proj_list_prime_meridians` at 16.58". Paris
    /// RGS is only in the first; Paris and Copenhagen only in the second.
    #[test]
    fn the_two_tables_of_named_prime_meridians_agree_but_where_proj_differs() {
        let mut shared = Vec::new();
        for (name, d, m, s) in PRIME_MERIDIANS_DMS {
            let decimal = d.signum() * (d.abs() + m / 60.0 + s / 3600.0);
            match crate::math::prime_meridian(&name.to_ascii_lowercase()) {
                Some(v) if name == "Madrid" => assert!((v - decimal + (16.58 - 14.55) / 3600.0).abs() < 1e-12, "{name}: {v} {decimal}"),
                Some(v) => assert!((v - decimal).abs() < 1e-12, "{name}: {v} {decimal}"),
                None => assert!(matches!(name, "Paris RGS" | "Paris_RGS"), "{name}"),
            }
            shared.push(name.to_ascii_lowercase());
        }
        let only_pm: Vec<&str> = crate::math::PRIME_MERIDIANS.iter().map(|(n, _)| *n).filter(|n| !shared.iter().any(|s| s == n)).collect();
        assert_eq!(only_pm, ["paris", "copenhagen"]);
    }

    /// GDAL's WKT of WGS 84 as a GeoJSON `crs` member or a PAM `<SRS>` carries it, and the same
    /// with the spheroid's `]` replaced by `)` (reproducers from a fuzzing campaign).
    const WGS84: &str = r#"GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#;
    const WGS84_PAREN: &str = r#"GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563)],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#;

    /// Runs `f` on a thread with a 256 MiB stack, failing when it does not return within `secs`:
    /// a parser that stops consuming input fails the test instead of hanging it, and a recursion
    /// the parser does not bound returns (deep) instead of overflowing a small stack.
    fn within<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(move || {
                let _ = tx.send(f());
            })
            .unwrap();
        rx.recv_timeout(Duration::from_secs(secs)).unwrap_or_else(|_| panic!("did not return within {secs} s"))
    }

    #[test]
    fn either_closer_ends_a_node_as_in_proj() {
        // PROJ's `WKTNode::createFrom` (GDAL 3.12.1's reader) takes `]` or `)` as the end of any
        // node: rasterio 1.5.0's `CRS.from_wkt(WGS84_PAREN)` is EPSG:4326.
        let good = parse(WGS84).unwrap();
        assert_eq!(within(5, || parse(WGS84_PAREN)), Some(good.clone()));
        let all_parens = WGS84.replace('[', "(").replace(']', ")");
        assert_eq!(within(5, move || parse(&all_parens)), Some(good.clone()));
        let crossed = WGS84.replacen("GEOGCS[", "GEOGCS(", 1);
        assert_eq!(within(5, move || parse(&crossed)), Some(good));
        assert_eq!(within(5, || parse("X(]")), Some(Node { key: "X".into(), args: vec![] }));
        let s = within(5, || SpatialRef::from_wkt(WGS84_PAREN));
        assert_eq!((s.epsg, s.esri), (Some(4326), None));
    }

    #[test]
    fn malformed_wkt_is_refused_promptly() {
        let prj = r#"GEOGCS["GCQ_WGS_1984",DATUM["D_WGS_1985"(SPHEROID["WGS_1984",6378137*0,298.257223563]],PRIMEM["Greenwhch",0.0],UNIT[&Degree",0.0174532925199433]]"#;
        // Vector case 1424's `.prj` (GDAL: "missing , or ]"), unterminated nodes and strings.
        for text in [prj, "A[)", "A[(", "A[\"x)", "A[1,)", "GEOGCS[\"x\",DATUM[)", "A[[[[", "A((((", "A[,,,"] {
            let t = text.to_string();
            let unterminated = !matches!(text, "A[)" | "A[1,)");
            assert_eq!(within(5, move || parse(&t).is_some()), !unterminated, "{text}");
        }
    }

    #[test]
    fn nesting_is_bounded_as_in_proj() {
        let nested = |levels: usize| format!("{}1{}", "A[".repeat(levels), "]".repeat(levels));
        // PROJ refuses a value at level 16: the 1 of 15 nested nodes is at level 15.
        assert!(parse(&nested(15)).is_some());
        assert!(parse(&nested(16)).is_none());
        assert!(parse(&format!("{}{}", "A[".repeat(15), "]".repeat(15))).is_some());
        assert!(parse(&format!("{}{}", "A[".repeat(17), "]".repeat(17))).is_none());
        // The deepest nesting that fits in GDAL's length limit.
        let deep = nested(MAX_LEN / 3);
        assert_eq!(within(30, move || parse(&deep)), None);
        let deep = format!("GEOGCS[{}", "[".repeat(MAX_LEN - 8));
        assert_eq!(within(30, move || parse(&deep)), None);
    }

    /// The parser refuses a NUL outside quotes, which text reaching it uncut (the WKT terrio
    /// builds from PROJJSON) could hold, as PROJ 9.8.1's text would end there; one in a quoted
    /// name stays part of the name.
    #[test]
    fn a_nul_outside_quotes_is_refused_by_the_parser() {
        let with = |part: &str| {
            format!(
                r#"GEOGCS["g",DATUM["d",SPHEROID["s",6378137,298.257223563]{part}],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#
            )
        };
        assert!(parse(&with("")).is_some());
        assert_eq!(parse(&with(",a\0b[\"x\"]")), None);
        assert_eq!(parse(&with(",x[\"a\",b\0c]")), None);
        let quoted = parse(&with(",x[\"a\0b\"]")).unwrap();
        let x = quoted.child("DATUM").and_then(|d| d.child("x")).unwrap();
        assert_eq!(x.name(), Some("a\0b"));
    }

    #[test]
    fn wkt_longer_than_gdal_imports_is_refused() {
        let padded = |len: usize| format!("{WGS84}{}", " ".repeat(len - WGS84.len()));
        assert_eq!(parse(&padded(MAX_LEN)), parse(WGS84));
        assert_eq!(parse(&padded(MAX_LEN + 1)), None);
    }

    #[test]
    fn refused_wkt_is_not_handed_to_wbprojection() {
        // WKT the reader refuses errors in terrio, rather than in wbprojection's parser, whose
        // recursion is unbounded (a stack overflow on the 8 MiB main thread for such text).
        let deep = format!("GEOGCS[\"x\",{}1{}]", "A[".repeat(30_000), "]".repeat(30_000));
        let (def, wbp) = within(30, move || {
            let s = SpatialRef { wkt: Some(deep), ..Default::default() };
            // Without the `wbprojection-fallback` feature, nothing reaches wbprojection.
            #[cfg(feature = "wbprojection-fallback")]
            let wbp = s.to_wbp().err().map(|e| e.to_string());
            #[cfg(not(feature = "wbprojection-fallback"))]
            let wbp: Option<String> = None;
            (crate::Def::new(&s).err().map(|e| e.to_string()), wbp)
        });
        assert!(def.is_some_and(|e| e.contains("not a WKT definition GDAL reads")));
        #[cfg(feature = "wbprojection-fallback")]
        assert!(wbp.is_some_and(|e| e.contains("not a WKT definition GDAL reads")));
        #[cfg(not(feature = "wbprojection-fallback"))]
        assert_eq!(wbp, None);
    }

    #[test]
    fn user_wkt_gdal_refuses_is_an_error() {
        // `SpatialRef::parse` (every string CRS input): as GDAL's `importFromWkt`, not an opaque
        // CRS that transforms to infinities.
        let deep = format!("GEOGCS[\"x\",{}1{}]", "A[".repeat(30_000), "]".repeat(30_000));
        for bad in [deep.as_str(), &WGS84[..WGS84.len() - 1]] {
            let b = bad.to_string();
            assert!(within(30, move || SpatialRef::parse(&b)).is_err());
        }
        assert_eq!(within(5, || SpatialRef::parse(WGS84_PAREN).unwrap().epsg), Some(4326));
    }

    #[test]
    fn user_wkt_keyword_without_definition_is_an_error() {
        // #1032: a CRS keyword with only its name was a CRS that no transformation could use.
        // GDAL 3.10.3 (PROJ 9.6.0) refuses them too: `gdalsrsinfo 'GEOGCS["x"]'` reports "Missing
        // DATUM or ENSEMBLE node", `PROJCS["x"]` "missing CONVERSION node".
        let keywords = "GEOGCS PROJCS GEOCCS COMPD_CS VERT_CS LOCAL_CS GEOGCRS GEOGRAPHICCRS GEODCRS GEODETICCRS \
                        PROJCRS PROJECTEDCRS COMPOUNDCRS BOUNDCRS VERTCRS VERTICALCRS ENGCRS ENGINEERINGCRS";
        for keyword in keywords.split_whitespace() {
            for text in [format!("{keyword}[x]"), format!("{keyword}[\"WGS 84\"]")] {
                let err = SpatialRef::parse_literal(&text).unwrap_err().to_string();
                assert!(err.contains("has no definition after its name"), "{text}: {err}");
            }
        }
        // A member node is enough: such minimal definitions stay readable.
        assert!(SpatialRef::parse_literal(r#"LOCAL_CS["site grid",UNIT["metre",1]]"#).is_ok());
    }

    #[test]
    fn crs_files_are_read_as_gdal_reads_them() {
        let dir = std::env::temp_dir().join(format!("terrio-crs-files-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A file naming itself: its text is a definition, not another file (it recursed forever).
        let own = dir.join("own.txt");
        std::fs::write(&own, own.to_str().unwrap()).unwrap();
        let path = own.to_str().unwrap().to_string();
        assert!(within(10, move || SpatialRef::parse(&path)).is_err());
        // GDAL reads at most 100,000 bytes and refuses a longer file ("File too large to be a WKT,
        // PROJ string or XML"), whatever it holds; `/dev/zero` was read until memory ran out.
        let long = dir.join("long.prj");
        std::fs::write(&long, format!("+proj=longlat +datum=WGS84 +no_defs{}", " ".repeat(100_000))).unwrap();
        let short = dir.join("short.prj");
        std::fs::write(&short, "+proj=longlat +datum=WGS84 +no_defs").unwrap();
        assert!(SpatialRef::parse(long.to_str().unwrap()).is_err());
        assert!(SpatialRef::parse(short.to_str().unwrap()).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_geographic_crs_in_parentheses_keeps_its_datum_shift() {
        // ESRI's NAD27 / UTM zone 13N, without AUTHORITY (its datum is identified from its GEOGCS
        // text), in brackets and in parentheses (both WKT 1 delimiters), transforms alike as
        // GDAL reads both alike.
        let esri = r#"PROJCS["NAD_1927_UTM_Zone_13N",GEOGCS["GCS_North_American_1927",DATUM["D_North_American_1927",SPHEROID["Clarke_1866",6378206.4,294.9786982]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["False_Easting",500000.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",-105.0],PARAMETER["Scale_Factor",0.9996],PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]"#;
        let wgs84 = SpatialRef::from_epsg(4326).unwrap();
        // Unidentified, as a CRS read from a file without its code.
        let run = |w: &str| {
            let s = SpatialRef { wkt: Some(w.to_string()), ..Default::default() };
            Transformer::new(&s, &wgs84).unwrap().transform(500_000.0, 4_400_000.0).unwrap()
        };
        let good = run(esri);
        assert_eq!(run(&esri.replace('[', "(").replace(']', ")")), good);
        assert_eq!(run(&esri.replace("294.9786982]]", "294.9786982)]")), good);
        // NAD27 is shifted (by tens of metres here), not taken as WGS 84.
        let unshifted = run(&esri.replace("D_North_American_1927", "D_Unknown"));
        assert!((good.1 - unshifted.1).abs() > 1e-5, "{good:?} {unshifted:?}");
    }

    #[test]
    fn geocentric_geographic_parent_uses_datum_registry() {
        // GDA2020 datum 1168 is on geographic CRS 7844, without a numeric offset.
        let wkt = r#"GEOCCS["GDA2020",DATUM["Geocentric_Datum_of_Australia_2020",SPHEROID["GRS 1980",6378137,298.257222101],AUTHORITY["EPSG","1168"]],PRIMEM["Greenwich",0],UNIT["metre",1]]"#;
        let c = to_proj(&parse(wkt).unwrap()).unwrap();
        assert_eq!(c.geog, Some(7844));
        let unknown = wkt.replace("1168", "999999");
        assert_eq!(to_proj(&parse(&unknown).unwrap()).unwrap().geog, None);
    }

    #[test]
    fn converts_esri_feet() {
        let esri = r#"PROJCS["NAD_1983_StatePlane_New_York_Long_Island_FIPS_3104_Feet",GEOGCS["GCS_North_American_1983",DATUM["D_North_American_1983",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Lambert_Conformal_Conic"],PARAMETER["False_Easting",984250.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",-74.0],PARAMETER["Standard_Parallel_1",40.66666666666666],PARAMETER["Standard_Parallel_2",41.03333333333333],PARAMETER["Latitude_Of_Origin",40.16666666666666],UNIT["Foot_US",0.3048006096012192]]"#;
        let c = to_proj(&parse(esri).unwrap()).unwrap();
        assert!(c.proj.starts_with("+proj=lcc +lat_1=40.66666666666666 +lat_2=41.03333333333333"), "{}", c.proj);
        assert!(c.proj.contains("+to_meter=0.3048006096012192"), "{}", c.proj);
        assert_eq!(c.geogcs.name(), Some("GCS_North_American_1983"));
    }
}
