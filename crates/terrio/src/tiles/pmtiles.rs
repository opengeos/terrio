//! PMTiles v3 archives: the header, directories and tile ids, read from any byte source (a file,
//! bytes in memory, or a remote file through HTTP range requests) and written in one go.
//!
//! The wire format code (header, directories, varints, Hilbert tile ids, the writer's directory
//! splitting) is adapted from the `geolibre-pmtiles` crate of GeoLibre
//! (<https://github.com/opengeos/geolibre-rust>, MIT, by the same author).
//!
//! The format is described at <https://github.com/protomaps/PMTiles/blob/main/spec/v3/spec.md>.

use super::source::ByteSource;
use crate::{Error, Result};
use std::collections::HashMap;
use std::io::{Read, Write};

/// The length of the fixed header.
pub(crate) const HEADER_LEN: usize = 127;
/// The header and root directory fit in the first 16 KiB.
const PRELUDE_LEN: usize = 16_384;

/// Compression codecs (internal and tile compression).
pub(crate) const COMPRESSION_NONE: u8 = 1;
pub(crate) const COMPRESSION_GZIP: u8 = 2;
pub(crate) const COMPRESSION_BROTLI: u8 = 3;
pub(crate) const COMPRESSION_ZSTD: u8 = 4;

/// Tile types.
pub(crate) const TILE_MVT: u8 = 1;
pub(crate) const TILE_PNG: u8 = 2;
pub(crate) const TILE_JPEG: u8 = 3;
pub(crate) const TILE_WEBP: u8 = 4;
pub(crate) const TILE_AVIF: u8 = 5;

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("PMTiles: {msg}"))
}

/// The 127-byte header.
#[derive(Clone, Debug, PartialEq, Default)]
pub(crate) struct Header {
    pub root_dir_offset: u64,
    pub root_dir_length: u64,
    pub metadata_offset: u64,
    pub metadata_length: u64,
    pub leaf_dirs_offset: u64,
    pub leaf_dirs_length: u64,
    pub tile_data_offset: u64,
    pub tile_data_length: u64,
    pub num_addressed_tiles: u64,
    pub num_tile_entries: u64,
    pub num_tile_contents: u64,
    pub clustered: bool,
    pub internal_compression: u8,
    pub tile_compression: u8,
    pub tile_type: u8,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub min_lon_e7: i32,
    pub min_lat_e7: i32,
    pub max_lon_e7: i32,
    pub max_lat_e7: i32,
    pub center_zoom: u8,
    pub center_lon_e7: i32,
    pub center_lat_e7: i32,
}

impl Header {
    pub(crate) fn parse(b: &[u8]) -> Result<Header> {
        if b.len() < HEADER_LEN {
            return Err(bad("truncated header"));
        }
        if &b[0..7] != b"PMTiles" {
            return Err(bad("not a PMTiles archive"));
        }
        if b[7] != 3 {
            return Err(bad(format!("version {} is not supported (only 3)", b[7])));
        }
        let u = |at: usize| u64::from_le_bytes(b[at..at + 8].try_into().unwrap_or_default());
        let i = |at: usize| i32::from_le_bytes(b[at..at + 4].try_into().unwrap_or_default());
        Ok(Header {
            root_dir_offset: u(8),
            root_dir_length: u(16),
            metadata_offset: u(24),
            metadata_length: u(32),
            leaf_dirs_offset: u(40),
            leaf_dirs_length: u(48),
            tile_data_offset: u(56),
            tile_data_length: u(64),
            num_addressed_tiles: u(72),
            num_tile_entries: u(80),
            num_tile_contents: u(88),
            clustered: b[96] == 1,
            internal_compression: b[97],
            tile_compression: b[98],
            tile_type: b[99],
            min_zoom: b[100],
            max_zoom: b[101],
            min_lon_e7: i(102),
            min_lat_e7: i(106),
            max_lon_e7: i(110),
            max_lat_e7: i(114),
            center_zoom: b[118],
            center_lon_e7: i(119),
            center_lat_e7: i(123),
        })
    }

