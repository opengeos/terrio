//! Disk-backed stable STR packing for the public multi-layer writer. The packed nodes and
//! shadow-table rows are identical to `Rtree`, without retaining one entry per feature.

use super::{LayerExtent, RTREE_MAX_CELLS, RTREE_NODE_SIZE, RtreeEntry, Value, f32_down, f32_up, rtree_node};
use crate::drivers::vector::{Spill, spill};
use crate::{Error, Result};
use std::cmp::Ordering;
use std::collections::VecDeque;
use std::io::{BufReader, BufWriter, Read, SeekFrom, Write};

// At most 2 MiB of records per sort run, plus stable-sort scratch and I/O buffers.
const RUN: usize = 65536;
const RECORD_BYTES: u64 = 32;
type Row = (i64, Vec<Value>);

#[derive(Clone, Copy)]
struct Record {
    id: i64,
    bounds: [f32; 4],
    link: i64,
}

impl Record {
    fn read(r: &mut impl Read) -> Result<Self> {
        let mut b = [0; 32];
        r.read_exact(&mut b)?;
        let bounds = std::array::from_fn(|i| f32::from_le_bytes(b[8 + 4 * i..12 + 4 * i].try_into().unwrap()));
        Ok(Self { id: i64::from_le_bytes(b[..8].try_into().unwrap()), bounds, link: i64::from_le_bytes(b[24..].try_into().unwrap()) })
    }

    fn write(self, w: &mut impl Write) -> Result<()> {
        w.write_all(&self.id.to_le_bytes())?;
        for v in self.bounds {
            w.write_all(&v.to_le_bytes())?;
        }
        w.write_all(&self.link.to_le_bytes())?;
        Ok(())
    }

    fn entry(self) -> RtreeEntry {
        (self.id, self.bounds)
    }
}

#[derive(Clone, Copy)]
enum Key {
    X,
    Y,
    Id,
}

impl Key {
    fn compare(self, a: &Record, b: &Record) -> Ordering {
        let center = |r: &Record, i| f64::from(r.bounds[i]) + f64::from(r.bounds[i + 1]);
        match self {
            Self::X => center(a, 0).total_cmp(&center(b, 0)),
            Self::Y => center(a, 2).total_cmp(&center(b, 2)),
            Self::Id => a.id.cmp(&b.id),
        }
    }
}

fn finish(mut out: BufWriter<Box<dyn Spill>>) -> Result<Box<dyn Spill>> {
    out.flush()?;
    out.into_inner().map_err(|e| Error::Io(e.into_error()))
}

/// A bounded read buffer for one run in a file shared by two merge cursors.
struct RunReader {
    next: usize,
    end: usize,
    values: VecDeque<Record>,
}

impl RunReader {
    fn new(start: usize, end: usize) -> Self {
        Self { next: start, end, values: VecDeque::new() }
    }

    fn pop(&mut self, mut input: &mut dyn Spill) -> Result<Option<Record>> {
        if self.values.is_empty() && self.next < self.end {
            input.seek(SeekFrom::Start(self.next as u64 * RECORD_BYTES))?;
            let end = (self.next + 512).min(self.end);
            let mut reader = BufReader::new(&mut input);
            for _ in self.next..end {
                self.values.push_back(Record::read(&mut reader)?);
            }
            self.next = end;
        }
        Ok(self.values.pop_front())
    }
}

