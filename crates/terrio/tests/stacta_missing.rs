//! STACTA missing tiles: an error, or nodata with `GDAL_STACTA_SKIP_MISSING_METATILE=YES`. A test
//! binary of its own, as it sets the environment variable.

mod stacta_common;

use serde_json::json;
use stacta_common::{item, read, write_item, write_tiles};

#[test]
fn reads_missing_tiles_as_an_error_or_nodata() {
    let dir = tempfile::tempdir().unwrap();
    // Tile 1/1/1 is missing.
    write_tiles(dir.path(), "", &[(0, 0, 0), (1, 0, 0), (1, 0, 1), (1, 1, 0)]);
    let json = write_item(dir.path(), "item.json", &item(None, json!({"data": {"href": "./{TileMatrix}/{TileRow}/{TileCol}.tif"}})));
    let mut r = terrio::open_raster_lazy(&json).unwrap();
    let err = r.read_window(terrio::Window::new(0, 0, 32, 32), &Default::default()).unwrap_err().to_string();
    assert!(err.contains("Cannot open") && err.contains("1/1/1.tif"), "{err}");
    // A window of tiles that exist reads.
    assert_eq!(read(&mut r, terrio::Window::new(0, 0, 2, 1), None), vec![1_000_000.0, 1_000_001.0]);
    // SAFETY: this test binary runs this test only, so nothing else reads the environment.
    unsafe { std::env::set_var("GDAL_STACTA_SKIP_MISSING_METATILE", "YES") };
    let opened = terrio::open_raster_lazy(&json);
    unsafe { std::env::remove_var("GDAL_STACTA_SKIP_MISSING_METATILE") };
    let mut r = opened.unwrap();
    let w = read(&mut r, terrio::Window::new(15, 15, 2, 2), None);
    assert_eq!(w, vec![(1_000_000 + 15 * 16 + 15) as f64, (1_001_000 + 15 * 16) as f64, (1_010_000 + 15) as f64, -1.0]);
}
