//! Zarr v2 and v3 stores, as GDAL writes them (an array in a group, `_ARRAY_DIMENSIONS`, X/Y
//! coordinate arrays, the CRS in a `_CRS` attribute) and as xarray writes them (a `grid_mapping`
//! variable with `crs_wkt` and `GeoTransform`).
//!
//! Codecs, all in pure Rust: none, zlib, gzip, zstd, lz4 and Blosc (with the blosclz, lz4, zlib
//! and zstd compressors, and byte or bit shuffling), Fortran order (v2) and transposition (v3),
//! and Zarr v3 sharding. Not supported: v2 filters.
//!
//! GeoZarr: the [`spatial`](https://github.com/zarr-conventions/spatial),
//! [`proj`](https://github.com/zarr-conventions/proj) and
//! [`multiscales`](https://github.com/zarr-conventions/multiscales) Zarr conventions are read
//! (on the array, its group, or the group holding the multiscales, as GDAL reads them), with the
//! lower-resolution levels of a multiscales layout as overviews, read by window
//! ([`crate::LazyRaster`]).
//!
//! Rasters are written as GDAL writes them, in Zarr v2 (the default) or v3, with GDAL's
//! `GEOREFERENCING_CONVENTION` creation option: `GDAL` (the default: `_CRS` and X/Y coordinate
//! arrays), `SPATIAL_PROJ` (GeoZarr's `spatial:` and `proj:` attributes besides those, which GDAL
//! 3.12 and earlier read) or `CF` (a `spatial_ref` grid mapping variable, as rioxarray writes it). With
//! `OVERVIEW_LEVELS=2,4,...` (or `AUTO`, halving until 256 pixels), lower resolutions are written
//! in `ovr_<N>x` groups described by a `multiscales` layout, as GDAL's overview builder lays
//! them out, resampled with `OVERVIEW_RESAMPLING` (`NEAREST` by default; any `terrio translate`
//! method).

use super::common;
use crate::formats::RasterDriver;
use crate::memfs;
use crate::{Error, RasterDataset, Result};
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};
use wbraster::raster::RasterData;
use wbraster::{CrsInfo, DataType, Raster};

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("Zarr: {msg}"))
}

fn read_json(p: &Path) -> Option<Value> {
    serde_json::from_slice(&memfs::read(p).ok()?).ok()
}

/// Whether a directory is a Zarr store (or array).
pub(crate) fn is_zarr(path: &Path) -> bool {
    memfs::is_dir(path) && [".zgroup", ".zarray", "zarr.json"].iter().any(|f| memfs::is_file(&path.join(f)))
}

/// A decompression step.
#[derive(Debug, Clone)]
enum Codec {
    Zlib,
    Gzip,
    Zstd,
    Lz4,
    Blosc,
    /// Zarr v3's CRC32C checksum: four trailing bytes.
    Crc32c,
}

/// How a chunk's bytes encode its elements.
#[derive(Debug, Clone)]
struct Pipeline {
    /// The order of the stored dimensions: stored dimension `i` is the array's `order[i]`. `None`
    /// for C order.
    order: Option<Vec<usize>>,
    little_endian: bool,
    /// The bytes-to-bytes codecs, in decoding order.
    codecs: Vec<Codec>,
    /// A sharded chunk: inner chunks and their index.
    shard: Option<Box<Shard>>,
}

impl Default for Pipeline {
    fn default() -> Self {
        Self { order: None, little_endian: true, codecs: Vec::new(), shard: None }
    }
}

/// Zarr v3 sharding: a chunk (shard) holds inner chunks, located by an index of `(offset,
/// length)` pairs.
#[derive(Debug, Clone)]
struct Shard {
    chunks: Vec<usize>,
    inner: Pipeline,
    /// The index's codecs (a CRC32C checksum adds four bytes).
    index_checksum: bool,
    index_at_start: bool,
}

/// An array's metadata.
#[derive(Debug)]
struct Array {
    dir: PathBuf,
    shape: Vec<usize>,
    chunks: Vec<usize>,
    data_type: DataType,
    pipeline: Pipeline,
    fill: Option<f64>,
    /// Chunk key prefix and separator: `("", ".")` for v2, `("c/", "/")` for v3.
    key_prefix: String,
    separator: String,
    dims: Vec<String>,
    attrs: Value,
}

fn parse_fill(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => match s.as_str() {
            "NaN" => Some(f64::NAN),
            "Infinity" => Some(f64::INFINITY),
            "-Infinity" => Some(f64::NEG_INFINITY),
            _ => s.parse().ok(),
        },
        Value::Bool(b) => Some(*b as u8 as f64),
        _ => None,
    }
}

fn v2_dtype(s: &str) -> Result<(DataType, bool)> {
    let little = !s.starts_with('>');
    let t = match &s[1.min(s.len())..] {
        "u1" | "b1" => DataType::U8,
        "i1" => DataType::I8,
        "u2" => DataType::U16,
        "i2" => DataType::I16,
        "u4" => DataType::U32,
        "i4" => DataType::I32,
        "u8" => DataType::U64,
        "i8" => DataType::I64,
        "f4" => DataType::F32,
        "f8" => DataType::F64,
        other => return Err(Error::Unsupported(format!("Zarr data type '{other}' is not supported"))),
    };
    Ok((t, little))
}

fn v3_dtype(s: &str) -> Result<DataType> {
    Ok(match s {
        "bool" | "uint8" => DataType::U8,
        "int8" => DataType::I8,
        "uint16" => DataType::U16,
        "int16" => DataType::I16,
        "uint32" => DataType::U32,
        "int32" => DataType::I32,
        "uint64" => DataType::U64,
        "int64" => DataType::I64,
        "float32" => DataType::F32,
        "float64" => DataType::F64,
        other => return Err(Error::Unsupported(format!("Zarr data type '{other}' is not supported"))),
    })
}

fn v2_codec(c: &Value) -> Result<Option<Codec>> {
    if c.is_null() {
        return Ok(None);
    }
    Ok(Some(match c["id"].as_str().unwrap_or_default() {
        "zlib" => Codec::Zlib,
        "gzip" => Codec::Gzip,
        "zstd" => Codec::Zstd,
        "lz4" => Codec::Lz4,
        "blosc" => Codec::Blosc,
        other => return Err(Error::Unsupported(format!("Zarr compressor '{other}' is not supported"))),
    }))
}

