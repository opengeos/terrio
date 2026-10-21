//! terrio's coordinate transformations against GDAL's for every EPSG geographic 2D and projected
//! CRS when datum shift grids are available: the grids of `scripts/proj_grids.txt` in the
//! `grids` directory of `TERRIO_TEST_DATA` (fetched by `scripts/fetch_test_data.sh`), and
//! reference points GDAL computed with them (`epsg_points_grids.tsv`, from
//! `scripts/gen_epsg_points.py --grids`).
//!
//! Set `TERRIO_EPSG_REPORT` to a file path to write the error of every point there.

mod epsg_common;

#[test]
fn epsg_transformations_with_grids_match_gdal() {
    let Some(dir) = std::env::var_os("TERRIO_TEST_DATA") else { return };
    let grids = std::path::Path::new(&dir).join("grids");
    if !grids.join("us_noaa_conus.tif").exists() {
        eprintln!("{} has no grids: run scripts/fetch_test_data.sh", grids.display());
        return;
    }
    terrio::crs::set_grid_search_paths(Some(vec![grids]));
    terrio::crs::set_network_enabled(Some(false));
    let Some((fwd, inv, cross)) = epsg_common::check("epsg_points_grids.tsv") else { return };
    // Regression guards, counting CRSs (pairs from CRS to CRS) whose every point is within the
    // distance. With GDAL 3.12.2 and PROJ 9.8.1 and 50 points of each kind per CRS, every point
    // of the 6354 CRSs is within 1 cm forward and inverse, and 6273 of 6295 pairs are within 1 m
    // (6201 within 1 cm): PROJ chains some pairs through several intermediate datums that
    // terrio does not try (NAD27 and NAD83 through NAD83(HARN) and NAD83(FBN)) (#153).
    let n = fwd.errors.len();
    assert!(n > 6000, "only {n} forward CRSs");
    assert!(fwd.points > 300_000, "only {} forward points (an older reference? run scripts/gen_epsg_points.py)", fwd.points);
    assert!(fwd.count(0.01) >= 6354, "forward within 1 cm: {}", fwd.count(0.01));
    assert!(inv.count(0.01) >= 6354, "inverse within 1 cm: {}", inv.count(0.01));
    assert!(cross.count(0.01) >= 6201, "CRS to CRS within 1 cm: {}", cross.count(0.01));
    assert!(cross.count(1.0) >= 6273, "CRS to CRS within 1 m: {}", cross.count(1.0));
}
