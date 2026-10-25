//! Public sparse PMTiles limits. Also runnable on wasm32-wasip1 with Wasmtime.
#![cfg(feature = "tiles")]

use std::io::Write;
use terrio::sparse::SparseFile;
use terrio::tiles::{PmTilesReadLimits, TileArchive};
use terrio::{Error, Result};

// Independently encode valid single-segment Zstandard frames using raw/RLE blocks.
// No native zstd dependency: these fixtures run in WebAssembly too.
fn encoded(data: &[u8], codec: u8) -> Vec<u8> {
    match codec {
        1 => data.to_vec(),
        2 => {
            let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            encoder.write_all(data).unwrap();
            encoder.finish().unwrap()
        }
        4 => {
            let mut out = vec![0x28, 0xb5, 0x2f, 0xfd, 0xa0];
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            let chunks: Vec<_> = data.chunks(128 * 1024).collect();
            for (i, block) in chunks.iter().enumerate() {
                let rle = block.iter().all(|b| *b == block[0]);
                let header = ((block.len() as u32) << 3) | (u32::from(rle) << 1) | u32::from(i + 1 == chunks.len());
                out.extend_from_slice(&header.to_le_bytes()[..3]);
                out.extend_from_slice(if rle { &block[..1] } else { block });
            }
            out
        }
        _ => unreachable!(),
    }
}

fn varint(mut n: usize, out: &mut Vec<u8>) {
    while n >= 128 {
        out.push((n as u8 & 127) | 128);
        n >>= 7;
    }
    out.push(n as u8);
}

fn directory(length: usize, leaf: bool) -> Vec<u8> {
    let mut out = vec![1, 0, u8::from(!leaf)]; // count, tile-id delta, run length
    varint(length, &mut out);
    out.push(1); // absolute offset zero, encoded plus one
    out
}

fn archive(codec: u8, tile: &[u8]) -> (Vec<u8>, PmTilesReadLimits) {
    let tile_raw = encoded(tile, codec);
    let leaf = directory(tile_raw.len(), false);
    let leaf_raw = encoded(&leaf, codec);
    let root = directory(leaf_raw.len(), true);
    let root_raw = encoded(&root, codec);
    let metadata = br#"{"name":"limits"}"#;
    let metadata_raw = encoded(metadata, codec);
    let mut bytes = vec![0; 127];
    bytes[..8].copy_from_slice(b"PMTiles\x03");
    let mut offset = 127usize;
    for (at, raw) in [(8, &root_raw), (24, &metadata_raw), (40, &leaf_raw), (56, &tile_raw)] {
        bytes[at..at + 8].copy_from_slice(&(offset as u64).to_le_bytes());
        bytes[at + 8..at + 16].copy_from_slice(&(raw.len() as u64).to_le_bytes());
        offset += raw.len();
        bytes.extend_from_slice(raw);
    }
    bytes[97] = codec;
    bytes[98] = codec;
    bytes[99] = 1;
    // A large unrequested tail ensures sparse tests cannot accidentally fetch the whole file.
    bytes.resize(bytes.len() + 256 * 1024, 0);
    (
        bytes,
        PmTilesReadLimits {
            root_directory: Some(root.len()),
            leaf_directory: Some(leaf.len()),
            metadata: Some(metadata.len()),
            tile: Some(tile.len()),
        },
    )
}

fn fill(file: &SparseFile, bytes: &[u8], fetched: &mut u64) {
    let missing = file.missing();
    assert!(!missing.is_empty());
    for (offset, len) in missing {
        *fetched += len;
        file.insert(offset, &bytes[offset as usize..(offset + len) as usize]);
    }
}

fn open(file: &SparseFile, bytes: &[u8], limits: PmTilesReadLimits, fetched: &mut u64) -> Result<TileArchive> {
    loop {
        let result = TileArchive::from_sparse_with_limits(file.clone(), limits);
        if !file.has_missing() {
            return result;
        }
        assert!(matches!(&result, Err(Error::Io(_))));
        fill(file, bytes, fetched);
    }
}

fn tile(archive: &mut TileArchive, file: &SparseFile, bytes: &[u8], fetched: &mut u64) -> Result<Option<Vec<u8>>> {
    loop {
        let result = archive.tile(0, 0, 0);
        if !file.has_missing() {
            return result;
        }
        assert!(matches!(&result, Err(Error::Io(_))));
        fill(file, bytes, fetched);
    }
}

#[test]
fn sparse_limits_cover_all_payloads_and_codecs() {
    for codec in [1, 2, 4] {
        let payload = vec![b'x'; 1024 * 1024];
        let (bytes, limits) = archive(codec, &payload);
        if codec != 1 {
            // The compressed payload is tiny compared with its decoded size.
            assert!(bytes.len() - 256 * 1024 < payload.len() / 100);
        }
        let file = SparseFile::with_block_size(bytes.len() as u64, 1024);
        let mut fetched = 0;
        let mut reader = open(&file, &bytes, limits, &mut fetched).unwrap();
        assert!(fetched < 20 * 1024, "open must not fetch tiles or the archive tail");
        assert_eq!(tile(&mut reader, &file, &bytes, &mut fetched).unwrap().unwrap(), payload);
        assert!(fetched < bytes.len() as u64 - 128 * 1024);
        // Each payload succeeds at its exact size above, and fails when one byte over budget.
        for (resource, limited) in [
            ("PMTiles root directory", PmTilesReadLimits { root_directory: limits.root_directory.map(|n| n - 1), ..limits }),
            ("PMTiles metadata", PmTilesReadLimits { metadata: limits.metadata.map(|n| n - 1), ..limits }),
            ("PMTiles leaf directory", PmTilesReadLimits { leaf_directory: limits.leaf_directory.map(|n| n - 1), ..limits }),
            ("PMTiles tile", PmTilesReadLimits { tile: limits.tile.map(|n| n - 1), ..limits }),
        ] {
            let result = open(&file, &bytes, limited, &mut fetched).and_then(|mut reader| tile(&mut reader, &file, &bytes, &mut fetched));
            assert!(matches!(result, Err(Error::DecodedLimitExceeded { resource: got, .. }) if got == resource), "{resource}: {result:?}");
        }
        let mut unlimited = TileArchive::from_sparse(file.clone()).unwrap();
        assert_eq!(unlimited.tile(0, 0, 0).unwrap().unwrap(), payload);
        let mut zero = TileArchive::from_sparse_with_limits(file, PmTilesReadLimits { tile: Some(0), ..limits }).unwrap();
        assert!(matches!(zero.tile(0, 0, 0), Err(Error::DecodedLimitExceeded { limit: 0, .. })));
    }
}

#[test]
fn overflowing_archive_offsets_are_invalid_data() {
    for section in [8, 24, 40, 56] {
        let (mut bytes, _) = archive(1, b"tile");
        let read_u64 = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap()) as usize;
        let root = read_u64(8);
        let leaf = read_u64(40);
        if section == 40 {
            // Root leaf pointer has relative offset one; MAX + 1 must be refused.
            bytes[root + 4] = 2;
        } else if section == 56 {
            // Tile pointer in the leaf has relative offset one.
            bytes[leaf + 4] = 2;
        }
        bytes[section..section + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        let file = SparseFile::new(bytes.len() as u64);
        file.insert(0, &bytes);
        let result = TileArchive::from_sparse_with_limits(file, PmTilesReadLimits::default()).and_then(|mut a| a.tile(0, 0, 0));
        assert!(matches!(result, Err(Error::InvalidData(_))), "section {section}: {result:?}");
    }
}
