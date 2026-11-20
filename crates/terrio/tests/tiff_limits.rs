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
fn compressed_byte_counts_past_the_end_of_the_file_are_errors() {
    // GDAL reads a compressed strip's stored bytes whole: a Deflate strip whose StripByteCounts
    // claims 4 GiB fails there (TIFFReadEncodedStrip() failed), lazily and eagerly, while the same
    // strip with its true byte count reads.
    use std::io::Write;
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(&[0u8; 128]).unwrap();
    let stream = z.finish().unwrap();
    let file = |declared: u32| {
        let mut bytes = compressed_tiff(8, 4, 4, declared, stream.len(), false);
        let at = bytes.len() - stream.len();
        bytes[at..].copy_from_slice(&stream);
        bytes
    };
    let lazy = |bytes: Vec<u8>| LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap().read_window(Window::full(4, 4), &band1());
    assert_eq!(lazy(file(stream.len() as u32)).unwrap().raster.data.len(), 16);
    let e = lazy(file(0xFFFF_FFF0)).unwrap_err();
    assert!(e.to_string().contains("past the end of the file"), "{e}");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("deflate.tif");
    std::fs::write(&path, file(0xFFFF_FFF0)).unwrap();
    let e = terrio::open_raster(path.to_str().unwrap()).unwrap_err();
    assert!(e.to_string().contains("past the end of the file"), "{e}");
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
    // header blocks. Reject the output size before prefetching that payload. Tiles of more
    // than 2^31 - 1 pixels a side are refused at open, as GDAL refuses them.
    let side = i32::MAX as u32 - 15;
    let entries: [(u16, u16, u32); 10] = [
        (256, 4, side),
        (257, 4, side),
        (258, 3, 64),
        (259, 3, 1),
        (262, 3, 1),
        (322, 4, side),
        (323, 4, side),
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
    assert!(matches!(result, Err(terrio::Error::ResourceLimit(ref message)) if message.contains("too large")), "{result:?}");
    assert_eq!(bytes_fetched, 0);
}

// The strips are read under a two-thread pool of the test's own, as a caller may read them: the
// disallowed methods of clippy.toml are for terrio's own work, which runs on its own pool.
#[test]
#[allow(clippy::disallowed_methods)]
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

/// A little-endian classic TIFF of 256 x 256 pixels of `samples` samples per pixel, 8 bits, coded
/// with `compression`, in one tile of `tile` (width, height) whose bytes are 64 zeros.
fn tiled(samples: u16, compression: u16, tile: (u32, u32)) -> Vec<u8> {
    let entries: [(u16, u16, u32); 11] = [
        (256, 4, 256),
        (257, 4, 256),
        (258, 3, 8),
        (259, 3, u32::from(compression)),
        (262, 3, 1),
        (277, 3, u32::from(samples)),
        (284, 3, 1),
        (322, 4, tile.0),
        (323, 4, tile.1),
        (324, 4, 8 + 2 + 11 * 12 + 4),
        (325, 4, 64),
    ];
    let mut out = b"II*\0".to_vec();
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for (tag, typ, value) in entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&typ.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        let v = if typ == 3 { u32::from(value as u16) } else { value };
        out.extend_from_slice(&v.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    out.resize(out.len() + 64, 0);
    out
}

/// Tags GDAL refuses before it reads a band (`GTiffDataset::OpenOffset`) are refused at open, before
/// terrio does any work per band: tiles wider than 2^31 - 1 pixels ("Too large block size"), and a
/// compression no codec decodes ("Cannot open TIFF file due to missing codec"), here with 65535 and
/// 32768 samples per pixel, as in two files whose bands terrio walked for minutes where GDAL refused
/// them at once. A tiled file of these tags otherwise still opens. rasterio 1.5.0 on these files:
/// the same four errors, and the last two open.
#[test]
fn tags_gdal_refuses_at_open_are_refused_before_any_band_is_read() {
    let dir = tempfile::tempdir().unwrap();
    for (name, bytes, want) in [
        ("wide-tiles", tiled(65535, 8, (u32::MAX, 256)), Some("Too large block size: 4294967295 x 256")),
        ("tall-tiles", tiled(1, 8, (256, 1 << 31)), Some("Too large block size: 256 x 2147483648")),
        ("code-0", tiled(32768, 0, (256, 256)), Some("Cannot open TIFF file due to missing codec of code 0.")),
        ("jpeg-2000", tiled(1, 34712, (256, 256)), Some("Cannot open TIFF file due to missing codec JP2000.")),
        ("deflate", tiled(1, 8, (256, 256)), None),
        ("thunderscan", tiled(1, 32809, (256, 256)), None),
    ] {
        let path = dir.path().join(format!("{name}.tif"));
        std::fs::write(&path, &bytes).unwrap();
        let start = std::time::Instant::now();
        let opened = [
            LazyRaster::from_tiff_bytes(bytes).map(|r| r.map(|r| r.band_count())),
            LazyRaster::open(path.to_str().unwrap()).map(|r| Some(r.band_count())),
        ];
        for r in opened {
            match want {
                Some(want) => assert!(r.as_ref().is_err_and(|e| e.to_string().contains(want)), "{name}: {r:?}"),
                None => assert!(r.is_ok(), "{name}: {r:?}"),
            }
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(2), "{name}: {:?}", start.elapsed());
        if let Some(want) = want {
            let e = terrio::open_raster(path.to_str().unwrap()).map(|ds| ds.raster.bands).unwrap_err();
            assert!(e.to_string().contains(want), "{name}: {e}");
        }
    }
}

/// A little-endian classic TIFF of a 4 x 4 8-bit image and its 2 x 2 overview, one strip each,
/// then a transparency mask of the full resolution (`NewSubfileType` 4, 1 bit) in one tile of
/// `tile` (width, height) of which 8 bytes, all bits set, are stored. The overview has no mask of
/// its own: its mask is sampled from the full resolution's.
fn masked(tile: (u32, u32)) -> Vec<u8> {
    // (tag, type, value) of each image directory: SHORT 3, LONG 4; the data offsets set below.
    let ifds: [Vec<(u16, u16, u32)>; 3] = [
        vec![(256, 4, 4), (257, 4, 4), (258, 3, 8), (259, 3, 1), (262, 3, 1), (273, 4, 0), (277, 3, 1), (278, 4, 4), (279, 4, 16)],
        vec![
            (254, 4, 1),
            (256, 4, 2),
            (257, 4, 2),
            (258, 3, 8),
            (259, 3, 1),
            (262, 3, 1),
            (273, 4, 0),
            (277, 3, 1),
            (278, 4, 2),
            (279, 4, 4),
        ],
        vec![
            (254, 4, 4),
            (256, 4, 4),
            (257, 4, 4),
            (258, 3, 1),
            (259, 3, 1),
            (262, 3, 4),
            (277, 3, 1),
            (322, 4, tile.0),
            (323, 4, tile.1),
            (324, 4, 0),
            (325, 4, 8),
        ],
    ];
    let data: [Vec<u8>; 3] = [vec![7; 16], vec![7; 4], vec![0xFF; 8]];
    let mut at = 8 + ifds.iter().map(|e| 2 + 12 * e.len() + 4).sum::<usize>();
    let mut out = b"II*\0".to_vec();
    out.extend_from_slice(&8u32.to_le_bytes());
    for (i, (entries, bytes)) in ifds.iter().zip(&data).enumerate() {
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for &(tag, typ, value) in entries {
            let value = if matches!(tag, 273 | 324) { at as u32 } else { value };
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&typ.to_le_bytes());
            out.extend_from_slice(&1u32.to_le_bytes());
            let v = if typ == 3 { u32::from(value as u16) } else { value };
            out.extend_from_slice(&v.to_le_bytes());
        }
        let next = if i + 1 < ifds.len() { out.len() + 4 } else { 0 };
        out.extend_from_slice(&(next as u32).to_le_bytes());
        at += bytes.len();
    }
    for bytes in &data {
        out.extend_from_slice(bytes);
    }
    out
}

/// A mask chunk declaring more than the limit is an error before it is read, from both mask reads
/// of a TIFF's levels: the full resolution's own mask, and the full resolution's mask sampled for
/// an overview without one of its own. Here the mask is one 2^20 x 2^20 tile (128 GiB decoded) of
/// 8 stored bytes, which a read without the limit takes as it is; the image's chunks are within
/// the limit, and the same mask in a 16 x 16 tile reads.
#[test]
fn mask_chunks_over_the_limit_are_errors_at_every_level() {
    let limit = TiffReadLimits::default().chunk_bytes;
    let mut r = LazyRaster::from_tiff_bytes(masked((1 << 20, 1 << 20))).unwrap().unwrap();
    assert!(r.has_mask());
    for overview in [None, Some(0)] {
        let n = if overview.is_some() { 2 } else { 4 };
        let window = Window::new(0, 0, n, n);
        assert!(r.read_window(window, &WindowOptions { bands: vec![1], overview }).is_ok(), "{overview:?}");
        let e = r.read_mask(window, overview).unwrap_err();
        assert!(matches!(e, terrio::Error::DecodedLimitExceeded { limit: l, .. } if l == limit), "{overview:?}: {e}");
    }
    let mut r = LazyRaster::from_tiff_bytes(masked((16, 16))).unwrap().unwrap();
    assert_eq!(r.read_mask(Window::new(0, 0, 4, 4), None).unwrap(), Some(vec![255; 16]));
    assert_eq!(r.read_mask(Window::new(0, 0, 2, 2), Some(0)).unwrap(), Some(vec![255; 4]));
}

/// Runs `f` with `TERRIO_TIFF_MAX_CHUNK_BYTES` set to `value` for the calling thread only, so that
/// tests running at the same time keep the default.
fn with_chunk_option<T>(value: &str, f: impl FnOnce() -> T) -> T {
    terrio::config::push_scope([("TERRIO_TIFF_MAX_CHUNK_BYTES".to_string(), Some(value.to_string()))].into());
    struct Scope;
    impl Drop for Scope {
        fn drop(&mut self) {
            terrio::config::pop_scope();
        }
    }
    let _scope = Scope;
    f()
}

#[test]
fn the_chunk_option_sets_the_limit_of_readers_opened_without_limits() {
    // A valid 4 x 4 Float64 strip: 128 bytes decoded, read lazily by path, from bytes, from a
    // sparse file, and eagerly.
    let bytes = single_strip(4, 4, 128, false);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("strip.tif");
    std::fs::write(&path, &bytes).unwrap();
    let path = path.to_str().unwrap();
    let lazy = || terrio::open_raster_lazy(path)?.read_window(Window::full(4, 4), &band1()).map(|ds| ds.raster.data.len());
    let in_memory =
        || LazyRaster::from_tiff_bytes(bytes.clone())?.unwrap().read_window(Window::full(4, 4), &band1()).map(|ds| ds.raster.data.len());
    let sparse = || {
        let file = SparseFile::new(bytes.len() as u64);
        let fetch = |o: u64, n: u64| Ok(bytes[o as usize..(o + n) as usize].to_vec());
        let mut r = read_with(&file, 8, fetch, || Ok(LazyRaster::from_sparse(file.clone())?.unwrap()))?;
        read_with(&file, 8, fetch, || r.read_window(Window::full(4, 4), &band1())).map(|ds| ds.raster.data.len())
    };
    let eager = || terrio::open_raster(path).map(|ds| ds.raster.data.len());
    let reads: [(&str, &dyn Fn() -> terrio::Result<usize>); 4] =
        [("lazy", &lazy), ("in memory", &in_memory), ("sparse", &sparse), ("eager", &eager)];
    for (name, read) in reads {
        assert_eq!(read().unwrap(), 16, "{name}: the default");
        assert_eq!(with_chunk_option("128", read).unwrap(), 16, "{name}");
        let e = with_chunk_option(" 127 ", read).unwrap_err();
        assert!(matches!(e, terrio::Error::DecodedLimitExceeded { limit: 127, .. }), "{name}: {e}");
        // The refusal names the option that raises it.
        assert!(
            e.to_string().contains("limit 127; for a trusted file, raise it with the TERRIO_TIFF_MAX_CHUNK_BYTES option"),
            "{name}: {e}"
        );
    }
    // A raster opened under the option keeps its limit for later reads, outside the scope.
    let mut r = with_chunk_option("127", || terrio::open_raster_lazy(path)).unwrap();
    assert!(matches!(r.read_window(Window::full(4, 4), &band1()), Err(terrio::Error::DecodedLimitExceeded { limit: 127, .. })));
    // Explicit limits are not overridden by the option.
    let explicit = with_chunk_option("127", || {
        LazyRaster::from_tiff_bytes_with_limits(bytes.clone(), TiffReadLimits { chunk_bytes: 128 })
            .unwrap()
            .unwrap()
            .read_window(Window::full(4, 4), &band1())
            .map(|ds| ds.raster.data.len())
    });
    assert_eq!(explicit.unwrap(), 16);
}

#[test]
fn a_chunk_option_that_is_not_a_positive_integer_is_an_error() {
    let bytes = single_strip(4, 4, 128, false);
    for value in ["0", "-1", "256MB", "1.5"] {
        let e = with_chunk_option(value, || LazyRaster::from_tiff_bytes(bytes.clone()).map(|_| ())).unwrap_err();
        assert!(matches!(&e, terrio::Error::InvalidArgument(m) if m.contains("TERRIO_TIFF_MAX_CHUNK_BYTES")), "{value}: {e}");
    }
    // Empty or blank counts as unset, as for every option.
    assert!(with_chunk_option(" ", || LazyRaster::from_tiff_bytes(bytes.clone())).unwrap().is_some());
}

#[test]
fn non_tiff_probes_ignore_invalid_tiff_configuration() {
    let bytes = b"not a TIFF file".to_vec();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("other.bin");
    std::fs::write(&path, &bytes).unwrap();
    with_chunk_option("invalid", || {
        assert!(LazyRaster::open_tiff(path.to_str().unwrap()).unwrap().is_none());
        assert!(LazyRaster::from_tiff_bytes(bytes.clone()).unwrap().is_none());
        assert!(LazyRaster::from_tiff_shared_bytes(std::sync::Arc::new(bytes.clone())).unwrap().is_none());
        let file = SparseFile::new(bytes.len() as u64);
        let fetch = |o: u64, n: u64| Ok(bytes[o as usize..(o + n) as usize].to_vec());
        assert!(read_with(&file, 8, fetch, || LazyRaster::from_sparse(file.clone())).unwrap().is_none());
    });
}

/// A UInt16 raster of `bands` bands, each value its own, written as one uncompressed strip per
/// plane (pixel-interleaved, or one plane per band).
fn one_strip(cols: usize, rows: usize, bands: usize, interleave: &str) -> Vec<u8> {
    use terrio::raster::RasterData;
    let cfg = terrio::RasterConfig { cols, rows, bands, data_type: terrio::DataType::U16, ..Default::default() };
    let values = (0..bands * cols * rows).map(|i| (i * 7 % 65_521) as u16).collect();
    let ds = terrio::RasterDataset::new(terrio::Raster::from_data_native(cfg, RasterData::U16(values)).unwrap(), true);
    let creation = [("BLOCKYSIZE", rows.to_string()), ("INTERLEAVE", interleave.to_string())];
    let options = terrio::RasterWriteOptions {
        compression: Some("NONE".into()),
        creation_options: creation.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        ..Default::default()
    };
    terrio::memory::write_raster_bytes(&ds, "strip.tif", &options).unwrap()
}

/// An uncompressed strip over the limit is read by the rows a window needs: the limit bounds
/// those rows, not the strip (TIFF 6.0 allows an image stored as one strip; libtiff reads one
/// as smaller strips, `ChopUpSingleUncompressedStrip`).
#[test]
fn windows_of_an_uncompressed_strip_over_the_limit_read_by_rows() {
    let (cols, rows, bands) = (64, 48, 3);
    for interleave in ["PIXEL", "BAND"] {
        let bytes = one_strip(cols, rows, bands, interleave);
        let read = |limits, w: Window, bands: &[usize]| {
            let mut r = LazyRaster::from_tiff_bytes_with_limits(bytes.clone(), limits).unwrap().unwrap();
            assert_eq!(r.block_size(), Some((cols, rows)), "{interleave}: one strip");
            r.read_window(w, &WindowOptions { bands: bands.to_vec(), overview: None }).map(|ds| ds.raster.data.to_f64_vec())
        };
        // Ten rows of a plane's strip: 3,840 bytes pixel-interleaved, 1,280 per band's plane.
        let row_bytes = if interleave == "PIXEL" { cols * bands * 2 } else { cols * 2 };
        let limits = TiffReadLimits { chunk_bytes: 10 * row_bytes };
        for (w, selected) in [
            (Window::new(5, 7, 20, 9), &[3usize, 1][..]),
            (Window::new(0, 0, 1, 1), &[2]),
            (Window::new(0, 38, cols, 10), &[1, 2, 3]),
            (Window::new(63, 47, 1, 1), &[1]),
        ] {
            let expected = read(TiffReadLimits::unlimited(), w, selected).unwrap();
            assert_eq!(read(limits, w, selected).unwrap(), expected, "{interleave}: {w:?}, bands {selected:?}");
        }
        // The rows of a window are what a read holds: eleven are over the limit.
        let e = read(limits, Window::new(0, 0, 4, 11), &[1]).unwrap_err();
        assert!(matches!(e, terrio::Error::DecodedLimitExceeded { limit, .. } if limit == 10 * row_bytes), "{interleave}: {e}");
    }
}

/// A window taller than one 8 MiB part of rows reads the same pixels (the parts themselves are
/// not observable here).
#[test]
fn a_tall_window_of_an_uncompressed_strip_over_the_limit_reads_the_same_pixels() {
    // 4096 x 3000 UInt16 pixels: rows of 8,192 bytes, 2,400 of them (19.7 MB) under a 20 MB limit.
    let bytes = one_strip(4096, 3000, 1, "PIXEL");
    let w = Window::new(100, 300, 500, 2400);
    let read = |limits| {
        let mut r = LazyRaster::from_tiff_bytes_with_limits(bytes.clone(), limits).unwrap().unwrap();
        r.read_window(w, &band1()).unwrap().raster.data.to_f64_vec()
    };
    assert_eq!(read(TiffReadLimits { chunk_bytes: 20 << 20 }), read(TiffReadLimits::unlimited()));
}

/// A raster of `bands` bands of `data_type` (UInt16 or Float32), written by terrio as one strip per
/// plane under `compress` with `predictor` (pixel-interleaved, or one plane per band).
fn compressed_strip(
    (cols, rows, bands): (usize, usize, usize),
    data_type: terrio::DataType,
    (compress, predictor): (&str, u16),
    interleave: &str,
    overviews: Vec<usize>,
) -> Vec<u8> {
    use terrio::raster::RasterData;
    let n = bands * cols * rows;
    let data = match data_type {
        terrio::DataType::U16 => RasterData::U16((0..n).map(|i| (i * 7 % 65_521) as u16).collect()),
        terrio::DataType::F32 => RasterData::F32((0..n).map(|i| ((i % 977) as f32 - 400.0) * 0.37).collect()),
        other => panic!("{other:?}"),
    };
    let cfg = terrio::RasterConfig { cols, rows, bands, data_type, ..Default::default() };
    let ds = terrio::RasterDataset::new(terrio::Raster::from_data_native(cfg, data).unwrap(), true);
    let creation = [("BLOCKYSIZE", rows.to_string()), ("INTERLEAVE", interleave.to_string()), ("PREDICTOR", predictor.to_string())];
    let options = terrio::RasterWriteOptions {
        compression: Some(compress.into()),
        creation_options: creation.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        overviews,
        ..Default::default()
    };
    terrio::memory::write_raster_bytes(&ds, "strip.tif", &options).unwrap()
}

/// A TIFF, big-endian ("MM") or little-endian ("II"), of `cols` x `rows` pixels of `spp` (at
/// most 2) samples of `bits` bits and `SampleFormat` `format`, stored as one strip holding `data`
/// under `compression` with `predictor`.
fn strip_tiff(
    big: bool,
    (cols, rows, spp): (u32, u32, u16),
    (bits, format): (u16, u16),
    (compression, predictor): (u16, u16),
    data: &[u8],
) -> Vec<u8> {
    // (tag, SHORT values, or a LONG value)
    let samples = |v: u16| Ok(vec![v; usize::from(spp)]);
    let entries: [(u16, Result<Vec<u16>, u32>); 11] = [
        (256, Err(cols)),
        (257, Err(rows)),
        (258, samples(bits)),
        (259, Ok(vec![compression])),
        (262, Ok(vec![1])),
        (273, Err(0)), // StripOffsets, set below
        (277, Ok(vec![spp])),
        (278, Err(rows)),
        (279, Err(data.len() as u32)),
        (317, Ok(vec![predictor])),
        (339, samples(format)),
    ];
    let data_offset = 8 + 2 + entries.len() * 12 + 4;
    let short = |v: u16| if big { v.to_be_bytes() } else { v.to_le_bytes() };
    let long = |v: u32| if big { v.to_be_bytes() } else { v.to_le_bytes() };
    let mut out = if big { b"MM\0*".to_vec() } else { b"II*\0".to_vec() };
    out.extend(long(8));
    out.extend(short(entries.len() as u16));
    for (tag, value) in entries {
        out.extend(short(tag));
        match value {
            Ok(shorts) => {
                assert!(shorts.len() <= 2);
                out.extend(short(3));
                out.extend(long(shorts.len() as u32));
                let mut field: Vec<u8> = shorts.iter().flat_map(|v| short(*v)).collect();
                field.resize(4, 0);
                out.extend(field);
            }
            Err(value) => {
                out.extend(short(4));
                out.extend(long(1));
                out.extend(long(if tag == 273 { data_offset as u32 } else { value }));
            }
        }
    }
    out.extend([0; 4]);
    assert_eq!(out.len(), data_offset);
    out.extend_from_slice(data);
    out
}

/// A big-endian one-band TIFF of `cols` x `rows` samples of `bits` bits and `SampleFormat`
/// `format`, stored as one uncompressed strip holding `data`.
fn big_endian_strip(cols: u32, rows: u32, bits: u16, format: u16, data: &[u8]) -> Vec<u8> {
    strip_tiff(true, (cols, rows, 1), (bits, format), (1, 1), data)
}

/// `data` compressed as TIFF's `compression` stores it: Deflate (zlib), LZW (TIFF's), Zstandard, or
/// PackBits (runs of three bytes or more repeated, literal bytes otherwise).
fn encode(compression: u16, data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    match compression {
        8 => {
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            z.write_all(data).unwrap();
            z.finish().unwrap()
        }
        5 => weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8).encode(data).unwrap(),
        50000 => ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest),
        32773 => {
            let mut out = Vec::new();
            let mut i = 0;
            while i < data.len() {
                let run = data[i..].iter().take(128).take_while(|b| **b == data[i]).count();
                if run >= 3 {
                    out.extend([(1 - run as i32) as u8, data[i]]);
                    i += run;
                    continue;
                }
                let mut j = i + 1;
                while j < data.len() && j - i < 128 && !(j + 2 < data.len() && data[j] == data[j + 1] && data[j] == data[j + 2]) {
                    j += 1;
                }
                out.push((j - i - 1) as u8);
                out.extend_from_slice(&data[i..j]);
                i = j;
            }
            out
        }
        other => panic!("compression {other}"),
    }
}

