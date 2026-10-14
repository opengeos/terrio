//! Lazy (windowed and overview) reads of real-world GeoTIFFs, checked against the in-memory reader
//! and against values from GDAL 3.12.
//!
//! The local files live outside the repository: set `TERRIO_TEST_DATA` to their directory (see
//! `scripts/fetch_test_data.sh`). Without it, these tests pass without checking anything. The tests
//! that read public cloud files are ignored by default: run them with `cargo test -- --ignored`.

use std::path::PathBuf;
use terrio::{Window, WindowOptions};

fn data(name: &str) -> Option<String> {
    let dir = PathBuf::from(std::env::var_os("TERRIO_TEST_DATA")?);
    let p = dir.join(name);
    assert!(p.exists(), "{} is missing from TERRIO_TEST_DATA", p.display());
    Some(p.to_string_lossy().into_owned())
}

const TIFFS: &[&str] = &[
    "ECOv002_L2T_LSTE_26860_001_10SGD_20230401T203733_0710_01_LST.tif",
    "OPERA_L3_DSWx_WTR.tif",
    "cog.tif",
    "dem.tif",
    "landsat.tif",
    "landsat7.tif",
    "las_vegas_dem.tif",
    "srtm90.tif",
    "usgs_dem.tif",
];

/// The values of one band of a raster, NaN-aware and bit-exact.
fn band_bits(ds: &terrio::RasterDataset, band: usize) -> Vec<u64> {
    ds.raster.band_view(band).as_slice().iter().map(|v| v.to_bits()).collect()
}

#[test]
fn whole_reads_match_the_in_memory_reader() {
    for name in TIFFS {
        let Some(p) = data(name) else { return };
        let full = terrio::open_raster(&p).unwrap();
        let mut lazy = terrio::open_raster_lazy(&p).unwrap();
        assert!(lazy.is_lazy());
        assert_eq!((lazy.width(), lazy.height(), lazy.band_count()), (full.raster.cols, full.raster.rows, full.raster.bands), "{name}");
        assert_eq!(lazy.data_type(), full.raster.data_type, "{name}");
        assert_eq!(lazy.geo_transform(), full.geo_transform(), "{name}");
        assert_eq!(lazy.crs(), full.crs(), "{name}");
        assert_eq!(lazy.nodata().map(f64::to_bits), full.nodata().map(f64::to_bits), "{name}");
        let read = lazy.read().unwrap();
        assert_eq!(read.geo_transform(), full.geo_transform(), "{name}");
        for b in 0..full.raster.bands {
            assert!(band_bits(&read, b) == band_bits(&full, b), "{name} band {}", b + 1);
        }
    }
}

#[test]
fn windows_match_the_in_memory_reader() {
    for name in TIFFS {
        let Some(p) = data(name) else { return };
        let full = terrio::open_raster(&p).unwrap();
        let mut lazy = terrio::open_raster_lazy(&p).unwrap();
        let (w, h) = (lazy.width(), lazy.height());
        let windows = [
            Window::new(0, 0, 1, 1),
            Window::new(w - 1, h - 1, 1, 1),
            Window::new(w / 3, h / 4, (w / 2).max(1), (h / 3).max(1)),
            Window::new(w.saturating_sub(300), h.saturating_sub(270), w.min(300), h.min(270)),
        ];
        let last = lazy.band_count();
        for win in windows {
            let opts = WindowOptions { bands: vec![last], overview: None };
            let ds = lazy.read_window(win, &opts).unwrap();
            assert_eq!((ds.raster.cols, ds.raster.rows, ds.raster.bands), (win.width, win.height, 1));
            let gt = full.geo_transform();
            assert_eq!(ds.geo_transform()[0], gt[0] + win.x as f64 * gt[1], "{name} {win:?}");
            assert!((ds.geo_transform()[3] - (gt[3] + win.y as f64 * gt[5])).abs() <= 1e-9 * gt[3].abs().max(1.0), "{name} {win:?}");
            for row in 0..win.height {
                for col in 0..win.width {
                    let a = ds.raster.get(0, row as isize, col as isize);
                    let b = full.raster.get((last - 1) as isize, (win.y + row) as isize, (win.x + col) as isize);
                    assert!(a.to_bits() == b.to_bits(), "{name} {win:?} at ({col}, {row}): {a} != {b}");
                }
            }
        }
    }
}

