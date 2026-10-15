//! GDAL's internal TIFF masks: 1-bit transparency mask images (`NewSubfileType` 4,
//! `PhotometricInterpretation` 4) holding a per-dataset validity mask, for the full-resolution
//! image and, with `NewSubfileType` 5, for overviews. The tiff crate does not decode them; terrio
//! reads their chunks itself.

use crate::{Error, Result};
use std::io::{Read, Seek, SeekFrom};

/// A mask image of a TIFF file.
#[derive(Debug, Clone)]
pub(crate) struct MaskImage {
    pub width: usize,
    pub height: usize,
    /// Whether the mask is of an overview (`NewSubfileType` 5).
    pub overview: bool,
    tiled: bool,
    /// The tile size, or the width and rows per strip.
    block: (usize, usize),
    compression: u16,
    /// Whether bits fill bytes from the least significant one (`FillOrder` 2).
    lsb_first: bool,
    offsets: Vec<u64>,
    counts: Vec<u64>,
}

/// The integer values of the tags of one IFD, and the offset of the next.
type Ifd = (Vec<(u16, Vec<u64>)>, u64);

fn read_ifd<R: Read + Seek>(f: &mut R, offset: u64, big: bool, le: bool) -> Option<Ifd> {
    let u16_at = |b: &[u8]| if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) };
    let u32_at = |b: &[u8]| {
        let a = [b[0], b[1], b[2], b[3]];
        if le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) }
    };
    let u64_at = |b: &[u8]| {
        let a: [u8; 8] = b[..8].try_into().unwrap_or_default();
        if le { u64::from_le_bytes(a) } else { u64::from_be_bytes(a) }
    };
    f.seek(SeekFrom::Start(offset)).ok()?;
    let count = if big {
        let mut b = [0u8; 8];
        f.read_exact(&mut b).ok()?;
        u64_at(&b)
    } else {
        let mut b = [0u8; 2];
        f.read_exact(&mut b).ok()?;
        u16_at(&b) as u64
    };
    let entry_len = if big { 20 } else { 12 };
    let mut entries = vec![0u8; (count as usize).min(4096) * entry_len];
    f.read_exact(&mut entries).ok()?;
    let mut next = [0u8; 8];
    let next = if big {
        f.read_exact(&mut next).ok()?;
        u64_at(&next)
    } else {
        f.read_exact(&mut next[..4]).ok()?;
        u32_at(&next) as u64
    };
    let mut tags = Vec::new();
    for e in entries.chunks(entry_len) {
        let tag = u16_at(&e[0..2]);
        if !matches!(tag, 254 | 256 | 257 | 258 | 259 | 262 | 266 | 273 | 277 | 278 | 279 | 322 | 323 | 324 | 325) {
            continue;
        }
        let typ = u16_at(&e[2..4]);
        let size = match typ {
            3 => 2,
            4 => 4,
            16 => 8,
            _ => continue,
        };
        let (n, inline, inline_len) = if big { (u64_at(&e[4..12]), &e[12..20], 8) } else { (u32_at(&e[4..8]) as u64, &e[8..12], 4) };
        let len = (n as usize).checked_mul(size)?;
        if len > 1 << 28 {
            continue;
        }
        let data = if len <= inline_len {
            inline[..len].to_vec()
        } else {
            let off = if big { u64_at(inline) } else { u32_at(inline) as u64 };
            let pos = f.stream_position().ok()?;
            f.seek(SeekFrom::Start(off)).ok()?;
            let mut d = vec![0u8; len];
            f.read_exact(&mut d).ok()?;
            f.seek(SeekFrom::Start(pos)).ok()?;
            d
        };
        let values = data
            .chunks(size)
            .map(|c| match size {
                2 => u16_at(c) as u64,
                4 => u32_at(c) as u64,
                _ => u64_at(c),
            })
            .collect();
        tags.push((tag, values));
    }
    Some((tags, next))
}

