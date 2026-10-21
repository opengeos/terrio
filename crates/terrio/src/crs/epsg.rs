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
use super::route::Leg;
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex, OnceLock};

static DATA: &[u8] = include_bytes!("../../data/epsg.tsv.zst");
static ESRI_DATA: &[u8] = include_bytes!("../../data/esri.tsv.zst");

/// The names and aliases of EPSG datums, generated from PROJ by `scripts/gen_datum_names.py`:
/// `code<TAB>rank<TAB>name` lines.
static DATUM_NAMES: &[u8] = include_bytes!("../../data/datums.tsv.zst");

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
}

/// Transformations without and with grids.
type OpLists = (Vec<Arc<Op>>, Vec<Arc<Op>>);

/// The table: its text, decompressed once, and an index of its lines. Entries are parsed on
/// first use.
struct Table {
    text: String,
    crs: HashMap<u32, Lazy<Option<Entry>>>,
    ops: HashMap<u32, Lazy<Option<Arc<Op>>>>,
    /// Direct transformations between geographic CRSs, by (source, target) code.
    pairs: HashMap<(u32, u32), Lazy<OpLists>>,
    verticals: HashMap<u32, Lazy<Option<Vertical>>>,
    compounds: HashMap<u32, Lazy<Option<Compound>>>,
    /// Geoid models from geographic 3D CRSs to vertical CRSs, by vertical CRS.
    geoids: HashMap<u32, Vec<(u32, Lazy<OpLists>)>>,
    /// Old names of grids.
    alternatives: HashMap<String, Vec<String>>,
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
fn decompress(data: &[u8]) -> String {
    #[cfg(not(target_arch = "wasm32"))]
    if let Ok(bytes) = zstd::stream::decode_all(data)
        && let Ok(text) = String::from_utf8(bytes)
    {
        return text;
    }
    let mut text = String::new();
    if let Ok(mut d) = ruzstd::decoding::StreamingDecoder::new(&mut &data[..]) {
        let _ = d.read_to_string(&mut text);
    }
    text
}

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| load(DATA))
}

fn esri_table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| load(ESRI_DATA))
}

/// Decompresses a table and indexes its lines.
fn load(data: &[u8]) -> Table {
    let text = decompress(data);
    let (mut crs, mut ops, mut pairs) = (HashMap::new(), HashMap::new(), HashMap::new());
    let (mut verticals, mut compounds, mut geoids) = (HashMap::new(), HashMap::new(), HashMap::<u32, Vec<_>>::new());
    let mut alternatives = HashMap::<String, Vec<String>>::new();
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
            "A" => {
                alternatives.entry(key.to_string()).or_default().push(line[rest - start..].to_string());
            }
            _ => {}
        }
    }
    Table { text, crs, ops, pairs, verticals, compounds, geoids, alternatives }
}

impl Table {
    /// The transformation with an id, parsed from its line (`bounds`, `accuracy`, `flags`,
    /// `steps`).
    fn op(&self, id: u32) -> Option<Arc<Op>> {
        self.ops
            .get(&id)?
            .get(&self.text, |line| {
                let mut f = line.split('\t');
                let (Some(bounds), Some(accuracy), Some(flags)) = (f.next(), f.next(), f.next()) else { return None };
                let b: Vec<f64> = bounds.split(',').filter_map(|v| v.parse().ok()).collect();
                let (true, Some(steps)) = (b.len() == 4, parse_steps(f.next().unwrap_or(""))) else { return None };
                Some(Arc::new(Op {
                    bounds: [b[0], b[1], b[2], b[3]],
                    accuracy: accuracy.parse().unwrap_or(-1.0),
                    offshore: flags.contains('o'),
                    unknown_area: flags.contains('u'),
                    ballpark: flags.contains('b'),
                    concatenated: flags.contains('c'),
                    between_3d: flags.contains('h'),
                    geocentric: flags.contains('g'),
                    priority: flags.contains('p'),
                    keeps_height: flags.contains('z'),
                    steps,
                }))
            })
            .clone()
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
                Some(Entry {
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
                })
            })
            .as_ref()
    }

    fn lists(&self, ids: &str) -> OpLists {
        let (a, b) = ids.split_once('\t').unwrap_or((ids, ""));
        (self.resolve(a), self.resolve(b))
    }

    fn pair(&self, a: u32, b: u32) -> Option<&OpLists> {
        Some(self.pairs.get(&(a, b))?.get(&self.text, |ids| self.lists(ids)))
    }
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
    #[cfg(not(target_arch = "wasm32"))]
    rayon::spawn(|| {
        table();
    });
}

