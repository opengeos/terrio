//! ERDAS Imagine (`.img`, HFA) rasters: the node tree, raster layers in uncompressed or
//! run-length compressed blocks, nodata, the map info and the ESRI PE string that GDAL writes
//! for the CRS.

use super::common;
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
        self.b.get(at..at + 2).map(|s| u16::from_le_bytes([s[0], s[1]])).ok_or_else(|| bad("truncated"))
    }

    fn u32(&self, at: usize) -> Result<u32> {
        self.b.get(at..at + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).ok_or_else(|| bad("truncated"))
    }

    fn f64(&self, at: usize) -> Result<f64> {
        self.b.get(at..at + 8).map(|s| f64::from_le_bytes(s.try_into().unwrap_or_default())).ok_or_else(|| bad("truncated"))
    }

    fn text(&self, at: usize, len: usize) -> String {
        let s = self.b.get(at..at + len).unwrap_or_default();
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

/// Decodes a run-length compressed block into `n` values (integer types only).
fn uncompress(src: &[u8], n: usize) -> Result<Vec<f64>> {
    let i32_at = |k: usize| src.get(k..k + 4).map(|s| i32::from_le_bytes([s[0], s[1], s[2], s[3]])).ok_or_else(|| bad("truncated block"));
    let min = i32_at(0)? as f64;
    let runs = i32_at(4)?;
    let data_offset = i32_at(8)? as usize;
    let bits = *src.get(12).ok_or_else(|| bad("truncated block"))? as usize;
    let values = src.get(data_offset..).unwrap_or_default();
    let mut out = Vec::with_capacity(n);
    if runs == -1 {
        for i in 0..n {
            out.push(packed(values, i, bits) as f64 + min);
        }
        return Ok(out);
    }
    let mut c = 13usize;
    for r in 0..runs.max(0) as usize {
        let b0 = *src.get(c).ok_or_else(|| bad("truncated runs"))? as usize;
        let extra = b0 >> 6;
        let mut count = b0 & 0x3f;
        for k in 1..=extra {
            count = (count << 8) | *src.get(c + k).ok_or_else(|| bad("truncated runs"))? as usize;
        }
        c += 1 + extra;
        let v = packed(values, r, bits) as f64 + min;
        out.extend(std::iter::repeat_n(v, count.min(n.saturating_sub(out.len()))));
    }
    out.resize(n, min);
    Ok(out)
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
    let n = cols * rows;
    let mut values = Vec::with_capacity(n * layers.len());
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
        let across = cols.div_ceil(bw.max(1));
        let mut band = vec![nodata.unwrap_or(0.0); n];
        let size = data_type.size_bytes();
        for k in 0..nblocks.min(count) {
            let e = s + 22 + 14 * k;
            let (offset, len, valid, compressed) = (f.u32(e + 2)? as usize, f.u32(e + 6)? as usize, f.u16(e + 10)?, f.u16(e + 12)?);
            if valid == 0 {
                continue;
            }
            let src = f.b.get(offset..offset + len).ok_or_else(|| bad("block out of the file"))?;
            let block: Vec<f64> = if compressed != 0 {
                if matches!(data_type, DataType::F32 | DataType::F64) {
                    return Err(Error::Unsupported("compressed floating-point Imagine blocks are not supported".into()));
                }
                uncompress(src, bw * bh)?
            } else if bits < 8 {
                (0..bw * bh).map(|i| packed(src, i, bits) as f64).collect()
            } else {
                let l = common::Layout {
                    cols: bw * bh,
                    rows: 1,
                    bands: 1,
                    data_type,
                    little_endian: true,
                    interleave: common::Interleave::Bsq,
                    offset: 0,
                    bottom_up: false,
                };
                if src.len() < bw * bh * size {
                    return Err(bad("short block"));
                }
                common::decode(src, &l)?.to_f64_vec()
            };
            let (bx, by) = (k % across, k / across);
            for r in 0..bh {
                let y = by * bh + r;
                if y >= rows {
                    break;
                }
                for c in 0..bw {
                    let x = bx * bw + c;
                    if x < cols {
                        band[y * cols + x] = block[r * bw + c];
                    }
                }
            }
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
    Ok(RasterDataset { raster, driver: RasterDriver::HFA, has_nodata: nodata.is_some() })
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
        assert_eq!(uncompress(&b, 5).unwrap(), vec![11.0, 11.0, 11.0, 15.0, 15.0]);
    }
}
