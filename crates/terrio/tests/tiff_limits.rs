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

/// A compressed strip over the limit is still refused: it is decoded whole.
#[test]
fn a_compressed_strip_over_the_limit_is_still_refused() {
    use terrio::raster::RasterData;
    let cfg = terrio::RasterConfig { cols: 64, rows: 48, bands: 1, data_type: terrio::DataType::U16, ..Default::default() };
    let ds = terrio::RasterDataset::new(terrio::Raster::from_data_native(cfg, RasterData::U16(vec![7; 64 * 48])).unwrap(), true);
    let options = terrio::RasterWriteOptions {
        compression: Some("DEFLATE".into()),
        creation_options: [("BLOCKYSIZE".to_string(), "48".to_string())].into_iter().collect(),
        ..Default::default()
    };
    let bytes = terrio::memory::write_raster_bytes(&ds, "strip.tif", &options).unwrap();
    let mut r = LazyRaster::from_tiff_bytes_with_limits(bytes, TiffReadLimits { chunk_bytes: 1280 }).unwrap().unwrap();
    let e = r.read_window(Window::new(0, 0, 1, 1), &band1()).unwrap_err();
    assert!(matches!(e, terrio::Error::DecodedLimitExceeded { limit: 1280, .. }), "{e}");
}

/// A big-endian ("MM") one-band TIFF of `cols` x `rows` samples of `bits` bits and
/// `SampleFormat` `format`, stored as one uncompressed strip holding `data`.
fn big_endian_strip(cols: u32, rows: u32, bits: u16, format: u16, data: &[u8]) -> Vec<u8> {
    // (tag, type, value): SHORT 3 (left-justified in the value field), LONG 4.
    let entries: [(u16, u16, u32); 9] = [
        (256, 4, cols),
        (257, 4, rows),
        (258, 3, u32::from(bits)),
        (259, 3, 1),
        (262, 3, 1),
        (273, 4, 0), // StripOffsets, set below
        (278, 4, rows),
        (279, 4, data.len() as u32),
        (339, 3, u32::from(format)),
    ];
    let data_offset = 8 + 2 + entries.len() * 12 + 4;
    let mut out = b"MM\0*".to_vec();
    out.extend_from_slice(&8u32.to_be_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_be_bytes());
    for (tag, typ, value) in entries {
        let value = if tag == 273 { data_offset as u32 } else { value };
        out.extend_from_slice(&tag.to_be_bytes());
        out.extend_from_slice(&typ.to_be_bytes());
        out.extend_from_slice(&1u32.to_be_bytes());
        if typ == 3 {
            out.extend_from_slice(&(value as u16).to_be_bytes());
            out.extend_from_slice(&[0, 0]);
        } else {
            out.extend_from_slice(&value.to_be_bytes());
        }
    }
    out.extend_from_slice(&[0; 4]);
    assert_eq!(out.len(), data_offset);
    out.extend_from_slice(data);
    out
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

/// A strip a sparse file leaves out (offset and byte count 0) reads as nodata beside strips over
/// the limit that are read by their rows: nothing is read for it, at offset 0 or anywhere.
#[test]
fn a_strip_left_out_reads_as_nodata_beside_strips_read_by_rows() {
    use terrio::raster::RasterData;
    // 64 x 48 UInt16 pixels of nodata 7 in strips of 16 rows (2,048 bytes); the middle strip, all
    // nodata, is left out.
    let (cols, rows) = (64, 48);
    let values: Vec<u16> = (0..cols * rows).map(|i| if (16..32).contains(&(i / cols)) { 7 } else { (i % 1000 + 8) as u16 }).collect();
    let cfg = terrio::RasterConfig { cols, rows, bands: 1, data_type: terrio::DataType::U16, nodata: 7.0, ..Default::default() };
    let ds = terrio::RasterDataset::new(terrio::Raster::from_data_native(cfg, RasterData::U16(values.clone())).unwrap(), true);
    let options = terrio::RasterWriteOptions {
        compression: Some("NONE".into()),
        creation_options: [("BLOCKYSIZE", "16"), ("SPARSE_OK", "YES")].into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        ..Default::default()
    };
    let bytes = terrio::memory::write_raster_bytes(&ds, "sparse.tif", &options).unwrap();
    // Ten rows (1,280 bytes) fit the limit; a strip does not.
    let mut r = LazyRaster::from_tiff_bytes_with_limits(bytes, TiffReadLimits { chunk_bytes: 10 * cols * 2 }).unwrap().unwrap();
    assert_eq!(r.tiff_block_range(1, 0, 1, None).unwrap(), Some((0, 0)));
    let v = &values;
    for w in [Window::new(3, 12, 20, 8), Window::new(0, 16, cols, 10), Window::new(5, 28, 30, 8)] {
        let expected: Vec<f64> =
            (w.y..w.y + w.height).flat_map(|y| (w.x..w.x + w.width).map(move |x| f64::from(v[y * cols + x]))).collect();
        assert_eq!(r.read_window(w, &band1()).unwrap().raster.data.to_f64_vec(), expected, "{w:?}");
    }
}
