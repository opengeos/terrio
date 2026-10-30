//! SQLite compaction from encoded records, independent of SQL parsing or execution.
//!
//! Implements the documented SQLite file format (<https://sqlite.org/fileformat2.html>).
//! Table records, index keys, rowids, text encodings and schema SQL are preserved. Every tree
//! is packed anew, including overflow chains, with new schema root pointers and pointer maps.
use super::sqlite_edit::varint;
use super::vector::sqlite::{self as writer, PageStore};
use crate::{Error, Result};
use std::collections::{BTreeMap, BTreeSet};

fn bad(message: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("SQLite repack: {message}"))
}
fn be(bytes: &[u8], at: usize) -> Result<u32> {
    let value = bytes.get(at..at + 4).ok_or_else(|| bad("truncated integer"))?;
    Ok(u32::from_be_bytes(value.try_into().unwrap()))
}
fn serial_size(serial: u64) -> Result<usize> {
    match serial {
        0 | 8 | 9 => Ok(0),
        1..=4 => Ok(serial as usize),
        5 => Ok(6),
        6 | 7 => Ok(8),
        10 | 11 => Err(bad("reserved record serial type")),
        _ => usize::try_from((serial - 12) / 2).map_err(|_| bad("record is too large")),
    }
}
/// Raw record field ranges; UTF-16 text and expression keys are never re-encoded.
fn fields(record: &[u8]) -> Result<Vec<(u64, std::ops::Range<usize>)>> {
    let (header, mut cursor) = varint(record, 0).ok_or_else(|| bad("record header"))?;
    let header = usize::try_from(header).map_err(|_| bad("record header size"))?;
    if header < cursor || header > record.len() {
        return Err(bad("record header bounds"));
    }
    let mut offset = header;
    let mut fields = Vec::new();
    while cursor < header {
        let (serial, length) = varint(record, cursor).ok_or_else(|| bad("record serial type"))?;
        if serial < 0 || cursor + length > header {
            return Err(bad("record serial type bounds"));
        }
        cursor += length;
        let size = serial_size(serial as u64)?;
        let end = offset.checked_add(size).filter(|end| *end <= record.len()).ok_or_else(|| bad("truncated record"))?;
        fields.push((serial as u64, offset..end));
        offset = end;
    }
    if offset != record.len() {
        return Err(bad("unaccounted record bytes"));
    }
    Ok(fields)
}
fn root(record: &[u8]) -> Result<u32> {
    let fields = fields(record)?;
    if fields.len() != 5 {
        return Err(bad("invalid sqlite_schema record"));
    }
    let (serial, range) = &fields[3];
    if matches!(serial, 0 | 8) {
        return Ok(0);
    }
    if *serial == 9 {
        return Ok(1);
    }
    if !(1..=6).contains(serial) || record[range.clone()][0] & 0x80 != 0 {
        return Err(bad("invalid schema root page"));
    }
    let mut value = 0u64;
    for byte in &record[range.clone()] {
        value = (value << 8) | u64::from(*byte);
    }
    u32::try_from(value).map_err(|_| bad("schema root page overflow"))
}
fn schema_name(record: &[u8], encoding: u32) -> Result<String> {
    let fields = fields(record)?;
    let (serial, range) = fields.get(1).ok_or_else(|| bad("missing schema name"))?;
    if *serial < 13 || serial % 2 == 0 {
        return Err(bad("schema name is not text"));
    }
    let bytes = &record[range.clone()];
    match encoding {
        1 => String::from_utf8(bytes.to_vec()).map_err(|_| bad("invalid schema UTF-8")),
        2 | 3 => {
            if !bytes.len().is_multiple_of(2) {
                return Err(bad("invalid schema UTF-16"));
            }
            let words: Vec<_> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| if encoding == 2 { u16::from_le_bytes([pair[0], pair[1]]) } else { u16::from_be_bytes([pair[0], pair[1]]) })
                .collect();
            String::from_utf16(&words).map_err(|_| bad("invalid schema UTF-16"))
        }
        _ => Err(bad("unsupported database text encoding")),
    }
}

fn replace_root(record: &[u8], page: u32) -> Result<Vec<u8>> {
    let fields = fields(record)?;
    let mut serials: Vec<_> = fields.iter().map(|(serial, _)| *serial).collect();
    let length = if page <= 127 {
        1
    } else if page <= 32767 {
        2
    } else if page <= 0x7f_ffff {
        3
    } else if page <= 0x7fff_ffff {
        4
    } else {
        6
    };
    serials[3] = if length == 6 { 5 } else { length as u64 };
    let mut header = Vec::new();
    for serial in serials {
        writer::varint(&mut header, serial);
    }
    let mut size = header.len() + 1;
    loop {
        let next = header.len() + writer::varint_len(size as u64);
        if next == size {
            break;
        }
        size = next;
    }
    let mut result = Vec::new();
    writer::varint(&mut result, size as u64);
    result.extend_from_slice(&header);
    for (index, (_, range)) in fields.iter().enumerate() {
        if index == 3 {
            result.extend_from_slice(&u64::from(page).to_be_bytes()[8 - length..]);
        } else {
            result.extend_from_slice(&record[range.clone()]);
        }
    }
    Ok(result)
}

