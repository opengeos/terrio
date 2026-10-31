//! Zarr decoding limits of the codecs besides Zstandard (`zstd_input_limits.rs`): a small zlib,
//! gzip, LZ4 or Blosc chunk (Blosc with each of its compressors: BloscLZ, LZ4, zlib and Zstandard)
//! expanding far beyond the size its array declares is an error past that size (with the slack
//! of Zstandard chunks), and a size the chunk itself states past it is refused before it is
//! allocated. Each stream below expands to about 4 GiB: run under a memory cap, a decoder without
//! the limit is stopped by it. And chunks as numcodecs writes them with each codec read as written.
//!
//! A chunk whose elements take more than 1 GiB is refused at read, stored or not, as GDAL 3.12.1
//! refuses it (`ZARR_ALLOW_BIG_TILE_SIZE`), and chunk dimensions whose product overflows are an
//! error ("Too large chunks").
#![cfg(feature = "zarr")]
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use terrio::{Window, WindowOptions};

/// The most bytes a size of LZ4's or Blosc's states (4 GiB less a byte).
const BIG: usize = u32::MAX as usize;
/// Matches of 258 bytes in about 4 GiB.
const REPEATS: usize = BIG / 258;
/// The stores below hold one 256 x 256 UInt8 chunk, 65536 elements of 1 byte: decoded no further
/// than those 65536 bytes and 64 bytes of slack, as Zstandard chunks.
const MOST: usize = 65_600;

/// Bits written least significant first, as Deflate packs them.
#[derive(Default)]
struct Bits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    /// `count` bits of `value`, least significant first.
    fn put(&mut self, value: u32, count: u32) {
        self.acc |= u64::from(value) << self.n;
        self.n += count;
        while self.n >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.n -= 8;
        }
    }

    /// A Huffman code of `len` bits, most significant bit first.
    fn code(&mut self, code: u32, len: u32) {
        for i in (0..len).rev() {
            self.put(code >> i & 1, 1);
        }
    }

    /// A run of `n` zero lengths (11 to 138): the code length code 18 and its 7 bits.
    fn zero_lengths(&mut self, n: u32) {
        self.code(0, 1);
        self.put(n - 11, 7);
    }
}

/// A raw Deflate stream decoding to `1 + 258 x repeats` zeros: one block of dynamic Huffman codes
/// holding a literal zero, `repeats` matches of 258 bytes at distance 1 (two bits each: the codes
/// of length 258 and of distance 1 take a bit each), then the end of the block.
fn deflate_bomb(repeats: usize) -> Vec<u8> {
    let mut b = Bits::default();
    // The last block, of dynamic codes: 286 literal/length codes, 2 distance codes, 18 code length
    // codes, whose lengths come in Deflate's order (16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3,
    // 13, 2, 14, 1): 18 (zero lengths) in 1 bit, 0 (coded 10) and 1 (coded 11) in 2 bits.
    b.put(1, 1);
    b.put(2, 2);
    b.put(286 - 257, 5);
    b.put(2 - 1, 5);
    b.put(18 - 4, 4);
    for len in [0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 2] {
        b.put(len, 3);
    }
    // The literal/length code lengths: 2 for the literal 0 and for 256 (the end of a block), 1 for
    // 285 (length 258); the distance code lengths: 1 for distances 1 and 2.
    b.code(0b11, 2);
    b.zero_lengths(138);
    b.zero_lengths(117);
    b.code(0b11, 2);
    b.zero_lengths(28);
    for _ in 0..3 {
        b.code(0b10, 2);
    }
    // Their codes: 285 is 0, the literal 0 is 10, 256 is 11; distance 1 is 0.
    b.code(0b10, 2);
    let bits = 2 * repeats;
    for _ in 0..bits / 32 {
        b.put(0, 32);
    }
    b.put(0, (bits % 32) as u32);
    b.code(0b11, 2);
    if b.n > 0 {
        b.out.push(b.acc as u8);
    }
    b.out
}

/// `deflate`, decoding to `n` zeros, as a zlib stream: its header, then the Adler-32 of the zeros
/// (1, and `n` modulo 65521 in the high half).
fn zlib(deflate: &[u8], n: usize) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    out.extend_from_slice(deflate);
    out.extend_from_slice(&((((n % 65521) as u32) << 16) | 1).to_be_bytes());
    out
}

/// `deflate`, decoding to `n` zeros, as a gzip member: its header, then the CRC-32 and the size of
/// the zeros.
fn gzip(deflate: &[u8], n: usize) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 255];
    out.extend_from_slice(deflate);
    out.extend_from_slice(&crc_of_zeros(n).to_le_bytes());
    out.extend_from_slice(&(n as u32).to_le_bytes());
    out
}

