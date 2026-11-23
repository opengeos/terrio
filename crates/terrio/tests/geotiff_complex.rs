//! Complex GeoTIFF samples (`SampleFormat` 5 and 6: GDAL's `CInt16`, `CInt32`, `CFloat32` and
//! `CFloat64`), read as GDAL reads them: each band is its real parts then its imaginary parts,
//! of the parts' type. The files are built here byte by byte, as TIFF 6.0 lays them out, in both
//! byte orders: libtiff swaps each part of a big-endian complex sample on its own.

use std::io::Write;
use terrio::{DataType, LazyRaster, RasterWriteOptions, Window, WindowOptions};

/// The bytes of one part of a sample in a byte order.
fn part(value: f64, data_type: DataType, big_endian: bool) -> Vec<u8> {
    macro_rules! bytes {
        ($v:expr) => {
            if big_endian { $v.to_be_bytes().to_vec() } else { $v.to_le_bytes().to_vec() }
        };
    }
    match data_type {
        DataType::I16 => bytes!(value as i16),
        DataType::I32 => bytes!(value as i32),
        DataType::F32 => bytes!(value as f32),
        DataType::F64 => bytes!(value),
        t => panic!("not a complex part type: {t:?}"),
    }
}

/// A TIFF of `w` x `h` pixels of `bands` complex samples, pixel-interleaved in one strip or
/// band-interleaved in a strip per band (`planar`), uncompressed or Deflate-compressed
/// (`deflate`): pixel `i` of band `b` is `re(b, i) + im(b, i) j`. `format` and `bits` are the
/// `SampleFormat` and `BitsPerSample` written; `predictor` a `Predictor` tag to add (its
/// differences are not applied).
fn complex_tiff(
    big_endian: bool,
    (w, h, bands, planar): (u32, u32, u16, bool),
    (format, bits, parts): (u16, u16, DataType),
    deflate: bool,
    predictor: Option<u16>,
) -> Vec<u8> {
    let u16b = |v: u16| if big_endian { v.to_be_bytes() } else { v.to_le_bytes() };
    let u32b = |v: u32| if big_endian { v.to_be_bytes() } else { v.to_le_bytes() };
    let sample = |b: u16, i: u32| [part(re(b, i), parts, big_endian), part(im(b, i), parts, big_endian)].concat();
    let strips: Vec<Vec<u8>> = if planar {
        (0..bands).map(|b| (0..w * h).flat_map(|i| sample(b, i)).collect()).collect()
    } else {
        vec![(0..w * h).flat_map(|i| (0..bands).flat_map(move |b| sample(b, i))).collect()]
    };
    let strips: Vec<Vec<u8>> = strips
        .into_iter()
        .map(|strip| {
            if !deflate {
                return strip;
            }
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            z.write_all(&strip).unwrap();
            z.finish().unwrap()
        })
        .collect();
    let mut offsets = Vec::new();
    let mut data = Vec::new();
    for strip in &strips {
        offsets.push(8 + data.len() as u32);
        data.extend(strip);
    }
    let counts: Vec<u32> = strips.iter().map(|s| s.len() as u32).collect();
    let mut out = if big_endian { b"MM\0*".to_vec() } else { b"II*\0".to_vec() };
    let shorts_at = 8 + data.len() as u32;
    // Per-sample tags of more than two values do not fit an entry: they go after the data.
    let per_sample = |v: u16| -> Vec<u8> { (0..bands).flat_map(|_| u16b(v)).collect() };
    let (bits_values, format_values) = (per_sample(bits), per_sample(format));
    let longs = |v: &[u32]| -> Vec<u8> { v.iter().flat_map(|x| u32b(*x)).collect() };
    let (offset_values, count_values) = (longs(&offsets), longs(&counts));
    let longs_at = shorts_at + 2 * (bits_values.len() as u32);
    let ifd = longs_at + 2 * (offset_values.len() as u32);
    out.extend(u32b(ifd));
    out.extend(&data);
    out.extend(&bits_values);
    out.extend(&format_values);
    out.extend(&offset_values);
    out.extend(&count_values);
    let inline_or_at = |values: &[u8], at: u32| -> [u8; 4] {
        if values.len() <= 4 {
            let mut v = [0u8; 4];
            v[..values.len()].copy_from_slice(values);
            v
        } else {
            u32b(at)
        }
    };
    let short = |v: u16| {
        let b = u16b(v);
        [b[0], b[1], 0, 0]
    };
    let mut entries: Vec<(u16, u16, u32, [u8; 4])> = vec![
        (256, 4, 1, u32b(w)),
        (257, 4, 1, u32b(h)),
        (258, 3, u32::from(bands), inline_or_at(&bits_values, shorts_at)),
        (259, 3, 1, short(if deflate { 8 } else { 1 })),
        (262, 3, 1, short(1)),
        (273, 4, offsets.len() as u32, inline_or_at(&offset_values, longs_at)),
        (277, 3, 1, short(bands)),
        (278, 4, 1, u32b(h)),
        (279, 4, counts.len() as u32, inline_or_at(&count_values, longs_at + offset_values.len() as u32)),
        (284, 3, 1, short(if planar { 2 } else { 1 })),
        (339, 3, u32::from(bands), inline_or_at(&format_values, shorts_at + bits_values.len() as u32)),
    ];
    if let Some(p) = predictor {
        entries.push((317, 3, 1, short(p)));
    }
    entries.sort_by_key(|e| e.0);
    out.extend(u16b(entries.len() as u16));
    for (tag, typ, count, value) in entries {
        out.extend(u16b(tag));
        out.extend(u16b(typ));
        out.extend(u32b(count));
        out.extend(value);
    }
    out.extend(u32b(0));
    out
}