    pub(crate) fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[0..7].copy_from_slice(b"PMTiles");
        h[7] = 3;
        let mut put = |at: usize, v: u64| h[at..at + 8].copy_from_slice(&v.to_le_bytes());
        put(8, self.root_dir_offset);
        put(16, self.root_dir_length);
        put(24, self.metadata_offset);
        put(32, self.metadata_length);
        put(40, self.leaf_dirs_offset);
        put(48, self.leaf_dirs_length);
        put(56, self.tile_data_offset);
        put(64, self.tile_data_length);
        put(72, self.num_addressed_tiles);
        put(80, self.num_tile_entries);
        put(88, self.num_tile_contents);
        h[96] = u8::from(self.clustered);
        h[97] = self.internal_compression;
        h[98] = self.tile_compression;
        h[99] = self.tile_type;
        h[100] = self.min_zoom;
        h[101] = self.max_zoom;
        h[118] = self.center_zoom;
        for (at, v) in [
            (102, self.min_lon_e7),
            (106, self.min_lat_e7),
            (110, self.max_lon_e7),
            (114, self.max_lat_e7),
            (119, self.center_lon_e7),
            (123, self.center_lat_e7),
        ] {
            h[at..at + 4].copy_from_slice(&v.to_le_bytes());
        }
        h
    }

    /// The bounds `(min_lon, min_lat, max_lon, max_lat)`, as GDAL reads them.
    pub(crate) fn bounds(&self) -> [f64; 4] {
        [self.min_lon_e7 as f64 / 1e7, self.min_lat_e7 as f64 / 1e7, self.max_lon_e7 as f64 / 1e7, self.max_lat_e7 as f64 / 1e7]
    }
}

/// A directory entry: a run of `run_length` tiles sharing one blob, or (with a run length of 0) a
/// leaf directory.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Entry {
    pub tile_id: u64,
    pub offset: u64,
    pub length: u32,
    pub run_length: u32,
}

fn write_uvarint(buf: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if value == 0 {
            break;
        }
    }
}

fn read_uvarint(bytes: &[u8], pos: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *bytes.get(*pos).ok_or_else(|| bad("truncated varint"))?;
        *pos += 1;
        if shift == 63 && byte > 1 {
            return Err(bad("varint overflows"));
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift > 63 {
            return Err(bad("varint too long"));
        }
    }
}

/// Serializes directory entries (sorted by tile id).
pub(crate) fn serialize_directory(entries: &[Entry]) -> Vec<u8> {
    let mut buf = Vec::new();
    write_uvarint(&mut buf, entries.len() as u64);
    let mut last_id = 0u64;
    for e in entries {
        write_uvarint(&mut buf, e.tile_id - last_id);
        last_id = e.tile_id;
    }
    for e in entries {
        write_uvarint(&mut buf, e.run_length as u64);
    }
    for e in entries {
        write_uvarint(&mut buf, e.length as u64);
    }
    for (i, e) in entries.iter().enumerate() {
        if i > 0 && e.offset == entries[i - 1].offset + entries[i - 1].length as u64 {
            write_uvarint(&mut buf, 0);
        } else {
            write_uvarint(&mut buf, e.offset + 1);
        }
    }
    buf
}

/// Parses an uncompressed directory.
pub(crate) fn parse_directory(bytes: &[u8]) -> Result<Vec<Entry>> {
    let mut pos = 0usize;
    let n = read_uvarint(bytes, &mut pos)? as usize;
    if n > bytes.len() {
        return Err(bad(format!("a directory claims {n} entries in {} bytes", bytes.len())));
    }
    let mut entries = vec![Entry { tile_id: 0, offset: 0, length: 0, run_length: 0 }; n];
    let mut last_id = 0u64;
    for e in entries.iter_mut() {
        last_id = last_id.checked_add(read_uvarint(bytes, &mut pos)?).ok_or_else(|| bad("tile id overflow"))?;
        e.tile_id = last_id;
    }
    for e in entries.iter_mut() {
        e.run_length = read_uvarint(bytes, &mut pos)? as u32;
    }
    for e in entries.iter_mut() {
        e.length = read_uvarint(bytes, &mut pos)? as u32;
    }
    for i in 0..n {
        let v = read_uvarint(bytes, &mut pos)?;
        entries[i].offset = if v == 0 {
            if i == 0 {
                return Err(bad("the first directory entry has a relative offset"));
            }
            entries[i - 1].offset + entries[i - 1].length as u64
        } else {
            v - 1
        };
    }
    Ok(entries)
}

