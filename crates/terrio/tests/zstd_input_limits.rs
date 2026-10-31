//! Zstandard decoding limits: a small compressed chunk expanding far beyond the size its container
//! declares is decoded no further than that size (an error, or for GeoTIFF the declared chunk as
//! GDAL reads it), rather than allocated whole. Each stream below is 131 KB expanding to 4 GiB:
//! run under a memory cap, a decoder without the limit is stopped by it.
use std::path::{Path, PathBuf};
use terrio::{DataType, LazyRaster, Raster, RasterConfig, RasterDataset, RasterWriteOptions, Window, WindowOptions};

/// The largest block of a frame with a 128 KiB window.
const BLOCK: u32 = 128 * 1024;
/// Blocks of 128 KiB in 4 GiB.
const BLOCKS: usize = 32 * 1024;

/// A Zstandard frame of `blocks` RLE blocks of 128 KiB of `byte`: 6 + 4 x `blocks` bytes that
/// decode to `blocks` x 128 KiB (without a content size or a checksum).
fn bomb(byte: u8, blocks: usize) -> Vec<u8> {
    // The magic number, a header descriptor of 0 (a window descriptor, no content size), and a
    // 128 KiB window: exponent 7, 2^(10 + 7) bytes.
    let mut out = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, 7 << 3];
    for i in 0..blocks {
        // The block's size (an RLE block's regenerated size), type 1 (RLE) and last-block flag.
        let header = (BLOCK << 3) | (1 << 1) | u32::from(i + 1 == blocks);
        out.extend_from_slice(&header.to_le_bytes()[..3]);
        out.push(byte);
    }
    out
}

/// A little-endian TIFF of one 8-bit band of `w` x `h` pixels in one strip, `strip` compressed
/// with `compression`, with `extra` tags: LONG ones (LERC's parameters), or values replacing those
/// of a tag below (a bit depth, sample format or photometric interpretation).
fn strip_tiff(w: u32, h: u32, compression: u16, strip: &[u8], extra: &[(u16, &[u32])]) -> Vec<u8> {
    // (tag, type, values): SHORT 3, LONG 4. StripOffsets (273) is set below.
    let mut entries: Vec<(u16, u16, Vec<u32>)> = vec![
        (256, 4, vec![w]),
        (257, 4, vec![h]),
        (258, 3, vec![8]),
        (259, 3, vec![u32::from(compression)]),
        (262, 3, vec![1]),
        (273, 4, vec![0]),
        (277, 3, vec![1]),
        (278, 4, vec![h]),
        (279, 4, vec![strip.len() as u32]),
        (339, 3, vec![1]),
    ];
    for (tag, v) in extra {
        match entries.iter_mut().find(|e| e.0 == *tag) {
            Some(e) => e.2 = v.to_vec(),
            None => entries.push((*tag, 4, v.to_vec())),
        }
    }
    entries.sort_by_key(|e| e.0);
    let width = |typ: u16| if typ == 3 { 2 } else { 4 };
    // Values of more than 4 bytes follow the IFD, then the strip.
    let ifd_end = 8 + 2 + entries.len() * 12 + 4;
    let outside: usize = entries.iter().map(|(_, typ, v)| v.len() * width(*typ)).filter(|n| *n > 4).sum();
    let data = ifd_end + outside;
    let mut out = b"II*\0".to_vec();
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    let mut values = Vec::new();
    for (tag, typ, v) in &entries {
        let v = if *tag == 273 { vec![data as u32] } else { v.clone() };
        let mut bytes: Vec<u8> =
            v.iter().flat_map(|x| if *typ == 3 { (*x as u16).to_le_bytes().to_vec() } else { x.to_le_bytes().to_vec() }).collect();
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&typ.to_le_bytes());
        out.extend_from_slice(&(v.len() as u32).to_le_bytes());
        if bytes.len() > 4 {
            out.extend_from_slice(&((ifd_end + values.len()) as u32).to_le_bytes());
            values.extend_from_slice(&bytes);
        } else {
            bytes.resize(4, 0);
            out.extend_from_slice(&bytes);
        }
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&values);
    assert_eq!(out.len(), data);
    out.extend_from_slice(strip);
    out
}

fn band1() -> WindowOptions {
    WindowOptions { bands: vec![1], overview: None }
}

/// `bytes` written as `name` in `dir`.
fn file(dir: &Path, name: &str, bytes: &[u8]) -> String {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path.to_str().unwrap().to_string()
}

/// Whether every one of the `n` values of `ds`'s first band is `v`.
fn all(ds: &RasterDataset, n: usize, v: f64) -> bool {
    ds.raster.data.len() == n && (0..n).all(|i| ds.raster.data.get_f64(i) == v)
}

