//! Grids read from the PROJ CDN in place, by HTTP range requests, as PROJ reads them with
//! network access on: `cargo test -p terrio --test crs_grids_remote -- --ignored`.
//!
//! The expected values are pyproj's (PROJ 9.7, `PROJ_NETWORK=ON`).

use terrio::crs::{SpatialRef, Transformer};

#[test]
#[ignore = "network"]
fn geoid_read_by_range_from_the_cdn() {
    // No local grid: only the CDN.
    terrio::crs::set_grid_search_paths(Some(Vec::new()));
    terrio::crs::set_network_enabled(Some(true));
    let before = terrio::http::fetch_stats();
    let t = Transformer::new(&SpatialRef::parse("EPSG:4979").unwrap(), &SpatialRef::parse("EPSG:4326+3855").unwrap()).unwrap();
    // EGM2008 (us_nga_egm08_25.tif, 80 MB): heights above the geoid.
    for ((x, y, z), expected) in [
        ((-105.27, 40.01, 1700.0), 1715.8606414245605),
        ((2.35, 48.85, 50.0), 5.383694305419951),
        ((139.69, 35.68, 40.0), 3.3394094177247453),
        ((-70.66, -33.45, 570.0), 543.3154596862793),
    ] {
        let (_, _, h) = t.transform_3d(x, y, z).unwrap();
        assert!((h - expected).abs() < 1e-9, "({x}, {y}): {h}, PROJ {expected}");
    }
    let fetched = terrio::http::fetch_stats().bytes - before.bytes;
    assert!(fetched < 5_000_000, "{fetched} bytes fetched of the 80 MB grid");
    // Direct standalone users see the same grid configuration and decoded block cache.
    let cached = terrio::http::fetch_stats().bytes;
    let direct = terrio_crs::Transformer::new(
        &terrio_crs::SpatialRef::parse("EPSG:4979").unwrap(),
        &terrio_crs::SpatialRef::parse("EPSG:4326+3855").unwrap(),
    )
    .unwrap();
    assert!((direct.transform_3d(-105.27, 40.01, 1700.0).unwrap().2 - 1715.8606414245605).abs() < 1e-9);
    assert_eq!(terrio::http::fetch_stats().bytes, cached, "standalone caller fetched a second copy of a cached grid");
    eprintln!("remote EGM2008 grid: {fetched} bytes fetched; standalone cached query: 0 additional bytes");
}
