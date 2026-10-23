//! Transformations between vertical CRSs, as PROJ lists and picks them, between compound CRSs.
//! The expected heights are pyproj's (PROJ 9.8.1); `scripts/compare_vertical_with_proj.py`
//! compares every pair of the table.

use terrio::crs::{SpatialRef, Transformer};

fn height(src: &str, dst: &str, x: f64, y: f64, z: f64) -> f64 {
    let t = Transformer::new(&SpatialRef::parse(src).unwrap(), &SpatialRef::parse(dst).unwrap()).unwrap();
    t.transform_3d(x, y, z).unwrap().2
}

/// Held by each test while it sets the process-wide network and grid search settings.
static SETTINGS: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn offsets_slopes_depths_and_units_as_proj() {
    let _settings = SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
    terrio::crs::set_network_enabled(Some(false));
    terrio::crs::set_grid_search_paths(Some(Vec::new()));
    let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
    // DHHN92 to SNN76: two offsets and slopes, through EVRF2000.
    let h = height("EPSG:4258+5783", "EPSG:4258+5785", 13.4, 52.5, 100.0);
    assert!(close(h, 99.85595970908807), "{h}");
    // Baltic 1977 depth to Caspian depth: an offset applied to depths.
    let h = height("EPSG:4326+5612", "EPSG:4326+5706", 49.5326, 41.2942, 100.0);
    assert!(close(h, 72.0), "{h}");
    // Ostend to NAP on WGS 84: the offsets, PROJ's choice over the grids it chains through ETRS89.
    let h = height("EPSG:4326+5710", "EPSG:4326+5709", 4.384, 51.0616, 100.0);
    assert!(close(h, 97.69102352591001), "{h}");
    // One vertical datum in US survey feet and in metres: units only.
    let h = height("EPSG:4269+6360", "EPSG:4269+5703", -100.0, 40.0, 100.0);
    assert!(close(h, 30.480060960121918), "{h}");
    // Outside every transformation's area: heights kept.
    let h = height("EPSG:4258+5783", "EPSG:4258+5785", -100.0, 40.0, 100.0);
    assert!(close(h, 100.0), "{h}");
}

/// VERTCON and grids chained through ETRS89, read from the PROJ CDN:
/// `cargo test -p terrio --test crs_vertical -- --ignored`.
#[test]
#[ignore = "network"]
fn grids_as_proj() {
    let _settings = SETTINGS.lock().unwrap_or_else(|e| e.into_inner());
    terrio::crs::set_network_enabled(Some(true));
    terrio::crs::set_grid_search_paths(Some(Vec::new()));
    let h = height("EPSG:4267+7968", "EPSG:4267+5703", -105.27, 40.01, 1700.0);
    assert!((h - 1701.002603325844).abs() < 1e-9, "{h}");
    let h = height("EPSG:4258+5709", "EPSG:4258+5710", 4.384, 51.0616, 100.0);
    assert!((h - 102.36370123901368).abs() < 1e-9, "{h}");
}
