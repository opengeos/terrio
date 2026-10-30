//! A writer of SQLite database files, enough for GeoPackages: tables (rowid b-trees, any size,
//! with overflow pages) and small indexes (the automatic indexes of UNIQUE and PRIMARY KEY
//! constraints), written in one go.
//!
//! The format is described at <https://www.sqlite.org/fileformat2.html>.

use crate::Result;

const PAGE_SIZE: usize = 4096;
/// The page holding the bytes at 1 GiB, which SQLite uses for file locks: never part of the
/// database, a file larger than 1 GiB has it unused.
const LOCK_PAGE: u32 = (0x4000_0000 / PAGE_SIZE) as u32 + 1;

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

pub(crate) fn varint(out: &mut Vec<u8>, v: u64) {
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

pub(crate) fn varint_len(v: u64) -> usize {
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

/// Page storage a b-tree is built in: pages allocated and written by number (1-based).
pub(crate) trait PageStore {
    /// The page size (and usable size: no bytes are reserved).
    fn page_size(&self) -> usize;
    /// Allocates a zeroed page and returns its number.
    fn alloc(&mut self) -> Result<u32>;
    /// The page to write.
    fn page_mut(&mut self, n: u32) -> Result<&mut [u8]>;
}

/// The pages being built: page 1, and pages `base..` (pages 2 to `base - 1` belong to tables
/// already written, see [`StreamTable`]).
struct Pager {
    page1: Vec<u8>,
    pages: Vec<Vec<u8>>,
    base: u32,
}

impl Pager {
    fn new(base: u32) -> Self {
        Self { page1: vec![0; PAGE_SIZE], pages: Vec::new(), base }
    }
}

impl PageStore for Pager {
    fn page_size(&self) -> usize {
        PAGE_SIZE
    }

    /// Allocates a page after page 1 (never the lock-byte page).
    fn alloc(&mut self) -> Result<u32> {
        self.pages.push(vec![0; PAGE_SIZE]);
        let n = self.base + self.pages.len() as u32 - 1;
        if n != LOCK_PAGE {
            return Ok(n);
        }
        self.pages.push(vec![0; PAGE_SIZE]);
        Ok(n + 1)
    }

    fn page_mut(&mut self, n: u32) -> Result<&mut [u8]> {
        if n == 1 {
            return Ok(&mut self.page1);
        }
        let i = n.checked_sub(self.base).ok_or_else(|| crate::Error::InvalidData(format!("page {n} was not built here")))? as usize;
        self.pages.get_mut(i).map(Vec::as_mut_slice).ok_or_else(|| crate::Error::InvalidData(format!("page {n} was not built here")))
    }
}

/// The most and least bytes of a payload kept in a table leaf cell, before overflow pages.
pub(crate) fn table_local_limits(usable: usize) -> (usize, usize) {
    (usable - 35, (usable - 12) * 32 / 255 - 23)
}

/// The most and least bytes of a payload kept in an index cell, before overflow pages.
pub(crate) fn index_local_limits(usable: usize) -> (usize, usize) {
    ((usable - 12) * 64 / 255 - 23, (usable - 12) * 32 / 255 - 23)
}

/// Stores a payload: the local part, and the rest in a chain of overflow pages.
pub(crate) fn store_payload<S: PageStore + ?Sized>(
    store: &mut S,
    payload: &[u8],
    max_local: usize,
    min_local: usize,
) -> Result<(Vec<u8>, Option<u32>)> {
    if payload.len() <= max_local {
        return Ok((payload.to_vec(), None));
    }
    let usable = store.page_size();
    let k = min_local + (payload.len() - min_local) % (usable - 4);
    let local = if k <= max_local { k } else { min_local };
    let mut rest = &payload[local..];
    let first = store.alloc()?;
    let mut page = first;
    loop {
        let n = rest.len().min(usable - 4);
        let next = if rest.len() > n { store.alloc()? } else { 0 };
        let p = store.page_mut(page)?;
        p[..4].copy_from_slice(&next.to_be_bytes());
        p[4..4 + n].copy_from_slice(&rest[..n]);
        rest = &rest[n..];
        if next == 0 {
            break;
        }
        page = next;
    }
    Ok((payload[..local].to_vec(), Some(first)))
}

/// Writes a b-tree page: header, cell pointers and cells (packed at the end of the page). On
/// page 1 the first 100 bytes (the database header) are kept.
pub(crate) fn write_node<S: PageStore + ?Sized>(store: &mut S, n: u32, flag: u8, cells: &[Vec<u8>], right: Option<u32>) -> Result<()> {
    let size = store.page_size();
    let offset = if n == 1 { 100 } else { 0 };
    let p = store.page_mut(n)?;
    p[offset..].fill(0);
    p[offset] = flag;
    p[offset + 3..offset + 5].copy_from_slice(&(cells.len() as u16).to_be_bytes());
    let header = if right.is_some() { 12 } else { 8 };
    if let Some(r) = right {
        p[offset + 8..offset + 12].copy_from_slice(&r.to_be_bytes());
    }
    let mut end = size;
    for (i, c) in cells.iter().enumerate() {
        end -= c.len();
        p[end..end + c.len()].copy_from_slice(c);
        let ptr = offset + header + 2 * i;
        p[ptr..ptr + 2].copy_from_slice(&(end as u16).to_be_bytes());
    }
    // A content area starting at 65536 is stored as 0.
    p[offset + 5..offset + 7].copy_from_slice(&((end % 65536) as u16).to_be_bytes());
    Ok(())
}

/// A table b-tree cell: payload size, rowid, the local payload and the first overflow page.
pub(crate) fn table_cell<S: PageStore + ?Sized>(store: &mut S, rowid: i64, rec: &[u8]) -> Result<Vec<u8>> {
    let (max_local, min_local) = table_local_limits(store.page_size());
    let (local, overflow) = store_payload(store, rec, max_local, min_local)?;
    let mut c = Vec::with_capacity(local.len() + 18);
    varint(&mut c, rec.len() as u64);
    varint(&mut c, rowid as u64);
    c.extend_from_slice(&local);
    if let Some(o) = overflow {
        c.extend_from_slice(&o.to_be_bytes());
    }
    Ok(c)
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

/// Moves a b-tree root built on page `from` to page `to` (neither being page 1, so that the
/// page layouts are the same), freeing nothing: `from` is returned to the caller to free.
fn move_root<S: PageStore + ?Sized>(store: &mut S, from: u32, to: u32) -> Result<()> {
    let bytes = store.page_mut(from)?.to_vec();
    store.page_mut(to)?.copy_from_slice(&bytes);
    Ok(())
}

/// Builds a table b-tree of encoded records and returns its root page. With `root`, the root is
/// that page.
pub(crate) fn build_table<S: PageStore + ?Sized>(store: &mut S, rows: &[(i64, Vec<u8>)], root: Option<u32>) -> Result<u32> {
    let size = store.page_size();
    let mut cells = Vec::with_capacity(rows.len());
    let mut keys = Vec::with_capacity(rows.len());
    for (rowid, rec) in rows {
        cells.push(table_cell(store, *rowid, rec)?);
        keys.push(*rowid);
    }
    let root_room = if root == Some(1) { size - 100 } else { size };
    // Leaves sized for the root's room, so that a single leaf can be the root.
    // (A root with no cell but its right-most pointer is valid, as SQLite's balance_deeper makes.)
    let leaves = pack(cells, 8, root_room);
    if leaves.len() == 1 && leaves[0].iter().map(|c| c.len() + 2).sum::<usize>() + 8 <= root_room {
        let n = match root {
            Some(r) => r,
            None => store.alloc()?,
        };
        write_node(store, n, 0x0D, &leaves[0], None)?;
        return Ok(n);
    }
    // Leaves, then interior levels up to the root. Each child is known by its largest key.
    let mut level: Vec<(u32, i64)> = Vec::new();
    let mut k = 0;
    for leaf in leaves {
        let n = store.alloc()?;
        k += leaf.len();
        write_node(store, n, 0x0D, &leaf, None)?;
        level.push((n, keys[k - 1]));
    }
    loop {
        // Children grouped into nodes; the last child of each node is its right-most pointer.
        let mut groups: Vec<Vec<(u32, i64)>> = vec![Vec::new()];
        let mut used = 12;
        for &(child, key) in &level {
            let len = 4 + varint_len(key as u64) + 2;
            if groups.last().is_some_and(|g| !g.is_empty()) && used + len > size - 100 {
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
        let node = |store: &mut S, g: &[(u32, i64)], page: Option<u32>| -> Result<(u32, i64)> {
            let cells: Vec<Vec<u8>> = g[..g.len() - 1]
                .iter()
                .map(|&(child, key)| {
                    let mut c = child.to_be_bytes().to_vec();
                    varint(&mut c, key as u64);
                    c
                })
                .collect();
            let (right, key) = g[g.len() - 1];
            let n = match page {
                Some(p) => p,
                None => store.alloc()?,
            };
            write_node(store, n, 0x05, &cells, Some(right))?;
            Ok((n, key))
        };
        if groups.len() == 1 {
            return Ok(node(store, &groups[0], root)?.0);
        }
        level = groups.iter().map(|g| node(store, g, None)).collect::<Result<_>>()?;
    }
}

/// Compares index keys as SQLite does with the BINARY collation: NULL < numbers < text < blobs.
pub(crate) fn compare(a: &[Value], b: &[Value]) -> std::cmp::Ordering {
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
            (Value::Int(p), Value::Real(q)) => (*p as f64).total_cmp(q),
            (Value::Real(p), Value::Int(q)) => p.total_cmp(&(*q as f64)),
            _ => rank(x).cmp(&rank(y)),
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    a.len().cmp(&b.len())
}

/// The keys of an index over `columns` of a table's rows: the column values then the rowid,
/// sorted.
pub(crate) fn index_keys(rows: &[(i64, Vec<Value>)], columns: &[usize]) -> Vec<Vec<Value>> {
    let mut keys: Vec<Vec<Value>> = rows
        .iter()
        .map(|(rowid, values)| {
            let mut k: Vec<Value> = columns.iter().map(|&c| values.get(c).cloned().unwrap_or(Value::Null)).collect();
            k.push(Value::Int(*rowid));
            k
        })
        .collect();
    keys.sort_by(|a, b| compare(a, b));
    keys
}

/// Builds an index b-tree of any size over sorted keys and returns its root page. Index b-trees
/// hold each key once: in a leaf, or as the divider between two children in an interior node.
/// With `root`, the root is that page (not page 1), and the page the root was built in is
/// pushed to `spare`.
pub(crate) fn build_index<S: PageStore + ?Sized>(
    store: &mut S,
    keys: &[Vec<Value>],
    root: Option<u32>,
    spare: &mut Vec<u32>,
) -> Result<u32> {
    let records: Vec<_> = keys.iter().map(|key| record(key)).collect();
    build_index_records(store, &records, root, spare)
}

/// Builds an index from already sorted, encoded keys, preserving their bytes and collation order.
/// This also represents WITHOUT ROWID tables, expression indexes and UTF-16 databases.
pub(crate) fn build_index_records<S: PageStore + ?Sized>(
    store: &mut S,
    records: &[Vec<u8>],
    root: Option<u32>,
    spare: &mut Vec<u32>,
) -> Result<u32> {
    let (max_local, min_local) = index_local_limits(store.page_size());
    let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(records.len());
    for rec in records {
        let (local, overflow) = store_payload(store, rec, max_local, min_local)?;
        let mut cell = Vec::with_capacity(local.len() + 13);
        varint(&mut cell, rec.len() as u64);
        cell.extend_from_slice(&local);
        if let Some(page) = overflow {
            cell.extend_from_slice(&page.to_be_bytes());
        }
        payloads.push(cell);
    }
    // The leaves, with the keys between them as dividers.
    let (mut children, mut dividers) = split_index_level(payloads, 8, 0, store, |store, cells, _| {
        let n = store.alloc()?;
        write_node(store, n, 0x0A, &cells, None)?;
        Ok(n)
    })?;
    // Interior levels until one node is left.
    while children.len() > 1 {
        let kids = children.clone();
        let items: Vec<Vec<u8>> = dividers;
        let (c, d) = split_index_level(items, 12, 4, store, |store, cells, range: (usize, usize)| {
            // Dividers `range.0..range.1`, each with the child on its left; the child after the
            // last is the right-most pointer.
            let with_children: Vec<Vec<u8>> = cells
                .iter()
                .enumerate()
                .map(|(j, c)| {
                    let mut v = kids[range.0 + j].to_be_bytes().to_vec();
                    v.extend_from_slice(c);
                    v
                })
                .collect();
            let n = store.alloc()?;
            write_node(store, n, 0x02, &with_children, Some(kids[range.1]))?;
            Ok(n)
        })?;
        children = c;
        dividers = d;
    }
    match root {
        Some(r) => {
            move_root(store, children[0], r)?;
            spare.push(children[0]);
            Ok(r)
        }
        None => Ok(children[0]),
    }
}

/// Groups a level's items into nodes that fit a page, keeping the item after each full node as
/// the divider passed up to the parent level. `extra` is the size of each cell besides its item
/// (the child pointer of interior cells); `make` writes a node of items `range.0..range.1` and
/// returns its page. Returns the nodes and the dividers between them.
fn split_index_level<S, F>(items: Vec<Vec<u8>>, header: usize, extra: usize, store: &mut S, mut make: F) -> Result<(Vec<u32>, Vec<Vec<u8>>)>
where
    S: PageStore + ?Sized,
    F: FnMut(&mut S, Vec<Vec<u8>>, (usize, usize)) -> Result<u32>,
{
    let size = store.page_size();
    let n = items.len();
    let mut nodes = Vec::new();
    let mut dividers = Vec::new();
    let mut i = 0;
    loop {
        let start = i;
        let mut used = header;
        let mut cells = Vec::new();
        while i < n && (cells.is_empty() || used + items[i].len() + extra + 2 <= size) {
            used += items[i].len() + extra + 2;
            cells.push(items[i].clone());
            i += 1;
        }
        // The last item cannot be a divider (nothing would follow it): this node gives up its own
        // last item instead.
        if i + 1 == n && cells.len() > 1 {
            cells.pop();
            i -= 1;
        }
        nodes.push(make(store, cells, (start, i))?);
        if i >= n {
            break;
        }
        dividers.push(items[i].clone());
        i += 1;
    }
    Ok((nodes, dividers))
}

/// A schema entry without storage of its own: a trigger, or a virtual table.
pub struct SchemaObject {
    /// `trigger`, or `table` for a virtual table.
    pub kind: &'static str,
    /// The object's name.
    pub name: String,
    /// The table it belongs to (its own name for a virtual table).
    pub table: String,
    /// The `CREATE` statement.
    pub sql: String,
}

/// An index a `CREATE INDEX` statement made (not one of a table's constraints).
pub struct Index {
    /// The index name.
    pub name: String,
    /// The table it indexes.
    pub table: String,
    /// The `CREATE INDEX` statement.
    pub sql: String,
    /// The indexed columns.
    pub columns: Vec<usize>,
}

/// A table written to a database file as its rows arrive, before the rest of the database: its
/// leaves and overflow pages from page 2 on, each written once filled, then its interior pages.
/// Only the current leaf is held, and the page and largest rowid of each node of the level being
/// built, so a table of any size is written in little memory. The pages are those
/// [`write_database`] writes for the same rows, except that overflow pages come before the leaf
/// that points to them rather than before all leaves.
pub struct StreamTable<W: std::io::Write + std::io::Seek> {
    out: W,
    next: u32,
    leaf: Vec<Vec<u8>>,
    used: usize,
    last_key: i64,
    level: Vec<(u32, i64)>,
}

impl<W: std::io::Write + std::io::Seek> StreamTable<W> {
    /// Starts a table whose pages go to `out`, a database file being written (page 1, the
    /// header and schema, is written last by [`write_database_parts`]).
    pub fn new(out: W) -> Self {
        Self::starting_at(out, 2)
    }

    /// Starts a table whose pages go to `out` from page `first` on, after other tables'.
    pub fn starting_at(out: W, first: u32) -> Self {
        Self { out, next: first, leaf: Vec::new(), used: 8, last_key: 0, level: Vec::new() }
    }

    fn alloc(&mut self) -> u32 {
        if self.next == LOCK_PAGE {
            self.next += 1;
        }
        self.next += 1;
        self.next - 1
    }

    fn write_page(&mut self, n: u32, page: &[u8]) -> Result<()> {
        self.out.seek(std::io::SeekFrom::Start(u64::from(n - 1) * PAGE_SIZE as u64))?;
        self.out.write_all(page)?;
        Ok(())
    }

    /// Adds a row (an encoded [`record`]); rowids must increase.
    pub fn push(&mut self, rowid: i64, rec: &[u8]) -> Result<()> {
        let max_local = PAGE_SIZE - 35;
        let min_local = (PAGE_SIZE - 12) * 32 / 255 - 23;
        // The payload, with its overflow pages written now (as `Pager::payload` lays them out).
        let (local, overflow) = if rec.len() <= max_local {
            (rec, None)
        } else {
            let usable = PAGE_SIZE;
            let k = min_local + (rec.len() - min_local) % (usable - 4);
            let local = if k <= max_local { k } else { min_local };
            let mut rest = &rec[local..];
            let first = self.alloc();
            let mut page = first;
            loop {
                let n = rest.len().min(usable - 4);
                let next = if rest.len() > n { self.alloc() } else { 0 };
                let mut p = vec![0u8; PAGE_SIZE];
                p[..4].copy_from_slice(&next.to_be_bytes());
                p[4..4 + n].copy_from_slice(&rest[..n]);
                self.write_page(page, &p)?;
                rest = &rest[n..];
                if next == 0 {
                    break;
                }
                page = next;
            }
            (&rec[..local], Some(first))
        };
        let mut c = Vec::with_capacity(local.len() + 18);
        varint(&mut c, rec.len() as u64);
        varint(&mut c, rowid as u64);
        c.extend_from_slice(local);
        if let Some(o) = overflow {
            c.extend_from_slice(&o.to_be_bytes());
        }
        // As `pack` splits cells into leaves.
        if self.used + c.len() + 2 > PAGE_SIZE && !self.leaf.is_empty() {
            self.flush_leaf()?;
        }
        self.used += c.len() + 2;
        self.leaf.push(c);
        self.last_key = rowid;
        Ok(())
    }

    fn flush_leaf(&mut self) -> Result<()> {
        let n = self.alloc();
        let page = node_bytes(0x0D, &std::mem::take(&mut self.leaf), None);
        self.write_page(n, &page)?;
        self.level.push((n, self.last_key));
        self.used = 8;
        Ok(())
    }

    /// Writes the rest of the table and returns its root page and the first page after it.
    pub fn finish(self) -> Result<(u32, u32)> {
        self.finish_with_output().map(|(root, next, _)| (root, next))
    }

    /// [`StreamTable::finish`], handing back the output for more pages.
    pub fn finish_with_output(mut self) -> Result<(u32, u32, W)> {
        if !self.leaf.is_empty() || self.level.is_empty() {
            self.flush_leaf()?;
        }
        let mut level = std::mem::take(&mut self.level);
        // A single leaf is the root.
        while level.len() > 1 {
            // As `build_table` groups children into interior nodes.
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
            let n = groups.len();
            if n > 1
                && groups[n - 1].len() == 1
                && let Some(moved) = groups[n - 2].pop()
            {
                groups[n - 1].insert(0, moved);
            }
            let mut next_level = Vec::with_capacity(groups.len());
            for g in &groups {
                let cells: Vec<Vec<u8>> = g[..g.len() - 1]
                    .iter()
                    .map(|&(child, key)| {
                        let mut c = child.to_be_bytes().to_vec();
                        varint(&mut c, key as u64);
                        c
                    })
                    .collect();
                let (right, key) = g[g.len() - 1];
                let page = self.alloc();
                let bytes = node_bytes(0x05, &cells, Some(right));
                self.write_page(page, &bytes)?;
                next_level.push((page, key));
            }
            level = next_level;
        }
        self.out.flush()?;
        Ok((level[0].0, self.next, self.out))
    }
}

/// Writes a table whose rows are sorted by rowid (and has no automatic index) with a
/// [`StreamTable`] from page `first` on: the rows of `table` then `more`. Returns the table as
/// listed in the schema, and the first page after it.
pub fn stream_table<W: std::io::Write + std::io::Seek>(out: W, first: u32, table: Table, more: &mut Rows) -> Result<(BuiltTable, u32, W)> {
    let mut st = StreamTable::starting_at(out, first);
    for (rowid, values) in &table.rows {
        st.push(*rowid, &record(values))?;
    }
    for (rowid, values) in more {
        st.push(rowid, &record(&values))?;
    }
    let (root, next, out) = st.finish_with_output()?;
    Ok((BuiltTable { name: table.name, sql: table.sql, root }, next, out))
}

/// A b-tree page (not page 1): header, cell pointers and cells packed at the end.
fn node_bytes(flag: u8, cells: &[Vec<u8>], right: Option<u32>) -> Vec<u8> {
    let mut p = vec![0u8; PAGE_SIZE];
    p[0] = flag;
    p[3..5].copy_from_slice(&(cells.len() as u16).to_be_bytes());
    let header = if right.is_some() { 12 } else { 8 };
    if let Some(r) = right {
        p[8..12].copy_from_slice(&r.to_be_bytes());
    }
    let mut end = PAGE_SIZE;
    for (i, c) in cells.iter().enumerate() {
        end -= c.len();
        p[end..end + c.len()].copy_from_slice(c);
        let ptr = header + 2 * i;
        p[ptr..ptr + 2].copy_from_slice(&(end as u16).to_be_bytes());
    }
    p[5..7].copy_from_slice(&(end as u16).to_be_bytes());
    p
}

/// A table's rows (rowid and values) by increasing rowid, made as they are read.
pub type Rows<'a> = dyn Iterator<Item = (i64, Vec<Value>)> + 'a;

/// A table already written (by a [`StreamTable`]): its name, `CREATE TABLE` statement and root.
pub struct BuiltTable {
    /// The table name.
    pub name: String,
    /// The `CREATE TABLE` statement.
    pub sql: String,
    /// Its root page.
    pub root: u32,
}

/// Writes a database file holding the tables and schema objects, with `user_version` and
/// `application_id` set.
pub fn write_database(tables: &[Table], objects: &[SchemaObject], user_version: u32, application_id: u32) -> Result<Vec<u8>> {
    write_database_with_indexes(tables, &[], objects, user_version, application_id)
}

/// Writes a database file holding the tables, their indexes (automatic ones and `indexes`) and
/// schema objects, with `user_version` and `application_id` set.
pub fn write_database_with_indexes(
    tables: &[Table],
    indexes: &[Index],
    objects: &[SchemaObject],
    user_version: u32,
    application_id: u32,
) -> Result<Vec<u8>> {
    let (page1, rest) = write_database_parts(tables, &[], indexes, objects, user_version, application_id, 2)?;
    Ok([page1, rest].concat())
}

/// Writes the parts of a database file whose pages 2 to `base - 1` hold tables already written
/// (`built`, each listed in the schema before the table of `tables` at its index, or after them
/// all): page 1, and the pages from `base` on, holding `tables`, their indexes and the schema.
pub fn write_database_parts(
    tables: &[Table],
    built: &[(usize, BuiltTable)],
    indexes: &[Index],
    objects: &[SchemaObject],
    user_version: u32,
    application_id: u32,
    base: u32,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut pager = Pager::new(base);
    let mut spare = Vec::new();
    let mut schema_rows = Vec::new();
    let built_row = |b: &BuiltTable| {
        vec![
            Value::Text("table".into()),
            Value::Text(b.name.clone()),
            Value::Text(b.name.clone()),
            Value::Int(i64::from(b.root)),
            Value::Text(b.sql.clone()),
        ]
    };
    for (ti, t) in tables.iter().enumerate() {
        for (_, b) in built.iter().filter(|(at, _)| *at == ti) {
            schema_rows.push(built_row(b));
        }
        let rows: Vec<(i64, Vec<u8>)> = crate::par::map_range(t.rows.len(), |i| (t.rows[i].0, record(&t.rows[i].1)));
        let root = build_table(&mut pager, &rows, None)?;
        schema_rows.push(vec![
            Value::Text("table".into()),
            Value::Text(t.name.clone()),
            Value::Text(t.name.clone()),
            Value::Int(root as i64),
            Value::Text(t.sql.clone()),
        ]);
        for (i, cols) in t.autoindexes.iter().enumerate() {
            let root = build_index(&mut pager, &index_keys(&t.rows, cols), None, &mut spare)?;
            schema_rows.push(vec![
                Value::Text("index".into()),
                Value::Text(format!("sqlite_autoindex_{}_{}", t.name, i + 1)),
                Value::Text(t.name.clone()),
                Value::Int(root as i64),
                Value::Null,
            ]);
        }
        for ix in indexes.iter().filter(|ix| ix.table.eq_ignore_ascii_case(&t.name)) {
            let root = build_index(&mut pager, &index_keys(&t.rows, &ix.columns), None, &mut spare)?;
            schema_rows.push(vec![
                Value::Text("index".into()),
                Value::Text(ix.name.clone()),
                Value::Text(t.name.clone()),
                Value::Int(root as i64),
                Value::Text(ix.sql.clone()),
            ]);
        }
    }
    for (_, b) in built.iter().filter(|(at, _)| *at >= tables.len()) {
        schema_rows.push(built_row(b));
    }
    for o in objects {
        schema_rows.push(vec![
            Value::Text(o.kind.into()),
            Value::Text(o.name.clone()),
            Value::Text(o.table.clone()),
            Value::Int(0),
            Value::Text(o.sql.clone()),
        ]);
    }
    let schema: Vec<(i64, Vec<u8>)> = schema_rows.iter().enumerate().map(|(i, r)| (i as i64 + 1, record(r))).collect();
    build_table(&mut pager, &schema, Some(1))?;

    let count = base - 1 + pager.pages.len() as u32;
    let h = pager.page_mut(1)?;
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
    Ok((pager.page1, pager.pages.concat()))
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
        let db = write_database(&[t], &[], 0, 0).unwrap();
        assert_eq!(db.len() % PAGE_SIZE, 0);
        assert_eq!(&db[..16], b"SQLite format 3\0");
    }

    #[test]
    fn skips_the_lock_byte_page() {
        let mut pager = Pager::new(LOCK_PAGE - 1);
        assert_eq!([pager.alloc().unwrap(), pager.alloc().unwrap(), pager.alloc().unwrap()], [LOCK_PAGE - 1, LOCK_PAGE + 1, LOCK_PAGE + 2]);
        assert_eq!(pager.pages.len(), 4);
        let mut table = StreamTable::new(std::io::Cursor::new(Vec::new()));
        table.next = LOCK_PAGE - 1;
        assert_eq!([table.alloc(), table.alloc(), table.alloc()], [LOCK_PAGE - 1, LOCK_PAGE + 1, LOCK_PAGE + 2]);
    }

    #[test]
    fn streamed_tables_match_built_tables() {
        let sql = "CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT, v REAL)";
        // No row, a single leaf, interior levels, and then rows with overflow pages.
        for (n, len) in [(0i64, 10), (3, 10), (20000, 50), (5000, 9000)] {
            let rows: Vec<(i64, Vec<Value>)> = (1..=n)
                .map(|i| (i * 2, vec![Value::Null, Value::Text("y".repeat((i * 37 % len) as usize)), Value::Real(i as f64)]))
                .collect();
            let t = Table { name: "t".into(), sql: sql.into(), rows: rows.clone(), autoindexes: vec![] };
            let whole = write_database(&[t], &[], 7, 9).unwrap();

            let mut file = std::io::Cursor::new(Vec::new());
            let mut table = StreamTable::new(&mut file);
            for (rowid, values) in &rows {
                table.push(*rowid, &record(values)).unwrap();
            }
            let (root, base) = table.finish().unwrap();
            let built = BuiltTable { name: "t".into(), sql: sql.into(), root };
            let (page1, rest) = write_database_parts(&[], &[(0, built)], &[], &[], 7, 9, base).unwrap();
            let mut streamed = file.into_inner();
            streamed.resize((base as usize - 1) * PAGE_SIZE, 0);
            streamed.extend_from_slice(&rest);
            streamed[..PAGE_SIZE].copy_from_slice(&page1);
            if len < 1000 {
                assert!(streamed == whole, "{n} rows");
            } else {
                assert_eq!(streamed.len(), whole.len());
            }
            let db = crate::drivers::sqlite::Db::new(streamed).unwrap();
            let read = db.raw_rows("t").unwrap();
            assert_eq!(read.len(), rows.len());
            for ((rowid, rec), (k, values)) in read.iter().zip(&rows) {
                assert_eq!(rowid, k);
                assert!(*rec == record(values), "row {k}");
            }
        }
    }
}
