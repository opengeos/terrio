//! A writer of SQLite database files, enough for GeoPackages: tables (rowid b-trees, any size,
//! with overflow pages) and small indexes (the automatic indexes of UNIQUE and PRIMARY KEY
//! constraints), written in one go.
//!
//! The format is described at <https://www.sqlite.org/fileformat2.html>.

use crate::{Error, Result};

const PAGE_SIZE: usize = 4096;

/// A value stored in a record.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL NULL.
    Null,
    /// A 64-bit integer.
    Int(i64),
    /// A double.
    Real(f64),
    /// UTF-8 text.
    Text(String),
    /// Bytes.
    Blob(Vec<u8>),
}

/// A table: its `CREATE TABLE` statement and rows (rowid and values).
pub struct Table {
    /// The table name.
    pub name: String,
    /// The `CREATE TABLE` statement.
    pub sql: String,
    /// The rows, by increasing rowid. A rowid alias column (`INTEGER PRIMARY KEY`) holds NULL.
    pub rows: Vec<(i64, Vec<Value>)>,
    /// The automatic indexes of its constraints, in declaration order: the indexed columns.
    pub autoindexes: Vec<Vec<usize>>,
}

fn varint(out: &mut Vec<u8>, v: u64) {
    if v <= 0x7f {
        out.push(v as u8);
        return;
    }
    if v > 0x00ff_ffff_ffff_ffff {
        let mut buf = [0u8; 9];
        buf[8] = v as u8;
        let mut x = v >> 8;
        for i in (0..8).rev() {
            buf[i] = (x as u8 & 0x7f) | 0x80;
            x >>= 7;
        }
        out.extend_from_slice(&buf);
        return;
    }
    let mut groups = Vec::new();
    let mut x = v;
    while x > 0 {
        groups.push((x & 0x7f) as u8);
        x >>= 7;
    }
    for (i, g) in groups.iter().rev().enumerate() {
        out.push(if i + 1 < groups.len() { g | 0x80 } else { *g });
    }
}

fn varint_len(v: u64) -> usize {
    let mut b = Vec::new();
    varint(&mut b, v);
    b.len()
}

/// Encodes a record: the header of serial types, then the values.
pub fn record(values: &[Value]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut body = Vec::new();
    for v in values {
        match v {
            Value::Null => varint(&mut types, 0),
            Value::Int(0) => varint(&mut types, 8),
            Value::Int(1) => varint(&mut types, 9),
            Value::Int(i) => {
                let (t, n) = match *i {
                    -128..=127 => (1, 1),
                    -32768..=32767 => (2, 2),
                    -8_388_608..=8_388_607 => (3, 3),
                    -2_147_483_648..=2_147_483_647 => (4, 4),
                    -140_737_488_355_328..=140_737_488_355_327 => (5, 6),
                    _ => (6, 8),
                };
                varint(&mut types, t);
                body.extend_from_slice(&i.to_be_bytes()[8 - n..]);
            }
            Value::Real(f) => {
                varint(&mut types, 7);
                body.extend_from_slice(&f.to_be_bytes());
            }
            Value::Text(s) => {
                varint(&mut types, 13 + 2 * s.len() as u64);
                body.extend_from_slice(s.as_bytes());
            }
            Value::Blob(b) => {
                varint(&mut types, 12 + 2 * b.len() as u64);
                body.extend_from_slice(b);
            }
        }
    }
    // The header size counts itself.
    let mut size = types.len() + 1;
    while varint_len(size as u64) + types.len() != size {
        size = varint_len(size as u64) + types.len();
    }
    let mut out = Vec::with_capacity(size + body.len());
    varint(&mut out, size as u64);
    out.extend_from_slice(&types);
    out.extend_from_slice(&body);
    out
}

struct Pager {
    pages: Vec<Vec<u8>>,
}

impl Pager {
    /// Allocates a page and returns its number (1-based).
    fn alloc(&mut self) -> u32 {
        self.pages.push(vec![0; PAGE_SIZE]);
        self.pages.len() as u32
    }

