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
