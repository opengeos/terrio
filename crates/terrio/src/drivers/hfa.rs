// SPDX-FileCopyrightText: Copyright (c) 1999, Intergraph Corporation
// SPDX-FileCopyrightText: Copyright (c) 2007-2012, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.13.3:
//     frmts/hfa/hfaband.cpp

//! ERDAS Imagine (`.img`, HFA) rasters: the node tree, raster layers in uncompressed or
//! run-length compressed blocks, nodata, the map info and the ESRI PE string that GDAL writes
//! for the CRS.

use super::common;
use crate::dataset::fallible;
use crate::formats::RasterDriver;
use crate::{Error, RasterDataset, Result};
use std::path::Path;
use wbraster::DataType;
use wbraster::raster::RasterData;

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("Imagine: {msg}"))
}

/// A node of the HFA tree.
#[derive(Debug, Clone)]
struct Node {
    next: u32,
    child: u32,
    data: u32,
    kind: String,
}

struct File {
    b: Vec<u8>,
}

impl File {
    fn u16(&self, at: usize) -> Result<u16> {
        self.b.get(at..at.saturating_add(2)).map(|s| u16::from_le_bytes([s[0], s[1]])).ok_or_else(|| bad("truncated"))
    }

    fn u32(&self, at: usize) -> Result<u32> {
        self.b.get(at..at.saturating_add(4)).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).ok_or_else(|| bad("truncated"))
    }

    fn f64(&self, at: usize) -> Result<f64> {
        self.b.get(at..at.saturating_add(8)).map(|s| f64::from_le_bytes(s.try_into().unwrap_or_default())).ok_or_else(|| bad("truncated"))
    }

    fn text(&self, at: usize, len: usize) -> String {
        let s = self.b.get(at..at.saturating_add(len)).unwrap_or_default();
        String::from_utf8_lossy(&s[..s.iter().position(|c| *c == 0).unwrap_or(s.len())]).into_owned()
    }

    fn node(&self, at: u32) -> Result<Node> {
        let a = at as usize;
        Ok(Node { next: self.u32(a)?, child: self.u32(a + 12)?, data: self.u32(a + 16)?, kind: self.text(a + 88, 32) })
    }

    /// The children of a node, in order.
    fn children(&self, n: &Node) -> Result<Vec<Node>> {
        let mut out = Vec::new();
        let mut at = n.child;
        while at != 0 && out.len() < 100_000 {
            let c = self.node(at)?;
            at = c.next;
            out.push(c);
        }
        Ok(out)
    }
}

/// Whether the file is an Imagine file.
pub(crate) fn is_hfa(path: &Path) -> bool {
    let mut b = [0u8; 15];
    std::fs::File::open(path).and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b)).is_ok() && &b == b"EHFA_HEADER_TAG"
}

/// Imagine pixel types: `(terrio type, bits per pixel)`.
fn pixel_type(ept: u16) -> Result<(DataType, usize)> {
    Ok(match ept {
        0 => (DataType::U8, 1),
        1 => (DataType::U8, 2),
        2 => (DataType::U8, 4),
        3 => (DataType::U8, 8),
        4 => (DataType::I8, 8),
        5 => (DataType::U16, 16),
        6 => (DataType::I16, 16),
        7 => (DataType::U32, 32),
        8 => (DataType::I32, 32),
        9 => (DataType::F32, 32),
        10 => (DataType::F64, 64),
        t => return Err(Error::Unsupported(format!("Imagine pixel type {t} (complex) is not supported"))),
    })
}

/// Reads `n` packed values of `bits` bits (1, 2 and 4 bits least significant first, 16 and 32
/// bits big-endian), as run-length compressed blocks store them.
fn packed(src: &[u8], i: usize, bits: usize) -> u32 {
    let byte = |k: usize| *src.get(k).unwrap_or(&0) as u32;
    match bits {
        0 => 0,
        1 => (byte(i >> 3) >> (i & 7)) & 1,
        2 => (byte(i >> 2) >> (2 * (i & 3))) & 3,
        4 => (byte(i >> 1) >> (4 * (i & 1))) & 0xf,
        8 => byte(i),
        16 => (byte(2 * i) << 8) | byte(2 * i + 1),
        _ => (byte(4 * i) << 24) | (byte(4 * i + 1) << 16) | (byte(4 * i + 2) << 8) | byte(4 * i + 3),
    }
}

