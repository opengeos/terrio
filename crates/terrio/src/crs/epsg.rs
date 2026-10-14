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

struct Table {
    crs: HashMap<u32, Entry>,
    /// Direct transformations between geographic CRSs, by (source, target) code.
    pairs: HashMap<(u32, u32), Vec<Arc<Op>>>,
}

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| {
        let mut text = String::new();
        if let Ok(mut d) = ruzstd::decoding::StreamingDecoder::new(&mut &DATA[..]) {
            let _ = d.read_to_string(&mut text);
        }
        let mut ops: HashMap<u32, Arc<Op>> = HashMap::new();
        let mut crs = HashMap::new();
        let mut pairs = HashMap::new();
        let resolve = |ids: &str, ops: &HashMap<u32, Arc<Op>>| -> Vec<Arc<Op>> {
            ids.split(',').filter_map(|i| i.parse().ok()).filter_map(|i| ops.get(&i).cloned()).collect()
        };
        for line in text.lines() {
            let mut f = line.split('\t');
            match f.next() {
                Some("O") => {
                    let (Some(id), Some(bounds), Some(accuracy), Some(flags)) = (f.next(), f.next(), f.next(), f.next()) else {
                        continue;
                    };
                    let b: Vec<f64> = bounds.split(',').filter_map(|v| v.parse().ok()).collect();
                    let (Ok(id), true, Some(steps)) = (id.parse(), b.len() == 4, parse_steps(f.next().unwrap_or(""))) else { continue };
                    let op = Op {
                        bounds: [b[0], b[1], b[2], b[3]],
                        accuracy: accuracy.parse().unwrap_or(-1.0),
                        offshore: flags.contains('o'),
                        unknown_area: flags.contains('u'),
                        ballpark: flags.contains('b'),
                        concatenated: flags.contains('c'),
                        steps,
                    };
                    ops.insert(id, Arc::new(op));
                }
                Some("C") => {
                    let fields: Vec<&str> = f.collect();
                    let [code, name, datum, geog, op_ids, proj, wkt] = fields[..] else { continue };
                    let Ok(code) = code.parse() else { continue };
                    let entry = Entry {
                        name: name.to_string(),
                        datum: datum.parse().unwrap_or(0),
                        geog: geog.parse().unwrap_or(0),
                        ops: resolve(op_ids, &ops),
                        proj: proj.to_string(),
                        wkt: wkt.to_string(),
                    };
                    crs.insert(code, entry);
                }
                Some("P") => {
                    let (Some(a), Some(b), Some(ids)) = (f.next(), f.next(), f.next()) else { continue };
                    let (Ok(a), Ok(b)) = (a.parse(), b.parse()) else { continue };
                    pairs.insert((a, b), resolve(ids, &ops));
                }
                _ => {}
            }
        }
        Table { crs, pairs }
    })
}

/// The table's entry for an EPSG code.
pub(crate) fn get(code: u32) -> Option<&'static Entry> {
    table().crs.get(&code)
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
        for (&(a, b), ops) in &t.pairs {
            add(a, b, single(ops, false));
            add(b, a, single(ops, true));
        }
        for (&code, e) in &t.crs {
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
    t.pairs.get(&(from, to)).map(|o| (o.as_slice(), false)).or_else(|| t.pairs.get(&(to, from)).map(|o| (o.as_slice(), true)))
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