/// `n` zeros as a zlib stream, or a gzip member, written by flate2.
fn deflated(n: usize, gz: bool) -> Vec<u8> {
    let level = flate2::Compression::default();
    if gz {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), level);
        e.write_all(&vec![0; n]).unwrap();
        e.finish().unwrap()
    } else {
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), level);
        e.write_all(&vec![0; n]).unwrap();
        e.finish().unwrap()
    }
}

/// The CRC-32 of `n` zeros, combined from those of 1, 2, 4... zeros along the bits of `n`.
fn crc_of_zeros(mut n: usize) -> u32 {
    let mut crc = flate2::Crc::new();
    let mut power = flate2::Crc::new();
    power.update(&[0]);
    while n > 0 {
        if n & 1 == 1 {
            crc.combine(&power);
        }
        let mut twice = flate2::Crc::new();
        twice.combine(&power);
        twice.combine(&power);
        power = twice;
        n >>= 1;
    }
    crc.sum()
}

/// An LZ4 block decoding to `n` zeros (25 or more): a literal zero and a match of `n - 6` bytes at
/// distance 1 (its length past 19 in bytes of 255 and the rest), then 5 literal zeros, with which a
/// block ends.
fn lz4_bomb(n: usize) -> Vec<u8> {
    // A literal and a match of 15 + 4 bytes or more; the literal; the distance.
    let mut out = vec![0x1f, 0, 1, 0];
    let more = n - 25;
    out.extend(std::iter::repeat_n(255, more / 255));
    out.push((more % 255) as u8);
    out.push(0x50);
    out.extend([0; 5]);
    out
}

/// A BloscLZ stream decoding to `n` zeros (12 or more): a literal zero, a match of `n - 2` bytes at
/// distance 1 (its length past 9 in bytes of 255 and the rest), then a literal zero.
fn blosclz_bomb(n: usize) -> Vec<u8> {
    // A run of one literal; a match of 6 + 3 bytes or more.
    let mut out = vec![0, 0, 7 << 5];
    let more = n - 11;
    out.extend(std::iter::repeat_n(255, more / 255));
    out.push((more % 255) as u8);
    // Distance 1, then a run of one literal.
    out.extend([0, 0, 0]);
    out
}

/// A Zstandard frame of `blocks` RLE blocks of 128 KiB of zeros: 6 + 4 x `blocks` bytes (as
/// `zstd_input_limits.rs`).
fn zstd_bomb(blocks: usize) -> Vec<u8> {
    // The magic number, no content size, a window of 2^(10 + 7) bytes.
    let mut out = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, 7 << 3];
    for i in 0..blocks {
        // The size of an RLE block, its type (1) and the last-block flag.
        let header = ((128u32 << 10) << 3) | (1 << 1) | u32::from(i + 1 == blocks);
        out.extend_from_slice(&header.to_le_bytes()[..3]);
        out.push(0);
    }
    out
}

/// numcodecs' LZ4 chunk: the size its block decodes to, stated as `size`, then the block.
fn lz4(size: u32, block: &[u8]) -> Vec<u8> {
    let mut out = size.to_le_bytes().to_vec();
    out.extend_from_slice(block);
    out
}

/// The Blosc compressors terrio reads, by their code in a Blosc header.
const BLOSCLZ: u8 = 0;
const LZ4: u8 = 1;
const ZLIB: u8 = 3;
const ZSTD: u8 = 4;

/// A Blosc chunk (format version 2) of `nbytes` bytes in one block of 1-byte elements, unshuffled,
/// in one `stream` of `compressor`.
fn blosc(compressor: u8, nbytes: u32, stream: &[u8]) -> Vec<u8> {
    let len = u32::try_from(stream.len()).unwrap();
    let mut out = vec![2, 1, compressor << 5, 1];
    // The bytes, the block size (one block), the chunk's size; where the block starts, its stream's
    // size.
    for v in [nbytes, nbytes, 24 + len, 20, len] {
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(stream);
    out
}

/// A Zarr v2 array `name` in `dir` of one chunk `chunk` of shape `shape` (rows, columns) and type
/// `dtype`, coded with `compressor` (numcodecs' metadata).
fn store(dir: &Path, name: &str, shape: [usize; 2], dtype: &str, compressor: &str, chunk: &[u8]) -> String {
    let path = dir.join(format!("{name}.zarr"));
    std::fs::create_dir_all(&path).unwrap();
    let zarray = format!(
        r#"{{"zarr_format": 2, "shape": {shape:?}, "chunks": {shape:?}, "dtype": "{dtype}",
        "compressor": {compressor}, "fill_value": 0, "order": "C", "filters": null}}"#
    );
    std::fs::write(path.join(".zarray"), zarray).unwrap();
    std::fs::write(path.join("0.0"), chunk).unwrap();
    path.to_str().unwrap().to_string()
}

/// The stores' 256 x 256 UInt8 chunk, read whole and by window.
fn read(path: &str) -> [terrio::Result<Vec<f64>>; 2] {
    let window = || {
        let mut r = terrio::open_raster_lazy(path)?;
        r.read_window(Window::full(256, 256), &WindowOptions { bands: vec![1], overview: None })
    };
    [terrio::open_raster(path), window()].map(|ds| ds.map(|ds| ds.raster.data.to_f64_vec()))
}