struct Source<'a> {
    bytes: &'a [u8],
    size: usize,
    count: u32,
    seen: BTreeSet<u32>,
}
impl<'a> Source<'a> {
    fn page(&self, page: u32) -> Result<&'a [u8]> {
        if page == 0 || page > self.count {
            return Err(bad(format!("page {page} out of range")));
        }
        let at = (page as usize - 1) * self.size;
        self.bytes.get(at..at + self.size).ok_or_else(|| bad("truncated page"))
    }
    fn claim(&mut self, page: u32) -> Result<&'a [u8]> {
        if !self.seen.insert(page) {
            return Err(bad(format!("page {page} is referenced more than once")));
        }
        self.page(page)
    }
    fn payload(&mut self, page: &[u8], at: usize, kind: u8) -> Result<(Option<i64>, Vec<u8>)> {
        let (size, n1) = varint(page, at).ok_or_else(|| bad("cell size"))?;
        let size = usize::try_from(size).map_err(|_| bad("negative cell size"))?;
        if size > self.bytes.len() {
            return Err(bad("cell payload exceeds database size"));
        }
        let (rowid, n2) = if kind == 13 {
            let (id, length) = varint(page, at + n1).ok_or_else(|| bad("rowid"))?;
            (Some(id), length)
        } else {
            (None, 0)
        };
        let (max, min) = if kind == 13 { writer::table_local_limits(self.size) } else { writer::index_local_limits(self.size) };
        let local = if size <= max {
            size
        } else {
            let k = min + (size - min) % (self.size - 4);
            if k <= max { k } else { min }
        };
        let start = at + n1 + n2;
        let mut value = page.get(start..start + local).ok_or_else(|| bad("cell payload bounds"))?.to_vec();
        if local < size {
            let mut next = be(page, start + local)?;
            while value.len() < size {
                let overflow = self.claim(next)?;
                next = be(overflow, 0)?;
                let take = (size - value.len()).min(self.size - 4);
                value.extend_from_slice(&overflow[4..4 + take]);
            }
            if next != 0 {
                return Err(bad("overflow chain exceeds payload"));
            }
        }
        Ok((rowid, value))
    }
    /// In-order traversal: index divider records are values too; table dividers are not.
    fn walk(
        &mut self,
        page: u32,
        index: bool,
        depth: usize,
        bounds: (Option<i64>, Option<i64>),
        visit: &mut impl FnMut(Option<i64>, Vec<u8>) -> Result<()>,
    ) -> Result<()> {
        if depth > 128 {
            return Err(bad("b-tree depth exceeds 128"));
        }
        let bytes = self.claim(page)?;
        let offset = if page == 1 { 100 } else { 0 };
        let kind = bytes[offset];
        if !matches!((index, kind), (false, 5 | 13) | (true, 2 | 10)) {
            return Err(bad("inconsistent b-tree page type"));
        }
        let interior = matches!(kind, 2 | 5);
        let header = if interior { 12 } else { 8 };
        let count = u16::from_be_bytes([bytes[offset + 3], bytes[offset + 4]]) as usize;
        if offset + header + count * 2 > self.size {
            return Err(bad("cell pointer array exceeds page"));
        }
        let mut lower = bounds.0;
        for i in 0..count {
            let pointer = offset + header + i * 2;
            let cell = u16::from_be_bytes([bytes[pointer], bytes[pointer + 1]]) as usize;
            if cell < offset + header + count * 2 || cell >= self.size {
                return Err(bad("cell pointer bounds"));
            }
            if interior {
                let upper = if kind == 5 {
                    let key = varint(bytes, cell + 4).ok_or_else(|| bad("invalid table divider"))?.0;
                    if lower.is_some_and(|min| key <= min) || bounds.1.is_some_and(|max| key > max) {
                        return Err(bad("table divider outside parent range"));
                    }
                    Some(key)
                } else {
                    None
                };
                self.walk(be(bytes, cell)?, index, depth + 1, (lower, upper), visit)?;
                if kind == 5 {
                    lower = upper;
                }
            }
            if kind != 5 {
                let (rowid, record) = self.payload(bytes, cell + if interior { 4 } else { 0 }, kind)?;
                if let Some(rowid) = rowid {
                    if lower.is_some_and(|min| rowid <= min) || bounds.1.is_some_and(|max| rowid > max) {
                        return Err(bad("table key outside parent range"));
                    }
                    lower = Some(rowid);
                }
                visit(rowid, record)?;
            }
        }
        if interior {
            self.walk(be(bytes, offset + 8)?, index, depth + 1, (lower, bounds.1), visit)?;
        }
        Ok(())
    }
    fn tree(&mut self, page: u32, index: bool, out: &mut Vec<(Option<i64>, Vec<u8>)>, depth: usize) -> Result<()> {
        self.walk(page, index, depth, (None, None), &mut |rowid, record| {
            out.push((rowid, record));
            Ok(())
        })
    }
}

