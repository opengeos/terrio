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
    terrio_crs::set_grid_search_paths(Some(vec![grids]));
    terrio_crs::set_network_enabled(Some(false));
    let Some((fwd, inv, cross)) = epsg_common::check("epsg_points_grids.tsv") else { return };
    // Regression guards, counting CRSs (pairs from CRS to CRS) whose every point is within the
    // distance. With GDAL 3.13.3 and PROJ 9.9.0 and 50 points of each kind per CRS, deprecated
    // codes by their own definition (`OSR_USE_NON_DEPRECATED=NO`, as PROJ), every point of the
    // 6458 CRSs is within 1 cm forward and inverse, and 6437 of 6440 pairs are within 1 m (6434
    // within 1 cm). The other three go from or to RRAF 1991 (EPSG:4640 and 2989), deprecated CRSs whose own datum PROJ chains through its
    // RRAFG transformation, where terrio uses its ballpark (#153, opengeos/terrio#1012).
    let n = fwd.errors.len();
    assert!(n >= 6458, "only {n} forward CRSs (a reference of PROJ 9.8.1? run scripts/gen_epsg_points.py with PROJ 9.9.0)");
    assert!(fwd.points > 300_000, "only {} forward points (an older reference? run scripts/gen_epsg_points.py)", fwd.points);
    assert!(fwd.count(0.01) >= 6458, "forward within 1 cm: {}", fwd.count(0.01));
    assert!(inv.count(0.01) >= 6458, "inverse within 1 cm: {}", inv.count(0.01));
    assert!(cross.count(0.01) >= 6434, "CRS to CRS within 1 cm: {}", cross.count(0.01));
    assert!(cross.count(1.0) >= 6437, "CRS to CRS within 1 m: {}", cross.count(1.0));
    // The CRSs and pairs beyond 1 cm, so a lower count hides no other: the three of RRAF 1991
    // (17 m to 118 m) and three within 1 m (0.31 m to 0.61 m).
    assert!(fwd.beyond(0.01).is_empty(), "forward beyond 1 cm: {:?}", fwd.beyond(0.01));
    assert!(inv.beyond(0.01).is_empty(), "inverse beyond 1 cm: {:?}", inv.beyond(0.01));
    assert_eq!(cross.beyond(0.01), ["2989>32220", "4640>32220", "5513>5224", "5651>9000", "10744>2989", "11117>3117"]);
}