/// `chunk`, coded with `compressor`, read as a 256 x 256 UInt8 chunk whole and by window: an error
/// whose message holds `want`.
fn refused(name: &str, compressor: &str, chunk: &[u8], want: &str) {
    let dir = tempfile::tempdir().unwrap();
    for r in read(&store(dir.path(), name, [256, 256], "|u1", compressor, chunk)) {
        let e = r.map(|v| v.len()).unwrap_err().to_string();
        assert!(e.contains(want), "{name}: {e}");
    }
}

fn blosc_metadata(cname: &str) -> String {
    format!(r#"{{"id": "blosc", "cname": "{cname}", "clevel": 5, "shuffle": 0, "blocksize": 0}}"#)
}

#[test]
fn the_test_streams_expand_as_described() {
    // Small ones, read by flate2 and lz4_flex, checksums included.
    let n = 1 + 258 * 3;
    for (stream, gz) in [(zlib(&deflate_bomb(3), n), false), (gzip(&deflate_bomb(3), n), true)] {
        let mut out = Vec::new();
        if gz {
            flate2::read::GzDecoder::new(&stream[..]).read_to_end(&mut out).unwrap();
        } else {
            flate2::read::ZlibDecoder::new(&stream[..]).read_to_end(&mut out).unwrap();
        }
        assert_eq!(out, vec![0; n]);
    }
    assert_eq!(lz4_flex::block::decompress(&lz4_bomb(1000), 1000).unwrap(), vec![0; 1000]);
    // And within a chunk's 65536 bytes, or past them within its limit (read as its first 65536),
    // read by terrio as zeros; one byte more is an error.
    let dir = tempfile::tempdir().unwrap();
    let zeros = vec![0.0; 65536];
    for (name, compressor, gz) in [("zlib", r#"{"id": "zlib", "level": 6}"#, false), ("gzip", r#"{"id": "gzip", "level": 6}"#, true)] {
        refused(name, compressor, &deflated(MOST + 1, gz), &format!("Zarr: {name}: a chunk decompressing to more than {MOST} bytes"));
    }
    for (name, compressor, chunk) in [
        ("zlib", r#"{"id": "zlib", "level": 6}"#.to_string(), deflated(65536, false)),
        ("gzip", r#"{"id": "gzip", "level": 6}"#.to_string(), deflated(MOST, true)),
        ("lz4", r#"{"id": "lz4", "acceleration": 1}"#.to_string(), lz4(65536, &lz4_bomb(65536))),
        ("blosclz", blosc_metadata("blosclz"), blosc(BLOSCLZ, 65536, &blosclz_bomb(65536))),
        ("blosc-lz4", blosc_metadata("lz4"), blosc(LZ4, 65536, &lz4_bomb(65536))),
    ] {
        for r in read(&store(dir.path(), name, [256, 256], "|u1", &compressor, &chunk)) {
            assert_eq!(r.unwrap_or_else(|e| panic!("{name}: {e}")), zeros, "{name}");
        }
    }
    // The large ones: 4 MB and 17 MB, expanding to about 4 GiB.
    assert_eq!((REPEATS, deflate_bomb(REPEATS).len(), 1 + 258 * REPEATS), (16_647_160, 4_161_804, 4_294_967_281));
    assert_eq!((lz4_bomb(BIG).len(), blosclz_bomb(BIG).len()), (16_843_019, 16_843_015));
}

/// A zlib chunk of 4 MB expanding to 4 GiB: an error past the chunk's limit.
#[test]
fn a_zlib_zarr_chunk_is_an_error_past_its_size() {
    let chunk = zlib(&deflate_bomb(REPEATS), 1 + 258 * REPEATS);
    let want = format!("Zarr: zlib: a chunk decompressing to more than {MOST} bytes");
    refused("zlib", r#"{"id": "zlib", "level": 6}"#, &chunk, &want);
}

/// A gzip chunk of 4 MB expanding to 4 GiB: an error past the chunk's limit.
#[test]
fn a_gzip_zarr_chunk_is_an_error_past_its_size() {
    let chunk = gzip(&deflate_bomb(REPEATS), 1 + 258 * REPEATS);
    let want = format!("Zarr: gzip: a chunk decompressing to more than {MOST} bytes");
    refused("gzip", r#"{"id": "gzip", "level": 6}"#, &chunk, &want);
}

/// An LZ4 chunk of 17 MB expanding to 4 GiB: stating that size, refused before it is allocated;
/// stating the chunk's 65536 bytes, an error past them.
#[test]
fn an_lz4_zarr_chunk_is_an_error_past_its_size() {
    let metadata = r#"{"id": "lz4", "acceleration": 1}"#;
    let block = lz4_bomb(BIG);
    refused("lz4", metadata, &lz4(u32::MAX, &block), &format!("Zarr: LZ4: a chunk declaring {BIG} bytes, more than {MOST}"));
    refused("lz4", metadata, &lz4(65536, &block), "Zarr: LZ4: provided output is too small for the decompressed data, actual 65536");
}

/// A Blosc chunk whose header states 4 GiB, its stream (of each compressor) expanding to that:
/// refused before anything is allocated.
#[test]
fn a_blosc_zarr_chunk_stating_more_than_its_size_is_refused() {
    let want = format!("Zarr: Blosc: a chunk declaring {BIG} bytes, more than {MOST}");
    let deflate = zlib(&deflate_bomb(REPEATS), 1 + 258 * REPEATS);
    for (cname, compressor, stream) in [
        ("blosclz", BLOSCLZ, blosclz_bomb(BIG)),
        ("lz4", LZ4, lz4_bomb(BIG)),
        ("zlib", ZLIB, deflate),
        ("zstd", ZSTD, zstd_bomb(32 * 1024)),
    ] {
        refused(cname, &blosc_metadata(cname), &blosc(compressor, u32::MAX, &stream), &want);
    }
}

/// A Blosc chunk of the chunk's 65536 bytes whose BloscLZ stream of 17 MB expands to 4 GiB: an
/// error past the 65536 bytes of its block.
// Without the GDAL backend: with it, what terrio refuses goes on to GDAL (gdal_fallback).
#[cfg(not(feature = "gdal"))]
#[test]
fn a_blosc_blosclz_zarr_chunk_is_an_error_past_its_size() {
    let chunk = blosc(BLOSCLZ, 65536, &blosclz_bomb(BIG));
    refused("blosclz", &blosc_metadata("blosclz"), &chunk, "Zarr: BloscLZ: a block decompressing to more than 65536 bytes");
}

/// As above, an LZ4 stream of 17 MB.
#[test]
fn a_blosc_lz4_zarr_chunk_is_an_error_past_its_size() {
    let chunk = blosc(LZ4, 65536, &lz4_bomb(BIG));
    refused("lz4", &blosc_metadata("lz4"), &chunk, "Zarr: Blosc LZ4: provided output is too small for the decompressed data, actual 65536");
}

/// As above, a zlib stream of 4 MB.
#[test]
fn a_blosc_zlib_zarr_chunk_is_an_error_past_its_size() {
    let chunk = blosc(ZLIB, 65536, &zlib(&deflate_bomb(REPEATS), 1 + 258 * REPEATS));
    refused("zlib", &blosc_metadata("zlib"), &chunk, "Zarr: Blosc zlib: a block decompressing to more than 65536 bytes");
}

/// As above, a Zstandard stream of 131 KB.
#[test]
fn a_blosc_zstd_zarr_chunk_is_an_error_past_its_size() {
    let chunk = blosc(ZSTD, 65536, &zstd_bomb(32 * 1024));
    refused("zstd", &blosc_metadata("zstd"), &chunk, "Zarr: zstd: a chunk decompressing to more than 65536 bytes");
}

/// A Blosc block decoding short of its size: an error, as in c-blosc, rather than a byte shuffle
/// undone past the end of the block.
#[test]
fn a_short_blosc_block_is_an_error() {
    // Elements of 2 bytes, byte-shuffled, the block in one stream (not split): 100 bytes of 65536.
    let mut chunk = blosc(BLOSCLZ, 65536, &blosclz_bomb(100));
    chunk[2] |= 0x01 | 0x10;
    chunk[3] = 2;
    refused("short", &blosc_metadata("blosclz"), &chunk, "Zarr: Blosc: a block decompressing to 100 bytes, 65536 expected");
}

/// Chunks numcodecs wrote with each codec: Blosc with BloscLZ, LZ4, LZ4HC, zlib and Zstandard,
/// unshuffled, byte- or bit-shuffled, in blocks of 256 bytes (the last one shorter), and LZ4, zlib,
/// gzip and Zstandard. Each reads as the values written, whole and by window.
#[test]
fn well_formed_chunks_of_each_codec_read_as_written() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/zarr_codecs/numcodecs.json");
    let fixtures: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!((&fixtures["shape"], &fixtures["dtype"]), (&serde_json::json!([33, 16]), &serde_json::json!("<u2")));
    let written: Vec<f64> = (0..33 * 16).map(|i| f64::from(2000 + (i / 16) * 5 + (i % 16) / 4)).collect();
    let chunks = fixtures["chunks"].as_object().unwrap();
    assert_eq!(chunks.len(), 12);
    let dir = tempfile::tempdir().unwrap();
    for (name, c) in chunks {
        let hex = c["chunk"].as_str().unwrap();
        let chunk: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
        let path = store(dir.path(), name, [33, 16], "<u2", &c["compressor"].to_string(), &chunk);
        let ds = terrio::open_raster(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!((ds.raster.cols, ds.raster.rows, ds.raster.data_type), (16, 33, terrio::DataType::U16), "{name}");
        assert_eq!(ds.raster.data.to_f64_vec(), written, "{name}");
        let mut r = terrio::open_raster_lazy(&path).unwrap();
        let window = r.read_window(Window::new(3, 5, 9, 20), &WindowOptions { bands: vec![1], overview: None }).unwrap();
        let want: Vec<f64> = (5..25).flat_map(|row| written[row * 16..][3..12].to_vec()).collect();
        assert_eq!(window.raster.data.to_f64_vec(), want, "{name}");
    }
}

/// A Zarr v2 array `name` in `dir` of 1 x 1 UInt8 (fill value 7) in chunks of `chunks` (rows,
/// columns) coded with zlib, its one chunk `chunk` when given.
fn declared(dir: &Path, name: &str, chunks: [u64; 2], chunk: Option<&[u8]>) -> String {
    let path = dir.join(format!("{name}.zarr"));
    std::fs::create_dir_all(&path).unwrap();
    let zarray = format!(
        r#"{{"zarr_format": 2, "shape": [1, 1], "chunks": {chunks:?}, "dtype": "|u1",
        "compressor": {{"id": "zlib", "level": 1}}, "fill_value": 7, "order": "C", "filters": null}}"#
    );
    std::fs::write(path.join(".zarray"), zarray).unwrap();
    if let Some(chunk) = chunk {
        std::fs::write(path.join("0.0"), chunk).unwrap();
    }
    path.to_str().unwrap().to_string()
}

/// A Zarr v3 array `name` in `dir` of 1 x 1 UInt8 (fill value 7) in shards of `shard` holding inner
/// chunks of `inner`, without chunks: read by parts, or decoded whole when a CRC32C checksum codes
/// each shard (`whole`).
fn sharded(dir: &Path, name: &str, shard: [u64; 2], inner: [u64; 2], whole: bool) -> String {
    let path = dir.join(format!("{name}.zarr"));
    std::fs::create_dir_all(&path).unwrap();
    let bytes = r#"{"name": "bytes", "configuration": {"endian": "little"}}"#;
    let checksum = if whole { r#", {"name": "crc32c"}"# } else { "" };
    let zarr = format!(
        r#"{{"zarr_format": 3, "node_type": "array", "shape": [1, 1], "data_type": "uint8",
        "chunk_grid": {{"name": "regular", "configuration": {{"chunk_shape": {shard:?}}}}},
        "chunk_key_encoding": {{"name": "default", "configuration": {{"separator": "/"}}}}, "fill_value": 7,
        "codecs": [{{"name": "sharding_indexed", "configuration": {{"chunk_shape": {inner:?}, "codecs": [{bytes}],
        "index_codecs": [{bytes}, {{"name": "crc32c"}}], "index_location": "end"}}}}{checksum}],
        "dimension_names": ["y", "x"]}}"#
    );
    std::fs::write(path.join("zarr.json"), zarr).unwrap();
    path.to_str().unwrap().to_string()
}

/// The 1 x 1 array of the store at `path`, read whole and by window.
fn read_one(path: &str) -> [terrio::Result<Vec<f64>>; 2] {
    let window = || {
        let mut r = terrio::open_raster_lazy(path)?;
        r.read_window(Window::full(1, 1), &WindowOptions { bands: vec![1], overview: None })
    };
    [terrio::open_raster(path), window()].map(|ds| ds.map(|ds| ds.raster.data.to_f64_vec()))
}

/// GDAL's error for a chunk of `bytes` past its 1 GB limit.
fn too_big(bytes: u64) -> String {
    format!("Zarr: tile allocation would require {bytes} bytes. By default the driver limits to 1 GB.")
}

/// Reads the store at `path`, each read an error holding `want`, or the fill value when `want` is
/// `None`.
fn expect(path: &str, want: Option<&str>) {
    for r in read_one(path) {
        match want {
            Some(want) => {
                let e = r.map(|v| v.len()).unwrap_err().to_string();
                assert!(e.contains(want), "{path}: {e}");
            }
            None => assert_eq!(r.unwrap_or_else(|e| panic!("{path}: {e}")), vec![7.0], "{path}"),
        }
    }
}

/// A chunk whose bytes exceed 1 GiB, stored or not, is refused at read, as GDAL 3.12.1 refuses it
/// (rasterio 1.5.0 on these stores: "Zarr tile allocation would require 1600000000 bytes. By
/// default the driver limits to 1 GB. To allow that memory allocation, set the
/// ZARR_ALLOW_BIG_TILE_SIZE configuration option to YES."), and read when an option allows it.
#[test]
fn a_chunk_past_1_gib_is_refused_as_gdal_refuses_it() {
    let dir = tempfile::tempdir().unwrap();
    // Chunks of 40000 x 40000 bytes, the one chunk a zlib stream of 4 MB expanding to 4 GiB, or
    // missing.
    let bomb = zlib(&deflate_bomb(REPEATS), 1 + 258 * REPEATS);
    expect(&declared(dir.path(), "stored", [40000, 40000], Some(&bomb)), Some(&too_big(1_600_000_000)));
    expect(&declared(dir.path(), "missing", [40000, 40000], None), Some(&too_big(1_600_000_000)));
    // 1 GiB is read, as by GDAL; a column more is refused.
    expect(&declared(dir.path(), "at", [32768, 32768], None), None);
    expect(&declared(dir.path(), "past", [32768, 32769], None), Some(&too_big(1_073_774_592)));
    // Allowed by terrio's option or GDAL's, terrio's read first.
    let path = declared(dir.path(), "allowed", [40000, 40000], None);
    for (options, allowed) in [
        (vec![("ZARR_ALLOW_BIG_TILE_SIZE", "YES")], true),
        (vec![("TERRIO_ZARR_ALLOW_BIG_TILE_SIZE", "YES")], true),
        (vec![("TERRIO_ZARR_ALLOW_BIG_TILE_SIZE", "NO"), ("ZARR_ALLOW_BIG_TILE_SIZE", "YES")], false),
    ] {
        terrio::config::push_scope(options.iter().map(|(k, v)| (k.to_string(), Some(v.to_string()))).collect::<HashMap<_, _>>());
        expect(&path, (!allowed).then(|| too_big(1_600_000_000)).as_deref());
        assert!(terrio::config::pop_scope());
    }
}

/// A Zarr v2 array `name` in `dir` of 1 x 1 elements of `dtype` (fill value 7) in chunks of
/// `chunks` (rows, columns) stored in `order` (`C` or `F`), coded with zlib, without chunks.
fn ordered(dir: &Path, name: &str, chunks: [u64; 2], dtype: &str, order: &str) -> String {
    let path = dir.join(format!("{name}.zarr"));
    std::fs::create_dir_all(&path).unwrap();
    let zarray = format!(
        r#"{{"zarr_format": 2, "shape": [1, 1], "chunks": {chunks:?}, "dtype": "{dtype}",
        "compressor": {{"id": "zlib", "level": 1}}, "fill_value": 7, "order": "{order}", "filters": null}}"#
    );
    std::fs::write(path.join(".zarray"), zarray).unwrap();
    path.to_str().unwrap().to_string()
}

/// A chunk in Fortran order counts twice, as GDAL 3.12.1 counts it (`ZarrV2Array::
/// AllocateWorkingBuffers`: the chunk, and the copy it transposes the chunk into): refused at read
/// past 512 MiB (rasterio 1.5.0 on a 1 GiB chunk: "Zarr tile allocation would require 2147483648
/// bytes. By default the driver limits to 1 GB. ..."), and read when an option allows it. A chunk
/// in C order counts once, and so does a byte-swapped one, which GDAL counts twice: terrio swaps it
/// in place.
#[test]
fn a_chunk_in_fortran_order_counts_twice_as_gdal_counts_it() {
    let dir = tempfile::tempdir().unwrap();
    // 512 MiB is read, as by GDAL; a column more, 600 MB and 1 GiB are refused.
    expect(&ordered(dir.path(), "f-at", [16384, 32768], "|u1", "F"), None);
    expect(&ordered(dir.path(), "f-past", [16384, 32769], "|u1", "F"), Some(&too_big(1_073_774_592)));
    expect(&ordered(dir.path(), "f-600mb", [24495, 24495], "|u1", "F"), Some(&too_big(1_200_010_050)));
    expect(&ordered(dir.path(), "f-1gib", [32768, 32768], "|u1", "F"), Some(&too_big(2_147_483_648)));
    // 1 GiB in C order, byte-swapped or not, is read.
    expect(&ordered(dir.path(), "c-1gib", [32768, 32768], "|u1", "C"), None);
    expect(&ordered(dir.path(), "be-1gib", [16384, 32768], ">u2", "C"), None);
    // Allowed by terrio's option or GDAL's.
    let path = ordered(dir.path(), "f-allowed", [32768, 32768], "|u1", "F");
    for option in ["TERRIO_ZARR_ALLOW_BIG_TILE_SIZE", "ZARR_ALLOW_BIG_TILE_SIZE"] {
        terrio::config::push_scope([(option.to_string(), Some("YES".to_string()))].into());
        expect(&path, None);
        assert!(terrio::config::pop_scope());
    }
}

/// Chunk dimensions whose product overflows (2^32 x 2^32 elements, 0 modulo 2^64), of a chunk or of
/// a shard's inner chunks: an error, GDAL's "Too large chunks", rather than a size that wraps.
#[test]
fn chunk_dimensions_whose_product_overflows_are_an_error() {
    let dir = tempfile::tempdir().unwrap();
    expect(&declared(dir.path(), "wrap", [1 << 32, 1 << 32], None), Some("Zarr: Too large chunks"));
    expect(&sharded(dir.path(), "wrap-shard", [1 << 32, 1 << 32], [1, 1], false), Some("Zarr: Too large chunks"));
}

/// A shard read by parts decodes its inner chunks one by one: they are what is checked against
/// the limit, as GDAL's sharding support (after 3.12; 3.12.1 opens no sharded array, "Unsupported
/// codec: sharding_indexed") checks them. A shard decoded whole, under a checksum, is checked
/// itself.
#[test]
fn shards_are_checked_as_they_are_decoded() {
    let dir = tempfile::tempdir().unwrap();
    expect(&sharded(dir.path(), "parts", [40000, 40000], [256, 256], false), None);
    expect(&sharded(dir.path(), "inner", [40000, 40000], [40000, 40000], false), Some(&too_big(1_600_000_000)));
    expect(&sharded(dir.path(), "whole", [40000, 40000], [256, 256], true), Some(&too_big(1_600_000_000)));
    expect(&sharded(dir.path(), "small", [256, 256], [256, 256], true), None);
}

/// The process's peak resident memory, in bytes (`VmHWM`).
#[cfg(target_os = "linux")]
fn peak_memory() -> usize {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status.lines().find(|l| l.starts_with("VmHWM:")).unwrap();
    line.split_whitespace().nth(1).unwrap().parse::<usize>().unwrap() * 1024
}

/// A zlib stream of 4 MB expanding to 4 GiB in a chunk declared just under 1 GiB (32767 x 32768
/// bytes): an error past those bytes and the slack, read no further. Under a memory cap of 4 GiB
/// (as these tests run), a decoder without the bound is stopped; the peak memory of this test's
/// process stays under 1.25 GiB.
#[test]
fn a_zlib_chunk_of_almost_1_gib_is_decoded_no_further_than_its_size() {
    let dir = tempfile::tempdir().unwrap();
    let path = declared(dir.path(), "almost", [32767, 32768], Some(&zlib(&deflate_bomb(REPEATS), 1 + 258 * REPEATS)));
    let e = terrio::open_raster(&path).map(|ds| ds.raster.cols).unwrap_err().to_string();
    assert!(e.contains("Zarr: zlib: a chunk decompressing to more than 1073709120 bytes"), "{e}");
    #[cfg(target_os = "linux")]
    assert!(peak_memory() < 5 << 28, "peak {} bytes", peak_memory());
}

/// A Zarr v2 array `name` in `dir` of UInt16 (fill value 7) declaring `shape` (bands, rows,
/// columns, or rows and columns) in chunks of `chunks`, uncompressed, its first chunk stored: the
/// value of element `i` of the chunk (C order) is `i`.
fn declaring(dir: &Path, name: &str, shape: &[u64], chunks: &[u64]) -> String {
    let path = dir.join(format!("{name}.zarr"));
    std::fs::create_dir_all(&path).unwrap();
    let zarray = format!(
        r#"{{"zarr_format": 2, "shape": {shape:?}, "chunks": {chunks:?}, "dtype": "<u2", "compressor": null,
        "fill_value": 7, "order": "C", "filters": null}}"#
    );
    std::fs::write(path.join(".zarray"), zarray).unwrap();
    let n: u64 = chunks.iter().product();
    let first = vec!["0"; chunks.len()].join(".");
    std::fs::write(path.join(first), (0..n).flat_map(|i| (i as u16).to_le_bytes()).collect::<Vec<u8>>()).unwrap();
    path.to_str().unwrap().to_string()
}