/// Reads an array's metadata, from a v2 `.zarray` (with `.zattrs`) or a v3 `zarr.json`.
fn read_array(dir: &Path) -> Result<Array> {
    if let Some(m) = read_json(&dir.join(".zarray")) {
        let attrs = read_json(&dir.join(".zattrs")).unwrap_or(json!({}));
        let (data_type, little_endian) = v2_dtype(m["dtype"].as_str().ok_or_else(|| bad("no dtype"))?)?;
        let mut codecs: Vec<Codec> = Vec::new();
        if let Some(filters) = m["filters"].as_array()
            && !filters.is_empty()
        {
            return Err(Error::Unsupported("Zarr filters are not supported".into()));
        }
        codecs.extend(v2_codec(&m["compressor"])?);
        let usizes = |v: &Value| {
            v.as_array().map(|a| a.iter().filter_map(|x| x.as_u64().map(|x| x as usize)).collect::<Vec<_>>()).unwrap_or_default()
        };
        let dims = attrs["_ARRAY_DIMENSIONS"]
            .as_array()
            .map(|a| a.iter().filter_map(|d| d.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let shape = usizes(&m["shape"]);
        // Fortran order: the chunk's dimensions are stored last first.
        let order = (m["order"].as_str() == Some("F")).then(|| (0..shape.len()).rev().collect());
        return Ok(Array {
            dir: dir.to_path_buf(),
            shape,
            chunks: usizes(&m["chunks"]),
            data_type,
            pipeline: Pipeline { order, little_endian, codecs, shard: None },
            fill: parse_fill(&m["fill_value"]),
            key_prefix: String::new(),
            separator: m["dimension_separator"].as_str().unwrap_or(".").to_string(),
            dims,
            attrs,
        });
    }
    let m = read_json(&dir.join("zarr.json")).ok_or_else(|| bad(format!("{} is not a Zarr array", dir.display())))?;
    if m["node_type"].as_str() != Some("array") {
        return Err(bad(format!("{} is not a Zarr array", dir.display())));
    }
    let data_type = v3_dtype(m["data_type"].as_str().unwrap_or_default())?;
    let usizes =
        |v: &Value| v.as_array().map(|a| a.iter().filter_map(|x| x.as_u64().map(|x| x as usize)).collect::<Vec<_>>()).unwrap_or_default();
    let shape = usizes(&m["shape"]);
    let pipeline = v3_pipeline(&m["codecs"], shape.len())?;
    let encoding = &m["chunk_key_encoding"];
    let separator = encoding["configuration"]["separator"]
        .as_str()
        .unwrap_or(if encoding["name"].as_str() == Some("v2") { "." } else { "/" })
        .to_string();
    let key_prefix = if encoding["name"].as_str() == Some("v2") { String::new() } else { format!("c{separator}") };
    let dims =
        m["dimension_names"].as_array().map(|a| a.iter().map(|d| d.as_str().unwrap_or_default().to_string()).collect()).unwrap_or_default();
    Ok(Array {
        dir: dir.to_path_buf(),
        shape,
        chunks: usizes(&m["chunk_grid"]["configuration"]["chunk_shape"]),
        data_type,
        pipeline,
        fill: parse_fill(&m["fill_value"]),
        key_prefix,
        separator,
        dims,
        attrs: m["attributes"].clone(),
    })
}

/// Parses a Zarr v3 codec chain for an array of `nd` dimensions.
fn v3_pipeline(codecs: &Value, nd: usize) -> Result<Pipeline> {
    let mut p = Pipeline::default();
    for c in codecs.as_array().into_iter().flatten() {
        let config = &c["configuration"];
        match c["name"].as_str().unwrap_or_default() {
            "bytes" | "endian" => p.little_endian = config["endian"].as_str() != Some("big"),
            "transpose" => {
                let order: Vec<usize> = match &config["order"] {
                    Value::String(s) if s == "C" => (0..nd).collect(),
                    Value::String(s) if s == "F" => (0..nd).rev().collect(),
                    Value::Array(a) => a.iter().filter_map(|x| x.as_u64().map(|x| x as usize)).collect(),
                    _ => return Err(bad("transpose without an order")),
                };
                let mut sorted = order.clone();
                sorted.sort_unstable();
                if sorted != (0..nd).collect::<Vec<_>>() {
                    return Err(bad(format!("invalid transpose order {order:?}")));
                }
                // Successive transpositions compose.
                p.order = Some(match &p.order {
                    Some(prev) => order.iter().map(|&i| prev[i]).collect(),
                    None => order,
                });
            }
            "sharding_indexed" => {
                let chunks: Vec<usize> =
                    config["chunk_shape"].as_array().into_iter().flatten().filter_map(|x| x.as_u64().map(|x| x as usize)).collect();
                if chunks.len() != nd || chunks.contains(&0) {
                    return Err(bad("sharding: invalid inner chunk shape"));
                }
                let inner = v3_pipeline(&config["codecs"], nd)?;
                let index = config["index_codecs"].as_array().cloned().unwrap_or_default();
                let mut index_checksum = false;
                for c in &index {
                    match c["name"].as_str().unwrap_or_default() {
                        "bytes" if c["configuration"]["endian"].as_str() == Some("big") => {
                            return Err(Error::Unsupported("big-endian shard indexes are not supported".into()));
                        }
                        "bytes" => {}
                        "crc32c" => index_checksum = true,
                        other => return Err(Error::Unsupported(format!("shard index codec '{other}' is not supported"))),
                    }
                }
                let index_at_start = config["index_location"].as_str() == Some("start");
                p.shard = Some(Box::new(Shard { chunks, inner, index_checksum, index_at_start }));
            }
            "gzip" => p.codecs.push(Codec::Gzip),
            "zstd" => p.codecs.push(Codec::Zstd),
            "blosc" => p.codecs.push(Codec::Blosc),
            "crc32c" => p.codecs.push(Codec::Crc32c),
            other => return Err(Error::Unsupported(format!("Zarr codec '{other}' is not supported"))),
        }
    }
    // Decoding applies the bytes-to-bytes codecs in reverse.
    p.codecs.reverse();
    Ok(p)
}

/// Decodes one chunk's bytes.
fn decompress(codec: &Codec, b: Vec<u8>) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    match codec {
        Codec::Zlib => {
            flate2::read::ZlibDecoder::new(&b[..]).read_to_end(&mut out)?;
        }
        Codec::Gzip => {
            flate2::read::GzDecoder::new(&b[..]).read_to_end(&mut out)?;
        }
        Codec::Zstd => out = zstd_decode(&b)?,
        Codec::Lz4 => {
            // numcodecs' LZ4: the uncompressed size (u32, little-endian), then an LZ4 block.
            out = lz4_flex::block::decompress_size_prepended(&b).map_err(|e| bad(format!("LZ4: {e}")))?;
        }
        Codec::Blosc => out = blosc_decompress(&b)?,
        Codec::Crc32c => {
            out = b;
            out.truncate(out.len().saturating_sub(4));
        }
    }
    Ok(out)
}

fn zstd_decode(b: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut src = b;
    while !src.is_empty() {
        let mut d = ruzstd::decoding::StreamingDecoder::new(&mut src).map_err(|e| bad(format!("zstd: {e}")))?;
        d.read_to_end(&mut out)?;
    }
    Ok(out)
}

/// Decompresses a Blosc (version 1 format) buffer.
fn blosc_decompress(src: &[u8]) -> Result<Vec<u8>> {
    if src.len() < 16 {
        return Err(bad("Blosc: truncated header"));
    }
    let flags = src[2];
    let typesize = src[3].max(1) as usize;
    let u32_at = |i: usize| src.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    let nbytes = u32_at(4).unwrap_or(0);
    let blocksize = u32_at(8).unwrap_or(0).max(1);
    if flags & 0x02 != 0 {
        return src.get(16..16 + nbytes).map(<[u8]>::to_vec).ok_or_else(|| bad("Blosc: truncated data"));
    }
    let compressor = flags >> 5;
    let nblocks = nbytes.div_ceil(blocksize);
    let mut out = Vec::with_capacity(nbytes);
    for j in 0..nblocks {
        let bsize = blocksize.min(nbytes - j * blocksize);
        let leftover = bsize < blocksize;
        let split = flags & 0x10 == 0 && typesize <= 16 && blocksize / typesize >= 128 && !leftover;
        let nsplits = if split { typesize } else { 1 };
        let mut pos = u32_at(16 + 4 * j).ok_or_else(|| bad("Blosc: truncated block offsets"))?;
        let mut block = Vec::with_capacity(bsize);
        for _ in 0..nsplits {
            let neblock = bsize / nsplits;
            let cbytes = u32_at(pos).ok_or_else(|| bad("Blosc: truncated block"))?;
            pos += 4;
            let stream = src.get(pos..pos + cbytes).ok_or_else(|| bad("Blosc: truncated stream"))?;
            pos += cbytes;
            if cbytes == neblock {
                block.extend_from_slice(stream);
                continue;
            }
            let decoded = match compressor {
                0 => blosclz_decompress(stream, neblock)?,
                1 => lz4_flex::block::decompress(stream, neblock).map_err(|e| bad(format!("Blosc LZ4: {e}")))?,
                3 => {
                    let mut v = Vec::new();
                    flate2::read::ZlibDecoder::new(stream).read_to_end(&mut v)?;
                    v
                }
                4 => zstd_decode(stream)?,
                c => return Err(Error::Unsupported(format!("Blosc compressor {c} is not supported"))),
            };
            block.extend_from_slice(&decoded[..neblock.min(decoded.len())]);
        }
        if flags & 0x01 == 0 && flags & 0x04 != 0 && bsize >= typesize {
            block = bit_unshuffle(&block, bsize, typesize, src[0]);
        } else if flags & 0x01 != 0 && typesize > 1 {
            // Undo the byte shuffle: the block holds byte 0 of every element, then byte 1...
            let n = bsize / typesize;
            let mut unshuffled = vec![0u8; bsize];
            for (e, item) in unshuffled.chunks_mut(typesize).enumerate().take(n) {
                for (k, byte) in item.iter_mut().enumerate() {
                    *byte = block[k * n + e];
                }
            }
            unshuffled[n * typesize..].copy_from_slice(&block[n * typesize..bsize]);
            block = unshuffled;
        }
        out.extend_from_slice(&block);
    }
    Ok(out)
}

/// Undoes Blosc's bit shuffle of a block: bit `kk` of byte `j` of element `i` is stored at bit
/// `(j * 8 + kk) * n + i` (least significant bit first), over a multiple of 8 elements (Blosc
/// format version 2 leaves blocks of other sizes as they are; later versions copy the rest).
fn bit_unshuffle(block: &[u8], bsize: usize, typesize: usize, version: u8) -> Vec<u8> {
    let nelems = bsize / typesize;
    if version <= 2 && !nelems.is_multiple_of(8) {
        return block[..bsize].to_vec();
    }
    let n = nelems - nelems % 8;
    let mut out = vec![0u8; bsize];
    for j in 0..typesize {
        for kk in 0..8 {
            let row = (j * 8 + kk) * n;
            for i in 0..n {
                let p = row + i;
                if (block[p / 8] >> (p % 8)) & 1 != 0 {
                    out[i * typesize + j] |= 1 << kk;
                }
            }
        }
    }
    out[n * typesize..bsize].copy_from_slice(&block[n * typesize..bsize]);
    out
}

/// BloscLZ decompression (a FastLZ variant).
fn blosclz_decompress(src: &[u8], maxout: usize) -> Result<Vec<u8>> {
    const MAX_DISTANCE: usize = 8191;
    let err = || bad("BloscLZ: corrupt stream");
    let mut out: Vec<u8> = Vec::with_capacity(maxout);
    let mut ip = 0usize;
    let mut ctrl = (*src.first().ok_or_else(err)? & 31) as usize;
    ip += 1;
    loop {
        if ctrl >= 32 {
            let mut len = (ctrl >> 5) - 1;
            let mut ofs = (ctrl & 31) << 8;
            if len == 6 {
                loop {
                    let code = *src.get(ip).ok_or_else(err)?;
                    ip += 1;
                    len += code as usize;
                    if code != 255 {
                        break;
                    }
                }
            }
            let code = *src.get(ip).ok_or_else(err)? as usize;
            ip += 1;
            len += 3;
            let mut back = ofs + code + 1;
            if code == 255 && ofs == (31 << 8) {
                ofs = ((*src.get(ip).ok_or_else(err)? as usize) << 8) | *src.get(ip + 1).ok_or_else(err)? as usize;
                ip += 2;
                back = ofs + MAX_DISTANCE + 1;
            }
            let start = out.len().checked_sub(back).ok_or_else(err)?;
            for k in 0..len {
                let b = out[start + k];
                out.push(b);
            }
        } else {
            let n = ctrl + 1;
            out.extend_from_slice(src.get(ip..ip + n).ok_or_else(err)?);
            ip += n;
        }
        if ip >= src.len() {
            break;
        }
        ctrl = src[ip] as usize;
        ip += 1;
    }
    Ok(out)
}

/// Reads an array's values (row-major over its shape).
fn read_values(a: &Array) -> Result<RasterData> {
    let n: usize = a.shape.iter().product();
    let size = a.data_type.size_bytes();
    let nd = a.shape.len();
    if a.chunks.len() != nd {
        return Err(bad("chunk shape and array shape differ in rank"));
    }
    let mut bytes = vec![0u8; n * size];
    // Fill with the fill value, for missing chunks.
    if let Some(fill) = a.fill
        && fill != 0.0
    {
        let mut one = Vec::new();
        super::gtiff_write::encode_sample_le(a.data_type, fill, &mut one);
        for c in bytes.chunks_mut(size) {
            c.copy_from_slice(&one);
        }
    }
    let grid: Vec<usize> = a.shape.iter().zip(&a.chunks).map(|(s, c)| s.div_ceil((*c).max(1))).collect();
    let total: usize = grid.iter().product();
    let mut idx = vec![0usize; nd];
    let mut out = Output { bytes: &mut bytes, shape: &a.shape, size };
    for _ in 0..total {
        let key = format!("{}{}", a.key_prefix, idx.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(&a.separator));
        let key = if nd == 0 { format!("{}0", a.key_prefix) } else { key };
        if let Ok(raw) = memfs::read(&a.dir.join(&key)) {
            let origin: Vec<usize> = idx.iter().zip(&a.chunks).map(|(i, c)| i * c).collect();
            decode_chunk(&a.pipeline, raw, &origin, &a.chunks, &mut out).map_err(|e| bad(format!("chunk {key}: {e}")))?;
        }
        for d in (0..nd).rev() {
            idx[d] += 1;
            if idx[d] < grid[d] {
                break;
            }
            idx[d] = 0;
        }
    }
    let layout = common::Layout {
        cols: n,
        rows: 1,
        bands: 1,
        data_type: a.data_type,
        little_endian: true,
        interleave: common::Interleave::Bsq,
        offset: 0,
        bottom_up: false,
    };
    common::decode(&bytes, &layout)
}

/// The array's samples being read: little-endian, row-major.
struct Output<'a> {
    bytes: &'a mut [u8],
    shape: &'a [usize],
    size: usize,
}