/// The real part of pixel `i` of band `b`: negative and positive values, within Int16.
fn re(b: u16, i: u32) -> f64 {
    f64::from(i as i32 * 3 - 50 + i32::from(b) * 1000)
}

/// The imaginary part.
fn im(b: u16, i: u32) -> f64 {
    f64::from(77 - i as i32 * 5 - i32::from(b) * 500)
}

/// GDAL's complex types: (SampleFormat, BitsPerSample, part type, GDAL's name).
const TYPES: [(u16, u16, DataType, &str); 4] = [
    (5, 32, DataType::I16, "CInt16"),
    (5, 64, DataType::I32, "CInt32"),
    (6, 64, DataType::F32, "CFloat32"),
    (6, 128, DataType::F64, "CFloat64"),
];

#[test]
fn complex_samples_read_as_real_and_imaginary_bands() {
    let (w, h) = (5u32, 3u32);
    for big_endian in [false, true] {
        for (format, bits, parts, name) in TYPES {
            for (bands, planar, deflate) in [(1u16, false, false), (3, false, false), (3, false, true), (3, true, true)] {
                let what = format!("{name}, {bands} band(s), planar {planar}, big-endian {big_endian}, deflate {deflate}");
                let bytes = complex_tiff(big_endian, (w, h, bands, planar), (format, bits, parts), deflate, None);
                let mut r = LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap();
                assert!(r.is_complex(), "{what}");
                assert_eq!((r.band_count(), r.data_type()), (2 * usize::from(bands), parts), "{what}");
                assert_eq!(r.header().data_type_name(), name, "{what}");
                assert_eq!(r.header().band_count(), usize::from(bands), "{what}");
                let ds = r.read().unwrap();
                assert!(ds.complex, "{what}");
                let v = ds.raster.data.to_f64_vec();
                let n = (w * h) as usize;
                for b in 0..bands {
                    let (real, imaginary) = (&v[2 * usize::from(b) * n..][..n], &v[(2 * usize::from(b) + 1) * n..][..n]);
                    let want_re: Vec<f64> = (0..w * h).map(|i| re(b, i)).collect();
                    let want_im: Vec<f64> = (0..w * h).map(|i| im(b, i)).collect();
                    assert_eq!((real, imaginary), (&want_re[..], &want_im[..]), "{what}, band {b}");
                }
                // A window of the second band's imaginary parts alone.
                if bands == 3 {
                    let win = r.read_window(Window::new(1, 1, 3, 2), &WindowOptions { bands: vec![4], overview: None }).unwrap();
                    let want: Vec<f64> = [(1, 1), (2, 1), (3, 1), (1, 2), (2, 2), (3, 2)].iter().map(|(x, y)| im(1, y * w + x)).collect();
                    assert_eq!(win.raster.data.to_f64_vec(), want, "{what}");
                    assert!(!win.complex, "a part alone is not a complex band: {what}");
                }
            }
        }
    }
}