/// Compresses with a PMTiles codec.
pub(crate) fn compress(data: &[u8], codec: u8) -> Result<Vec<u8>> {
    match codec {
        COMPRESSION_NONE => Ok(data.to_vec()),
        COMPRESSION_GZIP => {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(data)?;
            Ok(e.finish()?)
        }
        c => Err(Error::Unsupported(format!("PMTiles: compression {c} cannot be written (none and gzip can)"))),
    }
}

/// Decompresses with a PMTiles codec (none, gzip or zstd).
pub(crate) fn decompress(data: &[u8], codec: u8) -> Result<Vec<u8>> {
    match codec {
        COMPRESSION_NONE | 0 => Ok(data.to_vec()),
        COMPRESSION_GZIP => {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(data).read_to_end(&mut out).map_err(|e| bad(format!("gzip: {e}")))?;
            Ok(out)
        }
        COMPRESSION_ZSTD => {
            let mut src = data;
            let mut d = ruzstd::decoding::StreamingDecoder::new(&mut src).map_err(|e| bad(format!("zstd: {e}")))?;
            let mut out = Vec::new();
            d.read_to_end(&mut out).map_err(|e| bad(format!("zstd: {e}")))?;
            Ok(out)
        }
        COMPRESSION_BROTLI => Err(Error::Unsupported("PMTiles: brotli compression is not supported".into())),
        c => Err(bad(format!("unknown compression {c}"))),
    }
}

/// The number of tiles in the zoom levels below `z`: the first tile id of zoom `z`.
pub(crate) fn zoom_base(z: u8) -> u64 {
    // (4^z - 1) / 3
    ((1u128 << (2 * z as u32)) as u64).wrapping_sub(1) / 3
}

/// The PMTiles tile id of tile `(z, x, y)`: the zoom's base plus the tile's Hilbert curve index.
pub(crate) fn zxy_to_tile_id(z: u8, x: u32, y: u32) -> u64 {
    let n = 1i64 << z;
    let (mut xx, mut yy) = (x as i64, y as i64);
    let mut d: i64 = 0;
    let mut s = n / 2;
    while s > 0 {
        let rx = i64::from((xx & s) > 0);
        let ry = i64::from((yy & s) > 0);
        d += s * s * ((3 * rx) ^ ry);
        if ry == 0 {
            if rx == 1 {
                xx = s - 1 - xx;
                yy = s - 1 - yy;
            }
            std::mem::swap(&mut xx, &mut yy);
        }
        s /= 2;
    }
    zoom_base(z) + d as u64
}

/// The tile `(z, x, y)` of a PMTiles tile id.
pub(crate) fn tile_id_to_zxy(id: u64) -> (u8, u32, u32) {
    let mut z = 0u8;
    while z < 31 && zoom_base(z + 1) <= id {
        z += 1;
    }
    let mut t = id - zoom_base(z);
    let n = 1u64 << z;
    let (mut x, mut y) = (0u64, 0u64);
    let mut s = 1u64;
    while s < n {
        let rx = 1 & (t / 2);
        let ry = 1 & (t ^ rx);
        if ry == 0 {
            if rx == 1 {
                x = s - 1 - x;
                y = s - 1 - y;
            }
            std::mem::swap(&mut x, &mut y);
        }
        x += s * rx;
        y += s * ry;
        t /= 4;
        s *= 2;
    }
    (z, x as u32, y as u32)
}

/// A PMTiles archive opened for reading: its header, root directory and metadata are read on
/// open; leaf directories and tiles when they are needed.
pub(crate) struct Reader {
    source: ByteSource,
    pub header: Header,
    root: Vec<Entry>,
    leaves: HashMap<(u64, u32), Vec<Entry>>,
    /// The JSON metadata.
    pub metadata: serde_json::Value,
}

impl std::fmt::Debug for Reader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PMTiles").field("header", &self.header).finish_non_exhaustive()
    }
}