struct Output {
    data: Vec<u8>,
    size: usize,
    auto: bool,
}
impl Output {
    fn special(&self, page: u32) -> bool {
        page == (0x4000_0000 / self.size as u32) + 1 || (self.auto && self.ptrmap(page) == page)
    }
    fn ptrmap(&self, page: u32) -> u32 {
        let stride = (self.size / 5 + 1) as u32;
        let map = ((page.saturating_sub(2)) / stride) * stride + 2;
        if map == (0x4000_0000 / self.size as u32) + 1 { map + 1 } else { map }
    }
    fn pointer(&mut self, page: u32, kind: u8, parent: u32) -> Result<()> {
        if page == 1 {
            return Ok(());
        }
        let map = self.ptrmap(page);
        let at = (page - map - 1) as usize * 5;
        let start = (map as usize - 1) * self.size + at;
        let target = self.data.get_mut(start..start + 5).ok_or_else(|| bad("pointer map bounds"))?;
        target[0] = kind;
        target[1..].copy_from_slice(&parent.to_be_bytes());
        Ok(())
    }
}
impl PageStore for Output {
    fn page_size(&self) -> usize {
        self.size
    }
    fn alloc(&mut self) -> Result<u32> {
        loop {
            let page = u32::try_from(self.data.len() / self.size + 1).map_err(|_| bad("too many output pages"))?;
            if self.data.len() + self.size > self.data.capacity() {
                // Initial capacity equals the input image size. If repacking
                // outgrows it, bound extra capacity instead of doubling a GiB
                // buffer; allocators without in-place growth may copy here.
                self.data.reserve_exact(1 << 24);
            }
            self.data.resize(self.data.len() + self.size, 0);
            if !self.special(page) {
                return Ok(page);
            }
        }
    }
    fn page_mut(&mut self, page: u32) -> Result<&mut [u8]> {
        self.data.get_mut((page as usize - 1) * self.size..page as usize * self.size).ok_or_else(|| bad("output page out of range"))
    }
}