/// Decodes a chunk of shape `cshape` at `origin` (in array elements) into the output.
fn decode_chunk(p: &Pipeline, raw: Vec<u8>, origin: &[usize], cshape: &[usize], out: &mut Output) -> Result<()> {
    let mut data = raw;
    for c in &p.codecs {
        data = decompress(c, data)?;
    }
    let nd = cshape.len();
    if let Some(shard) = &p.shard {
        let inner_grid: Vec<usize> = cshape.iter().zip(&shard.chunks).map(|(c, i)| c.div_ceil(*i)).collect();
        let count: usize = inner_grid.iter().product();
        let index_len = count * 16 + if shard.index_checksum { 4 } else { 0 };
        if data.len() < index_len {
            return Err(bad("shard smaller than its index"));
        }
        let index_start = if shard.index_at_start { 0 } else { data.len() - index_len };
        let u64_at = |i: usize| u64::from_le_bytes(data[i..i + 8].try_into().unwrap_or_default());
        let mut iidx = vec![0usize; nd];
        for j in 0..count {
            let (offset, length) = (u64_at(index_start + 16 * j), u64_at(index_start + 16 * j + 8));
            if offset != u64::MAX || length != u64::MAX {
                let (o, l) = (offset as usize, length as usize);
                let chunk = data.get(o..o.saturating_add(l)).ok_or_else(|| bad("shard index out of range"))?.to_vec();
                let inner_origin: Vec<usize> = (0..nd).map(|d| origin[d] + iidx[d] * shard.chunks[d]).collect();
                decode_chunk(&shard.inner, chunk, &inner_origin, &shard.chunks, out)?;
            }
            for d in (0..nd).rev() {
                iidx[d] += 1;
                if iidx[d] < inner_grid[d] {
                    break;
                }
                iidx[d] = 0;
            }
        }
        return Ok(());
    }
    let size = out.size;
    let chunk_len: usize = cshape.iter().product();
    if data.len() < chunk_len * size {
        return Err(bad(format!("holds {} bytes, expected {}", data.len(), chunk_len * size)));
    }
    // Elements are stored in C order over the stored (possibly transposed) dimensions.
    let order: Vec<usize> = p.order.clone().unwrap_or_else(|| (0..nd).collect());
    let sshape: Vec<usize> = order.iter().map(|&d| cshape[d]).collect();
    let mut sidx = vec![0usize; nd];
    let mut cidx = vec![0usize; nd];
    for e in 0..chunk_len {
        for (i, &d) in order.iter().enumerate() {
            cidx[d] = sidx[i];
        }
        let mut flat = 0usize;
        let mut inside = true;
        for d in 0..nd {
            let g = origin[d] + cidx[d];
            if g >= out.shape[d] {
                inside = false;
                break;
            }
            flat = flat * out.shape[d] + g;
        }
        if inside {
            let src = &data[e * size..(e + 1) * size];
            let dst = &mut out.bytes[flat * size..(flat + 1) * size];
            if p.little_endian {
                dst.copy_from_slice(src);
            } else {
                for k in 0..size {
                    dst[k] = src[size - 1 - k];
                }
            }
        }
        for d in (0..nd).rev() {
            sidx[d] += 1;
            if sidx[d] < sshape[d] {
                break;
            }
            sidx[d] = 0;
        }
    }
    Ok(())
}

