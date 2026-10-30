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
    fn tree(&mut self, page: u32, index: bool, out: &mut Vec<(Option<i64>, Vec<u8>)>, depth: usize) -> Result<()> {
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
        for i in 0..count {
            let pointer = offset + header + i * 2;
            let cell = u16::from_be_bytes([bytes[pointer], bytes[pointer + 1]]) as usize;
            if cell < offset + header + count * 2 || cell >= self.size {
                return Err(bad("cell pointer bounds"));
            }
            if interior {
                self.tree(be(bytes, cell)?, index, out, depth + 1)?;
            }
            if kind != 5 {
                out.push(self.payload(bytes, cell + if interior { 4 } else { 0 }, kind)?);
            }
        }
        if interior {
            self.tree(be(bytes, offset + 8)?, index, out, depth + 1)?;
        }
        Ok(())
    }
}

fn extra_capacity(capacity: usize) -> usize {
    (capacity / 4).max(1 << 24)
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
                // outgrows it, grow by a quarter of the buffer: extra capacity
                // stays bounded instead of doubling a GiB buffer, and copies on
                // allocators without in-place growth stay amortized.
                self.data.reserve_exact(extra_capacity(self.data.capacity()));
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
            source.claim(be(page, 8 + i * 4)?)?;
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
    // The commit bumps the change counter once, as it does for in-place changes.
    let counter = be(bytes, 24)?;
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

#[cfg(test)]
mod growth_tests {
    #[test]
    fn large_output_growth_is_amortized() {
        for initial in [128usize << 20, 1usize << 30] {
            let target = initial * 2;
            let mut capacity = initial;
            let mut reserves = 0;
            while capacity < target {
                capacity += super::extra_capacity(capacity);
                reserves += 1;
            }
            assert!(reserves <= 4, "doubling capacity required {reserves} reserves");
            assert!(capacity <= target + target / 4, "over-allocation exceeds one quarter");
        }
    }
}
