// Written to match the behavior of:
//   GDAL v3.13.3:
//     ogr/ogrct.cpp
//   PROJ 9.8.1:
//     data/sql (proj.db)
// No code is translated from these sources.

//! The EPSG table terrio carries: official names, WKT, coordinate conversions as PROJ strings,
//! the datum transformations GDAL uses (without and with grids), vertical and compound CRSs and
//! geoid models, generated from GDAL and PROJ by `scripts/gen_epsg_table.py`. The ESRI table
//! (`scripts/gen_esri_table.py`) holds the geographic and projected CRSs of PROJ's ESRI
//! authority in the same form.
//!
//! Derived from the EPSG Geodetic Parameter Dataset (<https://epsg.org>), whose terms of use
//! permit redistribution with attribution, and from PROJ's database; the ESRI table and ESRI datum
//! names from Esri's projection engine data (Apache-2.0). See `NOTICE`.

use super::datum::{Op, parse_steps};
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// The EPSG table, which WebAssembly modules hold compressed: native builds embed it decompressed
/// ([`epsg_text`]).
#[cfg(any(target_family = "wasm", test))]
static DATA: &[u8] = include_bytes!("../data/epsg.tsv.zst");
static ESRI_DATA: &[u8] = include_bytes!("../data/esri.tsv.zst");
/// Transformations between EPSG vertical CRSs (`scripts/gen_vertical_table.py`).
static VERTICAL_DATA: &[u8] = include_bytes!("../data/epsg_vertical.tsv.zst");

/// The names and aliases of EPSG datums, generated from PROJ by `scripts/gen_datum_names.py`:
/// `code<TAB>rank<TAB>name` lines.
static DATUM_NAMES: &[u8] = include_bytes!("../data/datums.tsv.zst");
/// The CRSs PROJ finds by the names of CRSs without ID, generated from PROJ by
/// `scripts/gen_crs_names.py`.
static CRS_NAMES: &[u8] = include_bytes!("../data/crs_names.tsv.zst");
/// The names PROJ identifies vertical CRSs without ID by, generated from PROJ by
/// `scripts/gen_vertical_names.py`: `kind<TAB>name<TAB>code` lines.
static VERTICAL_NAMES: &[u8] = include_bytes!("../data/vertical_names.tsv.zst");
/// What GDAL adds to the PROJ string of an EPSG or ESRI CRS when it exports it, generated from
/// GDAL 3.13.3 by `scripts/gen_gdal_towgs84.py`: `authority<TAB>code<TAB>towgs84<TAB>geoid` lines.
static GDAL_TOWGS84: &[u8] = include_bytes!("../data/gdal_towgs84.tsv.zst");

/// The `+towgs84` values (`598.1,73.7,418.2,0.202,0.045,-2.455,6.7`) and the `+geoidgrids=...
/// +geoid_crs=...` tokens GDAL 3.13.3 adds to the PROJ string of an EPSG or ESRI CRS
/// (`OSRExportToProj4`), each empty when it adds none; `None` when it adds neither.
pub(crate) fn gdal_additions(authority: &str, code: u32) -> Option<(&'static str, &'static str)> {
    static TABLE: OnceLock<HashMap<(bool, u32), (String, String)>> = OnceLock::new();
    let esri = match authority {
        "EPSG" => false,
        "ESRI" => true,
        _ => return None,
    };
    let table = TABLE.get_or_init(|| {
        decompress(GDAL_TOWGS84)
            .lines()
            .filter_map(|l| {
                let mut f = l.split('\t');
                let (authority, code) = (f.next()?, f.next()?.parse().ok()?);
                Some(((authority == "ESRI", code), (f.next()?.to_string(), f.next()?.to_string())))
            })
            .collect()
    });
    table.get(&(esri, code)).map(|(t, g)| (t.as_str(), g.as_str()))
}

/// An EPSG (or ESRI) CRS of the table.
#[derive(Debug)]
pub(crate) struct Entry {
    /// The official name.
    pub name: String,
    /// The EPSG code of its datum, 0 when unknown.
    pub datum: u32,
    /// The EPSG code of its geographic CRS, 0 when unknown.
    pub geog: u32,
    /// Its datum transformations to WGS 84, in the order GDAL tries them when no grid is
    /// installed.
    pub ops: Vec<Arc<Op>>,
    /// Those GDAL tries when grids are available (empty when the same).
    pub grid_ops: Vec<Arc<Op>>,
    /// Whether it is a dynamic CRS (its datum is a dynamic reference frame).
    pub dynamic: bool,
    /// Whether it is a geographic 3D CRS.
    pub three_d: bool,
    /// Its area of use: west, south, east and north in degrees.
    pub area: Option<[f64; 4]>,
    /// The PROJ string of its coordinate conversion (without datum shift).
    pub proj: String,
    /// The WKT GDAL writes for it.
    pub wkt: String,
}

/// An EPSG vertical CRS.
#[derive(Debug)]
pub(crate) struct Vertical {
    pub name: String,
    /// The EPSG vertical datum code.
    pub datum: u32,
    /// Metres per unit.
    pub to_meter: f64,
    /// Whether heights are depths (positive down).
    pub down: bool,
    pub wkt: String,
}

/// An EPSG compound CRS: a horizontal CRS and a vertical CRS.
#[derive(Debug)]
pub(crate) struct Compound {
    pub name: String,
    pub horizontal: u32,
    pub vertical: u32,
    pub wkt: String,
    /// Its area of use (west, south, east, north in degrees), when known.
    pub area: Option<[f64; 4]>,
}

/// Transformations without and with grids.
type OpLists = (Vec<Arc<Op>>, Vec<Arc<Op>>);

/// A transformation between vertical CRSs, with the geographic CRS it interpolates in.
#[derive(Debug, Clone)]
pub(crate) struct VerticalOp {
    pub op: Arc<Op>,
    /// The EPSG geographic CRS its grids or offsets are referenced to: PROJ chains the
    /// horizontal transformation to it when the compound CRSs are on another datum.
    pub interpolation: Option<u32>,
}

/// Transformations between vertical CRSs without and with grids.
type VerticalLists = (Vec<VerticalOp>, Vec<VerticalOp>);

/// The transformations GDAL lists from one geographic CRS to another, without and with grids.
struct PairLists {
    lists: OpLists,
}

/// The table: its text and an index of its lines, built once. Entries are parsed on first use.
struct Table {
    text: Cow<'static, str>,
    /// Each entry boxed: indexing the table writes a slot per CRS line, not an entry's size.
    crs: HashMap<u32, Lazy<Option<Box<Entry>>>>,
    /// The codes and lines of the CRSs, in the table's order: indexes built from every CRS line
    /// read the text in order rather than in the map's.
    crs_lines: Vec<(u32, (u32, u32))>,
    ops: HashMap<u32, Lazy<Option<Arc<Op>>>>,
    /// Direct transformations between geographic CRSs, by (source, target) code.
    pairs: HashMap<(u32, u32), Lazy<PairLists>>,
    verticals: HashMap<u32, Lazy<Option<Vertical>>>,
    compounds: HashMap<u32, Lazy<Option<Compound>>>,
    /// Geoid models from geographic 3D CRSs to vertical CRSs, by vertical CRS.
    geoids: HashMap<u32, Vec<(u32, Lazy<OpLists>)>>,
    /// Transformations between vertical CRSs, by (source, target) code.
    vertical_ops: HashMap<(u32, u32), Lazy<VerticalLists>>,
    /// Old names of grids.
    alternatives: HashMap<String, Vec<String>>,
    /// The publication dates of datums (`2007-01-01`), by datum code.
    dates: HashMap<u32, String>,
    /// The lines of the geodetic CRSs (`E`) and registered operations (`R`), after their kind.
    geodetic: Vec<(usize, usize)>,
    registered: Vec<(usize, usize)>,
    /// The deprecated CRSs, with the codes of those that replace them.
    replacements: HashMap<u32, Vec<u32>>,
    /// The transformations PROJ lists for a geographic 3D or geocentric CRS in another direction
    /// than into WGS 84 (3D), where they differ, by code and direction.
    directed: HashMap<(u32, Direction), Lazy<OpLists>>,
    /// The names PROJ reports for registered concatenations, by `O` row id (`N` rows).
    reported: HashMap<u32, (usize, usize)>,
}