/// The arrays of a group: `(name, metadata)`, sorted by name.
fn group_arrays(dir: &Path) -> Vec<(String, Array)> {
    let mut out: Vec<(String, Array)> = memfs::subdirs(dir).into_iter().filter_map(|(n, p)| Some((n, read_array(&p).ok()?))).collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

const X_NAMES: &[&str] = &["x", "lon", "longitude", "easting"];
const Y_NAMES: &[&str] = &["y", "lat", "latitude", "northing"];

const SPATIAL_UUID: &str = "689b58e2-cf7b-45e0-9fff-9cfc0883d6b4";
const PROJ_UUID: &str = "f17cb550-5864-4468-aeb7-f3180cfb622f";
const MULTISCALES_UUID: &str = "d35379db-88df-4056-af3a-620245f8e347";

/// A group's attributes (v2 `.zattrs`, v3 `zarr.json`).
fn group_attrs(dir: &Path) -> Value {
    if let Some(v) = read_json(&dir.join(".zattrs")) {
        return v;
    }
    read_json(&dir.join("zarr.json")).map(|m| m["attributes"].clone()).filter(Value::is_object).unwrap_or(json!({}))
}

fn is_group(dir: &Path) -> bool {
    memfs::is_file(&dir.join(".zgroup")) || read_json(&dir.join("zarr.json")).is_some_and(|m| m["node_type"] == "group")
}

/// A `multiscales` layout: the group holding it and its levels.
struct Multiscales {
    group: PathBuf,
    layout: Vec<Value>,
}

fn multiscales(dir: &Path) -> Option<Multiscales> {
    let attrs = group_attrs(dir);
    let layout = attrs["multiscales"]["layout"].as_array().filter(|l| !l.is_empty())?.clone();
    Some(Multiscales { group: dir.to_path_buf(), layout })
}

/// The array a layout level's `asset` names: an array, or the array named `name` (by default the
/// first of two or more dimensions) in a group.
fn layout_array(ms: &Multiscales, level: &Value, name: Option<&str>) -> Option<PathBuf> {
    let p = ms.group.join(level["asset"].as_str()?);
    if is_group(&p) {
        return match name {
            Some(n) => Some(p.join(n)).filter(|a| read_array(a).is_ok()),
            None => group_arrays(&p).into_iter().find(|(_, a)| a.shape.len() >= 2).map(|(n, _)| p.join(n)),
        };
    }
    read_array(&p).is_ok().then_some(p)
}

/// The raster array of a store or array path, its group, and the multiscales layout it is the
/// base of.
fn locate(path: &Path) -> Result<(Array, Option<PathBuf>, Option<Multiscales>)> {
    if let Ok(a) = read_array(path) {
        let group = path.parent().map(Path::to_path_buf);
        // The multiscales layout of the array's group, or of the group above (GDAL's lookup).
        let ms = group.as_deref().and_then(|g| multiscales(g).or_else(|| g.parent().and_then(multiscales)));
        return Ok((a, group, ms));
    }
    if let Some(ms) = multiscales(path)
        && let Some(base) = ms.layout.first().and_then(|l| layout_array(&ms, l, None))
        && let Ok(a) = read_array(&base)
    {
        return Ok((a, base.parent().map(Path::to_path_buf), Some(ms)));
    }
    let arrays = group_arrays(path);
    // The raster: the first array of two or more dimensions.
    let (_, a) = arrays
        .into_iter()
        .find(|(_, a)| a.shape.len() >= 2)
        .ok_or_else(|| bad(format!("{}: no array of two or more dimensions", path.display())))?;
    let ms = multiscales(path);
    Ok((a, Some(path.to_path_buf()), ms))
}

/// GeoZarr georeferencing: the spatial dimensions `(y, x)`, geotransform and CRS, from the first
/// attribute set (most specific first) that has each.
#[derive(Debug, Default, PartialEq)]
struct GeoZarr {
    dims: Option<(String, String)>,
    gt: Option<[f64; 6]>,
    crs: Option<CrsInfo>,
}

fn geozarr(attrs: &[&Value], shape: impl Fn(&str, &str) -> Option<(usize, usize)>) -> GeoZarr {
    let mut g = GeoZarr::default();
    let first = |key: &str| attrs.iter().map(|a| &a[key]).find(|v| !v.is_null());
    if let Some(d) = first("spatial:dimensions").and_then(Value::as_array).filter(|d| d.len() >= 2) {
        let n = d.len();
        if let (Some(y), Some(x)) = (d[n - 2].as_str(), d[n - 1].as_str()) {
            g.dims = Some((y.to_string(), x.to_string()));
        }
    }
    let node = first("spatial:registration").and_then(Value::as_str) == Some("node");
    let affine = first("spatial:transform_type").and_then(Value::as_str).is_none_or(|t| t == "affine");
    let t: Option<Vec<f64>> = first("spatial:transform").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_f64).collect());
    if let Some(t) = t.filter(|t| t.len() == 6 && affine && t[1] == 0.0 && t[3] == 0.0) {
        let mut gt = [t[2], t[0], 0.0, t[5], 0.0, t[4]];
        if node {
            // From pixel centres to GDAL's corner convention.
            gt[0] -= 0.5 * t[0];
            gt[3] -= 0.5 * t[4];
        }
        g.gt = Some(gt);
    } else if let Some(b) = first("spatial:bbox").and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_f64).collect::<Vec<_>>())
        && b.len() == 4
        && let Some((y, x)) = &g.dims
        && let Some((h, w)) = shape(y, x)
    {
        let (w, h) = if node { (w.saturating_sub(1).max(1), h.saturating_sub(1).max(1)) } else { (w, h) };
        let (rx, ry) = ((b[2] - b[0]) / w as f64, (b[3] - b[1]) / h as f64);
        let (x0, y0) = if node { (b[0] - rx / 2.0, b[3] + ry / 2.0) } else { (b[0], b[3]) };
        g.gt = Some([x0, rx, 0.0, y0, 0.0, -ry]);
    }
    g.crs = attrs.iter().find_map(|a| proj_crs(a));
    g
}

/// The CRS of `proj:` attributes (`proj:code`, `proj:wkt2`, `proj:projjson`, or the EOPF
/// samples' `proj:epsg`).
fn proj_crs(a: &Value) -> Option<CrsInfo> {
    use crate::crs::SpatialRef;
    let s = if let Some(code) = a["proj:code"].as_str() {
        SpatialRef::parse(code).ok()?
    } else if let Some(code) = a["proj:epsg"].as_u64() {
        SpatialRef::from_epsg(code as u32).ok()?
    } else if let Some(w) = a["proj:wkt2"].as_str() {
        SpatialRef::from_wkt(w)
    } else if a["proj:projjson"].is_object() {
        let v = &a["proj:projjson"];
        match crate::drivers::vector::projjson::epsg(v) {
            Some(code) => SpatialRef::from_epsg(code).ok()?,
            None => SpatialRef::from_wkt(&crate::drivers::vector::projjson::to_wkt1(v)?),
        }
    } else {
        return None;
    };
    Some(s.to_crs_info())
}

/// Reads a Zarr store or array as a raster.
pub(crate) fn read(path: &Path) -> Result<RasterDataset> {
    let path = path.to_path_buf();
    let (array, group, ms) = locate(&path)?;
    let nd = array.shape.len();
    if !(2..=3).contains(&nd) {
        return Err(Error::Unsupported(format!("Zarr arrays of {nd} dimensions are not supported as rasters")));
    }
    // GeoZarr attributes: the array's, its group's, its multiscales level's and the multiscales
    // group's.
    let group_attrs_v = group.as_deref().map(group_attrs).unwrap_or(json!({}));
    let level_item = ms.as_ref().and_then(|m| {
        m.layout.iter().find(|l| layout_array(m, l, array.dir.file_name().and_then(|n| n.to_str())).as_deref() == Some(array.dir.as_path()))
    });
    let ms_attrs = ms.as_ref().map(|m| group_attrs(&m.group)).unwrap_or(json!({}));
    let null = Value::Null;
    let attr_sets: Vec<&Value> = vec![&array.attrs, &group_attrs_v, level_item.unwrap_or(&null), &ms_attrs];
    let dim_size = |name: &str| array.dims.iter().position(|d| d == name).map(|i| array.shape[i]);
    let geo = geozarr(&attr_sets, |y, x| Some((dim_size(y)?, dim_size(x)?)));
    let lower: Vec<String> = array.dims.iter().map(|d| d.to_ascii_lowercase()).collect();
    let find = |names: &[&str], default: usize| lower.iter().position(|d| names.contains(&d.as_str())).unwrap_or(default);
    let by_name = |n: &str| array.dims.iter().position(|d| d == n);
    let (xd, yd) = match geo.dims.as_ref().and_then(|(y, x)| Some((by_name(x)?, by_name(y)?))) {
        Some(d) => d,
        None => (find(X_NAMES, nd - 1), find(Y_NAMES, nd - 2)),
    };
    let bd = (0..nd).find(|d| *d != xd && *d != yd);
    let (cols, rows) = (array.shape[xd], array.shape[yd]);
    let bands = bd.map(|d| array.shape[d]).unwrap_or(1);

    // Rearrange into band-major order.
    let values = read_values(&array)?;
    let strides: Vec<usize> = (0..nd).map(|d| array.shape[d + 1..].iter().product()).collect();
    let n = cols * rows * bands;
    macro_rules! reorder {
        ($v:expr, $variant:ident) => {{
            let mut out = Vec::with_capacity(n);
            for b in 0..bands {
                for r in 0..rows {
                    for c in 0..cols {
                        let i = c * strides[xd] + r * strides[yd] + bd.map(|d| b * strides[d]).unwrap_or(0);
                        out.push($v[i]);
                    }
                }
            }
            RasterData::$variant(out)
        }};
    }
    let data = match values {
        RasterData::U8(v) => reorder!(v, U8),
        RasterData::I8(v) => reorder!(v, I8),
        RasterData::U16(v) => reorder!(v, U16),
        RasterData::I16(v) => reorder!(v, I16),
        RasterData::U32(v) => reorder!(v, U32),
        RasterData::I32(v) => reorder!(v, I32),
        RasterData::U64(v) => reorder!(v, U64),
        RasterData::I64(v) => reorder!(v, I64),
        RasterData::F32(v) => reorder!(v, F32),
        RasterData::F64(v) => reorder!(v, F64),
    };

    // Georeferencing: a grid mapping variable (xarray), or X/Y coordinate arrays (GDAL).
    let mut gt = geo.gt;
    let mut crs = geo.crs;
    let sibling = |name: &str| group.as_ref().and_then(|g| read_array(&g.join(name)).ok());
    if let Some(gm) = array.attrs["grid_mapping"].as_str().and_then(sibling) {
        if crs.is_none() {
            crs = gm.attrs["crs_wkt"].as_str().or(gm.attrs["spatial_ref"].as_str()).and_then(common::crs_from_text);
        }
        if gt.is_none() {
            gt = gm.attrs["GeoTransform"].as_str().and_then(|s| {
                let v: Vec<f64> = s.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                <[f64; 6]>::try_from(v).ok()
            });
        }
    }
    if gt.is_none() {
        let coord = |d: usize| -> Option<Vec<f64>> {
            let a = sibling(array.dims.get(d)?)?;
            (a.shape == [array.shape[d]]).then(|| read_values(&a).ok().map(|v| v.to_f64_vec())).flatten()
        };
        if let (Some(x), Some(y)) = (coord(xd), coord(yd))
            && cols > 1
            && rows > 1
        {
            let dx = (x[cols - 1] - x[0]) / (cols - 1) as f64;
            let dy = (y[rows - 1] - y[0]) / (rows - 1) as f64;
            gt = Some([x[0] - dx / 2.0, dx, 0.0, y[0] - dy / 2.0, 0.0, dy]);
        }
    }
    if crs.is_none() {
        crs = crs_attribute(&array.attrs["_CRS"]);
    }
    if crs.is_none() {
        // GDAL's Zarr v3 writer keeps the CRS in a sidecar inside the store.
        crs = group.as_ref().and_then(|g| common::read_pam(&g.join("pam"))).and_then(|p| p.crs);
    }
    let south_up = gt.is_some_and(|g| g[5] > 0.0);
    let gt = gt.unwrap_or(common::NO_GEOTRANSFORM);
    let mut raster = common::make_raster(cols, rows, bands, data, gt, crs.unwrap_or_default(), array.fill)?;
    if south_up {
        flip_rows(&mut raster);
    }
    Ok(RasterDataset { raster, driver: RasterDriver::Zarr, has_nodata: array.fill.is_some(), affine: None })
}