#[test]
fn the_test_stream_expands_as_described() {
    let small = bomb(9, 3);
    assert_eq!(small.len(), 6 + 4 * 3);
    let mut out = Vec::new();
    std::io::Read::read_to_end(&mut ruzstd::decoding::StreamingDecoder::new(&small[..]).unwrap(), &mut out).unwrap();
    assert_eq!((out.len(), out.iter().all(|b| *b == 9)), (3 * BLOCK as usize, true));
    assert_eq!(bomb(7, BLOCKS).len(), 6 + 4 * BLOCKS);
}

/// A ZSTD strip of 256 x 256 bytes (64 KiB declared) expanding to 4 GiB reads as its first
/// 64 KiB, the rest not decoded (libtiff ignores what a chunk decodes to past its size), lazily
/// and eagerly.
#[test]
fn a_zstd_geotiff_chunk_is_decoded_no_further_than_its_size() {
    let bytes = strip_tiff(256, 256, 50000, &bomb(7, BLOCKS), &[]);
    let mut r = LazyRaster::from_tiff_bytes(bytes.clone()).unwrap().unwrap();
    assert!(all(&r.read_window(Window::full(256, 256), &band1()).unwrap(), 256 * 256, 7.0));
    let dir = tempfile::tempdir().unwrap();
    let path = file(dir.path(), "zstd.tif", &bytes);
    assert!(all(&terrio::open_raster(&path).unwrap(), 256 * 256, 7.0));
}

/// ZSTD strips of 256 x 256 Float16 samples (128 KiB declared) and of WhiteIsZero bytes, which
/// terrio decodes as every ZSTD chunk, expanding to 4 GiB: read as their first 128 or 64 KiB (the
/// half-precision float of bytes 7 and 7, and 7 as stored).
#[test]
fn zstd_float16_and_white_is_zero_chunks_are_decoded_no_further_than_their_size() {
    let float16 = f64::from(half::f16::from_bits(0x0707).to_f32());
    for (extra, want) in [(&[(258, &[16][..]), (339, &[3])][..], float16), (&[(262, &[0])], 7.0)] {
        let bytes = strip_tiff(256, 256, 50000, &bomb(7, BLOCKS), extra);
        let mut r = LazyRaster::from_tiff_bytes(bytes.clone()).unwrap().unwrap();
        assert!(all(&r.read_window(Window::full(256, 256), &band1()).unwrap(), 256 * 256, want), "{extra:?}");
        let dir = tempfile::tempdir().unwrap();
        assert!(all(&terrio::open_raster(&file(dir.path(), "zstd.tif", &bytes)).unwrap(), 256 * 256, want), "{extra:?}");
    }
}

/// ZSTD strips of 256 x 256 complex samples, CInt16 and CFloat32 (256 and 512 KiB declared),
/// which terrio decodes as every ZSTD chunk, expanding to 4 GiB: read as their first 256 or
/// 512 KiB, each sample's real and imaginary parts two bands (Int16 parts of bytes 7 and 7,
/// Float32 parts of bytes 7, 7, 7 and 7).
#[test]
fn zstd_complex_chunks_are_decoded_no_further_than_their_size() {
    let both = WindowOptions { bands: vec![1, 2], overview: None };
    let cint16 = f64::from(i16::from_le_bytes([7; 2]));
    let cfloat32 = f64::from(f32::from_le_bytes([7; 4]));
    for (extra, want) in [(&[(258, &[32][..]), (339, &[5])][..], cint16), (&[(258, &[64]), (339, &[6])], cfloat32)] {
        let bytes = strip_tiff(256, 256, 50000, &bomb(7, BLOCKS), extra);
        let mut r = LazyRaster::from_tiff_bytes(bytes.clone()).unwrap().unwrap();
        let lazy = r.read_window(Window::full(256, 256), &both).unwrap();
        assert!(lazy.complex && all(&lazy, 2 * 256 * 256, want), "{extra:?}");
        let dir = tempfile::tempdir().unwrap();
        let eager = terrio::open_raster(&file(dir.path(), "zstd.tif", &bytes)).unwrap();
        assert!(eager.complex && all(&eager, 2 * 256 * 256, want), "{extra:?}");
    }
}

/// A LERC strip wrapped in Zstandard (LercParameters: version 4, Zstandard) expanding to 4 GiB:
/// an error past twice the chunk's 64 KiB and 4 KiB for each of its sample and mask.
#[test]
fn a_lerc_zstd_geotiff_chunk_is_an_error_past_its_size() {
    let bytes = strip_tiff(256, 256, 34887, &bomb(7, BLOCKS), &[(50674, &[4, 2])]);
    let want = "LERC: blobs of over 139264 bytes for a 256 x 256 chunk";
    let mut r = LazyRaster::from_tiff_bytes(bytes.clone()).unwrap().unwrap();
    let e = r.read_window(Window::full(256, 256), &band1()).unwrap_err().to_string();
    assert!(e.contains(want), "{e}");
    let dir = tempfile::tempdir().unwrap();
    let e = terrio::open_raster(&file(dir.path(), "lerc.tif", &bytes)).unwrap_err().to_string();
    assert!(e.contains(want), "{e}");
}

