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

/// An SQLite database file in memory.
pub(crate) struct Db {
    data: Vec<u8>,
    page_size: usize,
    usable: usize,
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

    /// Every row of the b-tree rooted at `root`, as `(rowid, payload)`.
    fn scan(&self, root: u32, out: &mut Vec<(i64, Vec<u8>)>, depth: usize) -> Result<()> {
        if depth > 64 {
            return Err(bad("b-tree too deep"));
        }
        let page = self.page(root)?;
        let h = if root == 1 { 100 } else { 0 };
        let kind = page[h];
        let cells = u16::from_be_bytes([page[h + 3], page[h + 4]]) as usize;
        let header_len = if kind == 0x05 || kind == 0x02 { 12 } else { 8 };
        for c in 0..cells {
            let p = h + header_len + 2 * c;
            let off = u16::from_be_bytes([page[p], page[p + 1]]) as usize;
            match kind {
                0x05 => {
                    let child = u32::from_be_bytes(page[off..off + 4].try_into().map_err(|_| bad("cell"))?);
                    self.scan(child, out, depth + 1)?;
                }
                0x0d => {
                    let (size, n1) = varint(page, off).ok_or_else(|| bad("cell"))?;
                    let (rowid, n2) = varint(page, off + n1).ok_or_else(|| bad("cell"))?;
                    out.push((rowid, self.payload(page, off + n1 + n2, size as usize)?));
                }
                _ => return Err(bad("unsupported page type")),
            }
        }
        if kind == 0x05 {
            let right = u32::from_be_bytes(page[h + 8..h + 12].try_into().map_err(|_| bad("page"))?);
            self.scan(right, out, depth + 1)?;
        }
        Ok(())
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

    /// Decodes a record.
    fn record(payload: &[u8]) -> Vec<Value> {
        let Some((header_len, n)) = varint(payload, 0) else { return Vec::new() };
        let mut types = Vec::new();
        let mut i = n;
        while i < header_len as usize {
            let Some((t, k)) = varint(payload, i) else { break };
            types.push(t);
            i += k;
        }
        let mut pos = header_len as usize;
        let mut out = Vec::with_capacity(types.len());
        for t in types {
            let int = |len: usize, pos: usize| -> i64 {
                let b = &payload[pos..(pos + len).min(payload.len())];
                let mut v: i64 = if b.first().is_some_and(|x| x & 0x80 != 0) { -1 } else { 0 };
                for x in b {
                    v = (v << 8) | *x as i64;
                }
                v
            };
            let (value, len) = match t {
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
            };
            pos += len;
            out.push(value);
        }
        out
    }

    /// Reads a whole table by name.
    pub(crate) fn table(&self, name: &str) -> Result<Table> {
        let mut master = Vec::new();
        self.scan(1, &mut master, 0)?;
        for (_, payload) in master {
            let r = Self::record(&payload);
            let is_table = r.first().and_then(Value::as_str) == Some("table");
            if !is_table || !r.get(1).and_then(Value::as_str).is_some_and(|n| n.eq_ignore_ascii_case(name)) {
                continue;
            }
            let root = r.get(3).and_then(Value::as_i64).ok_or_else(|| bad("root page"))? as u32;
            let sql = r.get(4).and_then(Value::as_str).unwrap_or_default();
            let columns = columns(sql);
            // An INTEGER PRIMARY KEY column is the rowid, stored as NULL.
            let rowid_col = integer_primary_key(sql, &columns);
            let mut rows = Vec::new();
            self.scan(root, &mut rows, 0)?;
            let rows = rows
                .into_iter()
                .map(|(rowid, payload)| {
                    let mut v = Self::record(&payload);
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

/// The column names of a `CREATE TABLE` statement.
fn columns(sql: &str) -> Vec<String> {
    column_defs(sql).iter().filter_map(|d| d.split_whitespace().next()).map(|n| n.trim_matches(['"', '`', '[', ']']).to_string()).collect()
}

/// The column that aliases the rowid (`INTEGER PRIMARY KEY`).
fn integer_primary_key(sql: &str, _columns: &[String]) -> Option<usize> {
    column_defs(sql).iter().position(|d| {
        let d = d.to_ascii_uppercase();
        d.split_whitespace().nth(1) == Some("INTEGER") && d.contains("PRIMARY KEY")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