/// The spatial axes `(x, y)` of an array: from `spatial:dimensions` in `attrs`, or from the
/// dimension names.
fn axes(a: &Array, attrs: &[&Value]) -> (usize, usize) {
    let nd = a.shape.len();
    let geo = geozarr(attrs, |_, _| None);
    let by_name = |n: &str| a.dims.iter().position(|d| d == n);
    if let Some(d) = geo.dims.as_ref().and_then(|(y, x)| Some((by_name(x)?, by_name(y)?))) {
        return d;
    }
    let lower: Vec<String> = a.dims.iter().map(|d| d.to_ascii_lowercase()).collect();
    let find = |names: &[&str], default: usize| lower.iter().position(|d| names.contains(&d.as_str())).unwrap_or(default);
    (find(X_NAMES, nd - 1), find(Y_NAMES, nd - 2))
}

/// The lower-resolution levels of a multiscales layout the raster at `path` is the base of:
/// `(array, width, height)`, largest first.
pub(crate) fn overview_levels(path: &Path) -> Vec<(PathBuf, usize, usize)> {
    let Ok((base, group, Some(ms))) = locate(path) else { return Vec::new() };
    let name = base.dir.file_name().and_then(|n| n.to_str()).map(str::to_string);
    let gattrs = group.as_deref().map(group_attrs).unwrap_or(json!({}));
    let mattrs = group_attrs(&ms.group);
    let (xd, yd) = axes(&base, &[&base.attrs, &gattrs, &mattrs]);
    let (bw, bh) = (base.shape[xd], base.shape[yd]);
    let mut out: Vec<(PathBuf, usize, usize)> = Vec::new();
    for level in &ms.layout {
        let Some(dir) = layout_array(&ms, level, name.as_deref()).or_else(|| layout_array(&ms, level, None)) else { continue };
        if dir == base.dir || out.iter().any(|(d, _, _)| *d == dir) {
            continue;
        }
        let Ok(a) = read_array(&dir) else { continue };
        if a.shape.len() != base.shape.len() || a.data_type != base.data_type {
            continue;
        }
        let (w, h) = (a.shape[xd], a.shape[yd]);
        if w <= bw && h <= bh && (w, h) != (bw, bh) && w > 0 && h > 0 {
            out.push((dir, w, h));
        }
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.1 * a.2));
    out
}

/// A multiscales pyramid read by window: the base raster and its lower-resolution levels, each
/// read when first needed.
struct Pyramid {
    base: RasterDataset,
    levels: Vec<(PathBuf, usize, usize, Option<RasterDataset>)>,
}

impl crate::cog::WindowSource for Pyramid {
    fn overviews(&self) -> Vec<(usize, usize)> {
        self.levels.iter().map(|l| (l.1, l.2)).collect()
    }

    fn read(&mut self, level: usize, window: crate::Window, bands: &[usize]) -> Result<RasterData> {
        let ds = if level == 0 {
            &self.base
        } else {
            let l = self.levels.get_mut(level - 1).ok_or_else(|| bad(format!("no overview {}", level - 1)))?;
            if l.3.is_none() {
                l.3 = Some(read(&l.0)?);
            }
            l.3.as_ref().ok_or_else(|| bad("unreachable"))?
        };
        if !window.fits(ds.raster.cols, ds.raster.rows) {
            return Err(bad(format!("{} is {}x{}, not as its multiscales layout says", ds.raster.cols, ds.raster.rows, level)));
        }
        Ok(crate::cog::cut(&ds.raster, window, bands))
    }
}

/// Opens a Zarr store with a multiscales layout lazily, its levels as overviews. `None` when the
/// store has no lower-resolution levels.
pub(crate) fn open_lazy(path: &Path) -> Option<Result<crate::LazyRaster>> {
    let levels = overview_levels(path);
    if levels.is_empty() {
        return None;
    }
    let base = match super::read(path.to_str()?)? {
        Ok(b) => b,
        Err(e) => return Some(Err(e)),
    };
    let mut header = base.clone();
    header.raster.data = RasterData::new_filled(base.raster.data_type, 0, 0.0);
    let levels = levels.into_iter().map(|(d, w, h)| (d, w, h, None)).collect();
    Some(Ok(crate::LazyRaster::from_source(header, Box::new(Pyramid { base, levels }))))
}

/// Turns a south-up raster (latitudes increasing with the row) north-up.
fn flip_rows(r: &mut Raster) {
    let (cols, rows) = (r.cols, r.rows);
    macro_rules! flip {
        ($v:expr) => {
            for band in $v.chunks_mut(cols * rows) {
                for row in 0..rows / 2 {
                    let (a, b) = band.split_at_mut((rows - 1 - row) * cols);
                    a[row * cols..(row + 1) * cols].swap_with_slice(&mut b[..cols]);
                }
            }
        };
    }
    match &mut r.data {
        RasterData::U8(v) => flip!(v),
        RasterData::I8(v) => flip!(v),
        RasterData::U16(v) => flip!(v),
        RasterData::I16(v) => flip!(v),
        RasterData::U32(v) => flip!(v),
        RasterData::I32(v) => flip!(v),
        RasterData::U64(v) => flip!(v),
        RasterData::I64(v) => flip!(v),
        RasterData::F32(v) => flip!(v),
        RasterData::F64(v) => flip!(v),
    }
}

/// The CRS of a GDAL `_CRS` attribute: its URL, PROJJSON identifier or WKT.
fn crs_attribute(v: &Value) -> Option<CrsInfo> {
    if let Some(code) = v["url"].as_str().and_then(|u| u.rsplit('/').next()).and_then(|c| c.parse::<u32>().ok()) {
        return Some(CrsInfo::from_epsg(code));
    }
    if v["projjson"]["id"]["authority"].as_str() == Some("EPSG")
        && let Some(code) = v["projjson"]["id"]["code"].as_u64()
    {
        return Some(CrsInfo::from_epsg(code as u32));
    }
    v["wkt"].as_str().and_then(common::crs_from_text)
}

/// The Zarr versions terrio writes.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Version {
    V2,
    V3,
}

/// Compressors terrio writes: the codec and its metadata.
fn compressor(name: Option<&str>, version: Version) -> Result<(Option<Codec>, Value)> {
    let v3 = version == Version::V3;
    Ok(match name.map(str::to_ascii_uppercase).as_deref() {
        None | Some("NONE") => (None, Value::Null),
        Some("ZLIB") | Some("DEFLATE") if !v3 => (Some(Codec::Zlib), json!({"id": "zlib", "level": 6})),
        Some("GZIP") if v3 => (Some(Codec::Gzip), json!({"name": "gzip", "configuration": {"level": 6}})),
        Some("GZIP") => (Some(Codec::Gzip), json!({"id": "gzip", "level": 6})),
        Some("ZSTD") if v3 => (Some(Codec::Zstd), json!({"name": "zstd", "configuration": {"level": 1, "checksum": false}})),
        Some("ZSTD") => (Some(Codec::Zstd), json!({"id": "zstd", "level": 1})),
        Some(other) => {
            let names = if v3 { "NONE, GZIP, ZSTD" } else { "NONE, ZLIB, GZIP, ZSTD" };
            return Err(Error::InvalidArgument(format!("Zarr compression '{other}' is not supported ({names})")));
        }
    })
}

fn compress(codec: &Option<Codec>, data: Vec<u8>) -> Result<Vec<u8>> {
    use std::io::Write;
    Ok(match codec {
        Some(Codec::Zlib) => {
            let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
            e.write_all(&data)?;
            e.finish()?
        }
        Some(Codec::Gzip) => {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::new(6));
            e.write_all(&data)?;
            e.finish()?
        }
        Some(Codec::Zstd) => ruzstd::encoding::compress_to_vec(&data[..], ruzstd::encoding::CompressionLevel::Fastest),
        _ => data,
    })
}