/// Values that compress little: a multiplicative hash of their index.
fn scattered(n: usize) -> Vec<u16> {
    (0..n).map(|i| ((i as u32).wrapping_mul(2_654_435_761) >> 16) as u16).collect()
}

/// `cols` x 40 UInt16 pixels of nodata 7 (none of them 7) written by terrio under `compress` in
/// strips of 16 rows, the last of 8: the values, and the file.
fn strips_of_16_rows(cols: usize, compress: &str) -> (Vec<u16>, Vec<u8>) {
    use terrio::raster::RasterData;
    let rows = 40;
    let values: Vec<u16> = (0..cols * rows).map(|i| (i * 7 % 997 + 8) as u16).collect();
    let cfg = terrio::RasterConfig { cols, rows, bands: 1, data_type: terrio::DataType::U16, nodata: 7.0, ..Default::default() };
    let ds = terrio::RasterDataset::new(terrio::Raster::from_data_native(cfg, RasterData::U16(values.clone())).unwrap(), true);
    let options = terrio::RasterWriteOptions {
        compression: Some(compress.into()),
        creation_options: vec![("BLOCKYSIZE".into(), "16".into())],
        ..Default::default()
    };
    (values, terrio::memory::write_raster_bytes(&ds, "strips.tif", &options).unwrap())
}

