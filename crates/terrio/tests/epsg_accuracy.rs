//! terrio's coordinate transformations against GDAL's for every EPSG geographic 2D and projected
//! CRS, without datum shift grids: forward from WGS 84, inverse to WGS 84 and between pairs of
//! CRSs, with reference points from `scripts/gen_epsg_points.py` (`epsg_points.tsv` in
//! `TERRIO_TEST_DATA`).
//!
//! Set `TERRIO_EPSG_REPORT` to a file path to write the error of every point there.

mod epsg_common;

#[test]
fn epsg_transformations_match_gdal() {
    // The reference uses no grid, whatever is installed: neither does terrio.
    terrio::crs::set_grid_search_paths(Some(Vec::new()));
    terrio::crs::set_network_enabled(Some(false));
    let Some((fwd, inv, cross)) = epsg_common::check("epsg_points.tsv") else { return };
    // Regression guards. With GDAL 3.12 and PROJ 9.8 (which generated the EPSG table), every
    // forward and inverse point is within 1 cm and 6276 of 6295 CRS-to-CRS points within 1 m;
    // the slack allows for reference points from other GDAL and PROJ versions.
    let n = fwd.errors.len();
    assert!(n > 6000, "only {n} forward points");
    assert!(fwd.count(0.01) >= 6300, "forward within 1 cm: {}", fwd.count(0.01));
    assert!(inv.count(0.01) >= 6300, "inverse within 1 cm: {}", inv.count(0.01));
    assert!(cross.count(1.0) >= 6250, "CRS to CRS within 1 m: {}", cross.count(1.0));
}
