//! A read-only SQLite reader that reads pages from the file as it needs them, so that the
//! feature tables of GeoPackages larger than memory are read a leaf page at a time.

use crate::drivers::sqlite::{self, Db, SchemaEntry, Table, Value};
use crate::drivers::vector::gpkg_read::Tables;
use crate::{Error, Result};
use std::io::{Read, Seek, SeekFrom};
use std::sync::Mutex;

fn bad(msg: &str) -> Error {
    Error::InvalidData(format!("SQLite: {msg}"))
}

/// Reads a varint at `i`: `(value, length)`.
fn varint(b: &[u8], i: usize) -> Option<(i64, usize)> {
    let mut v: u64 = 0;
    for k in 0..9 {
        let byte = *b.get(i + k)?;
        if k == 8 {
            v = (v << 8) | byte as u64;
            return Some((v as i64, 9));
        }
        v = (v << 7) | (byte & 0x7f) as u64;
        if byte & 0x80 == 0 {
            return Some((v as i64, k + 1));
        }
    }
    None
}

/// An SQLite database file read page by page.
pub(crate) struct FileDb {
    file: Mutex<std::fs::File>,
    page_size: usize,
    usable: usize,
    pages: u64,
    schema: Vec<SchemaEntry>,
}

impl FileDb {
    /// Opens a database file, reading its header and schema.
    pub(crate) fn open(path: &str) -> Result<Self> {
        let mut file = std::fs::File::open(path)?;
        let mut h = [0u8; 100];
        file.read_exact(&mut h).map_err(|_| bad("not an SQLite database"))?;
        if !h.starts_with(b"SQLite format 3\0") {
            return Err(bad("not an SQLite database"));
        }
        // A database in WAL mode may have pages that only its `-wal` file holds.
        let wal = std::path::Path::new(&format!("{path}-wal")).metadata().is_ok_and(|m| m.len() > 0);
        if wal {
            return Err(Error::Unsupported(format!("'{path}' has a write-ahead log: checkpoint it before streaming")));
        }
        let page_size = match u16::from_be_bytes([h[16], h[17]]) {
            1 => 65536,
            p if p >= 512 && p.is_power_of_two() => p as usize,
            _ => return Err(bad("invalid page size")),
        };
        // SQLite requires at least 480 usable bytes a page.
        let usable = page_size.checked_sub(h[20] as usize).filter(|&u| u >= 480).ok_or_else(|| bad("invalid reserved space"))?;
        let len = file.metadata()?.len();
        let mut db = Self { file: Mutex::new(file), page_size, usable, pages: len / page_size as u64, schema: Vec::new() };
        let mut entries = Vec::new();
        let mut cursor = db.cursor(1);
        while let Some(rows) = cursor.next_leaf(&db)? {
            for (_, payload) in rows {
                let r = Db::record_values(&payload);
                let text = |i: usize| r.get(i).and_then(Value::as_str).unwrap_or_default().to_string();
                entries.push((text(0), text(1), text(2), r.get(3).and_then(Value::as_i64).unwrap_or(0) as u32, text(4)));
            }
        }
        db.schema = entries;
        Ok(db)
    }

    fn page(&self, n: u32) -> Result<Vec<u8>> {
        if n == 0 || n as u64 > self.pages {
            return Err(bad("page out of range"));
        }
        let mut buf = vec![0u8; self.page_size];
        let mut f = self.file.lock().map_err(|_| bad("file lock"))?;
        f.seek(SeekFrom::Start((n as u64 - 1) * self.page_size as u64))?;
        f.read_exact(&mut buf)?;
        Ok(buf)
    }