impl Reader {
    /// Opens an archive: reads its first 16 KiB (header and root directory), then its metadata.
    pub(crate) fn open(mut source: ByteSource) -> Result<Self> {
        let len = source.len();
        let prelude = source.read_at(0, len.min(PRELUDE_LEN as u64))?;
        let header = Header::parse(&prelude)?;
        let root_end = header.root_dir_offset + header.root_dir_length;
        let raw = if root_end <= prelude.len() as u64 {
            prelude[header.root_dir_offset as usize..root_end as usize].to_vec()
        } else {
            source.read_at(header.root_dir_offset, header.root_dir_length)?
        };
        let root = parse_directory(&decompress(&raw, header.internal_compression)?)?;
        let metadata = if header.metadata_length > 0 {
            let raw = source.read_at(header.metadata_offset, header.metadata_length)?;
            let json = decompress(&raw, header.internal_compression)?;
            serde_json::from_slice(&json).map_err(|e| bad(format!("metadata: {e}")))?
        } else {
            serde_json::Value::Object(Default::default())
        };
        Ok(Self { source, header, root, leaves: HashMap::new(), metadata })
    }

    /// Whether the archive is read in place from a remote file.
    pub(crate) fn is_remote(&self) -> bool {
        self.source.is_remote()
    }

    fn leaf(&mut self, offset: u64, length: u32) -> Result<Vec<Entry>> {
        if let Some(e) = self.leaves.get(&(offset, length)) {
            return Ok(e.clone());
        }
        let raw = self.source.read_at(self.header.leaf_dirs_offset + offset, length as u64)?;
        let entries = parse_directory(&decompress(&raw, self.header.internal_compression)?)?;
        self.leaves.insert((offset, length), entries.clone());
        Ok(entries)
    }

    /// The location `(offset, length)` of a tile's data, absolute in the file.
    pub(crate) fn locate(&mut self, z: u8, x: u32, y: u32) -> Result<Option<(u64, u32)>> {
        let id = zxy_to_tile_id(z, x, y);
        let mut entries = self.root.clone();
        for _ in 0..4 {
            let i = entries.partition_point(|e| e.tile_id <= id);
            if i == 0 {
                return Ok(None);
            }
            let e = entries[i - 1];
            if e.run_length == 0 {
                entries = self.leaf(e.offset, e.length)?;
                continue;
            }
            return Ok((id < e.tile_id + e.run_length as u64).then_some((self.header.tile_data_offset + e.offset, e.length)));
        }
        Err(bad("directories nest too deeply"))
    }

    /// Reads a tile's bytes, decompressed (`None` when the archive does not have it).
    pub(crate) fn tile(&mut self, z: u8, x: u32, y: u32) -> Result<Option<Vec<u8>>> {
        match self.locate(z, x, y)? {
            Some((offset, length)) => {
                let raw = self.source.read_at(offset, length as u64)?;
                Ok(Some(decompress(&raw, self.header.tile_compression)?))
            }
            None => Ok(None),
        }
    }

    /// Reads several tiles, fetching their bytes in as few requests as possible.
    pub(crate) fn tiles(&mut self, wanted: &[(u8, u32, u32)]) -> Result<Vec<Option<Vec<u8>>>> {
        let mut locs = Vec::with_capacity(wanted.len());
        for &(z, x, y) in wanted {
            locs.push(self.locate(z, x, y)?);
        }
        let ranges: Vec<(u64, u64)> = locs.iter().flatten().map(|(o, l)| (*o, *l as u64)).collect();
        self.source.prefetch(&ranges)?;
        let mut out = Vec::with_capacity(wanted.len());
        for loc in locs {
            out.push(match loc {
                Some((offset, length)) => {
                    let raw = self.source.read_at(offset, length as u64)?;
                    Some(decompress(&raw, self.header.tile_compression)?)
                }
                None => None,
            });
        }
        Ok(out)
    }

    /// The tiles `(x, y)` present at zoom `z`, in tile id order, read from the directories.
    pub(crate) fn tiles_at_zoom(&mut self, z: u8) -> Result<Vec<(u32, u32)>> {
        let (lo, hi) = (zoom_base(z), zoom_base(z + 1));
        let mut out = Vec::new();
        let root = self.root.clone();
        self.collect(&root, lo, hi, &mut out, 0)?;
        Ok(out)
    }

