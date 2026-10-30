//! Seek-Optimized ZIP (SOZip, index version 1): creating, optimizing, listing and validating
//! archives, and reading their members at random positions, as GDAL's `gdal vsi sozip` does.
//!
//! A SOZip member is Deflate-compressed in independent chunks (a sync flush and a full flush
//! every `chunk_size` uncompressed bytes), and followed by a hidden index file
//! (`.name.sozip.idx`, stored, with a local header but no central directory entry) holding the
//! compressed offset of each chunk. Readers seek by inflating from the chunk holding a position.
//! The specification is (C) Even Rouault, CC-BY-4.0: <https://github.com/sozip/sozip-spec>.

use super::zipdir::{self, CdEntry};
use super::{Node, ReadSeek, not_found, read_into};
use crate::{Error, Result};
use flate2::{Compress, Compression, FlushCompress, Status};
use serde::Serialize;
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

/// When members are seek-optimized.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EnableSozip {
    /// Files of at least `min_file_size` bytes (and larger than a chunk).
    #[default]
    Auto,
    /// Every file larger than a chunk.
    Yes,
    /// None.
    No,
}

impl std::str::FromStr for EnableSozip {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "yes" | "true" | "on" => Ok(Self::Yes),
            "no" | "false" | "off" => Ok(Self::No),
            _ => Err(Error::InvalidArgument(format!("--enable-sozip: '{s}' (auto, yes or no)"))),
        }
    }
}

/// Options of [`create`] and [`optimize`].
#[derive(Debug, Clone)]
pub struct SozipOptions {
    /// When to seek-optimize members.
    pub enable: EnableSozip,
    /// The uncompressed bytes of a chunk (32 KiB by default).
    pub chunk_size: u32,
    /// The smallest file seek-optimized in `Auto` mode (1 MB by default).
    pub min_file_size: u64,
    /// Replace an existing output rather than add to it.
    pub overwrite: bool,
    /// Add the files of directories, recursively ([`create`]).
    pub recursive: bool,
    /// Store files by their names alone, without their directories ([`create`]).
    pub junk_paths: bool,
    /// A `Content-Type` stored with each file added (in GDAL's `KV` extra field).
    pub content_type: Option<String>,
}

impl Default for SozipOptions {
    fn default() -> Self {
        Self {
            enable: EnableSozip::Auto,
            chunk_size: 32 * 1024,
            min_file_size: 1024 * 1024,
            overwrite: false,
            recursive: false,
            junk_paths: false,
            content_type: None,
        }
    }
}

/// A member of a zip archive, as [`list`] describes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SozipMember {
    /// The name in the archive.
    pub name: String,
    /// The uncompressed size.
    pub size: u64,
    /// The compressed size.
    pub compressed_size: u64,
    /// The modification time as stored (seconds since the epoch, read as UTC).
    pub modified: Option<i64>,
    /// The compression method (0 stored, 8 Deflate).
    pub method: u16,
    /// The advertised chunk size after index header/offset checks; use `validate`
    /// to verify independent chunk decoding and the member checksum.
    pub sozip_chunk_size: Option<u32>,
    /// `key=value` properties (GDAL's `KV` extra field: `Content-Type`).
    pub properties: Vec<String>,
}

impl SozipMember {
    /// The line `gdal vsi sozip list` prints.
    pub fn list_line(&self) -> String {
        let date = self.modified.map(super::format_time).unwrap_or_else(|| "1980-01-01 00:00:00Z".into());
        let sozip = match self.sozip_chunk_size {
            Some(c) => format!("   yes ({c:>9} bytes)   "),
            None => " ".repeat(27),
        };
        format!("{:>11}  {}  {sozip}  {}               {}", self.size, &date[..19], self.name, self.properties.join(" "))
    }
}

/// The header lines of `gdal vsi sozip list`.
pub const LIST_HEADER: &str = "  Length          DateTime        Seek-optimized / chunk size  Name               Properties\n\
                               -----------  -------------------  ---------------------------  -----------------  --------------";

/// What [`validate`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Validation {
    /// Whether the archive is a valid zip file whose SOZip indexes (if any) are valid.
    pub valid: bool,
    /// The members with a valid SOZip index.
    pub sozip_files: Vec<String>,
    /// What was checked and found, as GDAL's verbose mode prints it.
    pub messages: Vec<String>,
    /// The problems found.
    pub errors: Vec<String>,
}

/// What [`create`] and [`optimize`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SozipReport {
    /// Files added.
    pub added: u64,
    /// Files added seek-optimized.
    pub seek_optimized: u64,
    /// Bytes read from the inputs.
    pub bytes: u64,
}

// --- The index -----------------------------------------------------------------------------------

/// A parsed SOZip index.
#[derive(Debug, Clone)]
pub(crate) struct Index {
    pub chunk_size: u32,
    pub uncompressed_size: u64,
    pub compressed_size: u64,
    /// The compressed offset of each chunk but the first (whose offset is 0).
    pub offsets: Vec<u64>,
    /// Archive-backed offset table, validated by a bounded streaming pass.
    table: Option<std::sync::Arc<OffsetTable>>,
}

#[derive(Debug)]
struct OffsetTable {
    start: u64,
    count: usize,
    cache: std::sync::Mutex<Option<(usize, Vec<u64>)>>,
}
impl Index {
    fn to_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(32 + 8 * self.offsets.len());
        b.extend_from_slice(&1u32.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&self.chunk_size.to_le_bytes());
        b.extend_from_slice(&8u32.to_le_bytes());
        b.extend_from_slice(&self.uncompressed_size.to_le_bytes());
        b.extend_from_slice(&self.compressed_size.to_le_bytes());
        for o in &self.offsets {
            b.extend_from_slice(&o.to_le_bytes());
        }
        b
    }

    fn header(b: &[u8], body_len: u64, member: &CdEntry) -> std::result::Result<(Index, usize, usize), String> {
        if b.len() < 32 {
            return Err("index shorter than its 32-byte header".into());
        }
        let u32_at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().unwrap_or([0; 8]));
        let (version, skip, chunk_size, offset_size) = (u32_at(0), u32_at(4) as usize, u32_at(8), u32_at(12));
        let (uncompressed_size, compressed_size) = (u64_at(16), u64_at(24));
        if version != 1 {
            return Err(format!("index version {version}, not 1"));
        }
        if chunk_size == 0 {
            return Err("chunk_size is 0".into());
        }
        if offset_size != 8 {
            return Err(format!("offset_size {offset_size}, not 8"));
        }
        if uncompressed_size != member.uncompressed_size {
            return Err(format!("uncompress_size {uncompressed_size} is not the file's {}", member.uncompressed_size));
        }
        if compressed_size != member.compressed_size {
            return Err(format!("compress_size {compressed_size} is not the file's {}", member.compressed_size));
        }
        if uncompressed_size <= chunk_size as u64 {
            return Err("uncompress_size is not larger than chunk_size".into());
        }
        let count = (uncompressed_size - 1) / u64::from(chunk_size);
        let expected = count.checked_mul(8).and_then(|n| n.checked_add(32 + skip as u64));
        if expected != Some(body_len) {
            return Err(format!("index of {} bytes does not contain exactly {count} offsets", body_len));
        }
        // The exact byte-length check also bounds both conversions on 32-bit targets.
        let count = usize::try_from(count).map_err(|_| "index offset count exceeds address space")?;
        Ok((Index { chunk_size, uncompressed_size, compressed_size, offsets: Vec::new(), table: None }, skip, count))
    }
    #[cfg(test)]
    fn parse(b: &[u8], member: &CdEntry) -> std::result::Result<Index, String> {
        let (mut index, skip, count) = Self::header(b, b.len() as u64, member)?;
        let start = 32 + skip;
        let mut last = 0;
        for i in 0..count {
            let offset = u64::from_le_bytes(b[start + 8 * i..start + 8 * i + 8].try_into().unwrap());
            if offset <= last || offset >= index.compressed_size {
                return Err(format!("offset {i} ({offset}) not increasing or past the compressed data"));
            }
            index.offsets.push(offset);
            last = offset;
        }
        Ok(index)
    }
    /// The compressed offset of chunk `k`.
    #[cfg(test)]
    fn offset(&self, k: usize) -> u64 {
        if k == 0 { 0 } else { self.offsets[k - 1] }
    }

    fn read_offset(&self, r: &mut dyn ReadSeek, k: usize) -> std::io::Result<u64> {
        if k == 0 {
            return Ok(0);
        }
        if let Some(table) = &self.table {
            if k > table.count {
                return Err(chunk_error("chunk offset outside index"));
            }
            let index = k - 1;
            let cache = table.cache.lock().map_err(|_| chunk_error("offset cache poisoned"))?;
            if let Some((start, values)) = &*cache
                && index >= *start
                && index - *start < values.len()
            {
                return Ok(values[index - *start]);
            }
            drop(cache);
            let first = index / 8192 * 8192;
            let n = (table.count - first).min(8192);
            let at = table.start.checked_add(first as u64 * 8).ok_or_else(|| chunk_error("index offset overflow"))?;
            r.seek(SeekFrom::Start(at))?;
            let mut bytes = vec![0; n * 8];
            r.read_exact(&mut bytes)?;
            let values = bytes.as_chunks::<8>().0.iter().copied().map(u64::from_le_bytes).collect::<Vec<_>>();
            let value = values[index - first];
            *table.cache.lock().map_err(|_| chunk_error("offset cache poisoned"))? = Some((first, values));
            Ok(value)
        } else {
            self.offsets.get(k - 1).copied().ok_or_else(|| chunk_error("chunk offset outside index"))
        }
    }
    fn chunks(&self) -> usize {
        self.table.as_ref().map_or(self.offsets.len(), |table| table.count) + 1
    }
}

