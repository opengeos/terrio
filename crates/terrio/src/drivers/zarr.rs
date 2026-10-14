//! Zarr v2 and v3 stores, as GDAL writes them (an array in a group, `_ARRAY_DIMENSIONS`, X/Y
//! coordinate arrays, the CRS in a `_CRS` attribute) and as xarray writes them (a `grid_mapping`
//! variable with `crs_wkt` and `GeoTransform`).
//!
//! Codecs, all in pure Rust: none, zlib, gzip, zstd, lz4 and Blosc (with the blosclz, lz4, zlib
//! and zstd compressors, and byte shuffling). Not supported: Blosc's bit shuffling, Fortran
//! order, and Zarr v3 sharding and transposition.

use super::common;
use crate::formats::RasterDriver;
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
    serde_json::from_slice(&std::fs::read(p).ok()?).ok()
}

/// Whether a directory is a Zarr store (or array).
pub(crate) fn is_zarr(path: &Path) -> bool {
    path.is_dir() && [".zgroup", ".zarray", "zarr.json"].iter().any(|f| path.join(f).is_file())
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

/// An array's metadata.
#[derive(Debug)]
struct Array {
    dir: PathBuf,
    shape: Vec<usize>,
    chunks: Vec<usize>,
    data_type: DataType,
    little_endian: bool,
    fill: Option<f64>,
    codecs: Vec<Codec>,
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
        if m["order"].as_str() == Some("F") {
            return Err(Error::Unsupported("Zarr arrays in Fortran order are not supported".into()));
        }
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
        return Ok(Array {
            dir: dir.to_path_buf(),
            shape: usizes(&m["shape"]),
            chunks: usizes(&m["chunks"]),
            data_type,
            little_endian,
            fill: parse_fill(&m["fill_value"]),
            codecs,
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
    let mut little_endian = true;
    let mut codecs = Vec::new();
    for c in m["codecs"].as_array().into_iter().flatten() {
        match c["name"].as_str().unwrap_or_default() {
            "bytes" => little_endian = c["configuration"]["endian"].as_str() != Some("big"),
            "gzip" => codecs.push(Codec::Gzip),
            "zstd" => codecs.push(Codec::Zstd),
            "blosc" => codecs.push(Codec::Blosc),
            "crc32c" => codecs.push(Codec::Crc32c),
            other => return Err(Error::Unsupported(format!("Zarr codec '{other}' is not supported"))),
        }
    }
    // Decoding applies the bytes-to-bytes codecs in reverse.
    codecs.reverse();
    let usizes =
        |v: &Value| v.as_array().map(|a| a.iter().filter_map(|x| x.as_u64().map(|x| x as usize)).collect::<Vec<_>>()).unwrap_or_default();
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
        shape: usizes(&m["shape"]),
        chunks: usizes(&m["chunk_grid"]["configuration"]["chunk_shape"]),
        data_type,
        little_endian,
        fill: parse_fill(&m["fill_value"]),
        codecs,
        key_prefix,
        separator,
        dims,
        attrs: m["attributes"].clone(),
    })
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
    if flags & 0x04 != 0 {
        return Err(Error::Unsupported("Blosc bit shuffling is not supported".into()));
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
        if flags & 0x01 != 0 && typesize > 1 {
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

/// Reads an array's values (row-major over its shape) as `f64`s converted to its type later.
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
    let chunk_len: usize = a.chunks.iter().product();
    let mut idx = vec![0usize; nd];
    for _ in 0..total {
        let key = format!("{}{}", a.key_prefix, idx.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(&a.separator));
        let key = if nd == 0 { format!("{}0", a.key_prefix) } else { key };
        if let Ok(raw) = std::fs::read(a.dir.join(&key)) {
            let mut data = raw;
            for c in &a.codecs {
                data = decompress(c, data)?;
            }
            if data.len() < chunk_len * size {
                return Err(bad(format!("chunk {key} holds {} bytes, expected {}", data.len(), chunk_len * size)));
            }
            // Copy the chunk's elements that fall inside the array.
            let mut cidx = vec![0usize; nd];
            for e in 0..chunk_len {
                let mut flat = 0usize;
                let mut inside = true;
                for d in 0..nd {
                    let g = idx[d] * a.chunks[d] + cidx[d];
                    if g >= a.shape[d] {
                        inside = false;
                        break;
                    }
                    flat = flat * a.shape[d] + g;
                }
                if inside {
                    let src = &data[e * size..(e + 1) * size];
                    let dst = &mut bytes[flat * size..(flat + 1) * size];
                    if a.little_endian {
                        dst.copy_from_slice(src);
                    } else {
                        for k in 0..size {
                            dst[k] = src[size - 1 - k];
                        }
                    }
                }
                for d in (0..nd).rev() {
                    cidx[d] += 1;
                    if cidx[d] < a.chunks[d] {
                        break;
                    }
                    cidx[d] = 0;
                }
            }
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

/// The arrays of a group: `(name, metadata)`, sorted by name.
fn group_arrays(dir: &Path) -> Vec<(String, Array)> {
    let mut out: Vec<(String, Array)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| Some((e.file_name().to_str()?.to_string(), read_array(&e.path()).ok()?)))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

const X_NAMES: &[&str] = &["x", "lon", "longitude", "easting"];
const Y_NAMES: &[&str] = &["y", "lat", "latitude", "northing"];

/// Reads a Zarr store or array as a raster.
pub(crate) fn read(path: &Path) -> Result<RasterDataset> {
    let path = path.to_path_buf();
    let (array, group) = match read_array(&path) {
        Ok(a) => (a, path.parent().map(Path::to_path_buf)),
        Err(_) => {
            let arrays = group_arrays(&path);
            // The raster: the first array of two or more dimensions.
            let (_, a) = arrays
                .into_iter()
                .find(|(_, a)| a.shape.len() >= 2)
                .ok_or_else(|| bad(format!("{}: no array of two or more dimensions", path.display())))?;
            (a, Some(path.clone()))
        }
    };
    let nd = array.shape.len();
    if !(2..=3).contains(&nd) {
        return Err(Error::Unsupported(format!("Zarr arrays of {nd} dimensions are not supported as rasters")));
    }
    let lower: Vec<String> = array.dims.iter().map(|d| d.to_ascii_lowercase()).collect();
    let find = |names: &[&str], default: usize| lower.iter().position(|d| names.contains(&d.as_str())).unwrap_or(default);
    let xd = find(X_NAMES, nd - 1);
    let yd = find(Y_NAMES, nd - 2);
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
    let mut gt = None;
    let mut crs = None;
    let sibling = |name: &str| group.as_ref().and_then(|g| read_array(&g.join(name)).ok());
    if let Some(gm) = array.attrs["grid_mapping"].as_str().and_then(sibling) {
        crs = gm.attrs["crs_wkt"].as_str().or(gm.attrs["spatial_ref"].as_str()).and_then(common::crs_from_text);
        gt = gm.attrs["GeoTransform"].as_str().and_then(|s| {
            let v: Vec<f64> = s.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            <[f64; 6]>::try_from(v).ok()
        });
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
    let gt = gt.unwrap_or(common::NO_GEOTRANSFORM);
    let mut raster = common::make_raster(cols, rows, bands, data, gt, crs.unwrap_or_default(), array.fill)?;
    if gt[5] > 0.0 {
        flip_rows(&mut raster);
    }
    Ok(RasterDataset { raster, driver: RasterDriver::Zarr, has_nodata: array.fill.is_some() })
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

/// Compressors terrio writes.
fn compressor_json(name: Option<&str>) -> Result<(Value, Option<Codec>)> {
    Ok(match name.map(str::to_ascii_uppercase).as_deref() {
        None | Some("NONE") => (Value::Null, None),
        Some("ZLIB") | Some("DEFLATE") => (json!({"id": "zlib", "level": 6}), Some(Codec::Zlib)),
        Some("GZIP") => (json!({"id": "gzip", "level": 6}), Some(Codec::Gzip)),
        Some(other) => return Err(Error::InvalidArgument(format!("Zarr compression '{other}' is not supported (NONE, ZLIB, GZIP)"))),
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

/// Writes one array (`.zarray`, `.zattrs` and chunks) from little-endian sample bytes.
fn write_array(
    dir: &Path,
    (shape, chunks): (&[usize], &[usize]),
    t: DataType,
    bytes: &[u8],
    fill: Option<f64>,
    attrs: Value,
    compression: Option<&str>,
) -> Result<(Value, Value)> {
    std::fs::create_dir_all(dir)?;
    let (compressor, codec) = compressor_json(compression)?;
    let zarray = json!({
        "chunks": chunks,
        "compressor": compressor,
        "dtype": dtype_v2(t),
        "fill_value": fill_json(fill),
        "filters": null,
        "order": "C",
        "shape": shape,
        "zarr_format": 2,
    });
    let size = t.size_bytes();
    let nd = shape.len();
    let grid: Vec<usize> = shape.iter().zip(chunks).map(|(s, c)| s.div_ceil(*c)).collect();
    let chunk_len: usize = chunks.iter().product();
    let mut idx = vec![0usize; nd];
    for _ in 0..grid.iter().product::<usize>() {
        let mut chunk = vec![0u8; chunk_len * size];
        if let Some(f) = fill {
            let mut one = Vec::new();
            super::gtiff_write::encode_sample_le(t, f, &mut one);
            for c in chunk.chunks_mut(size) {
                c.copy_from_slice(&one);
            }
        }
        let mut cidx = vec![0usize; nd];
        for e in 0..chunk_len {
            let mut flat = 0usize;
            let mut inside = true;
            for d in 0..nd {
                let g = idx[d] * chunks[d] + cidx[d];
                if g >= shape[d] {
                    inside = false;
                    break;
                }
                flat = flat * shape[d] + g;
            }
            if inside {
                chunk[e * size..(e + 1) * size].copy_from_slice(&bytes[flat * size..(flat + 1) * size]);
            }
            for d in (0..nd).rev() {
                cidx[d] += 1;
                if cidx[d] < chunks[d] {
                    break;
                }
                cidx[d] = 0;
            }
        }
        let key = idx.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(".");
        std::fs::write(dir.join(key), compress(&codec, chunk)?)?;
        for d in (0..nd).rev() {
            idx[d] += 1;
            if idx[d] < grid[d] {
                break;
            }
            idx[d] = 0;
        }
    }
    std::fs::write(dir.join(".zarray"), serde_json::to_string_pretty(&zarray).unwrap_or_default())?;
    std::fs::write(dir.join(".zattrs"), serde_json::to_string_pretty(&attrs).unwrap_or_default())?;
    Ok((zarray, attrs))
}

/// Writes a raster as a Zarr v2 group, as GDAL does: the array (named after the store, with
/// dimensions `[Band,] Y, X`), X and Y coordinate arrays, the CRS in `_CRS`, and consolidated
/// metadata.
pub(crate) fn write(r: &Raster, nodata: Option<f64>, path: &Path, compression: Option<&str>) -> Result<()> {
    if path.exists() {
        return Err(Error::InvalidArgument(format!("{} already exists", path.display())));
    }
    std::fs::create_dir_all(path)?;
    let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("data").to_string();
    let mut meta = serde_json::Map::new();
    let zgroup = json!({"zarr_format": 2});
    std::fs::write(path.join(".zgroup"), serde_json::to_string_pretty(&zgroup).unwrap_or_default())?;
    std::fs::write(path.join(".zattrs"), "{}")?;
    meta.insert(".zgroup".into(), zgroup);
    meta.insert(".zattrs".into(), json!({}));

    let (shape, chunks, dims) = if r.bands == 1 {
        (vec![r.rows, r.cols], vec![r.rows.min(256), r.cols.min(256)], json!(["Y", "X"]))
    } else {
        (vec![r.bands, r.rows, r.cols], vec![1, r.rows.min(256), r.cols.min(256)], json!(["Band", "Y", "X"]))
    };
    let mut attrs = json!({"_ARRAY_DIMENSIONS": dims});
    let s = super::common::spatial_ref(&r.crs);
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
    let (za, zat) = write_array(&path.join(&name), (&shape, &chunks), r.data_type, &bytes, nodata, attrs, compression)?;
    meta.insert(format!("{name}/.zarray"), za);
    meta.insert(format!("{name}/.zattrs"), zat);

    // Coordinates of the pixel centres.
    let xs: Vec<u8> = (0..r.cols).flat_map(|c| (r.x_min + (c as f64 + 0.5) * r.cell_size_x).to_le_bytes()).collect();
    let ys: Vec<u8> = (0..r.rows).flat_map(|row| (r.y_max() - (row as f64 + 0.5) * r.cell_size_y).to_le_bytes()).collect();
    for (dim, n, data) in [("X", r.cols, xs), ("Y", r.rows, ys)] {
        let (za, zat) = write_array(&path.join(dim), (&[n], &[n]), DataType::F64, &data, None, json!({"_ARRAY_DIMENSIONS": [dim]}), None)?;
        meta.insert(format!("{dim}/.zarray"), za);
        meta.insert(format!("{dim}/.zattrs"), zat);
    }
    let consolidated = json!({"metadata": meta, "zarr_consolidated_format": 1});
    std::fs::write(path.join(".zmetadata"), serde_json::to_string_pretty(&consolidated).unwrap_or_default())?;
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
        let p = dir.path().join("t.zarr");
        write(&r, Some(-1.0), &p, Some("ZLIB")).unwrap();
        let ds = read(&p).unwrap();
        assert_eq!(ds.raster.data_f32().unwrap(), &data[..]);
        assert_eq!(ds.nodata(), Some(-1.0));
        assert_eq!(ds.crs().epsg, Some(4326));
        let gt = ds.geo_transform();
        assert!((gt[0] + 110.0).abs() < 1e-9 && (gt[3] - 40.0).abs() < 1e-9, "{gt:?}");
    }
}