/// A 256 x 256 UInt8 raster of 3s.
fn raster() -> RasterDataset {
    let cfg = RasterConfig {
        cols: 256,
        rows: 256,
        bands: 1,
        x_min: 500000.0,
        y_min: 4597440.0,
        cell_size: 10.0,
        nodata: 0.0,
        data_type: DataType::U8,
        crs: wbraster::CrsInfo::from_epsg(32633),
        ..Default::default()
    };
    let data = wbraster::raster::RasterData::from_f64_vec(DataType::U8, vec![3.0; 256 * 256]);
    RasterDataset::new(Raster::from_data_native(cfg, data).unwrap(), true)
}

/// The files of a Zarr store besides its metadata and its X and Y coordinate arrays: the chunks of
/// its raster.
fn chunks(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        if path.is_dir() {
            if name != "X" && name != "Y" {
                out.extend(chunks(&path));
            }
        } else if ![".zarray", ".zattrs", ".zgroup", ".zmetadata", "zarr.json"].contains(&name.as_str()) {
            out.push(path);
        }
    }
    out
}

/// Zarr v2 and v3 stores of one 256 x 256 UInt8 chunk compressed with Zstandard, the chunk then
/// replaced by one expanding to 4 GiB: an error past the chunk's bytes and 64 bytes of slack,
/// eagerly and by window.
#[test]
fn a_zstd_zarr_chunk_is_an_error_past_its_size() {
    let dir = tempfile::tempdir().unwrap();
    // 65536 elements of 1 byte, and 64 bytes.
    let want = "Zarr: zstd: a chunk decompressing to more than 65600 bytes";
    for (name, format) in [("v2.zarr", "ZARR_V2"), ("v3.zarr", "ZARR_V3")] {
        let path = dir.path().join(name);
        let options = RasterWriteOptions {
            compression: Some("ZSTD".into()),
            creation_options: vec![("FORMAT".to_string(), format.to_string())],
            ..Default::default()
        };
        terrio::write_raster_dataset(&raster(), path.to_str().unwrap(), &options).unwrap();
        let path = path.to_str().unwrap();
        assert!(all(&terrio::open_raster(path).unwrap(), 256 * 256, 3.0));
        let chunks = chunks(Path::new(path));
        assert_eq!(chunks.len(), 1, "{name}: {chunks:?}");
        std::fs::write(&chunks[0], bomb(7, BLOCKS)).unwrap();
        let e = terrio::open_raster(path).unwrap_err().to_string();
        assert!(e.contains(want), "{name}: {e}");
        let mut r = terrio::open_raster_lazy(path).unwrap();
        let e = r.read_window(Window::full(256, 256), &band1()).unwrap_err().to_string();
        assert!(e.contains(want), "{name}: {e}");
    }
}

/// A `.zarr` directory that terrio's store detection declines (its `.zgroup` is a directory) but
/// `wbraster`'s reader takes (a multiscale level `0` whose 256 x 256 Zstandard chunk expands to
/// 4 GiB, which that reader decodes whole with `ruzstd` 0.8): read by terrio's driver, to the
/// limit of its chunks.
#[test]
fn a_zarr_path_never_reaches_the_wbraster_reader() {
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("level.zarr");
    std::fs::create_dir_all(store.join(".zgroup")).unwrap();
    std::fs::create_dir_all(store.join("0")).unwrap();
    std::fs::write(store.join(".zattrs"), r#"{"multiscales": [{"datasets": [{"path": "0"}]}]}"#).unwrap();
    let zarray = r#"{"zarr_format": 2, "shape": [256, 256], "chunks": [256, 256], "dtype": "|u1",
        "compressor": {"id": "zstd", "level": 1}, "fill_value": 0, "order": "C", "filters": null}"#;
    std::fs::write(store.join("0/.zarray"), zarray).unwrap();
    std::fs::write(store.join("0/0.0"), bomb(7, BLOCKS)).unwrap();
    let e = terrio::open_raster(store.to_str().unwrap()).unwrap_err().to_string();
    // terrio's driver reads the level, to the limit of its chunks.
    assert!(e.ends_with("level.zarr/0/0.0: Zarr: zstd: a chunk decompressing to more than 65600 bytes"), "{e}");
}