/// A direction between a geographic 3D or geocentric CRS and WGS 84 other than into WGS 84 (3D),
/// in which PROJ may list other transformations, or the same in another order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Direction {
    /// From WGS 84 (3D): ONGD14 to WGS 84 (2), where PROJ runs (1) into it.
    From3d,
    /// Into WGS 84 (2D): LUREF to WGS 84 (4) before (3), which PROJ ranks the other way between
    /// 3D CRSs.
    Into2d,
    /// From WGS 84 (2D).
    From2d,
}

/// A line of the table (its fields after the code) and what it parses to.
struct Lazy<T> {
    line: (u32, u32),
    value: OnceLock<T>,
}

impl<T> Lazy<T> {
    fn new(start: usize, end: usize) -> Self {
        Self { line: (start as u32, end as u32), value: OnceLock::new() }
    }

    fn get(&self, text: &str, parse: impl FnOnce(&str) -> T) -> &T {
        self.value.get_or_init(|| parse(&text[self.line.0 as usize..self.line.1 as usize]))
    }
}

/// A table's decompressed text.
pub(crate) fn decompress(data: &[u8]) -> String {
    zrip::decompress(data).ok().and_then(|bytes| String::from_utf8(bytes).ok()).unwrap_or_default()
}

/// The EPSG table's text. Native builds embed it decompressed (`build.rs`, 7.7 MB of read-only
/// data), so that the first lookup of a process indexes it without decompressing it first.
/// WebAssembly modules keep it compressed and decompress it on first use.
fn epsg_text() -> Cow<'static, str> {
    #[cfg(not(target_family = "wasm"))]
    return Cow::Borrowed(include_str!(concat!(env!("OUT_DIR"), "/epsg.tsv")));
    #[cfg(target_family = "wasm")]
    Cow::Owned(decompress(DATA))
}

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| load(epsg_text()))
}

fn vertical_table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| load(decompress(VERTICAL_DATA).into()))
}

fn esri_table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| load(decompress(ESRI_DATA).into()))
}

/// Indexes the lines of a table's text.
fn load(text: Cow<'static, str>) -> Table {
    let (mut crs, mut crs_lines, mut ops, mut pairs) = (HashMap::new(), Vec::new(), HashMap::new(), HashMap::new());
    let (mut verticals, mut compounds, mut geoids) = (HashMap::new(), HashMap::new(), HashMap::<u32, Vec<_>>::new());
    let mut vertical_ops = HashMap::new();
    let mut alternatives = HashMap::<String, Vec<String>>::new();
    let mut dates = HashMap::new();
    let (mut geodetic, mut registered) = (Vec::new(), Vec::new());
    let mut replacements = HashMap::new();
    let mut directed = HashMap::new();
    let mut reported = HashMap::new();
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        let start = pos;
        pos += line.len();
        let line = line.trim_end_matches(['\n', '\r']);
        let end = start + line.len();
        let mut f = line.splitn(3, '\t');
        let (Some(kind), Some(key)) = (f.next(), f.next()) else { continue };
        // The rest of the line, after the kind and key fields.
        let rest = (start + kind.len() + key.len() + 2).min(end);
        // The rest after one more field.
        let after_second = || (rest + line[rest - start..].find('\t').map_or(end - rest, |i| i + 1)).min(end);
        let second = || line[rest - start..].split('\t').next().and_then(|v| v.parse::<u32>().ok());
        match kind {
            "O" => {
                if let Ok(id) = key.parse::<u32>() {
                    ops.insert(id, Lazy::new(rest, end));
                }
            }
            "C" => {
                if let Ok(code) = key.parse::<u32>() {
                    crs.insert(code, Lazy::new(rest, end));
                    crs_lines.push((code, (rest as u32, end as u32)));
                }
            }
            "P" => {
                if let (Ok(a), Some(b)) = (key.parse::<u32>(), second()) {
                    pairs.insert((a, b), Lazy::new(after_second(), end));
                }
            }
            "V" => {
                if let Ok(code) = key.parse::<u32>() {
                    verticals.insert(code, Lazy::new(rest, end));
                }
            }
            "K" => {
                if let Ok(code) = key.parse::<u32>() {
                    compounds.insert(code, Lazy::new(rest, end));
                }
            }
            "G" => {
                if let (Ok(g), Some(v)) = (key.parse::<u32>(), second()) {
                    geoids.entry(v).or_default().push((g, Lazy::new(after_second(), end)));
                }
            }
            "W" => {
                if let (Ok(a), Some(b)) = (key.parse::<u32>(), second()) {
                    vertical_ops.insert((a, b), Lazy::new(after_second(), end));
                }
            }
            "A" => {
                alternatives.entry(key.to_string()).or_default().push(line[rest - start..].to_string());
            }
            "D" => {
                if let Ok(code) = key.parse::<u32>() {
                    dates.insert(code, line[rest - start..].to_string());
                }
            }
            "X" => {
                if let Ok(code) = key.parse::<u32>() {
                    replacements.insert(code, line[rest - start..].split(',').filter_map(|c| c.parse().ok()).collect());
                }
            }
            "L" => {
                let direction = match line[rest - start..].split('\t').next() {
                    Some("f3") => Some(Direction::From3d),
                    Some("t2") => Some(Direction::Into2d),
                    Some("f2") => Some(Direction::From2d),
                    _ => None,
                };
                if let (Ok(code), Some(direction)) = (key.parse::<u32>(), direction) {
                    directed.insert((code, direction), Lazy::new(after_second(), end));
                }
            }
            "N" => {
                if let Ok(id) = key.parse::<u32>() {
                    reported.insert(id, (rest, end));
                }
            }
            "E" => geodetic.push((start + 2, end)),
            "R" => registered.push((start + 2, end)),
            _ => {}
        }
    }
    Table {
        text,
        crs,
        crs_lines,
        ops,
        pairs,
        verticals,
        compounds,
        geoids,
        vertical_ops,
        alternatives,
        dates,
        geodetic,
        registered,
        replacements,
        directed,
        reported,
    }
}