/// Rebuild a self-contained SQLite image, preserving logical records and all schema objects.
pub(crate) fn compact(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() < 100 || !bytes.starts_with(b"SQLite format 3\0") {
        return Err(bad("not an SQLite file"));
    }
    let size = match u16::from_be_bytes([bytes[16], bytes[17]]) {
        1 => 65536,
        size => size as usize,
    };
    if !(512..=65536).contains(&size) || !size.is_power_of_two() || !bytes.len().is_multiple_of(size) {
        return Err(bad("invalid page size or truncated database"));
    }
    if bytes[20] != 0 {
        return Err(Error::Unsupported("SQLite repack: reserved/encrypted page bytes are unsupported".into()));
    }
    if !matches!((bytes[18], bytes[19]), (1 | 2, 1 | 2)) || bytes[21..24] != [64, 32, 32] {
        return Err(bad("unsupported SQLite format"));
    }
    let file_pages = u32::try_from(bytes.len() / size).map_err(|_| bad("too many pages"))?;
    let count = if be(bytes, 28)? > 0 && be(bytes, 24)? == be(bytes, 92)? { be(bytes, 28)? } else { file_pages };
    if count != file_pages {
        return Err(bad("database page count does not match its length"));
    }
    let auto = be(bytes, 52)? != 0;
    let mut source = Source { bytes, size, count, seen: BTreeSet::new() };
    let mut schema = Vec::new();
    source.tree(1, false, &mut schema, 0)?;
    if matches!(be(bytes, 68)?, 0x4750_4b47 | 0x4750_3130 | 0x4750_3131) {
        let names = schema.iter().map(|(_, record)| schema_name(record, be(bytes, 56)?)).collect::<Result<Vec<_>>>()?;
        if ["gpkg_contents", "gpkg_spatial_ref_sys"].iter().any(|required| !names.iter().any(|name| name.eq_ignore_ascii_case(required))) {
            return Err(bad("GeoPackage base tables are missing"));
        }
    }
    let mut roots = BTreeMap::new();
    let mut data = Vec::with_capacity(bytes.len());
    data.extend_from_slice(&bytes[..size]);
    let mut output = Output { data, size, auto };
    for (_, record) in &schema {
        let old = root(record)?;
        if old != 0 {
            if old == 1 || roots.contains_key(&old) {
                return Err(bad("duplicate schema root page"));
            }
            roots.insert(old, output.alloc()?);
        }
    }
    for (&old, &new) in &roots {
        let kind = source.page(old)?[0];
        let index = matches!(kind, 2 | 10);
        let mut records = Vec::new();
        source.tree(old, index, &mut records, 0)?;
        if index {
            let records: Vec<_> = records.into_iter().map(|(_, record)| record).collect();
            let mut spare = Vec::new();
            writer::build_index_records(&mut output, &records, Some(new), &mut spare)?;
            for page in spare {
                if page as usize != output.data.len() / size {
                    return Err(bad("index builder left a non-trailing spare"));
                }
                output.data.truncate(output.data.len() - size);
                while output.special((output.data.len() / size) as u32) {
                    output.data.truncate(output.data.len() - size);
                }
            }
        } else {
            let records: Vec<_> = records
                .into_iter()
                .map(|(rowid, record)| rowid.map(|rowid| (rowid, record)).ok_or_else(|| bad("missing table rowid")))
                .collect::<Result<_>>()?;
            if records.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
                return Err(bad("unordered table rowids"));
            }
            writer::build_table(&mut output, &records, Some(new))?;
        }
    }
    let records: Vec<_> = schema
        .into_iter()
        .map(|(rowid, record)| {
            let old = root(&record)?;
            Ok((rowid.ok_or_else(|| bad("missing schema rowid"))?, if old == 0 { record } else { replace_root(&record, roots[&old])? }))
        })
        .collect::<Result<_>>()?;
    writer::build_table(&mut output, &records, Some(1))?;
    // Check ownership of every input page, including the freelist and pointer-map pages.
    let mut trunk = be(bytes, 32)?;
    let mut free = 0u32;
    while trunk != 0 {
        let page = source.claim(trunk)?;
        free = free.checked_add(1).ok_or_else(|| bad("freelist count overflow"))?;
        let leaves = be(page, 4)? as usize;
        if leaves > (size - 8) / 4 {
            return Err(bad("freelist trunk is too long"));
        }
        for i in 0..leaves {
            let leaf = be(page, 8 + i * 4)?;
            source.claim(leaf)?;
            free = free.checked_add(1).ok_or_else(|| bad("freelist count overflow"))?;
        }
        trunk = be(page, 0)?;
    }
    if free != be(bytes, 36)? {
        return Err(bad("freelist count mismatch"));
    }
    for page in 1..=count {
        if output.special(page) {
            if source.seen.contains(&page) {
                return Err(bad("special page used by a b-tree or freelist"));
            }
        } else if !source.seen.contains(&page) {
            return Err(bad(format!("unaccounted input page {page}")));
        }
    }
    let n = (output.data.len() / size) as u32;
    let header = &mut output.data[..size];
    header[28..32].copy_from_slice(&n.to_be_bytes());
    header[32..40].fill(0);
    let counter = be(bytes, 24)?.wrapping_add(1);
    header[24..28].copy_from_slice(&counter.to_be_bytes());
    header[92..96].copy_from_slice(&counter.to_be_bytes());
    header[40..44].copy_from_slice(&be(bytes, 40)?.wrapping_add(1).to_be_bytes());
    if auto {
        let largest = roots.values().copied().max().unwrap_or(1);
        output.data[52..56].copy_from_slice(&largest.to_be_bytes());
        // Traverse the compact image to reconstruct auto-vacuum parent maps.
        let mappings = {
            let mut map = MapSource { bytes: &output.data, size, seen: BTreeSet::new(), mappings: Vec::new() };
            map.tree(1, 0, 1, 0)?;
            for &page in roots.values() {
                map.tree(page, 0, 1, 0)?;
            }
            map.mappings
        };
        for (page, role, parent) in mappings {
            output.pointer(page, role, parent)?;
        }
    }
    Ok(output.data)
}

struct MapSource<'a> {
    bytes: &'a [u8],
    size: usize,
    mappings: Vec<(u32, u8, u32)>,
    seen: BTreeSet<u32>,
}
impl MapSource<'_> {
    fn tree(&mut self, page: u32, parent: u32, role: u8, depth: usize) -> Result<()> {
        if depth > 128 || !self.seen.insert(page) {
            return Err(bad("invalid output tree"));
        }
        self.mappings.push((page, role, parent));
        let at = (page as usize - 1) * self.size;
        let bytes = self.bytes.get(at..at + self.size).ok_or_else(|| bad("output tree bounds"))?;
        let offset = if page == 1 { 100 } else { 0 };
        let kind = bytes[offset];
        let interior = matches!(kind, 2 | 5);
        let count = u16::from_be_bytes([bytes[offset + 3], bytes[offset + 4]]) as usize;
        for i in 0..count {
            let ptr = offset + if interior { 12 } else { 8 } + i * 2;
            let cell = u16::from_be_bytes([bytes[ptr], bytes[ptr + 1]]) as usize;
            if interior {
                self.tree(be(bytes, cell)?, page, 5, depth + 1)?;
            }
            if kind != 5 {
                let start = cell + if interior { 4 } else { 0 };
                let (length, n1) = varint(bytes, start).ok_or_else(|| bad("output cell"))?;
                let length = length as usize;
                let n2 = if kind == 13 { varint(bytes, start + n1).ok_or_else(|| bad("output rowid"))?.1 } else { 0 };
                let (max, min) = if kind == 13 { writer::table_local_limits(self.size) } else { writer::index_local_limits(self.size) };
                let local = if length <= max {
                    length
                } else {
                    let k = min + (length - min) % (self.size - 4);
                    if k <= max { k } else { min }
                };
                if local < length {
                    let mut next = be(bytes, start + n1 + n2 + local)?;
                    let mut previous = page;
                    let mut role = 3;
                    while next != 0 {
                        if !self.seen.insert(next) {
                            return Err(bad("output overflow cycle"));
                        }
                        self.mappings.push((next, role, previous));
                        previous = next;
                        role = 4;
                        next = be(self.bytes, (next as usize - 1) * self.size)?;
                    }
                }
            }
        }
        if interior {
            self.tree(be(bytes, offset + 8)?, page, 5, depth + 1)?;
        }
        Ok(())
    }
}