fn dtype_v2(t: DataType) -> &'static str {
    match t {
        DataType::U8 => "|u1",
        DataType::I8 => "|i1",
        DataType::U16 => "<u2",
        DataType::I16 => "<i2",
        DataType::U32 => "<u4",
        DataType::I32 => "<i4",
        DataType::U64 => "<u8",
        DataType::I64 => "<i8",
        DataType::F32 => "<f4",
        DataType::F64 => "<f8",
    }
}

fn dtype_v3(t: DataType) -> &'static str {
    match t {
        DataType::U8 => "uint8",
        DataType::I8 => "int8",
        DataType::U16 => "uint16",
        DataType::I16 => "int16",
        DataType::U32 => "uint32",
        DataType::I32 => "int32",
        DataType::U64 => "uint64",
        DataType::I64 => "int64",
        DataType::F32 => "float32",
        DataType::F64 => "float64",
    }
}

fn fill_json(v: Option<f64>) -> Value {
    match v {
        None => Value::Null,
        Some(x) if x.is_nan() => json!("NaN"),
        Some(x) if x == f64::INFINITY => json!("Infinity"),
        Some(x) if x == f64::NEG_INFINITY => json!("-Infinity"),
        Some(x) if x.fract() == 0.0 && x.abs() < 9.0e15 => json!(x as i64),
        Some(x) => json!(x),
    }
}

/// An array to write.
struct NewArray<'a> {
    shape: &'a [usize],
    chunks: &'a [usize],
    data_type: DataType,
    /// The samples, little-endian, row-major.
    bytes: &'a [u8],
    fill: Option<f64>,
    dims: Value,
    attrs: Value,
}

/// Copies into a chunk (of the chunk grid index `idx`) the rows, runs along the last dimension,
/// of an array of at least one dimension that lie inside the array.
fn copy_rows(a: &NewArray, idx: &[usize], chunk: &mut [u8]) {
    let (size, nd) = (a.data_type.size_bytes(), a.shape.len());
    let chunk_len: usize = a.chunks.iter().product();
    let last = nd - 1;
    let row_len = a.chunks[last];
    let x0 = idx[last] * a.chunks[last];
    let n = row_len.min(a.shape[last].saturating_sub(x0));
    if n == 0 {
        return;
    }
    let mut cidx = vec![0usize; nd];
    for row in 0..chunk_len / row_len.max(1) {
        let mut flat = 0usize;
        let mut inside = true;
        for d in 0..last {
            let g = idx[d] * a.chunks[d] + cidx[d];
            if g >= a.shape[d] {
                inside = false;
                break;
            }
            flat = flat * a.shape[d] + g;
        }
        if inside {
            let src = (flat * a.shape[last] + x0) * size;
            chunk[row * row_len * size..][..n * size].copy_from_slice(&a.bytes[src..src + n * size]);
        }
        for d in (0..last).rev() {
            cidx[d] += 1;
            if cidx[d] < a.chunks[d] {
                break;
            }
            cidx[d] = 0;
        }
    }
}

/// Writes one array (metadata and chunks), returning its metadata documents by file name.
fn write_array(dir: &Path, a: &NewArray, compression: Option<&str>, version: Version) -> Result<Vec<(String, Value)>> {
    memfs::create_dir_all(dir)?;
    let (codec, compressor_meta) = compressor(compression, version)?;
    let (size, nd) = (a.data_type.size_bytes(), a.shape.len());
    let grid: Vec<usize> = a.shape.iter().zip(a.chunks).map(|(s, c)| s.div_ceil(*c)).collect();
    let chunk_len: usize = a.chunks.iter().product();
    let mut fill = vec![0u8; size];
    if let Some(f) = a.fill {
        let mut one = Vec::new();
        super::gtiff_write::encode_sample_le(a.data_type, f, &mut one);
        fill = one;
    }
    // Chunks are built, compressed and written in parallel.
    let results = crate::par::map_range(grid.iter().product::<usize>(), |k| -> Result<()> {
        // The chunk's index along each dimension (C order).
        let mut idx = vec![0usize; nd];
        let mut rest = k;
        for d in (0..nd).rev() {
            idx[d] = rest % grid[d];
            rest /= grid[d];
        }
        let mut chunk = vec![0u8; chunk_len * size];
        if a.fill.is_some() {
            for c in chunk.chunks_mut(size) {
                c.copy_from_slice(&fill);
            }
        }
        if nd == 0 {
            // A scalar (0-dimensional) array: its one value.
            chunk.copy_from_slice(&a.bytes[..size]);
        } else {
            copy_rows(a, &idx, &mut chunk);
        }
        let names: Vec<String> = idx.iter().map(|i| i.to_string()).collect();
        let file = match version {
            // A scalar's one chunk is `0` (v2) or `c` (v3).
            Version::V2 if nd == 0 => dir.join("0"),
            Version::V2 => dir.join(names.join(".")),
            Version::V3 => {
                let mut p = dir.join("c");
                for n in &names {
                    p.push(n);
                }
                if let Some(parent) = p.parent() {
                    memfs::create_dir_all(parent)?;
                }
                p
            }
        };
        memfs::write(&file, compress(&codec, chunk)?)?;
        Ok(())
    });
    results.into_iter().collect::<Result<Vec<()>>>()?;
    let docs = match version {
        Version::V2 => {
            let zarray = json!({
                "chunks": a.chunks,
                "compressor": compressor_meta,
                "dtype": dtype_v2(a.data_type),
                "fill_value": fill_json(a.fill),
                "filters": null,
                "order": "C",
                "shape": a.shape,
                "zarr_format": 2,
            });
            let mut attrs = a.attrs.clone();
            attrs["_ARRAY_DIMENSIONS"] = a.dims.clone();
            vec![(".zarray".to_string(), zarray), (".zattrs".to_string(), attrs)]
        }
        Version::V3 => {
            let mut codecs = vec![json!({"name": "bytes", "configuration": {"endian": "little"}})];
            if !compressor_meta.is_null() {
                codecs.push(compressor_meta);
            }
            // Zarr v3 needs a fill value: GDAL uses NaN for floats and 0 for integers.
            let fill = match a.fill {
                Some(f) => fill_json(Some(f)),
                None if matches!(a.data_type, DataType::F32 | DataType::F64) => json!("NaN"),
                None => json!(0),
            };
            let meta = json!({
                "zarr_format": 3,
                "node_type": "array",
                "shape": a.shape,
                "data_type": dtype_v3(a.data_type),
                "chunk_grid": {"name": "regular", "configuration": {"chunk_shape": a.chunks}},
                "chunk_key_encoding": {"name": "default", "configuration": {"separator": "/"}},
                "fill_value": fill,
                "codecs": codecs,
                "attributes": a.attrs,
                "dimension_names": a.dims,
            });
            vec![("zarr.json".to_string(), meta)]
        }
    };
    for (name, doc) in &docs {
        memfs::write(&dir.join(name), serde_json::to_string_pretty(doc).unwrap_or_default())?;
    }
    Ok(docs)
}

/// How a written store records its georeferencing (GDAL's `GEOREFERENCING_CONVENTION`).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Convention {
    /// GDAL's: `_CRS`, X and Y coordinate arrays.
    Gdal,
    /// GeoZarr's `spatial:` and `proj:` conventions, besides the coordinate arrays.
    SpatialProj,
    /// CF: a `spatial_ref` grid mapping variable with `crs_wkt` and `GeoTransform` (rioxarray's).
    Cf,
}

fn convention(name: &str, uuid: &str, what: &str) -> Value {
    json!({
        "schema_url": format!("https://raw.githubusercontent.com/zarr-conventions/{what}/refs/tags/v0.1/schema.json"),
        "spec_url": format!("https://github.com/zarr-conventions/{what}/blob/v0.1/README.md"),
        "uuid": uuid,
        "name": name,
        "description": match name {
            "proj" => "Coordinate reference system information for geospatial data",
            "spatial" => "Spatial coordinate information",
            _ => "Multiscale layout of zarr datasets",
        },
    })
}

/// The `proj:` attributes of a CRS: its code, or its PROJJSON.
fn proj_attrs(s: &crate::crs::SpatialRef, attrs: &mut Value) {
    if let Some(code) = s.epsg {
        attrs["proj:code"] = json!(format!("EPSG:{code}"));
    } else if let Some(pj) = super::wkt::gdal_wkt(s).and_then(|w| crate::drivers::vector::projjson::from_wkt1(&w, None)) {
        attrs["proj:projjson"] = pj;
    }
}

/// GeoZarr `spatial:` attributes of a raster whose spatial dimensions are `dims` (`[y, x]`).
fn spatial_attrs(r: &Raster, dims: [&str; 2], attrs: &mut Value) {
    let (x0, y1) = (r.x_min, r.y_max());
    attrs["spatial:dimensions"] = json!(dims);
    attrs["spatial:registration"] = json!("pixel");
    attrs["spatial:transform_type"] = json!("affine");
    attrs["spatial:transform"] = json!([r.cell_size_x, 0.0, x0, 0.0, -r.cell_size_y, y1]);
    attrs["spatial:shape"] = json!([r.rows, r.cols]);
    attrs["spatial:bbox"] = json!([x0, y1 - r.rows as f64 * r.cell_size_y, x0 + r.cols as f64 * r.cell_size_x, y1]);
}