/// `bytes`, a little-endian classic TIFF, with the `(offset, byte count)` of strip `strip` of its
/// first image replaced by what `f` gives for them: `(0, 0)` leaves it out, as a sparse file does.
fn with_strip(mut bytes: Vec<u8>, strip: usize, f: impl FnOnce(u32, u32) -> (u32, u32)) -> Vec<u8> {
    let at = |b: &[u8], o: usize, n: usize| b[o..o + n].iter().rev().fold(0usize, |v, x| v << 8 | usize::from(*x));
    assert_eq!(&bytes[..4], b"II*\0");
    let ifd = at(&bytes, 4, 4);
    // Where the strip's offset and byte count are, and their sizes.
    let mut fields = [(0, 0); 2];
    for e in 0..at(&bytes, ifd, 2) {
        let entry = ifd + 2 + e * 12;
        let k = match at(&bytes, entry, 2) {
            273 => 0,
            279 => 1,
            _ => continue,
        };
        let size = if at(&bytes, entry + 2, 2) == 3 { 2 } else { 4 };
        let values = if at(&bytes, entry + 4, 4) * size <= 4 { entry + 8 } else { at(&bytes, entry + 8, 4) };
        fields[k] = (values + strip * size, size);
    }
    let [(offset_at, offset_size), (count_at, count_size)] = fields;
    assert!(offset_size > 0 && count_size > 0);
    let (offset, count) = f(at(&bytes, offset_at, offset_size) as u32, at(&bytes, count_at, count_size) as u32);
    bytes[offset_at..offset_at + offset_size].copy_from_slice(&offset.to_le_bytes()[..offset_size]);
    bytes[count_at..count_at + count_size].copy_from_slice(&count.to_le_bytes()[..count_size]);
    bytes
}