    fn collect(&mut self, entries: &[Entry], lo: u64, hi: u64, out: &mut Vec<(u32, u32)>, depth: usize) -> Result<()> {
        if depth > 4 {
            return Err(bad("directories nest too deeply"));
        }
        for (i, e) in entries.iter().enumerate() {
            if e.run_length == 0 {
                // A leaf covers the ids up to the next entry's.
                let end = entries.get(i + 1).map_or(u64::MAX, |n| n.tile_id);
                if end > lo && e.tile_id < hi {
                    let leaf = self.leaf(e.offset, e.length)?;
                    self.collect(&leaf, lo, hi, out, depth + 1)?;
                }
                continue;
            }
            let end = e.tile_id + e.run_length as u64;
            for id in e.tile_id.max(lo)..end.min(hi) {
                let (_, x, y) = tile_id_to_zxy(id);
                out.push((x, y));
            }
        }
        Ok(())
    }
}

/// A tile to write.
pub(crate) struct OutTile {
    pub z: u8,
    pub x: u32,
    pub y: u32,
    /// The tile's bytes, compressed as the archive says.
    pub data: Vec<u8>,
}

/// What the writer records in the header besides the layout.
pub(crate) struct ArchiveParams {
    pub tile_type: u8,
    pub tile_compression: u8,
    pub min_zoom: u8,
    pub max_zoom: u8,
    /// `(min_lon, min_lat, max_lon, max_lat)`.
    pub bounds: [f64; 4],
    pub center: (f64, f64, u8),
}

/// Writes an archive: tiles (deduplicated by content, laid out in tile id order), the JSON
/// metadata and gzipped directories, split into leaf directories when the root would not fit in
/// the first 16 KiB.
pub(crate) fn write_archive(mut tiles: Vec<OutTile>, metadata: &[u8], params: &ArchiveParams) -> Result<Vec<u8>> {
    tiles.sort_by_key(|t| zxy_to_tile_id(t.z, t.x, t.y));
    let addressed = tiles.len() as u64;
    let mut tile_data: Vec<u8> = Vec::new();
    let mut seen: HashMap<Vec<u8>, (u64, u32)> = HashMap::new();
    let mut entries: Vec<Entry> = Vec::with_capacity(tiles.len());
    for t in tiles {
        let id = zxy_to_tile_id(t.z, t.x, t.y);
        let (offset, length) = match seen.get(&t.data) {
            Some(&loc) => loc,
            None => {
                let loc = (tile_data.len() as u64, t.data.len() as u32);
                tile_data.extend_from_slice(&t.data);
                seen.insert(t.data, loc);
                loc
            }
        };
        // Consecutive ids with the same blob make one run.
        if let Some(last) = entries.last_mut()
            && last.offset == offset
            && last.tile_id + last.run_length as u64 == id
        {
            last.run_length += 1;
            continue;
        }
        entries.push(Entry { tile_id: id, offset, length, run_length: 1 });
    }
    let metadata = compress(metadata, COMPRESSION_GZIP)?;
    let (root, leaves) = build_directories(&entries)?;
    let root_dir_offset = HEADER_LEN as u64;
    let metadata_offset = root_dir_offset + root.len() as u64;
    let leaf_dirs_offset = metadata_offset + metadata.len() as u64;
    let tile_data_offset = leaf_dirs_offset + leaves.len() as u64;
    // Truncated, as GDAL writes them.
    let e7 = |deg: f64| (deg * 1e7) as i32;
    let header = Header {
        root_dir_offset,
        root_dir_length: root.len() as u64,
        metadata_offset,
        metadata_length: metadata.len() as u64,
        leaf_dirs_offset,
        leaf_dirs_length: leaves.len() as u64,
        tile_data_offset,
        tile_data_length: tile_data.len() as u64,
        num_addressed_tiles: addressed,
        num_tile_entries: entries.len() as u64,
        num_tile_contents: seen.len() as u64,
        clustered: true,
        internal_compression: COMPRESSION_GZIP,
        tile_compression: params.tile_compression,
        tile_type: params.tile_type,
        min_zoom: params.min_zoom,
        max_zoom: params.max_zoom,
        min_lon_e7: e7(params.bounds[0]),
        min_lat_e7: e7(params.bounds[1]),
        max_lon_e7: e7(params.bounds[2]),
        max_lat_e7: e7(params.bounds[3]),
        center_zoom: params.center.2,
        center_lon_e7: e7(params.center.0),
        center_lat_e7: e7(params.center.1),
    };
    let mut out = Vec::with_capacity(HEADER_LEN + root.len() + metadata.len() + leaves.len() + tile_data.len());
    out.extend_from_slice(&header.to_bytes());
    out.extend_from_slice(&root);
    out.extend_from_slice(&metadata);
    out.extend_from_slice(&leaves);
    out.extend_from_slice(&tile_data);
    Ok(out)
}