    fn page(&mut self, n: u32) -> &mut Vec<u8> {
        &mut self.pages[n as usize - 1]
    }

    /// Stores a payload: the local part, and the rest in a chain of overflow pages.
    fn payload(&mut self, payload: &[u8], max_local: usize, min_local: usize) -> (Vec<u8>, Option<u32>) {
        if payload.len() <= max_local {
            return (payload.to_vec(), None);
        }
        let usable = PAGE_SIZE;
        let k = min_local + (payload.len() - min_local) % (usable - 4);
        let local = if k <= max_local { k } else { min_local };
        let mut rest = &payload[local..];
        let first = self.alloc();
        let mut page = first;
        loop {
            let n = rest.len().min(usable - 4);
            let next = if rest.len() > n { self.alloc() } else { 0 };
            let p = self.page(page);
            p[..4].copy_from_slice(&next.to_be_bytes());
            p[4..4 + n].copy_from_slice(&rest[..n]);
            rest = &rest[n..];
            if next == 0 {
                break;
            }
            page = next;
        }
        (payload[..local].to_vec(), Some(first))
    }

    /// Writes a b-tree page: header, cell pointers and cells (packed at the end of the page).
    fn write_node(&mut self, n: u32, flag: u8, cells: &[Vec<u8>], right: Option<u32>) {
        let offset = if n == 1 { 100 } else { 0 };
        let p = self.page(n);
        p[offset] = flag;
        p[offset + 3..offset + 5].copy_from_slice(&(cells.len() as u16).to_be_bytes());
        let header = if right.is_some() { 12 } else { 8 };
        if let Some(r) = right {
            p[offset + 8..offset + 12].copy_from_slice(&r.to_be_bytes());
        }
        let mut end = PAGE_SIZE;
        for (i, c) in cells.iter().enumerate() {
            end -= c.len();
            p[end..end + c.len()].copy_from_slice(c);
            let ptr = offset + header + 2 * i;
            p[ptr..ptr + 2].copy_from_slice(&(end as u16).to_be_bytes());
        }
        let start = if end == PAGE_SIZE { 0 } else { end };
        p[offset + 5..offset + 7].copy_from_slice(&(start as u16).to_be_bytes());
    }
}

/// Splits cells into nodes that fit a page.
fn pack(cells: Vec<Vec<u8>>, header: usize, room: usize) -> Vec<Vec<Vec<u8>>> {
    let mut nodes = vec![Vec::new()];
    let mut used = header;
    for c in cells {
        if used + c.len() + 2 > room && !nodes.last().is_none_or(Vec::is_empty) {
            nodes.push(Vec::new());
            used = header;
        }
        used += c.len() + 2;
        if let Some(n) = nodes.last_mut() {
            n.push(c);
        }
    }
    nodes
}