/// A compressed strip over the limit is read by the rows a window needs, decoded from the start
/// of the strip up to the window's last row, with Deflate, LZW, Zstandard and PackBits, and with
/// horizontal differencing and the floating-point predictor: the values a whole decode gives,
/// pixel-interleaved and band-separate, at full resolution and in an overview stored as one strip.
/// So are PackBits runs that cross rows, and Zstandard frames one after another with a skippable
/// frame between them.
#[test]
fn windows_of_a_compressed_strip_over_the_limit_read_by_decoding_its_rows() {
    use terrio::DataType::{F32, U16};
    let (cols, rows, bands) = (64, 48, 3);
    let cases = [
        ("DEFLATE", U16, 1),
        ("DEFLATE", U16, 2),
        ("DEFLATE", F32, 3),
        ("LZW", U16, 1),
        ("LZW", U16, 2),
        ("LZW", F32, 3),
        ("ZSTD", U16, 1),
        ("ZSTD", U16, 2),
        ("ZSTD", F32, 3),
        ("PACKBITS", U16, 1),
        ("PACKBITS", F32, 1),
    ];
    for (compress, data_type, predictor) in cases {
        for interleave in ["PIXEL", "BAND"] {
            let case = format!("{compress}, {data_type:?}, predictor {predictor}, {interleave}");
            let bytes = compressed_strip((cols, rows, bands), data_type, (compress, predictor), interleave, vec![2]);
            let read = |limits, w: Window, bands: &[usize], overview| {
                let mut r = LazyRaster::from_tiff_bytes_with_limits(bytes.clone(), limits).unwrap().unwrap();
                assert_eq!(r.block_size(), Some((cols, rows)), "{case}: one strip");
                assert_eq!(r.overviews(), [(cols / 2, rows / 2)], "{case}");
                r.read_window(w, &WindowOptions { bands: bands.to_vec(), overview }).map(|ds| ds.raster.data.to_f64_vec())
            };
            // Ten rows of a plane's strip: 384 bytes a row pixel-interleaved, 128 a band's.
            let row_bytes = cols * data_type.size_bytes() * if interleave == "PIXEL" { bands } else { 1 };
            let limits = TiffReadLimits { chunk_bytes: 10 * row_bytes };
            for (w, selected, overview) in [
                (Window::new(0, 0, 1, 1), &[1usize][..], None),
                (Window::new(0, 0, cols, 4), &[1, 2, 3], None),
                (Window::new(5, 17, 20, 9), &[3, 1], None),
                (Window::new(0, 38, cols, 10), &[2, 3, 1], None),
                (Window::new(cols - 1, rows - 1, 1, 1), &[2], None),
                // The overview's rows are half as long: twenty fit the limit.
                (Window::new(3, 4, 20, 20), &[1, 3], Some(0)),
            ] {
                let expected = read(TiffReadLimits::unlimited(), w, selected, overview).unwrap();
                assert_eq!(read(limits, w, selected, overview).unwrap(), expected, "{case}: {w:?}, bands {selected:?}, {overview:?}");
            }
            // The strips, over the limit, are refused read whole, and so is a window of eleven rows.
            for (w, overview) in
                [(Window::full(cols, rows), None), (Window::full(cols / 2, rows / 2), Some(0)), (Window::new(0, 20, 4, 11), None)]
            {
                let e = read(limits, w, &[1], overview).unwrap_err();
                assert!(matches!(e, terrio::Error::DecodedLimitExceeded { limit, .. } if limit == 10 * row_bytes), "{case}: {w:?}: {e}");
            }
        }
    }
    // 50 x 40 bytes in runs of 37: PackBits runs cross rows, and parts of rows read or dropped.
    let (cols, rows) = (50usize, 40usize);
    let raw: Vec<u8> = (0..cols * rows).map(|i| (i / 37 % 5) as u8).collect();
    let packbits = strip_tiff(false, (cols as u32, rows as u32, 1), (8, 1), (32773, 1), &encode(32773, &raw));
    // 25 x 40 UInt16 pixels in two Zstandard frames, a skippable frame of 4 bytes between them.
    let half = raw.len() / 2;
    let mut zstd = encode(50000, &raw[..half]);
    zstd.extend([0x50, 0x2a, 0x4d, 0x18, 4, 0, 0, 0, 1, 2, 3, 4]);
    zstd.extend(encode(50000, &raw[half..]));
    let zstd = strip_tiff(false, (cols as u32 / 2, rows as u32, 1), (16, 1), (50000, 1), &zstd);
    for (bytes, w) in [
        (&packbits, Window::new(3, 7, 20, 9)),
        (&packbits, Window::new(0, 30, cols, 10)),
        (&zstd, Window::new(2, 15, 20, 10)),
        (&zstd, Window::new(0, 30, cols / 2, 10)),
    ] {
        let read = |limits| {
            let mut r = LazyRaster::from_tiff_bytes_with_limits(bytes.clone(), limits).unwrap().unwrap();
            r.read_window(w, &band1()).unwrap().raster.data.to_f64_vec()
        };
        assert_eq!(read(TiffReadLimits { chunk_bytes: 10 * cols }), read(TiffReadLimits::unlimited()), "{w:?}");
    }
}