/// Stable external merge sort. Runs share a file and are located arithmetically, so neither
/// the number of runs nor the feature count determines the size of any in-memory collection.
fn sort(input: &mut impl Read, count: usize, key: Key, run: usize) -> Result<Box<dyn Spill>> {
    let mut out = BufWriter::new(spill()?);
    let mut values = Vec::with_capacity(count.min(run));
    for start in (0..count).step_by(run) {
        values.clear();
        for _ in start..(start + run).min(count) {
            values.push(Record::read(input)?);
        }
        values.sort_by(|a, b| key.compare(a, b));
        for value in &values {
            value.write(&mut out)?;
        }
    }
    drop(values);
    let mut file = finish(out)?;
    let mut size = run;
    while size < count {
        let mut out = BufWriter::new(spill()?);
        for start in (0..count).step_by(size.saturating_mul(2)) {
            let mid = start.saturating_add(size).min(count);
            let end = mid.saturating_add(size).min(count);
            let (mut left, mut right) = (RunReader::new(start, mid), RunReader::new(mid, end));
            let (mut a, mut b) = (left.pop(&mut *file)?, right.pop(&mut *file)?);
            while a.is_some() || b.is_some() {
                // Left wins ties, preserving the stable in-memory sort's ordering.
                if b.is_none() || a.is_some_and(|a| key.compare(&a, &b.unwrap()) != Ordering::Greater) {
                    a.unwrap().write(&mut out)?;
                    a = left.pop(&mut *file)?;
                } else {
                    b.unwrap().write(&mut out)?;
                    b = right.pop(&mut *file)?;
                }
            }
        }
        file = finish(out)?;
        size = size.saturating_mul(2);
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(file)
}

/// The streaming writer's bounds and index entries. Native spills are temporary disk files;
/// WebAssembly uses the existing memory-backed spill implementation.
pub(crate) struct Extent {
    bounds: Option<[f64; 4]>,
    rows: BufWriter<Box<dyn Spill>>,
    count: usize,
}

impl Extent {
    pub(crate) fn new() -> Result<Self> {
        Ok(Self { bounds: None, rows: BufWriter::new(spill()?), count: 0 })
    }

    pub(crate) fn add(&mut self, id: i64, bbox: Option<wbvector::BBox>) -> Result<()> {
        if let Some(b) = bbox {
            self.bounds = Some(match self.bounds {
                None => [b.min_x, b.min_y, b.max_x, b.max_y],
                Some(a) => [a[0].min(b.min_x), a[1].min(b.min_y), a[2].max(b.max_x), a[3].max(b.max_y)],
            });
            let bounds = [f32_down(b.min_x), f32_up(b.max_x), f32_down(b.min_y), f32_up(b.max_y)];
            Record { id, bounds, link: 0 }.write(&mut self.rows)?;
            self.count += 1;
        }
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<LayerExtent> {
        let tree = Tree::new(finish(self.rows)?, self.count, RUN)?;
        Ok(LayerExtent { bounds: self.bounds, entries: Vec::new(), spilled: Some(tree) })
    }
}

struct Level {
    file: Box<dyn Spill>,
    count: usize,
}

pub(super) struct Tree {
    levels: Vec<Level>,
    run: usize,
}

impl Tree {
    fn new(mut file: Box<dyn Spill>, mut count: usize, run: usize) -> Result<Self> {
        let mut levels = Vec::new();
        while count > 0 {
            file.seek(SeekFrom::Start(0))?;
            let mut x = sort(&mut BufReader::new(file), count, Key::X, run)?;
            let mut input = BufReader::new(&mut x);
            let nodes = count.div_ceil(RTREE_MAX_CELLS);
            let per_slice = (nodes as f64).sqrt().ceil() as usize * RTREE_MAX_CELLS;
            let mut packed = BufWriter::new(spill()?);
            for start in (0..count).step_by(per_slice) {
                let mut y = sort(&mut input, per_slice.min(count - start), Key::Y, run)?;
                std::io::copy(&mut y, &mut packed)?;
            }
            let mut packed = finish(packed)?;
            packed.seek(SeekFrom::Start(0))?;
            let mut up = BufWriter::new(spill()?);
            let mut reader = BufReader::new(&mut packed);
            for node in 0..nodes {
                let mut entries = Vec::with_capacity(RTREE_MAX_CELLS);
                for _ in 0..RTREE_MAX_CELLS.min(count - node * RTREE_MAX_CELLS) {
                    entries.push(Record::read(&mut reader)?.entry());
                }
                Record { id: node as i64, bounds: super::union(&entries), link: 0 }.write(&mut up)?;
            }
            drop(reader);
            levels.push(Level { file: packed, count });
            if nodes == 1 {
                break;
            }
            file = finish(up)?;
            count = nodes;
        }
        Ok(Self { levels, run })
    }

    fn bases(&self) -> Vec<i64> {
        let mut bases = vec![0; self.levels.len()];
        let mut next = 1;
        for (i, level) in self.levels.iter().enumerate().rev() {
            bases[i] = next;
            next += level.count.div_ceil(RTREE_MAX_CELLS) as i64;
        }
        bases
    }

    fn nodes(&mut self) -> Result<Box<dyn Iterator<Item = Result<Row>> + '_>> {
        if self.levels.is_empty() {
            return Ok(Box::new(std::iter::once(Ok((1, vec![Value::Null, Value::Blob(vec![0; RTREE_NODE_SIZE])])))));
        }
        let bases = self.bases();
        let depth = self.levels.len() - 1;
        for level in &mut self.levels {
            level.file.seek(SeekFrom::Start(0))?;
        }
        Ok(Box::new(self.levels.iter_mut().enumerate().rev().flat_map(move |(i, level)| {
            let (base, child) = (bases[i], if i == 0 { 0 } else { bases[i - 1] });
            let mut input = BufReader::new(&mut level.file);
            let count = level.count;
            (0..count.div_ceil(RTREE_MAX_CELLS)).map(move |node| {
                let mut entries = Vec::with_capacity(RTREE_MAX_CELLS);
                for _ in 0..RTREE_MAX_CELLS.min(count - node * RTREE_MAX_CELLS) {
                    entries.push(Record::read(&mut input)?.entry());
                }
                let d = if i == depth { depth as u16 } else { 0 };
                Ok((base + node as i64, vec![Value::Null, Value::Blob(rtree_node(d, &entries, child))]))
            })
        })))
    }

    /// Sort the feature-to-leaf or child-to-parent mapping by rowid, externally as well.
    fn links(&mut self, leaves: bool) -> Result<(Box<dyn Spill>, usize)> {
        let bases = self.bases();
        let mut out = BufWriter::new(spill()?);
        let mut count = 0;
        for (i, level) in self.levels.iter_mut().enumerate() {
            if (leaves && i != 0) || (!leaves && i == 0) {
                continue;
            }
            level.file.seek(SeekFrom::Start(0))?;
            let mut input = BufReader::new(&mut level.file);
            for index in 0..level.count {
                let record = Record::read(&mut input)?;
                let id = record.id + if leaves { 0 } else { bases[i - 1] };
                let link = bases[i] + (index / RTREE_MAX_CELLS) as i64;
                Record { id, bounds: [0.0; 4], link }.write(&mut out)?;
                count += 1;
            }
        }
        let mut file = finish(out)?;
        file.seek(SeekFrom::Start(0))?;
        Ok((sort(&mut BufReader::new(file), count, Key::Id, self.run)?, count))
    }

    /// Adapt fallible disk iteration to the existing table callback, checking any read error
    /// after the callback rather than silently writing a truncated index.
    pub(super) fn write(&mut self, tables: [(String, String); 3], write: &mut super::WriteTable) -> Result<Vec<super::sqlite::BuiltTable>> {
        let mut built = Vec::new();
        for (i, (name, sql)) in tables.into_iter().enumerate() {
            let mut rows: Box<dyn Iterator<Item = Result<Row>> + '_> = if i == 1 {
                self.nodes()?
            } else {
                let (file, count) = self.links(i == 0)?;
                let mut input = BufReader::new(file);
                Box::new((0..count).map(move |_| {
                    let record = Record::read(&mut input)?;
                    Ok((record.id, vec![Value::Null, Value::Int(record.link)]))
                }))
            };
            let mut error = None;
            let mut checked = rows.by_ref().map_while(|row| match row {
                Ok(row) => Some(row),
                Err(e) => {
                    error = Some(e);
                    None
                }
            });
            let table = super::sqlite::Table { name, sql, rows: Vec::new(), autoindexes: Vec::new() };
            let result = write(table, &mut checked)?;
            if let Some(error) = error {
                return Err(error);
            }
            built.push(result);
        }
        Ok(built)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same SQLite R-tree shadow rows as the existing packed writer (GDAL 3.13.3 reads both).
    /// Seven-record runs force multiple merge passes, ties and multi-level parent mappings.
    #[test]
    fn external_packing_preserves_every_shadow_row() {
        for n in [0, 1, 51, 52, 2602, 10007, 140000] {
            let entries: Vec<_> = (0..n)
                .map(|i| {
                    let x = (i * 17 % 37) as f32;
                    let y = (i * 11 % 19) as f32;
                    (i as i64 + 1, [x, x + 0.25, y, y + 0.5])
                })
                .collect();
            let expected = super::super::Rtree::new(entries.clone());
            let mut file = BufWriter::new(spill().unwrap());
            for (id, bounds) in entries {
                Record { id, bounds, link: 0 }.write(&mut file).unwrap();
            }
            let mut actual = Tree::new(finish(file).unwrap(), n, if n > 10007 { 97 } else { 7 }).unwrap();
            let mut rows = Vec::new();
            let mut write = |table: super::super::sqlite::Table, r: &mut super::super::sqlite::Rows| {
                rows.push(r.collect::<Vec<_>>());
                Ok(super::super::sqlite::BuiltTable { name: table.name, sql: table.sql, root: 0 })
            };
            actual.write(std::array::from_fn(|i| (format!("table{i}"), String::new())), &mut write).unwrap();
            assert_eq!(rows[0], expected.rowid_rows().collect::<Vec<_>>(), "{n}: rowid");
            assert_eq!(rows[1], expected.node_rows().collect::<Vec<_>>(), "{n}: node");
            assert_eq!(rows[2], expected.parent_rows().collect::<Vec<_>>(), "{n}: parent");
        }
    }

    #[test]
    fn truncated_index_spill_is_an_error() {
        let mut tree = Tree::new(spill().unwrap(), 1, 7);
        assert!(tree.is_err());
        let mut file = BufWriter::new(spill().unwrap());
        Record { id: 1, bounds: [0.0; 4], link: 0 }.write(&mut file).unwrap();
        tree = Tree::new(finish(file).unwrap(), 1, 7);
        let mut tree = tree.unwrap();
        // Fail reads only after the rowid table has been consumed, so the error occurs
        // inside the node iterator passed to the infallible SQLite table callback.
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        struct Failing {
            file: Box<dyn Spill>,
            fail: Arc<AtomicBool>,
        }
        impl Read for Failing {
            fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
                if self.fail.load(Ordering::Relaxed) {
                    Err(std::io::Error::other("injected index read failure"))
                } else {
                    self.file.read(b)
                }
            }
        }
        impl Write for Failing {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.file.write(b)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.file.flush()
            }
        }
        impl std::io::Seek for Failing {
            fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
                self.file.seek(from)
            }
        }
        let fail = Arc::new(AtomicBool::new(false));
        let file = std::mem::replace(&mut tree.levels[0].file, spill().unwrap());
        tree.levels[0].file = Box::new(Failing { file, fail: fail.clone() });
        let mut write = |table: super::super::sqlite::Table, r: &mut super::super::sqlite::Rows| {
            r.for_each(drop);
            fail.store(true, Ordering::Relaxed);
            Ok(super::super::sqlite::BuiltTable { name: table.name, sql: table.sql, root: 0 })
        };
        assert!(tree.write(std::array::from_fn(|i| (format!("table{i}"), String::new())), &mut write).is_err());
    }
}