    /// A cell's payload, following overflow pages.
    fn payload(&self, page: &[u8], start: usize, size: usize) -> Result<Vec<u8>> {
        let u = self.usable;
        let x = u - 35;
        if size <= x {
            return start.checked_add(size).and_then(|end| page.get(start..end)).map(<[u8]>::to_vec).ok_or_else(|| bad("payload"));
        }
        // Each overflow page holds `u - 4` bytes; a chain longer than the file is corrupt.
        if (size / (u - 4)) as u64 > self.pages {
            return Err(bad("payload"));
        }
        let m = ((u - 12) * 32 / 255) - 23;
        let k = m + (size - m) % (u - 4);
        let local = if k <= x { k } else { m };
        let mut out = page.get(start..start + local).ok_or_else(|| bad("payload"))?.to_vec();
        let mut next =
            u32::from_be_bytes(page.get(start + local..start + local + 4).and_then(|b| b.try_into().ok()).ok_or_else(|| bad("overflow"))?);
        while out.len() < size && next != 0 {
            let p = self.page(next)?;
            next = u32::from_be_bytes([p[0], p[1], p[2], p[3]]);
            let take = (size - out.len()).min(u - 4);
            out.extend_from_slice(&p[4..4 + take]);
        }
        if out.len() < size {
            return Err(bad("payload"));
        }
        Ok(out)
    }

    /// A cursor over the rows of the table b-tree rooted at `root`, in rowid order.
    pub(crate) fn cursor(&self, root: u32) -> Cursor {
        Cursor { stack: vec![vec![root].into_iter()] }
    }

    /// The root page and `CREATE TABLE` statement of a rowid table.
    pub(crate) fn table_root(&self, name: &str) -> Result<(u32, String)> {
        for (kind, table, _, root, sql) in &self.schema {
            if kind != "table" || !table.eq_ignore_ascii_case(name) {
                continue;
            }
            if *root == 0 {
                return Err(bad(&format!("'{name}' is a virtual table")));
            }
            if sql.to_ascii_uppercase().split_whitespace().collect::<Vec<_>>().join(" ").contains("WITHOUT ROWID") {
                return Err(bad(&format!("'{name}' is a WITHOUT ROWID table, which is not supported")));
            }
            return Ok((*root, sql.clone()));
        }
        Err(bad(&format!("no table '{name}'")))
    }
}

/// The cell offset stored at `p` in a page's cell pointer array.
fn cell_pointer(page: &[u8], p: usize) -> Result<usize> {
    let b = page.get(p..p + 2).ok_or_else(|| bad("cell pointer"))?;
    Ok(u16::from_be_bytes([b[0], b[1]]) as usize)
}

/// Rows `(rowid, payload)`.
pub(crate) type Rows = Vec<(i64, Vec<u8>)>;

/// A position in a table b-tree: for each level from the root down, the pages still to visit
/// (each interior page is read once).
pub(crate) struct Cursor {
    stack: Vec<std::vec::IntoIter<u32>>,
}

impl Cursor {
    /// The rows `(rowid, payload)` of the next leaf page; `None` past the last.
    pub(crate) fn next_leaf(&mut self, db: &FileDb) -> Result<Option<Rows>> {
        while let Some(level) = self.stack.last_mut() {
            let Some(n) = level.next() else {
                self.stack.pop();
                continue;
            };
            if self.stack.len() > 64 {
                return Err(bad("b-tree too deep"));
            }
            let page = db.page(n)?;
            let h = if n == 1 { 100 } else { 0 };
            let cells = u16::from_be_bytes([page[h + 3], page[h + 4]]) as usize;
            match page[h] {
                0x05 => {
                    // Interior: the cells' left children, then the right-most pointer.
                    let mut children = Vec::with_capacity(cells + 1);
                    for c in 0..cells {
                        let off = cell_pointer(&page, h + 12 + 2 * c)?;
                        children
                            .push(u32::from_be_bytes(page.get(off..off + 4).and_then(|b| b.try_into().ok()).ok_or_else(|| bad("cell"))?));
                    }
                    children.push(u32::from_be_bytes(page[h + 8..h + 12].try_into().map_err(|_| bad("page"))?));
                    self.stack.push(children.into_iter());
                }
                0x0d => {
                    let mut out = Vec::with_capacity(cells);
                    for c in 0..cells {
                        let off = cell_pointer(&page, h + 8 + 2 * c)?;
                        let (size, n1) = varint(&page, off).ok_or_else(|| bad("cell"))?;
                        let (rowid, n2) = varint(&page, off + n1).ok_or_else(|| bad("cell"))?;
                        let size = usize::try_from(size).map_err(|_| bad("cell"))?;
                        out.push((rowid, db.payload(&page, off + n1 + n2, size)?));
                    }
                    return Ok(Some(out));
                }
                _ => return Err(bad("unsupported page type")),
            }
        }
        Ok(None)
    }
}