/// The gzipped root directory and the leaf directories section.
fn build_directories(entries: &[Entry]) -> Result<(Vec<u8>, Vec<u8>)> {
    let budget = PRELUDE_LEN - HEADER_LEN;
    let root = compress(&serialize_directory(entries), COMPRESSION_GZIP)?;
    if root.len() <= budget {
        return Ok((root, Vec::new()));
    }
    let mut chunk = 4096usize;
    loop {
        let mut leaves = Vec::new();
        let mut root_entries = Vec::with_capacity(entries.len() / chunk + 1);
        for group in entries.chunks(chunk) {
            let leaf = compress(&serialize_directory(group), COMPRESSION_GZIP)?;
            root_entries.push(Entry { tile_id: group[0].tile_id, offset: leaves.len() as u64, length: leaf.len() as u32, run_length: 0 });
            leaves.extend_from_slice(&leaf);
        }
        let root = compress(&serialize_directory(&root_entries), COMPRESSION_GZIP)?;
        if root.len() <= budget {
            return Ok((root, leaves));
        }
        chunk *= 2;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_ids_match_the_spec() {
        assert_eq!(zxy_to_tile_id(0, 0, 0), 0);
        assert_eq!(zxy_to_tile_id(1, 0, 0), 1);
        assert_eq!(zxy_to_tile_id(1, 0, 1), 2);
        assert_eq!(zxy_to_tile_id(1, 1, 1), 3);
        assert_eq!(zxy_to_tile_id(1, 1, 0), 4);
        assert_eq!(zxy_to_tile_id(2, 0, 0), 5);
        assert_eq!(zxy_to_tile_id(12, 3423, 1763), 19078479);
        for z in 0..8u8 {
            for x in 0..(1u32 << z) {
                for y in 0..(1u32 << z) {
                    assert_eq!(tile_id_to_zxy(zxy_to_tile_id(z, x, y)), (z, x, y));
                }
            }
        }
    }

    #[test]
    fn directories_and_headers_round_trip() {
        let entries = vec![
            Entry { tile_id: 0, offset: 0, length: 100, run_length: 1 },
            Entry { tile_id: 1, offset: 100, length: 50, run_length: 4 },
            Entry { tile_id: 10, offset: 0, length: 100, run_length: 1 },
            Entry { tile_id: 20, offset: 9_999, length: 77, run_length: 0 },
        ];
        assert_eq!(parse_directory(&serialize_directory(&entries)).unwrap(), entries);
        let h = Header { tile_type: TILE_MVT, max_zoom: 14, min_lon_e7: -1_800_000_000, center_lat_e7: -456, ..Default::default() };
        assert_eq!(Header::parse(&h.to_bytes()).unwrap(), h);
    }

    #[test]
    fn archives_with_leaf_directories_read_back() {
        // Enough distinct tiles at zoom 9 for the root directory to overflow into leaves.
        let mut tiles = Vec::new();
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        for x in 0..200u32 {
            for y in 0..200u32 {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                // Distinct contents of pseudo-random lengths keep the directory incompressible.
                let mut data = (x * 1000 + y).to_le_bytes().to_vec();
                data.resize(4 + (rng % 600) as usize, 0);
                tiles.push(OutTile { z: 9, x, y, data });
            }
        }
        let params = ArchiveParams {
            tile_type: TILE_PNG,
            tile_compression: COMPRESSION_NONE,
            min_zoom: 9,
            max_zoom: 9,
            bounds: [-180.0, -85.0, 180.0, 85.0],
            center: (0.0, 0.0, 9),
        };
        let bytes = write_archive(tiles, b"{}", &params).unwrap();
        let mut r = Reader::open(ByteSource::Memory(bytes)).unwrap();
        assert!(r.header.leaf_dirs_length > 0);
        assert_eq!(r.tile(9, 123, 45).unwrap().unwrap()[..4], (123u32 * 1000 + 45).to_le_bytes());
        assert_eq!(r.tile(9, 201, 45).unwrap(), None);
        assert_eq!(r.tiles_at_zoom(9).unwrap().len(), 40_000);
        assert!(r.tiles_at_zoom(8).unwrap().is_empty());
    }
}