/// A window over the last rows of a compressed strip, taller than a batch of rows (8 MiB), of
/// values that compress little: the rows before it are decoded and dropped through many parts of
/// stored bytes, then its rows are decoded in two batches, each with its values.
#[test]
fn a_tall_window_at_the_end_of_a_compressed_strip_reads_its_pixels() {
    use terrio::raster::RasterData;
    // 2048 x 2400 UInt16 pixels: rows of 4,096 bytes; the last 2,100 (8.6 MB) under a 9 MiB limit.
    let (cols, rows) = (2048, 2400);
    let values = scattered(cols * rows);
    let cfg = terrio::RasterConfig { cols, rows, bands: 1, data_type: terrio::DataType::U16, ..Default::default() };
    let ds = terrio::RasterDataset::new(terrio::Raster::from_data_native(cfg, RasterData::U16(values.clone())).unwrap(), true);
    let w = Window::new(100, 300, 1500, 2100);
    let expected: Vec<f64> =
        (300..rows).flat_map(|y| (100..1600).map(move |x| (x, y))).map(|(x, y)| f64::from(values[y * cols + x])).collect();
    for compress in ["DEFLATE", "LZW", "ZSTD", "PACKBITS"] {
        let options = terrio::RasterWriteOptions {
            compression: Some(compress.into()),
            creation_options: vec![("BLOCKYSIZE".into(), rows.to_string())],
            ..Default::default()
        };
        let bytes = terrio::memory::write_raster_bytes(&ds, "strip.tif", &options).unwrap();
        let mut r = LazyRaster::from_tiff_bytes_with_limits(bytes, TiffReadLimits { chunk_bytes: 9 << 20 }).unwrap().unwrap();
        assert_eq!(r.read_window(w, &band1()).unwrap().raster.data.to_f64_vec(), expected, "{compress}");
    }
}