/// Writes a group's metadata documents.
fn write_group(dir: &Path, attrs: &Value, version: Version, meta: &mut serde_json::Map<String, Value>, prefix: &str) -> Result<()> {
    memfs::create_dir_all(dir)?;
    match version {
        Version::V2 => {
            let zgroup = json!({"zarr_format": 2});
            memfs::write(&dir.join(".zgroup"), serde_json::to_string_pretty(&zgroup).unwrap_or_default())?;
            memfs::write(&dir.join(".zattrs"), serde_json::to_string_pretty(attrs).unwrap_or_default())?;
            meta.insert(format!("{prefix}.zgroup"), zgroup);
            meta.insert(format!("{prefix}.zattrs"), attrs.clone());
        }
        Version::V3 => {
            let group = json!({"zarr_format": 3, "node_type": "group", "attributes": attrs});
            memfs::write(&dir.join("zarr.json"), serde_json::to_string_pretty(&group).unwrap_or_default())?;
        }
    }
    Ok(())
}

/// Writes a raster as the array `name` of the group `dir`, with its coordinate arrays (and grid
/// mapping variable), adding their metadata to `meta` under `prefix`.
#[allow(clippy::too_many_arguments)]
fn write_level(
    dir: &Path,
    name: &str,
    r: &Raster,
    nodata: Option<f64>,
    compression: Option<&str>,
    version: Version,
    conv: Convention,
    meta: &mut serde_json::Map<String, Value>,
    prefix: &str,
) -> Result<()> {
    let (xname, yname) = if conv == Convention::Cf { ("x", "y") } else { ("X", "Y") };
    let (shape, chunks, dims) = if r.bands == 1 {
        (vec![r.rows, r.cols], vec![r.rows.min(256), r.cols.min(256)], json!([yname, xname]))
    } else {
        (vec![r.bands, r.rows, r.cols], vec![1, r.rows.min(256), r.cols.min(256)], json!(["Band", yname, xname]))
    };
    let mut attrs = json!({});
    let s = super::common::spatial_ref(&r.crs);
    match conv {
        // GDAL's `_CRS` with GeoZarr too: GDAL 3.12 and earlier do not read `proj:`.
        Convention::Gdal | Convention::SpatialProj => {
            if !s.is_unknown() {
                let mut c = serde_json::Map::new();
                if let Some(w) = super::wkt::gdal_wkt(&s) {
                    c.insert("wkt".into(), json!(w));
                }
                if let Some(code) = s.epsg {
                    c.insert("url".into(), json!(format!("http://www.opengis.net/def/crs/EPSG/0/{code}")));
                }
                attrs["_CRS"] = Value::Object(c);
            }
        }
        Convention::Cf => {
            attrs["grid_mapping"] = json!("spatial_ref");
            attrs["coordinates"] = json!("spatial_ref");
        }
    }
    if conv == Convention::SpatialProj {
        let mut conventions = Vec::new();
        if !s.is_unknown() {
            conventions.push(convention("proj", PROJ_UUID, "proj"));
            proj_attrs(&s, &mut attrs);
        }
        conventions.push(convention("spatial", SPATIAL_UUID, "spatial"));
        spatial_attrs(r, [yname, xname], &mut attrs);
        attrs["zarr_conventions"] = json!(conventions);
    }
    let layout = common::Layout {
        cols: r.cols,
        rows: r.rows,
        bands: r.bands,
        data_type: r.data_type,
        little_endian: true,
        interleave: common::Interleave::Bsq,
        offset: 0,
        bottom_up: false,
    };
    let bytes = common::encode(r, &layout);
    let array = NewArray { shape: &shape, chunks: &chunks, data_type: r.data_type, bytes: &bytes, fill: nodata, dims, attrs };
    for (file, doc) in write_array(&dir.join(name), &array, compression, version)? {
        meta.insert(format!("{prefix}{name}/{file}"), doc);
    }

    // Coordinates of the pixel centres.
    let geographic =
        s.epsg.and_then(|c| crate::crs::SpatialRef::from_epsg(c).ok()).and_then(|c| c.to_wkt()).is_some_and(|w| w.starts_with("GEOGCS"))
            || super::wkt::gdal_wkt(&s).is_some_and(|w| w.starts_with("GEOGCS"));
    let xs: Vec<u8> = (0..r.cols).flat_map(|c| (r.x_min + (c as f64 + 0.5) * r.cell_size_x).to_le_bytes()).collect();
    let ys: Vec<u8> = (0..r.rows).flat_map(|row| (r.y_max() - (row as f64 + 0.5) * r.cell_size_y).to_le_bytes()).collect();
    for (dim, n, data, axis) in [(xname, r.cols, xs, "X"), (yname, r.rows, ys, "Y")] {
        let fill = (version == Version::V3).then_some(f64::NAN);
        let attrs = if conv == Convention::Cf {
            match (geographic, axis) {
                (true, "X") => json!({"standard_name": "longitude", "long_name": "longitude", "units": "degrees_east", "axis": "X"}),
                (true, _) => json!({"standard_name": "latitude", "long_name": "latitude", "units": "degrees_north", "axis": "Y"}),
                (false, "X") => {
                    json!({"standard_name": "projection_x_coordinate", "long_name": "x coordinate of projection", "units": "metre", "axis": "X"})
                }
                (false, _) => {
                    json!({"standard_name": "projection_y_coordinate", "long_name": "y coordinate of projection", "units": "metre", "axis": "Y"})
                }
            }
        } else {
            json!({})
        };
        let coord = NewArray { shape: &[n], chunks: &[n], data_type: DataType::F64, bytes: &data, fill, dims: json!([dim]), attrs };
        for (file, doc) in write_array(&dir.join(dim), &coord, None, version)? {
            meta.insert(format!("{prefix}{dim}/{file}"), doc);
        }
    }
    if conv == Convention::Cf {
        // rioxarray's grid mapping variable: a scalar with the CRS and geotransform.
        let mut gm = json!({});
        if let Some(w) = super::wkt::gdal_wkt(&s) {
            gm["crs_wkt"] = json!(w);
            gm["spatial_ref"] = json!(w);
        }
        let gt = crate::geo_transform(r);
        gm["GeoTransform"] = json!(gt.iter().map(|v| super::super::ops::vrt::fmt_number(*v)).collect::<Vec<_>>().join(" "));
        let zero = 0i64.to_le_bytes();
        let var = NewArray { shape: &[], chunks: &[], data_type: DataType::I64, bytes: &zero, fill: Some(0.0), dims: json!([]), attrs: gm };
        for (file, doc) in write_array(&dir.join("spatial_ref"), &var, None, version)? {
            meta.insert(format!("{prefix}spatial_ref/{file}"), doc);
        }
    }
    Ok(())
}

/// A nearest-neighbour overview of `w` x `h` pixels, sampled as GDAL's overview builder samples
/// them (`GDALRegenerateOverviews()`: source pixel `floor(0.5 + i * ratio)`).
fn nearest_overview(r: &Raster, w: usize, h: usize) -> Raster {
    let (rx, ry) = (r.cols as f64 / w as f64, r.rows as f64 / h as f64);
    let xs: Vec<usize> = (0..w).map(|i| ((0.5 + i as f64 * rx) as usize).min(r.cols - 1)).collect();
    let ys: Vec<usize> = (0..h).map(|i| ((0.5 + i as f64 * ry) as usize).min(r.rows - 1)).collect();
    macro_rules! pick {
        ($($v:ident),*) => {
            match &r.data {
                $(RasterData::$v(src) => {
                    let mut out = Vec::with_capacity(w * h * r.bands);
                    for b in 0..r.bands {
                        for &y in &ys {
                            let row = b * r.cols * r.rows + y * r.cols;
                            out.extend(xs.iter().map(|&x| src[row + x]));
                        }
                    }
                    RasterData::$v(out)
                })*
            }
        };
    }
    let data = pick!(U8, I8, U16, I16, U32, I32, U64, I64, F32, F64);
    let mut out = r.clone();
    out.data = data;
    out.cols = w;
    out.rows = h;
    out.cell_size_x = r.cell_size_x * rx;
    out.cell_size_y = r.cell_size_y * ry;
    out.y_min = r.y_max() - h as f64 * out.cell_size_y;
    out
}