/// The part of a block inside the raster: the block's row length, and its first `cols` columns
/// of its first `rows` rows. Blocks are decoded straight into the band through it, so a block
/// declaring more pixels than the raster holds costs no memory for the pixels outside it.
struct Visible {
    bw: usize,
    cols: usize,
    rows: usize,
}

impl Visible {
    /// Calls `f(row, col)` for each visible pixel of the block's pixels `start..end`.
    fn each(&self, start: usize, end: usize, mut f: impl FnMut(usize, usize)) {
        if start >= end || self.bw == 0 || self.cols == 0 {
            return;
        }
        let last = ((end - 1) / self.bw).min(self.rows.saturating_sub(1));
        for r in start / self.bw..=last {
            if r >= self.rows {
                break;
            }
            let row_start = r * self.bw;
            let c0 = start.saturating_sub(row_start);
            let c1 = (end - row_start).min(self.cols);
            for c in c0..c1 {
                f(r, c);
            }
        }
    }
}

/// Decodes a run-length compressed block of `n` values (integer types only), calling
/// `put(row, col, value)` for its pixels that `visible` keeps. Runs adding up to more than
/// `n` values are cut at the block's end, as GDAL 3.13.3 cuts them (it reads such a block,
/// logging "Repeat count too big" as a debug message only).
fn uncompress(src: &[u8], n: usize, visible: &Visible, mut put: impl FnMut(usize, usize, f64)) -> Result<()> {
    let i32_at = |k: usize| src.get(k..k + 4).map(|s| i32::from_le_bytes([s[0], s[1], s[2], s[3]])).ok_or_else(|| bad("truncated block"));
    let min = i32_at(0)? as f64;
    let runs = i32_at(4)?;
    let data_offset = i32_at(8)? as usize;
    let bits = *src.get(12).ok_or_else(|| bad("truncated block"))? as usize;
    let values = src.get(data_offset..).unwrap_or_default();
    if runs == -1 {
        let row_len = visible.bw;
        visible.each(0, n, |r, c| put(r, c, packed(values, r * row_len + c, bits) as f64 + min));
        return Ok(());
    }
    let mut c = 13usize;
    let mut done = 0usize;
    for r in 0..runs.max(0) as usize {
        let b0 = *src.get(c).ok_or_else(|| bad("truncated runs"))? as usize;
        let extra = b0 >> 6;
        let mut count = b0 & 0x3f;
        for k in 1..=extra {
            count = (count << 8) | *src.get(c + k).ok_or_else(|| bad("truncated runs"))? as usize;
        }
        c += 1 + extra;
        let end = done.saturating_add(count).min(n);
        let v = packed(values, r, bits) as f64 + min;
        visible.each(done, end, |row, col| put(row, col, v));
        done = end;
    }
    visible.each(done, n, |row, col| put(row, col, min));
    Ok(())
}

/// A little-endian value of `t` at the start of `b` (at least its size long).
fn sample(b: &[u8], t: DataType) -> f64 {
    let a = |k: usize| -> [u8; 8] {
        let mut out = [0u8; 8];
        out[..k].copy_from_slice(&b[..k]);
        out
    };
    match t {
        DataType::I8 => b[0] as i8 as f64,
        DataType::U16 => u16::from_le_bytes([b[0], b[1]]) as f64,
        DataType::I16 => i16::from_le_bytes([b[0], b[1]]) as f64,
        DataType::U32 => u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
        DataType::I32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
        DataType::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64,
        DataType::F64 => f64::from_le_bytes(a(8)),
        DataType::U64 => u64::from_le_bytes(a(8)) as f64,
        DataType::I64 => i64::from_le_bytes(a(8)) as f64,
        DataType::U8 => b[0] as f64,
    }
}

