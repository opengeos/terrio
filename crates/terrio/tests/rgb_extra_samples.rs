//! Pixel-interleaved RGB GeoTIFFs with more samples than the color ones (#208): an unspecified
//! extra sample, as `gdalwarp` writes NAIP's red, green, blue and near-infrared, or several, with
//! or without an alpha. Every band reads back as written, in whole, lazy, windowed and overview
//! reads, whatever the compression, layout and sample type.

use terrio::raster::RasterData;
use terrio::{DataType, RasterWriteOptions, Window, WindowOptions};

const COLS: usize = 67;
const ROWS: usize = 45;

/// The value of band `b` at pixel `i`: different in every band.
fn value(b: usize, i: usize) -> f64 {
    ((i * (b + 1) + b * 37) % 251) as f64
}

fn raster(bands: usize, t: DataType) -> terrio::RasterDataset {
    let n = COLS * ROWS * bands;
    let v = |j: usize| value(j / (COLS * ROWS), j % (COLS * ROWS));
    let data = match t {
        DataType::U8 => RasterData::U8((0..n).map(|j| v(j) as u8).collect()),
        DataType::U16 => RasterData::U16((0..n).map(|j| (v(j) * 200.0 + 3.0) as u16).collect()),
        _ => RasterData::F32((0..n).map(|j| (v(j) * 0.5 - 7.0) as f32).collect()),
    };
    let cfg = terrio::RasterConfig { cols: COLS, rows: ROWS, bands, data_type: t, nodata: -9999.0, ..Default::default() };
    let mut r = terrio::Raster::from_data_native(cfg, data).unwrap();
    r.x_min = 0.0;
    r.y_min = 0.0;
    r.cell_size_x = 1.0;
    r.cell_size_y = 1.0;
    terrio::RasterDataset::new(r, true)
}

/// The values of every band, band after band.
fn values(ds: &terrio::RasterDataset) -> Vec<f64> {
    (0..ds.raster.bands).flat_map(|b| ds.raster.band_view(b).as_slice().to_vec()).collect()
}

/// Writes `ds` as a GeoTIFF with overviews, pixel- or band-interleaved.
fn write(ds: &terrio::RasterDataset, path: &str, compression: &str, tiled: bool, alpha: bool, pixel: bool) {
    let mut co = vec![("INTERLEAVE".to_string(), if pixel { "PIXEL" } else { "BAND" }.to_string())];
    co.push(("PHOTOMETRIC".into(), "RGB".into()));
    if alpha {
        co.push(("ALPHA".into(), "YES".into()));
    }
    if tiled {
        co.extend([("TILED".into(), "YES".into()), ("BLOCKXSIZE".into(), "16".into()), ("BLOCKYSIZE".into(), "16".into())]);
    } else {
        co.push(("BLOCKYSIZE".into(), "5".into()));
    }
    if matches!(compression, "DEFLATE" | "ZSTD" | "LZW") {
        co.push(("PREDICTOR".into(), "2".into()));
    }
    let options = RasterWriteOptions {
        compression: Some(compression.into()),
        creation_options: co,
        overviews: vec![2, 4],
        // Average, not nearest: with nearest, GDAL builds a band-interleaved file's overviews band
        // by band, each from the full resolution, and a pixel-interleaved one's each from the
        // previous level, so their x4 levels differ (GDAL 3.12.1: 476 of 816 pixels); with
        // average both cascade and agree, so the overviews read can be compared.
        overview_resampling: Some("average".into()),
        ..Default::default()
    };
    terrio::write_raster_dataset(ds, path, &options).unwrap();
}

