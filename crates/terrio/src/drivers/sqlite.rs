//! A read-only SQLite reader: rowid-table scans, bounded row visitors and index lookups
//! for GeoPackage features and tile pyramids.

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

/// A callback borrowing one encoded table record.
type TableRecordVisitor<'a> = dyn FnMut(usize, i64, &[u8]) -> Result<bool> + 'a;

/// A borrowed record whose attributes are decoded only after selection.
pub(crate) struct TableRow<'a> {
    db: &'a Db,
    payload: &'a [u8],
    rowid: i64,
    rowid_col: Option<usize>,
    columns: usize,
}

impl TableRow<'_> {
    /// Materialize all values and restore an INTEGER PRIMARY KEY alias.
    pub(crate) fn values(&self) -> Result<Vec<Value>> {
        let mut values = self.db.decode_record(self.payload)?;
        values.resize(self.columns.max(values.len()), Value::Null);
        if let Some(c) = self.rowid_col
            && values[c] == Value::Null
        {
            values[c] = Value::Int(self.rowid);
        }
        Ok(values)
    }

    /// Borrow one blob without decoding or allocating any other field, in any text encoding.
    pub(crate) fn blob(&self, column: usize) -> Result<Option<&[u8]>> {
        let (header, mut cursor) = varint(self.payload, 0).ok_or_else(|| bad("record header"))?;
        let header =
            usize::try_from(header).ok().filter(|&n| n >= cursor && n <= self.payload.len()).ok_or_else(|| bad("record header"))?;
        let mut offset = header;
        let mut index = 0;
        while cursor < header {
            let (kind, size) = varint(self.payload, cursor).filter(|&(_, n)| n <= header - cursor).ok_or_else(|| bad("record type"))?;
            let len =
                if kind >= 12 { usize::try_from((kind - 12) / 2).map_err(|_| bad("record value size"))? } else { Db::value_len(kind) };
            if index == column {
                if kind < 12 || kind % 2 != 0 {
                    return Ok(None);
                }
                let end = offset.checked_add(len).ok_or_else(|| bad("record value"))?;
                return self.payload.get(offset..end).map(Some).ok_or_else(|| bad("record value"));
            }
            offset = offset.checked_add(len).ok_or_else(|| bad("record value"))?;
            cursor += size;
            index += 1;
        }
        Ok(None)
    }
}

/// State of an ordered, early-stopping rowid-table traversal.
struct TableScan<'a> {
    skip: usize,
    remaining: Option<usize>,
    ordinal: usize,
    visited: PageSet,
    visit: &'a mut TableRecordVisitor<'a>,
}

/// A schema entry: `(type, name, table, root page, sql)`.
pub(crate) type SchemaEntry = (String, String, String, u32, String);

