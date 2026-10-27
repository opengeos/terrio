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
            p => p as usize,
        };
        let usable = page_size - data[20] as usize;
        Ok(Self { data, page_size, usable })
    }

    fn page(&self, n: u32) -> Result<&[u8]> {
        let start = (n as usize).checked_sub(1).ok_or_else(|| bad("page 0"))? * self.page_size;
        self.data.get(start..start + self.page_size).ok_or_else(|| bad("page out of range"))
    }

    /// Every row of the b-tree rooted at `root`, as `(rowid, payload)`. The interior pages are
    /// walked first, then the leaf pages are decoded in parallel.
    fn scan(&self, root: u32, out: &mut Vec<(i64, Vec<u8>)>, depth: usize) -> Result<()> {
        let mut leaves = Vec::new();
        let failure = self.leaves(root, &mut leaves, depth).err();
        let rows = crate::par::map_range(leaves.len(), |i| self.leaf_rows(leaves[i]));
        // The first error in page order, as a sequential scan would meet it.
        for r in rows {
            out.extend(r?);
        }
        failure.map_or(Ok(()), Err)
    }

    /// The pages holding rows of the b-tree rooted at `root`, in order, until an error.
    fn leaves(&self, root: u32, out: &mut Vec<u32>, depth: usize) -> Result<()> {
        if depth > 64 {
            return Err(bad("b-tree too deep"));
        }
        let page = self.page(root)?;
        let h = if root == 1 { 100 } else { 0 };
        if page[h] != 0x05 {
            // A leaf, or a page of another type that decoding it reports.
            out.push(root);
            return Ok(());
        }
        let cells = u16::from_be_bytes([page[h + 3], page[h + 4]]) as usize;
        for c in 0..cells {
            let p = h + 12 + 2 * c;
            let off = u16::from_be_bytes([page[p], page[p + 1]]) as usize;
            let child = u32::from_be_bytes(page[off..off + 4].try_into().map_err(|_| bad("cell"))?);
            self.leaves(child, out, depth + 1)?;
        }
        let right = u32::from_be_bytes(page[h + 8..h + 12].try_into().map_err(|_| bad("page"))?);
        self.leaves(right, out, depth + 1)
    }

    /// The first stored row of a table, descending only its leftmost b-tree branch.
    pub(crate) fn first_row(&self, mut n: u32) -> Result<Option<(i64, Vec<Value>)>> {
        for _ in 0..64 {
            let page = self.page(n)?;
            let h = if n == 1 { 100 } else { 0 };
            match page.get(h).copied() {
                Some(0x0d) => return Ok(self.leaf_rows(n)?.into_iter().next().map(|(id, payload)| (id, Self::record_values(&payload)))),
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
            let p = h + 8 + 2 * c;
            let off = u16::from_be_bytes([page[p], page[p + 1]]) as usize;
            let (size, n1) = varint(page, off).ok_or_else(|| bad("cell"))?;
            let (rowid, n2) = varint(page, off + n1).ok_or_else(|| bad("cell"))?;
            out.push((rowid, self.payload(page, off + n1 + n2, size as usize)?));
        }
        Ok(out)
    }

    /// A cell's payload, following overflow pages.
    fn payload(&self, page: &[u8], start: usize, size: usize) -> Result<Vec<u8>> {
        let u = self.usable;
        let x = u - 35;
        if size <= x {
            return page.get(start..start + size).map(<[u8]>::to_vec).ok_or_else(|| bad("payload"));
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
        Ok(out)
    }

    /// Decodes a record (a row's payload).
    pub(crate) fn record_values(payload: &[u8]) -> Vec<Value> {
        Self::record(payload)
    }

    /// The serial types of a record's columns, and where its values start.
    fn record_types(payload: &[u8]) -> (Vec<i64>, usize) {
        let Some((header_len, n)) = varint(payload, 0) else { return (Vec::new(), 0) };
        let mut types = Vec::new();
        let mut i = n;
        while i < header_len as usize {
            let Some((t, k)) = varint(payload, i) else { break };
            types.push(t);
            i += k;
        }
        (types, header_len as usize)
    }

    /// Decodes a value of serial type `t` at `pos`: the value and its length.
    fn value(payload: &[u8], t: i64, pos: usize) -> (Value, usize) {
        let int = |len: usize, pos: usize| -> i64 {
            let b = &payload[pos..(pos + len).min(payload.len())];
            let mut v: i64 = if b.first().is_some_and(|x| x & 0x80 != 0) { -1 } else { 0 };
            for x in b {
                v = (v << 8) | *x as i64;
            }
            v
        };
        match t {
            0 => (Value::Null, 0),
            1..=4 => (Value::Int(int(t as usize, pos)), t as usize),
            5 => (Value::Int(int(6, pos)), 6),
            6 => (Value::Int(int(8, pos)), 8),
            7 => (Value::Real(f64::from_bits(int(8, pos) as u64)), 8),
            8 => (Value::Int(0), 0),
            9 => (Value::Int(1), 0),
            t if t >= 12 && t % 2 == 0 => {
                let l = (t as usize - 12) / 2;
                (Value::Blob(payload.get(pos..pos + l).unwrap_or_default().to_vec()), l)
            }
            t if t >= 13 => {
                let l = (t as usize - 13) / 2;
                (Value::Text(String::from_utf8_lossy(payload.get(pos..pos + l).unwrap_or_default()).into_owned()), l)
            }
            _ => (Value::Null, 0),
        }
    }

    /// The length of a value of serial type `t`.
    fn value_len(t: i64) -> usize {
        match t {
            1..=4 => t as usize,
            5 => 6,
            6 | 7 => 8,
            t if t >= 12 => (t as usize - 12) / 2,
            _ => 0,
        }
    }

    /// Decodes a record.
    fn record(payload: &[u8]) -> Vec<Value> {
        let (types, mut pos) = Self::record_types(payload);
        let mut out = Vec::with_capacity(types.len());
        for t in types {
            let (value, len) = Self::value(payload, t, pos);
            pos += len;
            out.push(value);
        }
        out
    }

    /// Decodes one column of a record, as [`Db::table`] reads it (`Null` past the record's last
    /// column; the rowid alias column is not filled in).
    pub(crate) fn record_column(payload: &[u8], column: usize) -> Value {
        let (types, mut pos) = Self::record_types(payload);
        let Some(&t) = types.get(column) else { return Value::Null };
        for &t in &types[..column] {
            pos += Self::value_len(t);
        }
        Self::value(payload, t, pos).0
    }

    /// The schema: `(type, name, table, root page, sql)` of every table, index, view and trigger.
    pub(crate) fn schema(&self) -> Result<Vec<SchemaEntry>> {
        let mut master = Vec::new();
        self.scan(1, &mut master, 0)?;
        Ok(master
            .into_iter()
            .map(|(_, payload)| {
                let r = Self::record(&payload);
                let text = |i: usize| r.get(i).and_then(Value::as_str).unwrap_or_default().to_string();
                (text(0), text(1), text(2), r.get(3).and_then(Value::as_i64).unwrap_or(0) as u32, text(4))
            })
            .collect())
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
            let cell = |c: usize| -> Result<usize> {
                let p = h + header_len + 2 * c;
                Ok(u16::from_be_bytes([*page.get(p).ok_or_else(|| bad("cell"))?, *page.get(p + 1).ok_or_else(|| bad("cell"))?]) as usize)
            };
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
                                return self.payload(page, off + n1 + n2, size as usize).map(Some);
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
            let mut v = Self::record(&payload);
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
    /// stored. For rewriting a database.
    pub(crate) fn stored_rows(&self, root: u32) -> Result<Vec<(i64, Vec<Value>)>> {
        let mut rows = Vec::new();
        self.scan(root, &mut rows, 0)?;
        Ok(rows.into_iter().map(|(rowid, payload)| (rowid, Self::record(&payload))).collect())
    }

    /// The text encoding of the database: 1 for UTF-8, 2 and 3 for UTF-16.
    pub(crate) fn text_encoding(&self) -> u32 {
        u32::from_be_bytes([self.data[56], self.data[57], self.data[58], self.data[59]])
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
            let rows = crate::par::map_range(rows.len(), |i| (rows[i].0, Self::record(&rows[i].1)));
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

#[cfg(test)]
mod tests {
    use super::*;

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