/// The mask images of a TIFF stream, in file order. Rewinds nothing: the caller seeks as needed.
pub(crate) fn find_masks<R: Read + Seek>(f: &mut R) -> Vec<MaskImage> {
    let mut out = Vec::new();
    let mut h = [0u8; 16];
    if f.seek(SeekFrom::Start(0)).is_err() || f.read_exact(&mut h[..8]).is_err() {
        return out;
    }
    let le = &h[..2] == b"II";
    let version = if le { u16::from_le_bytes([h[2], h[3]]) } else { u16::from_be_bytes([h[2], h[3]]) };
    let big = version == 43;
    let first = if big {
        if f.read_exact(&mut h[8..16]).is_err() {
            return out;
        }
        let a: [u8; 8] = h[8..16].try_into().unwrap_or_default();
        if le { u64::from_le_bytes(a) } else { u64::from_be_bytes(a) }
    } else {
        let a = [h[4], h[5], h[6], h[7]];
        (if le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) }) as u64
    };
    let mut seen = Vec::new();
    let mut offset = first;
    while offset != 0 && seen.len() < 1024 && !seen.contains(&offset) {
        seen.push(offset);
        let Some((tags, next)) = read_ifd(f, offset, big, le) else { break };
        offset = next;
        let get = |t: u16| tags.iter().find(|(k, _)| *k == t).map(|(_, v)| v.as_slice());
        let first = |t: u16| get(t).and_then(|v| v.first().copied());
        let subfile = first(254).unwrap_or(0);
        if subfile & 4 == 0 || first(262) != Some(4) || first(258).unwrap_or(1) != 1 || first(277).unwrap_or(1) != 1 {
            continue;
        }
        let (Some(width), Some(height)) = (first(256), first(257)) else { continue };
        let (width, height) = (width as usize, height as usize);
        let tiled = get(324).is_some();
        let block = if tiled {
            (first(322).unwrap_or(0) as usize, first(323).unwrap_or(0) as usize)
        } else {
            (width, (first(278).unwrap_or(height as u64) as usize).clamp(1, height.max(1)))
        };
        let (offsets, counts) = if tiled { (get(324), get(325)) } else { (get(273), get(279)) };
        let (Some(offsets), Some(counts)) = (offsets, counts) else { continue };
        if block.0 == 0 || block.1 == 0 {
            continue;
        }
        out.push(MaskImage {
            width,
            height,
            overview: subfile & 1 == 1,
            tiled,
            block,
            compression: first(259).unwrap_or(1) as u16,
            lsb_first: first(266) == Some(2),
            offsets: offsets.to_vec(),
            counts: counts.to_vec(),
        });
    }
    out
}

/// Decompresses a chunk.
fn decompress(raw: Vec<u8>, compression: u16) -> Result<Vec<u8>> {
    Ok(match compression {
        1 => raw,
        5 => weezl::decode::Decoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8)
            .decode(&raw)
            .map_err(|e| Error::InvalidData(format!("mask LZW: {e}")))?,
        8 | 32946 => {
            let mut out = Vec::new();
            flate2::read::ZlibDecoder::new(&raw[..]).read_to_end(&mut out)?;
            out
        }
        32773 => {
            let mut out = Vec::new();
            let mut i = 0;
            while i < raw.len() {
                let n = raw[i] as i8;
                i += 1;
                if n >= 0 {
                    let end = (i + n as usize + 1).min(raw.len());
                    out.extend_from_slice(&raw[i..end]);
                    i = end;
                } else if n != -128 && i < raw.len() {
                    out.extend(std::iter::repeat_n(raw[i], (1 - n as i32) as usize));
                    i += 1;
                }
            }
            out
        }
        c => return Err(Error::Unsupported(format!("TIFF masks compressed with method {c} are not supported"))),
    })
}

/// Reads a window `(x, y, width, height)` of a mask image: 255 for valid pixels, 0 for invalid
/// ones, row by row.
pub(crate) fn read_mask<R: Read + Seek>(f: &mut R, m: &MaskImage, window: (usize, usize, usize, usize)) -> Result<Vec<u8>> {
    let (x, y, w, h) = window;
    let (bw, bh) = m.block;
    let across = m.width.div_ceil(bw);
    let row_bytes = bw.div_ceil(8);
    let mut out = vec![0u8; w * h];
    for by in y / bh..(y + h).div_ceil(bh) {
        for bx in x / bw..(x + w).div_ceil(bw) {
            let idx = if m.tiled { by * across + bx } else { by };
            let (Some(&off), Some(&count)) = (m.offsets.get(idx), m.counts.get(idx)) else { continue };
            if count == 0 {
                continue;
            }
            f.seek(SeekFrom::Start(off))?;
            let mut raw = vec![0u8; count as usize];
            f.read_exact(&mut raw)?;
            let bits = decompress(raw, m.compression)?;
            let (x0, y0) = (bx * bw, by * bh);
            for r in y0.max(y)..(y0 + bh).min(y + h).min(m.height) {
                let line = (r - y0) * row_bytes;
                for c in x0.max(x)..(x0 + bw).min(x + w).min(m.width) {
                    let k = c - x0;
                    let byte = bits.get(line + k / 8).copied().unwrap_or(0);
                    let bit = if m.lsb_first { byte >> (k % 8) } else { byte >> (7 - k % 8) } & 1;
                    out[(r - y) * w + (c - x)] = if bit == 1 { 255 } else { 0 };
                }
            }
        }
    }
    Ok(out)
}
