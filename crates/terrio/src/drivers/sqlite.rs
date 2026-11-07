//! A minimal read-only SQLite reader: whole-table scans of rowid tables, enough to read
//! GeoPackage tile pyramids.

use crate::{Error, Result};

/// A value in a row.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    pub(crate) fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(v) => Some(*v),
            Value::Real(v) => Some(*v as i64),
            _ => None,
        }
    }

    pub(crate) fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(v) => Some(*v as f64),
            Value::Real(v) => Some(*v),
            Value::Text(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    pub(crate) fn as_str(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }
}

/// A table's rows, with its column names.
pub(crate) struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

impl Table {
    /// The index of a column (case-insensitive).
    pub(crate) fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.eq_ignore_ascii_case(name))
    }

    /// A value by row and column name.
    pub(crate) fn get<'a>(&self, row: &'a [Value], name: &str) -> Option<&'a Value> {
        row.get(self.column(name)?)
    }
}

/// A schema entry: `(type, name, table, root page, sql)`.
pub(crate) type SchemaEntry = (String, String, String, u32, String);

/// An SQLite database file in memory.
pub(crate) struct Db {
    data: Bytes,
    page_size: usize,
    usable: usize,
}

/// A database's bytes: in memory, or a file mapped into memory (native builds), which reads only
/// the pages used and keeps them out of the process's own memory.
enum Bytes {
    Owned(Vec<u8>),
    #[cfg(not(target_family = "wasm"))]
    Mapped(memmap2::Mmap),
}

impl std::ops::Deref for Bytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            Self::Owned(v) => v,
            #[cfg(not(target_family = "wasm"))]
            Self::Mapped(m) => m,
        }
    }
}

pub(crate) fn bad(msg: &str) -> Error {
    Error::InvalidData(format!("SQLite: {msg}"))
}

/// The text encoding of a database header's encoding field, as SQLite reads it: its two low bits,
/// 0 meaning UTF-8. 1 for UTF-8, 2 and 3 for UTF-16LE and UTF-16BE.
pub(crate) fn text_encoding(field: u32) -> u32 {
    match field & 3 {
        0 => 1,
        encoding => encoding,
    }
}

/// Read the effective SQLite encoding once at the header boundary, including its UTF-8 default.
pub(crate) fn header_text_encoding(bytes: &[u8]) -> Result<u32> {
    let raw = bytes.get(56..60).and_then(|b| b.try_into().ok()).ok_or_else(|| bad("truncated header encoding"))?;
    Ok(text_encoding(u32::from_be_bytes(raw)))
}

/// Decode bounded batches in input order, stopping before the next batch after an error.
/// The page scan has already collected raw records; this bounds speculative value decoding,
/// not the memory used by the whole-table API.
fn decode_table_records(rows: &[(i64, Vec<u8>)], decode: impl Fn(&[u8]) -> Result<Vec<Value>> + Sync) -> Result<Vec<(i64, Vec<Value>)>> {
    let mut decoded = Vec::new();
    for batch in rows.chunks(256) {
        let values = crate::par::map_range(batch.len(), |i| decode(&batch[i].1).map(|v| (batch[i].0, v)));
        decoded.extend(values.into_iter().collect::<Result<Vec<_>>>()?);
    }
    Ok(decoded)
}