#[test]
fn complex_files_read_whole_and_summarized() {
    let dir = tempfile::tempdir().unwrap();
    for (format, bits, parts, name) in TYPES {
        let path = dir.path().join(format!("{name}.tif"));
        std::fs::write(&path, complex_tiff(false, (4, 2, 2, true), (format, bits, parts), true, None)).unwrap();
        let path = path.to_str().unwrap();
        // Read whole, by the reader of whole files.
        let ds = terrio::open_raster(path).unwrap();
        assert!(ds.complex, "{name}");
        assert_eq!((ds.raster.bands, ds.band_count(), ds.data_type_name()), (4, 2, name.to_string()));
        let v = ds.raster.data.to_f64_vec();
        assert_eq!((v[0], v[8], v[16], v[24]), (re(0, 0), im(0, 0), re(1, 0), im(1, 0)), "{name}");
        // Summarized from the headers, and from the pixels.
        for statistics in [false, true] {
            let options = terrio::info::InfoOptions { statistics, ..Default::default() };
            let terrio::info::Info::Raster(info) = terrio::info::info_path(path, &options).unwrap() else { panic!("not a raster") };
            assert_eq!(info.band_count, 2, "{name}");
            assert!(info.bands.iter().all(|b| b.data_type == name), "{name}");
            if statistics {
                // A complex band's statistics are those of its real parts, as GDAL's.
                let s = info.bands[1].statistics.unwrap();
                let real: Vec<f64> = (0..8).map(|i| re(1, i)).collect();
                assert_eq!((s.min, s.max), (real[0], real[7]), "{name}");
            }
        }
    }
}

#[test]
fn complex_geotiffs_are_copied_as_complex() {
    let dir = tempfile::tempdir().unwrap();
    for (format, bits, parts, name) in TYPES {
        let mut r = LazyRaster::from_tiff_bytes(complex_tiff(true, (7, 5, 2, false), (format, bits, parts), false, None)).unwrap().unwrap();
        let whole = r.read().unwrap();
        for driver in [terrio::formats::RasterDriver::GTiff, terrio::formats::RasterDriver::Cog] {
            let out = dir.path().join(format!("{name}-{driver:?}.tif"));
            let options = RasterWriteOptions { driver: Some(driver), ..Default::default() };
            r.write(out.to_str().unwrap(), &options).unwrap();
            let mut copy = LazyRaster::open(out.to_str().unwrap()).unwrap();
            assert!(copy.is_complex(), "{name} {driver:?}");
            assert_eq!(copy.read().unwrap().raster.data.to_f64_vec(), whole.raster.data.to_f64_vec(), "{name} {driver:?}");
        }
    }
}