impl Table {
    /// The transformation with an id, parsed from its line (`bounds`, `accuracy`, `flags`,
    /// `steps`, `name`, `chain accuracy`, `chain bounds`).
    fn op(&self, id: u32) -> Option<Arc<Op>> {
        self.ops
            .get(&id)?
            .get(&self.text, |line| {
                let mut f = line.split('\t');
                let (Some(bounds), Some(accuracy), Some(flags)) = (f.next(), f.next(), f.next()) else { return None };
                let b: Vec<f64> = bounds.split(',').filter_map(|v| v.parse().ok()).collect();
                let (true, Some(steps)) = (b.len() == 4, parse_steps(f.next().unwrap_or(""))) else { return None };
                let name = f.next().unwrap_or("").to_string();
                let accuracy = accuracy.parse().unwrap_or(-1.0);
                let bounds = [b[0], b[1], b[2], b[3]];
                Some(Arc::new(Op {
                    bounds,
                    accuracy,
                    chain_accuracy: f.next().and_then(|a| a.parse().ok()).unwrap_or(accuracy),
                    chain_bounds: f
                        .next()
                        .map(|v| v.split(',').filter_map(|v| v.parse().ok()).collect::<Vec<f64>>())
                        .and_then(|v| <[f64; 4]>::try_from(v).ok())
                        .unwrap_or(bounds),
                    offshore: flags.contains('o'),
                    unknown_area: flags.contains('u'),
                    ballpark: flags.contains('b'),
                    framed: false,
                    concatenated: flags.contains('c'),
                    between_3d: flags.contains('h'),
                    geocentric: flags.contains('g'),
                    priority: flags.contains('p'),
                    keeps_height: flags.contains('z'),
                    keeps_wgs84_leg_height: false,
                    keeps_height_3d: flags.contains('k'),
                    reported: self.reported.get(&id).map(|&(start, end)| Arc::from(&self.text[start..end])),
                    steps,
                    name,
                }))
            })
            .clone()
    }

    /// The field of a transformation's line after its 8 others (the interpolation CRS of the
    /// vertical table).
    fn op_interpolation(&self, id: u32) -> Option<u32> {
        let lazy = self.ops.get(&id)?;
        self.text[lazy.line.0 as usize..lazy.line.1 as usize].split('\t').nth(7)?.parse().ok()
    }

    /// Vertical transformations by `ops<TAB>grid ops` lists of ids.
    fn vertical_lists(&self, ids: &str) -> VerticalLists {
        let list = |s: &str| -> Vec<VerticalOp> {
            s.split(',')
                .filter_map(|i| i.parse().ok())
                .filter_map(|i| Some(VerticalOp { op: self.op(i)?, interpolation: self.op_interpolation(i) }))
                .collect()
        };
        let (a, b) = ids.split_once('\t').unwrap_or((ids, ""));
        (list(a), list(b))
    }

    /// Transformations by a comma-separated list of ids.
    fn resolve(&self, ids: &str) -> Vec<Arc<Op>> {
        ids.split(',').filter_map(|i| i.parse().ok()).filter_map(|i| self.op(i)).collect()
    }

    fn entry(&self, code: u32) -> Option<&Entry> {
        self.crs
            .get(&code)?
            .get(&self.text, |line| {
                let fields: Vec<&str> = line.split('\t').collect();
                let [name, datum, geog, op_ids, grid_ids, flags, area, proj, wkt] = fields[..] else { return None };
                let area: Vec<f64> = area.split(',').filter_map(|v| v.parse().ok()).collect();
                Some(Box::new(Entry {
                    name: name.to_string(),
                    datum: datum.parse().unwrap_or(0),
                    geog: geog.parse().unwrap_or(0),
                    ops: self.resolve(op_ids),
                    grid_ops: self.resolve(grid_ids),
                    dynamic: flags.contains('d'),
                    three_d: flags.contains('3'),
                    area: (area.len() == 4).then(|| [area[0], area[1], area[2], area[3]]),
                    proj: proj.to_string(),
                    wkt: wkt.to_string(),
                }))
            })
            .as_deref()
    }

    fn lists(&self, ids: &str) -> OpLists {
        let (a, b) = ids.split_once('\t').unwrap_or((ids, ""));
        (self.resolve(a), self.resolve(b))
    }

    fn pair(&self, a: u32, b: u32) -> Option<&PairLists> {
        Some(self.pairs.get(&(a, b))?.get(&self.text, |ids| {
            let mut f = ids.split('\t');
            let mut next = || self.resolve(f.next().unwrap_or(""));
            PairLists { lists: (next(), next()) }
        }))
    }
}

/// An EPSG ellipsoid's semi-major axis in metres and inverse flattening (0 for a sphere), by its
/// code, as the registry's WKT 1 gives it, for GeoKeys giving the code without the values
/// (libgeotiff's `GTIFGetEllipsoidInfo`, #1012 item 34). Also EPSG:7047, the deprecated GRS 1980
/// authalic sphere no CRS is on (6370997 m, PROJ's `+ellps=sphere`, as GDAL 3.12 reads it).
pub(crate) fn ellipsoid(code: u32) -> Option<(f64, f64)> {
    if code == 7047 {
        return Some((6_370_997.0, 0.0));
    }
    static ELLIPSOIDS: OnceLock<HashMap<u32, (f64, f64)>> = OnceLock::new();
    ELLIPSOIDS.get_or_init(|| nodes("SPHEROID[").filter_map(|n| Some((n.epsg()?, (n.num(1)?, n.num(2)?)))).collect()).get(&code).copied()
}

/// An EPSG prime meridian's longitude in degrees, by its code, as the registry's WKT 1 gives it
/// (Paris, EPSG:8903, 2.33722917), for GeoKeys giving the code without the longitude (#1012
/// item 34).
pub(crate) fn prime_meridian(code: u32) -> Option<f64> {
    static MERIDIANS: OnceLock<HashMap<u32, f64>> = OnceLock::new();
    MERIDIANS.get_or_init(|| nodes("PRIMEM[").filter_map(|n| Some((n.epsg()?, n.num(1)?))).collect()).get(&code).copied()
}

/// The WKT 1 nodes of a keyword in the registry's CRSs.
fn nodes(keyword: &'static str) -> impl Iterator<Item = crate::wkt1::Node> {
    let text = &table().text;
    text.match_indices(keyword).filter_map(move |(i, _)| {
        let mut depth = 0;
        let end = text[i..].char_indices().find(|&(_, c)| {
            depth += match c {
                '[' => 1,
                ']' => -1,
                _ => 0,
            };
            c == ']' && depth == 0
        })?;
        crate::wkt1::parse(&text[i..=i + end.0])
    })
}

/// An EPSG geodetic or vertical datum's anchor epoch, as PROJ 9.9.0's database gives it
/// (`scripts/gen_datum_anchor_epochs.py`), which PROJ writes in PROJJSON (#1012 item 65).
pub(crate) fn anchor_epoch(datum: u32) -> Option<f64> {
    static EPOCHS: OnceLock<HashMap<u32, f64>> = OnceLock::new();
    EPOCHS
        .get_or_init(|| {
            include_str!("../data/datum_anchor_epochs.tsv")
                .lines()
                .filter(|l| !l.starts_with('#'))
                .filter_map(|l| {
                    let (code, epoch) = l.split_once('\t')?;
                    Some((code.parse().ok()?, epoch.parse().ok()?))
                })
                .collect()
        })
        .get(&datum)
        .copied()
}

/// The transformations PROJ lists for an EPSG geographic 3D or geocentric CRS in a direction
/// other than into WGS 84 (3D), without and with grids (as [`Entry::ops`] and
/// [`Entry::grid_ops`]), when they differ from those.
pub(crate) fn directed(code: u32, direction: Direction) -> Option<&'static OpLists> {
    let t = table();
    Some(t.directed.get(&(code, direction))?.get(&t.text, |ids| t.lists(ids)))
}

