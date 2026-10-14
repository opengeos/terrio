//! The EPSG table terrio carries: official names, WKT, coordinate conversions as PROJ strings
//! and the datum transformations to WGS 84 GDAL uses, generated from GDAL and PROJ by
//! `scripts/gen_epsg_table.py`.
//!
//! Derived from the EPSG Geodetic Parameter Dataset (<https://epsg.org>), whose terms of use
//! permit redistribution with attribution.

use super::datum::{Op, parse_steps};
use super::route::Leg;
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, OnceLock};

static DATA: &[u8] = include_bytes!("../../data/epsg.tsv.zst");

/// An EPSG CRS of the table.
#[derive(Debug)]
pub(crate) struct Entry {
    /// The official name.
    pub name: String,
    /// The EPSG code of its datum, 0 when unknown.
    pub datum: u32,
    /// The EPSG code of its geographic CRS, 0 when unknown.
    pub geog: u32,
    /// Its datum transformations to WGS 84, in the order GDAL tries them.
    pub ops: Vec<Arc<Op>>,
    /// The PROJ string of its coordinate conversion (without datum shift).
    pub proj: String,
    /// The WKT GDAL writes for it.
    pub wkt: String,
}

/// The table: its text, decompressed once, and an index of its lines. Entries are parsed on
/// first use.
struct Table {
    text: String,
    crs: HashMap<u32, Lazy<Option<Entry>>>,
    ops: HashMap<u32, Lazy<Option<Arc<Op>>>>,
    /// Direct transformations between geographic CRSs, by (source, target) code.
    pairs: HashMap<(u32, u32), Lazy<Vec<Arc<Op>>>>,
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

/// The table's decompressed text.
fn decompress() -> String {
    #[cfg(not(target_arch = "wasm32"))]
    if let Ok(bytes) = zstd::stream::decode_all(DATA)
        && let Ok(text) = String::from_utf8(bytes)
    {
        return text;
    }
    let mut text = String::new();
    if let Ok(mut d) = ruzstd::decoding::StreamingDecoder::new(&mut &DATA[..]) {
        let _ = d.read_to_string(&mut text);
    }
    text
}

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| {
        let text = decompress();
        let (mut crs, mut ops, mut pairs) = (HashMap::new(), HashMap::new(), HashMap::new());
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
                    let mut g = line[rest - start..].splitn(2, '\t');
                    if let (Ok(a), Some(Ok(b))) = (key.parse::<u32>(), g.next().map(str::parse::<u32>)) {
                        let ids = (rest + line[rest - start..].find('\t').map_or(end - rest, |i| i + 1)).min(end);
                        pairs.insert((a, b), Lazy::new(ids, end));
                    }
                }
                _ => {}
            }
        }
        Table { text, crs, ops, pairs }
    })
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
                let [name, datum, geog, op_ids, proj, wkt] = fields[..] else { return None };
                Some(Entry {
                    name: name.to_string(),
                    datum: datum.parse().unwrap_or(0),
                    geog: geog.parse().unwrap_or(0),
                    ops: self.resolve(op_ids),
                    proj: proj.to_string(),
                    wkt: wkt.to_string(),
                })
            })
            .as_ref()
    }

    fn pair(&self, a: u32, b: u32) -> Option<&[Arc<Op>]> {
        Some(self.pairs.get(&(a, b))?.get(&self.text, |ids| self.resolve(ids)).as_slice())
    }
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

/// The single transformations from each geographic CRS, by the CRS they lead to.
type Adjacency = HashMap<u32, Vec<(u32, Vec<Leg>)>>;

/// The geographic CRSs a geographic CRS has single transformations to (WGS 84 among them), with
/// those transformations: what PROJ chains through intermediate datums.
pub(crate) fn neighbours(geog: u32) -> Option<&'static Vec<(u32, Vec<Leg>)>> {
    static ADJ: OnceLock<Adjacency> = OnceLock::new();
    ADJ.get_or_init(|| {
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
            let ops = t.pair(a, b).unwrap_or_default();
            add(a, b, single(ops, false));
            add(b, a, single(ops, true));
        }
        let mut codes: Vec<u32> = t.crs.keys().copied().collect();
        codes.sort_unstable();
        for code in codes {
            let Some(e) = t.entry(code) else { continue };
            if e.geog == code && code != 4326 && e.datum != 6326 {
                add(code, 4326, single(&e.ops, false));
                add(4326, code, single(&e.ops, true));
            }
        }
        adj
    })
    .get(&geog)
}

/// The direct transformations from one geographic CRS to another, and whether they run in
/// reverse (the table holds each pair once).
pub(crate) fn pair(from: u32, to: u32) -> Option<(&'static [Arc<Op>], bool)> {
    let t = table();
    t.pair(from, to).map(|o| (o, false)).or_else(|| t.pair(to, from).map(|o| (o, true)))
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
    }
}