/// Builds a table b-tree and returns its root page. With `root`, the root is that page.
fn build_table(pager: &mut Pager, rows: &[(i64, Vec<u8>)], root: Option<u32>) -> u32 {
    let max_local = PAGE_SIZE - 35;
    let min_local = (PAGE_SIZE - 12) * 32 / 255 - 23;
    let mut cells = Vec::with_capacity(rows.len());
    let mut keys = Vec::with_capacity(rows.len());
    for (rowid, rec) in rows {
        let (local, overflow) = pager.payload(rec, max_local, min_local);
        let mut c = Vec::with_capacity(local.len() + 18);
        varint(&mut c, rec.len() as u64);
        varint(&mut c, *rowid as u64);
        c.extend_from_slice(&local);
        if let Some(o) = overflow {
            c.extend_from_slice(&o.to_be_bytes());
        }
        cells.push(c);
        keys.push(*rowid);
    }
    let root_room = if root == Some(1) { PAGE_SIZE - 100 } else { PAGE_SIZE };
    // Leaves sized for the root's room, so that a single leaf can be the root.
    // (A root with no cell but its right-most pointer is valid, as SQLite's balance_deeper makes.)
    let leaves = pack(cells, 8, root_room);
    if leaves.len() == 1 && leaves[0].iter().map(|c| c.len() + 2).sum::<usize>() + 8 <= root_room {
        let n = root.unwrap_or_else(|| pager.alloc());
        pager.write_node(n, 0x0D, &leaves[0], None);
        return n;
    }
    // Leaves, then interior levels up to the root. Each child is known by its largest key.
    let mut level: Vec<(u32, i64)> = Vec::new();
    let mut k = 0;
    for leaf in leaves {
        let n = pager.alloc();
        k += leaf.len();
        pager.write_node(n, 0x0D, &leaf, None);
        level.push((n, keys[k - 1]));
    }
    loop {
        // Children grouped into nodes; the last child of each node is its right-most pointer.
        let mut groups: Vec<Vec<(u32, i64)>> = vec![Vec::new()];
        let mut used = 12;
        for &(child, key) in &level {
            let len = 4 + varint_len(key as u64) + 2;
            if groups.last().is_some_and(|g| !g.is_empty()) && used + len > PAGE_SIZE - 100 {
                groups.push(Vec::new());
                used = 12;
            }
            used += len;
            if let Some(g) = groups.last_mut() {
                g.push((child, key));
            }
        }
        // A node needs a cell besides its right-most pointer.
        let n = groups.len();
        if n > 1
            && groups[n - 1].len() == 1
            && let Some(moved) = groups[n - 2].pop()
        {
            groups[n - 1].insert(0, moved);
        }
        let node = |pager: &mut Pager, g: &[(u32, i64)], page: Option<u32>| -> (u32, i64) {
            let cells: Vec<Vec<u8>> = g[..g.len() - 1]
                .iter()
                .map(|&(child, key)| {
                    let mut c = child.to_be_bytes().to_vec();
                    varint(&mut c, key as u64);
                    c
                })
                .collect();
            let (right, key) = g[g.len() - 1];
            let n = page.unwrap_or_else(|| pager.alloc());
            pager.write_node(n, 0x05, &cells, Some(right));
            (n, key)
        };
        if groups.len() == 1 {
            return node(pager, &groups[0], root).0;
        }
        level = groups.iter().map(|g| node(pager, g, None)).collect();
    }
}