/// The operations GDAL uses given the grids that can be found: those listed with grids, without
/// the ones whose grids are missing, when at least one operation needing grids remains; else
/// those listed without grids.
pub(crate) fn usable(ops: &[Arc<Op>], grid_ops: &[Arc<Op>]) -> Vec<Arc<Op>> {
    if grid_ops.is_empty() {
        return ops.to_vec();
    }
    let kept: Vec<Arc<Op>> = grid_ops.iter().filter(|o| o.available()).cloned().collect();
    if kept.iter().any(|o| o.needs_grids() && !o.ballpark) { kept } else { ops.to_vec() }
}

/// Starts loading the table in the background, for work that will need it after reading its
/// input (native builds; a no-op in WebAssembly).
pub(crate) fn prefetch() {
    #[cfg(not(target_family = "wasm"))]
    crate::host::spawn(|| {
        table();
    });
}

/// The table's entry for an EPSG code.
pub(crate) fn get(code: u32) -> Option<&'static Entry> {
    table().entry(code).or_else(|| variant_c(code))
}

/// EPSG's CRSs on Polar Stereographic (variant C), method 9830, which no `+proj=` string names:
/// the generated table, which holds each CRS's conversion as one, leaves them out. Their name and
/// geographic CRS, and the latitude of the standard parallel, longitude of origin, and easting and
/// northing at the false origin of their projection (in degrees and metres).
const VARIANT_C: [(u32, &str, u32, [f64; 4]); 2] = [
    (2985, "Petrels 1972 / Terre Adelie Polar Stereographic", 4636, [-67.0, 140.0, 300_000.0, 200_000.0]),
    (2986, "Perroud 1950 / Terre Adelie Polar Stereographic", 4637, [-67.0, 140.0, 300_000.0, 200_000.0]),
];

/// Whether a code is one of [`VARIANT_C`], whose method no PROJ string names.
pub(crate) fn is_variant_c_code(code: u32) -> bool {
    VARIANT_C.iter().any(|(c, ..)| *c == code)
}

/// The entry of a CRS of [`VARIANT_C`]: its geographic CRS's datum, transformations and area of
/// use, with the WKT 1 of its definition and the PROJ string the WKT 1 reader gives it.
fn variant_c(code: u32) -> Option<&'static Entry> {
    static ENTRIES: OnceLock<Vec<(u32, Option<Entry>)>> = OnceLock::new();
    let entries = ENTRIES.get_or_init(|| {
        VARIANT_C
            .iter()
            .map(|&(code, name, geog, [lat_ts, lon_0, fe, fn_])| {
                let entry = table().entry(geog).and_then(|g| {
                    // The geographic CRS's WKT without its axes, as a projected CRS's holds it.
                    let mut geogcs = g.wkt.clone();
                    while let Some(i) = geogcs.find(",AXIS[") {
                        let len = geogcs[i..].find(']')?;
                        geogcs.replace_range(i..i + len + 1, "");
                    }
                    let wkt = format!(
                        r#"PROJCS["{name}",{geogcs},PROJECTION["Polar_Stereographic_(variant_C)"],PARAMETER["Latitude of standard parallel",{lat_ts}],PARAMETER["Longitude of origin",{lon_0}],PARAMETER["Easting at false origin",{fe}],PARAMETER["Northing at false origin",{fn_}],UNIT["metre",1,AUTHORITY["EPSG","9001"]],AUTHORITY["EPSG","{code}"]]"#
                    );
                    let proj = crate::model::wkt1_to_proj(&wkt)?.proj;
                    Some(Entry {
                        name: name.to_string(),
                        datum: g.datum,
                        geog,
                        ops: g.ops.clone(),
                        grid_ops: g.grid_ops.clone(),
                        dynamic: g.dynamic,
                        three_d: false,
                        area: g.area,
                        proj,
                        wkt,
                    })
                });
                (code, entry)
            })
            .collect()
    });
    entries.iter().find(|(c, _)| *c == code)?.1.as_ref()
}

/// The table's entry for an ESRI code.
pub(crate) fn esri(code: u32) -> Option<&'static Entry> {
    esri_table().entry(code)
}

/// An index of a table's CRSs by name, as `key` gives it.
fn name_index(t: &Table, key: impl Fn(&str) -> String) -> HashMap<String, Vec<u32>> {
    let mut names = HashMap::<String, Vec<u32>>::new();
    for &(code, (start, end)) in &t.crs_lines {
        if let Some(n) = t.text[start as usize..end as usize].split('\t').next() {
            names.entry(key(n)).or_default().push(code);
        }
    }
    names.values_mut().for_each(|c| c.sort_unstable());
    names
}

/// The codes of the CRSs with an official name, whatever its case, in order: from an index of
/// the names built on first use.
pub(crate) fn named(name: &str) -> Vec<u32> {
    static NAMES: OnceLock<HashMap<String, Vec<u32>>> = OnceLock::new();
    let names = NAMES.get_or_init(|| name_index(table(), str::to_ascii_lowercase));
    names.get(&name.to_ascii_lowercase()).cloned().unwrap_or_default()
}

/// A datum name as datums are compared by name: its ASCII letters, digits and `+` (CH1903+ is
/// not CH1903), whatever their case, without ESRI's `D_` prefix.
pub(crate) fn datum_key(name: &str) -> String {
    let name = name.strip_prefix("D_").unwrap_or(name);
    name.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '+').map(|c| c.to_ascii_lowercase()).collect()
}

/// The rank of the names ESRI writes for CRSs on another datum (`D_ETRS_1989` for ETRS89-NOR).
const ESRI_WRITTEN: u8 = 5;

/// The datum names of the table by rank: those of ranks 0 to 4, and those ESRI writes.
/// The datums of each name, of each name GDAL writes, and the official name of each datum.
type DatumNames = (HashMap<String, (u8, Vec<u32>)>, HashMap<String, Vec<u32>>, HashMap<u32, String>);

fn datum_names() -> &'static DatumNames {
    static NAMES: OnceLock<DatumNames> = OnceLock::new();
    NAMES.get_or_init(|| {
        let (mut names, mut written) = (HashMap::<String, (u8, Vec<u32>)>::new(), HashMap::<String, Vec<u32>>::new());
        let mut official = HashMap::<u32, String>::new();
        for line in decompress(DATUM_NAMES).lines().filter(|l| !l.starts_with('#')) {
            let mut f = line.splitn(3, '\t');
            let (Some(Ok(code)), Some(Ok(rank)), Some(name)) = (f.next().map(str::parse::<u32>), f.next().map(str::parse::<u8>), f.next())
            else {
                continue;
            };
            if rank == ESRI_WRITTEN {
                written.entry(datum_key(name)).or_default().push(code);
                continue;
            }
            // Rank 0 holds the official name and GDAL's WKT 1 name (`WGS_1984`): the former.
            if rank == 0 && official.get(&code).is_none_or(|n| n.contains('_') && !name.contains('_')) {
                official.insert(code, name.to_string());
            }
            let e = names.entry(datum_key(name)).or_insert((rank, Vec::new()));
            if rank < e.0 {
                *e = (rank, Vec::new());
            }
            if rank == e.0 && !e.1.contains(&code) {
                e.1.push(code);
            }
        }
        names.values_mut().for_each(|(_, c)| c.sort_unstable());
        written.values_mut().for_each(|c| c.sort_unstable());
        // Without it, every WKT without a datum code would be left without EPSG code.
        debug_assert!(!names.is_empty(), "the datum name table did not decompress");
        (names, written, official)
    })
}

/// The EPSG datums a name stands for, as PROJ's database lists them: those it is the official
/// name (or GDAL's WKT 1 name) of, else an EPSG alias of, else a PROJ, an ESRI (`D_ETRS_1989`)
/// or a former name of, in order of code.
pub(crate) fn datums_named(name: &str) -> Vec<u32> {
    datum_names().0.get(&datum_key(name)).map(|(_, c)| c.clone()).unwrap_or_default()
}