/// The rows of a strip over the limit are read from a file whose bytes the caller fetches: the
/// stored bytes are fetched as decoding reaches them, a part at a time, for every compression
/// (Zstandard's decoder reads its input itself), so a window at the top of the strip fetches few.
#[test]
fn a_window_of_a_compressed_strip_over_the_limit_reads_from_a_sparse_file() {
    // 1024 x 2400 UInt16 pixels (4.9 MB) that compress little; a window of the last 50 rows.
    let (cols, rows) = (1024usize, 2400usize);
    let values = scattered(cols * rows);
    let raw: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    let w = Window::new(10, 2350, 1000, 50);
    let expected: Vec<f64> =
        (2350..rows).flat_map(|y| (10..1010).map(move |x| (x, y))).map(|(x, y)| f64::from(values[y * cols + x])).collect();
    for compression in [8u16, 5, 50000, 32773] {
        let bytes = strip_tiff(false, (cols as u32, rows as u32, 1), (16, 1), (compression, 1), &encode(compression, &raw));
        let file = SparseFile::new(bytes.len() as u64);
        let fetch = |o: u64, n: u64| Ok(bytes[o as usize..(o + n) as usize].to_vec());
        let limits = TiffReadLimits { chunk_bytes: 64 * cols * 2 };
        let mut r = read_with(&file, 64, fetch, || Ok(LazyRaster::from_sparse_with_limits(file.clone(), limits)?.unwrap())).unwrap();
        let (opened, _) = file.fetched();
        let top = read_with(&file, 64, fetch, || r.read_window(Window::new(0, 0, 4, 2), &band1())).unwrap();
        assert_eq!(top.raster.data.to_f64_vec(), [0, 1, 2, 3, cols, cols + 1, cols + 2, cols + 3].map(|i| f64::from(values[i])));
        let stored = (bytes.len() - 200) as u64;
        assert!(file.fetched().0 - opened < stored / 3, "compression {compression}: {:?} of {stored} stored bytes", file.fetched());
        let got = read_with(&file, 64, fetch, || r.read_window(w, &band1())).unwrap();
        assert_eq!(got.raster.data.to_f64_vec(), expected, "compression {compression}");
    }
}

/// Big-endian Deflate strips with horizontal differencing (Int16, two samples a pixel) and the
/// floating-point predictor (Float32, Float64): their rows decode to their values on the row path,
/// as a whole decode gives them.
#[test]
fn rows_of_big_endian_strips_with_predictors_keep_their_values() {
    let (cols, rows) = (9usize, 7usize);
    // Int16 pairs, differenced along each row, sample by sample.
    let ints: Vec<i16> = (0..cols * rows * 2).map(|i| ((i * 37 % 101) as i16 - 50) * 300).collect();
    let mut diffs = ints.clone();
    for row in diffs.chunks_exact_mut(cols * 2) {
        for i in (2..cols * 2).rev() {
            row[i] = row[i].wrapping_sub(row[i - 2]);
        }
    }
    let int_bytes: Vec<u8> = diffs.iter().flat_map(|v| v.to_be_bytes()).collect();
    // Floating-point samples: each row's bytes as planes, most significant first, differenced.
    fn planes<const N: usize>(rows: &[[u8; N]], cols: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for row in rows.chunks_exact(cols) {
            let mut p: Vec<u8> = (0..N).flat_map(|b| row.iter().map(move |v| v[b])).collect();
            for i in (1..p.len()).rev() {
                p[i] = p[i].wrapping_sub(p[i - 1]);
            }
            out.extend(p);
        }
        out
    }
    let f32s: Vec<f32> = (0..cols * rows).map(|i| (i as f32 - 30.0) * 1.375e3).collect();
    let f64s: Vec<f64> = (0..cols * rows).map(|i| (i as f64 - 25.0) * -7.25e-5).collect();
    let f32_planes = planes(&f32s.iter().map(|v| v.to_be_bytes()).collect::<Vec<_>>(), cols);
    let f64_planes = planes(&f64s.iter().map(|v| v.to_be_bytes()).collect::<Vec<_>>(), cols);
    // (samples a pixel, bits, SampleFormat, predictor, the strip's bytes, its values)
    let cases = [
        (2u16, 16u16, 2u16, 2u16, int_bytes, ints.iter().map(|v| f64::from(*v)).collect::<Vec<f64>>()),
        (1, 32, 3, 3, f32_planes, f32s.iter().map(|v| f64::from(*v)).collect()),
        (1, 64, 3, 3, f64_planes, f64s.clone()),
    ];
    for (spp, bits, format, predictor, data, values) in cases {
        let bytes = strip_tiff(true, (cols as u32, rows as u32, spp), (bits, format), (8, predictor), &encode(8, &data));
        let row_bytes = cols * usize::from(spp) * usize::from(bits / 8);
        let read = |limits, w: Window| {
            let mut r = LazyRaster::from_tiff_bytes_with_limits(bytes.clone(), limits).unwrap().unwrap();
            let bands: Vec<usize> = (1..=usize::from(spp)).collect();
            r.read_window(w, &WindowOptions { bands, overview: None }).unwrap().raster.data.to_f64_vec()
        };
        // Two rows fit the limit, the strip does not.
        let limits = TiffReadLimits { chunk_bytes: 2 * row_bytes };
        let w = Window::new(2, 4, 6, 2);
        let spp = usize::from(spp);
        let expected: Vec<f64> =
            (0..spp).flat_map(|s| (4..6).flat_map(move |y| (2..8).map(move |x| (y * cols + x) * spp + s))).map(|i| values[i]).collect();
        assert_eq!(read(limits, w), expected, "{bits}-bit samples, predictor {predictor}");
        assert_eq!(read(TiffReadLimits::unlimited(), w), expected, "{bits}-bit samples, predictor {predictor}: a whole decode");
    }
}