/// Decode an encoded SQLite record with the database's declared text encoding. Unlike the
/// permissive display reader, validation reports truncated fields and invalid text.
#[cfg(test)]
pub(crate) fn decode_record(record: &[u8], encoding: u32) -> Result<Vec<super::sqlite::Value>> {
    use super::sqlite::Value;
    let layout = fields(record)?;
    let mut values = super::sqlite::Db::record_values(record);
    for (value, (serial, range)) in values.iter_mut().zip(layout) {
        if serial >= 13 && serial % 2 == 1 {
            let text = &record[range];
            let decoded = match encoding {
                1 => String::from_utf8(text.to_vec()).map_err(|_| bad("invalid UTF-8 record text"))?,
                2 | 3 => {
                    if !text.len().is_multiple_of(2) {
                        return Err(bad("odd UTF-16 record length"));
                    }
                    let words: Vec<_> = text
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|pair| if encoding == 2 { u16::from_le_bytes(*pair) } else { u16::from_be_bytes(*pair) })
                        .collect();
                    String::from_utf16(&words).map_err(|_| bad("invalid UTF-16 record text"))?
                }
                _ => return Err(bad("unknown text encoding")),
            };
            *value = Value::Text(decoded);
        }
    }
    Ok(values)
}

/// Inspect every encoded table/index and page owner without allocating a replacement image.
/// The visitor receives one record at a time; large table payloads are not retained.
/// This is shared file-format inspection, not SQL
/// constraint or collation/index-key validation; those checks belong to the caller.
#[cfg(test)]
pub(crate) fn inspect(
    bytes: &[u8],
    mut visit: impl FnMut(&super::sqlite::SchemaEntry, Option<i64>, &[u8]) -> Result<()>,
) -> Result<Vec<super::sqlite::SchemaEntry>> {
    use super::sqlite::Value;
    if bytes.len() < 100 || !bytes.starts_with(b"SQLite format 3\0") {
        return Err(bad("not an SQLite file"));
    }
    let size = match u16::from_be_bytes([bytes[16], bytes[17]]) {
        1 => 65536,
        n => usize::from(n),
    };
    if !(512..=65536).contains(&size) || !size.is_power_of_two() || !bytes.len().is_multiple_of(size) {
        return Err(bad("invalid page size or truncated database"));
    }
    if bytes[20] != 0 {
        return Err(Error::Unsupported("SQLite validation: reserved/encrypted page bytes are unsupported".into()));
    }
    if !matches!(bytes[18], 1 | 2) || !matches!(bytes[19], 1 | 2) || bytes[21..24] != [64, 32, 32] {
        return Err(bad("invalid SQLite header format"));
    }
    let count = u32::try_from(bytes.len() / size).map_err(|_| bad("too many pages"))?;
    if be(bytes, 24)? == be(bytes, 92)? && be(bytes, 28)? != 0 && be(bytes, 28)? != count {
        return Err(bad("database page count does not match its length"));
    }
    let encoding = be(bytes, 56)?;
    let mut source = Source { bytes, size, count, seen: BTreeSet::new() };
    let mut encoded = Vec::new();
    source.tree(1, false, &mut encoded, 0)?;
    check_rowids(&encoded)?;
    let mut schema = Vec::new();
    for (_, record) in &encoded {
        let values = decode_record(record, encoding)?;
        if values.len() != 5 {
            return Err(bad("invalid sqlite_schema record"));
        }
        let [Value::Text(kind), Value::Text(name), Value::Text(table), Value::Int(page), sql] = values.as_slice() else {
            return Err(bad("invalid schema field types"));
        };
        let page = u32::try_from(*page).map_err(|_| bad("invalid schema root page"))?;
        let sql = match sql {
            Value::Text(sql) => sql.clone(),
            Value::Null => String::new(),
            _ => return Err(bad("invalid schema SQL")),
        };
        schema.push((kind.clone(), name.clone(), table.clone(), page, sql));
    }
    let mut roots = BTreeSet::from([1]);
    for entry in &schema {
        let (kind, _, _, root, _) = entry;
        if *root == 0 {
            continue;
        }
        if !matches!(kind.as_str(), "table" | "index") || !roots.insert(*root) {
            return Err(bad("invalid or shared schema root"));
        }
        let index = matches!(source.page(*root)?[0], 2 | 10);
        let mut last_rowid = None;
        source.walk(*root, index, 0, (None, None), &mut |rowid, record| {
            if !index {
                let rowid = rowid.ok_or_else(|| bad("missing table rowid"))?;
                if last_rowid.is_some_and(|last| last >= rowid) {
                    return Err(bad("unordered table rowids"));
                }
                last_rowid = Some(rowid);
            }
            fields(&record)?;
            visit(entry, rowid, &record)
        })?;
    }
    let auto = be(bytes, 52)? != 0;
    let special = Output { data: Vec::new(), size, auto };
    let mut trunk = be(bytes, 32)?;
    let mut free = 0u32;
    while trunk != 0 {
        let page = source.claim(trunk)?;
        check_pointer_map(bytes, &special, trunk, 2, 0)?;
        free = free.checked_add(1).ok_or_else(|| bad("freelist count overflow"))?;
        let leaves = be(page, 4)? as usize;
        if leaves > (size - 8) / 4 {
            return Err(bad("freelist trunk is too long"));
        }
        for i in 0..leaves {
            let leaf = be(page, 8 + i * 4)?;
            source.claim(leaf)?;
            check_pointer_map(bytes, &special, leaf, 2, 0)?;
            free = free.checked_add(1).ok_or_else(|| bad("freelist count overflow"))?;
        }
        trunk = be(page, 0)?;
    }
    if free != be(bytes, 36)? {
        return Err(bad("freelist count mismatch"));
    }
    for page in 1..=count {
        if special.special(page) {
            if source.seen.contains(&page) {
                return Err(bad("special page used as data"));
            }
        } else if !source.seen.contains(&page) {
            return Err(bad(format!("unaccounted page {page}")));
        }
    }
    let mut maps = MapSource { bytes, size, mappings: Vec::new(), seen: BTreeSet::new() };
    for root in roots {
        maps.tree(root, 0, 1, 0)?;
    }
    for (page, role, parent) in maps.mappings {
        if matches!(role, 1 | 5) {
            check_page_layout(source.page(page)?, page, size)?;
        }
        check_pointer_map(bytes, &special, page, role, parent)?;
    }
    Ok(schema)
}
#[cfg(test)]
fn check_pointer_map(bytes: &[u8], layout: &Output, page: u32, role: u8, parent: u32) -> Result<()> {
    if !layout.auto || page == 1 {
        return Ok(());
    }
    let map = layout.ptrmap(page);
    let relative = page.checked_sub(map).and_then(|n| n.checked_sub(1)).ok_or_else(|| bad("invalid pointer-map page reference"))?;
    let offset = (map as usize - 1)
        .checked_mul(layout.size)
        .and_then(|start| start.checked_add(relative as usize * 5))
        .ok_or_else(|| bad("pointer-map offset overflows"))?;
    if bytes.get(offset) != Some(&role) || be(bytes, offset + 1)? != parent {
        return Err(bad("incorrect auto-vacuum pointer map"));
    }
    Ok(())
}
#[cfg(test)]
fn check_rowids(rows: &[(Option<i64>, Vec<u8>)]) -> Result<()> {
    if rows.windows(2).any(|pair| pair[0].0 >= pair[1].0) {
        return Err(bad("unordered table rowids"));
    }
    Ok(())
}
#[cfg(test)]
fn check_page_layout(bytes: &[u8], page: u32, size: usize) -> Result<()> {
    let h = if page == 1 { 100 } else { 0 };
    let kind = bytes[h];
    let interior = matches!(kind, 2 | 5);
    let count = u16::from_be_bytes([bytes[h + 3], bytes[h + 4]]) as usize;
    let pointer_end = h + if interior { 12 } else { 8 } + count * 2;
    let content = match u16::from_be_bytes([bytes[h + 5], bytes[h + 6]]) {
        0 if size == 65536 => 65536,
        n => usize::from(n),
    };
    if content < pointer_end || content > size || bytes[h + 7] > 60 {
        return Err(bad("invalid cell content area"));
    }
    let mut ranges = Vec::new();
    for i in 0..count {
        let at = h + if interior { 12 } else { 8 } + i * 2;
        let start = u16::from_be_bytes([bytes[at], bytes[at + 1]]) as usize;
        let offset = start + if interior { 4 } else { 0 };
        let length = if kind == 5 {
            let (_, n) = varint(bytes, offset).ok_or_else(|| bad("table divider"))?;
            4 + n
        } else {
            let (payload, n1) = varint(bytes, offset).ok_or_else(|| bad("cell payload"))?;
            let payload = usize::try_from(payload).map_err(|_| bad("negative payload length"))?;
            let n2 = if kind == 13 { varint(bytes, offset + n1).ok_or_else(|| bad("rowid"))?.1 } else { 0 };
            let (max, min) = if kind == 13 { writer::table_local_limits(size) } else { writer::index_local_limits(size) };
            let local = if payload <= max {
                payload
            } else {
                let k = min + (payload - min) % (size - 4);
                if k <= max { k } else { min }
            };
            (if interior { 4 } else { 0 }) + n1 + n2 + local + if local < payload { 4 } else { 0 }
        }
        .max(4);
        if start < content || start.checked_add(length).is_none_or(|end| end > size) {
            return Err(bad("cell crosses content bounds"));
        }
        ranges.push((start, start + length));
    }
    let mut block = u16::from_be_bytes([bytes[h + 1], bytes[h + 2]]) as usize;
    while block != 0 {
        let header = bytes.get(block..block + 4).ok_or_else(|| bad("freeblock bounds"))?;
        let next = u16::from_be_bytes([header[0], header[1]]) as usize;
        let length = u16::from_be_bytes([header[2], header[3]]) as usize;
        if block < content || length < 4 || block + length > size || next != 0 && next <= block + length {
            return Err(bad("invalid freeblock chain"));
        }
        ranges.push((block, block + length));
        block = next;
    }
    ranges.sort_unstable();
    if ranges.windows(2).any(|p| p[0].1 > p[1].0) {
        return Err(bad("overlapping cells or freeblocks"));
    }
    let mut end = content;
    let mut fragments = 0;
    for (start, next) in ranges {
        // SQLite can leave adjacent fragments after resizing schema records
        // (e.g. ALTER TABLE RENAME COLUMN). Their contiguous span may exceed
        // three bytes. Its integrity checker checks the total, not each gap.
        fragments += start - end;
        end = next;
    }
    fragments += size - end;
    if fragments != usize::from(bytes[h + 7]) {
        return Err(bad("fragmented byte count mismatch"));
    }
    Ok(())
}