/// An overview window and what GDAL reads in it.
struct OverviewCase {
    name: &'static str,
    overview: usize,
    window: Window,
    band: usize,
    /// The overview's size.
    size: (usize, usize),
    /// The sum and count of the valid (not NaN) values, and the first and last value.
    sum: f64,
    count: usize,
    first: f64,
    last: f64,
}

/// Overview windows, with values from GDAL: `band.GetOverview(i).ReadAsArray(x, y, w, h)`.
#[test]
fn overview_windows_match_gdal() {
    let case = |name, overview, window, band, size, sum, count, first, last| OverviewCase {
        name,
        overview,
        window,
        band,
        size,
        sum,
        count,
        first,
        last,
    };
    let cases = [
        case("usgs_dem.tif", 1, Window::new(100, 200, 64, 48), 1, (903, 903), 10788142.654541016, 3072, 3874.80859375, 3681.65966796875),
        case("srtm90.tif", 2, Window::new(500, 300, 34, 9), 1, (534, 309), 399902.0, 306, 1469.0, 468.0),
        case("landsat7.tif", 0, Window::new(1000, 900, 91, 59), 3, (1091, 959), 307612.0, 5369, 43.0, 76.0),
        case("OPERA_L3_DSWx_WTR.tif", 0, Window::new(800, 850, 115, 65), 1, (915, 915), 1906125.0, 7475, 255.0, 255.0),
        case("dem.tif", 1, Window::new(700, 400, 20, 44), 1, (720, 444), 429260.55560302734, 349, 979.491943359375, f64::NAN),
    ];
    for OverviewCase { name, overview: ovr, window: win, band, size, sum, count, first, last } in cases {
        let Some(p) = data(name) else { return };
        let mut lazy = terrio::open_raster_lazy(&p).unwrap();
        assert_eq!(lazy.overviews()[ovr], size, "{name}");
        let ds = lazy.read_window(win, &WindowOptions { bands: vec![band], overview: Some(ovr) }).unwrap();
        let values = ds.raster.band_view(0).as_slice().to_vec();
        let valid: Vec<f64> = values.iter().copied().filter(|v| !v.is_nan()).collect();
        assert_eq!(valid.len(), count, "{name}");
        assert_eq!(valid.iter().sum::<f64>(), sum, "{name}");
        assert_eq!(values[0].to_bits(), first.to_bits(), "{name}");
        assert_eq!(values[values.len() - 1].to_bits(), last.to_bits(), "{name}");
        // The overview's geotransform scales the full image's.
        let gt = lazy.geo_transform();
        let ogt = ds.geo_transform();
        assert!((ogt[1] - gt[1] * lazy.width() as f64 / size.0 as f64).abs() < 1e-12 * gt[1].abs(), "{name}");
    }
}

#[test]
fn rejects_bad_windows() {
    let Some(p) = data("srtm90.tif") else { return };
    let mut lazy = terrio::open_raster_lazy(&p).unwrap();
    assert!(lazy.read_window(Window::new(4000, 0, 270, 1), &WindowOptions::default()).is_err());
    assert!(lazy.read_window(Window::new(0, 0, 1, 1), &WindowOptions { bands: vec![2], overview: None }).is_err());
    assert!(lazy.read_window(Window::new(0, 0, 1, 1), &WindowOptions { bands: vec![], overview: Some(3) }).is_err());
}

#[test]
fn other_formats_are_read_into_memory() {
    let Some(p) = data("lv_dem.asc") else { return };
    let mut lazy = terrio::open_raster_lazy(&p).unwrap();
    assert!(!lazy.is_lazy());
    let full = terrio::open_raster(&p).unwrap();
    let ds = lazy.read_window(Window::new(10, 20, 30, 40), &WindowOptions::default()).unwrap();
    assert_eq!(ds.raster.get(0, 0, 0).to_bits(), full.raster.get(0, 20, 10).to_bits());
    assert_eq!(ds.raster.get(0, 39, 29).to_bits(), full.raster.get(0, 59, 39).to_bits());
}