/// Compares index keys as SQLite does with the BINARY collation: NULL < numbers < text < blobs.
fn compare(a: &[Value], b: &[Value]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let rank = |v: &Value| match v {
        Value::Null => 0,
        Value::Int(_) | Value::Real(_) => 1,
        Value::Text(_) => 2,
        Value::Blob(_) => 3,
    };
    for (x, y) in a.iter().zip(b) {
        let o = match (x, y) {
            (Value::Int(p), Value::Int(q)) => p.cmp(q),
            (Value::Text(p), Value::Text(q)) => p.as_bytes().cmp(q.as_bytes()),
            (Value::Blob(p), Value::Blob(q)) => p.cmp(q),
            (Value::Real(p), Value::Real(q)) => p.total_cmp(q),
            _ => rank(x).cmp(&rank(y)),
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

/// Builds a one-page index b-tree.
fn build_index(pager: &mut Pager, table: &Table, columns: &[usize]) -> Result<u32> {
    let mut keys: Vec<Vec<Value>> = table
        .rows
        .iter()
        .map(|(rowid, values)| {
            let mut k: Vec<Value> = columns.iter().map(|&c| values.get(c).cloned().unwrap_or(Value::Null)).collect();
            k.push(Value::Int(*rowid));
            k
        })
        .collect();
    keys.sort_by(|a, b| compare(a, b));
    let cells: Vec<Vec<u8>> = keys
        .iter()
        .map(|k| {
            let rec = record(k);
            let mut c = Vec::new();
            varint(&mut c, rec.len() as u64);
            c.extend_from_slice(&rec);
            c
        })
        .collect();
    if cells.iter().map(|c| c.len() + 2).sum::<usize>() + 8 > PAGE_SIZE {
        return Err(Error::Unsupported(format!("too many rows in '{}' for its index", table.name)));
    }
    let n = pager.alloc();
    pager.write_node(n, 0x0A, &cells, None);
    Ok(n)
}

/// Writes a database file holding the tables, with `user_version` and `application_id` set.
pub fn write_database(tables: &[Table], user_version: u32, application_id: u32) -> Result<Vec<u8>> {
    let mut pager = Pager { pages: Vec::new() };
    pager.alloc(); // page 1: the schema table's root
    let mut schema_rows = Vec::new();
    for t in tables {
        let rows: Vec<(i64, Vec<u8>)> = t.rows.iter().map(|(id, v)| (*id, record(v))).collect();
        let root = build_table(&mut pager, &rows, None);
        schema_rows.push(vec![
            Value::Text("table".into()),
            Value::Text(t.name.clone()),
            Value::Text(t.name.clone()),
            Value::Int(root as i64),
            Value::Text(t.sql.clone()),
        ]);
        for (i, cols) in t.autoindexes.iter().enumerate() {
            let root = build_index(&mut pager, t, cols)?;
            schema_rows.push(vec![
                Value::Text("index".into()),
                Value::Text(format!("sqlite_autoindex_{}_{}", t.name, i + 1)),
                Value::Text(t.name.clone()),
                Value::Int(root as i64),
                Value::Null,
            ]);
        }
    }
    let schema: Vec<(i64, Vec<u8>)> = schema_rows.iter().enumerate().map(|(i, r)| (i as i64 + 1, record(r))).collect();
    build_table(&mut pager, &schema, Some(1));

    let count = pager.pages.len() as u32;
    let h = pager.page(1);
    h[..16].copy_from_slice(b"SQLite format 3\0");
    h[16..18].copy_from_slice(&(PAGE_SIZE as u16).to_be_bytes());
    h[18] = 1; // write version: legacy
    h[19] = 1; // read version
    h[21] = 64; // maximum embedded payload fraction
    h[22] = 32; // minimum embedded payload fraction
    h[23] = 32; // leaf payload fraction
    h[24..28].copy_from_slice(&1u32.to_be_bytes()); // file change counter
    h[28..32].copy_from_slice(&count.to_be_bytes());
    h[40..44].copy_from_slice(&1u32.to_be_bytes()); // schema cookie
    h[44..48].copy_from_slice(&4u32.to_be_bytes()); // schema format
    h[56..60].copy_from_slice(&1u32.to_be_bytes()); // UTF-8
    h[60..64].copy_from_slice(&user_version.to_be_bytes());
    h[68..72].copy_from_slice(&application_id.to_be_bytes());
    h[92..96].copy_from_slice(&1u32.to_be_bytes()); // version-valid-for
    h[96..100].copy_from_slice(&3_045_000u32.to_be_bytes()); // SQLite version number
    Ok(pager.pages.concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_varints_and_records() {
        for (v, bytes) in [(0u64, vec![0u8]), (127, vec![0x7f]), (128, vec![0x81, 0x00]), (16383, vec![0xff, 0x7f])] {
            let mut out = Vec::new();
            varint(&mut out, v);
            assert_eq!(out, bytes);
        }
        let r = record(&[Value::Null, Value::Int(1), Value::Int(300), Value::Text("ab".into())]);
        assert_eq!(r, vec![5, 0, 9, 2, 17, 0x01, 0x2c, b'a', b'b']);
    }

    #[test]
    fn writes_big_tables() {
        let rows: Vec<(i64, Vec<Value>)> =
            (1..=5000).map(|i| (i, vec![Value::Null, Value::Text("x".repeat((i % 7000) as usize)), Value::Real(i as f64)])).collect();
        let t =
            Table { name: "t".into(), sql: "CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT, v REAL)".into(), rows, autoindexes: vec![] };
        let db = write_database(&[t], 0, 0).unwrap();
        assert_eq!(db.len() % PAGE_SIZE, 0);
        assert_eq!(&db[..16], b"SQLite format 3\0");
    }
}
