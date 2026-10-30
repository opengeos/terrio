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
    /// The chunk size of a valid SOZip index, when the member has one.
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Index {
    pub chunk_size: u32,
    pub uncompressed_size: u64,
    pub compressed_size: u64,
    /// The compressed offset of each chunk but the first (whose offset is 0).
    pub offsets: Vec<u64>,
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

    /// Parses an index, checking it against its member as the specification requires.
    fn parse(b: &[u8], member: &CdEntry) -> std::result::Result<Index, String> {
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
        if expected != Some(b.len() as u64) {
            return Err(format!("index of {} bytes does not contain exactly {count} offsets", b.len()));
        }
        // The exact byte-length check also bounds both conversions on 32-bit targets.
        let count = usize::try_from(count).map_err(|_| "index offset count exceeds address space")?;
        let start = 32usize.checked_add(skip).ok_or("index header size exceeds address space")?;
        let offsets: Vec<u64> = (0..count).map(|i| u64_at(start + 8 * i)).collect();
        let mut last = 0;
        for (i, &o) in offsets.iter().enumerate() {
            if o <= last || o >= compressed_size {
                return Err(format!("offset {i} ({o}) not increasing or past the compressed data"));
            }
            last = o;
        }
        Ok(Index { chunk_size, uncompressed_size, compressed_size, offsets })
    }

    /// The compressed offset of chunk `k`.
    fn offset(&self, k: usize) -> u64 {
        if k == 0 { 0 } else { self.offsets[k - 1] }
    }

    fn chunks(&self) -> usize {
        self.offsets.len() + 1
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
    if e.method != 8 || e.is_dir() {
        return Ok(Ok(None));
    }
    let start = zipdir::data_offset(r, e)?;
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
        let zip64 = e.compressed_size > 0xFFFF_FFFF || e.uncompressed_size > 0xFFFF_FFFF;
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
    if zipdir::decode_name(&name, true) != index_name(&e.name) {
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
    if size > 1 << 31 {
        return Ok(Err("index file too large".into()));
    }
    let index_start = r.stream_position()?;
    let archive_end = r.seek(SeekFrom::End(0))?;
    if size > archive_end.saturating_sub(index_start) {
        return Ok(Err("index data extends beyond the archive".into()));
    }
    r.seek(SeekFrom::Start(index_start))?;
    let mut b = Vec::new();
    b.try_reserve_exact(size as usize).map_err(|_| Error::InvalidData("cannot allocate SOZip index".into()))?;
    b.resize(size as usize, 0);
    r.read_exact(&mut b)?;
    Ok(Index::parse(&b, e).map(Some))
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
}

fn dos_time(secs: Option<i64>) -> (u16, u16) {
    let s = super::format_time(secs.unwrap_or(315_532_800).max(315_532_800));
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
        Self { out, offset, kept, written: Vec::new() }
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
            h.extend_from_slice(&((z64.len() + w.extra.len()) as u16).to_le_bytes());
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
        e.extend_from_slice(&0u16.to_le_bytes());
        self.write(&e)?;
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

/// Adds one member, compressing what `fill` writes: seek-optimized when `sozip` is the chunk
/// size, stored when empty.
#[allow(clippy::too_many_arguments)]
fn add_member<W: Write>(
    zip: &mut ZipOut<W>,
    name: &str,
    size: u64,
    modified: Option<i64>,
    sozip: Option<u32>,
    extra: Vec<u8>,
    fill: impl FnOnce(&mut dyn Write) -> Result<u64>,
) -> Result<u64> {
    let time = dos_time(modified);
    let mut d = Deflater::new(Spool::new(size)?, sozip);
    let n = fill(&mut d)?;
    let (spool, compressed, offsets, crc) = d.finish()?;
    let index = match sozip {
        Some(chunk_size) if n > chunk_size as u64 => Some(Index { chunk_size, uncompressed_size: n, compressed_size: compressed, offsets }),
        _ => None,
    };
    zip.add(name, 8, time, crc, n, &mut *spool.reader()?, compressed, extra, index.as_ref())?;
    Ok(n)
}

fn sozip_for(size: u64, options: &SozipOptions) -> Option<u32> {
    let big_enough = size > options.chunk_size as u64;
    match options.enable {
        EnableSozip::No => None,
        EnableSozip::Yes => big_enough.then_some(options.chunk_size),
        EnableSozip::Auto => (big_enough && size >= options.min_file_size).then_some(options.chunk_size),
    }
}

/// An output archive being written: local or in memory, replaced in full once complete (a
/// failure leaves an existing archive as it was), or added to.
struct Output {
    node: Node,
    path: String,
}

type OpenedOutput = (Output, Vec<u8>, Vec<Vec<u8>>, Vec<String>);

impl Output {
    fn open(path: &str, overwrite: bool) -> Result<OpenedOutput> {
        let node = Node::parse(path)?;
        if !matches!(node, Node::Local(_) | Node::Mem(_)) {
            return Err(Error::Unsupported(format!("'{path}': SOZip archives are written to local files or /vsimem/")));
        }
        // An existing archive is added to: its members kept, their central records re-written.
        let mut prefix = Vec::new();
        let mut kept = Vec::new();
        let mut names = Vec::new();
        if !overwrite && super::stat_node(&node)?.is_some_and(|s| !s.dir) {
            let mut bytes = Vec::new();
            read_into(&node, &mut bytes)?;
            let (cd, cd_offset) = zipdir::central_directory_at(&mut std::io::Cursor::new(&bytes))?;
            bytes.truncate(cd_offset as usize);
            prefix = bytes;
            kept = cd.iter().map(|e| e.raw.clone()).collect();
            names = cd.into_iter().map(|e| e.name).collect();
        }
        Ok((Self { node, path: path.to_string() }, prefix, kept, names))
    }

    fn commit(self, bytes: Vec<u8>) -> Result<()> {
        match &self.node {
            Node::Local(p) => super::write_local_atomic(p, |f| Ok(f.write_all(&bytes)?)),
            _ => super::write(&self.path, &bytes),
        }
    }
}

type InputFile = (Node, String, u64, Option<i64>);

/// The files to add for an input path: `(source node, name in the archive, size, modified)`.
fn inputs_of(input: &str, options: &SozipOptions) -> Result<Vec<InputFile>> {
    let node = Node::parse(input)?;
    let stat = super::stat_node(&node)?.ok_or_else(|| not_found(input))?;
    let stored = |path: &str| -> String {
        let p = path.replace('\\', "/");
        if options.junk_paths {
            return p.rsplit('/').next().unwrap_or(&p).to_string();
        }
        // As GDAL stores them: without a leading `/` or drive, `.` and `..` components.
        let p = p.split_once(":/").map_or(p.as_str(), |(_, r)| r);
        p.split('/').filter(|c| !c.is_empty() && *c != "." && *c != "..").collect::<Vec<_>>().join("/")
    };
    if !stat.dir {
        return Ok(vec![(node, stored(input), stat.size.unwrap_or(0), stat.modified)]);
    }
    if !options.recursive {
        return Err(Error::InvalidArgument(format!("'{input}' is a directory: use the recursive option")));
    }
    let base = input.trim_end_matches('/');
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
    let (out, prefix, kept, existing) = Output::open(output, options.overwrite)?;
    let mut names: HashSet<String> = existing.into_iter().collect();
    let offset = prefix.len() as u64;
    let mut zip = ZipOut::new(prefix, offset, kept);
    let mut report = SozipReport::default();
    let extra = options.content_type.as_deref().map(|c| kv_extra(&[("Content-Type", c)])).transpose()?.unwrap_or_default();
    for input in inputs {
        for (node, name, size, modified) in inputs_of(input, options)? {
            if !names.insert(name.clone()) {
                return Err(Error::InvalidArgument(format!("'{name}' is already in '{output}'")));
            }
            let sozip = sozip_for(size, options);
            report.bytes += add_member(&mut zip, &name, size, modified, sozip, extra.clone(), |w| read_into(&node, w))?;
            report.added += 1;
            report.seek_optimized += u64::from(sozip.is_some_and(|c| size > c as u64));
        }
    }
    out.commit(zip.finish()?)?;
    Ok(report)
}

/// Rewrites a zip archive as a SOZip archive, as `gdal vsi sozip optimize` does: every member
/// decompressed and recompressed (seek-optimized as `options.enable` says), names, dates and
/// properties kept, hidden indexes of the input dropped.
pub fn optimize(input: &str, output: &str, options: &SozipOptions) -> Result<SozipReport> {
    if options.chunk_size == 0 {
        return Err(Error::InvalidArgument("the chunk size must not be 0".into()));
    }
    let in_node = Node::parse(input)?;
    let out_node = Node::parse(output)?;
    if super::is_same(&in_node, &out_node) {
        return Err(Error::InvalidArgument("the input and output archives are the same".into()));
    }
    if !options.overwrite && super::stat_node(&out_node)?.is_some() {
        return Err(Error::InvalidArgument(format!("'{output}' exists: use the overwrite option")));
    }
    let (out, ..) = Output::open(output, true)?;
    let mut zip = ZipOut::new(Vec::new(), 0, Vec::new());
    let mut report = SozipReport::default();
    super::with_archive_reader(&in_node, false, |r| {
        for e in zipdir::central_directory(r)? {
            if e.is_dir() {
                zip.add(&e.name, 0, (e.dos_time, e.dos_date), 0, 0, &mut std::io::empty(), 0, Vec::new(), None)?;
                continue;
            }
            let kv: Vec<u8> = keep_kv(&e.extra);
            let sozip = sozip_for(e.uncompressed_size, options);
            let time = (e.dos_time, e.dos_date);
            let mut d = Deflater::new(Spool::new(e.uncompressed_size)?, sozip);
            let n = zipdir::extract(r, &e, &mut d)?;
            let (spool, compressed, offsets, crc) = d.finish()?;
            let index = sozip.filter(|c| n > *c as u64).map(|chunk_size| Index {
                chunk_size,
                uncompressed_size: n,
                compressed_size: compressed,
                offsets,
            });
            zip.add(&e.name, 8, time, crc, n, &mut *spool.reader()?, compressed, kv, index.as_ref())?;
            report.added += 1;
            report.bytes += n;
            report.seek_optimized += u64::from(index.is_some());
        }
        Ok(())
    })?;
    out.commit(zip.finish()?)?;
    Ok(report)
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
        for e in zipdir::central_directory(r)? {
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
        for e in cd.iter().filter(|e| !e.is_dir()) {
            v.messages.push(format!("Testing {}...", e.name));
            let index = read_index(r, e)?;
            let mut bytes = Vec::new();
            if let Err(err) = zipdir::extract(r, e, &mut bytes) {
                v.errors.push(format!("{}: {err}", e.name));
                continue;
            }
            match index {
                Ok(None) => {}
                Err(why) => v.errors.push(format!("{}: invalid SOZip index: {why}", e.name)),
                Ok(Some(index)) => {
                    v.messages.push(format!("  {} has an associated .sozip.idx file", e.name));
                    v.messages.push(format!("  {}: checking index offset values...", e.name));
                    v.messages.push(format!("  {}: checking if chunks can be independently decompressed...", e.name));
                    let start = zipdir::data_offset(r, e)?;
                    match check_chunks(r, start, &index, &bytes) {
                        Ok(()) => {
                            v.messages.push(format!("* File {} has a valid SOZip index, using chunk_size = {}.", e.name, index.chunk_size));
                            v.sozip_files.push(e.name.clone());
                        }
                        Err(why) => v.errors.push(format!("{}: {why}", e.name)),
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

/// Decompresses each chunk alone from its offset and compares it with the member's bytes.
fn check_chunks(r: &mut dyn ReadSeek, data_start: u64, index: &Index, bytes: &[u8]) -> std::result::Result<(), String> {
    let cs = index.chunk_size as usize;
    for k in 0..index.chunks() {
        let expected = &bytes[(k * cs).min(bytes.len())..((k + 1) * cs).min(bytes.len())];
        let got = inflate_chunk(r, data_start, index, k, expected.len()).map_err(|e| format!("chunk {k}: {e}"))?;
        if got != expected {
            return Err(format!("chunk {k} (at compressed offset {}) does not decompress independently", index.offset(k)));
        }
    }
    Ok(())
}

/// Inflates `len` bytes of chunk `k`, starting a fresh decompressor at the chunk's offset.
fn inflate_chunk(r: &mut dyn ReadSeek, data_start: u64, index: &Index, k: usize, len: usize) -> std::io::Result<Vec<u8>> {
    let from = index.offset(k);
    let to = if k + 1 < index.chunks() { index.offset(k + 1) } else { index.compressed_size };
    let invalid = |message| std::io::Error::new(std::io::ErrorKind::InvalidData, message);
    let offset = data_start.checked_add(from).ok_or_else(|| invalid("chunk offset overflow"))?;
    let size = usize::try_from(to - from).map_err(|_| invalid("chunk exceeds address space"))?;
    r.seek(SeekFrom::Start(offset))?;
    let mut compressed = Vec::new();
    compressed.try_reserve_exact(size).map_err(|_| invalid("cannot allocate compressed chunk"))?;
    compressed.resize(size, 0);
    r.read_exact(&mut compressed)?;
    if k + 1 < index.chunks() {
        // Annex F of the SOZip specification: turn the byte-aligned full-flush
        // block into a final block, then require complete independent decoding.
        if !compressed.ends_with(&[0, 0, 0, 255, 255]) {
            return Err(invalid("chunk does not end with the SOZip full-flush marker"));
        }
        compressed[size - 5] = 1;
    }
    // Grow with actual decoded output, never with the untrusted advertised
    // chunk size. A truncated small stream cannot trigger a multi-GiB zero fill.
    let mut out = Vec::new();
    let mut scratch = [0u8; 64 * 1024];
    let mut decoder = flate2::Decompress::new(false);
    loop {
        let before = (decoder.total_in(), decoder.total_out());
        let status = decoder
            .decompress(&compressed[decoder.total_in() as usize..], &mut scratch, flate2::FlushDecompress::Finish)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let made = (decoder.total_out() - before.1) as usize;
        if out.len().checked_add(made).is_none_or(|size| size > len) {
            return Err(invalid("chunk has excess decoded output"));
        }
        out.try_reserve(made).map_err(|_| invalid("cannot allocate decoded chunk"))?;
        out.extend_from_slice(&scratch[..made]);
        if status == flate2::Status::StreamEnd {
            if decoder.total_in() != size as u64 || out.len() != len {
                return Err(invalid("chunk compressed or decoded size mismatch"));
            }
            return Ok(out);
        }
        if before == (decoder.total_in(), decoder.total_out()) {
            return Err(invalid("chunk stream is truncated"));
        }
    }
}

// --- Random access -------------------------------------------------------------------------------

/// A zip member read at any position: stored members directly, SOZip members by inflating the
/// chunk holding a position (one chunk cached).
pub(crate) struct MemberReader {
    inner: Box<dyn ReadSeek + Send + Sync>,
    data_start: u64,
    size: u64,
    index: Option<Index>,
    pos: u64,
    cached: Option<(usize, Vec<u8>)>,
}

impl MemberReader {
    /// Opens `inner` (a zip archive) at a member, when it can be read at random positions:
    /// stored, or Deflate with a valid SOZip index. `None` otherwise.
    pub(crate) fn open(mut inner: Box<dyn ReadSeek + Send + Sync>, member: &str) -> Result<Option<Self>> {
        let cd = zipdir::central_directory(&mut *inner)?;
        let e = cd.iter().find(|e| super::clean_member(&e.name) == member).ok_or_else(|| not_found(member))?;
        let data_start = zipdir::data_offset(&mut *inner, e)?;
        let index = match e.method {
            0 => None,
            8 => match read_index(&mut *inner, e)? {
                Ok(Some(i)) => Some(i),
                _ => return Ok(None),
            },
            _ => return Ok(None),
        };
        if e.flags & 1 != 0 {
            return Ok(None);
        }
        Ok(Some(Self { inner, data_start, size: e.uncompressed_size, index, pos: 0, cached: None }))
    }
}

impl Read for MemberReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.pos >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let Some(index) = &self.index else {
            let n = (buf.len() as u64).min(self.size - self.pos) as usize;
            self.inner.seek(SeekFrom::Start(self.data_start + self.pos))?;
            let n = self.inner.read(&mut buf[..n])?;
            self.pos += n as u64;
            return Ok(n);
        };
        let cs = index.chunk_size as u64;
        let k = (self.pos / cs) as usize;
        if self.cached.as_ref().is_none_or(|(c, _)| *c != k) {
            let len = (cs.min(self.size - k as u64 * cs)) as usize;
            let chunk = inflate_chunk(&mut *self.inner, self.data_start, index, k, len)?;
            if chunk.len() != len {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, format!("SOZip chunk {k} is short")));
            }
            self.cached = Some((k, chunk));
        }
        let (_, chunk) = self.cached.as_ref().unwrap_or_else(|| unreachable!());
        let off = (self.pos - k as u64 * cs) as usize;
        let n = buf.len().min(chunk.len() - off);
        buf[..n].copy_from_slice(&chunk[off..off + n]);
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
    let inner: Box<dyn ReadSeek + Send + Sync> = match archive {
        crate::vfs::Location::Local(p) => {
            if !Path::new(p).is_file() {
                return Ok(None);
            }
            Box::new(std::io::BufReader::new(std::fs::File::open(p)?))
        }
        #[cfg(remote)]
        crate::vfs::Location::Url(_) | crate::vfs::Location::S3 { .. } => {
            let remote = archive.remote().ok_or_else(|| Error::InvalidArgument("not remote".into()))?;
            let mut options = crate::http::HttpOptions::from_env();
            if std::env::var_os("TERRIO_HTTP_BLOCK_SIZE").is_none() && std::env::var_os("CPL_VSIL_CURL_CHUNK_SIZE").is_none() {
                options.block_size = 16 * 1024;
            }
            Box::new(crate::http::RangeReader::open_with(&remote, options)?)
        }
        _ => return Ok(None),
    };
    MemberReader::open(inner, member)
}

#[cfg(test)]
mod tests;
