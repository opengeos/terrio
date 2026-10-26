//! A zip archive's central directory and members, read with few seeks: the end of the archive
//! once, the central directory once, a member's local header and data when it is read. Remote
//! archives (read through [`crate::http::RangeReader`]) are listed with one or two range
//! requests, where the `zip` crate scans the whole archive for its end records.

use super::ReadSeek;
use crate::{Error, Result};
use std::io::{Read, SeekFrom};

const EOCD: u32 = 0x0605_4b50;
const ZIP64_LOCATOR: u32 = 0x0706_4b50;
const ZIP64_EOCD: u32 = 0x0606_4b50;
const CENTRAL: u32 = 0x0201_4b50;
pub(crate) const LOCAL: u32 = 0x0403_4b50;

/// An entry of the central directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CdEntry {
    /// The member's name as stored (UTF-8, or CP437 read as Latin-1 when not flagged UTF-8).
    pub name: String,
    pub flags: u16,
    /// 0: stored, 8: deflate.
    pub method: u16,
    pub dos_time: u16,
    pub dos_date: u16,
    pub crc32: u32,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
    /// The offset of the member's local header.
    pub local_offset: u64,
}

impl CdEntry {
    pub(crate) fn is_dir(&self) -> bool {
        self.name.ends_with('/') || self.name.ends_with('\\')
    }

    /// The modification time, as stored (a local time without zone, read as UTC).
    pub(crate) fn modified(&self) -> Option<i64> {
        let (d, t) = (self.dos_date as i64, self.dos_time as i64);
        let (y, m, day) = (1980 + (d >> 9), (d >> 5) & 15, d & 31);
        if !(1..=12).contains(&m) || day == 0 {
            return None;
        }
        Some(super::civil_seconds(y, m, day, t >> 11, (t >> 5) & 63, (t & 31) * 2))
    }
}

fn u16_at(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes([b[i], b[i + 1]])
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

fn u64_at(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap_or([0; 8]))
}

fn invalid(what: &str) -> Error {
    Error::InvalidData(format!("not a valid zip archive: {what}"))
}

fn read_at(r: &mut dyn ReadSeek, offset: u64, len: usize) -> Result<Vec<u8>> {
    r.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// A name's bytes as text: UTF-8 when flagged (or valid), else each byte as a character.
pub(crate) fn decode_name(bytes: &[u8], utf8: bool) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) if utf8 => String::from_utf8_lossy(bytes).into_owned(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

/// The entries of the central directory.
pub(crate) fn central_directory(r: &mut dyn ReadSeek) -> Result<Vec<CdEntry>> {
    let len = r.seek(SeekFrom::End(0))?;
    if len < 22 {
        return Err(invalid("too short"));
    }
    // The end record is in the last 22 + 65535 (comment) bytes: the last 4 KiB first, where it
    // is unless the archive has a long comment.
    let find = |tail: &[u8]| (0..=tail.len() - 22).rev().find(|&i| u32_at(tail, i) == EOCD);
    let mut tail_len = len.min(22 + 4096) as usize;
    let mut tail = read_at(r, len - tail_len as u64, tail_len)?;
    let mut found = find(&tail);
    if found.is_none() && (tail_len as u64) < len {
        tail_len = len.min(22 + 65535) as usize;
        tail = read_at(r, len - tail_len as u64, tail_len)?;
        found = find(&tail);
    }
    let tail_start = len - tail_len as u64;
    let at = found.ok_or_else(|| invalid("no end of central directory"))?;
    let e = &tail[at..];
    let mut count = u16_at(e, 10) as u64;
    let mut cd_size = u32_at(e, 12) as u64;
    let mut cd_offset = u32_at(e, 16) as u64;
    let eocd_offset = tail_start + at as u64;
    if (count == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_offset == 0xFFFF_FFFF) && eocd_offset >= 20 {
        let loc = read_at(r, eocd_offset - 20, 20)?;
        if u32_at(&loc, 0) == ZIP64_LOCATOR {
            let z = read_at(r, u64_at(&loc, 8), 56)?;
            if u32_at(&z, 0) != ZIP64_EOCD {
                return Err(invalid("bad zip64 end record"));
            }
            count = u64_at(&z, 32);
            cd_size = u64_at(&z, 40);
            cd_offset = u64_at(&z, 48);
        }
    }
    if cd_offset.checked_add(cd_size).is_none_or(|end| end > len) || cd_size > 1 << 32 {
        return Err(invalid("central directory out of bounds"));
    }
    let cd = read_at(r, cd_offset, cd_size as usize)?;
    let mut out = Vec::with_capacity(count.min(1 << 20) as usize);
    let mut i = 0usize;
    while i + 46 <= cd.len() && u32_at(&cd, i) == CENTRAL {
        let flags = u16_at(&cd, i + 8);
        let method = u16_at(&cd, i + 10);
        let dos_time = u16_at(&cd, i + 12);
        let dos_date = u16_at(&cd, i + 14);
        let crc32 = u32_at(&cd, i + 16);
        let mut compressed_size = u32_at(&cd, i + 20) as u64;
        let mut uncompressed_size = u32_at(&cd, i + 24) as u64;
        let name_len = u16_at(&cd, i + 28) as usize;
        let extra_len = u16_at(&cd, i + 30) as usize;
        let comment_len = u16_at(&cd, i + 32) as usize;
        let mut local_offset = u32_at(&cd, i + 42) as u64;
        let end = i + 46 + name_len + extra_len + comment_len;
        if end > cd.len() {
            return Err(invalid("truncated central directory"));
        }
        let name = decode_name(&cd[i + 46..i + 46 + name_len], flags & 0x800 != 0);
        // The zip64 extra field holds the values stored as 0xFFFFFFFF, in this order.
        let mut extra = &cd[i + 46 + name_len..i + 46 + name_len + extra_len];
        while extra.len() >= 4 {
            let (id, size) = (u16_at(extra, 0), u16_at(extra, 2) as usize);
            let body = &extra[4..(4 + size).min(extra.len())];
            if id == 1 {
                let mut k = 0;
                for v in [&mut uncompressed_size, &mut compressed_size, &mut local_offset] {
                    if *v == 0xFFFF_FFFF && k + 8 <= body.len() {
                        *v = u64_at(body, k);
                        k += 8;
                    }
                }
            }
            extra = &extra[(4 + size).min(extra.len())..];
        }
        out.push(CdEntry { name, flags, method, dos_time, dos_date, crc32, compressed_size, uncompressed_size, local_offset });
        i = end;
    }
    if out.len() as u64 != count {
        return Err(invalid("central directory entry count mismatch"));
    }
    // The only permitted trailing directory record is the optional digital signature.
    if i < cd.len() && !(cd.len() - i >= 6 && u32_at(&cd, i) == 0x0505_4b50 && i + 6 + u16_at(&cd, i + 4) as usize == cd.len()) {
        return Err(invalid("corrupt central directory record"));
    }
    Ok(out)
}

/// The offset of a member's data: past its local header.
pub(crate) fn data_offset(r: &mut dyn ReadSeek, e: &CdEntry) -> Result<u64> {
    let h = read_at(r, e.local_offset, 30)?;
    if u32_at(&h, 0) != LOCAL {
        return Err(invalid(&format!("no local header for '{}'", e.name)));
    }
    Ok(e.local_offset + 30 + u16_at(&h, 26) as u64 + u16_at(&h, 28) as u64)
}

/// Copies a member's uncompressed bytes into `out`, checking its CRC-32.
pub(crate) fn extract(r: &mut dyn ReadSeek, e: &CdEntry, out: &mut dyn std::io::Write) -> Result<u64> {
    if e.flags & 1 != 0 {
        return Err(Error::Unsupported(format!("'{}' is encrypted", e.name)));
    }
    let start = data_offset(r, e)?;
    r.seek(SeekFrom::Start(start))?;
    let raw = (&mut *r).take(e.compressed_size);
    let mut check = Crc32Writer { out, crc: flate2::Crc::new() };
    let n = match e.method {
        0 => std::io::copy(&mut raw.take(e.uncompressed_size.saturating_add(1)), &mut check)?,
        8 => std::io::copy(&mut flate2::read::DeflateDecoder::new(raw).take(e.uncompressed_size.saturating_add(1)), &mut check)?,
        m => return Err(Error::Unsupported(format!("'{}': compression method {m} is not supported", e.name))),
    };
    if n != e.uncompressed_size || check.crc.sum() != e.crc32 {
        return Err(Error::InvalidData(format!("'{}': corrupt member (size or CRC-32 mismatch)", e.name)));
    }
    Ok(n)
}

struct Crc32Writer<'a> {
    out: &'a mut dyn std::io::Write,
    crc: flate2::Crc,
}

