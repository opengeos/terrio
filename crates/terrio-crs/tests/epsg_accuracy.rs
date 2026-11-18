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
    terrio_crs::set_grid_search_paths(Some(Vec::new()));
    terrio_crs::set_network_enabled(Some(false));
    let Some((fwd, inv, cross)) = epsg_common::check("epsg_points.tsv") else { return };
    // Regression guards, counting CRSs (pairs of CRSs from CRS to CRS) whose every point is
    // within the distance. With GDAL 3.13.3 and PROJ 9.9.0 (which generated the EPSG table, as
    // CI's reference) and 50 points of each kind per CRS, deprecated codes by their own
    // definition (`OSR_USE_NON_DEPRECATED=NO`, as PROJ), every point of the 6458 CRSs is within 1
    // cm forward and inverse, and 6437 of 6440 pairs are within 1 m (6436 within 1 cm). The other
    // three go from or to RRAF 1991 (EPSG:4640 and 2989), deprecated CRSs whose own datum PROJ
    // chains through its RRAFG transformation, where terrio uses its ballpark (#153,
    // opengeos/terrio#1012).
    let n = fwd.errors.len();
    assert!(n >= 6458, "only {n} forward CRSs (a reference of PROJ 9.8.1? run scripts/gen_epsg_points.py with PROJ 9.9.0)");
    assert!(fwd.points > 300_000, "only {} forward points (an older reference? run scripts/gen_epsg_points.py)", fwd.points);
    assert!(fwd.count(0.01) >= 6458, "forward within 1 cm: {}", fwd.count(0.01));
    assert!(inv.count(0.01) >= 6458, "inverse within 1 cm: {}", inv.count(0.01));
    assert!(cross.count(0.01) >= 6436, "CRS to CRS within 1 cm: {}", cross.count(0.01));
    assert!(cross.count(1.0) >= 6437, "CRS to CRS within 1 m: {}", cross.count(1.0));
    // The CRSs and pairs beyond 1 cm, so a lower count hides no other: the three of RRAF 1991
    // (17 m to 118 m) and those within 1 m (EPSG:11117 to 3117, 0.31 m).
    assert!(fwd.beyond(0.01).is_empty(), "forward beyond 1 cm: {:?}", fwd.beyond(0.01));
    assert!(inv.beyond(0.01).is_empty(), "inverse beyond 1 cm: {:?}", inv.beyond(0.01));
    assert_eq!(cross.beyond(0.01), ["2989>32220", "4640>32220", "10744>2989", "11117>3117"]);
}