/// Serializes the network tests, so that each counts only its own bytes.
static NETWORK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Reads a window of a remote raster, checking the bytes fetched and, per band, the sum of the
/// values and the first and last value (from GDAL reading the same window over `/vsicurl/` or
/// `/vsis3/`).
fn check_remote_window(src: &str, window: Window, overview: Option<usize>, max_bytes: u64, expected: &[(f64, f64, f64)]) {
    let _serial = NETWORK.lock().unwrap_or_else(|e| e.into_inner());
    let before = terrio::http::fetch_stats();
    let mut lazy = terrio::open_raster_lazy(src).unwrap();
    assert!(lazy.is_lazy());
    let ds = lazy.read_window(window, &WindowOptions { bands: vec![], overview }).unwrap();
    let fetched = terrio::http::fetch_stats().bytes - before.bytes;
    assert!(fetched < max_bytes, "{src}: {fetched} bytes fetched");
    assert_eq!(ds.raster.bands, expected.len());
    for (b, &(sum, first, last)) in expected.iter().enumerate() {
        let values = ds.raster.band_view(b).as_slice().to_vec();
        let total: f64 = values.iter().sum();
        assert!((total - sum).abs() <= 1e-9 * sum.abs(), "{src} band {}: sum {total} != {sum}", b + 1);
        assert_eq!((values[0], values[values.len() - 1]), (first, last), "{src} band {}", b + 1);
    }
}

/// A USGS 3DEP 1 arc-second tile over HTTPS: a 512 x 512 window at full resolution.
#[test]
#[ignore = "network"]
fn usgs_3dep_window_over_http() {
    let url = "https://prd-tnm.s3.amazonaws.com/StagedProducts/Elevation/1/TIFF/current/n40w106/USGS_1_n40w106.tif";
    // The file is 50 MB; the window's tiles are about 3 MB.
    check_remote_window(url, Window::new(1024, 1024, 512, 512), None, 5_000_000, &[(886312289.9777832, 3323.39453125, 3262.534423828125)]);
}

/// A Copernicus DEM tile in a public S3 bucket in another region: an overview window.
#[test]
#[ignore = "network"]
fn copernicus_dem_overview_over_s3() {
    let src = "s3://copernicus-dem-30m/Copernicus_DSM_COG_10_N37_00_W123_00_DEM/Copernicus_DSM_COG_10_N37_00_W123_00_DEM.tif";
    check_remote_window(
        src,
        Window::new(600, 400, 256, 128),
        Some(1),
        3_000_000,
        &[(1890000.8516133684, 46.241371154785156, 4.418295860290527)],
    );
}

/// A Sentinel-2 L2A true-colour image: three bands of an overview window.
#[test]
#[ignore = "network"]
fn sentinel2_overview_over_http() {
    let url = "https://sentinel-cogs.s3.us-west-2.amazonaws.com/sentinel-s2-l2a-cogs/10/S/EG/2026/10/S2C_10SEG_20261010_0_L2A/TCI.tif";
    let expected = [(5423753.0, 17.0, 42.0), (5544879.0, 24.0, 33.0), (5155046.0, 26.0, 24.0)];
    check_remote_window(url, Window::new(2000, 2100, 300, 200), Some(0), 3_000_000, &expected);
}

/// `info` on a remote COG fetches only its header.
#[test]
#[ignore = "network"]
fn remote_info_reads_only_the_header() {
    let url = "https://sentinel-cogs.s3.us-west-2.amazonaws.com/sentinel-s2-l2a-cogs/10/S/EG/2026/10/S2C_10SEG_20261010_0_L2A/B04.tif";
    let _serial = NETWORK.lock().unwrap_or_else(|e| e.into_inner());
    let before = terrio::http::fetch_stats();
    let terrio::info::Info::Raster(info) = terrio::info::info_path(url, &Default::default()).unwrap() else { panic!("not a raster") };
    assert!(terrio::http::fetch_stats().bytes - before.bytes <= 65536);
    assert_eq!(info.size, [10980, 10980]);
    assert_eq!(info.overviews, vec![[5490, 5490], [2745, 2745], [1373, 1373], [687, 687]]);
    assert_eq!(info.crs.epsg, Some(32610));
    assert_eq!(info.bands[0].nodata, Some(0.0));
}