impl std::io::Write for Crc32Writer<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.out.write(buf)?;
        self.crc.update(&buf[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn reads_what_the_zip_crate_writes() {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buf);
            let deflated = zip::write::SimpleFileOptions::default();
            let stored = deflated.compression_method(zip::CompressionMethod::Stored);
            zip.add_directory("d/", stored).unwrap();
            zip.start_file("d/a.txt", deflated).unwrap();
            zip.write_all(&b"hello ".repeat(1000)).unwrap();
            zip.start_file("b.bin", stored).unwrap();
            zip.write_all(&[1, 2, 3]).unwrap();
            // Large enough to need zip64 if forced.
            zip.start_file("big.txt", deflated.large_file(true)).unwrap();
            zip.write_all(b"zip64").unwrap();
            let _ = zip.set_comment("a comment".to_string());
            zip.finish().unwrap();
        }
        let mut r = std::io::Cursor::new(buf.into_inner());
        let cd = central_directory(&mut r).unwrap();
        let names: Vec<&str> = cd.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["d/", "d/a.txt", "b.bin", "big.txt"]);
        assert!(cd[0].is_dir());
        let mut out = Vec::new();
        extract(&mut r, &cd[1], &mut out).unwrap();
        assert_eq!(out, b"hello ".repeat(1000));
        out.clear();
        extract(&mut r, &cd[2], &mut out).unwrap();
        assert_eq!(out, [1, 2, 3]);
        out.clear();
        extract(&mut r, &cd[3], &mut out).unwrap();
        assert_eq!(out, b"zip64");
        // Incorrect advertised size cannot cause unbounded decompressed output.
        let mut short = cd[1].clone();
        short.uncompressed_size = 2;
        out.clear();
        assert!(extract(&mut r, &short, &mut out).is_err());
        assert!(out.len() <= 3);
        // A central directory that advertises omitted entries is refused.
        let mut bad_directory = r.get_ref().clone();
        let end_record = bad_directory.windows(4).rposition(|w| w == EOCD.to_le_bytes()).unwrap();
        bad_directory[end_record + 10..end_record + 12].copy_from_slice(&5u16.to_le_bytes());
        assert!(central_directory(&mut std::io::Cursor::new(bad_directory)).is_err());
        // A corrupt byte is detected.
        let mut bytes = r.into_inner();
        let at = data_offset(&mut std::io::Cursor::new(&bytes), &cd[2]).unwrap() as usize;
        bytes[at] ^= 0xFF;
        assert!(extract(&mut std::io::Cursor::new(bytes), &cd[2], &mut Vec::new()).is_err());
        assert!(central_directory(&mut std::io::Cursor::new(b"not a zip at all, really not".to_vec())).is_err());
    }
}