#[test]
fn rgb_with_extra_samples_reads_as_written() {
    let dir = tempfile::tempdir().unwrap();
    for (bands, alpha) in [(4, false), (5, false), (6, false), (5, true)] {
        for t in [DataType::U8, DataType::U16, DataType::F32] {
            let ds = raster(bands, t);
            let expected = values(&ds);
            for compression in ["NONE", "DEFLATE", "PACKBITS", "ZSTD", "LZW"] {
                for tiled in [false, true] {
                    let case = format!("{bands} bands (alpha {alpha}) {t:?} {compression} tiled {tiled}");
                    let path = dir.path().join("pixel.tif");
                    let path = path.to_str().unwrap();
                    write(&ds, path, compression, tiled, alpha, true);
                    assert_eq!(values(&terrio::open_raster(path).unwrap()), expected, "whole read, {case}");
                    let bytes = std::fs::read(path).unwrap();
                    assert_eq!(values(&terrio::memory::open_raster_bytes(bytes.clone(), "p.tif").unwrap()), expected, "{case}");

                    let mut lazy = terrio::open_raster_lazy(path).unwrap();
                    assert_eq!(values(&lazy.read().unwrap()), expected, "lazy read, {case}");
                    let w = Window { x: 13, y: 7, width: 40, height: 30 };
                    let window: Vec<f64> = (0..bands)
                        .flat_map(|b| (7..37).flat_map(move |y| (13..53).map(move |x| (b, y * COLS + x))))
                        .map(|(b, i)| expected[b * COLS * ROWS + i])
                        .collect();
                    assert_eq!(values(&lazy.read_window(w, &WindowOptions::default()).unwrap()), window, "window, {case}");
                    let one = WindowOptions { bands: vec![bands], overview: None };
                    let last = &expected[(bands - 1) * COLS * ROWS..];
                    let full = Window { x: 0, y: 0, width: COLS, height: ROWS };
                    assert_eq!(values(&lazy.read_window(full, &one).unwrap()), last, "last band, {case}");

                    // Overviews, against those of the same pixels written band-interleaved.
                    let reference = dir.path().join("band.tif");
                    let reference = reference.to_str().unwrap();
                    write(&ds, reference, compression, tiled, alpha, false);
                    let mut by_band = terrio::open_raster_lazy(reference).unwrap();
                    assert_eq!(lazy.overviews().len(), 2, "{case}");
                    for (i, (w, h)) in lazy.overviews().into_iter().enumerate() {
                        let o = WindowOptions { bands: vec![], overview: Some(i) };
                        let win = Window { x: 0, y: 0, width: w, height: h };
                        let got = values(&lazy.read_window(win, &o).unwrap());
                        assert_eq!(got, values(&by_band.read_window(win, &o).unwrap()), "overview {i}, {case}");
                    }
                    let mut remote = terrio::memory::open_raster_lazy_bytes(bytes, "p.tif").unwrap();
                    assert_eq!(values(&remote.read_window(w, &WindowOptions::default()).unwrap()), window, "in memory, {case}");
                }
            }
        }
    }
}

/// An RGBA GeoTIFF's bands 1-3 have the alpha band as their mask band (GDAL 3.12's
/// `GMF_ALPHA`): `read_band_mask` gives the alpha values, read from the same level. The alpha
/// band's own mask is all valid, and so is every band's of a raster without alpha or of five bands
/// with one. A nodata value the bands' type cannot hold (-9999 for Byte) leaves the alpha mask,
/// while one it can hold masks its band instead.
#[test]
fn an_rgba_geotiffs_band_masks_come_from_its_alpha_band() {
    let dir = tempfile::tempdir().unwrap();
    let (cols, rows) = (16, 12);
    let n = cols * rows;
    let alpha: Vec<u8> = (0..n).map(|i| if i % 3 == 0 { 0 } else { (i % 250 + 5) as u8 }).collect();
    let rgba = |nodata: f64| {
        let mut data = Vec::with_capacity(4 * n);
        for b in 0..3 {
            for i in 0..n {
                data.push(((i + b * 31) % 251) as u8);
            }
        }
        data.extend_from_slice(&alpha);
        let cfg = terrio::RasterConfig { cols, rows, bands: 4, data_type: DataType::U8, nodata, ..Default::default() };
        let mut r = terrio::Raster::from_data_native(cfg, RasterData::U8(data)).unwrap();
        r.x_min = 0.0;
        r.y_min = 0.0;
        r.cell_size_x = 1.0;
        r.cell_size_y = 1.0;
        terrio::RasterDataset::new(r, true)
    };
    let path = dir.path().join("rgba.tif").to_str().unwrap().to_string();
    write(&rgba(-9999.0), &path, "NONE", false, true, true);

    let mut lazy = terrio::open_raster_lazy(&path).unwrap();
    assert_eq!(lazy.band_count(), 4);
    assert_eq!(lazy.alpha_band(), Some(4));
    assert_eq!(lazy.band_nodata(1), Some(-9999.0), "a nodata value Byte cannot hold");
    assert!(lazy.read_mask(Window::full(cols, rows), None).unwrap().is_none(), "no per-dataset mask");
    for band in [1, 2, 3] {
        assert_eq!(lazy.read_band_mask(band, Window::full(cols, rows), None).unwrap(), alpha, "band {band}");
    }
    assert_eq!(lazy.read_band_mask(4, Window::full(cols, rows), None).unwrap(), vec![255; n], "the alpha band's own mask is all valid");
    let w = Window { x: 2, y: 3, width: 5, height: 4 };
    let win_expected: Vec<u8> = (3..7).flat_map(|y| (2..7).map(move |x| (y, x))).map(|(y, x)| alpha[y * cols + x]).collect();
    assert_eq!(lazy.read_band_mask(1, w, None).unwrap(), win_expected, "a window");
    // An overview's mask is the same overview's alpha band.
    for level in 0..lazy.overviews().len() {
        let (ow, oh) = lazy.overviews()[level];
        let full = Window::full(ow, oh);
        let level_alpha = lazy.read_window(full, &WindowOptions { bands: vec![4], overview: Some(level) }).unwrap().raster.data;
        let expected: Vec<u8> = level_alpha.iter_f64().map(|v| v as u8).collect();
        assert_eq!(lazy.read_band_mask(2, full, Some(level)).unwrap(), expected, "overview {level}");
    }
    assert!(lazy.read_band_mask(0, w, None).is_err());
    assert!(lazy.read_band_mask(5, w, None).is_err());

    // A nodata value Byte holds masks each band by its own values, not by the alpha band.
    let nodata_path = dir.path().join("rgba_nodata.tif").to_str().unwrap().to_string();
    write(&rgba(0.0), &nodata_path, "NONE", false, true, true);
    let mut nodata = terrio::open_raster_lazy(&nodata_path).unwrap();
    let band1 = nodata.read_window(Window::full(cols, rows), &WindowOptions { bands: vec![1], overview: None }).unwrap().raster.data;
    let expected: Vec<u8> = band1.iter_f64().map(|v| if v == 0.0 { 0 } else { 255 }).collect();
    assert!(expected.contains(&0) && expected.contains(&255));
    assert_eq!(nodata.read_band_mask(1, Window::full(cols, rows), None).unwrap(), expected);

    // A three-band raster without alpha, and five bands of which the last is alpha (GDAL takes the
    // alpha band for a mask with two or four bands only): every band's mask is all valid.
    for (bands, with_alpha) in [(3, false), (5, true)] {
        let path = dir.path().join(format!("rgb{bands}.tif")).to_str().unwrap().to_string();
        write(&raster(bands, DataType::U8), &path, "NONE", false, with_alpha, true);
        let mut lazy = terrio::open_raster_lazy(&path).unwrap();
        assert_eq!(lazy.alpha_band(), with_alpha.then_some(bands), "{bands} bands");
        assert_eq!(lazy.read_band_mask(1, Window::full(COLS, ROWS), None).unwrap(), vec![255; COLS * ROWS], "{bands} bands");
        // Windows past the edge and overviews the raster does not have are errors, all-valid or not.
        assert!(lazy.read_band_mask(1, Window { x: COLS - 2, y: 0, width: 3, height: 1 }, None).is_err(), "{bands} bands");
        assert!(lazy.read_band_mask(1, Window::full(1, 1), Some(lazy.overviews().len())).is_err(), "{bands} bands");
    }
}

