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
use super::zarr_store::{Node, Range};

mod multidim;
use crate::formats::RasterDriver;
use crate::memfs;
use crate::{Error, RasterDataset, Result};
pub(crate) use multidim::{MdSource, md_open, md_write};
use serde_json::{Value, json};
use std::io::Read;
use std::path::Path;
use wbraster::raster::RasterData;
use wbraster::{CrsInfo, DataType, Raster};

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("Zarr: {msg}"))
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

/// How elements are stored when not as the array's data type.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Stored {
    /// IEEE half precision (read as Float32, as GDAL before 3.11 and xarray's float32 casts do).
    F16,
    Type(DataType),
}

impl Stored {
    fn size(self) -> usize {
        match self {
            Stored::F16 => 2,
            Stored::Type(t) => t.size_bytes(),
        }
    }

    /// Reads one element.
    fn read(self, b: &[u8], little: bool) -> f64 {
        macro_rules! num {
            ($t:ty) => {{
                let a: [u8; size_of::<$t>()] = b.try_into().unwrap_or_default();
                (if little { <$t>::from_le_bytes(a) } else { <$t>::from_be_bytes(a) }) as f64
            }};
        }
        match self {
            Stored::F16 => {
                let a: [u8; 2] = b.try_into().unwrap_or_default();
                half::f16::from_bits(if little { u16::from_le_bytes(a) } else { u16::from_be_bytes(a) }).to_f64()
            }
            Stored::Type(t) => match t {
                DataType::U8 => b[0] as f64,
                DataType::I8 => b[0] as i8 as f64,
                DataType::U16 => num!(u16),
                DataType::I16 => num!(i16),
                DataType::U32 => num!(u32),
                DataType::I32 => num!(i32),
                DataType::U64 => num!(u64),
                DataType::I64 => num!(i64),
                DataType::F32 => num!(f32),
                DataType::F64 => num!(f64),
            },
        }
    }
}

/// An array-to-array codec that changes the values (Zarr v3 extensions).
#[derive(Debug, Clone)]
enum Step {
    /// `scale_offset`: stored as `(value - offset) * scale`.
    ScaleOffset { scale: f64, offset: f64 },
    /// `cast_value`: stored in another type, with the stored values mapped to special values
    /// (`(stored, value)`, NaN matching NaN).
    Cast { decode: Vec<(f64, f64)> },
}

/// How a chunk's bytes encode its elements.
#[derive(Debug, Clone)]
struct Pipeline {
    /// The order of the stored dimensions: stored dimension `i` is the array's `order[i]`. `None`
    /// for C order.
    order: Option<Vec<usize>>,
    /// A Zarr v2 chunk in Fortran order, which GDAL's rule counts twice: GDAL transposes it into a
    /// second buffer of its size (terrio reads it in place).
    fortran: bool,
    little_endian: bool,
    /// The bytes-to-bytes codecs, in decoding order.
    codecs: Vec<Codec>,
    /// A sharded chunk: inner chunks and their index.
    shard: Option<Box<Shard>>,
    /// The stored element type, when not the array's.
    stored: Option<Stored>,
    /// The array-to-array codecs changing values, in encoding order.
    steps: Vec<Step>,
}