/// A compressed stream cut short, within the bytes the file holds: the windows before the cut read,
/// and one reaching past it is refused as invalid data, as a whole decode of the strip is. So is a
/// stream that is not valid where a window starts reading it (a Deflate stream's zlib header), and
/// a Deflate stream whose Adler-32 is wrong once a window reaches the strip's last row (the
/// Adler-32 in the next part of stored bytes read, after the samples), while one cut in its
/// Adler-32 reads, as whole. A file cut short after it was opened is an I/O error, whatever the
/// compression.
#[test]
fn a_compressed_strip_cut_short_is_invalid_where_a_window_reaches_the_cut() {
    // 256 x 600 UInt16 pixels (307 KB) that compress little, cut in their last eighth: Zstandard
    // codes them in three blocks, and its decoder hands out bytes once they leave its 128 KiB
    // window, so the cut is in the third.
    let (cols, rows) = (256usize, 600usize);
    let values = scattered(cols * rows);
    let raw: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    let limits = TiffReadLimits { chunk_bytes: 10 * cols * 2 };
    let read = |bytes: &[u8], limits, w: Window| {
        let mut r = LazyRaster::from_tiff_bytes_with_limits(bytes.to_vec(), limits).unwrap().unwrap();
        r.read_window(w, &band1()).map(|ds| ds.raster.data.to_f64_vec())
    };
    for compression in [8u16, 5, 50000, 32773] {
        let stream = encode(compression, &raw);
        let cut = strip_tiff(false, (cols as u32, rows as u32, 1), (16, 1), (compression, 1), &stream[..stream.len() * 7 / 8]);
        let top = read(&cut, limits, Window::new(0, 0, cols, 2)).unwrap();
        assert_eq!(top, values[..2 * cols].iter().map(|v| f64::from(*v)).collect::<Vec<_>>(), "compression {compression}");
        let e = read(&cut, limits, Window::new(0, rows - 5, 3, 5)).unwrap_err();
        assert!(matches!(e, terrio::Error::InvalidData(_)), "compression {compression}: {e}");
        let e = read(&cut, TiffReadLimits::unlimited(), Window::new(0, rows - 5, 3, 5)).unwrap_err();
        assert!(matches!(e, terrio::Error::InvalidData(_)), "compression {compression}, a whole decode: {e}");
    }
    let mut header = encode(8, &raw);
    header[0] = 0x79; // not Deflate: the zlib header's method must be 8
    let header = strip_tiff(false, (cols as u32, rows as u32, 1), (16, 1), (8, 1), &header);
    for limits in [limits, TiffReadLimits::unlimited()] {
        let e = read(&header, limits, Window::new(0, 0, 1, 1)).unwrap_err();
        assert!(matches!(e, terrio::Error::InvalidData(_)), "{limits:?}: {e}");
    }
    // One stored Deflate block of 809 x 81 bytes: the stream's first 65,536 bytes (the first part
    // read) end with the samples, its Adler-32 is in the next.
    let (width, height) = (809usize, 81usize);
    let data: Vec<u8> = (0..width * height).map(|i| (i % 251) as u8).collect();
    let (mut a, mut b) = (1u32, 0u32);
    for &x in &data {
        a = (a + u32::from(x)) % 65_521;
        b = (b + a) % 65_521;
    }
    let len = data.len() as u16;
    let mut stream = vec![0x78, 0x01, 0x01];
    stream.extend(len.to_le_bytes());
    stream.extend((!len).to_le_bytes());
    stream.extend(&data);
    assert_eq!(stream.len(), 65_536);
    stream.extend((b << 16 | a).to_be_bytes());
    let mut adler = stream.clone();
    *adler.last_mut().unwrap() ^= 1;
    let file = |stream: &[u8]| strip_tiff(false, (width as u32, height as u32, 1), (8, 1), (8, 1), stream);
    let (top, end) = (Window::new(0, 0, 3, 1), Window::new(0, height - 2, width, 2));
    let pixels =
        |w: Window| -> Vec<f64> { (w.y..w.y + w.height).flat_map(|y| data[y * width..][..w.width].to_vec()).map(f64::from).collect() };
    for limits in [TiffReadLimits { chunk_bytes: 10 * width }, TiffReadLimits::unlimited()] {
        assert_eq!(read(&file(&stream), limits, end).unwrap(), pixels(end), "{limits:?}");
        let e = read(&file(&adler), limits, end).unwrap_err();
        assert!(matches!(e, terrio::Error::InvalidData(_)), "{limits:?}: {e}");
        assert_eq!(read(&file(&stream[..stream.len() - 2]), limits, end).unwrap(), pixels(end), "{limits:?}");
    }
    assert_eq!(read(&file(&adler), TiffReadLimits { chunk_bytes: 10 * width }, top).unwrap(), pixels(top));
    let end = Window::new(cols - 3, rows - 1, 3, 1);
    let dir = tempfile::tempdir().unwrap();
    for compression in [8u16, 5, 50000, 32773] {
        let path = dir.path().join(format!("{compression}.tif"));
        std::fs::write(&path, strip_tiff(false, (cols as u32, rows as u32, 1), (16, 1), (compression, 1), &encode(compression, &raw)))
            .unwrap();
        let mut r = LazyRaster::open_tiff_with_limits(path.to_str().unwrap(), limits).unwrap().unwrap();
        // The first part of the stored bytes (64 KiB) is in the file, the next is not: Zstandard's
        // decoder reads past it itself.
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(70_000).unwrap();
        let e = r.read_window(end, &band1()).unwrap_err();
        assert!(matches!(e, terrio::Error::Io(_)), "compression {compression}: {e}");
    }
}

/// Deflate streams at their limits read by rows: the bound on what a stored byte can decode to
/// refuses no valid stream (a strip of zeros, compressed as far as Deflate goes, about 1,030 bytes
/// from one), and matches reaching back almost 32 KiB, the farthest Deflate refers, are decoded
/// across the parts the stream is inflated in (rows of 30,000 bytes, each the one before it).
#[test]
fn deflate_streams_at_their_limits_read_by_rows() {
    use std::io::Write;
    let deflate = |data: &[u8]| {
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        z.write_all(data).unwrap();
        z.finish().unwrap()
    };
    let (cols, rows) = (8192usize, 4096usize);
    let zeros = deflate(&vec![0u8; cols * rows]);
    assert!(zeros.len() * 1000 < cols * rows, "{} bytes for {}", zeros.len(), cols * rows);
    let row: Vec<u8> = scattered(15_000).iter().flat_map(|v| v.to_le_bytes()).collect();
    let repeated = deflate(&row.repeat(400));
    for (bytes, (cols, rows)) in [
        (strip_tiff(false, (cols as u32, rows as u32, 1), (8, 1), (8, 1), &zeros), (cols, rows)),
        (strip_tiff(false, (30_000, 400, 1), (8, 1), (8, 1), &repeated), (30_000, 400)),
    ] {
        let mut r = LazyRaster::from_tiff_bytes_with_limits(bytes, TiffReadLimits { chunk_bytes: 1 << 20 }).unwrap().unwrap();
        let got = r.read_window(Window::new(0, rows - 2, cols, 2), &band1()).unwrap().raster.data.to_f64_vec();
        let expected: Vec<f64> = if cols == 30_000 { row.repeat(2).iter().map(|v| f64::from(*v)).collect() } else { vec![0.0; 2 * cols] };
        assert!(got == expected, "{cols} x {rows}");
    }
}

/// A nearest-neighbour read smaller than an image stored as one strip over the limit reads the
/// rows it samples, the strip once and in order, compressed or not: the values it gives from a
/// whole decode. It holds a row at a time: more rows sampled than fit the limit are read too.
#[test]
fn a_decimated_read_of_a_strip_over_the_limit_reads_its_sampled_rows() {
    use terrio::ResampledReadOptions;
    let (cols, rows, bands) = (64, 48, 3);
    for (compress, interleave) in
        [("DEFLATE", "PIXEL"), ("ZSTD", "BAND"), ("LZW", "PIXEL"), ("PACKBITS", "BAND"), ("NONE", "PIXEL"), ("NONE", "BAND")]
    {
        let case = format!("{compress}, {interleave}");
        let bytes = compressed_strip((cols, rows, bands), terrio::DataType::U16, (compress, 1), interleave, Vec::new());
        let row_bytes = cols * 2 * if interleave == "PIXEL" { bands } else { 1 };
        let limits = TiffReadLimits { chunk_bytes: 10 * row_bytes };
        let open = |limits| LazyRaster::from_tiff_bytes_with_limits(bytes.clone(), limits).unwrap().unwrap();
        let resampled = |limits, size| {
            let options = ResampledReadOptions { size, bands: vec![3, 1], ..Default::default() };
            open(limits).read_resampled(&options).map(|ds| ds.raster.data.to_f64_vec())
        };
        assert_eq!(resampled(limits, (16, 8)).unwrap(), resampled(TiffReadLimits::unlimited(), (16, 8)).unwrap(), "{case}");
        // Pixels picked in any order, rows repeated.
        let (pick_cols, pick_rows) = ([63, 0, 5, 5], [47, 3, 3, 20, 0]);
        let options = WindowOptions { bands: vec![2, 3], overview: None };
        let got = open(limits).read_sampled(&pick_cols, &pick_rows, &options).unwrap().expect("read by rows").to_f64_vec();
        let whole = open(TiffReadLimits::unlimited()).read_window(Window::full(cols, rows), &options).unwrap().raster.data.to_f64_vec();
        let mut expected = Vec::new();
        for b in 0..2 {
            for y in pick_rows {
                expected.extend(pick_cols.iter().map(|x| whole[(b * rows + y) * cols + x]));
            }
        }
        assert_eq!(got, expected, "{case}");
        // Twelve rows sampled, more than fit the limit.
        assert_eq!(resampled(limits, (16, 12)).unwrap(), resampled(TiffReadLimits::unlimited(), (16, 12)).unwrap(), "{case}");
    }
}