/// The best rank of a datum name in the table: 0 for an official name or GDAL's WKT 1 name, 1
/// for an EPSG alias, 2 for PROJ's, 3 for ESRI's (`D_ETRS_1989`), 4 for a former name.
pub(crate) fn datum_name_rank(name: &str) -> Option<u8> {
    datum_names().0.get(&datum_key(name)).map(|(rank, _)| *rank)
}

/// The names of vertical CRSs and datums by kind (`c`, `e`, `d`; see
/// `scripts/gen_vertical_names.py`), each with the codes it stands for.
fn vertical_names() -> &'static HashMap<(u8, String), Vec<u32>> {
    static NAMES: OnceLock<HashMap<(u8, String), Vec<u32>>> = OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names: HashMap<(u8, String), Vec<u32>> = HashMap::new();
        for l in decompress(VERTICAL_NAMES).lines().filter(|l| !l.starts_with('#')) {
            let mut f = l.splitn(3, '\t');
            let (Some(kind), Some(name), Some(Ok(code))) = (f.next(), f.next(), f.next().map(str::parse)) else { continue };
            let Some(&kind) = kind.as_bytes().first() else { continue };
            names.entry((kind, name.to_string())).or_default().push(code);
        }
        debug_assert!(!names.is_empty(), "the vertical name table did not decompress");
        names
    })
}

/// The EPSG vertical CRSs PROJ 9.9.0 finds by the name of a vertical CRS without ID: among the
/// names and aliases of its database's vertical CRSs, whatever the case of their ASCII letters;
/// for ESRI's `VERTCS` (`esri`), first the CRS of the official name the name is an ESRI alias of,
/// else the name with spaces for its underscores (`Cais_da_Pontinha_depth`); else those of an
/// equivalent name, as PROJ compares names (`GLLAT_2025_depth` is `GLLAT(2025) depth`).
pub(crate) fn verticals_named(name: &str, esri: bool) -> Vec<u32> {
    let names = vertical_names();
    let spaced = esri.then(|| name.replace('_', " ").to_ascii_lowercase());
    esri.then(|| names.get(&(b'e', name.to_string())))
        .flatten()
        .or_else(|| spaced.and_then(|n| names.get(&(b'c', n))))
        .or_else(|| names.get(&(b'c', name.to_ascii_lowercase())))
        .or_else(|| names.get(&(b'n', equivalent_name(name))))
        .cloned()
        .unwrap_or_default()
}

/// The EPSG vertical CRSs on the datum (or datum ensemble) a name is the name or an alias of, as
/// PROJ compares names (`Identifier::isEquivalentName`: without spaces, `_-/().&,`, case and `19`
/// before two digits).
pub(crate) fn verticals_on_datum_named(name: &str) -> Vec<u32> {
    vertical_names().get(&(b'd', equivalent_name(name))).cloned().unwrap_or_default()
}

/// A name as PROJ's `Identifier::isEquivalentName` compares it: its ASCII letters in lowercase,
/// without spaces and `_-/().&,`, and without the `19` of a year before two digits (`1988` is
/// `88`) unless a digit precedes it.
fn equivalent_name(name: &str) -> String {
    let kept: Vec<char> = name.chars().filter(|c| !" _-/().&,".contains(*c)).map(|c| c.to_ascii_lowercase()).collect();
    let mut out = String::with_capacity(kept.len());
    let mut i = 0;
    while i < kept.len() {
        let year = kept[i] == '1'
            && kept.get(i + 1) == Some(&'9')
            && kept.get(i + 2).is_some_and(char::is_ascii_digit)
            && kept.get(i + 3).is_some_and(char::is_ascii_digit)
            && (i == 0 || !kept[i - 1].is_ascii_digit());
        if year {
            i += 2;
            continue;
        }
        out.push(kept[i]);
        i += 1;
    }
    out
}

/// The EPSG CRS PROJ 9.8.1 finds by the name of a geographic 2D or projected CRS without ID, when
/// it finds a single CRS (`createObjectsFromName`, among the names and aliases of its database's
/// CRSs, whatever the case of their ASCII letters); first, for an ESRI WKT 1 (`esri`), the CRS the
/// name is an ESRI alias of. A name absent from the table stands for the single EPSG CRS of that
/// official name and kind, if any. PROJ looks a geographic 3D CRS (`three_d`) up among those,
/// which the table leaves out: by official name alone.
pub(crate) fn crs_named_as_proj(name: &str, projected: bool, three_d: bool, esri: bool) -> Option<u32> {
    static NAMES: OnceLock<HashMap<(u8, String), u32>> = OnceLock::new();
    let names = NAMES.get_or_init(|| {
        decompress(CRS_NAMES)
            .lines()
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| {
                let mut f = l.splitn(3, '\t');
                let (kind, name, code) = (f.next()?, f.next()?, f.next()?.parse().ok()?);
                Some(((*kind.as_bytes().first()?, name.to_string()), code))
            })
            .collect()
    });
    let (exact, folded) = if projected { (b'P', b'p') } else { (b'G', b'g') };
    let found = if three_d && !projected {
        None
    } else {
        (esri.then(|| names.get(&(exact, name.to_string()))).flatten()).or_else(|| names.get(&(folded, name.to_ascii_lowercase())))
    };
    match found {
        Some(&code) => (code != 0).then_some(code),
        // PROJ drops the deprecated CRSs after current ones (RRAF 1991: EPSG:4557, not 4973).
        None => {
            let codes = officially_named().get(&(kind_of(projected, three_d), name.to_ascii_lowercase()))?;
            let current: Vec<u32> = codes.iter().filter(|c| c.1).map(|c| c.0).collect();
            match (&current[..], &codes[..]) {
                ([code], _) => Some(*code),
                ([], [(code, _)]) => Some(*code),
                _ => None,
            }
        }
    }
}

/// The kind of CRS PROJ looks a name up among: `p` projected, `g` geographic 2D, `G` 3D.
fn kind_of(projected: bool, three_d: bool) -> u8 {
    match (projected, three_d) {
        (true, _) => b'p',
        (false, false) => b'g',
        (false, true) => b'G',
    }
}

/// EPSG codes by kind and name, with whether each is current.
type NameIndex = HashMap<(u8, String), Vec<(u32, bool)>>;

/// The fields of a CRS line before its WKT: its name, datum, geographic CRS, transformation and
/// grid transformation ids, flags, area of use and PROJ string. `None` for a line without its
/// WKT field, which has no entry either.
fn crs_fields(t: &'static Table, (start, end): (u32, u32)) -> Option<[&'static str; 8]> {
    let mut f = t.text[start as usize..end as usize].splitn(9, '\t');
    let mut fields = [""; 8];
    for field in &mut fields {
        *field = f.next()?;
    }
    f.next().map(|_| fields)
}

/// The EPSG geographic and projected CRSs of the table by kind ([`kind_of`]) and official name,
/// its ASCII letters in lowercase, with whether each is current.
fn officially_named() -> &'static NameIndex {
    static INDEX: OnceLock<NameIndex> = OnceLock::new();
    INDEX.get_or_init(|| {
        let t = table();
        let mut index = NameIndex::new();
        for &(code, line) in &t.crs_lines {
            let Some([name, _, _, _, _, flags, _, proj]) = crs_fields(t, line) else { continue };
            let kind = if proj.starts_with("+proj=longlat") {
                kind_of(false, flags.contains('3'))
            } else if proj.starts_with("+proj=geocent") {
                continue;
            } else {
                b'p'
            };
            let current = !flags.contains('x');
            index.entry((kind, name.to_ascii_lowercase())).or_default().push((code, current));
        }
        index
    })
}