/// The error of a window of `width` x `height` pixels of an array declaring `shape`, read lazily
/// and opened whole.
#[cfg(not(feature = "gdal"))]
fn window_errors(path: &str, width: usize, height: usize) -> [String; 2] {
    let mut r = terrio::open_raster_lazy(path).unwrap();
    let lazy = r.read_window(Window::new(0, 0, width, height), &WindowOptions { bands: vec![1], overview: None });
    [lazy.map(|ds| ds.raster.cols), terrio::open_raster(path).map(|ds| ds.raster.cols)].map(|r| r.unwrap_err().to_string())
}

/// The error of a window of `size` pixels taking `bytes`: refused against the usable memory where
/// it is known, else by the allocation that fails.
#[cfg(not(feature = "gdal"))]
fn window_refusal(size: &str, bytes: u64) -> String {
    if terrio::usable_memory().is_some() {
        format!("Zarr: a window of {size} pixels in 1 band(s) is too large: {bytes} bytes")
    } else {
        format!("cannot allocate {bytes} bytes for a Zarr region")
    }
}

/// A shape of 2^52 columns declared by 4 KB of metadata: its window of 2^59 bytes is an error before
/// anything is allocated, as GeoTIFF's windows past the memory are, rather than an allocation that
/// ends the process ("memory allocation of 576460752303423488 bytes failed").
// Without the GDAL backend: with it, what terrio refuses goes on to GDAL (gdal_fallback).
#[cfg(not(feature = "gdal"))]
#[test]
fn a_window_of_a_huge_declared_shape_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = declaring(dir.path(), "wide", &[64, 1 << 52], &[32, 32]);
    let expected = window_refusal("4503599627370496x64", 1 << 59);
    for e in window_errors(&path, 1 << 52, 64) {
        assert!(e.contains(&expected), "{e}");
    }
}