/// The hidden index name of a member: `dir/.name.sozip.idx`.
fn index_name(member: &str) -> String {
    match member.rsplit_once('/') {
        Some((dir, file)) => format!("{dir}/.{file}.sozip.idx"),
        None => format!(".{member}.sozip.idx"),
    }
}

/// The member's SOZip index: `Ok(None)` when it has none, `Err` with the reason when the index
/// found is invalid.
pub(crate) fn read_index(r: &mut dyn ReadSeek, e: &CdEntry) -> Result<std::result::Result<Option<Index>, String>> {
    read_index_mode(r, e, true)
}
fn read_index_mode(r: &mut dyn ReadSeek, e: &CdEntry, check_offsets: bool) -> Result<std::result::Result<Option<Index>, String>> {
    if e.method != 8 || e.is_dir() {
        return Ok(Ok(None));
    }
    let (start, local_zip64) = zipdir::data_layout(r, e, e.flags & 8 != 0)?;
    let archive_end = r.seek(SeekFrom::End(0))?;
    let Some(mut at) = start.checked_add(e.compressed_size).filter(|end| *end <= archive_end) else {
        return Ok(Err("member compressed data extends beyond the archive".into()));
    };
    // A data descriptor (with or without its signature, sizes of 4 or 8 bytes).
    if e.flags & 8 != 0 {
        r.seek(SeekFrom::Start(at))?;
        let mut d = [0u8; 4];
        r.read_exact(&mut d)?;
        let sig = u32::from_le_bytes(d) == 0x0807_4b50;
        let zip64 = local_zip64 || e.compressed_size > 0xFFFF_FFFF || e.uncompressed_size > 0xFFFF_FFFF;
        at += if sig { 4 } else { 0 } + 4 + if zip64 { 16 } else { 8 };
    }
    r.seek(SeekFrom::Start(at))?;
    let mut h = [0u8; 30];
    match r.read_exact(&mut h) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(Ok(None)),
        Err(error) => return Err(error.into()),
    }
    if u32::from_le_bytes([h[0], h[1], h[2], h[3]]) != zipdir::LOCAL {
        return Ok(Ok(None));
    }
    let u16_at = |i: usize| u16::from_le_bytes([h[i], h[i + 1]]) as usize;
    let (method, name_len, extra_len) = (u16_at(8), u16_at(26), u16_at(28));
    let mut size = u32::from_le_bytes([h[18], h[19], h[20], h[21]]) as u64;
    let mut name = vec![0u8; name_len];
    r.read_exact(&mut name)?;
    if zipdir::decode_name(&name, u16_at(6) & 0x800 != 0) != index_name(&e.name) {
        return Ok(Ok(None));
    }
    let mut extra = vec![0u8; extra_len];
    r.read_exact(&mut extra)?;
    if size == 0xFFFF_FFFF && extra.len() >= 20 && u16::from_le_bytes([extra[0], extra[1]]) == 1 {
        size = u64::from_le_bytes(extra[4..12].try_into().unwrap_or([0; 8]));
    }
    if method != 0 {
        return Ok(Err("the index file is compressed, not stored".into()));
    }
    let index_start = r.stream_position()?;
    let archive_end = r.seek(SeekFrom::End(0))?;
    if size < 32 || size > archive_end.saturating_sub(index_start) {
        return Ok(Err("index data is truncated or extends beyond the archive".into()));
    }
    r.seek(SeekFrom::Start(index_start))?;
    let mut header = [0; 32];
    r.read_exact(&mut header)?;
    let (mut index, skip, count) = match Index::header(&header, size, e) {
        Ok(value) => value,
        Err(error) => return Ok(Err(error)),
    };
    let table_start = index_start + 32 + skip as u64;
    if check_offsets {
        r.seek(SeekFrom::Start(table_start))?;
        let mut table = std::io::BufReader::with_capacity(64 * 1024, r.take(count as u64 * 8));
        let mut last = 0;
        for i in 0..count {
            let mut bytes = [0; 8];
            table.read_exact(&mut bytes)?;
            let offset = u64::from_le_bytes(bytes);
            if offset <= last || offset >= index.compressed_size {
                return Ok(Err(format!("offset {i} ({offset}) not increasing or past the compressed data")));
            }
            last = offset;
        }
    }
    index.table = Some(std::sync::Arc::new(OffsetTable { start: table_start, count, cache: std::sync::Mutex::new(None) }));
    Ok(Ok(Some(index)))
}

// --- Writing -------------------------------------------------------------------------------------

/// A member written: what its central directory record holds.
struct Written {
    name: String,
    method: u16,
    dos_time: u16,
    dos_date: u16,
    crc32: u32,
    compressed: u64,
    uncompressed: u64,
    offset: u64,
    extra: Vec<u8>,
}

/// A zip writer appending members to `out`, then writing the central directory.
struct ZipOut<W: Write> {
    out: W,
    offset: u64,
    /// Central directory records kept from an archive appended to.
    kept: Vec<Vec<u8>>,
    written: Vec<Written>,
    comment: Vec<u8>,
}