/// GDAL 3.13.3 preserves complex sample types in streamed path conversions.
#[test]
fn complex_path_copies_keep_pairs_across_block_rows() {
    let dir = tempfile::tempdir().unwrap();
    for (format, bits, parts, name) in TYPES {
        let source = dir.path().join(format!("source-{name}.tif"));
        std::fs::write(&source, complex_tiff(false, (23, 37, 2, false), (format, bits, parts), false, None)).unwrap();
        let mut expected = LazyRaster::open(source.to_str().unwrap()).unwrap().read().unwrap();
        expected.band_metadata = ["first", "second"]
            .iter()
            .map(|name| terrio::BandMetadata { description: Some((*name).into()), ..Default::default() })
            .collect();
        let translated = terrio::ops::translate::translate(&expected, &Default::default()).unwrap();
        assert!(translated.complex, "in-memory translation {name}");
        assert_eq!(translated.band_metadata, expected.band_metadata);
        assert_eq!(translated.raster.data.to_f64_vec(), expected.raster.data.to_f64_vec());
        for component in [2, 3] {
            let options = terrio::ops::translate::TranslateOptions { bands: vec![component], ..Default::default() };
            let partial = terrio::ops::translate::translate(&expected, &options).unwrap();
            assert!(!partial.complex);
            assert_eq!(partial.band_metadata, [expected.band_metadata[(component - 1) / 2].clone()]);
            let plane = expected.raster.cols * expected.raster.rows;
            assert_eq!(partial.raster.data.to_f64_vec(), expected.raster.data.to_f64_vec()[(component - 1) * plane..component * plane]);
        }

        for interleave in ["PIXEL", "BAND"] {
            let target = dir.path().join(format!("copy-{name}-{interleave}.tif"));
            let options = RasterWriteOptions {
                driver: Some(terrio::formats::RasterDriver::GTiff),
                tile_size: Some(16),
                compression: Some("DEFLATE".into()),
                creation_options: vec![("INTERLEAVE".into(), interleave.into())],
                ..Default::default()
            };
            LazyRaster::open(source.to_str().unwrap()).unwrap().write(target.to_str().unwrap(), &options).unwrap();
            let actual = LazyRaster::open(target.to_str().unwrap()).unwrap().read().unwrap();
            assert!(actual.complex, "{name} {interleave}");
            assert_eq!(actual.raster.data_type, parts);
            assert_eq!(actual.band_count(), 2);
            assert_eq!(actual.raster.data.to_f64_vec(), expected.raster.data.to_f64_vec());
            let translated = dir.path().join(format!("translated-{name}-{interleave}.tif"));
            terrio::ops::translate::translate_path_to_file(
                source.to_str().unwrap(),
                &Default::default(),
                terrio::ops::overview::OverviewSelection::None,
                translated.to_str().unwrap(),
                &options,
            )
            .unwrap();
            let actual = LazyRaster::open(translated.to_str().unwrap()).unwrap().read().unwrap();
            assert!(actual.complex, "path translation {name} {interleave}");
            assert_eq!(actual.band_count(), 2);
            assert_eq!(actual.raster.data.to_f64_vec(), expected.raster.data.to_f64_vec());
        }
    }
}

#[test]
fn complex_samples_with_a_predictor_are_refused() {
    for (format, bits, parts, name) in TYPES {
        // libtiff's horizontal predictor adds a complex sample as one word of its whole width.
        let mut r =
            LazyRaster::from_tiff_bytes(complex_tiff(false, (4, 2, 1, false), (format, bits, parts), true, Some(2))).unwrap().unwrap();
        let error = r.read().unwrap_err().to_string();
        assert!(error.contains("predictor 2"), "{name}: {error}");
        // Uncompressed samples have no predictor in libtiff: the tag is not applied.
        let mut r =
            LazyRaster::from_tiff_bytes(complex_tiff(false, (4, 2, 1, false), (format, bits, parts), false, Some(2))).unwrap().unwrap();
        let v = r.read().unwrap().raster.data.to_f64_vec();
        assert_eq!((v[1], v[9]), (re(0, 1), im(0, 1)), "{name}");
    }
}

/// The issue #1222 fixture: two `CFloat32` bands of 64 x 64 pixels, `x + y j` and
/// `100 + 2x + (200 + 3y) j`, DEFLATE-compressed in 16-pixel tiles.
fn two_band_cfloat32(path: &std::path::Path) {
    let n = 64usize;
    let plane = |f: &dyn Fn(f32, f32) -> f32| -> Vec<f32> { (0..n * n).map(|i| f((i % n) as f32, (i / n) as f32)).collect() };
    let values = [plane(&|x, _| x), plane(&|_, y| y), plane(&|x, _| 100.0 + 2.0 * x), plane(&|_, y| 200.0 + 3.0 * y)].concat();
    let config = terrio::RasterConfig { cols: n, rows: n, bands: 4, data_type: DataType::F32, ..Default::default() };
    let raster = terrio::Raster::from_data_native(config, terrio::raster::RasterData::F32(values)).unwrap();
    let mut dataset = terrio::RasterDataset::new(raster, false);
    dataset.complex = true;
    let options = RasterWriteOptions { tile_size: Some(16), compression: Some("DEFLATE".into()), ..Default::default() };
    terrio::write_raster_dataset(&dataset, path.to_str().unwrap(), &options).unwrap();
}

