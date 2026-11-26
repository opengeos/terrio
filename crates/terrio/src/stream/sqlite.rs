//! A read-only SQLite reader that reads pages from the file as it needs them, so that the
//! feature tables of GeoPackages larger than memory are read a leaf page at a time.

use crate::drivers::sqlite::{self, PageSet, SchemaEntry, Table, Value, bad, cell_pointer, varint};
use crate::drivers::vector::gpkg_read::Tables;
use crate::{Error, Result};
use std::io::{Read, Seek, SeekFrom};
use std::sync::Mutex;

/// An SQLite database file read page by page.
pub(crate) struct FileDb {
    file: Mutex<std::fs::File>,
    page_size: usize,
    usable: usize,
    pages: u64,
    encoding: u32,
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
        let mut db = Self {
            file: Mutex::new(file),
            page_size,
            usable,
            pages: len / page_size as u64,
            encoding: sqlite::text_encoding(u32::from_be_bytes([h[56], h[57], h[58], h[59]])),
            schema: Vec::new(),
        };
        let mut entries = Vec::new();
        let mut cursor = db.cursor(1);
        while let Some(rows) = cursor.next_leaf(&db)? {
            for (_, payload) in rows {
                let r = db.decode_record(&payload)?;
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
    fn payload(&self, page_num: u32, page: &[u8], start: usize, size: usize) -> Result<Vec<u8>> {
        let mut seen = std::collections::HashSet::new();
        self.payload_checked(page, start, size, |p| {
            if p == page_num || !seen.insert(p) {
                return Err(bad("overflow page reached twice"));
            }
            Ok(())
        })
    }

    /// A cell's payload, following overflow pages and notifying `visit_overflow` for each overflow
    /// page before reading it so cycles and shared overflow pages are rejected.
    fn payload_checked(
        &self,
        page: &[u8],
        start: usize,
        size: usize,
        mut visit_overflow: impl FnMut(u32) -> Result<()>,
    ) -> Result<Vec<u8>> {
        let u = self.usable;
        let x = u - 35;
        if size <= x {
            return start.checked_add(size).and_then(|end| page.get(start..end)).map(<[u8]>::to_vec).ok_or_else(|| bad("payload"));
        }
        // Each overflow page holds `u - 4` bytes; a chain longer than the file is corrupt.
        if (size / (u - 4)) as u64 > self.pages {
            return Err(bad("payload"));
        }
        let local = sqlite::local_payload_size(u, size, x);
        let mut out = page.get(start..start + local).ok_or_else(|| bad("payload"))?.to_vec();
        let mut next =
            u32::from_be_bytes(page.get(start + local..start + local + 4).and_then(|b| b.try_into().ok()).ok_or_else(|| bad("overflow"))?);
        while out.len() < size && next != 0 {
            visit_overflow(next)?;
            let p = self.page(next)?;
            next = u32::from_be_bytes([p[0], p[1], p[2], p[3]]);
            let take = (size - out.len()).min(u - 4);
            out.extend_from_slice(&p[4..4 + take]);
        }
        if out.len() < size || next != 0 {
            return Err(bad("payload"));
        }
        Ok(out)
    }

    /// A cursor over the rows of the table b-tree rooted at `root`, in rowid order.
    pub(crate) fn cursor(&self, root: u32) -> Cursor {
        Cursor { stack: vec![vec![root].into_iter()], visited: PageSet::new(self.pages) }
    }

    /// Decodes a record for reading, as [`crate::drivers::sqlite::Db::decode_record`] does.
    pub(crate) fn decode_record(&self, payload: &[u8]) -> Result<Vec<Value>> {
        crate::drivers::sqlite_repack::decode_record_lossy(payload, self.encoding)
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

/// Rows `(rowid, payload)`.
pub(crate) type Rows = Vec<(i64, Vec<u8>)>;

/// A position in a table b-tree: for each level from the root down, the pages still to visit
/// (each interior page is read once).
pub(crate) struct Cursor {
    stack: Vec<std::vec::IntoIter<u32>>,
    /// The pages visited: a b-tree reaches each page once, so a page reached twice is corrupt
    /// (a cycle, or interior pages sharing children, which would otherwise visit exponentially
    /// many leaves).
    visited: PageSet,
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
            self.visited.insert(n)?;
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
                    let cells = sqlite::table_leaf_cells(&page, n, db.usable)?;
                    let mut out = Vec::with_capacity(cells.len());
                    for cell in cells {
                        let payload = db.payload_checked(&page, cell.payload_start, cell.payload_size, |p| self.visited.insert(p))?;
                        out.push((cell.rowid, payload));
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
                rows.push(row_values(self, &payload, rowid, columns.len(), rowid_col)?);
            }
        }
        Ok(Table { columns, rows })
    }

    fn lookup(&self, root: u32, rowid: i64) -> Result<Option<Vec<u8>>> {
        let mut n = root;
        for _ in 0..64 {
            let page = self.page(n)?;
            let h = if n == 1 { 100 } else { 0 };
            match page[h] {
                0x05 => {
                    let cells = u16::from_be_bytes([page[h + 3], page[h + 4]]) as usize;
                    let cell = |c: usize| cell_pointer(&page, h + 12 + 2 * c);
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
                    let cells = sqlite::table_leaf_cells(&page, n, self.usable)?;
                    if let Ok(i) = cells.binary_search_by_key(&rowid, |c| c.rowid) {
                        let cell = &cells[i];
                        return self.payload(n, &page, cell.payload_start, cell.payload_size).map(Some);
                    }
                    return Ok(None);
                }
                _ => return Err(bad("unsupported page type")),
            }
        }
        Err(bad("b-tree too deep"))
    }
}

/// A row's values from its record, as [`crate::drivers::sqlite::Db::table`] gives them: padded to the table's columns,
/// the rowid alias column filled in.
pub(crate) fn row_values(db: &FileDb, payload: &[u8], rowid: i64, columns: usize, rowid_col: Option<usize>) -> Result<Vec<Value>> {
    let mut v = db.decode_record(payload)?;
    v.resize(columns.max(v.len()), Value::Null);
    if let Some(c) = rowid_col
        && v[c] == Value::Null
    {
        v[c] = Value::Int(rowid);
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Interior pages that share children (each page's cell and right-most pointer naming the
    /// next page) are an error, as in the in-memory reader: a cursor over them would visit
    /// 2^depth leaves. Five levels here would visit the leaf 32 times.
    #[test]
    fn b_tree_pages_reached_twice_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dag.gpkg");
        std::fs::write(&path, crate::drivers::sqlite::fixtures::shared_children(5)).unwrap();
        let error = FileDb::open(path.to_str().unwrap()).err().expect("an error");
        assert!(error.to_string().contains("reached twice"), "{error}");
    }

    #[test]
    fn cyclic_overflow_chains_are_errors() {
        use crate::drivers::sqlite::fixtures;
        let dir = tempfile::tempdir().unwrap();
        for (name, bytes) in [
            ("self.gpkg", fixtures::cyclic_overflow_self()),
            ("pair.gpkg", fixtures::cyclic_overflow_pair()),
            ("single.gpkg", fixtures::cyclic_overflow_single_page()),
            ("shared.gpkg", fixtures::shared_overflow_cells()),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            let db = open_raw_without_schema(&path);
            let mut cursor = db.cursor(1);
            assert!(cursor.next_leaf(&db).is_err(), "{name}: cursor should reject cyclic/shared overflow");
            if name != "shared.gpkg" {
                assert!(db.lookup(1, 1).is_err(), "{name}: lookup should reject cyclic overflow");
            }
        }

        // Cross-leaf shared overflow page across cursor leaf steps.
        let shared_leaves_path = dir.path().join("shared_leaves.gpkg");
        std::fs::write(&shared_leaves_path, fixtures::shared_overflow_leaves()).unwrap();
        let db = open_raw_without_schema(&shared_leaves_path);
        let mut cursor = db.cursor(1);
        assert!(cursor.next_leaf(&db).unwrap().is_some());
        assert!(cursor.next_leaf(&db).is_err());

        // Cross-leaf disjoint overflow chains succeed across cursor leaf steps and lookup.
        let disjoint_leaves_path = dir.path().join("disjoint_leaves.gpkg");
        std::fs::write(&disjoint_leaves_path, fixtures::disjoint_overflow_leaves()).unwrap();
        let db = open_raw_without_schema(&disjoint_leaves_path);
        let mut cursor = db.cursor(1);
        let leaf1 = cursor.next_leaf(&db).unwrap().unwrap();
        let leaf2 = cursor.next_leaf(&db).unwrap().unwrap();
        assert!(cursor.next_leaf(&db).unwrap().is_none());
        assert_eq!((leaf1.len(), leaf1[0].0, leaf1[0].1[3]), (1, 1, 1));
        assert_eq!((leaf2.len(), leaf2[0].0, leaf2[0].1[3]), (1, 2, 2));
        assert_eq!(db.lookup(1, 1).unwrap().unwrap()[3], 1);
        assert_eq!(db.lookup(1, 2).unwrap().unwrap()[3], 2);
        assert!(db.lookup(1, 3).unwrap().is_none());

        // Also test FileDb::lookup directly on a valid empty-schema DB with a cyclic overflow
        // leaf on page 2 (`3 -> 3`).
        let mut b = fixtures::database(3);
        b[100] = 0x0d; // empty valid schema leaf on page 1
        b[512] = 0x0d; // leaf table on page 2
        b[512 + 3..512 + 5].copy_from_slice(&1u16.to_be_bytes());
        b[512 + 8..512 + 10].copy_from_slice(&200u16.to_be_bytes());
        let start = fixtures::write_leaf_cell(&mut b, 2, 200, 1, 92 + 2 * 508);
        b[start + 92..start + 96].copy_from_slice(&3u32.to_be_bytes());
        b[1024..1028].copy_from_slice(&3u32.to_be_bytes());
        let path = dir.path().join("lookup_cycle.gpkg");
        std::fs::write(&path, b).unwrap();
        let db = FileDb::open(path.to_str().unwrap()).unwrap();
        assert!(db.lookup(2, 1).is_err());
    }

    #[test]
    fn aliased_and_overlapping_leaf_cells_are_errors() {
        use crate::drivers::sqlite::fixtures;
        let dir = tempfile::tempdir().unwrap();
        for (name, bytes) in [("aliased.gpkg", fixtures::aliased_leaf_cells()), ("overlapping.gpkg", fixtures::overlapping_leaf_cells())] {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            let db = open_raw_without_schema(&path);
            let mut cursor = db.cursor(1);
            assert!(cursor.next_leaf(&db).is_err(), "{name}: cursor should reject aliased/overlapping cells");
            assert!(db.lookup(1, 1).is_err(), "{name}: lookup should reject aliased/overlapping cells");
        }

        // Cell pointer falling inside the page header / cell pointer array (`104 < 110`).
        let mut b = fixtures::database(2);
        b[100] = 0x0d;
        b[103..105].copy_from_slice(&1u16.to_be_bytes());
        b[108..110].copy_from_slice(&104u16.to_be_bytes());
        let header_path = dir.path().join("header_overlap.gpkg");
        std::fs::write(&header_path, b).unwrap();
        let db = open_raw_without_schema(&header_path);
        assert!(db.lookup(1, 1).is_err());
    }

    /// Opens a synthetic 512-byte-page `FileDb` without decoding `sqlite_master` records, so tests
    /// can exercise `Cursor::next_leaf` on raw payloads directly.
    fn open_raw_without_schema(path: &std::path::Path) -> FileDb {
        let file = std::fs::File::open(path).unwrap();
        let len = file.metadata().unwrap().len();
        FileDb { file: Mutex::new(file), page_size: 512, usable: 512, pages: len / 512, encoding: 1, schema: Vec::new() }
    }
}