impl Tables for FileDb {
    fn schema(&self) -> Result<Vec<SchemaEntry>> {
        Ok(self.schema.clone())
    }

    fn table(&self, name: &str) -> Result<Table> {
        let (root, sql) = self.table_root(name)?;
        let columns: Vec<String> = sqlite::column_types(&sql).into_iter().map(|(n, _)| n).collect();
        let rowid_col = sqlite::rowid_column(&sql);
        let mut rows = Vec::new();
        let mut cursor = self.cursor(root);
        while let Some(leaf) = cursor.next_leaf(self)? {
            for (rowid, payload) in leaf {
                rows.push(row_values(&payload, rowid, columns.len(), rowid_col));
            }
        }
        Ok(Table { columns, rows })
    }

    fn lookup(&self, root: u32, rowid: i64) -> Result<Option<Vec<u8>>> {
        let mut n = root;
        for _ in 0..64 {
            let page = self.page(n)?;
            let h = if n == 1 { 100 } else { 0 };
            let kind = page[h];
            let cells = u16::from_be_bytes([page[h + 3], page[h + 4]]) as usize;
            let header_len = if kind == 0x05 { 12 } else { 8 };
            let cell = |c: usize| cell_pointer(&page, h + header_len + 2 * c);
            match kind {
                0x05 => {
                    let (mut lo, mut hi) = (0, cells);
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let (key, _) = varint(&page, cell(mid)? + 4).ok_or_else(|| bad("cell"))?;
                        if key < rowid { lo = mid + 1 } else { hi = mid }
                    }
                    n = if lo < cells {
                        let off = cell(lo)?;
                        u32::from_be_bytes(page.get(off..off + 4).and_then(|b| b.try_into().ok()).ok_or_else(|| bad("cell"))?)
                    } else {
                        u32::from_be_bytes(page[h + 8..h + 12].try_into().map_err(|_| bad("page"))?)
                    };
                }
                0x0d => {
                    let (mut lo, mut hi) = (0, cells);
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let off = cell(mid)?;
                        let (size, n1) = varint(&page, off).ok_or_else(|| bad("cell"))?;
                        let (key, n2) = varint(&page, off + n1).ok_or_else(|| bad("cell"))?;
                        match key.cmp(&rowid) {
                            std::cmp::Ordering::Less => lo = mid + 1,
                            std::cmp::Ordering::Greater => hi = mid,
                            std::cmp::Ordering::Equal => {
                                let size = usize::try_from(size).map_err(|_| bad("cell"))?;
                                return self.payload(&page, off + n1 + n2, size).map(Some);
                            }
                        }
                    }
                    return Ok(None);
                }
                _ => return Err(bad("unsupported page type")),
            }
        }
        Err(bad("b-tree too deep"))
    }
}

/// A row's values from its record, as [`Db::table`] gives them: padded to the table's columns,
/// the rowid alias column filled in.
pub(crate) fn row_values(payload: &[u8], rowid: i64, columns: usize, rowid_col: Option<usize>) -> Vec<Value> {
    let mut v = Db::record_values(payload);
    v.resize(columns.max(v.len()), Value::Null);
    if let Some(c) = rowid_col
        && v[c] == Value::Null
    {
        v[c] = Value::Int(rowid);
    }
    v
}