/// The table's entry for an EPSG code.
pub(crate) fn get(code: u32) -> Option<&'static Entry> {
    table().entry(code)
}

/// The table's entry for an ESRI code.
pub(crate) fn esri(code: u32) -> Option<&'static Entry> {
    esri_table().entry(code)
}

/// An index of a table's CRSs by name, as `key` gives it.
fn name_index(t: &Table, key: impl Fn(&str) -> String) -> HashMap<String, Vec<u32>> {
    let mut names = HashMap::<String, Vec<u32>>::new();
    for (code, l) in &t.crs {
        if let Some(n) = t.text[l.line.0 as usize..l.line.1 as usize].split('\t').next() {
            names.entry(key(n)).or_default().push(*code);
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
type DatumNames = (HashMap<String, (u8, Vec<u32>)>, HashMap<String, Vec<u32>>);

fn datum_names() -> &'static DatumNames {
    static NAMES: OnceLock<DatumNames> = OnceLock::new();
    NAMES.get_or_init(|| {
        let (mut names, mut written) = (HashMap::<String, (u8, Vec<u32>)>::new(), HashMap::<String, Vec<u32>>::new());
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
        (names, written)
    })
}

/// The EPSG datums a name stands for, as PROJ's database lists them: those it is the official
/// name (or GDAL's WKT 1 name) of, else an EPSG alias of, else a PROJ, an ESRI (`D_ETRS_1989`)
/// or a former name of, in order of code.
pub(crate) fn datums_named(name: &str) -> Vec<u32> {
    datum_names().0.get(&datum_key(name)).map(|(_, c)| c.clone()).unwrap_or_default()
}

/// The other EPSG datums whose CRSs ESRI writes with a datum name: ETRS89-NOR for
/// `D_ETRS_1989` (ETRS89-NOR [EUREF89] / NTM zone 18, EPSG:5118), S-JTSK (Ferro) for `D_S_JTSK`.
pub(crate) fn datums_written_as(name: &str) -> &'static [u32] {
    datum_names().1.get(&datum_key(name)).map_or(&[], Vec::as_slice)
}

/// The EPSG geographic 2D CRSs on a datum, without deprecated ones, in order of code.
pub(crate) fn geographic_on(datum: u32) -> &'static [u32] {
    static INDEX: OnceLock<HashMap<u32, Vec<u32>>> = OnceLock::new();
    let index = INDEX.get_or_init(|| {
        let t = table();
        let mut index = HashMap::<u32, Vec<u32>>::new();
        for (code, l) in &t.crs {
            let f: Vec<&str> = t.text[l.line.0 as usize..l.line.1 as usize].splitn(9, '\t').collect();
            // A deprecated code has its replacement's geographic CRS.
            let [_, datum_field, geog, _, _, flags, _, proj, ..] = f[..] else { continue };
            if geog.parse() == Ok(*code)
                && !flags.contains('3')
                && proj.starts_with("+proj=longlat")
                && let Ok(d) = datum_field.parse::<u32>()
            {
                index.entry(d).or_default().push(*code);
            }
        }
        index.values_mut().for_each(|c| c.sort_unstable());
        index
    });
    index.get(&datum).map_or(&[], Vec::as_slice)
}