#[cfg(test)]
mod validation_tests {
    use super::*;
    #[test]
    fn adjacent_fragments_require_an_exact_bounded_total() {
        let mut page = vec![0u8; 512];
        page[0] = 13;
        page[3..5].copy_from_slice(&1u16.to_be_bytes());
        page[5..7].copy_from_slice(&503u16.to_be_bytes());
        page[7] = 4;
        page[8..10].copy_from_slice(&507u16.to_be_bytes());
        page[507..].copy_from_slice(&[3, 1, 2, 15, b'a']);
        check_page_layout(&page, 2, 512).unwrap();
        page[7] = 3;
        assert!(check_page_layout(&page, 2, 512).unwrap_err().to_string().contains("fragmented byte count"));
        page[7] = 61;
        assert!(check_page_layout(&page, 2, 512).is_err());
        page[7] = 4;
        page[1..3].copy_from_slice(&503u16.to_be_bytes());
        page[503..507].copy_from_slice(&[0, 0, 0, 8]);
        assert!(check_page_layout(&page, 2, 512).unwrap_err().to_string().contains("overlapping"));
    }
    #[test]
    fn streamed_inspection_preserves_rows_overflow_and_index_dividers() {
        use writer::{Table, Value};
        let rows: Vec<_> = (0..2000)
            .map(|id| {
                (
                    id,
                    vec![
                        Value::Null,
                        Value::Text(format!("entry-{id:06}")),
                        Value::Blob(vec![id as u8; if id == 7 { 20000 } else { 100 }]),
                    ],
                )
            })
            .collect();
        let table = Table {
            name: "items".into(),
            sql: "CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT UNIQUE, payload BLOB)".into(),
            rows: rows.clone(),
            autoindexes: vec![vec![1]],
        };
        let tables = [
            table,
            Table { name: "gpkg_contents".into(), sql: "CREATE TABLE gpkg_contents(name TEXT)".into(), rows: vec![], autoindexes: vec![] },
            Table {
                name: "gpkg_spatial_ref_sys".into(),
                sql: "CREATE TABLE gpkg_spatial_ref_sys(id INTEGER)".into(),
                rows: vec![],
                autoindexes: vec![],
            },
        ];
        let bytes = writer::write_database(&tables, &[], 10400, 0x4750_4b47).unwrap();
        let mut next = 0i64;
        let mut index_rows = 0;
        let schema = inspect(&bytes, |entry, rowid, record| {
            let values = decode_record(record, 1)?;
            if entry.0 == "table" {
                assert_eq!(rowid, Some(next));
                assert_eq!(values[1], super::super::sqlite::Value::Text(format!("entry-{next:06}")));
                assert_eq!(values[2], super::super::sqlite::Value::Blob(vec![next as u8; if next == 7 { 20000 } else { 100 }]));
                next += 1;
            } else {
                index_rows += 1;
            }
            Ok(())
        })
        .unwrap();
        assert_eq!(next, 2000);
        assert_eq!(index_rows, 2000);
        assert_eq!(schema.len(), 4);
        // The same walker remains the source for repacking, without a second record copy.
        let after = compact(&bytes).unwrap();
        let mut count = 0;
        inspect(&after, |_, _, _| {
            count += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(count, 4000);
        let root = schema.iter().find(|entry| entry.1 == "items").unwrap().3;
        let start = (root as usize - 1) * 4096;
        assert_eq!(bytes[start], 5);
        let cell = u16::from_be_bytes([bytes[start + 12], bytes[start + 13]]) as usize;
        let mut corrupt = bytes.clone();
        corrupt[start + cell + 4] = 0;
        assert!(inspect(&corrupt, |_, _, _| Ok(())).unwrap_err().to_string().contains("parent range"));
    }
    #[cfg(not(target_family = "wasm"))]
    #[test]
    #[ignore = "requires Python sqlite3 for independent file-format conformance"]
    fn independent_sqlite_encodings_vacuum_modes_and_corrupt_pointer_maps() {
        let dir = tempfile::tempdir().unwrap();
        let script = r#"
import sqlite3, pathlib, sys, json
root=pathlib.Path(sys.argv[1]); cases=[]
for mode in ['NONE','FULL','INCREMENTAL']:
 for encoding in ['UTF-8','UTF-16le','UTF-16be']:
  path=root/(mode+'-'+encoding+'.db')
  with sqlite3.connect(path) as db:
   db.execute('PRAGMA page_size=512'); db.execute('PRAGMA auto_vacuum='+mode)
   db.execute("PRAGMA encoding='"+encoding+"'")
   db.execute('CREATE TABLE gpkg_contents(table_name TEXT)')
   db.execute('CREATE TABLE gpkg_spatial_ref_sys(srs_id INTEGER PRIMARY KEY)')
   db.execute('CREATE TABLE items(id INTEGER PRIMARY KEY, name TEXT, payload BLOB)')
   db.execute('CREATE INDEX names ON items(name COLLATE NOCASE DESC)')
   db.execute('CREATE TABLE words(k TEXT PRIMARY KEY, v BLOB) WITHOUT ROWID')
   db.executemany('INSERT INTO items VALUES(?,?,?)', [(i,'entry-'+str(i),bytes([i%256])*900) for i in [-2**63, -1, *range(100), 2**63-1]])
   db.executemany('INSERT INTO words VALUES(?,?)', [('水-'+str(i),bytes([i%256])*700) for i in range(80)])
   db.execute('CREATE TABLE discarded(payload BLOB)')
   db.executemany('INSERT INTO discarded VALUES(?)',[(bytes(1200),)]*30)
   db.execute('DROP TABLE discarded'); db.commit()
   assert db.execute('PRAGMA integrity_check').fetchone()[0]=='ok'
  cases.append({'path':str(path),'valid':True})
  data=bytearray(path.read_bytes()); trunk=int.from_bytes(data[32:36],'big')
  if mode=='INCREMENTAL' and trunk:
   page_size=512; stride=page_size//5+1; map_page=2+((trunk-2)//stride)*stride
   at=(map_page-1)*page_size+(trunk-map_page-1)*5
   assert data[at]==2; data[at]=5
   broken=path.with_name('bad-'+path.name);broken.write_bytes(data)
   with sqlite3.connect(broken) as db:
    assert db.execute('PRAGMA integrity_check').fetchone()[0]!='ok'
   cases.append({'path':str(broken),'valid':False})
print(json.dumps(cases))
"#;
        let output = std::process::Command::new("python3").arg("-c").arg(script).arg(dir.path()).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let cases: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        for case in cases.as_array().unwrap() {
            let bytes = std::fs::read(case["path"].as_str().unwrap()).unwrap();
            let encoding = be(&bytes, 56).unwrap();
            let result = inspect(&bytes, |_, _, record| {
                decode_record(record, encoding)?;
                Ok(())
            });
            assert_eq!(result.is_ok(), case["valid"].as_bool().unwrap(), "{}: {result:?}", case["path"]);
            if case["valid"].as_bool().unwrap() {
                let repacked = dir.path().join("repacked.db");
                std::fs::write(&repacked, compact(&bytes).unwrap()).unwrap();
                let compare = r#"
import sqlite3, sys
with sqlite3.connect(sys.argv[1]) as a, sqlite3.connect(sys.argv[2]) as b:
 assert b.execute('PRAGMA integrity_check').fetchone()[0]=='ok'
 assert a.execute('PRAGMA encoding').fetchone()==b.execute('PRAGMA encoding').fetchone()
 for table, key in [('items','id'),('words','k')]:
  query='SELECT * FROM '+table+' ORDER BY '+key
  assert a.execute(query).fetchall()==b.execute(query).fetchall()
"#;
                let output = std::process::Command::new("python3")
                    .arg("-c")
                    .arg(compare)
                    .arg(case["path"].as_str().unwrap())
                    .arg(&repacked)
                    .output()
                    .unwrap();
                assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            }
        }
    }
    #[test]
    fn record_text_encoding_is_strict_and_supports_utf16() {
        use writer::Value;
        for encoding in [2, 3] {
            let text = "水🌍";
            let data: Vec<_> = text.encode_utf16().flat_map(|v| if encoding == 2 { v.to_le_bytes() } else { v.to_be_bytes() }).collect();
            let mut record = writer::record(&[Value::Blob(data)]);
            record[1] += 1; // Change the blob serial type to text with the same byte length.
            assert_eq!(decode_record(&record, encoding).unwrap(), [super::super::sqlite::Value::Text(text.into())]);
        }
        let mut odd = writer::record(&[Value::Blob(vec![0])]);
        odd[1] += 1;
        assert!(decode_record(&odd, 2).is_err());
        assert!(decode_record(&odd, 1).is_ok());
    }
}