fn dos_time(secs: Option<i64>) -> (u16, u16) {
    let maximum = crate::civil_time::checked_unix_seconds(2107, 12, 31, 23, 59, 58).expect("valid DOS maximum date");
    let s = super::format_time(secs.unwrap_or(315_532_800).clamp(315_532_800, maximum));
    let n = |a: usize, b: usize| s[a..b].parse::<u16>().unwrap_or(0);
    let date = ((n(0, 4).saturating_sub(1980)) << 9) | (n(5, 7) << 5) | n(8, 10);
    let time = (n(11, 13) << 11) | (n(14, 16) << 5) | (n(17, 19) / 2);
    (time, date)
}

/// GDAL's `KV` extra field: `KeyValuePairs`, a count, then length-prefixed keys and values.
fn kv_extra(pairs: &[(&str, &str)]) -> Result<Vec<u8>> {
    let invalid = || Error::InvalidArgument("ZIP property fields exceed their format limits".into());
    if pairs.len() > u8::MAX as usize {
        return Err(invalid());
    }
    let mut size = 14usize;
    for (key, value) in pairs {
        if key.len() > u16::MAX as usize || value.len() > u16::MAX as usize {
            return Err(invalid());
        }
        size = size.checked_add(4 + key.len() + value.len()).ok_or_else(invalid)?;
    }
    if size > u16::MAX as usize {
        return Err(invalid());
    }
    let mut body = b"KeyValuePairs".to_vec();
    body.push(pairs.len() as u8);
    for (k, v) in pairs {
        body.extend_from_slice(&(k.len() as u16).to_le_bytes());
        body.extend_from_slice(k.as_bytes());
        body.extend_from_slice(&(v.len() as u16).to_le_bytes());
        body.extend_from_slice(v.as_bytes());
    }
    let mut out = b"KV".to_vec();
    out.extend_from_slice(&(body.len() as u16).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// The `key=value` pairs of a `KV` extra field.
fn kv_properties(mut extra: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    while extra.len() >= 4 {
        let (id, size) = (&extra[..2], u16::from_le_bytes([extra[2], extra[3]]) as usize);
        let body = &extra[4..(4 + size).min(extra.len())];
        if id == b"KV" && body.starts_with(b"KeyValuePairs") && body.len() > 13 {
            let mut b = &body[14..];
            for _ in 0..body[13] {
                let take = |b: &mut &[u8]| -> Option<String> {
                    let n = u16::from_le_bytes([*b.first()?, *b.get(1)?]) as usize;
                    let s = String::from_utf8_lossy(b.get(2..2 + n)?).into_owned();
                    *b = &b[2 + n..];
                    Some(s)
                };
                match (take(&mut b), take(&mut b)) {
                    (Some(k), Some(v)) => out.push(format!("{k}={v}")),
                    _ => break,
                }
            }
        }
        extra = &extra[(4 + size).min(extra.len())..];
    }
    out
}

impl<W: Write> ZipOut<W> {
    fn new(out: W, offset: u64, kept: Vec<Vec<u8>>) -> Self {
        Self { out, offset, kept, written: Vec::new(), comment: Vec::new() }
    }

    fn write(&mut self, b: &[u8]) -> Result<()> {
        self.out.write_all(b)?;
        self.offset += b.len() as u64;
        Ok(())
    }

    /// A local header; with `zip64`, sizes in a zip64 extra field.
    #[allow(clippy::too_many_arguments)]
    fn local_header(
        &mut self,
        name: &str,
        method: u16,
        time: (u16, u16),
        crc: u32,
        compressed: u64,
        uncompressed: u64,
        extra: &[u8],
    ) -> Result<()> {
        let zip64 = compressed >= 0xFFFF_FFFF || uncompressed >= 0xFFFF_FFFF;
        if name.len() > u16::MAX as usize || extra.len().saturating_add(if zip64 { 20 } else { 0 }) > u16::MAX as usize {
            return Err(Error::InvalidArgument("ZIP member name or extra fields exceed their format limits".into()));
        }
        let mut h = Vec::with_capacity(30 + name.len() + 20 + extra.len());
        h.extend_from_slice(&zipdir::LOCAL.to_le_bytes());
        h.extend_from_slice(&(if zip64 { 45u16 } else { 20 }).to_le_bytes());
        h.extend_from_slice(&(if name.is_ascii() { 0u16 } else { 0x800 }).to_le_bytes());
        h.extend_from_slice(&method.to_le_bytes());
        h.extend_from_slice(&time.0.to_le_bytes());
        h.extend_from_slice(&time.1.to_le_bytes());
        h.extend_from_slice(&crc.to_le_bytes());
        let small = |v: u64| if zip64 { 0xFFFF_FFFFu32 } else { v as u32 };
        h.extend_from_slice(&small(compressed).to_le_bytes());
        h.extend_from_slice(&small(uncompressed).to_le_bytes());
        h.extend_from_slice(&(name.len() as u16).to_le_bytes());
        h.extend_from_slice(&((extra.len() + if zip64 { 20 } else { 0 }) as u16).to_le_bytes());
        h.extend_from_slice(name.as_bytes());
        if zip64 {
            h.extend_from_slice(&1u16.to_le_bytes());
            h.extend_from_slice(&16u16.to_le_bytes());
            h.extend_from_slice(&uncompressed.to_le_bytes());
            h.extend_from_slice(&compressed.to_le_bytes());
        }
        h.extend_from_slice(extra);
        self.write(&h)
    }

    /// Adds a member whose compressed data is in `data` (already compressed with `method`).
    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        name: &str,
        method: u16,
        time: (u16, u16),
        crc: u32,
        uncompressed: u64,
        data: &mut dyn Read,
        compressed: u64,
        extra: Vec<u8>,
        index: Option<&Index>,
    ) -> Result<()> {
        let offset = self.offset;
        self.local_header(name, method, time, crc, compressed, uncompressed, &extra)?;
        let copied = std::io::copy(data, &mut self.out)?;
        if copied != compressed {
            return Err(Error::InvalidData(format!("'{name}': {copied} compressed bytes, {compressed} expected")));
        }
        self.offset += copied;
        if let Some(index) = index {
            let bytes = index.to_bytes();
            let crc = {
                let mut c = flate2::Crc::new();
                c.update(&bytes);
                c.sum()
            };
            self.local_header(&index_name(name), 0, time, crc, bytes.len() as u64, bytes.len() as u64, &[])?;
            self.write(&bytes)?;
        }
        self.written.push(Written {
            name: name.to_string(),
            method,
            dos_time: time.0,
            dos_date: time.1,
            crc32: crc,
            compressed,
            uncompressed,
            offset,
            extra,
        });
        Ok(())
    }

    /// Writes the central directory and the end records.
    fn finish(mut self) -> Result<W> {
        let cd_start = self.offset;
        let kept = std::mem::take(&mut self.kept);
        for raw in &kept {
            self.write(raw)?;
        }
        let written = std::mem::take(&mut self.written);
        for w in &written {
            let big = w.compressed >= 0xFFFF_FFFF || w.uncompressed >= 0xFFFF_FFFF || w.offset >= 0xFFFF_FFFF;
            let mut z64 = Vec::new();
            if big {
                z64.extend_from_slice(&1u16.to_le_bytes());
                z64.extend_from_slice(&24u16.to_le_bytes());
                z64.extend_from_slice(&w.uncompressed.to_le_bytes());
                z64.extend_from_slice(&w.compressed.to_le_bytes());
                z64.extend_from_slice(&w.offset.to_le_bytes());
            }
            let small = |v: u64| if big { 0xFFFF_FFFFu32 } else { v as u32 };
            let mut h = Vec::new();
            h.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            h.extend_from_slice(&(if big { 45u16 } else { 20 }).to_le_bytes());
            h.extend_from_slice(&(if big { 45u16 } else { 20 }).to_le_bytes());
            h.extend_from_slice(&(if w.name.is_ascii() { 0u16 } else { 0x800 }).to_le_bytes());
            h.extend_from_slice(&w.method.to_le_bytes());
            h.extend_from_slice(&w.dos_time.to_le_bytes());
            h.extend_from_slice(&w.dos_date.to_le_bytes());
            h.extend_from_slice(&w.crc32.to_le_bytes());
            h.extend_from_slice(&small(w.compressed).to_le_bytes());
            h.extend_from_slice(&small(w.uncompressed).to_le_bytes());
            h.extend_from_slice(&(w.name.len() as u16).to_le_bytes());
            let extra_len = u16::try_from(z64.len() + w.extra.len())
                .map_err(|_| Error::InvalidArgument("ZIP central directory extra fields exceed their format limit".into()))?;
            h.extend_from_slice(&extra_len.to_le_bytes());
            h.extend_from_slice(&[0; 10]); // comment length, disk, internal and external attributes
            h.extend_from_slice(&small(w.offset).to_le_bytes());
            h.extend_from_slice(w.name.as_bytes());
            h.extend_from_slice(&z64);
            h.extend_from_slice(&w.extra);
            self.write(&h)?;
        }
        let count = (kept.len() + written.len()) as u64;
        let cd_size = self.offset - cd_start;
        if count >= 0xFFFF || cd_size >= 0xFFFF_FFFF || cd_start >= 0xFFFF_FFFF {
            let z64_at = self.offset;
            let mut e = Vec::new();
            e.extend_from_slice(&0x0606_4b50u32.to_le_bytes());
            e.extend_from_slice(&44u64.to_le_bytes());
            e.extend_from_slice(&45u16.to_le_bytes());
            e.extend_from_slice(&45u16.to_le_bytes());
            e.extend_from_slice(&[0; 8]);
            e.extend_from_slice(&count.to_le_bytes());
            e.extend_from_slice(&count.to_le_bytes());
            e.extend_from_slice(&cd_size.to_le_bytes());
            e.extend_from_slice(&cd_start.to_le_bytes());
            e.extend_from_slice(&0x0706_4b50u32.to_le_bytes());
            e.extend_from_slice(&0u32.to_le_bytes());
            e.extend_from_slice(&z64_at.to_le_bytes());
            e.extend_from_slice(&1u32.to_le_bytes());
            self.write(&e)?;
        }
        let mut e = Vec::new();
        e.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        e.extend_from_slice(&[0, 0, 0, 0]);
        let c16 = count.min(0xFFFF) as u16;
        e.extend_from_slice(&c16.to_le_bytes());
        e.extend_from_slice(&c16.to_le_bytes());
        e.extend_from_slice(&(cd_size.min(0xFFFF_FFFF) as u32).to_le_bytes());
        e.extend_from_slice(&(cd_start.min(0xFFFF_FFFF) as u32).to_le_bytes());
        let comment = std::mem::take(&mut self.comment);
        let comment_len =
            u16::try_from(comment.len()).map_err(|_| Error::InvalidArgument("ZIP comment exceeds its format limit".into()))?;
        e.extend_from_slice(&comment_len.to_le_bytes());
        self.write(&e)?;
        self.write(&comment)?;
        self.out.flush()?;
        Ok(self.out)
    }
}

/// Compresses what is written to it with raw Deflate into a sink, independently every
/// `chunk_size` bytes when chunked, recording the chunks' offsets and the CRC-32.
struct Deflater<S: Write> {
    sink: S,
    compress: Compress,
    chunk_size: Option<u64>,
    in_chunk: u64,
    offsets: Vec<u64>,
    crc: flate2::Crc,
    buf: Vec<u8>,
}

impl<S: Write> Deflater<S> {
    fn new(sink: S, chunk_size: Option<u32>) -> Self {
        Self {
            sink,
            compress: Compress::new(Compression::default(), false),
            chunk_size: chunk_size.map(u64::from),
            in_chunk: 0,
            offsets: Vec::new(),
            crc: flate2::Crc::new(),
            buf: vec![0u8; 64 * 1024],
        }
    }

    fn run(&mut self, mut input: &[u8], flush: FlushCompress) -> std::io::Result<()> {
        loop {
            let (before_in, before_out) = (self.compress.total_in(), self.compress.total_out());
            let status = self.compress.compress(input, &mut self.buf, flush).map_err(std::io::Error::other)?;
            let used = (self.compress.total_in() - before_in) as usize;
            let made = (self.compress.total_out() - before_out) as usize;
            self.sink.write_all(&self.buf[..made])?;
            input = &input[used..];
            let done = match flush {
                FlushCompress::None => input.is_empty(),
                FlushCompress::Finish => status == Status::StreamEnd,
                // A flush is complete when output space was left over.
                _ => input.is_empty() && made < self.buf.len(),
            };
            if done {
                return Ok(());
            }
        }
    }

    /// Ends the stream; returns the sink, the chunk offsets and the CRC-32.
    fn finish(mut self) -> std::io::Result<(S, u64, Vec<u64>, u32)> {
        self.run(&[], FlushCompress::Finish)?;
        let total = self.compress.total_out();
        Ok((self.sink, total, self.offsets, self.crc.sum()))
    }
}

impl<S: Write> Write for Deflater<S> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.crc.update(data);
        let mut rest = data;
        while !rest.is_empty() {
            let take = match self.chunk_size {
                Some(c) => ((c - self.in_chunk) as usize).min(rest.len()),
                None => rest.len(),
            };
            self.run(&rest[..take], FlushCompress::None)?;
            self.in_chunk += take as u64;
            rest = &rest[take..];
            if let Some(c) = self.chunk_size
                && self.in_chunk == c
                && !rest.is_empty()
            {
                // The end of a chunk followed by more data: a sync flush, then a full flush, so
                // that the next chunk starts on a byte boundary with an empty dictionary.
                self.run(&[], FlushCompress::Sync)?;
                self.run(&[], FlushCompress::Full)?;
                self.offsets.push(self.compress.total_out());
                self.in_chunk = 0;
            }
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Where compressed data waits until its local header can be written.
enum Spool {
    Memory(Vec<u8>),
    #[cfg(not(target_family = "wasm"))]
    File(std::fs::File),
}

impl Write for Spool {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        #[cfg(not(target_family = "wasm"))]
        if let Spool::Memory(bytes) = self
            && bytes.len().saturating_add(b.len()) > 64 * 1024 * 1024
        {
            let mut file = tempfile::tempfile()?;
            file.write_all(bytes)?;
            *self = Spool::File(file);
        }
        match self {
            Spool::Memory(v) => v.write(b),
            #[cfg(not(target_family = "wasm"))]
            Spool::File(f) => f.write(b),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Spool {
    /// A spool for about `size` bytes: a temporary file when large (native builds).
    fn new(size: u64) -> Result<Self> {
        #[cfg(not(target_family = "wasm"))]
        if size > 64 << 20 {
            return Ok(Spool::File(tempfile::tempfile()?));
        }
        let _ = size;
        Ok(Spool::Memory(Vec::new()))
    }

    fn reader(self) -> Result<Box<dyn Read>> {
        Ok(match self {
            Spool::Memory(v) => Box::new(std::io::Cursor::new(v)),
            #[cfg(not(target_family = "wasm"))]
            Spool::File(mut f) => {
                f.seek(SeekFrom::Start(0))?;
                Box::new(std::io::BufReader::new(f))
            }
        })
    }
}

/// Compresses one streamed member and decides index eligibility from its actual length.
#[allow(clippy::too_many_arguments)]
fn add_member<W: Write>(
    zip: &mut ZipOut<W>,
    name: &str,
    size: u64,
    time: (u16, u16),
    options: &SozipOptions,
    extra: Vec<u8>,
    fill: impl FnOnce(&mut dyn Write) -> Result<u64>,
) -> Result<(u64, bool)> {
    // Auto inputs can have unknown or stale stat lengths. Flush chunks provisionally;
    // an ordinary Deflate member may contain flush markers without a seek index.
    let chunk = (options.enable != EnableSozip::No).then_some(options.chunk_size);
    let mut d = Deflater::new(Spool::new(size)?, chunk);
    let n = fill(&mut d)?;
    let (spool, compressed, offsets, crc) = d.finish()?;
    let eligible = n > u64::from(options.chunk_size)
        && (options.enable == EnableSozip::Yes || options.enable == EnableSozip::Auto && n >= options.min_file_size);
    let index = chunk.filter(|_| eligible).map(|chunk_size| Index {
        chunk_size,
        uncompressed_size: n,
        compressed_size: compressed,
        offsets,
        table: None,
    });
    zip.add(name, 8, time, crc, n, &mut *spool.reader()?, compressed, extra, index.as_ref())?;
    Ok((n, index.is_some()))
}

#[derive(Clone)]
enum AppendVersion {
    Native(std::path::PathBuf, std::fs::Metadata),
    Memory(std::path::PathBuf, crate::memfs::SharedBytes),
}
impl AppendVersion {
    fn check(&self) -> Result<()> {
        if let Self::Native(path, before) = self {
            let after = std::fs::metadata(path)?;
            let unchanged = before.len() == after.len()
                && before.modified().ok() == after.modified().ok()
                && before.created().ok() == after.created().ok();
            #[cfg(unix)]
            let unchanged = {
                use std::os::unix::fs::MetadataExt;
                unchanged
                    && (before.dev(), before.ino(), before.ctime(), before.ctime_nsec())
                        == (after.dev(), after.ino(), after.ctime(), after.ctime_nsec())
            };
            if !unchanged {
                return Err(Error::Unsupported("archive changed while preparing an append".into()));
            }
        }
        Ok(())
    }
}
struct AppendSource {
    reader: Box<dyn ReadSeek>,
    version: AppendVersion,
}

/// Native outputs stream into an atomic temporary file. Appends retain one source
/// handle/snapshot for both central records and the copied payload prefix.
struct Output {
    node: Node,
    path: String,
    prefix: u64,
    kept: Vec<Vec<u8>>,
    comment: Vec<u8>,
    source: Option<AppendSource>,
}
impl Output {
    fn open(path: &str, overwrite: bool) -> Result<(Self, Vec<String>)> {
        let node = Node::parse(path)?;
        if !matches!(node, Node::Local(_) | Node::Mem(_)) {
            return Err(Error::Unsupported(format!(
                "'{}': SOZip archives are written to local files or /vsimem/",
                super::redact_diagnostic_location(path)
            )));
        }
        if let Some(reason) = node.read_only_reason() {
            return Err(Error::Unsupported(format!("'{}': {reason}", super::redact_diagnostic_location(path))));
        }
        let (mut prefix, mut kept, mut names, mut comment, mut source) = (0, Vec::new(), Vec::new(), Vec::new(), None);
        if !overwrite && super::stat_node(&node)?.is_some_and(|stat| !stat.dir) {
            let mut snapshot = match &node {
                Node::Local(path) => {
                    let file = std::fs::File::open(path)?;
                    let version = AppendVersion::Native(path.clone(), file.metadata()?);
                    AppendSource { reader: Box::new(std::io::BufReader::new(file)), version }
                }
                Node::Mem(path) => {
                    let bytes = crate::memfs::read_shared(Path::new(path))?;
                    AppendSource {
                        reader: Box::new(std::io::Cursor::new(bytes.clone())),
                        version: AppendVersion::Memory(path.into(), bytes),
                    }
                }
                _ => unreachable!(),
            };
            let (entries, offset, old_comment) = zipdir::central_directory_at(&mut *snapshot.reader)?;
            snapshot.version.check()?;
            prefix = offset;
            comment = old_comment;
            kept = entries.iter().map(|entry| entry.raw.clone()).collect();
            names = entries.into_iter().map(|entry| entry.name).collect();
            source = Some(snapshot);
        }
        Ok((Self { node, path: path.into(), prefix, kept, comment, source }, names))
    }
    fn build(self, fill: impl FnOnce(&mut ZipOut<&mut dyn Write>) -> Result<SozipReport>) -> Result<SozipReport> {
        let Self { node, path, prefix, kept, comment, mut source } = self;
        let version = source.as_ref().map(|source| source.version.clone());
        let run = |writer: &mut dyn Write| -> Result<SozipReport> {
            if let Some(source) = &mut source {
                source.version.check()?;
                source.reader.seek(SeekFrom::Start(0))?;
                let mut remaining = prefix;
                let mut buffer = vec![0; 64 * 1024];
                while remaining > 0 {
                    let n = remaining.min(buffer.len() as u64) as usize;
                    source.reader.read_exact(&mut buffer[..n])?;
                    writer.write_all(&buffer[..n])?;
                    remaining -= n as u64;
                }
            }
            let mut zip = ZipOut::new(writer, prefix, kept);
            zip.comment = comment;
            let report = fill(&mut zip)?;
            zip.finish()?;
            if let Some(source) = &source {
                source.version.check()?;
            }
            Ok(report)
        };
        match &node {
            Node::Local(path) => {
                let mut report = None;
                super::write_local_atomic(path, |file| {
                    let mut buffered = std::io::BufWriter::with_capacity(64 * 1024, file);
                    report = Some(run(&mut buffered)?);
                    buffered.flush()?;
                    Ok(())
                })?;
                report.ok_or_else(|| Error::InvalidData("ZIP writer did not complete".into()))
            }
            _ => {
                let mut bytes = Vec::new();
                let report = run(&mut bytes)?;
                if let Some(AppendVersion::Memory(path, expected)) = version {
                    crate::memfs::replace_if_unchanged(&path, &expected, bytes)?;
                } else {
                    super::write(&path, &bytes)?;
                }
                Ok(report)
            }
        }
    }
}

fn stored_input_name(path: &str, remote: bool, junk_paths: bool) -> String {
    let mut path = if remote { path.split(['?', '#']).next().unwrap_or(path).to_string() } else { path.to_string() };
    if remote && let Some(scheme) = path.find("://") {
        let authority = scheme + 3;
        let end = path[authority..].find('/').map_or(path.len(), |end| authority + end);
        if let Some(userinfo) = path[authority..end].rfind('@') {
            path.replace_range(authority..authority + userinfo + 1, "");
        }
    }
    let path = path.replace('\\', "/");
    if junk_paths {
        return path.rsplit('/').next().unwrap_or(&path).to_string();
    }
    let path = path.split_once(":/").map_or(path.as_str(), |(_, rest)| rest);
    path.split('/').filter(|component| !component.is_empty() && *component != "." && *component != "..").collect::<Vec<_>>().join("/")
}

type InputFile = (Node, String, u64, Option<i64>);

/// The files to add for an input path: `(source node, name in the archive, size, modified)`.
fn inputs_of(input: &str, options: &SozipOptions) -> Result<Vec<InputFile>> {
    let node = Node::parse(input)?;
    let stat = super::stat_node(&node)?.ok_or_else(|| not_found(input))?;
    let remote = matches!(node, Node::Url { .. });
    let stored = |path: &str| stored_input_name(path, remote, options.junk_paths);
    if !stat.dir {
        return Ok(vec![(node, stored(input), stat.size.unwrap_or(0), stat.modified)]);
    }
    if !options.recursive {
        return Err(Error::InvalidArgument(format!(
            "'{}' is a directory: use the recursive option",
            super::redact_diagnostic_location(input)
        )));
    }
    let base = if remote { input.split(['?', '#']).next().unwrap_or(input) } else { input };
    let base = base.trim_end_matches('/');
    Ok(super::read_dir(&node, true, None)?
        .into_iter()
        .filter(|e| !e.is_dir())
        .map(|e| (node.join(&e.name), stored(&format!("{base}/{}", e.name)), e.size.unwrap_or(0), e.modified))
        .collect())
}

/// Creates a SOZip archive from files (or directories, with `recursive`), as
/// `gdal vsi sozip create` does: files of at least `min_file_size` bytes are seek-optimized
/// (`enable`), others Deflate-compressed. An existing archive is added to unless `overwrite`.
pub fn create(inputs: &[&str], output: &str, options: &SozipOptions) -> Result<SozipReport> {
    if inputs.is_empty() {
        return Err(Error::InvalidArgument("no input files".into()));
    }
    if options.chunk_size == 0 {
        return Err(Error::InvalidArgument("the chunk size must not be 0".into()));
    }
    let (out, existing) = Output::open(output, options.overwrite)?;
    let mut names: HashSet<String> = existing.into_iter().collect();
    let extra = options.content_type.as_deref().map(|c| kv_extra(&[("Content-Type", c)])).transpose()?.unwrap_or_default();
    // Enumerate before creating the temporary output, and exclude an existing output
    // from recursive inputs. Otherwise the archive could include itself while growing.
    let mut files = Vec::new();
    for input in inputs {
        for file in inputs_of(input, options)? {
            if super::is_same(&file.0, &out.node) {
                continue;
            }
            if !names.insert(file.1.clone()) {
                return Err(Error::InvalidArgument(format!("'{}' is already in '{}'", file.1, super::redact_diagnostic_location(output))));
            }
            files.push(file);
        }
    }
    out.build(|zip| {
        let mut report = SozipReport::default();
        for (node, name, size, modified) in files {
            let (n, indexed) = add_member(zip, &name, size, dos_time(modified), options, extra.clone(), |writer| read_into(&node, writer))?;
            report.bytes += n;
            report.added += 1;
            report.seek_optimized += u64::from(indexed);
        }
        Ok(report)
    })
}

/// Rewrites a zip archive as a SOZip archive, as `gdal vsi sozip optimize` does: every member
/// decompressed and recompressed (seek-optimized as `options.enable` says), names, dates and
/// properties kept, hidden indexes of the input dropped.
pub fn optimize(input: &str, output: &str, options: &SozipOptions) -> Result<SozipReport> {
    if options.chunk_size == 0 {
        return Err(Error::InvalidArgument("the chunk size must not be 0".into()));
    }
    if options.recursive || options.junk_paths || options.content_type.is_some() {
        return Err(Error::InvalidArgument("recursive, junk_paths and content_type are only supported by SOZip create".into()));
    }
    let in_node = Node::parse(input)?;
    let out_node = Node::parse(output)?;
    if super::is_same(&in_node, &out_node) {
        return Err(Error::InvalidArgument("the input and output archives are the same".into()));
    }
    let (out, _) = Output::open(output, true)?;
    if !options.overwrite && super::stat_node(&out_node)?.is_some() {
        return Err(Error::InvalidArgument(format!("'{}' exists: use the overwrite option", super::redact_diagnostic_location(output))));
    }
    out.build(|zip| {
        let mut report = SozipReport::default();
        super::with_archive_reader(&in_node, false, |r| {
            let (entries, _, comment) = zipdir::central_directory_at(r)?;
            zip.comment = comment;
            for e in entries {
                if e.is_dir() {
                    zip.add(&e.name, 0, (e.dos_time, e.dos_date), 0, 0, &mut std::io::empty(), 0, Vec::new(), None)?;
                    continue;
                }
                let (n, indexed) =
                    add_member(zip, &e.name, e.uncompressed_size, (e.dos_time, e.dos_date), options, keep_kv(&e.extra), |writer| {
                        zipdir::extract(r, &e, writer)
                    })?;
                report.added += 1;
                report.bytes += n;
                report.seek_optimized += u64::from(indexed);
            }
            Ok(())
        })?;
        Ok(report)
    })
}

/// The `KV` extra field of a member's extra fields, if any.
fn keep_kv(mut extra: &[u8]) -> Vec<u8> {
    while extra.len() >= 4 {
        let size = u16::from_le_bytes([extra[2], extra[3]]) as usize;
        let end = (4 + size).min(extra.len());
        if &extra[..2] == b"KV" {
            return extra[..end].to_vec();
        }
        extra = &extra[end..];
    }
    Vec::new()
}

/// Lists a zip archive's members with their SOZip status, as `gdal vsi sozip list` does.
pub fn list(input: &str) -> Result<Vec<SozipMember>> {
    let node = Node::parse(input)?;
    super::with_archive_reader(&node, true, |r| {
        let mut out = Vec::new();
        for e in zipdir::central_directory_at(r)?.0 {
            let index = read_index(r, &e)?;
            out.push(SozipMember {
                name: e.name.clone(),
                size: e.uncompressed_size,
                compressed_size: e.compressed_size,
                modified: e.modified(),
                method: e.method,
                sozip_chunk_size: index.ok().flatten().map(|i| i.chunk_size),
                properties: kv_properties(&e.extra),
            });
        }
        Ok(out)
    })
}

/// Validates a zip archive and its SOZip indexes, as `gdal vsi sozip validate` does: every
/// member decompressed and its CRC-32 checked; each index's header checked against its member,
/// its offsets increasing, and every chunk decompressed alone from its offset and compared with
/// the member's bytes.
pub fn validate(input: &str) -> Result<Validation> {
    let node = Node::parse(input)?;
    let mut v = Validation::default();
    let result = super::with_archive_reader(&node, false, |r| {
        let cd = zipdir::central_directory(r)?;
        for e in &cd {
            v.messages.push(format!("Testing {}...", e.name));
            if e.is_dir() {
                if let Err(error) = zipdir::extract(r, e, &mut std::io::sink()) {
                    v.errors.push(format!("{}: {error}", e.name));
                }
                continue;
            }
            match read_index(r, e)? {
                Ok(Some(index)) => {
                    v.messages.push(format!("  {} has an associated .sozip.idx file", e.name));
                    v.messages.push(format!("  {}: checking index offset values...", e.name));
                    v.messages.push(format!("  {}: checking if chunks can be independently decompressed...", e.name));
                    let start = zipdir::data_offset(r, e)?;
                    match check_chunks(r, start, &index, e.crc32) {
                        Ok(()) => {
                            v.messages.push(format!("* File {} has a valid SOZip index, using chunk_size = {}.", e.name, index.chunk_size));
                            v.sozip_files.push(e.name.clone());
                        }
                        Err(why) => v.errors.push(format!("{}: {why}", e.name)),
                    }
                }
                index => {
                    if let Err(why) = index {
                        v.errors.push(format!("{}: invalid SOZip index: {why}", e.name));
                    }
                    if let Err(error) = zipdir::extract(r, e, &mut std::io::sink()) {
                        v.errors.push(format!("{}: {error}", e.name));
                    }
                }
            }
        }
        Ok(())
    });
    if let Err(e) = result {
        v.errors.push(e.to_string());
    }
    v.valid = v.errors.is_empty();
    Ok(v)
}

fn chunk_error(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

/// Independent, bounded-memory decoding of one chunk. Input position is explicit
/// so metadata/index seeks on the shared reader cannot disturb decoder state.
struct ChunkDecoder {
    decoder: flate2::Decompress,
    input: Vec<u8>,
    at: usize,
    len: usize,
    next: u64,
    end: u64,
    patch: Option<u64>,
    compressed_size: u64,
    expected: u64,
    finished: bool,
}
impl ChunkDecoder {
    fn new(r: &mut dyn ReadSeek, data_start: u64, index: &Index, k: usize) -> std::io::Result<Self> {
        let mut decoder = Self::whole(data_start, 0, 0)?;
        decoder.reset_chunk(r, data_start, index, k)?;
        Ok(decoder)
    }
    fn reset_chunk(&mut self, r: &mut dyn ReadSeek, data_start: u64, index: &Index, k: usize) -> std::io::Result<()> {
        if k >= index.chunks() {
            return Err(chunk_error("chunk outside index"));
        }
        let from = index.read_offset(r, k)?;
        let to = if k + 1 < index.chunks() { index.read_offset(r, k + 1)? } else { index.compressed_size };
        if from >= to || to > index.compressed_size {
            return Err(chunk_error("invalid compressed chunk range"));
        }
        let next = data_start.checked_add(from).ok_or_else(|| chunk_error("chunk offset overflow"))?;
        let end = data_start.checked_add(to).ok_or_else(|| chunk_error("chunk end overflow"))?;
        let patch = if k + 1 < index.chunks() {
            if end - next < 5 {
                return Err(chunk_error("chunk lacks full-flush marker"));
            }
            r.seek(SeekFrom::Start(end - 5))?;
            let mut marker = [0; 5];
            r.read_exact(&mut marker)?;
            if marker != [0, 0, 0, 255, 255] {
                return Err(chunk_error("chunk lacks SOZip full-flush marker"));
            }
            Some(end - 5)
        } else {
            None
        };
        let start = (k as u64).checked_mul(u64::from(index.chunk_size)).ok_or_else(|| chunk_error("chunk position overflow"))?;
        let expected =
            index.uncompressed_size.checked_sub(start).ok_or_else(|| chunk_error("chunk outside member"))?.min(u64::from(index.chunk_size));
        self.decoder.reset(false);
        self.at = 0;
        self.len = 0;
        self.next = next;
        self.end = end;
        self.patch = patch;
        self.compressed_size = to - from;
        self.expected = expected;
        self.finished = false;
        Ok(())
    }
    fn whole(data_start: u64, compressed_size: u64, expected: u64) -> std::io::Result<Self> {
        let end = data_start.checked_add(compressed_size).ok_or_else(|| chunk_error("member end overflow"))?;
        Ok(Self {
            decoder: flate2::Decompress::new(false),
            input: vec![0; 64 * 1024],
            at: 0,
            len: 0,
            next: data_start,
            end,
            patch: None,
            compressed_size,
            expected,
            finished: false,
        })
    }
    fn position(&self) -> u64 {
        self.decoder.total_out()
    }
    fn read(&mut self, r: &mut dyn ReadSeek, out: &mut [u8]) -> std::io::Result<usize> {
        if self.finished || out.is_empty() {
            return Ok(0);
        }
        let mut produced = 0;
        loop {
            if self.at == self.len && self.next < self.end {
                self.len = (self.end - self.next).min(self.input.len() as u64) as usize;
                r.seek(SeekFrom::Start(self.next))?;
                r.read_exact(&mut self.input[..self.len])?;
                if let Some(patch) = self.patch
                    && patch >= self.next
                    && patch < self.next + self.len as u64
                {
                    self.input[(patch - self.next) as usize] = 1;
                }
                self.next += self.len as u64;
                self.at = 0;
            }
            let before = (self.decoder.total_in(), self.decoder.total_out());
            let mut overflow = [0; 1];
            let target = if before.1 == self.expected { &mut overflow[..] } else { &mut out[produced..] };
            let status = self
                .decoder
                .decompress(&self.input[self.at..self.len], target, flate2::FlushDecompress::None)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
            self.at += (self.decoder.total_in() - before.0) as usize;
            let made = (self.decoder.total_out() - before.1) as usize;
            if self.decoder.total_out() > self.expected {
                return Err(chunk_error("chunk has excess decoded output"));
            }
            produced += made;
            if status == flate2::Status::StreamEnd {
                if self.decoder.total_in() != self.compressed_size || self.decoder.total_out() != self.expected {
                    return Err(chunk_error("chunk compressed or decoded size mismatch"));
                }
                self.finished = true;
                return Ok(produced);
            }
            if before == (self.decoder.total_in(), self.decoder.total_out()) {
                return Err(chunk_error("chunk stream is truncated"));
            }
            if produced > 0 && self.decoder.total_out() < self.expected {
                return Ok(produced);
            }
        }
    }
}

/// Compare independently decoded chunks byte-for-byte against the original
/// stream and check its CRC. Both decoders keep bounded input/output buffers;
/// explicit compressed positions allow them to share one seekable archive.
fn check_chunks(r: &mut dyn ReadSeek, data_start: u64, index: &Index, expected_crc: u32) -> std::result::Result<(), String> {
    let mut original = ChunkDecoder::whole(data_start, index.compressed_size, index.uncompressed_size).map_err(|e| e.to_string())?;
    let mut crc = flate2::Crc::new();
    let mut expected = vec![0; 64 * 1024];
    let mut actual = vec![0; 64 * 1024];
    let mut decoder = ChunkDecoder::new(r, data_start, index, 0).map_err(|e| format!("chunk 0: {e}"))?;
    for k in 0..index.chunks() {
        if k > 0 {
            decoder.reset_chunk(r, data_start, index, k).map_err(|e| format!("chunk {k}: {e}"))?;
        }
        loop {
            let n = decoder.read(r, &mut actual).map_err(|e| format!("chunk {k}: {e}"))?;
            if n == 0 {
                break;
            }
            let mut filled = 0;
            while filled < n {
                let made = original.read(r, &mut expected[filled..n]).map_err(|e| format!("original member stream: {e}"))?;
                if made == 0 {
                    return Err("original member stream is short".into());
                }
                filled += made;
            }
            if expected[..n] != actual[..n] {
                return Err(format!("chunk {k} differs from the original member stream"));
            }
            crc.update(&expected[..n]);
        }
    }
    if original.read(r, &mut expected[..1]).map_err(|e| e.to_string())? != 0 {
        return Err("original member has excess output".into());
    }
    if crc.sum() != expected_crc {
        return Err("corrupt member (CRC-32 mismatch)".into());
    }
    Ok(())
}

#[cfg(test)]
fn inflate_chunk(r: &mut dyn ReadSeek, data_start: u64, index: &Index, k: usize, len: usize) -> std::io::Result<Vec<u8>> {
    let mut decoder = ChunkDecoder::new(r, data_start, index, k)?;
    let mut result = Vec::new();
    let mut buffer = vec![0; 64 * 1024];
    loop {
        let n = decoder.read(r, &mut buffer)?;
        if n == 0 {
            break;
        }
        result.extend_from_slice(&buffer[..n]);
    }
    if result.len() != len {
        return Err(chunk_error("chunk decoded size mismatch"));
    }
    Ok(result)
}

// --- Random access -------------------------------------------------------------------------------

/// A zip member read at any position: stored members directly, SOZip members by inflating the
/// chunk holding a position (one chunk cached).
type ArchiveOpener = dyn Fn() -> Result<Box<dyn ReadSeek + Send + Sync>> + Send + Sync;
#[derive(Clone)]
pub(crate) struct MemberReopen {
    open: std::sync::Arc<ArchiveOpener>,
    data_start: u64,
    size: u64,
    index: Option<Index>,
    native_path: Option<std::path::PathBuf>,
}
impl MemberReopen {
    pub(crate) fn native_archive(&self) -> Option<&Path> {
        self.native_path.as_deref()
    }
    pub(crate) fn open(&self) -> Result<MemberReader> {
        Ok(MemberReader {
            inner: (self.open)()?,
            data_start: self.data_start,
            size: self.size,
            index: self.index.clone(),
            pos: 0,
            cached: None,
            decoder: None,
            reopen: Some(self.clone()),
        })
    }
}

pub(crate) struct MemberReader {
    inner: Box<dyn ReadSeek + Send + Sync>,
    data_start: u64,
    size: u64,
    index: Option<Index>,
    pos: u64,
    cached: Option<(u64, Vec<u8>)>,
    decoder: Option<(usize, ChunkDecoder)>,
    reopen: Option<MemberReopen>,
}

impl MemberReader {
    pub(crate) fn reopen(&self) -> Option<MemberReopen> {
        self.reopen.clone()
    }
    /// Opens `inner` (a zip archive) at a member, when it can be read at random positions:
    /// stored, or Deflate with a valid SOZip index. `None` otherwise.
    pub(crate) fn open(mut inner: Box<dyn ReadSeek + Send + Sync>, member: &str) -> Result<Option<Self>> {
        let cd = zipdir::central_directory(&mut *inner)?;
        let e = cd.iter().find(|e| super::clean_member(&e.name) == member).ok_or_else(|| not_found(member))?;
        let data_start = zipdir::data_offset(&mut *inner, e)?;
        let index = match e.method {
            0 => {
                let archive_end = inner.seek(SeekFrom::End(0))?;
                if e.compressed_size != e.uncompressed_size || data_start.checked_add(e.compressed_size).is_none_or(|end| end > archive_end)
                {
                    return Ok(None);
                }
                None
            }
            8 => match read_index_mode(&mut *inner, e, false)? {
                Ok(Some(i)) => Some(i),
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };
        if e.flags & 1 != 0 {
            return Ok(None);
        }
        Ok(Some(Self { inner, data_start, size: e.uncompressed_size, index, pos: 0, cached: None, decoder: None, reopen: None }))
    }
}

impl Read for MemberReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let Some(index) = &self.index else {
            let n = (buf.len() as u64).min(self.size - self.pos) as usize;
            let at = self.data_start.checked_add(self.pos).ok_or_else(|| chunk_error("stored member offset overflow"))?;
            self.inner.seek(SeekFrom::Start(at))?;
            let n = self.inner.read(&mut buf[..n])?;
            if n == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "short stored ZIP member"));
            }
            self.pos += n as u64;
            return Ok(n);
        };
        let cs = u64::from(index.chunk_size);
        let k = (self.pos / cs) as usize;
        if self.cached.as_ref().is_none_or(|(start, bytes)| self.pos < *start || self.pos >= *start + bytes.len() as u64) {
            let relative = self.pos - k as u64 * cs;
            if self.decoder.is_none() {
                self.decoder = Some((k, ChunkDecoder::new(&mut *self.inner, self.data_start, index, k)?));
            }
            let (chunk, decoder) = self.decoder.as_mut().unwrap();
            if *chunk != k || decoder.position() > relative {
                decoder.reset_chunk(&mut *self.inner, self.data_start, index, k)?;
                *chunk = k;
            }
            let mut bytes = self.cached.take().map_or_else(|| vec![0; 64 * 1024], |(_, bytes)| bytes);
            bytes.resize(decoder.expected.min(64 * 1024) as usize, 0);
            while decoder.position() < relative {
                let take = (relative - decoder.position()).min(bytes.len() as u64) as usize;
                if decoder.read(&mut *self.inner, &mut bytes[..take])? == 0 {
                    return Err(chunk_error("short SOZip chunk"));
                }
            }
            let wanted = (decoder.expected - relative).min(64 * 1024) as usize;
            bytes.resize(wanted, 0);
            let mut n = 0;
            while n < wanted {
                let made = decoder.read(&mut *self.inner, &mut bytes[n..])?;
                if made == 0 {
                    return Err(chunk_error("short SOZip chunk"));
                }
                n += made;
            }
            self.cached = Some((self.pos, bytes));
        }
        let (start, bytes) = self.cached.as_ref().unwrap();
        let offset = (self.pos - *start) as usize;
        let n = buf.len().min(bytes.len() - offset);
        buf[..n].copy_from_slice(&bytes[offset..offset + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for MemberReader {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let new = match pos {
            SeekFrom::Start(p) => Some(p),
            SeekFrom::End(d) => self.size.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = new.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek before the start"))?;
        Ok(self.pos)
    }
}

/// Opens a member of a zip archive for random access (see [`MemberReader`]): a local archive,
/// or a remote one read by range requests.
pub(crate) fn open_member(archive: &crate::vfs::Location, member: &str) -> Result<Option<MemberReader>> {
    let (open, native_path): (std::sync::Arc<ArchiveOpener>, Option<std::path::PathBuf>) = match archive {
        crate::vfs::Location::Local(path) => {
            if !path.is_file() {
                return Ok(None);
            }
            let source = path.clone();
            (std::sync::Arc::new(move || Ok(Box::new(std::io::BufReader::new(std::fs::File::open(&source)?)))), Some(path.clone()))
        }
        #[cfg(remote)]
        crate::vfs::Location::Url(_) | crate::vfs::Location::S3 { .. } => {
            let remote = archive.remote().ok_or_else(|| Error::InvalidArgument("not remote".into()))?;
            let mut options = crate::http::HttpOptions::from_env();
            if std::env::var_os("TERRIO_HTTP_BLOCK_SIZE").is_none() && std::env::var_os("CPL_VSIL_CURL_CHUNK_SIZE").is_none() {
                options.block_size = 16 * 1024;
            }
            let reader = crate::http::RangeReader::open_with(&remote, options)?;
            (std::sync::Arc::new(move || Ok(Box::new(reader.clone()))), None)
        }
        _ => return Ok(None),
    };
    let mut reader = MemberReader::open(open()?, member)?;
    if let Some(reader) = &mut reader {
        reader.reopen =
            Some(MemberReopen { open, data_start: reader.data_start, size: reader.size, index: reader.index.clone(), native_path });
    }
    Ok(reader)
}

#[cfg(test)]
mod tests;