/// The EPSG projected CRSs on a geographic CRS, without deprecated ones, in order of code, with
/// their projection method (`+proj=tmerc`).
pub(crate) fn projected_on(geog: u32) -> &'static [(u32, &'static str)] {
    static INDEX: OnceLock<HashMap<u32, Vec<(u32, &'static str)>>> = OnceLock::new();
    let index = INDEX.get_or_init(|| {
        let t = table();
        let mut index = HashMap::<u32, Vec<(u32, &'static str)>>::new();
        for (code, l) in &t.crs {
            let f: Vec<&'static str> = t.text[l.line.0 as usize..l.line.1 as usize].splitn(9, '\t').collect();
            let [_, _, geog, _, _, flags, _, proj, wkt] = f[..] else { continue };
            // A deprecated code has its replacement's WKT.
            let current = super::declared_epsg(wkt) == Some(*code);
            let method = proj.split_whitespace().next().unwrap_or_default();
            if current
                && !flags.contains('3')
                && method != "+proj=longlat"
                && let Ok(g) = geog.parse::<u32>()
            {
                index.entry(g).or_default().push((*code, method));
            }
        }
        index.values_mut().for_each(|c| c.sort_unstable());
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

/// The single transformations from each geographic CRS, by the CRS they lead to.
pub(crate) type Adjacency = HashMap<u32, Vec<(u32, Vec<Leg>)>>;

/// For each geographic CRS, the geographic CRSs it has single transformations to (WGS 84 among
/// them), with those transformations: what PROJ chains through intermediate datums. Rebuilt
/// when the grids that can be found change.
pub(crate) fn adjacency() -> Arc<Adjacency> {
    static ADJ: Mutex<Option<(u64, Arc<Adjacency>)>> = Mutex::new(None);
    let generation = super::grid::generation();
    let mut cached = ADJ.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((g, adj)) = cached.as_ref()
        && *g == generation
    {
        return adj.clone();
    }
    let adj = Arc::new({
        let t = table();
        let single = |ops: &[Arc<Op>], reverse: bool| -> Vec<Leg> {
            ops.iter().filter(|o| !o.ballpark && !o.concatenated).map(|op| Leg { op: op.clone(), reverse }).collect()
        };
        let mut adj = Adjacency::new();
        let mut add = |a: u32, b: u32, legs: Vec<Leg>| {
            if !legs.is_empty() {
                adj.entry(a).or_default().push((b, legs));
            }
        };
        // In code order, so that the result does not depend on the hash maps' order.
        let mut pair_keys: Vec<(u32, u32)> = t.pairs.keys().copied().collect();
        pair_keys.sort_unstable();
        for (a, b) in pair_keys {
            let Some((ops, grid_ops)) = t.pair(a, b) else { continue };
            let ops = usable(ops, grid_ops);
            add(a, b, single(&ops, false));
            add(b, a, single(&ops, true));
        }
        let mut codes: Vec<u32> = t.crs.keys().copied().collect();
        codes.sort_unstable();
        for code in codes {
            let Some(e) = t.entry(code) else { continue };
            if e.geog == code && code != 4326 && e.datum != 6326 && !e.three_d {
                let ops = usable(&e.ops, &e.grid_ops);
                add(code, 4326, single(&ops, false));
                add(4326, code, single(&ops, true));
            }
        }
        adj
    });
    *cached = Some((generation, adj.clone()));
    adj
}

/// The direct transformations from one geographic CRS to another (those GDAL uses given the
/// grids available), and whether they run in reverse (the table holds each pair once).
pub(crate) fn pair(from: u32, to: u32) -> Option<(Vec<Arc<Op>>, bool)> {
    let t = table();
    let found = t.pair(from, to).map(|o| (o, false)).or_else(|| t.pair(to, from).map(|o| (o, true)))?;
    Some((usable(&found.0.0, &found.0.1), found.1))
}

/// An EPSG vertical CRS.
pub(crate) fn vertical(code: u32) -> Option<&'static Vertical> {
    let t = table();
    t.verticals
        .get(&code)?
        .get(&t.text, |line| {
            let f: Vec<&str> = line.split('\t').collect();
            let [name, _datum, unit, direction, wkt] = f[..] else { return None };
            Some(Vertical {
                name: name.to_string(),
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
            let [name, h, v, wkt] = f[..] else { return None };
            Some(Compound { name: name.to_string(), horizontal: h.parse().ok()?, vertical: v.parse().ok()?, wkt: wkt.to_string() })
        })
        .as_ref()
}

/// The geoid models (transformations from geographic 3D CRSs) of a vertical CRS, with the
/// code of their geographic 3D CRS, as GDAL uses them given the grids available.
pub(crate) fn geoid_models(vertical: u32) -> Vec<(u32, Vec<Arc<Op>>)> {
    let t = table();
    let Some(list) = t.geoids.get(&vertical) else { return Vec::new() };
    list.iter()
        .map(|(g, lazy)| {
            let (ops, grid_ops) = lazy.get(&t.text, |ids| t.lists(ids));
            (*g, usable(ops, grid_ops))
        })
        .filter(|(_, ops)| !ops.is_empty())
        .collect()
}

/// The names a PROJ-data grid had before PROJ 7.
pub(crate) fn grid_alternatives(name: &str) -> Vec<String> {
    table().alternatives.get(name).cloned().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

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