/// An SQLite database file in memory.
pub(crate) struct Db {
    data: Bytes,
    page_size: usize,
    usable: usize,
    #[cfg(test)]
    decoded_records: std::sync::atomic::AtomicUsize,
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
        // `pages` is bounded by a file's length or a `u32`, so its words fit in memory.
        PageSet { bits: vec![0; usize::try_from(pages.div_ceil(64)).expect("page count fits in memory")], pages }
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

/// Orders two values as SQLite's BINARY collation does in a database whose text is in
/// `encoding` (1 for UTF-8, 2 and 3 for UTF-16LE and UTF-16BE): NULL, then numbers (an integer
/// and a real compared exactly), text (by its encoded bytes) and blobs.
pub(crate) fn compare_values(a: &Value, b: &Value, encoding: u32) -> std::cmp::Ordering {
    let rank = |v: &Value| match v {
        Value::Null => 0,
        Value::Int(_) | Value::Real(_) => 1,
        Value::Text(_) => 2,
        Value::Blob(_) => 3,
    };
    match (a, b) {
        (Value::Int(p), Value::Int(q)) => p.cmp(q),
        // -0.0 and 0.0 are equal, as in SQLite and in `compare_int_real`; NaN is stored as NULL.
        (Value::Real(p), Value::Real(q)) => p.partial_cmp(q).unwrap_or(std::cmp::Ordering::Equal),
        (Value::Int(p), Value::Real(q)) => compare_int_real(*p, *q),
        (Value::Real(p), Value::Int(q)) => compare_int_real(*q, *p).reverse(),
        (Value::Text(p), Value::Text(q)) => match encoding {
            2 => p.encode_utf16().flat_map(u16::to_le_bytes).cmp(q.encode_utf16().flat_map(u16::to_le_bytes)),
            3 => p.encode_utf16().flat_map(u16::to_be_bytes).cmp(q.encode_utf16().flat_map(u16::to_be_bytes)),
            _ => p.as_bytes().cmp(q.as_bytes()),
        },
        (Value::Blob(p), Value::Blob(q)) => p.cmp(q),
        _ => rank(a).cmp(&rank(b)),
    }
}

/// Orders an integer against a real without rounding the integer, as SQLite does: integers past
/// 2^53 are not made equal to a nearby real.
fn compare_int_real(i: i64, r: f64) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    // 2^63: every i64 is below it, and at least -2^63.
    const LIMIT: f64 = 9_223_372_036_854_775_808.0;
    if r.is_nan() {
        return Ordering::Greater;
    }
    if r >= LIMIT {
        return Ordering::Less;
    }
    if r < -LIMIT {
        return Ordering::Greater;
    }
    let floor = r.floor();
    match i.cmp(&(floor as i64)) {
        Ordering::Equal if floor < r => Ordering::Less,
        o => o,
    }
}

/// Orders an index record's leading values against `prefix`.
fn compare_prefix(record: &[Value], prefix: &[Value], encoding: u32) -> std::cmp::Ordering {
    prefix
        .iter()
        .enumerate()
        .map(|(i, p)| compare_values(record.get(i).unwrap_or(&Value::Null), p, encoding))
        .find(|o| o.is_ne())
        .unwrap_or(std::cmp::Ordering::Equal)
}