/// A UInt16 alpha band masks the other bands rescaled to 0-255 as GDAL 3.12's
/// `GDALRescaledAlphaBand` does (1 to 256 give 1, the others times 255 / 65535), and band 1 of a
/// gray+alpha raster takes band 2 for its mask.
#[test]
fn uint16_and_gray_alpha_band_masks() {
    let dir = tempfile::tempdir().unwrap();
    let (cols, rows) = (5, 2);
    let n = cols * rows;
    let write_with = |bands: usize, data: RasterData, photometric: &str, name: &str| {
        let cfg = terrio::RasterConfig { cols, rows, bands, data_type: data.data_type(), nodata: -9999.0, ..Default::default() };
        let ds = terrio::RasterDataset::new(terrio::Raster::from_data_native(cfg, data).unwrap(), false);
        let path = dir.path().join(name).to_str().unwrap().to_string();
        let co = vec![("PHOTOMETRIC".to_string(), photometric.to_string()), ("ALPHA".to_string(), "YES".to_string())];
        terrio::write_raster_dataset(&ds, &path, &RasterWriteOptions { creation_options: co, ..Default::default() }).unwrap();
        terrio::open_raster_lazy(&path).unwrap()
    };
    let alpha16: Vec<u16> = [0, 1, 256, 257, 65535].repeat(2);
    let mut rgba16: Vec<u16> = (0..3 * n).map(|i| i as u16 * 100 + 1).collect();
    rgba16.extend_from_slice(&alpha16);
    let mut lazy = write_with(4, RasterData::U16(rgba16), "RGB", "rgba16.tif");
    assert_eq!(lazy.alpha_band(), Some(4));
    let expected: Vec<u8> = [0, 1, 1, 1, 255].repeat(2);
    for band in [1, 2, 3] {
        assert_eq!(lazy.read_band_mask(band, Window::full(cols, rows), None).unwrap(), expected, "band {band}");
    }

    let alpha8: Vec<u8> = (0..n).map(|i| (i * 28) as u8).collect();
    let mut gray_alpha: Vec<u8> = (0..n).map(|i| i as u8 + 1).collect();
    gray_alpha.extend_from_slice(&alpha8);
    let mut lazy = write_with(2, RasterData::U8(gray_alpha), "MINISBLACK", "gray_alpha.tif");
    assert_eq!(lazy.alpha_band(), Some(2));
    assert_eq!(lazy.read_band_mask(1, Window::full(cols, rows), None).unwrap(), alpha8);
    assert_eq!(lazy.read_band_mask(2, Window::full(cols, rows), None).unwrap(), vec![255; n]);
}