/// The overview factors of an `OVERVIEW_LEVELS` creation option: `2,4,8`, or `AUTO` (halving
/// until the raster fits in 256 pixels, as GDAL's default levels).
fn overview_factors(v: &str, cols: usize, rows: usize) -> Result<Vec<usize>> {
    if v.eq_ignore_ascii_case("AUTO") {
        let mut out = Vec::new();
        let mut f = 2;
        while cols.max(rows).div_ceil(f / 2) > 256 {
            out.push(f);
            f *= 2;
        }
        return Ok(out);
    }
    if v.eq_ignore_ascii_case("NONE") || v.is_empty() {
        return Ok(Vec::new());
    }
    let mut out: Vec<usize> = v
        .split([',', ' '])
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.trim()
                .parse::<usize>()
                .ok()
                .filter(|f| *f >= 2)
                .ok_or_else(|| Error::InvalidArgument(format!("invalid overview level '{s}'")))
        })
        .collect::<Result<_>>()?;
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// Writes a raster as a Zarr group, as GDAL does: the array (named after the store, with
/// dimensions `[Band,] Y, X`), X and Y coordinate arrays, the CRS in `_CRS`, and (v2) consolidated
/// metadata. `format` is GDAL's `FORMAT` creation option: `ZARR_V2` (the default) or `ZARR_V3`.
#[cfg(test)]
pub(crate) fn write(r: &Raster, nodata: Option<f64>, path: &Path, compression: Option<&str>, format: Option<&str>) -> Result<()> {
    let co: Vec<(String, String)> = format.map(|f| ("FORMAT".to_string(), f.to_string())).into_iter().collect();
    write_with(r, nodata, path, compression, &co)
}

/// Writes a raster as a Zarr group (see the module's documentation), with GDAL's creation options
/// `FORMAT` (`ZARR_V2`, the default, or `ZARR_V3`) and `GEOREFERENCING_CONVENTION` (`GDAL`,
/// `SPATIAL_PROJ` or `CF`), and terrio's `OVERVIEW_LEVELS` and `OVERVIEW_RESAMPLING`.
pub(crate) fn write_with(r: &Raster, nodata: Option<f64>, path: &Path, compression: Option<&str>, co: &[(String, String)]) -> Result<()> {
    let opt = |key: &str| co.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());
    let version = match opt("FORMAT").map(str::to_ascii_uppercase).as_deref() {
        None | Some("ZARR_V2") | Some("V2") | Some("2") => Version::V2,
        Some("ZARR_V3") | Some("V3") | Some("3") => Version::V3,
        Some(other) => return Err(Error::InvalidArgument(format!("Zarr format '{other}' is not supported (ZARR_V2, ZARR_V3)"))),
    };
    let conv = match opt("GEOREFERENCING_CONVENTION").map(str::to_ascii_uppercase).as_deref() {
        None | Some("GDAL") => Convention::Gdal,
        Some("SPATIAL_PROJ") | Some("GEOZARR") => Convention::SpatialProj,
        Some("CF") => Convention::Cf,
        Some(other) => {
            return Err(Error::InvalidArgument(format!(
                "Zarr georeferencing convention '{other}' is not supported (GDAL, SPATIAL_PROJ, CF)"
            )));
        }
    };
    let factors = overview_factors(opt("OVERVIEW_LEVELS").unwrap_or("NONE"), r.cols, r.rows)?;
    let resampling = match opt("OVERVIEW_RESAMPLING") {
        Some(m) => crate::ops::Resampling::parse(m)?,
        None => crate::ops::Resampling::Nearest,
    };
    // Fail on an unknown compression before creating anything.
    compressor(compression, version)?;
    if memfs::exists(path) {
        return Err(Error::InvalidArgument(format!("{} already exists", path.display())));
    }
    let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("data").to_string();
    let mut meta = serde_json::Map::new();

    // The overviews, resampled from the full resolution.
    let mut levels: Vec<(usize, RasterDataset)> = Vec::new();
    if !factors.is_empty() {
        let full = RasterDataset { raster: r.clone(), driver: RasterDriver::Zarr, has_nodata: nodata.is_some(), affine: None };
        for f in &factors {
            let (w, h) = (r.cols.div_ceil(*f), r.rows.div_ceil(*f));
            if resampling == crate::ops::Resampling::Nearest {
                levels.push((*f, RasterDataset { raster: nearest_overview(r, w, h), ..full.clone() }));
                continue;
            }
            let o = crate::ops::translate::TranslateOptions {
                out_size: Some((crate::ops::translate::Size::Pixels(w), crate::ops::translate::Size::Pixels(h))),
                resampling: Some(resampling),
                ..Default::default()
            };
            levels.push((*f, crate::ops::translate::translate(&full, &o)?));
        }
    }

    // The root group: a multiscales layout when there are overviews.
    let mut root = json!({});
    if !levels.is_empty() {
        let nd = if r.bands == 1 { 2 } else { 3 };
        let ones: Vec<f64> = vec![1.0; nd];
        let mut layout = vec![json!({"asset": name, "transform": {"scale": ones}})];
        let mut prev = (name.clone(), r.cols, r.rows);
        for (f, l) in &levels {
            let asset = format!("ovr_{f}x");
            let (w, h) = (l.raster.cols, l.raster.rows);
            let mut scale = vec![prev.2 as f64 / h as f64, prev.1 as f64 / w as f64];
            if nd == 3 {
                scale.insert(0, 1.0);
            }
            let mut item = json!({
                "asset": asset,
                "derived_from": prev.0,
                "transform": {"scale": scale, "translation": vec![0.0; nd]},
                "resampling_method": resampling.name(),
            });
            if conv == Convention::SpatialProj {
                let mut sa = json!({});
                spatial_attrs(&l.raster, ["Y", "X"], &mut sa);
                item["spatial:shape"] = sa["spatial:shape"].clone();
                item["spatial:transform"] = sa["spatial:transform"].clone();
            }
            layout.push(item);
            prev = (asset, w, h);
        }
        let mut conventions = vec![convention("multiscales", MULTISCALES_UUID, "multiscales")];
        let s = super::common::spatial_ref(&r.crs);
        if conv == Convention::SpatialProj {
            if !s.is_unknown() {
                conventions.push(convention("proj", PROJ_UUID, "proj"));
                proj_attrs(&s, &mut root);
            }
            conventions.push(convention("spatial", SPATIAL_UUID, "spatial"));
            root["spatial:dimensions"] = json!(["Y", "X"]);
        }
        root["zarr_conventions"] = json!(conventions);
        root["multiscales"] = json!({"layout": layout, "resampling_method": resampling.name()});
    }
    write_group(path, &root, version, &mut meta, "")?;
    write_level(path, &name, r, nodata, compression, version, conv, &mut meta, "")?;
    for (f, l) in &levels {
        let group = format!("ovr_{f}x");
        let dir = path.join(&group);
        write_group(&dir, &json!({}), version, &mut meta, &format!("{group}/"))?;
        write_level(&dir, &name, &l.raster, nodata, compression, version, conv, &mut meta, &format!("{group}/"))?;
    }
    if version == Version::V2 {
        let consolidated = json!({"metadata": meta, "zarr_consolidated_format": 1});
        memfs::write(&path.join(".zmetadata"), serde_json::to_string_pretty(&consolidated).unwrap_or_default())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blosclz_literals_and_matches() {
        // A literal run of 3 bytes, then a match of 4 bytes at distance 3.
        let stream = [2u8, b'a', b'b', b'c', (2 << 5), 2];
        assert_eq!(blosclz_decompress(&stream, 7).unwrap(), b"abcabca");
    }

    #[test]
    fn bit_unshuffles() {
        // Eight 16-bit elements 0..8 bit-shuffled: one row of 8 bits per bit of each byte.
        let elems: Vec<u16> = (0..8).map(|i| i * 3 + 1).collect();
        let mut shuffled = [0u8; 16];
        for (i, e) in elems.iter().enumerate() {
            for bit in 0..16 {
                if e >> bit & 1 == 1 {
                    let p = bit * 8 + i;
                    shuffled[p / 8] |= 1 << (p % 8);
                }
            }
        }
        let out = bit_unshuffle(&shuffled, 16, 2, 2);
        let back: Vec<u16> = out.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(back, elems);
    }

    #[test]
    fn round_trips_a_store() {
        let data: Vec<f32> = (0..300 * 270).map(|i| i as f32 * 0.5).collect();
        let r = common::make_raster(
            300,
            270,
            1,
            RasterData::F32(data.clone()),
            [-110.0, 0.01, 0.0, 40.0, 0.0, -0.01],
            CrsInfo::from_epsg(4326),
            None,
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        for (i, (compression, format)) in [
            (Some("ZLIB"), None),
            (Some("ZSTD"), None),
            (None, Some("ZARR_V3")),
            (Some("ZSTD"), Some("ZARR_V3")),
            (Some("GZIP"), Some("ZARR_V3")),
        ]
        .into_iter()
        .enumerate()
        {
            let p = dir.path().join(format!("t{i}.zarr"));
            write(&r, Some(-1.0), &p, compression, format).unwrap();
            let ds = read(&p).unwrap();
            assert_eq!(ds.raster.data_f32().unwrap(), &data[..]);
            assert_eq!(ds.nodata(), Some(-1.0));
            assert_eq!(ds.crs().epsg, Some(4326));
            let gt = ds.geo_transform();
            assert!((gt[0] + 110.0).abs() < 1e-9 && (gt[3] - 40.0).abs() < 1e-9, "{gt:?}");
        }
    }
}