/// GDAL 3.13.2's `gdaladdo -r average` averages the real and the imaginary parts of a complex
/// band on their own (`GDALResampleChunkC32R`): overview pixel `(c, r)` of the fixture is
/// `2c + 0.5 + (2r + 0.5) j` and `101 + 4c + (201.5 + 6r) j`.
#[test]
fn complex_average_overviews_average_each_part() {
    use terrio::ops::Resampling;
    use terrio::ops::overview::{OverviewOptions, RefreshOptions, add_overviews, refresh_overviews};
    let dir = tempfile::tempdir().unwrap();
    // The real and imaginary parts of each band at overview column `c`, row `r`.
    let parts: [fn(f64, f64) -> f64; 4] = [|c, _| 2.0 * c + 0.5, |_, r| 2.0 * r + 0.5, |c, _| 101.0 + 4.0 * c, |_, r| 201.5 + 6.0 * r];
    let expected: Vec<f64> = parts.iter().flat_map(|f| (0..32 * 32).map(move |i| f((i % 32) as f64, (i / 32) as f64))).collect();
    for external in [true, false] {
        let path = dir.path().join(format!("complex-{external}.tif"));
        two_band_cfloat32(&path);
        let path = path.to_str().unwrap();
        let options = OverviewOptions {
            external,
            levels: vec![2],
            resampling: Resampling::Average,
            compression: Some("DEFLATE".into()),
            ..Default::default()
        };
        assert_eq!(add_overviews(path, &options).unwrap(), vec![(32, 32)]);
        let mut r = LazyRaster::open(path).unwrap();
        assert_eq!(r.overviews(), vec![(32, 32)]);
        let level = r.read_window(Window::new(0, 0, 32, 32), &WindowOptions { bands: vec![1, 2, 3, 4], overview: Some(0) }).unwrap();
        assert!(level.complex, "external {external}");
        assert_eq!(level.raster.data_type, DataType::F32);
        assert_eq!(level.raster.data.to_f64_vec(), expected, "external {external}");
        if external {
            // The sidecar holds one CFloat32 image of the two complex bands.
            let mut ovr = LazyRaster::open(&format!("{path}.ovr")).unwrap();
            assert!(ovr.is_complex());
            assert_eq!((ovr.width(), ovr.height(), ovr.band_count()), (32, 32, 4));
            assert_eq!(ovr.read().unwrap().raster.data.to_f64_vec(), expected);
        }
        // A whole refresh recomputes the level with its stored average; a partial one is refused.
        let gt = r.geo_transform();
        drop(r);
        let refresh = RefreshOptions { external, ..Default::default() };
        assert_eq!(refresh_overviews(path, &refresh).unwrap(), vec![(32, 32)]);
        let mut r = LazyRaster::open(path).unwrap();
        let level = r.read_window(Window::new(0, 0, 32, 32), &WindowOptions { bands: vec![1, 2, 3, 4], overview: Some(0) }).unwrap();
        assert_eq!(level.raster.data.to_f64_vec(), expected, "refreshed, external {external}");
        let bbox = [gt[0], gt[3] + 8.0 * gt[5], gt[0] + 8.0 * gt[1], gt[3]];
        let error = refresh_overviews(path, &RefreshOptions { bbox: vec![bbox], ..refresh }).unwrap_err();
        assert!(error.to_string().contains("partial refreshes of complex"), "{error}");
    }
}

/// GDAL computes only nearest, average, average of magnitude and phase and RMS overviews of
/// complex bands; terrio's minimum, maximum and mode are refused for them.
#[test]
fn complex_overviews_refuse_other_methods() {
    use terrio::ops::Resampling;
    use terrio::ops::overview::{OverviewOptions, add_overviews};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("complex.tif");
    two_band_cfloat32(&path);
    for resampling in [Resampling::Min, Resampling::Max, Resampling::Mode] {
        let options = OverviewOptions { external: true, levels: vec![2], resampling, ..Default::default() };
        let error = add_overviews(path.to_str().unwrap(), &options).unwrap_err();
        assert!(error.to_string().contains("overviews of complex bands are not supported"), "{resampling:?}: {error}");
    }
    assert!(!std::path::Path::new(&format!("{}.ovr", path.display())).exists());
    let options = OverviewOptions { external: true, levels: vec![2], resampling: Resampling::Nearest, ..Default::default() };
    add_overviews(path.to_str().unwrap(), &options).unwrap();
    let mut r = LazyRaster::open(path.to_str().unwrap()).unwrap();
    let first = r.read_window(Window::new(1, 1, 1, 1), &WindowOptions { bands: vec![1, 2, 3, 4], overview: Some(0) }).unwrap();
    assert_eq!(first.raster.data.to_f64_vec(), vec![2.0, 2.0, 104.0, 206.0]);
}