impl Default for Pipeline {
    fn default() -> Self {
        Self { order: None, fortran: false, little_endian: true, codecs: Vec::new(), shard: None, stored: None, steps: Vec::new() }
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

impl Shard {
    /// The number of inner chunks of a shard of shape `shape`, and the length of its index. More
    /// than the address space holds is an error.
    fn index_len(&self, shape: &[usize]) -> Result<(usize, usize)> {
        let count = shape.iter().zip(&self.chunks).try_fold(1usize, |n, (c, i)| n.checked_mul(c.div_ceil(*i))).ok_or_else(too_large)?;
        let len = count.checked_mul(16).and_then(|l| l.checked_add(if self.index_checksum { 4 } else { 0 })).ok_or_else(too_large)?;
        Ok((count, len))
    }
}

/// GDAL's limit on a chunk's decoding (`ZarrArray::AllocateWorkingBuffers`: "By default the driver
/// limits to 1 GB"): a chunk whose elements take more bytes as stored is refused, unless
/// [`big_chunks_allowed`].
const MAX_CHUNK_BYTES: usize = 1 << 30;

/// The bytes a codec may output past those its chunk decodes to, which are read from the start of
/// the output (what follows them is ignored): GDAL's own slack in its codec buffers (`n + n / 3 +
/// 64`), room for a checksum or padding a writer appends without declaring it.
const CODEC_SLACK: usize = 64;

/// "Too large chunks", GDAL's error for chunk dimensions whose product overflows.
fn too_large() -> Error {
    bad("Too large chunks")
}

/// The number of elements of a chunk (or region) of shape `shape`.
fn element_count(shape: &[usize]) -> Result<usize> {
    shape.iter().try_fold(1usize, |n, &d| n.checked_mul(d)).ok_or_else(too_large)
}

/// The bytes of `n` elements of `size` bytes.
fn bytes_of(n: usize, size: usize) -> Result<usize> {
    n.checked_mul(size).ok_or_else(too_large)
}

/// Whether chunks of more than [`MAX_CHUNK_BYTES`] are decoded: `TERRIO_ZARR_ALLOW_BIG_TILE_SIZE`,
/// else GDAL's `ZARR_ALLOW_BIG_TILE_SIZE`, true unless `NO`, `FALSE`, `OFF` or `0` (GDAL's
/// `CPLTestBool`); unset, they are refused. Read on the calling thread, before chunks are decoded
/// on the pool's, which do not see its scoped options.
fn big_chunks_allowed() -> bool {
    crate::config::get_any(&["TERRIO_ZARR_ALLOW_BIG_TILE_SIZE", "ZARR_ALLOW_BIG_TILE_SIZE"])
        .is_some_and(|v| !matches!(v.trim().to_ascii_uppercase().as_str(), "NO" | "FALSE" | "OFF" | "0"))
}

/// The most bytes `codec` encodes `n` bytes into: four more for a CRC32C checksum; for a
/// compressor, GDAL's bound for its codec buffers (`ZarrV3CodecSequence::AllocateBuffer`), above
/// the worst case of each format read here (zlib's `deflateBound`, at most `n + n / 8 + n / 64 +
/// 25` with a gzip header and trailer; Zstandard's `ZSTD_COMPRESSBOUND`, `n + n / 256 + 64`; LZ4's
/// `n + n / 255 + 16` and the 4 bytes of numcodecs' size; Blosc's `n + 16`).
fn encoded_bound(codec: &Codec, n: usize) -> Result<usize> {
    match codec {
        Codec::Crc32c => n.checked_add(4),
        _ => n.checked_add(n / 3).and_then(|b| b.checked_add(64)),
    }
    .ok_or_else(too_large)
}

impl Pipeline {
    /// The bytes the array-to-bytes codec makes of a chunk of shape `shape` and type `t`: its
    /// elements as stored, or, sharded, its inner chunks (each at most [`Pipeline::encoded_len`])
    /// and its index.
    fn stage_len(&self, shape: &[usize], t: DataType) -> Result<usize> {
        let Some(shard) = &self.shard else {
            return bytes_of(element_count(shape)?, self.stored.unwrap_or(Stored::Type(t)).size());
        };
        let (count, index_len) = shard.index_len(shape)?;
        let inner = shard.inner.encoded_len(&shard.chunks, t)?;
        count.checked_mul(inner).and_then(|b| b.checked_add(index_len)).ok_or_else(too_large)
    }

    /// The most bytes a chunk of shape `shape` takes encoded: its [`Pipeline::stage_len`] through
    /// each bytes-to-bytes codec at its worst.
    fn encoded_len(&self, shape: &[usize], t: DataType) -> Result<usize> {
        self.codecs.iter().rev().try_fold(self.stage_len(shape, t)?, |n, c| encoded_bound(c, n))
    }

    /// The most bytes each bytes-to-bytes codec (in decoding order) may output decoding a chunk of
    /// shape `shape`: the last, what the chunk decodes to and [`CODEC_SLACK`]; each before it, what
    /// the next one's input can be at most.
    fn output_bounds(&self, shape: &[usize], t: DataType) -> Result<Vec<usize>> {
        let mut most = self.stage_len(shape, t)?.checked_add(CODEC_SLACK).ok_or_else(too_large)?;
        let mut bounds = vec![0; self.codecs.len()];
        for (k, c) in self.codecs.iter().enumerate().rev() {
            bounds[k] = most;
            most = encoded_bound(c, most)?;
        }
        Ok(bounds)
    }

    /// Refuses a chunk of shape `shape` whose working buffers, as GDAL 3.12.1 counts them
    /// (`ZarrV2Array::AllocateWorkingBuffers`), take more than [`MAX_CHUNK_BYTES`] unless
    /// `allow_big`, as GDAL refuses it before reading: its elements as stored, twice for a v2 chunk
    /// in Fortran order (the copy GDAL transposes it into). A shard's elements are stored as its
    /// inner chunks store them. A byte-swapped chunk is counted once, where GDAL counts its swapped
    /// copy too: terrio swaps it in place.
    fn check_chunk_size(&self, shape: &[usize], t: DataType, allow_big: bool) -> Result<()> {
        let stored = match &self.shard {
            Some(shard) => shard.inner.stored,
            None => self.stored,
        };
        let elements = bytes_of(element_count(shape)?, stored.unwrap_or(Stored::Type(t)).size())?;
        let bytes = if self.fortran { elements.checked_mul(2).ok_or_else(|| bad("Too large chunk size"))? } else { elements };
        if bytes > MAX_CHUNK_BYTES && !allow_big {
            return Err(bad(format!(
                "tile allocation would require {bytes} bytes. By default the driver limits to 1 GB. To allow that memory \
                 allocation, set the TERRIO_ZARR_ALLOW_BIG_TILE_SIZE (or ZARR_ALLOW_BIG_TILE_SIZE) configuration option to YES."
            )));
        }
        Ok(())
    }

    /// Whether a chunk's elements are converted into a buffer of their own to be read as type `t`:
    /// stored as another type, or changed by array-to-array codecs.
    fn converts(&self, t: DataType) -> bool {
        !self.steps.is_empty() || self.stored.unwrap_or(Stored::Type(t)) != Stored::Type(t)
    }

    /// The bytes decoding a chunk of shape `shape` (not a shard) allocates beside its stored bytes:
    /// the codecs' output, reserved at once for the chunk's elements as stored and the slack, and
    /// a buffer of its elements when they are converted. Nothing else: a chunk in Fortran order,
    /// or transposed, is read in the order it is stored.
    fn decoding_bytes(&self, shape: &[usize], t: DataType) -> Result<usize> {
        let decoded =
            if self.codecs.is_empty() { 0 } else { self.stage_len(shape, t)?.checked_add(CODEC_SLACK + 1).ok_or_else(too_large)? };
        let converted = if self.converts(t) { bytes_of(element_count(shape)?, t.size_bytes())? } else { 0 };
        decoded.checked_add(converted).ok_or_else(too_large)
    }
}

/// An array's metadata.
#[derive(Debug)]
struct Array {
    node: Node,
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

impl Array {
    /// The key of the chunk at grid position `idx`.
    fn chunk_key(&self, idx: &[usize]) -> String {
        if idx.is_empty() {
            return format!("{}0", self.key_prefix);
        }
        format!("{}{}", self.key_prefix, idx.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(&self.separator))
    }

    /// The fill value as one little-endian element.
    fn fill_bytes(&self) -> Vec<u8> {
        let mut one = Vec::new();
        super::gtiff_write::encode_sample_le(self.data_type, self.fill.unwrap_or(0.0), &mut one);
        one
    }
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

/// A v2 data type: the array's type, whether it is little-endian, and how it is stored when not
/// as that type.
fn v2_dtype(s: &str) -> Result<(DataType, bool, Option<Stored>)> {
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
        "f2" => return Ok((DataType::F32, little, Some(Stored::F16))),
        "f4" => DataType::F32,
        "f8" => DataType::F64,
        other => return Err(Error::Unsupported(format!("Zarr data type '{other}' is not supported"))),
    };
    Ok((t, little, None))
}

/// A v3 data type, and how it is stored when not as the type terrio reads it as.
fn v3_dtype(s: &str) -> Result<(DataType, Option<Stored>)> {
    Ok((
        match s {
            "bool" | "uint8" => DataType::U8,
            "int8" => DataType::I8,
            "uint16" => DataType::U16,
            "int16" => DataType::I16,
            "uint32" => DataType::U32,
            "int32" => DataType::I32,
            "uint64" => DataType::U64,
            "int64" => DataType::I64,
            "float16" => return Ok((DataType::F32, Some(Stored::F16))),
            "float32" => DataType::F32,
            "float64" => DataType::F64,
            other => return Err(Error::Unsupported(format!("Zarr data type '{other}' is not supported"))),
        },
        None,
    ))
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
fn read_array(node: &Node) -> Result<Array> {
    read_array_opt(node)?.ok_or_else(|| bad(format!("{} is not a Zarr array", node.display())))
}

/// Reads an array's metadata: `None` when the node has none (a group, or nothing). Metadata that
/// cannot be read is an error, and so is a document that is not JSON, unless the other Zarr
/// version's document describes the node (some servers answer unknown paths with a page).
fn read_array_opt(node: &Node) -> Result<Option<Array>> {
    let v2 = match node.json(".zarray") {
        Err(e @ Error::InvalidData(_)) => Err(e),
        r => Ok(r?),
    };
    if let Ok(Some(m)) = v2 {
        // GDAL 3.12.1 (`ZarrV2Group::LoadArray`): the value as text must be "2".
        let format = match &m["zarr_format"] {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            v => v.to_string(),
        };
        if format != "2" {
            return Err(bad(format!("Invalid value for zarr_format: {format}")));
        }
        let attrs = node.json(".zattrs")?.unwrap_or(json!({}));
        let (data_type, little_endian, stored) = v2_dtype(m["dtype"].as_str().ok_or_else(|| bad("no dtype"))?)?;
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
        let fortran = m["order"].as_str() == Some("F");
        let order = fortran.then(|| (0..shape.len()).rev().collect());
        return Ok(Some(Array {
            node: node.clone(),
            shape,
            chunks: usizes(&m["chunks"]),
            data_type,
            pipeline: Pipeline { order, fortran, little_endian, codecs, shard: None, stored, steps: Vec::new() },
            fill: parse_fill(&m["fill_value"]),
            key_prefix: String::new(),
            separator: m["dimension_separator"].as_str().unwrap_or(".").to_string(),
            dims,
            attrs,
        }));
    }
    let m = match (v2, node.json("zarr.json")?) {
        (_, Some(m)) => m,
        (Err(e), None) => return Err(e),
        (Ok(_), None) => return Ok(None),
    };
    if m["node_type"].as_str() != Some("array") {
        return Ok(None);
    }
    // GDAL 3.12.1 (`ZarrSharedResource::OpenRootGroup`, `ZarrV3Group::ExploreDirectory`): the
    // value read as an integer (json-c's: a number truncated, a string parsed, otherwise 0) must
    // be 3.
    let format = match &m["zarr_format"] {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        Value::Bool(b) => Some(i64::from(*b)),
        _ => None,
    };
    if format != Some(3) {
        return Err(bad("Unhandled zarr_format value"));
    }
    let (data_type, stored) = v3_dtype(m["data_type"].as_str().unwrap_or_default())?;
    let usizes =
        |v: &Value| v.as_array().map(|a| a.iter().filter_map(|x| x.as_u64().map(|x| x as usize)).collect::<Vec<_>>()).unwrap_or_default();
    let shape = usizes(&m["shape"]);
    let pipeline = v3_pipeline(&m["codecs"], shape.len(), stored)?;
    let encoding = &m["chunk_key_encoding"];
    let separator = encoding["configuration"]["separator"]
        .as_str()
        .unwrap_or(if encoding["name"].as_str() == Some("v2") { "." } else { "/" })
        .to_string();
    let key_prefix = if encoding["name"].as_str() == Some("v2") { String::new() } else { format!("c{separator}") };
    let dims =
        m["dimension_names"].as_array().map(|a| a.iter().map(|d| d.as_str().unwrap_or_default().to_string()).collect()).unwrap_or_default();
    Ok(Some(Array {
        node: node.clone(),
        shape,
        chunks: usizes(&m["chunk_grid"]["configuration"]["chunk_shape"]),
        data_type,
        pipeline,
        fill: parse_fill(&m["fill_value"]),
        key_prefix,
        separator,
        dims,
        attrs: m["attributes"].clone(),
    }))
}

/// Parses a Zarr v3 codec chain for an array of `nd` dimensions, whose elements are stored as
/// `stored` when not as its data type.
fn v3_pipeline(codecs: &Value, nd: usize, stored: Option<Stored>) -> Result<Pipeline> {
    let mut p = Pipeline { stored, ..Default::default() };
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
            "scale_offset" => {
                let scale = config["scale"].as_f64().unwrap_or(1.0);
                let offset = config["offset"].as_f64().unwrap_or(0.0);
                if scale == 0.0 {
                    return Err(bad("scale_offset with a zero scale"));
                }
                p.steps.push(Step::ScaleOffset { scale, offset });
            }
            "cast_value" => {
                let (t, s) = v3_dtype(config["data_type"].as_str().unwrap_or_default())?;
                p.stored = Some(s.unwrap_or(Stored::Type(t)));
                let decode = config["scalar_map"]["decode"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|pair| Some((parse_fill(pair.get(0)?)?, parse_fill(pair.get(1)?)?)))
                    .collect();
                p.steps.push(Step::Cast { decode });
            }
            "sharding_indexed" => {
                let chunks: Vec<usize> =
                    config["chunk_shape"].as_array().into_iter().flatten().filter_map(|x| x.as_u64().map(|x| x as usize)).collect();
                if chunks.len() != nd || chunks.contains(&0) {
                    return Err(bad("sharding: invalid inner chunk shape"));
                }
                let inner = v3_pipeline(&config["codecs"], nd, p.stored)?;
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
                // The shard's inner chunks hold the elements.
                p.stored = None;
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

/// An empty buffer with room for `n` bytes, allocated fallibly (its size comes from a store's
/// metadata: an allocation that fails is an error for that store, not the end of the process).
fn buffer(n: usize, what: &str) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    crate::dataset::fallible::reserve(&mut out, n, what)?;
    Ok(out)
}

/// A copy of `b`, allocated fallibly.
fn copied(b: &[u8], what: &str) -> Result<Vec<u8>> {
    let mut out = buffer(b.len(), what)?;
    out.extend_from_slice(b);
    Ok(out)
}

/// The buffer a decompressor writes at most `most` bytes, and one more, into: reserved at once, so
/// that a chunk's decoded bytes are not held twice (or reserved twice over) as a growing buffer
/// is moved or doubled.
fn codec_output(most: usize) -> Result<Vec<u8>> {
    buffer(most.saturating_add(1), "a Zarr chunk's decoded bytes")
}

/// An LZ4 block decoded into a buffer of `size` bytes, allocated fallibly: the bytes it decodes to.
fn lz4_into(block: &[u8], size: usize, what: &str) -> Result<Vec<u8>> {
    let mut out = crate::dataset::fallible::filled(size, 0u8, "an LZ4 block")?;
    let n = lz4_flex::block::decompress_into(block, &mut out).map_err(|e| bad(format!("{what}: {e}")))?;
    out.truncate(n);
    Ok(out)
}

/// Decodes one chunk's bytes, to at most `most` bytes: more is an error rather than an allocation
/// of whatever a small chunk expands to, and a size the chunk states past `most` is refused
/// before it is allocated.
fn decompress(codec: &Codec, b: Vec<u8>, most: usize) -> Result<Vec<u8>> {
    Ok(match codec {
        Codec::Zlib => read_at_most(flate2::read::ZlibDecoder::new(&b[..]), most, "zlib: a chunk")?,
        Codec::Gzip => read_at_most(flate2::read::GzDecoder::new(&b[..]), most, "gzip: a chunk")?,
        Codec::Zstd => zstd_decode(&b, most)?,
        Codec::Lz4 => {
            // numcodecs' LZ4: the uncompressed size (u32, little-endian), then an LZ4 block,
            // decoded into a buffer of that size.
            let (size, block) = lz4_flex::block::uncompressed_size(&b).map_err(|e| bad(format!("LZ4: {e}")))?;
            if size > most {
                return Err(bad(format!("LZ4: a chunk declaring {size} bytes, more than {most}")));
            }
            lz4_into(block, size, "LZ4")?
        }
        Codec::Blosc => blosc_decompress(&b, most)?,
        Codec::Crc32c => {
            let mut out = b;
            out.truncate(out.len().saturating_sub(4));
            out
        }
    })
}

/// Reads a decompressing reader to its end, `what` decompressing to more than `most` bytes being
/// an error.
fn read_at_most(r: impl Read, most: usize, what: &str) -> Result<Vec<u8>> {
    let mut out = codec_output(most)?;
    r.take((most as u64).saturating_add(1)).read_to_end(&mut out)?;
    if out.len() > most {
        return Err(bad(format!("{what} decompressing to more than {most} bytes")));
    }
    Ok(out)
}

/// Decompresses every frame of `b`, to at most `most` bytes: more is an error rather than an
/// allocation of whatever a small chunk expands to.
fn zstd_decode(b: &[u8], most: usize) -> Result<Vec<u8>> {
    let mut out = codec_output(most)?;
    let mut src = b;
    while !src.is_empty() && out.len() <= most {
        let d = ruzstd::decoding::StreamingDecoder::new(&mut src).map_err(|e| bad(format!("zstd: {e}")))?;
        d.take(((most - out.len()) as u64).saturating_add(1)).read_to_end(&mut out)?;
    }
    if out.len() > most {
        return Err(bad(format!("zstd: a chunk decompressing to more than {most} bytes")));
    }
    Ok(out)
}

/// Decompresses a Blosc (version 1 format) buffer of at most `most` bytes: a header declaring more
/// is refused before anything is allocated, and each block decompresses to its size or is an
/// error, as in c-blosc.
fn blosc_decompress(src: &[u8], most: usize) -> Result<Vec<u8>> {
    if src.len() < 16 {
        return Err(bad("Blosc: truncated header"));
    }
    let flags = src[2];
    let typesize = src[3].max(1) as usize;
    let u32_at = |i: usize| src.get(i..i + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    let nbytes = u32_at(4).unwrap_or(0);
    if nbytes > most {
        return Err(bad(format!("Blosc: a chunk declaring {nbytes} bytes, more than {most}")));
    }
    let blocksize = u32_at(8).unwrap_or(0).max(1);
    if flags & 0x02 != 0 {
        return copied(src.get(16..16 + nbytes).ok_or_else(|| bad("Blosc: truncated data"))?, "a Blosc chunk");
    }
    let compressor = flags >> 5;
    let nblocks = nbytes.div_ceil(blocksize);
    let mut out = buffer(nbytes, "a Blosc chunk")?;
    for j in 0..nblocks {
        let bsize = blocksize.min(nbytes - j * blocksize);
        let leftover = bsize < blocksize;
        let split = flags & 0x10 == 0 && typesize <= 16 && blocksize / typesize >= 128 && !leftover;
        let nsplits = if split { typesize } else { 1 };
        let mut pos = u32_at(16 + 4 * j).ok_or_else(|| bad("Blosc: truncated block offsets"))?;
        let mut block = buffer(bsize, "a Blosc block")?;
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
            // Each into at most the `neblock` bytes it stands for.
            let decoded = match compressor {
                0 => blosclz_decompress(stream, neblock)?,
                1 => lz4_into(stream, neblock, "Blosc LZ4")?,
                3 => read_at_most(flate2::read::ZlibDecoder::new(stream), neblock, "Blosc zlib: a block")?,
                4 => zstd_decode(stream, neblock)?,
                c => return Err(Error::Unsupported(format!("Blosc compressor {c} is not supported"))),
            };
            block.extend_from_slice(&decoded);
        }
        // Short, the block would be unshuffled out of its bounds.
        if block.len() != bsize {
            return Err(bad(format!("Blosc: a block decompressing to {} bytes, {bsize} expected", block.len())));
        }
        if flags & 0x01 == 0 && flags & 0x04 != 0 && bsize >= typesize {
            block = bit_unshuffle(&block, bsize, typesize, src[0])?;
        } else if flags & 0x01 != 0 && typesize > 1 {
            // Undo the byte shuffle: the block holds byte 0 of every element, then byte 1...
            let n = bsize / typesize;
            let mut unshuffled = crate::dataset::fallible::filled_now(bsize, 0u8, "a Blosc block")?;
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
fn bit_unshuffle(block: &[u8], bsize: usize, typesize: usize, version: u8) -> Result<Vec<u8>> {
    let nelems = bsize / typesize;
    if version <= 2 && !nelems.is_multiple_of(8) {
        return copied(&block[..bsize], "a Blosc block");
    }
    let n = nelems - nelems % 8;
    let mut out = crate::dataset::fallible::filled_now(bsize, 0u8, "a Blosc block")?;
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
    Ok(out)
}

/// BloscLZ decompression (a FastLZ variant), to at most `maxout` bytes: more is an error, as in
/// c-blosc.
fn blosclz_decompress(src: &[u8], maxout: usize) -> Result<Vec<u8>> {
    const MAX_DISTANCE: usize = 8191;
    let err = || bad("BloscLZ: corrupt stream");
    let past = || bad(format!("BloscLZ: a block decompressing to more than {maxout} bytes"));
    let mut out = buffer(maxout, "a BloscLZ block")?;
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
            if out.len() + len > maxout {
                return Err(past());
            }
            for k in 0..len {
                let b = out[start + k];
                out.push(b);
            }
        } else {
            let n = ctrl + 1;
            if out.len() + n > maxout {
                return Err(past());
            }
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

/// Converts a chunk's decoded bytes (`n` stored elements) to little-endian elements of the
/// array's type, in the stored order: in place when they are stored as that type, so that a chunk
/// is not held twice; otherwise into a buffer allocated fallibly.
fn to_array_elements(p: &Pipeline, mut data: Vec<u8>, n: usize, t: DataType) -> Result<Vec<u8>> {
    let size = t.size_bytes();
    let stored = p.stored.unwrap_or(Stored::Type(t));
    let ssize = stored.size();
    if data.len() < n * ssize {
        return Err(bad(format!("holds {} bytes, expected {}", data.len(), n * ssize)));
    }
    if !p.converts(t) {
        data.truncate(n * size);
        if !p.little_endian && size > 1 {
            for e in data.chunks_mut(size) {
                e.reverse();
            }
        }
        return Ok(data);
    }
    let f32_math = t == DataType::F32;
    let mut out = buffer(n * size, "a Zarr chunk")?;
    for e in data[..n * ssize].chunks(ssize) {
        let mut v = stored.read(e, p.little_endian);
        for step in p.steps.iter().rev() {
            match step {
                Step::Cast { decode } => {
                    if let Some((_, to)) = decode.iter().find(|(from, _)| *from == v || (from.is_nan() && v.is_nan())) {
                        v = *to;
                    }
                }
                Step::ScaleOffset { scale, offset } => {
                    v = if f32_math { ((v as f32) / (*scale as f32) + (*offset as f32)) as f64 } else { v / scale + offset };
                }
            }
        }
        super::gtiff_write::encode_sample_le(t, v, &mut out);
    }
    Ok(out)
}

/// `n` copies of the element `fill`, allocated fallibly, an error rather than the end of the
/// process: zeros come from the allocator zeroed, their pages untouched until written.
fn repeated(fill: &[u8], n: usize, what: &str) -> Result<Vec<u8>> {
    let len = n.checked_mul(fill.len()).ok_or_else(|| crate::dataset::fallible::out_of_memory(None, what))?;
    if fill.iter().all(|&b| b == 0) {
        return Ok(crate::dataset::fallible::filled(len, 0u8, what)?);
    }
    let mut out = Vec::new();
    crate::dataset::fallible::reserve(&mut out, len, what)?;
    for _ in 0..n {
        out.extend_from_slice(fill);
    }
    Ok(out)
}

/// A decoded chunk: its little-endian elements in the order they are stored, `strides` elements
/// apart along each of the array's dimensions. A chunk in Fortran order, or transposed, is read in
/// place rather than copied into C order, so that it is held once.
struct Decoded {
    bytes: Vec<u8>,
    strides: Vec<usize>,
}

/// The distance, in elements, between neighbours along each of the array's dimensions in a chunk
/// of shape `cshape` stored in `order` (stored dimension `i` is the array's `order[i]`), or in C
/// order.
fn strides(order: Option<&[usize]>, cshape: &[usize]) -> Vec<usize> {
    let c_order: Vec<usize> = (0..cshape.len()).collect();
    let mut out = vec![0; cshape.len()];
    let mut step = 1usize;
    for &d in order.unwrap_or(&c_order).iter().rev() {
        out[d] = step;
        step = step.saturating_mul(cshape[d]);
    }
    out
}

/// Decodes a chunk of shape `cshape` into little-endian elements of type `t`, in the order they
/// are stored (a shard's, in C order), inner chunks of a shard missing from it set to `fill` (one
/// element). A chunk past [`MAX_CHUNK_BYTES`] is refused unless `allow_big`, and so is a chunk
/// whose decoding allocates more than the memory the process can use; each codec's output is
/// bounded by what the chunk needs ([`Pipeline::output_bounds`]).
fn chunk_buffer(p: &Pipeline, raw: Vec<u8>, cshape: &[usize], t: DataType, fill: &[u8], allow_big: bool) -> Result<Decoded> {
    p.check_chunk_size(cshape, t, allow_big)?;
    let n = element_count(cshape)?;
    if p.shard.is_none() {
        // Refused before it is reserved: where the system overcommits memory, a reservation the
        // process cannot hold succeeds, and the process is killed as the decoder writes it.
        let bytes = p.decoding_bytes(cshape, t)?;
        crate::dataset::fallible::check_usable(Some(bytes), || "cannot allocate a Zarr chunk's decoded bytes".into()).map_err(bad)?;
    }
    let mut data = raw;
    for (c, most) in p.codecs.iter().zip(p.output_bounds(cshape, t)?) {
        data = decompress(c, data, most)?;
    }
    if let Some(shard) = &p.shard {
        let (count, index_len) = shard.index_len(cshape)?;
        if data.len() < index_len {
            return Err(bad("shard smaller than its index"));
        }
        let index_start = if shard.index_at_start { 0 } else { data.len() - index_len };
        let index = parse_index(&data[index_start..index_start + count * 16]);
        let mut out = repeated(fill, n, "a Zarr shard")?;
        let inner_grid: Vec<usize> = cshape.iter().zip(&shard.chunks).map(|(c, i)| c.div_ceil(*i)).collect();
        let zeros = vec![0usize; cshape.len()];
        for (j, entry) in index.iter().enumerate() {
            let Some((o, l)) = *entry else { continue };
            let chunk = copied(data.get(o..o.saturating_add(l)).ok_or_else(|| bad("shard index out of range"))?, "a Zarr chunk")?;
            let buf = chunk_buffer(&shard.inner, chunk, &shard.chunks, t, fill, allow_big)?;
            let origin: Vec<usize> = unravel(j, &inner_grid).iter().zip(&shard.chunks).map(|(i, c)| i * c).collect();
            scatter_strided(&buf.bytes, &buf.strides, &origin, &shard.chunks, &mut out, &zeros, cshape, t.size_bytes());
        }
        return Ok(Decoded { bytes: out, strides: strides(None, cshape) });
    }
    Ok(Decoded { bytes: to_array_elements(p, data, n, t)?, strides: strides(p.order.as_deref(), cshape) })
}

/// A shard index: `(offset, length)` of each inner chunk, `None` for missing ones.
fn parse_index(b: &[u8]) -> Vec<Option<(usize, usize)>> {
    b.as_chunks::<16>()
        .0
        .iter()
        .map(|e| {
            let o = u64::from_le_bytes(e[..8].try_into().unwrap_or_default());
            let l = u64::from_le_bytes(e[8..].try_into().unwrap_or_default());
            (o != u64::MAX || l != u64::MAX).then_some((o as usize, l as usize))
        })
        .collect()
}

/// The grid position of the `j`-th cell (C order) of a grid.
fn unravel(mut j: usize, grid: &[usize]) -> Vec<usize> {
    let mut idx = vec![0; grid.len()];
    for d in (0..grid.len()).rev() {
        idx[d] = j % grid[d].max(1);
        j /= grid[d].max(1);
    }
    idx
}

/// Copies the part of a chunk (C order, shape `cshape`, at `origin`) inside the region of shape
/// `shape` at `start` into the region's buffer (C order), elements of `size` bytes.
fn scatter(chunk: &[u8], origin: &[usize], cshape: &[usize], out: &mut [u8], start: &[usize], shape: &[usize], size: usize) {
    scatter_strided(chunk, &strides(None, cshape), origin, cshape, out, start, shape, size);
}

/// [`scatter`] for a chunk whose elements are `strides` elements apart along each dimension: in
/// runs along the last dimension when it is the chunk's fastest (C order), element by element
/// otherwise.
#[allow(clippy::too_many_arguments)]
fn scatter_strided(
    chunk: &[u8],
    strides: &[usize],
    origin: &[usize],
    cshape: &[usize],
    out: &mut [u8],
    start: &[usize],
    shape: &[usize],
    size: usize,
) {
    let nd = cshape.len();
    if nd == 0 {
        out[..size].copy_from_slice(&chunk[..size]);
        return;
    }
    let lo: Vec<usize> = (0..nd).map(|d| origin[d].max(start[d])).collect();
    let hi: Vec<usize> = (0..nd).map(|d| (origin[d] + cshape[d]).min(start[d] + shape[d])).collect();
    if (0..nd).any(|d| lo[d] >= hi[d]) {
        return;
    }
    let run = (hi[nd - 1] - lo[nd - 1]) * size;
    // The bytes between neighbours along the last dimension, in the chunk.
    let step = strides[nd - 1] * size;
    let mut idx = lo.clone();
    loop {
        let (mut c, mut o) = (0usize, 0usize);
        for d in 0..nd {
            c += (idx[d] - origin[d]) * strides[d];
            o = o * shape[d] + (idx[d] - start[d]);
        }
        let (c, o) = (c * size, o * size);
        if step == size {
            out[o..o + run].copy_from_slice(&chunk[c..c + run]);
        } else {
            for (k, e) in out[o..o + run].chunks_exact_mut(size).enumerate() {
                e.copy_from_slice(&chunk[c + k * step..c + k * step + size]);
            }
        }
        // The next run: dimensions before the last.
        let mut d = nd - 1;
        loop {
            if d == 0 {
                return;
            }
            d -= 1;
            idx[d] += 1;
            if idx[d] < hi[d] {
                break;
            }
            idx[d] = lo[d];
        }
    }
}

/// Byte ranges of a shard closer than this are fetched in one request.
const MERGE_GAP: usize = 256 * 1024;
/// The most bytes fetched in one request of inner chunks, so that large reads go in parallel.
const MAX_RUN: usize = 16 * 1024 * 1024;

/// The bytes one chunk decoding holds: a chunk's decoded bytes, or, for a shard read by parts, an
/// inner chunk's. `usize::MAX` when they overflow (such chunks are refused before decoding).
fn decoded_bytes(a: &Array) -> usize {
    let p = &a.pipeline;
    match &p.shard {
        Some(shard) if p.codecs.is_empty() => shard.inner.stage_len(&shard.chunks, a.data_type),
        _ => p.stage_len(&a.chunks, a.data_type),
    }
    .unwrap_or(usize::MAX)
}

/// How many chunks decoding to `bytes` bytes each are decoded at once: as many as the thread pool
/// runs while their buffers take at most half the memory the process can use ([`usable_memory`]),
/// and one at the least, as GDAL decodes them. Chunks of 1 GiB, which GDAL's rule allows, are
/// then decoded one after another in a process that can use 2 GiB.
///
/// [`usable_memory`]: crate::dataset::fallible::usable_memory
fn decoding_at_once(bytes: usize) -> usize {
    at_once(bytes, crate::par::threads(), crate::dataset::fallible::usable_memory())
}

/// [`decoding_at_once`] on `threads` threads in a process that can use `usable` bytes (`None`:
/// unknown, the whole pool). Chunks of 16 MiB or less are decoded on the whole pool.
fn at_once(bytes: usize, threads: usize, usable: Option<u64>) -> usize {
    let threads = threads.max(1);
    if bytes <= 1 << 24 {
        return threads;
    }
    usable.map_or(threads, |m| usize::try_from(m / 2 / bytes as u64).unwrap_or(usize::MAX).clamp(1, threads))
}

/// Calls `f` on each of `0..n`, in parallel, at most `at_once` at a time; the first error. As many
/// as the thread pool runs: all of them on the pool, as it schedules them.
fn at_most(n: usize, at_once: usize, f: impl Fn(usize) -> Result<()> + Sync + Send) -> Result<()> {
    if at_once >= crate::par::threads() {
        return crate::par::map_range_coarse(n, f).into_iter().collect();
    }
    let step = at_once.max(1);
    let mut first = 0;
    while first < n {
        let k = step.min(n - first);
        crate::par::map_range_coarse(k, |i| f(first + i)).into_iter().collect::<Result<()>>()?;
        first += k;
    }
    Ok(())
}

/// Refuses an array whose chunks GDAL's rule refuses, before any is read (see
/// [`Pipeline::check_chunk_size`]): the inner chunks of a shard read by parts, or the chunk (a
/// shard included) that is decoded whole. Whether chunks past the rule are allowed.
fn check_chunks(a: &Array) -> Result<bool> {
    let nd = a.shape.len();
    if a.chunks.len() != nd || a.chunks.contains(&0) {
        return Err(bad("chunk shape and array shape differ in rank"));
    }
    let p = &a.pipeline;
    let allow_big = big_chunks_allowed();
    match &p.shard {
        Some(shard) if p.codecs.is_empty() => {
            shard.index_len(&a.chunks)?;
            shard.inner.check_chunk_size(&shard.chunks, a.data_type, allow_big)?;
        }
        _ => p.check_chunk_size(&a.chunks, a.data_type, allow_big)?,
    }
    Ok(allow_big)
}

/// A decoded chunk handed on: its elements (see [`Decoded`]), its origin and shape.
type Put<'a> = dyn Fn(&Decoded, &[usize], &[usize]) + Sync + 'a;

/// Decodes the chunk at grid position `idx` of an array and hands it to `put`; for a shard read by
/// parts, its index, then the inner chunks `wanted` keeps (given an inner chunk's origin and
/// shape), nearby ones fetched in one request, decoded `inner_at_once` at a time. Missing chunks
/// are not handed.
fn decode_chunk(
    a: &Array,
    idx: &[usize],
    allow_big: bool,
    inner_at_once: usize,
    wanted: &(dyn Fn(&[usize], &[usize]) -> bool + Sync),
    put: &Put,
) -> Result<()> {
    let p = &a.pipeline;
    let fill = a.fill_bytes();
    let origin: Vec<usize> = idx.iter().zip(&a.chunks).map(|(i, c)| i * c).collect();
    let key = a.chunk_key(idx);
    let err = |e: Error| bad(format!("chunk {}/{key}: {e}", a.node.display()));
    let Some(shard) = p.shard.as_ref().filter(|_| p.codecs.is_empty()) else {
        let Some(raw) = a.node.get(&key, Range::Whole).map_err(err)? else { return Ok(()) };
        let buf = chunk_buffer(p, raw, &a.chunks, a.data_type, &fill, allow_big).map_err(err)?;
        put(&buf, &origin, &a.chunks);
        return Ok(());
    };
    let (count, index_len) = shard.index_len(&a.chunks).map_err(err)?;
    let range = if shard.index_at_start { Range::At(0, index_len as u64) } else { Range::Suffix(index_len as u64) };
    let Some(index) = a.node.get(&key, range).map_err(err)? else { return Ok(()) };
    if index.len() < count * 16 {
        return Err(err(bad("shard smaller than its index")));
    }
    let index = parse_index(&index[..count * 16]);
    let inner_grid: Vec<usize> = a.chunks.iter().zip(&shard.chunks).map(|(c, i)| c.div_ceil(*i)).collect();
    // The inner chunks wanted, by offset in the shard: (offset, end, origin).
    let mut chosen: Vec<(usize, usize, Vec<usize>)> = Vec::new();
    for (j, entry) in index.iter().enumerate() {
        let Some((o, l)) = *entry else { continue };
        let iorigin: Vec<usize> = unravel(j, &inner_grid).iter().zip(&shard.chunks).zip(&origin).map(|((i, c), o)| o + i * c).collect();
        if wanted(&iorigin, &shard.chunks) {
            let end = o.checked_add(l).ok_or_else(|| err(bad("shard index out of range")))?;
            chosen.push((o, end, iorigin));
        }
    }
    chosen.sort_by_key(|w| w.0);
    // Runs of nearby inner chunks, each fetched in one request.
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for (k, w) in chosen.iter().enumerate() {
        match runs.last_mut() {
            Some((s, e)) if w.0 <= chosen[*e - 1].1.saturating_add(MERGE_GAP) && w.1 - chosen[*s].0 <= MAX_RUN => *e = k + 1,
            _ => runs.push((k, k + 1)),
        }
    }
    let fetch = |r: usize| -> Result<()> {
        let (s, e) = runs[r];
        let from = chosen[s].0;
        let to = chosen[s..e].iter().map(|w| w.1).max().unwrap_or(from);
        let Some(bytes) = a.node.get(&key, Range::At(from as u64, (to - from) as u64))? else { return Ok(()) };
        for (o, end, iorigin) in &chosen[s..e] {
            let part = copied(bytes.get(o - from..end - from).ok_or_else(|| bad("shard index out of range"))?, "a Zarr chunk")?;
            let buf = chunk_buffer(&shard.inner, part, &shard.chunks, a.data_type, &fill, allow_big)?;
            put(&buf, iorigin, &shard.chunks);
        }
        Ok(())
    };
    if inner_at_once >= crate::par::threads() {
        crate::par::map_range(runs.len(), fetch).into_iter().collect::<Result<()>>().map_err(err)
    } else {
        at_most(runs.len(), inner_at_once, fetch).map_err(err)
    }
}

/// Reads the region of shape `shape` at `start` of an array: little-endian elements in C order,
/// missing chunks set to the fill value. Only the chunks (or, in shards, the inner chunks)
/// intersecting the region are read, in parallel, as many at once as the memory allows
/// ([`decoding_at_once`]).
fn read_region(a: &Array, start: &[usize], shape: &[usize]) -> Result<Vec<u8>> {
    let nd = a.shape.len();
    if a.chunks.len() != nd || a.chunks.contains(&0) {
        return Err(bad("chunk shape and array shape differ in rank"));
    }
    let size = a.data_type.size_bytes();
    // A region the process cannot hold is refused before a chunk is listed or a byte allocated:
    // its size comes from the metadata.
    let bytes = shape.iter().try_fold(1usize, |n, &d| n.checked_mul(d)).and_then(|n| n.checked_mul(size));
    let n = crate::dataset::fallible::check_usable(bytes, || format!("a region of shape {shape:?} is too large")).map_err(bad)? / size;
    let allow_big = check_chunks(a)?;
    if n == 0 {
        return Ok(Vec::new());
    }
    let out = std::sync::Mutex::new(repeated(&a.fill_bytes(), n, "a Zarr region")?);
    let first: Vec<usize> = (0..nd).map(|d| start[d] / a.chunks[d]).collect();
    let counts: Vec<usize> = (0..nd).map(|d| (start[d] + shape[d] - 1) / a.chunks[d] - first[d] + 1).collect();
    // Fewer chunks than elements: the product of the counts does not overflow.
    let total: usize = counts.iter().product();
    let put = |buf: &Decoded, origin: &[usize], cshape: &[usize]| {
        if let Ok(mut o) = out.lock() {
            scatter_strided(&buf.bytes, &buf.strides, origin, cshape, &mut o, start, shape, size);
        }
    };
    let wanted =
        |iorigin: &[usize], ishape: &[usize]| (0..nd).all(|d| iorigin[d] < start[d] + shape[d] && iorigin[d] + ishape[d] > start[d]);
    let at_once = decoding_at_once(decoded_bytes(a));
    let inner_at_once = if at_once < crate::par::threads() { 1 } else { at_once };
    at_most(total, at_once, |i| {
        let idx: Vec<usize> = unravel(i, &counts).iter().zip(&first).map(|(k, f)| k + f).collect();
        decode_chunk(a, &idx, allow_big, inner_at_once, &wanted, &put)
    })?;
    out.into_inner().map_err(|_| bad("a chunk decoder panicked"))
}

/// Selected positions grouped by chunk: `(chunk, [(position, index)])`, chunks in increasing order.
type ByChunk = Vec<(usize, Vec<(usize, usize)>)>;

/// The positions of `indices` (in any order, repeated or not) grouped by the chunk of `chunk`
/// elements holding them.
fn by_chunk(indices: &[usize], chunk: usize) -> ByChunk {
    let mut order: Vec<(usize, usize)> = indices.iter().copied().enumerate().collect();
    order.sort_unstable_by_key(|&(_, i)| i);
    let mut out: ByChunk = Vec::new();
    for (pos, i) in order {
        match out.last_mut() {
            Some((c, members)) if *c == i / chunk => members.push((pos, i)),
            _ => out.push((i / chunk, vec![(pos, i)])),
        }
    }
    out
}

/// Reads the elements of a selection of an array: along each dimension `d`, the indices `sel[d]`
/// (in any order, repeated or not), every combination of them; little-endian elements in C order
/// over the selection's shape (`sel[d].len()` along `d`), missing chunks the fill value. Only the
/// chunks (or, in shards read by parts, the inner chunks) holding a selected element are read,
/// each once, and the elements are picked from them: no buffer beside the output is larger than a
/// chunk, as GDAL's nearest-neighbour decimation reads the blocks holding the pixels it samples.
fn read_points(a: &Array, sel: &[Vec<usize>]) -> Result<Vec<u8>> {
    let nd = a.shape.len();
    if sel.len() != nd || sel.iter().zip(&a.shape).any(|(s, &len)| s.iter().any(|&i| i >= len)) {
        return Err(bad("a selection outside the array"));
    }
    let size = a.data_type.size_bytes();
    let shape: Vec<usize> = sel.iter().map(Vec::len).collect();
    let bytes = shape.iter().try_fold(1usize, |n, &d| n.checked_mul(d)).and_then(|n| n.checked_mul(size));
    let n = crate::dataset::fallible::check_usable(bytes, || format!("a selection of shape {shape:?} is too large")).map_err(bad)? / size;
    let allow_big = check_chunks(a)?;
    if n == 0 {
        return Ok(Vec::new());
    }
    let out = std::sync::Mutex::new(repeated(&a.fill_bytes(), n, "a Zarr selection")?);
    let ostrides: Vec<usize> = (0..nd).map(|d| shape[d + 1..].iter().product()).collect();
    let groups: Vec<ByChunk> = (0..nd).map(|d| by_chunk(&sel[d], a.chunks[d])).collect();
    let counts: Vec<usize> = groups.iter().map(Vec::len).collect();
    // At most one chunk per selected element: the product does not overflow.
    let total: usize = counts.iter().product();
    let at_once = decoding_at_once(decoded_bytes(a));
    let inner_at_once = if at_once < crate::par::threads() { 1 } else { at_once };
    at_most(total, at_once, |i| {
        let k = unravel(i, &counts);
        let members: Vec<&[(usize, usize)]> = (0..nd).map(|d| groups[d][k[d]].1.as_slice()).collect();
        let idx: Vec<usize> = (0..nd).map(|d| groups[d][k[d]].0).collect();
        // The selected elements inside a (sub)chunk of shape `cshape` at `origin`.
        let inside = |origin: &[usize], cshape: &[usize]| -> Vec<Vec<(usize, usize)>> {
            (0..nd).map(|d| members[d].iter().copied().filter(|&(_, x)| x >= origin[d] && x < origin[d] + cshape[d]).collect()).collect()
        };
        let wanted = |origin: &[usize], cshape: &[usize]| inside(origin, cshape).iter().all(|m| !m.is_empty());
        let put = |buf: &Decoded, origin: &[usize], cshape: &[usize]| {
            let local = inside(origin, cshape);
            if local.iter().any(Vec::is_empty) {
                return;
            }
            let Ok(mut o) = out.lock() else { return };
            // Every combination of the selected elements, the last dimension fastest.
            let mut at = vec![0usize; nd];
            loop {
                let (mut to, mut from) = (0usize, 0usize);
                for d in 0..nd {
                    let (pos, x) = local[d][at[d]];
                    to += pos * ostrides[d];
                    from += (x - origin[d]) * buf.strides[d];
                }
                o[to * size..(to + 1) * size].copy_from_slice(&buf.bytes[from * size..(from + 1) * size]);
                let mut d = nd;
                loop {
                    if d == 0 {
                        return;
                    }
                    d -= 1;
                    at[d] += 1;
                    if at[d] < local[d].len() {
                        break;
                    }
                    at[d] = 0;
                }
            }
        };
        decode_chunk(a, &idx, allow_big, inner_at_once, &wanted, &put)
    })?;
    out.into_inner().map_err(|_| bad("a chunk decoder panicked"))
}

/// An array's values, all of them, as `f64` (for coordinate arrays).
fn read_all_f64(a: &Array) -> Option<Vec<f64>> {
    let bytes = read_region(a, &vec![0; a.shape.len()], &a.shape).ok()?;
    let size = a.data_type.size_bytes();
    let n = bytes.len() / size;
    // As the region was: refused past the memory the process can use, allocated fallibly.
    crate::dataset::fallible::check_usable(n.checked_mul(8), || "a coordinate array".into()).ok()?;
    let mut out = Vec::new();
    crate::dataset::fallible::reserve(&mut out, n, "a coordinate array").ok()?;
    let stored = Stored::Type(a.data_type);
    out.extend(bytes.chunks_exact(size).map(|e| stored.read(e, true)));
    Some(out)
}

/// The most arrays listed as subdatasets, and the deepest group looked into.
const MAX_SUBDATASETS: usize = 10_000;
const MAX_GROUP_DEPTH: usize = 16;

/// The arrays of two or more dimensions of a group and the groups below it, depth first in name
/// order, as GDAL's Zarr driver lists them as subdatasets: `SUBDATASET_<n>_NAME`
/// (`ZARR:"store":/path`) and `SUBDATASET_<n>_DESC` (`[2x3x4x5] /path (Float32)`) items. None
/// when the group holds one array (of any dimensions), which is the raster.
fn subdatasets(group: &Node) -> Vec<(String, String)> {
    fn walk(node: &Node, depth: usize, out: &mut Vec<Array>) {
        // The children's metadata read in parallel (a request or two each for a remote store),
        // then visited in name order.
        let mut names = node.children();
        // No more children looked at than subdatasets can still be listed.
        names.truncate(MAX_SUBDATASETS.saturating_sub(out.len()));
        let kinds = crate::par::map_range(names.len(), |i| {
            let child = node.join(&names[i]);
            match read_array_opt(&child) {
                Ok(Some(a)) => (Some(a), false),
                _ => (None, depth < MAX_GROUP_DEPTH && is_group(&child)),
            }
        });
        for (name, (array, group)) in names.iter().zip(kinds) {
            if out.len() >= MAX_SUBDATASETS {
                return;
            }
            match array {
                Some(a) if a.shape.len() >= 2 => out.push(a),
                Some(_) => {}
                None if group => walk(&node.join(name), depth + 1, out),
                None => {}
            }
        }
    }
    let mut arrays = Vec::new();
    walk(group, 0, &mut arrays);
    if arrays.len() < 2 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (i, a) in arrays.iter().enumerate() {
        let (store, key) = a.node.store_and_key();
        let shape: Vec<String> = a.shape.iter().map(usize::to_string).collect();
        out.push((format!("SUBDATASET_{}_NAME", i + 1), format!("ZARR:\"{store}\":/{key}")));
        out.push((
            format!("SUBDATASET_{}_DESC", i + 1),
            format!("[{}] /{key} ({})", shape.join("x"), crate::info::gdal_type_name(a.data_type)),
        ));
    }
    out
}

/// The arrays of a group: `(name, metadata)`, sorted by name.
fn group_arrays(node: &Node) -> Vec<(String, Array)> {
    node.children().into_iter().filter_map(|name| Some((name.clone(), read_array(&node.join(&name)).ok()?))).collect()
}

const X_NAMES: &[&str] = &["x", "lon", "longitude", "easting"];
const Y_NAMES: &[&str] = &["y", "lat", "latitude", "northing"];

const SPATIAL_UUID: &str = "689b58e2-cf7b-45e0-9fff-9cfc0883d6b4";
const PROJ_UUID: &str = "f17cb550-5864-4468-aeb7-f3180cfb622f";
const MULTISCALES_UUID: &str = "d35379db-88df-4056-af3a-620245f8e347";

/// A group's attributes (v2 `.zattrs`, v3 `zarr.json`). Best effort: none when they cannot be
/// read.
fn group_attrs(node: &Node) -> Value {
    if let Ok(Some(v)) = node.json(".zattrs") {
        return v;
    }
    node.json("zarr.json").ok().flatten().map(|m| m["attributes"].clone()).filter(Value::is_object).unwrap_or(json!({}))
}

fn is_group(node: &Node) -> bool {
    node.json(".zgroup").is_ok_and(|g| g.is_some()) || node.json("zarr.json").ok().flatten().is_some_and(|m| m["node_type"] == "group")
}

/// A `multiscales` layout: the group holding it and its levels.
struct Multiscales {
    group: Node,
    layout: Vec<Value>,
}

fn multiscales(node: &Node) -> Option<Multiscales> {
    let attrs = group_attrs(node);
    let layout = attrs["multiscales"]["layout"].as_array().filter(|l| !l.is_empty())?.clone();
    Some(Multiscales { group: node.clone(), layout })
}

/// The array a layout level's `asset` names: an array, or the array named `name` (by default the
/// first of two or more dimensions) in a group.
fn layout_array(ms: &Multiscales, level: &Value, name: Option<&str>) -> Option<Node> {
    let p = ms.group.join(level["asset"].as_str()?);
    if is_group(&p) {
        return match name {
            Some(n) => Some(p.join(n)).filter(|a| read_array(a).is_ok()),
            None => group_arrays(&p).into_iter().find(|(_, a)| a.shape.len() >= 2).map(|(n, _)| p.join(&n)),
        };
    }
    read_array(&p).is_ok().then_some(p)
}

/// The raster array of a store or array node, its group, and the multiscales layout it is the
/// base of.
fn locate(node: &Node) -> Result<(Array, Option<Node>, Option<Multiscales>)> {
    // The node's own metadata, when it cannot be read, is reported (not looked past).
    if let Some(a) = read_array_opt(node)? {
        let group = node.parent();
        // The multiscales layout of the array's group, or of the group above (GDAL's lookup).
        let ms = group.as_ref().and_then(|g| multiscales(g).or_else(|| g.parent().as_ref().and_then(multiscales)));
        return Ok((a, group, ms));
    }
    if let Some(ms) = multiscales(node)
        && let Some(base) = ms.layout.first().and_then(|l| layout_array(&ms, l, None))
        && let Ok(a) = read_array(&base)
    {
        return Ok((a, base.parent(), Some(ms)));
    }
    let mut arrays = group_arrays(node);
    if arrays.is_empty() && node.is_remote() {
        // A remote group without consolidated metadata cannot be listed: the array GDAL and
        // terrio name after the store (`dem.zarr/dem`).
        let name = node.name();
        let stem = name.strip_suffix(".zarr").unwrap_or(&name).to_string();
        arrays.extend(read_array(&node.join(&stem)).ok().map(|a| (stem, a)));
    }
    // The raster: the first array of two or more dimensions.
    let (_, a) = arrays.into_iter().find(|(_, a)| a.shape.len() >= 2).ok_or_else(|| {
        let hint = if node.is_remote() { " (or no consolidated metadata listing its arrays: name the array)" } else { "" };
        bad(format!("{}: no array of two or more dimensions{hint}", node.display()))
    })?;
    let ms = multiscales(node);
    Ok((a, Some(node.clone()), ms))
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

/// An array read as a raster: its spatial and band dimensions.
struct Level {
    array: Array,
    xd: usize,
    yd: usize,
    /// The other dimensions, in the array's order: their every combination is a band, the last
    /// dimension varying fastest (as GDAL numbers the bands of arrays of three or more
    /// dimensions).
    bds: Vec<usize>,
    /// Rows stored south first (latitudes increasing), read north-up.
    south_up: bool,
}

impl Level {
    fn size(&self) -> (usize, usize) {
        (self.array.shape[self.xd], self.array.shape[self.yd])
    }

    /// The index along each band dimension of a band (numbered from 1).
    fn band_index(&self, band: usize) -> Vec<usize> {
        band_index(&self.bds.iter().map(|&d| self.array.shape[d]).collect::<Vec<_>>(), band)
    }

    /// Reads a window (in north-up rows) of some bands (numbered from 1), band after band. Bands
    /// whose chunks lie apart are read apart, so that the chunks between them are not read. A
    /// window the process cannot hold is refused before anything is read.
    fn read(&self, w: crate::Window, bands: &[usize]) -> Result<RasterData> {
        let size = self.array.data_type.size_bytes();
        let bytes = bands.len().checked_mul(w.width).and_then(|n| n.checked_mul(w.height)).and_then(|n| n.checked_mul(size));
        crate::dataset::fallible::check_usable(bytes, || {
            format!("a window of {}x{} pixels in {} band(s) is too large", w.width, w.height, bands.len())
        })
        .map_err(bad)?;
        let Some(&last) = self.bds.last() else { return self.read_group(w, bands) };
        let step = self.array.chunks[last].max(1);
        let mut unique: Vec<usize> = bands.to_vec();
        unique.sort_unstable();
        unique.dedup();
        // Bands read together: the same index along the outer band dimensions, and near along
        // the last.
        let mut groups: Vec<Vec<usize>> = Vec::new();
        for b in unique {
            let i = self.band_index(b);
            match groups.last_mut() {
                Some(g) => {
                    let j = self.band_index(g[g.len() - 1]);
                    if i[..i.len() - 1] == j[..j.len() - 1] && i[i.len() - 1] - j[j.len() - 1] < step {
                        g.push(b);
                    } else {
                        groups.push(vec![b]);
                    }
                }
                None => groups.push(vec![b]),
            }
        }
        if groups.len() == 1 && groups[0] == bands {
            return self.read_group(w, bands);
        }
        // In parallel unless their chunks are decoded a few at a time ([`decoding_at_once`]).
        let read = |i: usize| Ok((groups[i].clone(), self.read_group(w, &groups[i])?));
        let parts = if decoding_at_once(decoded_bytes(&self.array)) < crate::par::threads() {
            (0..groups.len()).map(read).collect::<Result<Vec<_>>>()?
        } else {
            crate::par::map_range(groups.len(), read).into_iter().collect::<Result<Vec<_>>>()?
        };
        let n = w.width * w.height;
        let find =
            |b: usize| parts.iter().enumerate().find_map(|(p, (g, _))| g.iter().position(|x| *x == b).map(|i| (p, i))).unwrap_or((0, 0));
        macro_rules! pick {
            ($($v:ident),*) => {
                match &parts[0].1 {
                    $(RasterData::$v(_) => {
                        let mut out = Vec::new();
                        crate::dataset::fallible::reserve(&mut out, bands.len() * n, "a Zarr window")?;
                        for &b in bands {
                            let (p, i) = find(b);
                            if let RasterData::$v(v) = &parts[p].1 {
                                out.extend_from_slice(&v[i * n..(i + 1) * n]);
                            }
                        }
                        RasterData::$v(out)
                    })*
                }
            };
        }
        Ok(pick!(U8, I8, U16, I16, U32, I32, U64, I64, F32, F64))
    }

    /// Reads a window of some bands, those between the first and the last (in band order) read
    /// together.
    fn read_group(&self, w: crate::Window, bands: &[usize]) -> Result<RasterData> {
        let a = &self.array;
        let nd = a.shape.len();
        let (_, rows) = self.size();
        let mut start = vec![0usize; nd];
        let mut shape = vec![1usize; nd];
        start[self.xd] = w.x;
        shape[self.xd] = w.width;
        start[self.yd] = if self.south_up { rows - w.y - w.height } else { w.y };
        shape[self.yd] = w.height;
        // The box of band indices the bands span.
        let indices: Vec<Vec<usize>> = bands.iter().map(|&b| self.band_index(b.max(1))).collect();
        for (k, &d) in self.bds.iter().enumerate() {
            let lo = indices.iter().map(|i| i[k]).min().unwrap_or(0);
            let hi = indices.iter().map(|i| i[k]).max().unwrap_or(0);
            start[d] = lo;
            shape[d] = hi - lo + 1;
        }
        let bytes = read_region(a, &start, &shape)?;
        // Rearranged band-major, north-up, from the region's little-endian elements.
        let size = a.data_type.size_bytes();
        let strides: Vec<usize> = (0..nd).map(|d| shape[d + 1..].iter().product()).collect();
        let (xs, ys) = (strides[self.xd], strides[self.yd]);
        // The offset of each band's values in the region read.
        let band_offsets: Vec<usize> =
            indices.iter().map(|i| self.bds.iter().enumerate().map(|(k, &d)| (i[k] - start[d]) * strides[d]).sum()).collect();
        let index = |offset: usize, r: usize, c: usize| {
            let rr = if self.south_up { w.height - 1 - r } else { r };
            (c * xs + rr * ys + offset) * size
        };
        // Checked by `Level::read`.
        let len = bands.len() * w.width * w.height;
        macro_rules! reorder {
            ($t:ty, $variant:ident) => {{
                let mut out: Vec<$t> = Vec::new();
                crate::dataset::fallible::reserve(&mut out, len, "a Zarr window")?;
                for &offset in &band_offsets {
                    for r in 0..w.height {
                        for c in 0..w.width {
                            let i = index(offset, r, c);
                            out.push(<$t>::from_le_bytes(bytes[i..i + size].try_into().unwrap_or_default()));
                        }
                    }
                }
                RasterData::$variant(out)
            }};
        }
        Ok(match a.data_type {
            DataType::U8 => reorder!(u8, U8),
            DataType::I8 => reorder!(i8, I8),
            DataType::U16 => reorder!(u16, U16),
            DataType::I16 => reorder!(i16, I16),
            DataType::U32 => reorder!(u32, U32),
            DataType::I32 => reorder!(i32, I32),
            DataType::U64 => reorder!(u64, U64),
            DataType::I64 => reorder!(i64, I64),
            DataType::F32 => reorder!(f32, F32),
            DataType::F64 => reorder!(f64, F64),
        })
    }

    /// Reads the pixels at columns `cols` of rows `rows` (north-up; each in any order, repeated or
    /// not) of some bands, `(bands, rows, cols)` band-major, as GDAL's nearest-neighbour
    /// decimation reads them: picked from the chunks that hold them, each decoded once
    /// ([`read_points`]), no buffer beside the output larger than a chunk. A decimated read of an
    /// array of billions of columns reads the few chunks it samples, and one of a 1 GiB chunk holds
    /// that chunk once, not the window spanning the pixels.
    fn read_sampled(&self, cols: &[usize], rows: &[usize], bands: &[usize]) -> Result<RasterData> {
        let a = &self.array;
        let nd = a.shape.len();
        let (width, height) = self.size();
        if cols.iter().any(|&c| c >= width) || rows.iter().any(|&r| r >= height) {
            return Err(bad(format!("a pixel outside the {width}x{height} array")));
        }
        let size = a.data_type.size_bytes();
        let (nr, nc) = (rows.len(), cols.len());
        let len = bands.len().checked_mul(nr).and_then(|n| n.checked_mul(nc));
        let bytes = crate::dataset::fallible::check_usable(len.and_then(|n| n.checked_mul(size)), || {
            format!("a window of {nc}x{nr} pixels in {} band(s) is too large", bands.len())
        })
        .map_err(bad)?;
        // The bands' indices along the band dimensions, all read at once, so that a chunk holding
        // several bands is decoded once; rows stored south first are their stored rows.
        let indices: Vec<Vec<usize>> = bands.iter().map(|&b| self.band_index(b.max(1))).collect();
        let sel: Vec<Vec<usize>> = (0..nd)
            .map(|d| match self.bds.iter().position(|&bd| bd == d) {
                Some(k) => {
                    let mut along: Vec<usize> = indices.iter().map(|i| i[k]).collect();
                    along.sort_unstable();
                    along.dedup();
                    along
                }
                None if d == self.xd => cols.to_vec(),
                None => rows.iter().map(|&r| if self.south_up { height - 1 - r } else { r }).collect(),
            })
            .collect();
        let values = read_points(a, &sel)?;
        // In C order over the selection.
        let strides: Vec<usize> = (0..nd).map(|d| sel[d + 1..].iter().map(Vec::len).product()).collect();
        let (xs, ys) = (strides[self.xd], strides[self.yd]);
        let mut picked = buffer(bytes, "a Zarr window")?;
        for index in &indices {
            let band: usize = self.bds.iter().enumerate().map(|(k, &d)| sel[d].binary_search(&index[k]).unwrap_or(0) * strides[d]).sum();
            for r in 0..nr {
                for c in 0..nc {
                    let i = (band + r * ys + c * xs) * size;
                    picked.extend_from_slice(&values[i..i + size]);
                }
            }
        }
        macro_rules! typed {
            ($t:ty, $variant:ident) => {{
                let mut out: Vec<$t> = Vec::new();
                crate::dataset::fallible::reserve(&mut out, picked.len() / size, "a Zarr window")?;
                out.extend(picked.chunks_exact(size).map(|e| <$t>::from_le_bytes(e.try_into().unwrap_or_default())));
                RasterData::$variant(out)
            }};
        }
        Ok(match a.data_type {
            DataType::U8 => RasterData::U8(picked),
            DataType::I8 => typed!(i8, I8),
            DataType::U16 => typed!(u16, U16),
            DataType::I16 => typed!(i16, I16),
            DataType::U32 => typed!(u32, U32),
            DataType::I32 => typed!(i32, I32),
            DataType::U64 => typed!(u64, U64),
            DataType::I64 => typed!(i64, I64),
            DataType::F32 => typed!(f32, F32),
            DataType::F64 => typed!(f64, F64),
        })
    }
}

/// A Zarr raster read by window: its full-resolution array and the lower-resolution levels of
/// its multiscales layout.
struct ZarrSource {
    levels: Vec<Level>,
}

impl crate::cog::WindowSource for ZarrSource {
    fn overviews(&self) -> Vec<(usize, usize)> {
        self.levels[1..].iter().map(Level::size).collect()
    }

    fn read(&mut self, level: usize, window: crate::Window, bands: &[usize]) -> Result<RasterData> {
        let l = self.levels.get(level).ok_or_else(|| bad(format!("no overview {}", level.saturating_sub(1))))?;
        let (w, h) = l.size();
        if !window.fits(w, h) {
            return Err(bad(format!("window outside the {w}x{h} level")));
        }
        l.read(window, bands)
    }

    fn read_sampled(&mut self, level: usize, cols: &[usize], rows: &[usize], bands: &[usize]) -> Result<Option<RasterData>> {
        let l = self.levels.get(level).ok_or_else(|| bad(format!("no overview {}", level.saturating_sub(1))))?;
        l.read_sampled(cols, rows, bands).map(Some)
    }
}

/// Opens the raster of a store, group or array node: its header (without pixels) and reader.
fn open_node(node: &Node, list_subdatasets: bool) -> Result<(RasterDataset, ZarrSource)> {
    let (array, group, ms) = locate(node)?;
    let nd = array.shape.len();
    if nd < 2 {
        return Err(Error::Unsupported(format!("Zarr arrays of {nd} dimensions are not supported as rasters")));
    }
    // GeoZarr attributes: the array's, its group's, its multiscales level's and the multiscales
    // group's.
    let group_attrs_v = group.as_ref().map(group_attrs).unwrap_or(json!({}));
    let name = array.node.name();
    let level_item = ms.as_ref().and_then(|m| m.layout.iter().find(|l| layout_array(m, l, Some(&name)).as_ref() == Some(&array.node)));
    let ms_attrs = ms.as_ref().map(|m| group_attrs(&m.group)).unwrap_or(json!({}));
    let null = Value::Null;
    let attr_sets: Vec<&Value> = vec![&array.attrs, &group_attrs_v, level_item.unwrap_or(&null), &ms_attrs];
    let dim_size = |name: &str| array.dims.iter().position(|d| d == name).map(|i| array.shape[i]);
    let geo = geozarr(&attr_sets, |y, x| Some((dim_size(y)?, dim_size(x)?)));
    let (xd, yd) = axes(&array, &attr_sets);
    let bds: Vec<usize> = (0..nd).filter(|d| *d != xd && *d != yd).collect();
    let (cols, rows) = (array.shape[xd], array.shape[yd]);
    // At most 65536 bands, as GDAL 3.12.1 exposes an array as a raster
    // (`GDALMDArray::AsClassicDataset`): a store declaring billions would have terrio describe
    // each.
    let mut bands = 1usize;
    for &d in &bds {
        if array.shape[d] > MAX_BANDS / bands.max(1) {
            return Err(bad("Too many bands. Operate on a sliced view"));
        }
        bands *= array.shape[d];
    }
    if bands == 0 {
        return Err(bad(format!("{}: an empty dimension", array.node.display())));
    }

    // Georeferencing: a grid mapping variable (xarray), or X/Y coordinate arrays (GDAL).
    let mut gt = geo.gt;
    let mut crs = geo.crs;
    // A coordinate or grid mapping array: in the array's group or, as GDAL looks for them, in a
    // group above.
    let sibling = |name: &str| {
        let mut g = group.clone();
        while let Some(n) = g {
            if let Ok(a) = read_array(&n.join(name)) {
                return Some(a);
            }
            g = n.parent();
        }
        None
    };
    if let Some(gm) = array.attrs["grid_mapping"].as_str().and_then(sibling) {
        if crs.is_none() {
            // Its WKT, or its CF parameters (as GDAL reads them).
            let numbers = |k: &str| match &gm.attrs[k] {
                Value::Number(n) => n.as_f64().map(|v| vec![v]),
                Value::Array(a) => Some(a.iter().filter_map(Value::as_f64).collect()),
                _ => None,
            };
            crs = super::cf::cf_crs(|k| gm.attrs[k].as_str().map(str::to_string), numbers).map(|c| match &c.proj4 {
                // CF parameters: a PROJ string, parsed by terrio (not matched to an EPSG code).
                Some(p) => crate::crs::SpatialRef::parse(p).map(|s| s.to_crs_info()).unwrap_or(c),
                None => c,
            });
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
            (a.shape == [array.shape[d]]).then(|| read_all_f64(&a)).flatten()
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
    if crs.is_none()
        && array.key_prefix.starts_with('c')
        && let Some(g) = &group
    {
        // GDAL's Zarr v3 writer keeps the CRS in a sidecar inside the store.
        crs = g.get("pam.aux.xml", Range::Whole).ok().flatten().and_then(|b| common::parse_pam(&String::from_utf8_lossy(&b)).crs);
    }
    let south_up = gt.is_some_and(|g| g[5] > 0.0);
    let gt = gt.unwrap_or(common::NO_GEOTRANSFORM);
    if gt[2] != 0.0 || gt[4] != 0.0 {
        return Err(Error::Unsupported("rotated geotransforms are not supported".into()));
    }
    let raster = Raster {
        cols,
        rows,
        bands,
        x_min: gt[0],
        y_min: if south_up { gt[3] } else { gt[3] + gt[5] * rows as f64 },
        cell_size_x: gt[1],
        cell_size_y: gt[5].abs(),
        nodata: array.fill.unwrap_or(-9999.0),
        data_type: array.data_type,
        crs: crs.unwrap_or_default(),
        metadata: band_dim_metadata(&array, &bds, |d| sibling(array.dims.get(d)?)),
        data: RasterData::new_filled(array.data_type, 0, 0.0),
    };
    let mut header = RasterDataset::with_driver(raster, RasterDriver::Zarr, array.fill.is_some());
    header.band_metadata = (0..bands)
        .map(|i| {
            let colors = &array.attrs["COLOR_INTERPRETATION"];
            let color = if colors.is_string() { colors.as_str() } else { colors.get(i).and_then(Value::as_str) };
            crate::BandMetadata { color_interp: Some(color.unwrap_or("Undefined").into()), ..Default::default() }
        })
        .collect();
    // A group opened: its arrays, as GDAL lists them (for its header: not to read its pixels).
    if list_subdatasets && array.node != *node {
        header.raster.metadata.extend(subdatasets(node));
    }

    // The lower-resolution levels of the multiscales layout, largest first.
    let mut levels: Vec<Level> = Vec::new();
    if let Some(ms) = &ms {
        for item in &ms.layout {
            let Some(n) = layout_array(ms, item, Some(&name)).or_else(|| layout_array(ms, item, None)) else { continue };
            if n == array.node || levels.iter().any(|l| l.array.node == n) {
                continue;
            }
            let Ok(a) = read_array(&n) else { continue };
            if a.shape.len() != nd || a.data_type != array.data_type {
                continue;
            }
            let (w, h) = (a.shape[xd], a.shape[yd]);
            if w <= cols && h <= rows && (w, h) != (cols, rows) && w > 0 && h > 0 && bds.iter().all(|&d| a.shape[d] == array.shape[d]) {
                levels.push(Level { array: a, xd, yd, bds: bds.clone(), south_up });
            }
        }
    }
    levels.sort_by_key(|l| std::cmp::Reverse(l.size().0 * l.size().1));
    levels.insert(0, Level { array, xd, yd, bds, south_up });
    Ok((header, ZarrSource { levels }))
}

/// The index along each of dimensions of sizes `sizes` of a band (numbered from 1), the last
/// dimension varying fastest.
fn band_index(sizes: &[usize], band: usize) -> Vec<usize> {
    let mut rest = band.saturating_sub(1);
    let mut out = vec![0; sizes.len()];
    for (k, &n) in sizes.iter().enumerate().rev() {
        out[k] = rest % n.max(1);
        rest /= n.max(1);
    }
    out
}

/// The most bands whose dimension indices and values are listed.
const MAX_BAND_DIM_METADATA: usize = 65536;

/// The most bands of an array read as a raster, GDAL's.
const MAX_BANDS: usize = 65536;

/// The position of each band in the dimensions of an array of more than three dimensions, as
/// GDAL's band metadata `DIM_<name>_INDEX`, `DIM_<name>_VALUE` and `DIM_<name>_UNIT` (the
/// coordinate and its unit, when the dimension has a coordinate array, found by `coord`), as
/// `BAND_<n>_DIM_<name>_...` items, and the dimensions as `DIM_EXTRA`.
fn band_dim_metadata(array: &Array, bds: &[usize], coord: impl Fn(usize) -> Option<Array>) -> Vec<(String, String)> {
    let sizes: Vec<usize> = bds.iter().map(|&d| array.shape[d]).collect();
    let bands: usize = sizes.iter().product();
    if bds.len() < 2 || bands > MAX_BAND_DIM_METADATA {
        return Vec::new();
    }
    let names: Vec<String> =
        bds.iter().map(|&d| array.dims.get(d).filter(|n| !n.is_empty()).cloned().unwrap_or_else(|| format!("dim{d}"))).collect();
    let coords: Vec<Option<Array>> = bds.iter().map(|&d| coord(d).filter(|a| a.shape == [array.shape[d]])).collect();
    let values: Vec<Option<Vec<f64>>> = coords.iter().map(|a| a.as_ref().and_then(read_all_f64)).collect();
    let units: Vec<Option<String>> =
        coords.iter().map(|a| a.as_ref().and_then(|a| a.attrs["units"].as_str().map(str::to_string))).collect();
    let mut out = vec![("DIM_EXTRA".to_string(), format!("{{{}}}", names.join(",")))];
    for b in 1..=bands {
        for (k, i) in band_index(&sizes, b).into_iter().enumerate() {
            out.push((format!("BAND_{b}_DIM_{}_INDEX", names[k]), i.to_string()));
            if let Some(v) = values[k].as_ref().and_then(|v| v.get(i)) {
                out.push((format!("BAND_{b}_DIM_{}_VALUE", names[k]), v.to_string()));
                if let Some(u) = &units[k] {
                    out.push((format!("BAND_{b}_DIM_{}_UNIT", names[k]), u.clone()));
                }
            }
        }
    }
    out
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

/// Reads a local Zarr store or array as a raster.
pub(crate) fn read(path: &Path) -> Result<RasterDataset> {
    read_node(&Node::local(path))
}

fn read_node(node: &Node) -> Result<RasterDataset> {
    let (mut header, mut source) = open_node(node, false)?;
    let r = &header.raster;
    let bands: Vec<usize> = (1..=r.bands).collect();
    header.raster.data = crate::cog::WindowSource::read(&mut source, 0, crate::Window::full(r.cols, r.rows), &bands)?;
    Ok(header)
}

/// The store and node a dataset name designates: GDAL's `ZARR:"store"` and
/// `ZARR:"store":/path/to/array` names, a local directory, or a remote location (HTTP(S),
/// `s3://`, `gs://`, `az://` or GDAL's `/vsi` names) with a path component ending in `.zarr`.
/// `None` when the name is none of these.
pub(crate) fn parse_name(name: &str) -> Option<(String, Option<String>)> {
    let t = name.trim();
    if t.len() > 5 && t[..5].eq_ignore_ascii_case("ZARR:") {
        let rest = &t[5..];
        if let Some(q) = rest.strip_prefix('"') {
            let (store, after) = q.split_once('"')?;
            let array = after.strip_prefix(':').map(str::to_string).filter(|a| !a.is_empty());
            return Some((store.to_string(), array));
        }
        return Some((rest.to_string(), None));
    }
    let loc = crate::vfs::Location::parse(t);
    match &loc {
        crate::vfs::Location::Local(p) => is_zarr(p).then(|| (t.to_string(), None)),
        crate::vfs::Location::Url(u) => {
            let path = u.split(['?', '#']).next().unwrap_or(u);
            path.split('/').any(|s| s.to_ascii_lowercase().ends_with(".zarr")).then(|| (t.to_string(), None))
        }
        crate::vfs::Location::S3 { key, .. } => {
            key.split('/').any(|s| s.to_ascii_lowercase().ends_with(".zarr")).then(|| (t.to_string(), None))
        }
        crate::vfs::Location::Zip { .. } => None,
    }
}

/// Opens the node a dataset name designates (see [`parse_name`]).
fn open_name(store: &str, array: Option<&str>) -> Result<Node> {
    let loc = crate::vfs::Location::parse(store);
    let node = match &loc {
        crate::vfs::Location::Local(p) => {
            if !is_zarr(p) {
                return Err(bad(format!("{} is not a Zarr store", p.display())));
            }
            Node::local(p)
        }
        // Named as given (`az://`, `s3://`...), so that its subdatasets' names open the same way.
        #[cfg(remote)]
        crate::vfs::Location::Url(_) | crate::vfs::Location::S3 { .. } => Node::remote_named(&loc, store)?,
        _ => return Err(Error::Unsupported(format!("Zarr stores at '{store}' are not supported"))),
    };
    Ok(match array {
        Some(a) => node.join(a.trim_start_matches('/')),
        None => node,
    })
}

/// Opens a Zarr store, group or array read by window, chunks read when a window needs them:
/// local, remote (read in place), or named as GDAL names them. `None` when `name` is not a Zarr
/// dataset name.
pub(crate) fn open_lazy(name: &str) -> Option<Result<crate::LazyRaster>> {
    let (store, array) = parse_name(name)?;
    // A local store is found by its layout whatever its name: read only if Zarr is allowed.
    Some(super::check_recognised(RasterDriver::Zarr, name).and_then(|()| open_name(&store, array.as_deref())).and_then(|node| {
        let (header, source) = open_node(&node, true)?;
        Ok(crate::LazyRaster::from_source(header, Box::new(source)))
    }))
}

/// Reads a Zarr dataset whole, local or remote (see [`open_lazy`]). `None` when `name` is not a
/// Zarr dataset name.
pub(crate) fn open_named(name: &str) -> Option<Result<RasterDataset>> {
    let (store, array) = parse_name(name)?;
    Some(
        super::check_recognised(RasterDriver::Zarr, name)
            .and_then(|()| open_name(&store, array.as_deref()))
            .and_then(|node| read_node(&node)),
    )
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
    let results = crate::par::map_range_coarse(grid.iter().product::<usize>(), |k| -> Result<()> {
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
    if let Some(code) = s.authority_code() {
        attrs["proj:code"] = json!(code);
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
    band_metadata: &[crate::BandMetadata],
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
    let colors: Vec<_> = (0..r.bands)
        .map(|i| band_metadata.get(i).and_then(|b| b.color_interp.as_deref()).unwrap_or(if r.bands == 1 { "Gray" } else { "Undefined" }))
        .collect();
    let mut attrs = json!({"COLOR_INTERPRETATION": if r.bands == 1 { json!(colors[0]) } else { json!(colors) }});
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
    write_with(r, nodata, path, compression, &co, &[])
}

/// Writes a raster as a Zarr group (see the module's documentation), with GDAL's creation options
/// `FORMAT` (`ZARR_V2`, the default, or `ZARR_V3`) and `GEOREFERENCING_CONVENTION` (`GDAL`,
/// `SPATIAL_PROJ` or `CF`), and terrio's `OVERVIEW_LEVELS` and `OVERVIEW_RESAMPLING`.
pub(crate) fn write_with(
    r: &Raster,
    nodata: Option<f64>,
    path: &Path,
    compression: Option<&str>,
    co: &[(String, String)],
    band_metadata: &[crate::BandMetadata],
) -> Result<()> {
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
        let full = RasterDataset::with_driver(r.clone(), RasterDriver::Zarr, nodata.is_some());
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
    write_level(path, &name, r, nodata, compression, band_metadata, version, conv, &mut meta, "")?;
    for (f, l) in &levels {
        let group = format!("ovr_{f}x");
        let dir = path.join(&group);
        write_group(&dir, &json!({}), version, &mut meta, &format!("{group}/"))?;
        write_level(&dir, &name, &l.raster, nodata, compression, band_metadata, version, conv, &mut meta, &format!("{group}/"))?;
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
    fn numbers_bands_of_many_dimensions_as_gdal() {
        // (time 2, level 3): band 1 is (0, 0), band 2 (0, 1), band 4 (1, 0).
        assert_eq!(band_index(&[2, 3], 1), vec![0, 0]);
        assert_eq!(band_index(&[2, 3], 2), vec![0, 1]);
        assert_eq!(band_index(&[2, 3], 4), vec![1, 0]);
        assert_eq!(band_index(&[2, 3], 6), vec![1, 2]);
        assert_eq!(band_index(&[2, 3, 4], 24), vec![1, 2, 3]);
        assert_eq!(band_index(&[5], 3), vec![2]);
        assert!(band_index(&[], 1).is_empty());
    }

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
        let out = bit_unshuffle(&shuffled, 16, 2, 2).unwrap();
        let back: Vec<u16> = out.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(back, elems);
    }

    #[test]
    fn preserves_explicit_colors_in_both_store_versions() {
        let r = common::make_raster(2, 2, 2, RasterData::U8(vec![1; 8]), common::NO_GEOTRANSFORM, CrsInfo::default(), None).unwrap();
        let colors = ["Alpha", "Undefined"].map(|s| crate::BandMetadata { color_interp: Some(s.into()), ..Default::default() });
        let dir = tempfile::tempdir().unwrap();
        for version in ["ZARR_V2", "ZARR_V3"] {
            let p = dir.path().join(format!("{version}.zarr"));
            write_with(&r, None, &p, None, &[("FORMAT".into(), version.into())], &colors).unwrap();
            let (header, _) = open_node(&Node::local(&p), false).unwrap();
            assert_eq!(header.band_color_interp(1), "Alpha");
            assert_eq!(header.band_color_interp(2), "Undefined");
        }
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
            let node = Node::local(&p);
            let array = read_array(&node.join(&format!("t{i}"))).unwrap();
            assert_eq!(array.attrs["COLOR_INTERPRETATION"], json!("Gray"));
            let ds = read(&p).unwrap();
            assert_eq!(ds.raster.data_f32().unwrap(), &data[..]);
            assert_eq!(ds.nodata(), Some(-1.0));
            assert_eq!(ds.crs().epsg, Some(4326));
            let gt = ds.geo_transform();
            assert!((gt[0] + 110.0).abs() < 1e-9 && (gt[3] - 40.0).abs() < 1e-9, "{gt:?}");
        }
    }

    /// A Zstandard stream of `blocks` RLE blocks of 128 KiB of zeros, built by hand (RFC 8878):
    /// `blocks` x 128 KiB in 4 bytes a block. For the Linux-only tests of memory.
    #[cfg(target_os = "linux")]
    fn zeros_zstd(blocks: usize) -> Vec<u8> {
        // The magic number, a frame header without content size (a 128 KiB window), the blocks.
        let mut out = vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x38];
        for k in 0..blocks {
            let header = (128 * 1024) << 3 | 1 << 1 | u32::from(k + 1 == blocks);
            out.extend_from_slice(&header.to_le_bytes()[..3]);
            out.push(0);
        }
        out
    }

    /// The process's peak resident memory, in bytes (`VmHWM`, which only Linux has).
    #[cfg(target_os = "linux")]
    fn peak_memory() -> usize {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let kb = status.lines().find_map(|l| l.strip_prefix("VmHWM:")).and_then(|v| v.trim().strip_suffix("kB")).unwrap();
        kb.trim().parse::<usize>().unwrap() * 1024
    }

    /// One honest chunk of 32768 x 32768 bytes (1 GiB, the most GDAL's rule allows; 32 KB stored)
    /// is read, by a 1 x 1 window and by a decimated read, holding the chunk once: the codec's
    /// output reserved at once and converted in place, the decimated read picking its pixels from
    /// the chunk rather than allocating the window spanning them. Peak memory under 1.2 GiB (1.05
    /// GiB measured), in a process of its own, as GDAL reads such a chunk in 1.03 GiB; it held the
    /// chunk twice, and at the decimated read a window of 1 GiB besides.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_chunk_of_1_gib_is_read_holding_it_once() {
        crate::par::alone(600, || {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("one.zarr");
            std::fs::create_dir_all(&path).unwrap();
            let zarray = r#"{"zarr_format": 2, "shape": [32768, 32768], "chunks": [32768, 32768], "dtype": "|u1",
                "compressor": {"id": "zstd", "level": 1}, "fill_value": 7, "order": "C", "filters": null}"#;
            std::fs::write(path.join(".zarray"), zarray).unwrap();
            std::fs::write(path.join("0.0"), zeros_zstd(8192)).unwrap();
            let mut r = crate::LazyRaster::open(path.to_str().unwrap()).unwrap();
            let one = r.read_window(crate::Window::new(32000, 5, 1, 1), &crate::WindowOptions::default()).unwrap();
            assert_eq!(one.raster.data.to_f64_vec(), vec![0.0], "the chunk's value, not the fill value");
            let picks: Vec<usize> = (0..64).map(|i| i * 512 + 256).collect();
            let decimated = r.read_sampled(&picks, &picks, &crate::WindowOptions::default()).unwrap().unwrap();
            assert_eq!(decimated.to_f64_vec(), vec![0.0; 64 * 64]);
            let peak = peak_memory();
            eprintln!("peak {peak} bytes");
            peak < (12 << 30) / 10
        });
    }

    /// Chunks are decoded in parallel only while their buffers fit in half the memory the process
    /// can use, and one at a time at the least, the memory read from a machine's files as the
    /// usable memory reads them (`/proc/meminfo`, the cgroup's `memory.max`, `/proc/self/limits`),
    /// given here: nothing is read from the machine running the test.
    #[test]
    fn chunks_are_decoded_in_parallel_only_while_they_fit_in_half_the_usable_memory() {
        // A machine of 32 GiB, the process in a cgroup limited to `group` ("max": none), under an
        // address-space limit of `space`.
        let machine = |group: &str, space: &str| {
            let files: std::collections::HashMap<&str, String> = [
                ("/proc/meminfo", "MemTotal:       33554432 kB\n".to_string()),
                ("/proc/self/cgroup", "0::/app.slice/run-1.scope\n".to_string()),
                ("/sys/fs/cgroup/app.slice/run-1.scope/memory.max", format!("{group}\n")),
                ("/sys/fs/cgroup/app.slice/memory.max", "max\n".to_string()),
                (
                    "/proc/self/limits",
                    format!(
                        "Limit                     Soft Limit           Hard Limit           Units     \n\
                         Max address space         {space:<21}unlimited            bytes     \n\
                         Max resident set          unlimited            unlimited            bytes     \n"
                    ),
                ),
            ]
            .into();
            // The groups' paths as the reader joins them, a slash doubled.
            crate::dataset::fallible::usable_memory_from(&|path: &str| files.get(path.replace("//", "/").as_str()).cloned())
        };
        let gib = 1usize << 30;
        let in_2_gib = machine("2147483648", "unlimited");
        assert_eq!(in_2_gib, Some(2 << 30), "the cgroup's limit");
        // Chunks of 1 GiB, the most GDAL's rule allows: one at a time in 2 GiB, as GDAL decodes
        // them, two in 4 GiB, the whole pool of 16 threads (or 8) on the machine's 32 GiB.
        assert_eq!(at_once(gib, 16, in_2_gib), 1);
        assert_eq!(at_once(gib, 16, machine("4294967296", "unlimited")), 2);
        assert_eq!(at_once(gib, 16, machine("max", "unlimited")), 16);
        assert_eq!(at_once(gib, 8, machine("max", "unlimited")), 8);
        // An address-space limit counts too: 3 GiB holds one.
        assert_eq!(at_once(gib, 16, machine("max", "3221225472")), 1);
        // Chunks of 256 MiB in 2 GiB: four, a half of it; past half of it, still one.
        assert_eq!(at_once(256 << 20, 16, in_2_gib), 4);
        assert_eq!(at_once(3 * gib / 2, 16, in_2_gib), 1);
        // Chunks of 16 MiB or less, or the memory unknown: the whole pool.
        assert_eq!(at_once(16 << 20, 16, Some(16 << 20)), 16);
        assert_eq!(at_once((16 << 20) + 1, 16, Some(16 << 20)), 1);
        assert_eq!(at_once(gib, 16, None), 16);
    }

    /// The value of the element at `idx` (band, row, column, or row and column) of the arrays of
    /// [`chunks_stored_transposed_are_read_in_place`]: a different one for each element, and both
    /// of its bytes differ between neighbours.
    fn value_at(idx: &[usize]) -> u16 {
        let (b, r, c) = if let [b, r, c] = idx { (*b, *r, *c) } else { (0, idx[0], idx[1]) };
        (b * 1000 + r * 40 + c + 1) as u16 * 3
    }

    /// The bytes of the chunk at grid position `grid` of an array of `shape` in chunks of `chunks`
    /// whose elements are UInt16 ([`value_at`], 0 past the array's edge), `big`-endian or not,
    /// stored with the array's dimension `perm[i]` as stored dimension `i`, in C order over those.
    fn stored_chunk(shape: &[usize], chunks: &[usize], grid: &[usize], perm: &[usize], big: bool) -> Vec<u8> {
        let nd = shape.len();
        let n: usize = chunks.iter().product();
        let mut out = Vec::with_capacity(2 * n);
        let mut sidx = vec![0usize; nd];
        for _ in 0..n {
            let mut idx = vec![0usize; nd];
            for (i, &d) in perm.iter().enumerate() {
                idx[d] = grid[d] * chunks[d] + sidx[i];
            }
            let v = if idx.iter().zip(shape).all(|(i, s)| i < s) { value_at(&idx) } else { 0 };
            out.extend_from_slice(&if big { v.to_be_bytes() } else { v.to_le_bytes() });
            for i in (0..nd).rev() {
                sidx[i] += 1;
                if sidx[i] < chunks[perm[i]] {
                    break;
                }
                sidx[i] = 0;
            }
        }
        out
    }

    /// Chunks stored in Fortran order (v2, little- and big-endian) and transposed (v3: by the
    /// chunk, in shards read by parts and in shards decoded whole), which are read in place rather
    /// than copied into C order, give their elements where they are, across chunks and at the
    /// arrays' edges: read whole, by a window and by a decimated read.
    #[test]
    fn chunks_stored_transposed_are_read_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let (shape, rows, cols) = ([2usize, 9, 11], 9, 11);
        let grid_of = |chunks: &[usize]| -> Vec<Vec<usize>> {
            let counts: Vec<usize> = shape.iter().zip(chunks).map(|(s, c)| s.div_ceil(*c)).collect();
            (0..counts.iter().product()).map(|j| unravel(j, &counts)).collect()
        };
        let mut stores = Vec::new();
        // Zarr v2 in Fortran order: two bands, and one.
        for (name, dtype, big) in [("f_le", "<u2", false), ("f_be", ">u2", true)] {
            let p = dir.path().join(format!("{name}.zarr"));
            std::fs::create_dir_all(&p).unwrap();
            let zarray = format!(
                r#"{{"zarr_format": 2, "shape": [2, 9, 11], "chunks": [2, 4, 5], "dtype": "{dtype}",
                "compressor": null, "fill_value": 0, "order": "F", "filters": null}}"#
            );
            std::fs::write(p.join(".zarray"), zarray).unwrap();
            for g in grid_of(&[2, 4, 5]) {
                let key = g.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(".");
                std::fs::write(p.join(key), stored_chunk(&shape, &[2, 4, 5], &g, &[2, 1, 0], big)).unwrap();
            }
            stores.push((p, 2));
        }
        let p = dir.path().join("f_2d.zarr");
        std::fs::create_dir_all(&p).unwrap();
        let zarray = r#"{"zarr_format": 2, "shape": [9, 11], "chunks": [4, 5], "dtype": "<u2",
            "compressor": {"id": "zlib", "level": 1}, "fill_value": 0, "order": "F", "filters": null}"#;
        std::fs::write(p.join(".zarray"), zarray).unwrap();
        for g in grid_of(&[1, 4, 5]).into_iter().filter(|g| g[0] == 0) {
            let chunk = stored_chunk(&[9, 11], &[4, 5], &g[1..], &[1, 0], false);
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            std::io::Write::write_all(&mut z, &chunk).unwrap();
            std::fs::write(p.join(format!("{}.{}", g[1], g[2])), z.finish().unwrap()).unwrap();
        }
        stores.push((p, 1));
        // Zarr v3 transposed: by the chunk; in shards of 2 x 8 x 10, inner chunks of 1 x 4 x 5,
        // read by parts, or decoded whole under a CRC32C checksum.
        let bytes = r#"{"name": "bytes", "configuration": {"endian": "little"}}"#;
        let transpose = |order: &[usize]| format!(r#"{{"name": "transpose", "configuration": {{"order": {order:?}}}}}"#);
        let v3 = |name: &str, chunks: [usize; 3], codecs: String| {
            let p = dir.path().join(format!("{name}.zarr"));
            std::fs::create_dir_all(&p).unwrap();
            let zarr = format!(
                r#"{{"zarr_format": 3, "node_type": "array", "shape": [2, 9, 11], "data_type": "uint16",
                "chunk_grid": {{"name": "regular", "configuration": {{"chunk_shape": {chunks:?}}}}},
                "chunk_key_encoding": {{"name": "default", "configuration": {{"separator": "/"}}}}, "fill_value": 0,
                "codecs": [{codecs}]}}"#
            );
            std::fs::write(p.join("zarr.json"), zarr).unwrap();
            p
        };
        let p = v3("t", [2, 4, 5], format!("{}, {bytes}", transpose(&[1, 2, 0])));
        for g in grid_of(&[2, 4, 5]) {
            std::fs::create_dir_all(p.join(format!("c/{}/{}", g[0], g[1]))).unwrap();
            std::fs::write(p.join(format!("c/{}/{}/{}", g[0], g[1], g[2])), stored_chunk(&shape, &[2, 4, 5], &g, &[1, 2, 0], false))
                .unwrap();
        }
        stores.push((p, 2));
        for (name, whole) in [("s_parts", false), ("s_whole", true)] {
            let inner = format!("{}, {bytes}", transpose(&[2, 0, 1]));
            let checksum = if whole { r#", {"name": "crc32c"}"# } else { "" };
            let codecs = format!(
                r#"{{"name": "sharding_indexed", "configuration": {{"chunk_shape": [1, 4, 5], "codecs": [{inner}],
                "index_codecs": [{bytes}, {{"name": "crc32c"}}], "index_location": "end"}}}}{checksum}"#
            );
            let p = v3(name, [2, 8, 10], codecs);
            for g in grid_of(&[2, 8, 10]) {
                // The shard's inner chunks, then its index and the index's (unchecked) checksum.
                let (mut shard, mut index) = (Vec::new(), Vec::new());
                for j in 0..8 {
                    let ig = unravel(j, &[2, 2, 2]);
                    let at: Vec<usize> = (0..3).map(|d| g[d] * [2, 8, 10][d] / [1, 4, 5][d] + ig[d]).collect();
                    let chunk = stored_chunk(&shape, &[1, 4, 5], &at, &[2, 0, 1], false);
                    index.extend_from_slice(&(shard.len() as u64).to_le_bytes());
                    index.extend_from_slice(&(chunk.len() as u64).to_le_bytes());
                    shard.extend_from_slice(&chunk);
                }
                shard.extend_from_slice(&index);
                shard.extend_from_slice(&[0; 4]);
                if whole {
                    shard.extend_from_slice(&[0; 4]);
                }
                std::fs::create_dir_all(p.join(format!("c/{}/{}", g[0], g[1]))).unwrap();
                std::fs::write(p.join(format!("c/{}/{}/{}", g[0], g[1], g[2])), shard).unwrap();
            }
            stores.push((p, 2));
        }
        for (p, bands) in stores {
            let name = p.display().to_string();
            // The elements at rows `rs` and columns `cs` of each band, band by band.
            let picked = |rs: &[usize], cs: &[usize]| -> Vec<f64> {
                let mut out = Vec::new();
                for b in 0..bands {
                    for &r in rs {
                        out.extend(cs.iter().map(|&c| f64::from(value_at(&[b, r, c]))));
                    }
                }
                out
            };
            let all = read(&p).unwrap().raster.data.to_f64_vec();
            assert_eq!(all, picked(&(0..rows).collect::<Vec<_>>(), &(0..cols).collect::<Vec<_>>()), "{name}: read whole");
            let options = crate::WindowOptions { bands: (1..=bands).collect(), overview: None };
            let mut r = crate::LazyRaster::open(&name).unwrap();
            let window = r.read_window(crate::Window::new(3, 2, 6, 5), &options).unwrap();
            assert_eq!(window.raster.data.to_f64_vec(), picked(&[2, 3, 4, 5, 6], &[3, 4, 5, 6, 7, 8]), "{name}: a window");
            let (cs, rs) = ([10, 0, 4, 5, 4], [8, 3, 0]);
            let sampled = r.read_sampled(&cs, &rs, &options).unwrap().unwrap();
            assert_eq!(sampled.to_f64_vec(), picked(&rs, &cs), "{name}: a decimated read");
        }
    }

    /// A chunk of 32768 x 32768 bytes in Fortran order (1 GiB, read under the option, since GDAL's
    /// rule counts it twice) is read in place, by a 1 x 1 window and by a decimated read, holding
    /// the chunk once, as a chunk in C order is held: peak memory under 1.2 GiB, in a process of
    /// its own. It was copied into C order, a second 1 GiB, and the process was killed in a 2 GiB
    /// cgroup.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_chunk_of_1_gib_in_fortran_order_is_read_holding_it_once() {
        crate::par::alone(600, || {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("one.zarr");
            std::fs::create_dir_all(&path).unwrap();
            let zarray = r#"{"zarr_format": 2, "shape": [32768, 32768], "chunks": [32768, 32768], "dtype": "|u1",
                "compressor": {"id": "zstd", "level": 1}, "fill_value": 7, "order": "F", "filters": null}"#;
            std::fs::write(path.join(".zarray"), zarray).unwrap();
            std::fs::write(path.join("0.0"), zeros_zstd(8192)).unwrap();
            let mut r = crate::LazyRaster::open(path.to_str().unwrap()).unwrap();
            let window = crate::Window::new(32000, 5, 1, 1);
            let e = r.read_window(window, &crate::WindowOptions::default()).map(|_| ()).unwrap_err().to_string();
            assert!(e.contains("tile allocation would require 2147483648 bytes"), "{e}");
            crate::config::push_scope([("TERRIO_ZARR_ALLOW_BIG_TILE_SIZE".to_string(), Some("YES".to_string()))].into());
            let one = r.read_window(window, &crate::WindowOptions::default()).unwrap();
            assert_eq!(one.raster.data.to_f64_vec(), vec![0.0], "the chunk's value, not the fill value");
            let picks: Vec<usize> = (0..64).map(|i| i * 512 + 256).collect();
            let decimated = r.read_sampled(&picks, &picks, &crate::WindowOptions::default()).unwrap().unwrap();
            assert_eq!(decimated.to_f64_vec(), vec![0.0; 64 * 64]);
            crate::config::pop_scope();
            let peak = peak_memory();
            eprintln!("peak {peak} bytes");
            peak < (12 << 30) / 10
        });
    }

    /// A chunk whose decoding allocates more than the memory the process can use is refused before
    /// its buffer is reserved, whatever its order: where the system overcommits memory (a cgroup's
    /// limit, no address-space limit), the reservation succeeds and the process is killed as the
    /// decoder writes it. In a process of its own whose resident-set limit (which the usable memory
    /// counts, and Linux does not enforce) is 64 MiB: chunks of 128 MiB (64 KB of Zstandard)
    /// refused, one of 32 MiB read.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_chunk_decoding_to_more_than_the_usable_memory_is_refused() {
        crate::par::alone(120, || {
            let limit = libc::rlimit { rlim_cur: 64 << 20, rlim_max: libc::RLIM_INFINITY };
            // SAFETY: `limit` is a valid rlimit, read by the call.
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_RSS, &limit) }, 0);
            assert_eq!(crate::dataset::fallible::usable_memory(), Some(64 << 20));
            let dir = tempfile::tempdir().unwrap();
            let store = |name: &str, order: &str, rows: usize| {
                let path = dir.path().join(format!("{name}.zarr"));
                std::fs::create_dir_all(&path).unwrap();
                let zarray = format!(
                    r#"{{"zarr_format": 2, "shape": [{rows}, 16384], "chunks": [{rows}, 16384], "dtype": "|u1",
                    "compressor": {{"id": "zstd", "level": 1}}, "fill_value": 7, "order": "{order}", "filters": null}}"#
                );
                std::fs::write(path.join(".zarray"), zarray).unwrap();
                std::fs::write(path.join("0.0"), zeros_zstd(rows / 8)).unwrap();
                let mut r = crate::LazyRaster::open(path.to_str().unwrap()).unwrap();
                r.read_window(crate::Window::new(16000, 5, 1, 1), &crate::WindowOptions::default()).map(|d| d.raster.data.to_f64_vec())
            };
            for order in ["F", "C"] {
                let e = store(&format!("big-{order}"), order, 8192).unwrap_err().to_string();
                let want = "cannot allocate a Zarr chunk's decoded bytes: 134217793 bytes, more than the 67108864 bytes of memory \
                            the process can use";
                assert!(e.contains(want), "{order}: {e}");
                assert_eq!(store(&format!("small-{order}"), order, 2048).unwrap(), vec![0.0], "{order}");
            }
            true
        });
    }

    /// Sets this process's resident-set limit to `bytes`, its hard limit kept: the memory the
    /// process can use counts that limit, which Linux does not enforce, so nothing is exhausted.
    #[cfg(target_os = "linux")]
    fn limit_resident_set(bytes: u64) {
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: `limit` is a valid rlimit, written by the first call and read by the second.
        assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_RSS, &mut limit) }, 0);
        limit.rlim_cur = bytes;
        // SAFETY: as above.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_RSS, &limit) }, 0, "a resident-set limit of {bytes} bytes");
    }

    /// The readers decode chunks at once as the memory the process can use allows: they ask
    /// [`decoding_at_once`], which measures that memory and applies the rule to it. In a process of
    /// its own, of 4 threads, whose resident-set limit (which the usable memory counts, and Linux
    /// does not enforce) is set in turn: chunks of 64 MiB are decoded one at a time under 128 MiB,
    /// two at a time under 256 MiB, on the whole pool under 512 MiB; chunks of 16 MiB on the whole
    /// pool under each. Fed no measure of the memory (or any constant), the rule decoded stores of
    /// two and four 1 GiB chunks all at once in a 2 GiB cgroup, where the process was killed.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_chunks_decoded_at_once_follow_the_memory_the_process_can_use() {
        crate::par::alone(60, || {
            // SAFETY: in the test's own process, on its one test thread, before its pool starts.
            unsafe { std::env::set_var("RAYON_NUM_THREADS", "4") };
            assert_eq!(crate::par::threads(), 4);
            for (limit, want) in [(128u64 << 20, 1), (256 << 20, 2), (512 << 20, 4)] {
                limit_resident_set(limit);
                assert_eq!(crate::dataset::fallible::usable_memory(), Some(limit));
                assert_eq!(decoding_at_once(64 << 20), want, "chunks of 64 MiB under {limit} bytes");
                assert_eq!(decoding_at_once(16 << 20), 4, "chunks of 16 MiB under {limit} bytes");
            }
            true
        });
    }

    /// A chunk whose elements are converted to the array's type counts their converted copy, a
    /// buffer of its own beside the decoded bytes, against the memory the process can use. In a
    /// process of its own whose resident-set limit is 64 MiB: a chunk of 4096 x 4096 `float16`
    /// elements, read as Float32 (32 MiB decoded, 64 MiB converted), is refused; the same chunk of
    /// `uint16` elements, read as stored (32 MiB decoded), is read.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_converted_chunk_counts_its_converted_copy_against_the_usable_memory() {
        crate::par::alone(120, || {
            limit_resident_set(64 << 20);
            assert_eq!(crate::dataset::fallible::usable_memory(), Some(64 << 20));
            let dir = tempfile::tempdir().unwrap();
            let read = |dtype: &str| {
                let path = dir.path().join(format!("{}.zarr", &dtype[1..]));
                std::fs::create_dir_all(&path).unwrap();
                let zarray = format!(
                    r#"{{"zarr_format": 2, "shape": [4096, 4096], "chunks": [4096, 4096], "dtype": "{dtype}",
                    "compressor": {{"id": "zstd", "level": 1}}, "fill_value": 7, "order": "C", "filters": null}}"#
                );
                std::fs::write(path.join(".zarray"), zarray).unwrap();
                // 32 MiB of zeros: elements of 0.
                std::fs::write(path.join("0.0"), zeros_zstd(256)).unwrap();
                let mut r = crate::LazyRaster::open(path.to_str().unwrap()).unwrap();
                r.read_window(crate::Window::new(4000, 5, 1, 1), &crate::WindowOptions::default()).map(|d| d.raster.data.to_f64_vec())
            };
            assert_eq!(read("<u2").unwrap(), vec![0.0], "uint16");
            let e = read("<f2").unwrap_err().to_string();
            let want = "cannot allocate a Zarr chunk's decoded bytes: 100663361 bytes, more than the 67108864 bytes of memory the \
                        process can use";
            assert!(e.contains(want), "float16: {e}");
            true
        });
    }

    /// An array's `zarr_format` must be its file's version, as GDAL 3.12.1 requires it (rasterio
    /// 1.5.0 on a v2 store saying 65535: "Invalid value for zarr_format: 65535"; on a v3 store
    /// saying 2: "Unhandled zarr_format value"): other values, and none, are refused at open.
    #[test]
    fn zarr_format_is_the_version_the_file_holds() {
        let dir = tempfile::tempdir().unwrap();
        let v2 = |format: &str| {
            let p = dir.path().join(format!("v2-{}.zarr", format.replace('"', "s")));
            std::fs::create_dir_all(&p).unwrap();
            let zarray = format!(
                r#"{{{format} "shape": [2, 2], "chunks": [2, 2], "dtype": "|u1", "compressor": null, "fill_value": 7, "order": "C", "filters": null}}"#
            );
            std::fs::write(p.join(".zarray"), zarray).unwrap();
            read(&p).map(|ds| ds.raster.data.to_f64_vec())
        };
        assert_eq!(v2(r#""zarr_format": 2,"#).unwrap(), vec![7.0; 4]);
        assert_eq!(v2(r#""zarr_format": "2","#).unwrap(), vec![7.0; 4]);
        for (format, shown) in [(r#""zarr_format": 65535,"#, "65535"), (r#""zarr_format": 3,"#, "3"), ("", "")] {
            let e = v2(format).unwrap_err().to_string();
            assert_eq!(e, format!("Zarr: Invalid value for zarr_format: {shown}"), "{format}");
        }
        let v3 = |format: &str| {
            let p = dir.path().join(format!("v3-{}.zarr", format.replace('"', "s")));
            std::fs::create_dir_all(&p).unwrap();
            let zarr = format!(
                r#"{{{format} "node_type": "array", "shape": [2, 2], "data_type": "uint8",
                "chunk_grid": {{"name": "regular", "configuration": {{"chunk_shape": [2, 2]}}}},
                "chunk_key_encoding": {{"name": "default", "configuration": {{"separator": "/"}}}}, "fill_value": 7,
                "codecs": [{{"name": "bytes", "configuration": {{"endian": "little"}}}}]}}"#
            );
            std::fs::write(p.join("zarr.json"), zarr).unwrap();
            read(&p).map(|ds| ds.raster.data.to_f64_vec())
        };
        assert_eq!(v3(r#""zarr_format": 3,"#).unwrap(), vec![7.0; 4]);
        for format in [r#""zarr_format": 2,"#, r#""zarr_format": 65535,"#, ""] {
            assert_eq!(v3(format).unwrap_err().to_string(), "Zarr: Unhandled zarr_format value", "{format}");
        }
    }
}