/// Reads a varint at `i`: `(value, length)`.
pub(crate) fn varint(b: &[u8], i: usize) -> Option<(i64, usize)> {
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

/// The pages of a database a b-tree walk has reached: one bit per page of the file, so that a
/// walk of a large file costs an eighth of a byte per page, and a page reached twice (a cycle, or
/// interior pages sharing children) is found.
pub(crate) struct PageSet {
    bits: Vec<u64>,
    pages: u64,
}

impl PageSet {
    /// An empty set for a file of `pages` pages.
    pub(crate) fn new(pages: u64) -> Self {
        PageSet { bits: vec![0; usize::try_from(pages.div_ceil(64)).unwrap_or(usize::MAX)], pages }
    }

    /// Adds page `n` (1-based). An error when it was there already, or is not a page of the file.
    pub(crate) fn insert(&mut self, n: u32) -> Result<()> {
        if n == 0 || u64::from(n) > self.pages {
            return Err(bad("page out of range"));
        }
        let i = n as usize - 1;
        let word = self.bits.get_mut(i / 64).ok_or_else(|| bad("page out of range"))?;
        let bit = 1u64 << (i % 64);
        if *word & bit != 0 {
            return Err(bad("b-tree page reached twice"));
        }
        *word |= bit;
        Ok(())
    }
}

/// The cell offset stored at `p` in a page's cell pointer array.
pub(crate) fn cell_pointer(page: &[u8], p: usize) -> Result<usize> {
    let b = page.get(p..p + 2).ok_or_else(|| bad("cell pointer"))?;
    Ok(u16::from_be_bytes([b[0], b[1]]) as usize)
}

impl Db {
    /// Opens a database from its bytes.
    pub(crate) fn new(data: Vec<u8>) -> Result<Self> {
        Self::from_bytes(Bytes::Owned(data))
    }

    /// Opens a database file. Native builds map it into memory rather than reading it: a
    /// GeoPackage's pages are then read as they are used, and a summary of a multi-gigabyte file
    /// reads only its metadata. The file must not be changed while it is open, as with any
    /// SQLite reader that does not lock it.
    pub(crate) fn open(path: &std::path::Path) -> Result<Self> {
        #[cfg(not(target_family = "wasm"))]
        {
            let file = std::fs::File::open(path)?;
            // SAFETY: the map is read-only, and lives as long as `Db`; a file changed or
            // truncated by another process while it is open is outside what terrio supports.
            if let Ok(map) = unsafe { memmap2::Mmap::map(&file) } {
                return Self::from_bytes(Bytes::Mapped(map));
            }
        }
        Self::new(std::fs::read(path)?)
    }

    fn from_bytes(data: Bytes) -> Result<Self> {
        if data.len() < 100 || !data.starts_with(b"SQLite format 3\0") {
            return Err(bad("not an SQLite database"));
        }
        let page_size = match u16::from_be_bytes([data[16], data[17]]) {
            1 => 65536,
            p if p >= 512 && p.is_power_of_two() => p as usize,
            _ => return Err(bad("invalid page size")),
        };
        // SQLite requires at least 480 usable bytes a page.
        let usable = page_size.checked_sub(data[20] as usize).filter(|&u| u >= 480).ok_or_else(|| bad("invalid reserved space"))?;
        Ok(Self { data, page_size, usable })
    }

    /// The number of whole pages in the file.
    fn pages(&self) -> usize {
        self.data.len() / self.page_size
    }

    fn page(&self, n: u32) -> Result<&[u8]> {
        let start = (n as usize).checked_sub(1).ok_or_else(|| bad("page 0"))?.checked_mul(self.page_size);
        start.and_then(|start| self.data.get(start..start.checked_add(self.page_size)?)).ok_or_else(|| bad("page out of range"))
    }

    /// Every row of the b-tree rooted at `root`, as `(rowid, payload)`. The interior pages are
    /// walked first, then the leaf pages are decoded in parallel.
    fn scan(&self, root: u32, out: &mut Vec<(i64, Vec<u8>)>, depth: usize) -> Result<()> {
        let mut leaves = Vec::new();
        let failure = self.leaves(root, &mut leaves, &mut PageSet::new(self.pages() as u64), depth).err();
        let rows = crate::par::map_range(leaves.len(), |i| self.leaf_rows(leaves[i]));
        // The first error in page order, as a sequential scan would meet it.
        for r in rows {
            out.extend(r?);
        }
        failure.map_or(Ok(()), Err)
    }

    /// The pages holding rows of the b-tree rooted at `root`, in order, until an error.
    /// `visited` holds the pages already walked: a b-tree reaches each page once, so a page
    /// reached twice is corrupt (a cycle, or interior pages sharing children, which would
    /// otherwise visit exponentially many leaves).
    fn leaves(&self, root: u32, out: &mut Vec<u32>, visited: &mut PageSet, depth: usize) -> Result<()> {
        if depth > 64 {
            return Err(bad("b-tree too deep"));
        }
        visited.insert(root)?;
        let page = self.page(root)?;
        let h = if root == 1 { 100 } else { 0 };
        if page[h] != 0x05 {
            // A leaf, or a page of another type that decoding it reports.
            out.push(root);
            return Ok(());
        }
        let cells = u16::from_be_bytes([page[h + 3], page[h + 4]]) as usize;
        for c in 0..cells {
            let off = cell_pointer(page, h + 12 + 2 * c)?;
            let child = u32::from_be_bytes(page.get(off..off + 4).and_then(|b| b.try_into().ok()).ok_or_else(|| bad("cell"))?);
            self.leaves(child, out, visited, depth + 1)?;
        }
        let right = u32::from_be_bytes(page[h + 8..h + 12].try_into().map_err(|_| bad("page"))?);
        self.leaves(right, out, visited, depth + 1)
    }

    /// The first stored row of a table, descending only its leftmost b-tree branch.
    pub(crate) fn first_row(&self, mut n: u32) -> Result<Option<(i64, Vec<Value>)>> {
        for _ in 0..64 {
            let page = self.page(n)?;
            let h = if n == 1 { 100 } else { 0 };
            match page.get(h).copied() {
                Some(0x0d) => {
                    return self.leaf_rows(n)?.into_iter().next().map(|(id, payload)| Ok((id, self.decode_record(&payload)?))).transpose();
                }
                Some(0x05) => {
                    let count = u16::from_be_bytes(page.get(h + 3..h + 5).and_then(|b| b.try_into().ok()).ok_or_else(|| bad("page"))?);
                    let offset = if count == 0 {
                        h + 8
                    } else {
                        u16::from_be_bytes(page.get(h + 12..h + 14).and_then(|b| b.try_into().ok()).ok_or_else(|| bad("page"))?) as usize
                    };
                    n = u32::from_be_bytes(page.get(offset..offset + 4).and_then(|b| b.try_into().ok()).ok_or_else(|| bad("page"))?);
                }
                _ => return Err(bad("unsupported page type")),
            }
        }
        Err(bad("b-tree is too deep"))
    }

    /// The rows of a leaf page, as `(rowid, payload)`.
    fn leaf_rows(&self, n: u32) -> Result<Vec<(i64, Vec<u8>)>> {
        let page = self.page(n)?;
        let h = if n == 1 { 100 } else { 0 };
        if page[h] != 0x0d {
            return Err(bad("unsupported page type"));
        }
        let cells = u16::from_be_bytes([page[h + 3], page[h + 4]]) as usize;
        let mut out = Vec::with_capacity(cells);
        for c in 0..cells {
            let off = cell_pointer(page, h + 8 + 2 * c)?;
            let (size, n1) = varint(page, off).ok_or_else(|| bad("cell"))?;
            let (rowid, n2) = varint(page, off + n1).ok_or_else(|| bad("cell"))?;
            let size = usize::try_from(size).map_err(|_| bad("cell"))?;
            out.push((rowid, self.payload(page, off + n1 + n2, size)?));
        }
        Ok(out)
    }

    /// A cell's payload, following overflow pages.
    fn payload(&self, page: &[u8], start: usize, size: usize) -> Result<Vec<u8>> {
        let u = self.usable;
        let x = u - 35;
        if size <= x {
            return start.checked_add(size).and_then(|end| page.get(start..end)).map(<[u8]>::to_vec).ok_or_else(|| bad("payload"));
        }
        // Each overflow page holds `u - 4` bytes; a chain longer than the file is corrupt, and
        // its declared size must not size the allocation.
        if size / (u - 4) > self.pages() {
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

    /// Decodes a record (a row's payload). A record whose values run past its end is an error.
    pub(crate) fn record_values(payload: &[u8]) -> Result<Vec<Value>> {
        Self::record(payload)
    }

    /// The serial types of a record's columns, and where its values start: an error when the
    /// header runs past the record.
    fn record_types(payload: &[u8]) -> Result<(Vec<i64>, usize)> {
        let Some((header_len, n)) = varint(payload, 0) else { return Ok((Vec::new(), 0)) };
        let header_len = usize::try_from(header_len).ok().filter(|&len| len <= payload.len()).ok_or_else(|| bad("record header"))?;
        let mut types = Vec::new();
        let mut i = n;
        while i < header_len {
            let Some((t, k)) = varint(payload, i) else { break };
            types.push(t);
            i += k;
        }
        Ok((types, header_len))
    }

    /// Decodes a value of serial type `t` at `pos`: the value and its length. A value that runs
    /// past the record is an error.
    fn value(payload: &[u8], t: i64, pos: usize) -> Result<(Value, usize)> {
        let len = Self::value_len(t);
        let b = pos.checked_add(len).and_then(|end| payload.get(pos..end)).ok_or_else(|| bad("truncated record"))?;
        let int = || -> i64 {
            let mut v: i64 = if b.first().is_some_and(|x| x & 0x80 != 0) { -1 } else { 0 };
            for x in b {
                v = (v << 8) | *x as i64;
            }
            v
        };
        let value = match t {
            0 => Value::Null,
            1..=6 => Value::Int(int()),
            7 => Value::Real(f64::from_bits(int() as u64)),
            8 => Value::Int(0),
            9 => Value::Int(1),
            t if t >= 12 && t % 2 == 0 => Value::Blob(b.to_vec()),
            t if t >= 13 => Value::Text(String::from_utf8_lossy(b).into_owned()),
            _ => Value::Null,
        };
        Ok((value, len))
    }

    /// The length of a value of serial type `t`.
    fn value_len(t: i64) -> usize {
        match t {
            1..=4 => t as usize,
            5 => 6,
            6 | 7 => 8,
            // A length no address space holds saturates, and fails the bounds check.
            t if t >= 12 => usize::try_from((t - 12) / 2).unwrap_or(usize::MAX),
            _ => 0,
        }
    }

    /// Decodes a record.
    fn record(payload: &[u8]) -> Result<Vec<Value>> {
        let (types, mut pos) = Self::record_types(payload)?;
        let mut out = Vec::with_capacity(types.len());
        for t in types {
            let (value, len) = Self::value(payload, t, pos)?;
            pos += len;
            out.push(value);
        }
        Ok(out)
    }

    /// Decodes one column of a record, as [`Db::table`] reads it (`Null` past the record's last
    /// column; the rowid alias column is not filled in). A column, or a column before it, that
    /// runs past the record is an error.
    pub(crate) fn record_column(payload: &[u8], column: usize) -> Result<Value> {
        let (types, mut pos) = Self::record_types(payload)?;
        let Some(&t) = types.get(column) else { return Ok(Value::Null) };
        for &t in &types[..column] {
            pos = pos.checked_add(Self::value_len(t)).filter(|&end| end <= payload.len()).ok_or_else(|| bad("truncated record"))?;
        }
        Ok(Self::value(payload, t, pos)?.0)
    }

    /// The schema: `(type, name, table, root page, sql)` of every table, index, view and trigger.
    pub(crate) fn schema(&self) -> Result<Vec<SchemaEntry>> {
        let mut master = Vec::new();
        self.scan(1, &mut master, 0)?;
        master
            .into_iter()
            .map(|(_, payload)| {
                let r = self.decode_record(&payload)?;
                let text = |i: usize| r.get(i).and_then(Value::as_str).unwrap_or_default().to_string();
                Ok((text(0), text(1), text(2), r.get(3).and_then(Value::as_i64).unwrap_or(0) as u32, text(4)))
            })
            .collect()
    }

    /// The `CREATE TABLE` statement of a table.
    pub(crate) fn table_sql(&self, name: &str) -> Option<String> {
        self.schema().ok()?.into_iter().find(|(t, n, ..)| t == "table" && n.eq_ignore_ascii_case(name)).map(|r| r.4)
    }

    /// The payload of the row `rowid` of the table b-tree rooted at `root`, found by descending
    /// the b-tree (binary searches of its interior pages), or `None` when there is no such row.
    pub(crate) fn lookup(&self, root: u32, rowid: i64) -> Result<Option<Vec<u8>>> {
        let mut n = root;
        for _ in 0..64 {
            let page = self.page(n)?;
            let h = if n == 1 { 100 } else { 0 };
            let kind = page[h];
            let cells = u16::from_be_bytes([page[h + 3], page[h + 4]]) as usize;
            let header_len = if kind == 0x05 { 12 } else { 8 };
            let cell = |c: usize| cell_pointer(page, h + header_len + 2 * c);
            match kind {
                // Interior: (left child, key) cells by increasing key; a child holds the rowids up to
                // its key, the right-most pointer the larger ones.
                0x05 => {
                    let (mut lo, mut hi) = (0, cells);
                    while lo < hi {
                        let mid = (lo + hi) / 2;
                        let off = cell(mid)?;
                        let (key, _) = varint(page, off + 4).ok_or_else(|| bad("cell"))?;
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
                        let (_, n1) = varint(page, off).ok_or_else(|| bad("cell"))?;
                        let (key, _) = varint(page, off + n1).ok_or_else(|| bad("cell"))?;
                        match key.cmp(&rowid) {
                            std::cmp::Ordering::Less => lo = mid + 1,
                            std::cmp::Ordering::Greater => hi = mid,
                            std::cmp::Ordering::Equal => {
                                let (size, n1) = varint(page, off).ok_or_else(|| bad("cell"))?;
                                let (_, n2) = varint(page, off + n1).ok_or_else(|| bad("cell"))?;
                                let size = usize::try_from(size).map_err(|_| bad("cell"))?;
                                return self.payload(page, off + n1 + n2, size).map(Some);
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

    /// The root page and `CREATE TABLE` statement of a rowid table.
    fn table_root(&self, name: &str) -> Result<(u32, String)> {
        for (kind, table, _, root, sql) in self.schema()? {
            if kind != "table" || !table.eq_ignore_ascii_case(name) {
                continue;
            }
            if root == 0 {
                return Err(bad(&format!("'{name}' is a virtual table")));
            }
            if sql.to_ascii_uppercase().split_whitespace().collect::<Vec<_>>().join(" ").contains("WITHOUT ROWID") {
                return Err(bad(&format!("'{name}' is a WITHOUT ROWID table, which is not supported")));
            }
            return Ok((root, sql));
        }
        Err(bad(&format!("no table '{name}'")))
    }

    /// The rows of a table with the given rowids (in that order; missing rowids are skipped),
    /// read by b-tree lookups rather than a scan.
    pub(crate) fn table_rows(&self, name: &str, rowids: &[i64]) -> Result<Table> {
        let (root, sql) = self.table_root(name)?;
        let columns = columns(&sql);
        let rowid_col = integer_primary_key(&sql, &columns);
        let mut rows = Vec::with_capacity(rowids.len());
        for &rowid in rowids {
            let Some(payload) = self.lookup(root, rowid)? else { continue };
            let mut v = self.decode_record(&payload)?;
            v.resize(columns.len().max(v.len()), Value::Null);
            if let Some(c) = rowid_col
                && v[c] == Value::Null
            {
                v[c] = Value::Int(rowid);
            }
            rows.push(v);
        }
        Ok(Table { columns, rows })
    }

    /// The raw rows of a table: `(rowid, values)` with a rowid alias column left NULL, as
    /// stored. For rewriting a database: invalid text is an error, not rewritten as U+FFFD.
    pub(crate) fn stored_rows(&self, root: u32) -> Result<Vec<(i64, Vec<Value>)>> {
        let mut rows = Vec::new();
        self.scan(root, &mut rows, 0)?;
        rows.into_iter().map(|(rowid, payload)| Ok((rowid, super::sqlite_repack::decode_record(&payload, self.text_encoding())?))).collect()
    }

    /// The rows of the table b-tree rooted at `root`, as [`Db::stored_rows`] gives them but
    /// decoded for reading, as [`Db::decode_record`] does.
    pub(crate) fn rows(&self, root: u32) -> Result<Vec<(i64, Vec<Value>)>> {
        let mut rows = Vec::new();
        self.scan(root, &mut rows, 0)?;
        rows.into_iter().map(|(rowid, payload)| Ok((rowid, self.decode_record(&payload)?))).collect()
    }

    /// Decodes a record for reading, its text in the database's encoding. Invalid text becomes
    /// U+FFFD, preserving the display reader's replacement policy; a malformed record is an error.
    pub(crate) fn decode_record(&self, payload: &[u8]) -> Result<Vec<Value>> {
        super::sqlite_repack::decode_record_lossy(payload, self.text_encoding())
    }

    /// The text encoding of the database: 1 for UTF-8, 2 and 3 for UTF-16.
    pub(crate) fn text_encoding(&self) -> u32 {
        text_encoding(u32::from_be_bytes([self.data[56], self.data[57], self.data[58], self.data[59]]))
    }

    /// A 32-bit header field at `offset` (`user_version` at 60, `application_id` at 68).
    pub(crate) fn header_u32(&self, offset: usize) -> u32 {
        u32::from_be_bytes([self.data[offset], self.data[offset + 1], self.data[offset + 2], self.data[offset + 3]])
    }

    /// Reads a whole table by name.
    pub(crate) fn table(&self, name: &str) -> Result<Table> {
        for (kind, table, _, root, sql) in self.schema()? {
            if kind != "table" || !table.eq_ignore_ascii_case(name) {
                continue;
            }
            if root == 0 {
                return Err(bad(&format!("'{name}' is a virtual table")));
            }
            let sql = sql.as_str();
            let columns = columns(sql);
            // An INTEGER PRIMARY KEY column is the rowid, stored as NULL.
            let rowid_col = integer_primary_key(sql, &columns);
            let without_rowid = sql.to_ascii_uppercase().split_whitespace().collect::<Vec<_>>().join(" ").contains("WITHOUT ROWID");
            if without_rowid {
                return Err(bad(&format!("'{name}' is a WITHOUT ROWID table, which is not supported")));
            }
            let mut rows = Vec::new();
            self.scan(root, &mut rows, 0)?;
            let rows = decode_table_records(&rows, |payload| self.decode_record(payload))?;
            let rows = rows
                .into_iter()
                .map(|(rowid, mut v)| {
                    v.resize(columns.len().max(v.len()), Value::Null);
                    if let Some(c) = rowid_col
                        && v[c] == Value::Null
                    {
                        v[c] = Value::Int(rowid);
                    }
                    v
                })
                .collect();
            return Ok(Table { columns, rows });
        }
        Err(bad(&format!("no table '{name}'")))
    }

    /// The rows of a table by name, as `(rowid, record)`, without decoding them.
    pub(crate) fn raw_rows(&self, name: &str) -> Result<Vec<(i64, Vec<u8>)>> {
        for (kind, table, _, root, sql) in self.schema()? {
            if kind != "table" || !table.eq_ignore_ascii_case(name) {
                continue;
            }
            if root == 0 {
                return Err(bad(&format!("'{name}' is a virtual table")));
            }
            let without_rowid = sql.to_ascii_uppercase().split_whitespace().collect::<Vec<_>>().join(" ").contains("WITHOUT ROWID");
            if without_rowid {
                return Err(bad(&format!("'{name}' is a WITHOUT ROWID table, which is not supported")));
            }
            let mut rows = Vec::new();
            self.scan(root, &mut rows, 0)?;
            return Ok(rows);
        }
        Err(bad(&format!("no table '{name}'")))
    }
}

/// The column definitions of a `CREATE TABLE` statement.
fn column_defs(sql: &str) -> Vec<String> {
    let Some(open) = sql.find('(') else { return Vec::new() };
    let body = &sql[open + 1..sql.rfind(')').unwrap_or(sql.len())];
    let mut out = Vec::new();
    let mut depth = 0;
    let mut current = String::new();
    for ch in body.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(ch);
    }
    out.push(current);
    out.into_iter()
        .map(|d| d.trim().to_string())
        .filter(|d| {
            let first = d.split_whitespace().next().unwrap_or_default().to_ascii_uppercase();
            !d.is_empty() && !matches!(first.as_str(), "CONSTRAINT" | "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN")
        })
        .collect()
}

/// Splits a column definition into its (unquoted) name and the rest: the declared type and
/// constraints.
fn split_column(def: &str) -> (String, &str) {
    let def = def.trim();
    let close = match def.chars().next() {
        Some('"') => Some('"'),
        Some('`') => Some('`'),
        Some('[') => Some(']'),
        Some('\'') => Some('\''),
        _ => None,
    };
    match close {
        Some(c) => {
            // Quoted: up to the closing quote, a doubled quote standing for one.
            let mut name = String::new();
            let mut chars = def.char_indices().skip(1).peekable();
            while let Some((i, ch)) = chars.next() {
                if ch == c {
                    if c != ']' && chars.peek().is_some_and(|&(_, n)| n == c) {
                        chars.next();
                        name.push(c);
                        continue;
                    }
                    return (name, &def[i + ch.len_utf8()..]);
                }
                name.push(ch);
            }
            (name, "")
        }
        None => {
            let end = def.find(|c: char| c.is_whitespace()).unwrap_or(def.len());
            (def[..end].to_string(), &def[end..])
        }
    }
}

/// The columns of a `CREATE TABLE` statement: names and declared types (`TEXT(10)`, `MEDIUMINT`,
/// empty when none).
pub(crate) fn column_types(sql: &str) -> Vec<(String, String)> {
    const CONSTRAINTS: [&str; 9] = ["PRIMARY", "NOT", "NULL", "UNIQUE", "CHECK", "DEFAULT", "COLLATE", "REFERENCES", "CONSTRAINT"];
    column_defs(sql)
        .iter()
        .map(|d| {
            let (name, rest) = split_column(d);
            let mut ty = String::new();
            let mut depth = 0;
            for word in rest.split_whitespace() {
                if depth == 0 && CONSTRAINTS.contains(&word.to_ascii_uppercase().as_str()) {
                    break;
                }
                depth += word.matches('(').count() as i32 - word.matches(')').count() as i32;
                if !ty.is_empty() {
                    ty.push(' ');
                }
                ty.push_str(word);
            }
            (name, ty)
        })
        .collect()
}

/// The `NOT NULL` and `UNIQUE` constraints of each column of a `CREATE TABLE` statement (in the
/// order of [`column_types`]).
pub(crate) fn column_constraints(sql: &str) -> Vec<(bool, bool)> {
    column_defs(sql)
        .iter()
        .map(|d| {
            let (_, rest) = split_column(d);
            let k = keywords(rest);
            let words: Vec<&str> = k.split_whitespace().collect();
            (words.windows(2).any(|w| w == ["NOT", "NULL"]), words.contains(&"UNIQUE"))
        })
        .collect()
}

/// The column names of a `CREATE TABLE` statement.
fn columns(sql: &str) -> Vec<String> {
    column_types(sql).into_iter().map(|(n, _)| n).collect()
}

/// The column of a `CREATE TABLE` statement that aliases the rowid.
pub(crate) fn rowid_column(sql: &str) -> Option<usize> {
    integer_primary_key(sql, &columns(sql))
}

/// The column that aliases the rowid (`INTEGER PRIMARY KEY`), as SQLite decides it: a column
/// declared `INTEGER` with a `PRIMARY KEY` constraint, or named by a table `PRIMARY KEY (column)`
/// constraint.
fn integer_primary_key(sql: &str, columns: &[String]) -> Option<usize> {
    let types = column_types(sql);
    let pk_in_def = column_defs(sql)
        .iter()
        .zip(&types)
        .position(|(d, (_, t))| t.eq_ignore_ascii_case("INTEGER") && split_column(d).1.to_ascii_uppercase().contains("PRIMARY KEY"));
    pk_in_def.or_else(|| {
        // A table constraint `PRIMARY KEY (column)` on a single INTEGER column.
        let upper = sql.to_ascii_uppercase();
        let at = upper.rfind("PRIMARY KEY")?;
        let rest = &sql[at + 11..];
        let inner = rest.trim_start().strip_prefix('(')?.split(')').next()?;
        if inner.contains(',') {
            return None;
        }
        let (name, _) = split_column(inner);
        let i = columns.iter().position(|c| c.eq_ignore_ascii_case(&name))?;
        types[i].1.eq_ignore_ascii_case("INTEGER").then_some(i)
    })
}

/// The items of a `CREATE TABLE` body: column definitions and table constraints, in order.
fn table_items(sql: &str) -> Vec<String> {
    let Some(open) = sql.find('(') else { return Vec::new() };
    let body = &sql[open + 1..sql.rfind(')').unwrap_or(sql.len())];
    let mut out = Vec::new();
    let mut depth = 0;
    let mut quote: Option<char> = None;
    let mut current = String::new();
    for ch in body.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => quote = Some(ch),
            (None, '[') => quote = Some(']'),
            (None, '(') => depth += 1,
            (None, ')') => depth -= 1,
            (None, ',') if depth == 0 => {
                out.push(std::mem::take(&mut current).trim().to_string());
                continue;
            }
            _ => {}
        }
        current.push(ch);
    }
    out.push(current.trim().to_string());
    out.retain(|s| !s.is_empty());
    out
}

/// Text with its quoted strings and parenthesized parts blanked out, uppercase: for finding
/// keywords.
fn keywords(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0;
    let mut quote: Option<char> = None;
    for ch in s.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => quote = Some(ch),
            (None, '(') => depth += 1,
            (None, ')') => depth -= 1,
            (None, c) if depth == 0 => out.push(c.to_ascii_uppercase()),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The column names in a parenthesized list such as `(a, "b c" ASC)`, with whether each has
/// only a name (no `DESC` or `COLLATE`).
fn column_list(s: &str) -> Option<Vec<(String, bool)>> {
    let open = s.find('(')?;
    let close = s.rfind(')')?;
    let inner = s.get(open + 1..close)?;
    Some(
        inner
            .split(',')
            .map(|item| {
                let (name, rest) = split_column(item);
                let rest = rest.trim().to_ascii_uppercase();
                (name, rest.is_empty() || rest == "ASC")
            })
            .collect(),
    )
}

/// The columns of a table's automatic indexes (`sqlite_autoindex_<table>_N`), in the order
/// SQLite numbers them: one per `PRIMARY KEY` (unless it is the rowid alias) and `UNIQUE`
/// constraint, column constraints and table constraints in the order they are written, an index
/// over the same columns as an earlier one being reused.
pub(crate) fn autoindex_columns(sql: &str) -> Vec<Vec<usize>> {
    let types = column_types(sql);
    let names: Vec<String> = types.iter().map(|(n, _)| n.clone()).collect();
    let find = |n: &str| names.iter().position(|c| c.eq_ignore_ascii_case(n));
    let mut out: Vec<Vec<usize>> = Vec::new();
    let add = |cols: Vec<usize>, out: &mut Vec<Vec<usize>>| {
        if !cols.is_empty() && !out.contains(&cols) {
            out.push(cols);
        }
    };
    let mut column = 0;
    for item in table_items(sql) {
        let k = keywords(&item);
        let constraint = {
            let first = k.split(' ').next().unwrap_or_default();
            matches!(first, "CONSTRAINT" | "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN")
        };
        if constraint {
            let body = match k.find("PRIMARY KEY").or_else(|| k.find("UNIQUE")) {
                Some(_) => item.as_str(),
                None => continue,
            };
            let Some(list) = column_list(body) else { continue };
            let cols: Vec<usize> = list.iter().filter_map(|(n, _)| find(n)).collect();
            let rowid = k.contains("PRIMARY KEY") && cols.len() == 1 && list[0].1 && types[cols[0]].1.eq_ignore_ascii_case("INTEGER");
            if !rowid {
                add(cols, &mut out);
            }
            continue;
        }
        let i = column;
        column += 1;
        let (_, rest) = split_column(&item);
        let rest = keywords(rest);
        // Constraints in the order they are written.
        let pk = rest.find("PRIMARY KEY");
        let unique = rest.find("UNIQUE");
        let mut found: Vec<(usize, bool)> = Vec::new();
        if let Some(p) = pk {
            found.push((p, true));
        }
        if let Some(u) = unique {
            found.push((u, false));
        }
        found.sort();
        for (at, is_pk) in found {
            if is_pk {
                let desc = rest[at..].starts_with("PRIMARY KEY DESC");
                if types.get(i).is_some_and(|(_, t)| t.eq_ignore_ascii_case("INTEGER")) && !desc {
                    continue;
                }
            }
            add(vec![i], &mut out);
        }
    }
    out
}

/// The columns a `CREATE [UNIQUE] INDEX name ON table (columns)` statement indexes, for an index
/// on plain columns (no expressions, `DESC`, `COLLATE` or `WHERE`). `None` otherwise.
pub(crate) fn index_columns(sql: &str, table_sql: &str) -> Option<Vec<usize>> {
    let names: Vec<String> = column_types(table_sql).into_iter().map(|(n, _)| n).collect();
    let k = keywords(sql);
    if k.contains(" WHERE ") {
        return None;
    }
    let list = column_list(sql)?;
    list.iter().map(|(n, plain)| if *plain { names.iter().position(|c| c.eq_ignore_ascii_case(n)) } else { None }).collect()
}

/// The constant `DEFAULT` of each column of a `CREATE TABLE` statement: a number, a string or
/// NULL (`None` for columns without one, or whose default is an expression).
pub(crate) fn column_defaults(sql: &str) -> Vec<Option<Value>> {
    column_defs(sql)
        .iter()
        .map(|d| {
            let (_, rest) = split_column(d);
            let upper = rest.to_ascii_uppercase();
            let at = keywords(rest).find("DEFAULT").and(upper.find("DEFAULT"))?;
            let value = rest[at + 7..].trim_start();
            if let Some(s) = value.strip_prefix('\'') {
                let mut out = String::new();
                let mut chars = s.chars().peekable();
                while let Some(c) = chars.next() {
                    if c == '\'' {
                        if chars.peek() == Some(&'\'') {
                            chars.next();
                            out.push(c);
                            continue;
                        }
                        return Some(Value::Text(out));
                    }
                    out.push(c);
                }
                return None;
            }
            let token: String = value.chars().take_while(|c| !c.is_whitespace() && *c != ',').collect();
            if token.eq_ignore_ascii_case("NULL") {
                return Some(Value::Null);
            }
            if let Ok(i) = token.parse::<i64>() {
                return Some(Value::Int(i));
            }
            token.parse::<f64>().ok().map(Value::Real)
        })
        .collect()
}

/// Hostile SQLite page layouts shared by the readers' and the editor's tests.
#[cfg(test)]
pub(crate) mod fixtures {
    /// A database of `pages` zeroed 512-byte pages behind an SQLite header.
    pub(crate) fn database(pages: usize) -> Vec<u8> {
        let mut b = vec![0u8; 512 * pages];
        b[..16].copy_from_slice(b"SQLite format 3\0");
        b[16..18].copy_from_slice(&512u16.to_be_bytes());
        (b[18], b[19], b[21], b[22], b[23]) = (1, 1, 64, 32, 32);
        b[28..32].copy_from_slice(&(pages as u32).to_be_bytes());
        b[44..48].copy_from_slice(&4u32.to_be_bytes());
        b[56..60].copy_from_slice(&1u32.to_be_bytes());
        b
    }

    /// Makes page `n` an interior table page whose one cell child and right-most pointer both
    /// name `child`.
    pub(crate) fn interior(b: &mut [u8], n: usize, child: u32) {
        let (page, h) = ((n - 1) * 512, if n == 1 { 100 } else { 0 });
        b[page + h] = 0x05;
        b[page + h + 3..page + h + 5].copy_from_slice(&1u16.to_be_bytes());
        b[page + h + 8..page + h + 12].copy_from_slice(&child.to_be_bytes());
        b[page + h + 12..page + h + 14].copy_from_slice(&400u16.to_be_bytes());
        b[page + 400..page + 404].copy_from_slice(&child.to_be_bytes());
        b[page + 404] = 1;
    }

    /// A b-tree rooted at page 1 of `levels` interior pages, each page's cell and right-most
    /// pointer both naming the next page, over an empty leaf: a walk that does not notice a
    /// page reached twice visits the leaf 2^levels times.
    pub(crate) fn shared_children(levels: usize) -> Vec<u8> {
        let mut b = database(levels + 1);
        for n in 1..=levels {
            interior(&mut b, n, n as u32 + 1);
        }
        b[levels * 512] = 0x0d;
        b
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{database, interior, shared_children};
    use super::*;

    /// Makes page 1 a table leaf with one cell at offset 200 of rowid 1 and payload `size`,
    /// returning where its local payload starts.
    fn leaf_cell(b: &mut [u8], size: usize) -> usize {
        b[100] = 0x0d;
        b[103..105].copy_from_slice(&1u16.to_be_bytes());
        b[108..110].copy_from_slice(&200u16.to_be_bytes());
        let mut cell = Vec::new();
        crate::drivers::vector::sqlite::varint(&mut cell, size as u64);
        cell.push(1);
        b[200..200 + cell.len()].copy_from_slice(&cell);
        200 + cell.len()
    }

    /// Page sizes that are not a power of two from 512, and reserved space leaving fewer than
    /// 480 usable bytes, are errors, not an arithmetic underflow or out-of-range page index.
    #[test]
    fn invalid_page_sizes_are_errors() {
        for (size, reserved) in [(0u16, 0u8), (2, 0), (256, 0), (513, 0), (512, 40), (512, 255)] {
            let mut b = database(2);
            b[16..18].copy_from_slice(&size.to_be_bytes());
            b[20] = reserved;
            assert!(Db::new(b).is_err(), "page size {size}, reserved {reserved}");
        }
        let mut b = database(2);
        b[20] = 32;
        assert!(Db::new(b).is_ok());
    }

    /// An overflow chain cannot be longer than the file: a payload size implying more overflow
    /// pages than it has is an error before anything is allocated for it (a 64 GiB payload over
    /// a self-referencing page would allocate until out of memory), and a chain that ends short
    /// of its payload is an error. A chain within the file's length that loops ends after the
    /// payload's page count, so it is bounded without a page set.
    #[test]
    fn overflow_chains_longer_than_the_file_are_errors() {
        // Ten pages of overflow over a page that names itself as the next one, rejected by the
        // size check before the chain is walked.
        let mut b = database(2);
        let start = leaf_cell(&mut b, 10 * 508);
        // The local part is 39 bytes for this size (`k` exceeds the 477-byte maximum, so it is
        // the minimum `m`), then the first overflow page number.
        b[start + 39..start + 43].copy_from_slice(&2u32.to_be_bytes());
        b[512..516].copy_from_slice(&2u32.to_be_bytes());
        assert!(Db::new(b).unwrap().leaf_rows(1).is_err());

        // Two pages of overflow within the file's length, passing the size check, but the chain
        // ends after one: the walk stops short of the payload. The local part is 92 bytes here
        // (`k` is 39 + 1069 % 508).
        let mut b = database(3);
        let start = leaf_cell(&mut b, 92 + 2 * 508);
        b[start + 92..start + 96].copy_from_slice(&2u32.to_be_bytes());
        assert!(Db::new(b).unwrap().leaf_rows(1).is_err());

        // The same, with the chain pointing past the file's last page.
        let mut b = database(3);
        let start = leaf_cell(&mut b, 92 + 2 * 508);
        b[start + 92..start + 96].copy_from_slice(&2u32.to_be_bytes());
        b[512..516].copy_from_slice(&9u32.to_be_bytes());
        assert!(Db::new(b).unwrap().leaf_rows(1).is_err());

        // A chain that holds the payload reads it.
        let mut b = database(3);
        let start = leaf_cell(&mut b, 92 + 2 * 508);
        b[start + 92..start + 96].copy_from_slice(&2u32.to_be_bytes());
        b[512..516].copy_from_slice(&3u32.to_be_bytes());
        assert_eq!(Db::new(b).unwrap().leaf_rows(1).unwrap()[0].1.len(), 92 + 2 * 508);
    }

    /// Interior pages that share children (each page's cell and right-most pointer naming the
    /// next page) are an error: walking them would visit 2^depth leaves, so that a 31 KB file
    /// ran forever. Five levels here would visit the leaf 32 times.
    #[test]
    fn b_tree_pages_reached_twice_are_errors() {
        let db = Db::new(shared_children(5)).unwrap();
        let error = db.schema().unwrap_err();
        assert!(error.to_string().contains("reached twice"), "{error}");
        assert!(db.raw_rows("t").is_err());
    }

    /// Cell pointers and cell offsets past the end of a page are errors, not slice panics.
    #[test]
    fn cell_offsets_past_the_page_are_errors() {
        // An interior cell at the page's last bytes.
        let mut b = database(3);
        interior(&mut b, 1, 2);
        b[112..114].copy_from_slice(&510u16.to_be_bytes());
        b[512] = 0x0d;
        let db = Db::new(b).unwrap();
        assert!(db.schema().is_err());
        assert!(db.lookup(1, 1).is_err());
        // A leaf with more cell pointers than its page holds.
        let mut b = database(2);
        b[100] = 0x0d;
        b[103..105].copy_from_slice(&u16::MAX.to_be_bytes());
        let db = Db::new(b).unwrap();
        assert!(db.schema().is_err());
        assert!(db.lookup(1, 1).is_err());
    }

    /// A record whose header declares values longer than the record is an error: an earlier
    /// column's length once moved the next column's start past the record, a slice panic.
    #[test]
    fn records_running_past_their_end_are_errors() {
        // A header of three bytes: a 57-byte BLOB, then a one-byte integer; seven value bytes.
        let record = [3u8, 12 + 2 * 57, 1, 0xde, 0xad, 0xbe, 0xef, 0xca, 0xfe, 7];
        assert!(Db::record_column(&record, 1).is_err());
        assert!(Db::record_column(&record, 0).is_err());
        assert!(Db::record_values(&record).is_err());
        // Serial types whose lengths would overflow a sum of them.
        let mut record = vec![0u8];
        for _ in 0..3 {
            crate::drivers::vector::sqlite::varint(&mut record, i64::MAX as u64);
        }
        record[0] = record.len() as u8;
        assert!(Db::record_column(&record, 2).is_err());
        assert!(Db::record_values(&record).is_err());
        // A header longer than the record.
        assert!(Db::record_values(&[9, 1, 1]).is_err());
        // A record that holds its values decodes.
        let record = [3u8, 12 + 2 * 2, 1, 0xab, 0xcd, 7];
        assert_eq!(Db::record_column(&record, 1).unwrap(), Value::Int(7));
        assert_eq!(Db::record_values(&record).unwrap(), [Value::Blob(vec![0xab, 0xcd]), Value::Int(7)]);
        assert_eq!(Db::record_column(&record, 2).unwrap(), Value::Null);
    }

    #[test]
    fn display_reads_replace_invalid_text_but_rewrites_remain_strict() {
        mod fixtures {
            include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/sqlite_encodings/fixtures.rs"));
        }
        for (name, bytes) in fixtures::DATABASES.iter().filter(|(name, _)| name.ends_with("-invalid")) {
            let db = Db::new(bytes.to_vec()).unwrap();
            let root = db.schema().unwrap().iter().find(|e| e.1 == "couches_é水").unwrap().3;
            assert_eq!(db.rows(root).unwrap()[1].1[2], Value::Text("\u{fffd}".into()), "{name}");
            assert!(db.stored_rows(root).is_err(), "{name}");
        }
    }

    #[test]
    fn table_record_errors_stop_before_later_batches_in_row_order() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let rows: Vec<_> = (0i64..600).map(|i| (i, i.to_be_bytes().to_vec())).collect();
        let calls = AtomicUsize::new(0);
        let error = decode_table_records(&rows, |bytes| {
            calls.fetch_add(1, Ordering::Relaxed);
            let row = i64::from_be_bytes(bytes.try_into().unwrap());
            if row == 2 || row == 100 {
                return Err(bad(&format!("row {row}")));
            }
            Ok(vec![Value::Int(row)])
        })
        .unwrap_err();
        assert!(error.to_string().contains("row 2"));
        assert_eq!(calls.load(Ordering::Relaxed), 256);
        let decoded = decode_table_records(&rows, |bytes| Ok(vec![Value::Int(i64::from_be_bytes(bytes.try_into().unwrap()))])).unwrap();
        assert_eq!(decoded.len(), rows.len());
        for (id, values) in decoded {
            assert_eq!(values, [Value::Int(id)]);
        }
    }

    #[test]
    fn parses_quoted_columns_and_types() {
        let sql = r#"CREATE TABLE "a b" ( "fid" INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, "geom" MULTIPOLYGON, "my ""x"" col" TEXT(10) NOT NULL DEFAULT 'a', n MEDIUMINT, d DATETIME DEFAULT (strftime('%Y','now')))"#;
        let t = column_types(sql);
        assert_eq!(
            t,
            vec![
                ("fid".to_string(), "INTEGER".to_string()),
                ("geom".into(), "MULTIPOLYGON".into()),
                ("my \"x\" col".into(), "TEXT(10)".into()),
                ("n".into(), "MEDIUMINT".into()),
                ("d".into(), "DATETIME".into())
            ]
        );
        assert_eq!(integer_primary_key(sql, &columns(sql)), Some(0));
        let sql = "CREATE TABLE t (id INTEGER NOT NULL, v TEXT, CONSTRAINT pk PRIMARY KEY (id))";
        assert_eq!(integer_primary_key(sql, &columns(sql)), Some(0));
    }

    #[test]
    fn finds_automatic_indexes_as_sqlite() {
        // GDAL's GeoPackage tables.
        let contents = "CREATE TABLE gpkg_contents (table_name TEXT NOT NULL PRIMARY KEY,data_type TEXT NOT NULL,identifier TEXT UNIQUE,description TEXT DEFAULT '',last_change DATETIME NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),min_x DOUBLE, min_y DOUBLE,max_x DOUBLE, max_y DOUBLE,srs_id INTEGER,CONSTRAINT fk_gc_r_srs_id FOREIGN KEY (srs_id) REFERENCES gpkg_spatial_ref_sys(srs_id))";
        assert_eq!(autoindex_columns(contents), vec![vec![0], vec![2]]);
        let geometry_columns = "CREATE TABLE gpkg_geometry_columns (table_name TEXT NOT NULL,column_name TEXT NOT NULL,geometry_type_name TEXT NOT NULL,srs_id INTEGER NOT NULL,z TINYINT NOT NULL,m TINYINT NOT NULL,CONSTRAINT pk_geom_cols PRIMARY KEY (table_name, column_name),CONSTRAINT uk_gc_table_name UNIQUE (table_name),CONSTRAINT fk_gc_tn FOREIGN KEY (table_name) REFERENCES gpkg_contents(table_name),CONSTRAINT fk_gc_srs FOREIGN KEY (srs_id) REFERENCES gpkg_spatial_ref_sys (srs_id))";
        assert_eq!(autoindex_columns(geometry_columns), vec![vec![0, 1], vec![0]]);
        let metadata =
            "CREATE TABLE gpkg_metadata (id INTEGER CONSTRAINT m_pk PRIMARY KEY ASC NOT NULL,md_scope TEXT NOT NULL DEFAULT 'dataset')";
        assert!(autoindex_columns(metadata).is_empty());
        let tiles = "CREATE TABLE \"t\" (id INTEGER PRIMARY KEY AUTOINCREMENT,zoom_level INTEGER NOT NULL,tile_column INTEGER NOT NULL,tile_row INTEGER NOT NULL,tile_data BLOB NOT NULL,UNIQUE (zoom_level, tile_column, tile_row))";
        assert_eq!(autoindex_columns(tiles), vec![vec![1, 2, 3]]);
        assert_eq!(autoindex_columns("CREATE TABLE t (a TEXT PRIMARY KEY, b TEXT UNIQUE, UNIQUE (a))"), vec![vec![0], vec![1]]);
        assert_eq!(index_columns("CREATE INDEX i ON t (b, \"a\")", "CREATE TABLE t (a, b)"), Some(vec![1, 0]));
        assert_eq!(index_columns("CREATE INDEX i ON t (b DESC)", "CREATE TABLE t (a, b)"), None);
        assert_eq!(
            column_defaults("CREATE TABLE t (a TEXT DEFAULT 'it''s', b INTEGER DEFAULT 3, c REAL DEFAULT (1+1), d TEXT)"),
            vec![Some(Value::Text("it's".into())), Some(Value::Int(3)), None, None]
        );
    }

    #[test]
    fn parses_column_lists() {
        assert_eq!(
            columns(
                "CREATE TABLE t (id INTEGER PRIMARY KEY AUTOINCREMENT, zoom_level INTEGER NOT NULL, tile_data BLOB, UNIQUE (zoom_level))"
            ),
            vec!["id", "zoom_level", "tile_data"]
        );
        assert_eq!(varint(&[0x81, 0x00], 0), Some((128, 2)));
    }
}