/// A decimated read of strips over the limit holds one row at a time, and reads a strip left out
/// (offset and byte count 0, as a sparse file leaves it) as nodata: more rows sampled in a strip
/// than fit the limit are read, with the values of a whole read; a row over the limit is not read
/// by rows.
#[test]
fn a_decimated_read_holds_a_row_at_a_time_and_reads_a_strip_left_out_as_nodata() {
    use terrio::ResampledReadOptions;
    // Rows of 4,096 bytes: half a row, the limit that refuses one, still holds the file's tag
    // values (the limit bounds those too).
    let cols = 2048usize;
    for compress in ["DEFLATE", "LZW", "ZSTD", "PACKBITS", "NONE"] {
        let (values, bytes) = strips_of_16_rows(cols, compress);
        let bytes = with_strip(bytes, 1, |_, _| (0, 0));
        let open = |limits| LazyRaster::from_tiff_bytes_with_limits(bytes.clone(), limits).unwrap().unwrap();
        // Ten rows fit the limit, a strip of 16 does not: eleven rows are sampled in the first
        // strip, two in the one left out and one in the last.
        let limits = TiffReadLimits { chunk_bytes: 10 * cols * 2 };
        let (pick_cols, pick_rows) = ([63, 0, 9], [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 17, 30, 35]);
        let got = open(limits).read_sampled(&pick_cols, &pick_rows, &band1()).unwrap().expect("read by rows").to_f64_vec();
        let v = &values;
        let expected: Vec<f64> = pick_rows
            .iter()
            .flat_map(|&y| pick_cols.iter().map(move |&x| if (16..32).contains(&y) { 7.0 } else { f64::from(v[y * cols + x]) }))
            .collect();
        assert_eq!(got, expected, "{compress}");
        let resampled = |limits| {
            let options = ResampledReadOptions { size: (16, 20), bands: vec![1], ..Default::default() };
            open(limits).read_resampled(&options).map(|ds| ds.raster.data.to_f64_vec())
        };
        assert_eq!(resampled(limits).unwrap(), resampled(TiffReadLimits::unlimited()).unwrap(), "{compress}");
        let half_a_row = TiffReadLimits { chunk_bytes: cols };
        assert!(matches!(open(half_a_row).read_sampled(&pick_cols, &pick_rows, &band1()), Ok(None)), "{compress}");
    }
}

/// A strip whose bytes are more than 64 bits count (2^31 x 2^32 - 1 UInt64 pixels in one strip,
/// declared in a header of a few bytes) is not sampled by rows, rather than its size wrapping. No
/// limit: a row of 2^34 bytes fits it, so the strip's size is computed.
#[test]
fn a_strip_of_more_bytes_than_64_bits_count_is_not_sampled_by_rows() {
    let bytes = strip_tiff(false, (1 << 31, u32::MAX, 1), (64, 1), (1, 1), &[0; 16]);
    let mut r = LazyRaster::from_tiff_bytes_with_limits(bytes, TiffReadLimits::unlimited()).unwrap().unwrap();
    assert!(matches!(r.read_sampled(&[0, 1], &[0, 1], &band1()), Ok(None)));
}

/// The rows of a big-endian Int16 or Float32 strip over the limit are read as their native type,
/// with their values: the byte order and sample format are applied on the row path too.
#[test]
fn rows_of_a_big_endian_strip_over_the_limit_keep_their_type_and_values() {
    use terrio::raster::RasterData;
    let (cols, rows) = (8usize, 6usize);
    let ints: Vec<i16> = (0..cols * rows).map(|i| (i as i16 - 20) * 101).collect();
    let floats: Vec<f32> = (0..cols * rows).map(|i| i as f32 * -1.25).collect();
    let cases = [
        (16, 2, ints.iter().flat_map(|v| v.to_be_bytes()).collect::<Vec<u8>>(), ints.iter().map(|v| f64::from(*v)).collect::<Vec<f64>>()),
        (32, 3, floats.iter().flat_map(|v| v.to_be_bytes()).collect(), floats.iter().map(|v| f64::from(*v)).collect()),
    ];
    for (bits, format, data, values) in cases {
        let bytes = big_endian_strip(cols as u32, rows as u32, bits, format, &data);
        // Two rows fit the limit, the strip does not: the window's rows are read by themselves.
        let limits = TiffReadLimits { chunk_bytes: 2 * cols * usize::from(bits / 8) };
        let mut r = LazyRaster::from_tiff_bytes_with_limits(bytes, limits).unwrap().unwrap();
        let ds = r.read_window(Window::new(1, 2, 5, 2), &band1()).unwrap();
        match (&ds.raster.data, bits) {
            (RasterData::I16(_), 16) | (RasterData::F32(_), 32) => {}
            (other, _) => panic!("{bits}-bit samples read as {other:?}"),
        }
        let expected: Vec<f64> = (2..4).flat_map(|y| (1..6).map(move |x| (x, y))).map(|(x, y)| values[y * cols + x]).collect();
        assert_eq!(ds.raster.data.to_f64_vec(), expected, "{bits}-bit samples");
    }
}

/// A strip whose StripByteCounts runs past the end of the file is decoded whole, and so refused
/// over the limit, even when the file holds all of its pixels: the row path reads only strips
/// the file holds as declared.
#[test]
fn a_strip_declared_past_the_end_of_the_file_is_not_read_by_rows() {
    // 4 x 4 Float64 pixels: 128 bytes stored, 2,176 declared; rows of 32 bytes.
    let bytes = tiff(4, 4, 128 + 2048, 128, false);
    let mut r = LazyRaster::from_tiff_bytes_with_limits(bytes, TiffReadLimits { chunk_bytes: 64 }).unwrap().unwrap();
    let e = r.read_window(Window::new(0, 1, 4, 1), &band1()).unwrap_err();
    assert!(matches!(e, terrio::Error::DecodedLimitExceeded { limit: 64, .. }), "{e}");
}