/// A window of 2^63 bytes, past what a `Vec` can hold: an error, not a panic ("capacity overflow").
// Without the GDAL backend: with it, what terrio refuses goes on to GDAL (gdal_fallback).
#[cfg(not(feature = "gdal"))]
#[test]
fn a_window_past_what_a_buffer_holds_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = declaring(dir.path(), "wider", &[64, 1 << 56], &[32, 32]);
    let expected = window_refusal("72057594037927936x64", 1 << 63);
    for e in window_errors(&path, 1 << 56, 64) {
        assert!(e.contains(&expected), "{e}");
    }
}

/// A decimated read (nearest neighbour) of an array declaring 2^52 columns reads the chunks
/// holding the pixels it picks, as GDAL's decimated read does, not the window spanning them: the
/// stored chunk's values and the fill value elsewhere, repeated pixels and pixels in any order
/// included.
#[test]
fn a_decimated_read_of_a_huge_declared_shape_reads_the_chunks_it_picks() {
    let dir = tempfile::tempdir().unwrap();
    let path = declaring(dir.path(), "wide", &[64, 1 << 52], &[32, 32]);
    let mut r = terrio::open_raster_lazy(&path).unwrap();
    let cols = [3, 0, 33, 1 << 42, (1 << 52) - 1, 3];
    let rows = [5, 0, 40, 63];
    let got = r.read_sampled(&cols, &rows, &WindowOptions::default()).unwrap().expect("Zarr reads the chunks it picks");
    let want: Vec<f64> = rows
        .iter()
        .flat_map(|&row| cols.iter().map(move |&col| if row < 32 && col < 32 { (row * 32 + col) as f64 } else { 7.0 }))
        .collect();
    assert_eq!(got.to_f64_vec(), want);
    // A pixel outside the array is an error.
    assert!(r.read_sampled(&[1 << 52], &[0], &WindowOptions::default()).is_err());
}

