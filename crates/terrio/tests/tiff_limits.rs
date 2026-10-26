//! Decoding limits of lazy GeoTIFF reads: a header declaring a chunk far larger than the file is
//! an error, not an allocation that aborts the process.

use terrio::sparse::{SparseFile, read_with};
use terrio::{LazyRaster, TiffReadLimits, Window, WindowOptions};

/// A one-band Float64 TIFF of `width` x `height` pixels in one uncompressed strip of `stored`
/// bytes (zeros) declaring `declared` bytes, little-endian, classic or BigTIFF.
fn tiff(width: u32, height: u32, declared: u32, stored: usize, big: bool) -> Vec<u8> {
    compressed_tiff(1, width, height, declared, stored, big)
}

/// [`tiff`] with the strip's `Compression` tag.
fn compressed_tiff(compression: u32, width: u32, height: u32, declared: u32, stored: usize, big: bool) -> Vec<u8> {
    // (tag, type, value): SHORT 3, LONG 4.
    let entries: [(u16, u16, u32); 9] = [
        (256, 4, width),
        (257, 4, height),
        (258, 3, 64),
        (259, 3, compression),
        (262, 3, 1),
        (273, 4, 0), // StripOffsets, set below
        (278, 4, height),
        (279, 4, declared),
        (339, 3, 3),
    ];
    let (header, entry, count, next) = if big { (16, 20, 8, 8) } else { (8, 12, 2, 4) };
    let data_offset = header + count + entries.len() * entry + next;
    let mut out = Vec::new();
    if big {
        out.extend_from_slice(b"II+\0");
        out.extend_from_slice(&8u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&16u64.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    } else {
        out.extend_from_slice(b"II*\0");
        out.extend_from_slice(&8u32.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    }
    for (tag, typ, value) in entries {
        let value = if tag == 273 { data_offset as u32 } else { value };
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&typ.to_le_bytes());
        if big {
            out.extend_from_slice(&1u64.to_le_bytes());
            let v = if typ == 3 { u64::from(value as u16) } else { u64::from(value) };
            out.extend_from_slice(&v.to_le_bytes());
        } else {
            out.extend_from_slice(&1u32.to_le_bytes());
            let v = if typ == 3 { u32::from(value as u16) } else { value };
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out.extend_from_slice(&vec![0u8; next]);
    assert_eq!(out.len(), data_offset);
    out.resize(data_offset + stored, 0);
    out
}

fn single_strip(width: u32, height: u32, stored: u32, big: bool) -> Vec<u8> {
    tiff(width, height, stored, stored as usize, big)
}

fn band1() -> WindowOptions {
    WindowOptions { bands: vec![1], overview: None }
}

#[test]
fn a_huge_declared_strip_is_an_error() {
    // 10^6 x 10^6 Float64 pixels (8 TB) in one strip of 128 bytes: a 4 x 4 window decodes the
    // strip, which allocated it whole.
    for big in [false, true] {
        let bytes = single_strip(1_000_000, 1_000_000, 128, big);
        let mut r = LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap();
        assert_eq!((r.width(), r.height()), (1_000_000, 1_000_000));
        let e = r.read_window(Window::new(0, 0, 4, 4), &band1()).unwrap_err();
        let limit = TiffReadLimits::default().chunk_bytes;
        assert!(matches!(e, terrio::Error::DecodedLimitExceeded { limit: l, .. } if l == limit), "{e}");
        assert!(e.to_string().contains(&limit.to_string()), "{e}");
    }
}

#[test]
fn chunks_of_terrios_own_codecs_have_the_limit_too() {
    // The same strip with LZW, which terrio decodes rather than the `tiff` crate: refused before
    // its bytes are read, with the limit set.
    let bytes = compressed_tiff(5, 1_000_000, 1_000_000, 128, 128, false);
    let mut r = LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap();
    let e = r.read_window(Window::new(0, 0, 4, 4), &band1()).unwrap_err();
    let limit = TiffReadLimits::default().chunk_bytes;
    assert!(matches!(e, terrio::Error::DecodedLimitExceeded { limit: l, .. } if l == limit), "{e}");
    // A valid 4 x 4 LZW strip reads at its own size as the limit.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("lzw.tif");
    let file = std::fs::File::create(&path).unwrap();
    let mut encoder = tiff::encoder::TiffEncoder::new(file).unwrap().with_compression(tiff::encoder::Compression::Lzw);
    encoder.write_image::<tiff::encoder::colortype::Gray64Float>(4, 4, &[1.5; 16]).unwrap();
    drop(encoder);
    let read = |chunk_bytes| {
        let mut r = LazyRaster::open_tiff_with_limits(path.to_str().unwrap(), TiffReadLimits { chunk_bytes }).unwrap().unwrap();
        r.read_window(Window::full(4, 4), &band1()).map(|ds| ds.raster.data.get_f64(0))
    };
    assert_eq!(read(128).unwrap(), 1.5);
    assert!(matches!(read(127), Err(terrio::Error::DecodedLimitExceeded { limit: 127, .. })));
}

#[test]
fn local_and_sparse_files_have_the_limits_too() {
    let bytes = single_strip(1_000_000, 1_000_000, 128, false);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("huge.tif");
    std::fs::write(&path, &bytes).unwrap();
    let mut r = terrio::open_raster_lazy(path.to_str().unwrap()).unwrap();
    assert!(r.read_window(Window::new(0, 0, 4, 4), &band1()).is_err());

    let file = SparseFile::new(bytes.len() as u64);
    let fetch = |o: u64, n: u64| Ok(bytes[o as usize..(o + n) as usize].to_vec());
    let mut r = read_with(&file, 8, fetch, || Ok(LazyRaster::from_sparse(file.clone())?.unwrap())).unwrap();
    assert!(read_with(&file, 8, fetch, || r.read_window(Window::new(0, 0, 4, 4), &band1())).is_err());
}

#[test]
fn limits_are_the_callers_to_set() {
    // A valid 4 x 4 Float64 strip: 128 bytes decoded.
    let bytes = single_strip(4, 4, 128, false);
    let read = |limits| {
        let mut r = LazyRaster::from_tiff_bytes_with_limits(bytes.clone(), limits).unwrap().unwrap();
        r.read_window(Window::full(4, 4), &band1()).map(|ds| ds.raster.data.len())
    };
    assert_eq!(read(TiffReadLimits { chunk_bytes: 128 }).unwrap(), 16);
    assert!(read(TiffReadLimits { chunk_bytes: 127 }).is_err());
    assert_eq!(read(TiffReadLimits::unlimited()).unwrap(), 16);
}

#[test]
fn stored_byte_counts_past_the_end_of_the_file_still_read() {
    // A 4 x 4 strip whose StripByteCounts claims 4 GiB: read as far as the file goes, as GDAL does.
    let bytes = tiff(4, 4, 0xFFFF_FFF0, 128, false);
    let mut r = LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap();
    assert_eq!(r.read_window(Window::full(4, 4), &band1()).unwrap().raster.data.len(), 16);
}

#[test]
fn a_window_too_large_to_allocate_is_an_error() {
    // (2^32 - 1)^2 Float64 pixels: more bytes than an allocation can hold.
    let mut r = LazyRaster::from_tiff_bytes(single_strip(u32::MAX, u32::MAX, 128, false)).unwrap().unwrap();
    let e = r.read_window(Window::full(r.width(), r.height()), &band1()).unwrap_err();
    assert!(e.to_string().contains("too large"), "{e}");
}

#[test]
fn oversized_tiled_windows_are_rejected_before_fetching_chunks() {
    // A single enormous tile has valid metadata, with its payload far beyond the
    // header blocks. Reject the output size before prefetching that payload.
    let entries: [(u16, u16, u32); 10] = [
        (256, 4, u32::MAX - 15),
        (257, 4, u32::MAX - 15),
        (258, 3, 64),
        (259, 3, 1),
        (262, 3, 1),
        (322, 4, u32::MAX - 15),
        (323, 4, u32::MAX - 15),
        (324, 4, 1 << 20),
        (325, 4, 2048),
        (339, 3, 3),
    ];
    let mut bytes = b"II*\0".to_vec();
    bytes.extend_from_slice(&8u32.to_le_bytes());
    bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for (tag, typ, value) in entries {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&typ.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.resize((1 << 20) + 2048, 0);
    let file = SparseFile::new(bytes.len() as u64);
    let fetch = |o: u64, n: u64| Ok(bytes[o as usize..(o + n) as usize].to_vec());
    let mut raster = read_with(&file, 8, fetch, || Ok(LazyRaster::from_sparse(file.clone())?.unwrap())).unwrap();
    let mut bytes_fetched = 0;
    let result = read_with(
        &file,
        8,
        |o, n| {
            bytes_fetched += n;
            fetch(o, n)
        },
        || raster.read_window(Window::full(raster.width(), raster.height()), &band1()),
    );
    assert!(matches!(result, Err(terrio::Error::InvalidArgument(ref message)) if message.contains("too large")));
    assert_eq!(bytes_fetched, 0);
}

#[test]
fn eager_reads_apply_selected_limits_to_reopened_decoders() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("strips.tif");
    let values: Vec<f64> = (0..32).map(f64::from).collect();
    let file = std::fs::File::create(&path).unwrap();
    let mut encoder = tiff::encoder::TiffEncoder::new(file).unwrap();
    let mut image = encoder.new_image::<tiff::encoder::colortype::Gray64Float>(4, 8).unwrap();
    image.rows_per_strip(2).unwrap();
    image.write_data(&values).unwrap();
    drop(encoder);
    // Four 64-byte strips exercise the decoder-reopen path under a two-thread pool.
    let pool = rayon::ThreadPoolBuilder::new().num_threads(2).build().unwrap();
    pool.install(|| {
        let path = path.to_str().unwrap();
        let read = |limit| terrio::open_raster_with_tiff_limits(path, TiffReadLimits { chunk_bytes: limit });
        assert!(matches!(read(63), Err(terrio::Error::DecodedLimitExceeded { limit: 63, .. })));
        for limit in [64, usize::MAX] {
            let result = read(limit).unwrap();
            assert_eq!(result.raster.data.len(), values.len());
            for (i, value) in values.iter().enumerate() {
                assert_eq!(result.raster.data.get_f64(i), *value);
            }
        }
        assert_eq!(terrio::open_raster(path).unwrap().raster.data.len(), values.len());
        assert!(matches!(terrio::open(path).unwrap(), terrio::Dataset::Raster(_)));
    });
}