/// Whether an EPSG CRS (geographic, geocentric, projected, vertical or compound) is deprecated.
pub(crate) fn is_deprecated(code: u32) -> bool {
    table().replacements.contains_key(&code)
}

/// The EPSG CRSs that replace a deprecated one, those PROJ lists, in order of code (none for a
/// current CRS, or a deprecated one without replacement).
pub(crate) fn replacements(code: u32) -> &'static [u32] {
    table().replacements.get(&code).map_or(&[], Vec::as_slice)
}

/// The official name of an EPSG datum; of the WGS 84 and ETRS89 ensembles, without `ensemble`
/// (they are written as datums).
pub(crate) fn datum_name(code: u32) -> Option<String> {
    datum_names().2.get(&code).map(|n| n.strip_suffix(" ensemble").unwrap_or(n).to_string())
}

/// Whether two EPSG datums have a name or alias in common in PROJ's database (`GR96` for
/// Greenland 1996 and its ensemble).
pub(crate) fn datums_share_a_name(a: u32, b: u32) -> bool {
    a == b || datum_names().0.values().any(|(_, c)| c.contains(&a) && c.contains(&b))
}

/// The other EPSG datums whose CRSs ESRI writes with a datum name: ETRS89-NOR for
/// `D_ETRS_1989` (ETRS89-NOR [EUREF89] / NTM zone 18, EPSG:5118), S-JTSK (Ferro) for `D_S_JTSK`.
pub(crate) fn datums_written_as(name: &str) -> &'static [u32] {
    datum_names().1.get(&datum_key(name)).map_or(&[], Vec::as_slice)
}

/// The EPSG geographic 2D CRSs on a datum, current ones first (as PROJ ranks deprecated CRSs
/// after them), each in order of code.
pub(crate) fn geographic_on(datum: u32) -> &'static [u32] {
    static INDEX: OnceLock<HashMap<u32, Vec<u32>>> = OnceLock::new();
    let index = INDEX.get_or_init(|| {
        let t = table();
        let mut index = HashMap::<u32, Vec<u32>>::new();
        for &(code, line) in &t.crs_lines {
            let Some([_, datum_field, geog, _, _, flags, _, proj]) = crs_fields(t, line) else { continue };
            if geog.parse() == Ok(code)
                && !flags.contains('3')
                && proj.starts_with("+proj=longlat")
                && let Ok(d) = datum_field.parse::<u32>()
            {
                index.entry(d).or_default().push(code);
            }
        }
        index.values_mut().for_each(|c| c.sort_unstable_by_key(|c| (is_deprecated(*c), *c)));
        index
    });
    index.get(&datum).map_or(&[], Vec::as_slice)
}

/// The EPSG projected CRSs on a geographic CRS, current ones first, each in order of code, with
/// their projection method (`+proj=tmerc`).
pub(crate) fn projected_on(geog: u32) -> &'static [(u32, &'static str)] {
    static INDEX: OnceLock<HashMap<u32, Vec<(u32, &'static str)>>> = OnceLock::new();
    let index = INDEX.get_or_init(|| {
        let t = table();
        let mut index = HashMap::<u32, Vec<(u32, &'static str)>>::new();
        for &(code, line) in &t.crs_lines {
            let Some([_, _, geog, _, _, flags, _, proj]) = crs_fields(t, line) else { continue };
            let method = proj.split_whitespace().next().unwrap_or_default();
            if !flags.contains('3')
                && method != "+proj=longlat"
                && let Ok(g) = geog.parse::<u32>()
            {
                index.entry(g).or_default().push((code, method));
            }
        }
        index.values_mut().for_each(|c| c.sort_unstable_by_key(|(c, _)| (is_deprecated(*c), *c)));
        index
    });
    index.get(&geog).map_or(&[], Vec::as_slice)
}

/// The ESRI codes of the CRSs with a name, whatever its case and with spaces for underscores
/// (`World Mollweide` for `World_Mollweide`), in order.
pub(crate) fn esri_named(name: &str) -> Vec<u32> {
    static NAMES: OnceLock<HashMap<String, Vec<u32>>> = OnceLock::new();
    let key = |n: &str| n.to_ascii_lowercase().replace(' ', "_");
    let names = NAMES.get_or_init(|| name_index(esri_table(), key));
    names.get(&key(name)).cloned().unwrap_or_default()
}

/// The kind of a geodetic CRS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GeodeticKind {
    Geographic2D,
    Geographic3D,
    Geocentric,
}

/// An EPSG geodetic CRS (geographic 2D or 3D, or geocentric), as PROJ chains transformations
/// through them.
#[derive(Debug)]
pub(crate) struct Geodetic {
    pub code: u32,
    pub datum: u32,
    pub kind: GeodeticKind,
    pub name: String,
}

/// A transformation PROJ's database registers between two geodetic CRSs of different datums.
#[derive(Debug)]
pub(crate) struct Registered {
    /// Its authority and code (`EPSG:1321`).
    pub key: String,
    /// The CRSs PROJ's database registers it from and to.
    pub source: u32,
    pub target: u32,
    /// The transformation, run between the geographic coordinates of the two datums.
    pub op: Arc<Op>,
    /// The keys of the transformations that replace it between the same CRSs.
    pub replaced_by: Vec<String>,
    /// Whether it needs the coordinates' epochs (a time-dependent transformation).
    pub time_dependent: bool,
    /// Whether it is PROJ's null transformation between a datum ensemble and one of its members
    /// (WGS 84 to WGS 84 (G1762)).
    pub ensemble: bool,
    /// The CRSs a concatenation goes through.
    pub via: Vec<u32>,
}

/// The geodetic CRSs and registered transformations PROJ chains transformations with.
#[derive(Debug, Default)]
pub(crate) struct Registry {
    pub geodetic: HashMap<u32, Geodetic>,
    /// The codes of the geodetic CRSs of each datum, in order.
    pub by_datum: HashMap<u32, Vec<u32>>,
    pub ops: Vec<Registered>,
    /// The indices in `ops` of the transformations registered from or to each CRS.
    pub at: HashMap<u32, Vec<usize>>,
}

/// The geodetic CRSs and the transformations PROJ's database registers between them, which
/// terrio chains through intermediate datums as PROJ does.
pub(crate) fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let t = table();
        let mut r = Registry::default();
        for &(start, end) in &t.geodetic {
            let f: Vec<&str> = t.text[start..end].splitn(4, '\t').collect();
            let [code, datum, kind, name] = f[..] else { continue };
            // A deprecated CRS is chained from or to itself, not through, as PROJ's datum pivot
            // goes through the current CRSs of a datum.
            let (kind, deprecated) = kind.strip_suffix('x').map_or((kind, false), |k| (k, true));
            let kind = match kind {
                "2" => GeodeticKind::Geographic2D,
                "3" => GeodeticKind::Geographic3D,
                "g" => GeodeticKind::Geocentric,
                _ => continue,
            };
            let (Ok(code), Ok(datum)) = (code.parse(), datum.parse()) else { continue };
            if !deprecated {
                r.by_datum.entry(datum).or_default().push(code);
            }
            r.geodetic.insert(code, Geodetic { code, datum, kind, name: name.to_string() });
        }
        r.by_datum.values_mut().for_each(|c| c.sort_unstable());
        let codes = |s: &str| s.split(',').filter_map(|v| v.parse().ok()).collect::<Vec<u32>>();
        for &(start, end) in &t.registered {
            let f: Vec<&str> = t.text[start..end].split('\t').collect();
            let [key, source, target, id, replaced_by, flags, via] = f[..] else { continue };
            let (Ok(source), Ok(target), Some(op)) = (source.parse(), target.parse(), id.parse().ok().and_then(|i| t.op(i))) else {
                continue;
            };
            let i = r.ops.len();
            r.at.entry(source).or_default().push(i);
            r.at.entry(target).or_default().push(i);
            r.ops.push(Registered {
                key: key.to_string(),
                source,
                target,
                op,
                replaced_by: replaced_by.split(',').filter(|v| !v.is_empty()).map(str::to_string).collect(),
                time_dependent: flags.contains('t'),
                ensemble: flags.contains('e'),
                via: codes(via),
            });
        }
        r
    })
}