/// The pixels a decimated read picks, in bands, rows and columns crossing chunk edges, are those
/// of the window spanning them.
#[test]
fn a_decimated_read_picks_the_pixels_of_the_window_spanning_them() {
    let dir = tempfile::tempdir().unwrap();
    let path = declaring(dir.path(), "cube", &[3, 70, 90], &[2, 32, 32]);
    let mut r = terrio::open_raster_lazy(&path).unwrap();
    let options = WindowOptions { bands: vec![3, 1], overview: None };
    let whole = r.read_window(Window::full(90, 70), &options).unwrap().raster.data.to_f64_vec();
    let (cols, rows) = ([89, 0, 31, 32, 63, 64, 64, 5], [69, 0, 31, 32, 33, 64]);
    let got = r.read_sampled(&cols, &rows, &options).unwrap().expect("Zarr reads the chunks it picks").to_f64_vec();
    let mut want = Vec::new();
    for b in 0..2 {
        for &row in &rows {
            want.extend(cols.iter().map(|&col| whole[(b * 70 + row) * 90 + col]));
        }
    }
    assert_eq!(got, want);
}

/// At most 65536 bands, as GDAL 3.12.1 exposes an array as a raster (rasterio 1.5.0: an array of
/// 65536 x 2 x 2 opens, one of 65537 x 2 x 2 is "Too many bands. Operate on a sliced view"): a
/// store declaring 2^40 bands is an error at open, not a description of each band ("memory
/// allocation of 140737488355328 bytes failed").
#[test]
fn more_bands_than_gdal_reads_are_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = declaring(dir.path(), "most", &[65536, 2, 2], &[1, 2, 2]);
    let mut r = terrio::open_raster_lazy(&path).unwrap();
    assert_eq!(r.band_count(), 65536);
    let first = r.read_window(Window::full(2, 2), &WindowOptions { bands: vec![1, 65536], overview: None }).unwrap();
    assert_eq!(first.raster.data.to_f64_vec(), vec![0.0, 1.0, 2.0, 3.0, 7.0, 7.0, 7.0, 7.0]);
    for bands in [65537u64, 1 << 40] {
        let path = declaring(dir.path(), &format!("b{bands}"), &[bands, 2, 2], &[1, 2, 2]);
        for e in [terrio::open_raster_lazy(&path).map(|r| r.band_count()), terrio::open_raster(&path).map(|ds| ds.raster.bands)] {
            let e = e.unwrap_err().to_string();
            assert!(e.contains("Zarr: Too many bands. Operate on a sliced view"), "{bands}: {e}");
        }
    }
}
