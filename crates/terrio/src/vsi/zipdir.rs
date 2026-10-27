//! A zip archive's central directory and members, read with few seeks: the end of the archive
//! once, the central directory once, a member's local header and data when it is read. Remote
//! archives (read through [`crate::http::RangeReader`]) are listed with one or two range
//! requests, where the `zip` crate scans the whole archive for its end records.

use super::ReadSeek;
use crate::{Error, Result};
use std::io::{BufReader, Read, SeekFrom};

const EOCD: u32 = 0x0605_4b50;
const ZIP64_LOCATOR: u32 = 0x0706_4b50;
const ZIP64_EOCD: u32 = 0x0606_4b50;
const CENTRAL: u32 = 0x0201_4b50;
pub(crate) const LOCAL: u32 = 0x0403_4b50;

/// An entry of the central directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CdEntry {
    /// The member name as UTF-8, with CP437 fallback for unflagged non-UTF-8 names.
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
        crate::civil_time::checked_unix_seconds(y, m, day, t >> 11, (t >> 5) & 63, (t & 31) * 2)
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

/// Unicode mappings for the high half of legacy code page 437.
const CP437: [char; 128] = [
    '\u{c7}', '\u{fc}', '\u{e9}', '\u{e2}', '\u{e4}', '\u{e0}', '\u{e5}', '\u{e7}', '\u{ea}', '\u{eb}', '\u{e8}', '\u{ef}', '\u{ee}',
    '\u{ec}', '\u{c4}', '\u{c5}', '\u{c9}', '\u{e6}', '\u{c6}', '\u{f4}', '\u{f6}', '\u{f2}', '\u{fb}', '\u{f9}', '\u{ff}', '\u{d6}',
    '\u{dc}', '\u{a2}', '\u{a3}', '\u{a5}', '\u{20a7}', '\u{192}', '\u{e1}', '\u{ed}', '\u{f3}', '\u{fa}', '\u{f1}', '\u{d1}', '\u{aa}',
    '\u{ba}', '\u{bf}', '\u{2310}', '\u{ac}', '\u{bd}', '\u{bc}', '\u{a1}', '\u{ab}', '\u{bb}', '\u{2591}', '\u{2592}', '\u{2593}',
    '\u{2502}', '\u{2524}', '\u{2561}', '\u{2562}', '\u{2556}', '\u{2555}', '\u{2563}', '\u{2551}', '\u{2557}', '\u{255d}', '\u{255c}',
    '\u{255b}', '\u{2510}', '\u{2514}', '\u{2534}', '\u{252c}', '\u{251c}', '\u{2500}', '\u{253c}', '\u{255e}', '\u{255f}', '\u{255a}',
    '\u{2554}', '\u{2569}', '\u{2566}', '\u{2560}', '\u{2550}', '\u{256c}', '\u{2567}', '\u{2568}', '\u{2564}', '\u{2565}', '\u{2559}',
    '\u{2558}', '\u{2552}', '\u{2553}', '\u{256b}', '\u{256a}', '\u{2518}', '\u{250c}', '\u{2588}', '\u{2584}', '\u{258c}', '\u{2590}',
    '\u{2580}', '\u{3b1}', '\u{df}', '\u{393}', '\u{3c0}', '\u{3a3}', '\u{3c3}', '\u{b5}', '\u{3c4}', '\u{3a6}', '\u{398}', '\u{3a9}',
    '\u{3b4}', '\u{221e}', '\u{3c6}', '\u{3b5}', '\u{2229}', '\u{2261}', '\u{b1}', '\u{2265}', '\u{2264}', '\u{2320}', '\u{2321}',
    '\u{f7}', '\u{2248}', '\u{b0}', '\u{2219}', '\u{b7}', '\u{221a}', '\u{207f}', '\u{b2}', '\u{25a0}', '\u{a0}',
];