/// The values of `columns` decoded from the part of a record kept on its page, `None` when the
/// header or one of them lies past it (or is text, when the database's text is not UTF-8).
fn local_columns(local: &[u8], columns: &[usize], utf8: bool) -> Option<Vec<Value>> {
    let (header_len, n) = varint(local, 0)?;
    let header_len = usize::try_from(header_len).ok().filter(|&len| len >= n && len <= local.len())?;
    let (mut types, mut i) = (Vec::new(), n);
    while i < header_len {
        let (t, k) = varint(local, i).filter(|&(_, k)| i + k <= header_len)?;
        types.push(t);
        i += k;
    }
    columns
        .iter()
        .map(|&c| {
            let Some(&t) = types.get(c) else { return Some(Value::Null) };
            if !utf8 && t >= 13 && t % 2 == 1 {
                return None;
            }
            let pos = types[..c].iter().try_fold(header_len, |pos, &t| pos.checked_add(Db::value_len(t)))?;
            Db::value(local, t, pos).ok().map(|(v, _)| v)
        })
        .collect()
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
        Ok(Self {
            data,
            page_size,
            usable,
            #[cfg(test)]
            decoded_records: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    /// Whether the database is a file mapped into memory, not bytes read.
    #[cfg(test)]
    pub(crate) fn is_mapped(&self) -> bool {
        #[cfg(not(target_family = "wasm"))]
        {
            matches!(self.data, Bytes::Mapped(_))
        }
        #[cfg(target_family = "wasm")]
        false
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

    /// Walk only as far as the requested rows. Skipped leaf cells do not read their payloads
    /// or overflow chains, and reaching a limit does not visit subsequent tree branches.
    fn walk_table(&self, root: u32, scan: &mut TableScan<'_>, depth: usize) -> Result<bool> {
        if depth > 64 {
            return Err(bad("b-tree too deep"));
        }
        scan.visited.insert(root)?;
        let page = self.page(root)?;
        let h = if root == 1 { 100 } else { 0 };
        let cells = u16::from_be_bytes([page[h + 3], page[h + 4]]) as usize;
        match page[h] {
            0x05 => {
                for c in 0..cells {
                    let off = cell_pointer(page, h + 12 + 2 * c)?;
                    let child = u32::from_be_bytes(page.get(off..off + 4).and_then(|b| b.try_into().ok()).ok_or_else(|| bad("cell"))?);
                    if !self.walk_table(child, scan, depth + 1)? {
                        return Ok(false);
                    }
                }
                let right = u32::from_be_bytes(page[h + 8..h + 12].try_into().map_err(|_| bad("page"))?);
                self.walk_table(right, scan, depth + 1)
            }
            0x0d => {
                let skipped = scan.skip.min(cells);
                scan.skip -= skipped;
                scan.ordinal += skipped;
                for c in skipped..cells {
                    let off = cell_pointer(page, h + 8 + 2 * c)?;
                    let (size, n1) = varint(page, off).ok_or_else(|| bad("cell"))?;
                    let (rowid, n2) = varint(page, off + n1).ok_or_else(|| bad("cell"))?;
                    let size = usize::try_from(size).map_err(|_| bad("cell"))?;
                    let payload = self.payload(page, off + n1 + n2, size)?;
                    let ordinal = scan.ordinal;
                    scan.ordinal += 1;
                    if let Some(left) = &mut scan.remaining {
                        *left -= 1;
                    }
                    if !(scan.visit)(ordinal, rowid, &payload)? || scan.remaining == Some(0) {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            _ => Err(bad("unsupported page type")),
        }
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

    /// A table cell's payload, following overflow pages.
    fn payload(&self, page: &[u8], start: usize, size: usize) -> Result<Vec<u8>> {
        self.payload_within(page, start, size, self.usable - 35)
    }

    /// How many bytes of a payload of `size` bytes a cell keeps on its page, when the most a cell
    /// keeps is `x` (`usable - 35` in table leaves, less in indexes).
    fn local_size(&self, size: usize, x: usize) -> usize {
        let u = self.usable;
        if size <= x {
            return size;
        }
        let m = ((u - 12) * 32 / 255) - 23;
        let k = m + (size - m) % (u - 4);
        if k <= x { k } else { m }
    }

    /// A cell's payload, following overflow pages, when the most a cell keeps on its page is `x`.
    fn payload_within(&self, page: &[u8], start: usize, size: usize, x: usize) -> Result<Vec<u8>> {
        let u = self.usable;
        if size <= x {
            return start.checked_add(size).and_then(|end| page.get(start..end)).map(<[u8]>::to_vec).ok_or_else(|| bad("payload"));
        }
        // Each overflow page holds `u - 4` bytes; a chain longer than the file is corrupt, and
        // its declared size must not size the allocation.
        if size / (u - 4) > self.pages() {
            return Err(bad("payload"));
        }
        let local = self.local_size(size, x);
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
        let (types, mut pos) = Self::record_types(payload)?;
        let mut out = Vec::with_capacity(types.len());
        for t in types {
            let (value, len) = Self::value(payload, t, pos)?;
            pos += len;
            out.push(value);
        }
        Ok(out)
    }

    /// Checks a record's layout, as decoding it would, without decoding its values.
    pub(crate) fn check_record(payload: &[u8]) -> Result<()> {
        Self::record_types(payload).map(drop)
    }

    /// The serial types of a record's columns, and where its values start. An error when the
    /// header is incomplete or runs past the record, or the values it declares do.
    fn record_types(payload: &[u8]) -> Result<(Vec<i64>, usize)> {
        if payload.is_empty() {
            return Ok((Vec::new(), 0));
        }
        let (header_len, n) = varint(payload, 0).ok_or_else(|| bad("record header"))?;
        let header_len =
            usize::try_from(header_len).ok().filter(|&len| len >= n && len <= payload.len()).ok_or_else(|| bad("record header"))?;
        let mut types = Vec::new();
        let (mut i, mut end) = (n, header_len);
        while i < header_len {
            let (t, k) = varint(payload, i).filter(|&(_, k)| i + k <= header_len).ok_or_else(|| bad("record header"))?;
            end = end.checked_add(Self::value_len(t)).filter(|&end| end <= payload.len()).ok_or_else(|| bad("truncated record"))?;
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

    /// Decodes one column of a record, as [`Db::table`] reads it (`Null` past the record's last
    /// column; the rowid alias column is not filled in). A record whose header is incomplete, or
    /// any of whose values runs past its end, is an error, whichever column is asked for.
    pub(crate) fn record_column(payload: &[u8], column: usize) -> Result<Value> {
        let (types, mut pos) = Self::record_types(payload)?;
        let Some(&t) = types.get(column) else { return Ok(Value::Null) };
        // `record_types` has checked that every value lies within the record.
        for &t in &types[..column] {
            pos += Self::value_len(t);
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
    pub(crate) fn table_root(&self, name: &str) -> Result<(u32, String)> {
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

    /// Visits in order the records of the index b-tree rooted at `root` whose leading values equal
    /// `prefix` (a record holds the indexed columns, then the rowid), until `visit` returns
    /// `false`. Only the branches that can hold such records are read: finding one row of a large
    /// table reads a page per level of its index.
    pub(crate) fn index_prefix(&self, root: u32, prefix: &[Value], visit: &mut dyn FnMut(Vec<Value>) -> bool) -> Result<()> {
        self.index_walk(root, prefix, visit, &mut std::collections::HashSet::new(), 0).map(drop)
    }

    /// The decoded record of the index cell at `off`.
    fn index_cell(&self, page: &[u8], off: usize, interior: bool) -> Result<Vec<Value>> {
        let start = if interior { off + 4 } else { off };
        let (size, n) = varint(page, start).ok_or_else(|| bad("cell"))?;
        let size = usize::try_from(size).map_err(|_| bad("cell"))?;
        let x = (self.usable - 12) * 64 / 255 - 23;
        self.decode_record(&self.payload_within(page, start + n, size, x)?)
    }

    /// [`Db::index_prefix`] from page `n`: `false` once the walk is over (past the prefix, or
    /// stopped by `visit`). `visited` holds the pages already read: a page reached twice is corrupt.
    fn index_walk(
        &self,
        n: u32,
        prefix: &[Value],
        visit: &mut dyn FnMut(Vec<Value>) -> bool,
        visited: &mut std::collections::HashSet<u32>,
        depth: usize,
    ) -> Result<bool> {
        use std::cmp::Ordering;
        if depth > 64 {
            return Err(bad("b-tree too deep"));
        }
        if !visited.insert(n) {
            return Err(bad("b-tree page reached twice"));
        }
        let encoding = self.text_encoding();
        let page = self.page(n)?;
        let h = if n == 1 { 100 } else { 0 };
        let interior = match page[h] {
            0x02 => true,
            0x0a => false,
            _ => return Err(bad("unsupported page type")),
        };
        let cells = u16::from_be_bytes([page[h + 3], page[h + 4]]) as usize;
        let header = if interior { 12 } else { 8 };
        let offset = |c: usize| cell_pointer(page, h + header + 2 * c);
        // The first cell not ordered before the prefix: the children left of it hold smaller keys.
        let (mut lo, mut hi) = (0, cells);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if compare_prefix(&self.index_cell(page, offset(mid)?, interior)?, prefix, encoding) == Ordering::Less {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        for c in lo..cells {
            let off = offset(c)?;
            if interior {
                let child = u32::from_be_bytes(page.get(off..off + 4).and_then(|b| b.try_into().ok()).ok_or_else(|| bad("cell"))?);
                if !self.index_walk(child, prefix, visit, visited, depth + 1)? {
                    return Ok(false);
                }
            }
            let record = self.index_cell(page, off, interior)?;
            if compare_prefix(&record, prefix, encoding) != Ordering::Equal || !visit(record) {
                return Ok(false);
            }
        }
        if interior {
            let right = u32::from_be_bytes(page[h + 8..h + 12].try_into().map_err(|_| bad("page"))?);
            return self.index_walk(right, prefix, visit, visited, depth + 1);
        }
        Ok(true)
    }

    /// Calls `f` with the rowid and the values of `columns` of each row of the table b-tree rooted
    /// at `root`, in rowid order. A row's overflow pages are read only when the columns do not all
    /// lie in the part of the row kept on its leaf page, so the leading key columns of a table of
    /// large blobs are read without the blobs.
    pub(crate) fn scan_columns(&self, root: u32, columns: &[usize], f: &mut dyn FnMut(i64, Vec<Value>)) -> Result<()> {
        let mut leaves = Vec::new();
        self.leaves(root, &mut leaves, &mut PageSet::new(self.pages() as u64), 0)?;
        let utf8 = self.text_encoding() == 1;
        for n in leaves {
            let page = self.page(n)?;
            let h = if n == 1 { 100 } else { 0 };
            if page[h] != 0x0d {
                return Err(bad("unsupported page type"));
            }
            let cells = u16::from_be_bytes([page[h + 3], page[h + 4]]) as usize;
            for c in 0..cells {
                let off = cell_pointer(page, h + 8 + 2 * c)?;
                let (size, n1) = varint(page, off).ok_or_else(|| bad("cell"))?;
                let (rowid, n2) = varint(page, off + n1).ok_or_else(|| bad("cell"))?;
                let size = usize::try_from(size).map_err(|_| bad("cell"))?;
                let start = off + n1 + n2;
                let local = start.checked_add(self.local_size(size, self.usable - 35)).and_then(|end| page.get(start..end));
                let values = match local.and_then(|l| local_columns(l, columns, utf8)) {
                    Some(values) => values,
                    None => {
                        let all = self.decode_record(&self.payload(page, start, size)?)?;
                        columns.iter().map(|&c| all.get(c).cloned().unwrap_or(Value::Null)).collect()
                    }
                };
                f(rowid, values);
            }
        }
        Ok(())
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

    /// Visit borrowed rows in table order, skipping payloads outside the range. Optional
    /// rowids restrict the traversal to those lookups (in the supplied order). The callback
    /// receives the ordinal including skipped rows (within the supplied sequence for rowid
    /// lookups), and can stop before reading another record.
    pub(crate) fn visit_table_rows(
        &self,
        name: &str,
        rowids: Option<&[i64]>,
        skip: usize,
        maximum: Option<usize>,
        visit: &mut dyn FnMut(usize, TableRow<'_>) -> Result<bool>,
    ) -> Result<()> {
        let (root, sql) = self.table_root(name)?;
        let columns = columns(&sql);
        let rowid_col = integer_primary_key(&sql, &columns);
        if maximum == Some(0) {
            return Ok(());
        }
        let mut decode =
            |ordinal, rowid, payload: &[u8]| visit(ordinal, TableRow { db: self, payload, rowid, rowid_col, columns: columns.len() });
        if let Some(ids) = rowids {
            let mut ordinal = 0;
            let mut kept = 0;
            for &rowid in ids {
                let Some(payload) = self.lookup(root, rowid)? else { continue };
                let current = ordinal;
                ordinal += 1;
                if current < skip {
                    continue;
                }
                kept += 1;
                if !decode(current, rowid, &payload)? || maximum.is_some_and(|m| kept == m) {
                    break;
                }
            }
        } else {
            let mut scan =
                TableScan { skip, remaining: maximum, ordinal: 0, visited: PageSet::new(self.pages() as u64), visit: &mut decode };
            self.walk_table(root, &mut scan, 0)?;
        }
        Ok(())
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
        #[cfg(test)]
        self.decoded_records.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        super::sqlite_repack::decode_record_lossy(payload, self.text_encoding())
    }

    /// Actual decoder calls on this database, including calls from worker threads.
    #[cfg(test)]
    pub(crate) fn decoded_record_count(&self) -> usize {
        self.decoded_records.load(std::sync::atomic::Ordering::Relaxed)
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

/// Whether column `i` of a `CREATE TABLE` statement declares a collation other than BINARY,
/// which orders its indexes otherwise than [`compare_values`] does.
pub(crate) fn column_has_collation(sql: &str, i: usize) -> bool {
    column_defs(sql).get(i).is_some_and(|d| {
        let k = keywords(d);
        k.match_indices("COLLATE").any(|(at, _)| !k[at + 7..].trim_start().starts_with("BINARY"))
    })
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
/// The items of a `CREATE TABLE` statement that make an automatic index: column definitions and
/// table constraints with `PRIMARY KEY` or `UNIQUE`.
pub(crate) fn constraint_items(sql: &str) -> Vec<String> {
    table_items(sql)
        .into_iter()
        .filter(|item| {
            let k = keywords(item);
            k.contains("PRIMARY KEY") || k.split(' ').any(|w| w == "UNIQUE")
        })
        .collect()
}

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
    // A partial index (`WHERE`, however it is spaced) lacks rows, so it is not one.
    if keywords(sql).split(|c: char| !c.is_ascii_alphanumeric() && c != '_').any(|w| w == "WHERE") {
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

    /// GDAL 3.13.3 table order is preserved without decoding skipped overflow records.
    #[test]
    fn table_pages_preserve_row_ordinals_and_primary_keys() {
        use crate::drivers::vector::sqlite as writer;
        let table = writer::Table {
            name: "records".into(),
            sql: "CREATE TABLE records(fid INTEGER PRIMARY KEY,name TEXT,payload BLOB)".into(),
            rows: (0..1800)
                .map(|i| {
                    (
                        i * 3 + 11,
                        vec![
                            writer::Value::Null,
                            writer::Value::Text(format!("café水-{i}")),
                            writer::Value::Blob(vec![i as u8; if i % 7 == 0 { 8192 } else { 3 }]),
                        ],
                    )
                })
                .collect(),
            autoindexes: vec![],
        };
        let db = Db::new(writer::write_database(&[table], &[], 0, 0).unwrap()).unwrap();
        let all = db.table("records").unwrap();
        let before_schema = db.decoded_record_count();
        db.visit_table_rows("records", None, 0, Some(0), &mut |_, _| panic!("zero limit visited a record")).unwrap();
        let schema_decodes = db.decoded_record_count() - before_schema;
        assert_eq!(schema_decodes, 1, "even a zero limit reads the one-row schema to find the table");
        for (skip, limit) in [(0, Some(0)), (1, Some(2)), (497, Some(5)), (1799, None), (1800, None), (usize::MAX, Some(usize::MAX))] {
            let mut rows = Vec::new();
            let mut ordinals = Vec::new();
            let before = db.decoded_record_count();
            db.visit_table_rows("records", None, skip, limit, &mut |ordinal, row| {
                ordinals.push(ordinal);
                rows.push(row.values()?);
                Ok(true)
            })
            .unwrap();
            let start = skip.min(all.rows.len());
            let end = limit.map_or(all.rows.len(), |m| start.saturating_add(m).min(all.rows.len()));
            assert_eq!(rows, all.rows[start..end]);
            assert_eq!(ordinals, (start..end).collect::<Vec<_>>());
            assert_eq!(db.decoded_record_count() - before - schema_decodes, end - start);
        }
        let mut rows = Vec::new();
        db.visit_table_rows("records", Some(&[11, 14, 15, 35]), 1, Some(2), &mut |_, row| {
            rows.push(row.values()?);
            Ok(true)
        })
        .unwrap();
        assert_eq!(rows, [all.rows[1].clone(), all.rows[8].clone()]);
    }

    /// Geometry projection does not materialize neighboring text, including UTF-16 databases.
    #[test]
    fn borrowed_blob_projection_leaves_other_values_undecoded() {
        use crate::drivers::vector::sqlite as writer;
        let payload = writer::record(&[
            writer::Value::Text("café水".repeat(2000)),
            writer::Value::Blob(vec![1, 2, 3, 255]),
            writer::Value::Null,
            writer::Value::Blob(Vec::new()),
        ]);
        for encoding in 1u32..=3 {
            let mut bytes = database(2);
            bytes[56..60].copy_from_slice(&encoding.to_be_bytes());
            let db = Db::new(bytes).unwrap();
            let row = TableRow { db: &db, payload: &payload, rowid: 1, rowid_col: None, columns: 4 };
            let blob = row.blob(1).unwrap().unwrap();
            assert_eq!(blob, &[1, 2, 3, 255]);
            assert!(blob.as_ptr() >= payload.as_ptr() && blob.as_ptr() < payload.as_ptr_range().end);
            assert!(row.blob(0).unwrap().is_none());
            assert!(row.blob(2).unwrap().is_none());
            assert_eq!(row.blob(3).unwrap(), Some([].as_slice()));
            assert!(row.blob(4).unwrap().is_none());
            assert_eq!(db.decoded_record_count(), 0);
            let truncated = TableRow { payload: &payload[..payload.len() - 2], ..row };
            assert!(truncated.blob(1).is_err());
        }
    }

    #[test]
    fn paged_table_walk_rejects_shared_children() {
        let db = Db::new(shared_children(4)).unwrap();
        let mut visit = |_, _, _: &[u8]| Ok(true);
        let mut scan = TableScan { skip: 0, remaining: None, ordinal: 0, visited: PageSet::new(db.pages() as u64), visit: &mut visit };
        assert!(db.walk_table(1, &mut scan, 0).is_err());
    }

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

    /// Index keys are compared as SQLite orders them: an integer past 2^53 and a real exactly,
    /// and text by its bytes in the database's encoding.
    #[test]
    fn compares_index_keys_as_sqlite() {
        use std::cmp::Ordering::*;
        let big = 1i64 << 53;
        assert_eq!(compare_values(&Value::Int(big + 1), &Value::Real(big as f64), 1), Greater);
        assert_eq!(compare_values(&Value::Real(big as f64), &Value::Int(big + 1), 1), Less);
        assert_eq!(compare_values(&Value::Int(big), &Value::Real(big as f64), 1), Equal);
        assert_eq!(compare_values(&Value::Int(1), &Value::Real(1.5), 1), Less);
        assert_eq!(compare_values(&Value::Int(-2), &Value::Real(-1.5), 1), Less);
        assert_eq!(compare_values(&Value::Int(i64::MAX), &Value::Real(9.3e18), 1), Less);
        assert_eq!(compare_values(&Value::Real(-0.0), &Value::Real(0.0), 1), Equal);
        assert_eq!(compare_values(&Value::Int(0), &Value::Real(-0.0), 1), Equal);
        // U+0100 is after 'B' in UTF-8 and UTF-16BE, but before it in UTF-16LE.
        let (b, a) = (Value::Text("B".into()), Value::Text("\u{100}".into()));
        assert_eq!(compare_values(&b, &a, 1), Less);
        assert_eq!(compare_values(&b, &a, 3), Less);
        assert_eq!(compare_values(&b, &a, 2), Greater);
        assert!(column_has_collation("CREATE TABLE t (a TEXT COLLATE NOCASE, b TEXT COLLATE BINARY, c TEXT)", 0));
        assert!(!column_has_collation("CREATE TABLE t (a TEXT COLLATE NOCASE, b TEXT COLLATE BINARY, c TEXT)", 1));
        assert!(!column_has_collation("CREATE TABLE t (a TEXT COLLATE NOCASE, b TEXT COLLATE BINARY, c TEXT)", 2));
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