/// The direct transformations from one geographic CRS to another (those GDAL uses given the
/// grids available), and whether they run in reverse (the table holds each pair once).
pub(crate) fn pair(from: u32, to: u32) -> Option<(Vec<Arc<Op>>, bool)> {
    let t = table();
    let (p, reverse) = t.pair(from, to).map(|o| (o, false)).or_else(|| t.pair(to, from).map(|o| (o, true)))?;
    Some((usable(&p.lists.0, &p.lists.1), reverse))
}

/// The direct operations assuming every grid is available.
pub(crate) fn pair_all(from: u32, to: u32) -> Option<(Vec<Arc<Op>>, bool)> {
    let t = table();
    let (p, reverse) = t.pair(from, to).map(|o| (o, false)).or_else(|| t.pair(to, from).map(|o| (o, true)))?;
    Some((if p.lists.1.is_empty() { p.lists.0.clone() } else { p.lists.1.clone() }, reverse))
}

/// An EPSG vertical CRS.
pub(crate) fn vertical(code: u32) -> Option<&'static Vertical> {
    let t = table();
    t.verticals
        .get(&code)?
        .get(&t.text, |line| {
            let f: Vec<&str> = line.split('\t').collect();
            let [name, datum, unit, direction, wkt] = f[..] else { return None };
            Some(Vertical {
                name: name.to_string(),
                datum: datum.parse().unwrap_or(0),
                to_meter: unit.parse().unwrap_or(1.0),
                down: direction == "down",
                wkt: wkt.to_string(),
            })
        })
        .as_ref()
}

/// An EPSG compound CRS.
pub(crate) fn compound(code: u32) -> Option<&'static Compound> {
    let t = table();
    t.compounds
        .get(&code)?
        .get(&t.text, |line| {
            let f: Vec<&str> = line.split('\t').collect();
            let (name, h, v, wkt, area) = match f[..] {
                [name, h, v, wkt] => (name, h, v, wkt, ""),
                [name, h, v, wkt, area] => (name, h, v, wkt, area),
                _ => return None,
            };
            let area: Vec<f64> = area.split(',').filter_map(|x| x.parse().ok()).collect();
            let area = <[f64; 4]>::try_from(area).ok();
            Some(Compound { name: name.to_string(), horizontal: h.parse().ok()?, vertical: v.parse().ok()?, wkt: wkt.to_string(), area })
        })
        .as_ref()
}

/// The EPSG compound CRS of a horizontal and a vertical EPSG CRS: the only one of the table, or
/// the one of several named `name` (the table keeps deprecated entries: 7412, of 27572 and 5719
/// as 7411 is, but named otherwise). `None` when there is none, or no single one.
pub(crate) fn compound_of(horizontal: u32, vertical: u32, name: Option<&str>) -> Option<u32> {
    compound_among(horizontal, vertical, name, false)
}

/// The current EPSG compound CRS of a horizontal and a vertical EPSG CRS, as PROJ 9.9.0 identifies
/// a compound CRS by its parts (`createCompoundCRSFromExisting` leaves deprecated ones out): of
/// 27572 and 5719, EPSG:7411, not the deprecated 7412.
pub(crate) fn current_compound_of(horizontal: u32, vertical: u32, name: Option<&str>) -> Option<u32> {
    compound_among(horizontal, vertical, name, true)
}

fn compound_among(horizontal: u32, vertical: u32, name: Option<&str>, current: bool) -> Option<u32> {
    let mut codes: Vec<u32> = table()
        .compounds
        .keys()
        .copied()
        .filter(|code| compound(*code).is_some_and(|c| (c.horizontal, c.vertical) == (horizontal, vertical)))
        .filter(|code| !current || !is_deprecated(*code))
        .collect();
    codes.sort_unstable();
    match codes[..] {
        [code] => Some(code),
        _ => {
            let named: Vec<u32> = codes.into_iter().filter(|c| compound(*c).is_some_and(|c| Some(c.name.as_str()) == name)).collect();
            (named.len() == 1).then(|| named[0])
        }
    }
}

/// The geoid models (transformations from geographic 3D CRSs) of a vertical CRS, with the
/// code of their geographic 3D CRS and of the vertical CRS they are defined to, as GDAL uses them
/// given the grids available.
///
/// A vertical CRS with none of its own (NAVD88 height (ftUS), NAVD88 depth) uses those of the
/// vertical CRSs of its datum (NAVD88 height's GEOID18), as PROJ chains them after a conversion
/// between the two: the models give heights in metres, up, which [`Vertical`]'s unit and
/// direction convert.
pub(crate) fn geoid_models(vertical: u32) -> Vec<(u32, u32, Vec<Arc<Op>>)> {
    let t = table();
    let models = |v: u32, list: &[(u32, Lazy<OpLists>)]| -> Vec<(u32, u32, Vec<Arc<Op>>)> {
        list.iter()
            .filter_map(|(g, lazy)| {
                let (ops, grid_ops) = lazy.get(&t.text, |ids| t.lists(ids));
                (!(ops.is_empty() && grid_ops.is_empty())).then(|| (*g, v, usable(ops, grid_ops)))
            })
            .collect()
    };
    // Its own models when it has any, even when none is usable with the grids available.
    if let Some(list) = t.geoids.get(&vertical) {
        return models(vertical, list);
    }
    // One vertical CRS of the datum has models in the table (`datum_fallbacks_have_one_source`),
    // so PROJ's choice does not depend on an order among them.
    let Some(datum) = self::vertical(vertical).map(|v| v.datum).filter(|d| *d != 0) else { return Vec::new() };
    let mut same: Vec<u32> = t.geoids.keys().copied().filter(|&c| self::vertical(c).is_some_and(|v| v.datum == datum)).collect();
    same.sort_unstable();
    same.into_iter().flat_map(|v| models(v, &t.geoids[&v])).collect()
}

/// The geoid models of a vertical CRS (as [`geoid_models`] finds them) that need grids, whether
/// or not the grids can be found: when none of them can, [`geoid_models`] falls back to those
/// listed without grids, which keep heights unchanged.
pub(crate) fn geoid_grid_models(vertical: u32) -> Vec<Arc<Op>> {
    let t = table();
    let grids = |list: &[(u32, Lazy<OpLists>)]| -> Vec<Arc<Op>> {
        list.iter()
            .flat_map(|(_, lazy)| {
                // The list with grids is stored empty when it equals the one without
                let (ops, grid_ops) = lazy.get(&t.text, |ids| t.lists(ids));
                if grid_ops.is_empty() { ops } else { grid_ops }.iter().filter(|o| o.needs_grids()).cloned().collect::<Vec<_>>()
            })
            .collect()
    };
    if let Some(list) = t.geoids.get(&vertical) {
        return grids(list);
    }
    let Some(datum) = self::vertical(vertical).map(|v| v.datum).filter(|d| *d != 0) else { return Vec::new() };
    t.geoids.iter().filter(|(c, _)| self::vertical(**c).is_some_and(|v| v.datum == datum)).flat_map(|(_, l)| grids(l)).collect()
}

