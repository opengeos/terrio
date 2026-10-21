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
    // Regression guards, counting CRSs (pairs of CRSs from CRS to CRS) whose every point is
    // within the distance. With GDAL 3.12.2 and PROJ 9.8.1 (which generated the EPSG table, as
    // CI's reference) and 50 points of each kind per CRS, every point of the 6354 CRSs is within
    // 1 cm forward and inverse, and 6293 of 6295 pairs are within 1 m (6291 within 1 cm). The
    // other two are deprecated CRSs whose own datum PROJ uses (#153).
    let n = fwd.errors.len();
    assert!(n > 6000, "only {n} forward CRSs");
    assert!(fwd.points > 300_000, "only {} forward points (an older reference? run scripts/gen_epsg_points.py)", fwd.points);
    assert!(fwd.count(0.01) >= 6354, "forward within 1 cm: {}", fwd.count(0.01));
    assert!(inv.count(0.01) >= 6354, "inverse within 1 cm: {}", inv.count(0.01));
    assert!(cross.count(0.01) >= 6291, "CRS to CRS within 1 cm: {}", cross.count(0.01));
    assert!(cross.count(1.0) >= 6293, "CRS to CRS within 1 m: {}", cross.count(1.0));
}