/// Reads an Imagine file: every raster layer is a band.
pub(crate) fn read(path: &Path) -> Result<RasterDataset> {
    let f = File { b: std::fs::read(path)? };
    if !f.b.starts_with(b"EHFA_HEADER_TAG") {
        return Err(bad("not an Imagine file"));
    }
    let header = f.u32(16)? as usize;
    let root = f.node(f.u32(header + 8)?)?;
    let layers: Vec<Node> = f.children(&root)?.into_iter().filter(|n| n.kind == "Eimg_Layer").collect();
    let first = layers.first().ok_or_else(|| bad("no raster layer"))?;
    let d = first.data as usize;
    let (cols, rows) = (f.u32(d)? as usize, f.u32(d + 4)? as usize);
    let (data_type, _) = pixel_type(f.u16(d + 10)?)?;
    // What the header declares, before any block is read (see `crate::declared`).
    crate::declared::check(cols, rows, layers.len(), data_type, false)?;
    // Sizes come from the header: a raster no allocator can hold is refused before anything is
    // allocated (the bands' Float64 values, then their converted copy).
    let n = fallible::len(&[cols, rows], "the Imagine raster")?;
    let total = fallible::len(&[n, layers.len()], "the Imagine raster")?;
    // A resource refusal, which the GDAL fallback does not retry.
    fallible::check_usable(total.checked_mul(size_of::<f64>()), || format!("Imagine: a raster of {cols} x {rows} x {}", layers.len()))
        .map_err(Error::ResourceLimit)?;
    let mut values = Vec::new();
    let mut nodata = None;
    let mut map_info = None;
    for layer in &layers {
        let d = layer.data as usize;
        if (f.u32(d)? as usize, f.u32(d + 4)? as usize) != (cols, rows) {
            return Err(Error::Unsupported("Imagine layers of different sizes are not supported".into()));
        }
        let (t, bits) = pixel_type(f.u16(d + 10)?)?;
        if t != data_type {
            return Err(Error::Unsupported("Imagine layers of different types are not supported".into()));
        }
        let (bw, bh) = (f.u32(d + 12)? as usize, f.u32(d + 16)? as usize);
        let children = f.children(layer)?;
        if let Some(nd) = children.iter().find(|c| c.kind == "Eimg_NonInitializedValue") {
            // A pointer (count, offset), then a base data header (rows, columns, type,
            // object type) and the value.
            let at = nd.data as usize + 8;
            let (vt, _) = pixel_type(f.u16(at + 8)?)?;
            let v = at + 12;
            nodata = Some(match vt {
                DataType::F64 => f.f64(v)?,
                DataType::F32 => f32::from_bits(f.u32(v)?) as f64,
                DataType::I32 => f.u32(v)? as i32 as f64,
                DataType::U32 => f.u32(v)? as f64,
                DataType::I16 => f.u16(v)? as i16 as f64,
                DataType::U16 => f.u16(v)? as f64,
                _ => *f.b.get(v).ok_or_else(|| bad("truncated"))? as f64,
            });
        }
        if map_info.is_none() {
            map_info = children.iter().find(|c| c.kind == "Eprj_MapInfo").cloned();
        }
        let dms = children.iter().find(|c| c.kind == "Edms_State").ok_or_else(|| bad("a layer has no block index"))?;
        let s = dms.data as usize;
        let nblocks = f.u32(s)? as usize;
        let count = f.u32(s + 14)? as usize;
        // GDAL 3.13.3's bounds: blocks of 1 to INT_MAX pixels (it reads a block of 46340 x 46340
        // pixels and refuses one of 46341 x 46341).
        let block_len = bw.checked_mul(bh).filter(|n| (1..=i32::MAX as usize).contains(n));
        let block_len = block_len.ok_or_else(|| bad(format!("a block of {bw} x {bh} pixels")))?;
        let (across, down) = (cols.div_ceil(bw), rows.div_ceil(bh));
        // GDAL 3.13.3 reads an entry for each of the raster's blocks and refuses an index with
        // fewer ("Cannot read blockinfo[1].offset" for a two-block raster indexing one): refused
        // here too, as are entries (14 bytes each) past the end of the file.
        let needed = across.checked_mul(down).filter(|b| *b <= nblocks.min(count));
        let needed = needed.ok_or_else(|| bad(format!("{} block entries for a raster of {across} x {down} blocks", nblocks.min(count))))?;
        if s.checked_add(22).and_then(|e| e.checked_add(needed.checked_mul(14)?)).is_none_or(|e| e > f.b.len()) {
            return Err(bad("the block index runs past the end of the file"));
        }
        let mut band = fallible::filled(n, nodata.unwrap_or(0.0), "an Imagine band")?;
        let size = data_type.size_bytes();
        for k in 0..needed {
            let e = s + 22 + 14 * k;
            let (offset, len, valid, compressed) = (f.u32(e + 2)? as usize, f.u32(e + 6)? as usize, f.u16(e + 10)?, f.u16(e + 12)?);
            if valid == 0 {
                continue;
            }
            let src = f.b.get(offset..offset.saturating_add(len)).ok_or_else(|| bad("block out of the file"))?;
            // Block (bx, by) starts at pixel (x0, y0) of the band; only its pixels inside the band
            // are decoded.
            let (x0, y0) = ((k % across) * bw, (k / across) * bh);
            let visible = Visible { bw, cols: bw.min(cols - x0), rows: bh.min(rows - y0) };
            let mut put = |r: usize, c: usize, v: f64| band[(y0 + r) * cols + x0 + c] = v;
            if compressed != 0 {
                if matches!(data_type, DataType::F32 | DataType::F64) {
                    return Err(Error::Unsupported("compressed floating-point Imagine blocks are not supported".into()));
                }
                uncompress(src, block_len, &visible, &mut put)?;
            } else if bits < 8 {
                visible.each(0, block_len, |r, c| put(r, c, packed(src, r * bw + c, bits) as f64));
            } else {
                if block_len.checked_mul(size).is_none_or(|b| src.len() < b) {
                    return Err(bad("short block"));
                }
                // Each visible sample decoded straight into the band: measured faster than a bulk
                // decode of the block through a Float64 buffer (Byte and Float32 rasters of 64 MB).
                visible.each(0, block_len, |r, c| {
                    let at = (r * bw + c) * size;
                    put(r, c, sample(&src[at..at + size], data_type));
                });
            }
        }
        if values.is_empty() {
            fallible::reserve(&mut values, total, "the Imagine raster")?;
        }
        values.extend(band);
    }

    let mut gt = common::NO_GEOTRANSFORM;
    if let Some(m) = map_info {
        // Projection name, upper-left pixel centre, lower-right pixel centre, pixel size, each a
        // pointer (count, offset) followed by its data.
        let mut at = m.data as usize;
        let name_len = f.u32(at)? as usize;
        at += 8 + name_len;
        let (ulx, uly) = (f.f64(at + 8)?, f.f64(at + 16)?);
        at += 24 + 24;
        let (pw, ph) = (f.f64(at + 8)?, f.f64(at + 16)?);
        gt = [ulx - pw / 2.0, pw, 0.0, uly + ph / 2.0, 0.0, -ph];
    }
    let data = RasterData::from_f64_vec(data_type, values);
    let crs = super::nodata::hfa_pe_string(path).unwrap_or_default();
    let raster = common::make_raster(cols, rows, layers.len(), data, gt, crs, nodata)?;
    Ok(RasterDataset::with_driver(raster, RasterDriver::HFA, nodata.is_some()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_run_length_blocks() {
        // min 10, 2 runs, values at offset 15, 8 bits: 3 x (10+1), 2 x (10+5).
        let mut b = Vec::new();
        b.extend(10i32.to_le_bytes());
        b.extend(2i32.to_le_bytes());
        b.extend(15i32.to_le_bytes());
        b.push(8);
        b.extend([3u8, 2]);
        b.extend([1u8, 5]);
        let mut out = vec![0.0; 5];
        uncompress(&b, 5, &Visible { bw: 5, cols: 5, rows: 1 }, |_, c, v| out[c] = v).unwrap();
        assert_eq!(out, vec![11.0, 11.0, 11.0, 15.0, 15.0]);
        // Runs past the block's 4 values are cut at its end, as GDAL cuts them.
        let mut out = vec![0.0; 4];
        uncompress(&b, 4, &Visible { bw: 4, cols: 4, rows: 1 }, |_, c, v| out[c] = v).unwrap();
        assert_eq!(out, vec![11.0, 11.0, 11.0, 15.0]);
    }

    /// A 2 KB Imagine file of one Byte layer of `cols` x `rows` pixels in blocks of `bw` x `bh`,
    /// with a block index of `nblocks` entries, each pointing to the run-length compressed
    /// `block` (stored at byte 1500).
    fn imagine(cols: u32, rows: u32, bw: u32, bh: u32, nblocks: u32, block: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 2048];
        let put = |b: &mut Vec<u8>, at: usize, v: &[u8]| b[at..at + v.len()].copy_from_slice(v);
        put(&mut b, 0, b"EHFA_HEADER_TAG");
        put(&mut b, 16, &32u32.to_le_bytes());
        put(&mut b, 40, &100u32.to_le_bytes());
        // Nodes: next, child and data pointers, and the type name at +88.
        for (at, child, data, kind) in [(100, 300u32, 0u32, &b"root"[..]), (300, 500, 900, b"Eimg_Layer"), (500, 0, 1000, b"Edms_State")] {
            put(&mut b, at + 12, &child.to_le_bytes());
            put(&mut b, at + 16, &data.to_le_bytes());
            put(&mut b, at + 88, kind);
        }
        // The layer: width, height, layer type, pixel type 3 (Byte), block width and height.
        put(&mut b, 900, &cols.to_le_bytes());
        put(&mut b, 904, &rows.to_le_bytes());
        put(&mut b, 910, &3u16.to_le_bytes());
        put(&mut b, 912, &bw.to_le_bytes());
        put(&mut b, 916, &bh.to_le_bytes());
        // The block index: its count, then entries of offset, size, valid flag and compression.
        put(&mut b, 1000, &nblocks.to_le_bytes());
        put(&mut b, 1014, &nblocks.to_le_bytes());
        for k in 0..nblocks.min(30) as usize {
            let e = 1022 + 14 * k;
            put(&mut b, e + 2, &1500u32.to_le_bytes());
            put(&mut b, e + 6, &(block.len() as u32).to_le_bytes());
            put(&mut b, e + 10, &1u16.to_le_bytes());
            put(&mut b, e + 12, &1u16.to_le_bytes());
        }
        put(&mut b, 1500, block);
        b
    }

    /// A compressed block of 8-bit values stored as they are (no runs), each `min` more.
    fn packed_block(min: i32, values: &[u8]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend(min.to_le_bytes());
        b.extend((-1i32).to_le_bytes());
        b.extend(13i32.to_le_bytes());
        b.push(8);
        b.extend(values);
        b
    }

    fn read_bytes(bytes: &[u8]) -> Result<RasterDataset> {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("h.img");
        std::fs::write(&p, bytes).unwrap();
        read(&p)
    }

    #[test]
    fn a_raster_larger_than_memory_is_refused() {
        // 4294967295 x 4294967295 pixels in a 2 KB file: their count overflows the size of a
        // buffer of Float64 values (of their count on 32-bit targets): a resource refusal either way.
        let e = read_bytes(&imagine(u32::MAX, u32::MAX, 1, 1, 1, &packed_block(0, &[7]))).unwrap_err();
        assert!(e.is_resource_refusal(), "{e:?}");
        #[cfg(target_pointer_width = "64")]
        assert!(
            matches!(&e, Error::ResourceLimit(m) if m == "Imagine: a raster of 4294967295 x 4294967295 x 1: more bytes than the address space holds"),
            "{e:?}"
        );
        // 4096 x 4096 pixels in single-pixel blocks need 16777216 block entries, not 1: refused,
        // as GDAL 3.13.3 refuses a block index shorter than the raster's blocks.
        let e = read_bytes(&imagine(1 << 12, 1 << 12, 1, 1, 1, &packed_block(0, &[7]))).unwrap_err();
        assert!(e.to_string().contains("block entries"), "{e}");
    }

    #[test]
    fn oversized_blocks_are_refused_and_decoded_only_inside_the_raster() {
        // Blocks of 4294967295 x 4294967295 and 65536 x 65536 pixels: more than the INT_MAX pixels
        // GDAL 3.13.3 allows.
        for size in [u32::MAX, 1 << 16] {
            let e = read_bytes(&imagine(1, 1, size, size, 1, &packed_block(10, &[7]))).unwrap_err();
            assert!(e.to_string().contains("a block of"), "{e}");
        }
        // A 1 x 1 raster in a block of 4096 x 4096 pixels: only its first pixel is decoded.
        let ds = read_bytes(&imagine(1, 1, 4096, 4096, 1, &packed_block(10, &[7]))).unwrap();
        assert_eq!(ds.raster.data.to_f64_vec(), vec![17.0]);
        // A 2 x 2 raster in 2 x 1 blocks: the blocks of rows 0 and 1 both point to (11, 12).
        let ds = read_bytes(&imagine(2, 2, 2, 1, 2, &packed_block(10, &[1, 2]))).unwrap();
        assert_eq!(ds.raster.data.to_f64_vec(), vec![11.0, 12.0, 11.0, 12.0]);
    }

    #[test]
    fn reads_uncompressed_blocks_clipped_to_the_raster() {
        // A 3 x 3 raster in 2 x 2 blocks, each the uncompressed (1, 2, 3, 4): the right column and
        // bottom row of blocks are cut to the raster.
        let mut b = imagine(3, 3, 2, 2, 4, &[1, 2, 3, 4]);
        for k in 0..4 {
            b[1022 + 14 * k + 12] = 0;
        }
        let ds = read_bytes(&b).unwrap();
        assert_eq!(ds.raster.data.to_f64_vec(), vec![1.0, 2.0, 1.0, 3.0, 4.0, 3.0, 1.0, 2.0, 1.0]);
    }

    #[test]
    fn a_run_crossing_a_clipped_row_continues_on_the_next() {
        // A 3 x 2 raster in one 4 x 2 block (its last column outside): a run of 5 values covers
        // row 0 and the first pixel of row 1, then a run of 3 the rest.
        let mut block = Vec::new();
        block.extend(0i32.to_le_bytes());
        block.extend(2i32.to_le_bytes());
        block.extend(15i32.to_le_bytes());
        block.push(8);
        block.extend([5u8, 3]);
        block.extend([1u8, 2]);
        let ds = read_bytes(&imagine(3, 2, 4, 2, 1, &block)).unwrap();
        assert_eq!(ds.raster.data.to_f64_vec(), vec![1.0, 1.0, 1.0, 1.0, 2.0, 2.0]);
    }

    #[test]
    fn runs_past_the_block_are_cut_at_its_end() {
        // A 2 x 1 block holding one run of 3 values: read as 2, as GDAL 3.13.3 reads them.
        let mut block = Vec::new();
        block.extend(0i32.to_le_bytes());
        block.extend(1i32.to_le_bytes());
        block.extend(14i32.to_le_bytes());
        block.push(8);
        block.push(3);
        block.push(9);
        let ds = read_bytes(&imagine(2, 1, 2, 1, 1, &block)).unwrap();
        assert_eq!(ds.raster.data.to_f64_vec(), vec![9.0, 9.0]);
    }
}