/// The transformations from a vertical CRS to another that PROJ lists (vertical offsets, offsets
/// and slopes, VERTCON and other grids, chained through an intermediate vertical CRS), given the
/// grids that can be found. Empty when the EPSG dataset connects them by none but PROJ's ballpark.
pub(crate) fn vertical_transformations(source: u32, target: u32) -> Vec<VerticalOp> {
    let t = vertical_table();
    let Some(lazy) = t.vertical_ops.get(&(source, target)) else { return Vec::new() };
    let (ops, grid_ops) = lazy.get(&t.text, |ids| t.vertical_lists(ids));
    let arcs = |l: &[VerticalOp]| l.iter().map(|v| v.op.clone()).collect::<Vec<_>>();
    let chosen = usable(&arcs(ops), &arcs(grid_ops));
    let all: Vec<&VerticalOp> = grid_ops.iter().chain(ops.iter()).collect();
    chosen.iter().filter_map(|c| all.iter().find(|v| Arc::ptr_eq(&v.op, c)).map(|v| (*v).clone())).collect()
}

/// The transformations from a vertical CRS to another that PROJ lists when every grid can be
/// found (see [`vertical_transformations`]).
pub(crate) fn vertical_transformations_all(source: u32, target: u32) -> Vec<VerticalOp> {
    let t = vertical_table();
    let Some(lazy) = t.vertical_ops.get(&(source, target)) else { return Vec::new() };
    let (ops, grid_ops) = lazy.get(&t.text, |ids| t.vertical_lists(ids));
    if grid_ops.is_empty() { ops.clone() } else { grid_ops.clone() }
}

/// The publication date of an EPSG datum (`2007-01-01`), when it has one.
pub(crate) fn datum_date(datum: u32) -> Option<&'static str> {
    table().dates.get(&datum).map(String::as_str)
}

/// The names a PROJ-data grid had before PROJ 7.
pub(crate) fn grid_alternatives(name: &str) -> Vec<String> {
    table().alternatives.get(name).cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Indexing the table writes each CRS line's position and an empty slot, a pointer and its
    /// once-state: a slot holding an [`Entry`] itself would take the size of one for every CRS line.
    #[test]
    fn crs_index_slots_do_not_hold_parsed_entries() {
        fn slot_size<V>(_: &HashMap<u32, V>) -> usize {
            std::mem::size_of::<V>()
        }
        assert!(slot_size(&table().crs) <= std::mem::size_of::<(u32, u32)>() + 2 * std::mem::size_of::<usize>());
    }

    /// The indexes built by scanning the CRS lines in the table's order see each CRS of the map
    /// once, at its line.
    #[test]
    fn crs_lines_are_the_indexed_lines_in_table_order() {
        for t in [table(), esri_table()] {
            assert_eq!(t.crs_lines.len(), t.crs.len());
            assert!(t.crs_lines.iter().all(|(code, line)| t.crs[code].line == *line));
            assert!(t.crs_lines.windows(2).all(|w| w[0].1.0 < w[1].1.0));
        }
    }

    #[test]
    fn geocentric_registry_entries_are_usable() {
        let codes: Vec<u32> = table()
            .crs
            .iter()
            .filter(|(_, lazy)| {
                table().text[lazy.line.0 as usize..lazy.line.1 as usize]
                    .split('\t')
                    .nth(7)
                    .is_some_and(|proj| proj.starts_with("+proj=geocent"))
            })
            .map(|(code, _)| *code)
            .collect();
        assert!(!codes.is_empty());
        for code in codes {
            let crs = crate::SpatialRef::from_epsg(code).unwrap();
            assert!(crs.is_geocentric() && !crs.is_geographic(), "EPSG:{code}");
            let t = crate::Transformer::new(&crs, &crs).unwrap();
            assert_eq!(t.transform_3d(1.0, 2.0, 3.0).unwrap(), (1.0, 2.0, 3.0));
        }
    }

    /// The vertical CRSs that use the geoid models of another vertical CRS of their datum, with
    /// it. Each datum has one vertical CRS with models, so the models used are those PROJ chains
    /// (PROJ 9.9.0, checked for each). Another would need ordering as PROJ does.
    #[test]
    fn datum_fallbacks_have_one_source() {
        let t = table();
        let mut fallbacks: Vec<(u32, u32)> = Vec::new();
        for &code in t.verticals.keys() {
            if t.geoids.contains_key(&code) {
                continue;
            }
            let models = geoid_models(code);
            let mut sources: Vec<u32> = models.iter().map(|(_, v, _)| *v).collect();
            sources.dedup();
            assert!(sources.len() <= 1, "EPSG:{code} uses the models of {sources:?}");
            if let Some(&v) = sources.first() {
                fallbacks.push((code, v));
            }
        }
        fallbacks.sort_unstable();
        let expected = [
            (5715, 5714),
            (6357, 5703),
            (6358, 5703),
            (6360, 5703),
            (8050, 5714),
            (8051, 5714),
            (8052, 5714),
            (8053, 5714),
            (8228, 5703),
            (8358, 8357),
            (10364, 5780),
            (10919, 10918),
        ];
        assert_eq!(fallbacks, expected);
    }

    /// The embedded table is the compressed one's text, so every lookup gives what it did when the
    /// table was decompressed on first use.
    #[test]
    fn embedded_table_is_the_compressed_table() {
        let text = epsg_text();
        assert!(text.len() > 1_000_000);
        assert!(text == decompress(DATA));
    }

    #[test]
    fn table_loads() {
        let e = get(2263).unwrap();
        assert_eq!(e.name, "NAD83 / New York Long Island (ftUS)");
        assert!(e.proj.contains("+units=us-ft"));
        assert!(e.wkt.starts_with("PROJCS[\"NAD83 / New York Long Island (ftUS)\""));
        assert_eq!(e.datum, 6269);
        assert!(get(23030).unwrap().ops.len() > 3);
        assert_eq!(get(31467).unwrap().geog, 4314);
        assert!(pair(4314, 4258).is_some());
        assert_eq!(vertical(5773).unwrap().name, "EGM96 height");
        let c = compound(5498).unwrap();
        assert_eq!((c.horizontal, c.vertical), (4269, 5703));
        assert!(get(4979).unwrap().three_d && get(4979).unwrap().geog == 4326);
        assert!(get(9000).unwrap().dynamic);
        assert_eq!(grid_alternatives("us_noaa_conus.tif"), vec!["conus".to_string()]);
        assert_eq!(datum_date(6759), Some("2007-01-01"));
        assert_eq!(datum_date(6326), None);
    }

    #[test]
    fn esri_table_loads() {
        let e = esri(102003).unwrap();
        assert_eq!(e.name, "USA_Contiguous_Albers_Equal_Area_Conic");
        assert_eq!((e.datum, e.geog), (6269, 4269));
        assert!(e.proj.starts_with("+proj=aea") && !e.ops.is_empty());
        assert!(esri(4326).is_none() && get(54009).is_none());
        assert_eq!(esri_named("World Mollweide"), vec![54009]);
    }
}