/// A name as text: UTF-8 when flagged (or valid), otherwise legacy CP437.
pub(crate) fn decode_name(bytes: &[u8], utf8: bool) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) if utf8 => String::from_utf8_lossy(bytes).into_owned(),
        Err(_) => bytes.iter().map(|&b| if b < 128 { b as char } else { CP437[usize::from(b - 128)] }).collect(),
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
    if cd_offset.checked_add(cd_size).is_none_or(|end| end > len) {
        return Err(invalid("central directory out of bounds"));
    }
    r.seek(SeekFrom::Start(cd_offset))?;
    // Do not allocate the advertised directory size or entry count. Both can be
    // forged by a remote server; parse validated records through a bounded buffer.
    let mut cd = BufReader::with_capacity(64 * 1024, r.take(cd_size));
    let mut remaining = cd_size;
    let mut out = Vec::new();
    while remaining > 0 {
        if remaining < 4 {
            return Err(invalid("truncated central directory"));
        }
        let mut h = [0; 46];
        cd.read_exact(&mut h[..4])?;
        remaining -= 4;
        match u32_at(&h, 0) {
            0x0505_4b50 => {
                if remaining < 2 {
                    return Err(invalid("truncated digital signature"));
                }
                cd.read_exact(&mut h[4..6])?;
                remaining -= 2;
                let size = u16_at(&h, 4) as usize;
                if remaining != size as u64 {
                    return Err(invalid("corrupt central directory record"));
                }
                cd.read_exact(&mut vec![0; size])?;
                break;
            }
            CENTRAL => {}
            _ => return Err(invalid("corrupt central directory record")),
        }
        if remaining < 42 {
            return Err(invalid("truncated central directory"));
        }
        cd.read_exact(&mut h[4..])?;
        remaining -= 42;
        let flags = u16_at(&h, 8);
        let method = u16_at(&h, 10);
        let dos_time = u16_at(&h, 12);
        let dos_date = u16_at(&h, 14);
        let crc32 = u32_at(&h, 16);
        let mut compressed_size = u32_at(&h, 20) as u64;
        let mut uncompressed_size = u32_at(&h, 24) as u64;
        let name_len = u16_at(&h, 28) as usize;
        let extra_len = u16_at(&h, 30) as usize;
        let comment_len = u16_at(&h, 32) as usize;
        let mut local_offset = u32_at(&h, 42) as u64;
        // Each variable field is u16-sized, so this is at most 196605 bytes.
        let size = name_len + extra_len + comment_len;
        if size as u64 > remaining {
            return Err(invalid("truncated central directory"));
        }
        let mut fields = vec![0; size];
        cd.read_exact(&mut fields)?;
        remaining -= size as u64;
        let name = decode_name(&fields[..name_len], flags & 0x800 != 0);
        // Zip64 stores the values marked 0xFFFFFFFF in this order.
        let mut extra = &fields[name_len..name_len + extra_len];
        while extra.len() >= 4 {
            let (id, size) = (u16_at(extra, 0), u16_at(extra, 2) as usize);
            let body = &extra[4..(4 + size).min(extra.len())];
            if id == 1 {
                let mut k = 0;
                for value in [&mut uncompressed_size, &mut compressed_size, &mut local_offset] {
                    if *value == 0xFFFF_FFFF && k + 8 <= body.len() {
                        *value = u64_at(body, k);
                        k += 8;
                    }
                }
            }
            extra = &extra[(4 + size).min(extra.len())..];
        }
        out.try_reserve(1).map_err(|_| invalid("cannot allocate central directory entries"))?;
        out.push(CdEntry { name, flags, method, dos_time, dos_date, crc32, compressed_size, uncompressed_size, local_offset });
    }
    if out.len() as u64 != count {
        return Err(invalid("central directory entry count mismatch"));
    }
    Ok(out)
}

/// The offset of a member's data: past its local header.
pub(crate) fn data_offset(r: &mut dyn ReadSeek, e: &CdEntry) -> Result<u64> {
    let header_end = e.local_offset.checked_add(30).ok_or_else(|| invalid("local header offset overflow"))?;
    let h = read_at(r, e.local_offset, 30)?;
    if u32_at(&h, 0) != LOCAL {
        return Err(invalid(&format!("no local header for '{}'", e.name)));
    }
    header_end.checked_add(u16_at(&h, 26) as u64 + u16_at(&h, 28) as u64).ok_or_else(|| invalid("member data offset overflow"))
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
    fn legacy_non_utf8_names_use_cp437() {
        assert_eq!(decode_name(b"caf\x82.tif", false), "café.tif");
        assert_eq!(decode_name(b"\xb0\xdb.tif", false), "░█.tif");
        assert_eq!(decode_name("café.tif".as_bytes(), true), "café.tif");
        assert_eq!(decode_name(b"plain.tif", false), "plain.tif");
    }

    #[test]
    fn corrupt_dos_timestamps_are_not_reported_as_valid_dates() {
        let mut entry = CdEntry {
            name: "x".into(),
            flags: 0,
            method: 0,
            dos_time: 0,
            dos_date: (44 << 9) | (2 << 5) | 29,
            crc32: 0,
            compressed_size: 0,
            uncompressed_size: 0,
            local_offset: 0,
        };
        assert!(entry.modified().is_some()); // 2024 is a leap year.
        entry.dos_date = (43 << 9) | (2 << 5) | 29;
        assert!(entry.modified().is_none());
        entry.dos_date = (44 << 9) | (1 << 5) | 1;
        for time in [24 << 11, 60 << 5, 31] {
            entry.dos_time = time;
            assert!(entry.modified().is_none());
        }
    }

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
        let mut oversized = r.get_ref().clone();
        let end = oversized.windows(4).rposition(|w| w == EOCD.to_le_bytes()).unwrap();
        oversized[end + 12..end + 16].copy_from_slice(&(1u32 << 20).to_le_bytes());
        // A malformed record cannot request a directory-sized read/allocation.
        let mut forged = vec![0; (1 << 20) + 22];
        forged[(1 << 20)..].copy_from_slice(&oversized[end..end + 22]);
        forged[(1 << 20) + 16..(1 << 20) + 20].copy_from_slice(&0u32.to_le_bytes());
        struct Counted {
            cursor: std::io::Cursor<Vec<u8>>,
            max_read: usize,
        }
        impl Read for Counted {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.max_read = self.max_read.max(buf.len());
                self.cursor.read(buf)
            }
        }
        impl std::io::Seek for Counted {
            fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
                self.cursor.seek(pos)
            }
        }
        let mut forged = Counted { cursor: std::io::Cursor::new(forged), max_read: 0 };
        assert!(central_directory(&mut forged).is_err());
        assert!(forged.max_read <= 64 * 1024);

        let cd = central_directory(&mut r).unwrap();
        let names: Vec<&str> = cd.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["d/", "d/a.txt", "b.bin", "big.txt"]);
        assert!(cd[0].is_dir());
        let mut overflow = cd[1].clone();
        overflow.local_offset = u64::MAX - 1;
        assert!(data_offset(&mut r, &overflow).unwrap_err().to_string().contains("overflow"));
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
