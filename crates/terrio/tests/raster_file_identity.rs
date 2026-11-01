//! An open raster keeps its file identity after replacement, as GDAL 3.13.3 does.
use terrio::formats::RasterDriver;
use terrio::{RasterDataset, RasterWriteOptions, Window, WindowOptions};
use wbraster::raster::RasterData;

#[test]
fn parallel_windows_keep_the_open_file_after_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("open.tif");
    let replacement = dir.path().join("replacement.tif");
    for compression in ["NONE", "DEFLATE"] {
        let write = |path: &std::path::Path, value, tile_size| {
            let config = wbraster::RasterConfig { cols: 256, rows: 256, bands: 1, data_type: wbraster::DataType::U8, ..Default::default() };
            let raster = wbraster::Raster::from_data_native(config, RasterData::U8(vec![value; 256 * 256])).unwrap();
            let ds = RasterDataset::with_driver(raster, RasterDriver::GTiff, false);
            terrio::write_raster_dataset(
                &ds,
                path.to_str().unwrap(),
                &RasterWriteOptions { tile_size: Some(tile_size), compression: Some(compression.into()), ..Default::default() },
            )
            .unwrap();
        };
        write(&path, 10, 64);
        let mut original = terrio::open_raster_lazy(path.to_str().unwrap()).unwrap();
        let read = |r: &mut terrio::LazyRaster, size| {
            let result = r.read_window(Window::new(0, 0, size, size), &WindowOptions::default()).unwrap();
            result.raster.data.to_f64_vec()
        };
        assert!(read(&mut original, 128).iter().all(|v| *v == 10.0));
        // Different chunk tables as well as values: reusing the path would decode incorrectly.
        write(&replacement, 200, 128);
        std::fs::rename(&replacement, &path).unwrap();
        for size in [32, 128, 256, 64, 256] {
            assert!(read(&mut original, size).iter().all(|v| *v == 10.0), "{compression} {size}");
        }
        let mut current = terrio::open_raster_lazy(path.to_str().unwrap()).unwrap();
        assert!(read(&mut current, 256).iter().all(|v| *v == 200.0));
        // Cached tables are not required: replace before the first chunk read too.
        write(&replacement, 30, 64);
        let mut unread = terrio::open_raster_lazy(replacement.to_str().unwrap()).unwrap();
        std::fs::remove_file(&replacement).unwrap();
        assert!(read(&mut unread, 256).iter().all(|v| *v == 30.0));
    }
}
