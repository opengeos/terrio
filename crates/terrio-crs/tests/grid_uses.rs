// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! The grids a transformation needs at a point, missing ones included, with where to get them
//! (`Transformer::best_operation_grids_at`). Its own test binary: no grid is found (search paths
//! empty, network off), as in WebAssembly before an application registers grids.
//!
//! The grids expected are those of the operation PROJ 9.9.0 uses for the point with every grid
//! present (pyproj 3.8.0, network off, the grids of `scripts/proj_grids.txt` in its data
//! directory: `Transformer.from_crs(source, target, always_xy=True)`, `transform`, then
//! `get_last_used_operation()` and its grids), and their URLs, direct download and licence those
//! of PROJ's database (`proj_coordoperation_get_grid_used`).

use terrio_crs::{Error, GridUse, SpatialRef, TransformOptions, Transformer};

fn grids(source: &str, target: &str, x: f64, y: f64, z: f64) -> Vec<GridUse> {
    terrio_crs::set_grid_search_paths(Some(Vec::new()));
    terrio_crs::set_network_enabled(Some(false));
    let t = Transformer::new(&SpatialRef::parse(source).unwrap(), &SpatialRef::parse(target).unwrap()).unwrap();
    t.best_operation_grids_at(x, y, z).unwrap()
}

fn names(uses: &[GridUse]) -> Vec<&str> {
    uses.iter().map(|g| g.name.as_str()).collect()
}

#[test]
fn a_transformation_names_the_grids_its_best_operation_needs() {
    for (source, target, x, y, expected) in [
        // NAD27 to WGS 84 (48): NAD27 to NAD83 by NADCON, then the Kansas HPGN.
        ("EPSG:4267", "EPSG:4326", -100.0, 40.0, &["us_noaa_conus.tif", "us_noaa_kshpgn.tif"][..]),
        // NAD27 to WGS 84 (33), in Canada.
        ("EPSG:4267", "EPSG:4326", -100.0, 50.0, &["ca_nrc_ntv2_0.tif"]),
        // NAD27 to WGS 84 (85), in Alaska.
        ("EPSG:4267", "EPSG:4326", -150.0, 62.0, &["us_noaa_alaska.tif"]),
        // NAD27 to NAD83 (7), NADCON5.
        ("EPSG:4267", "EPSG:4269", -100.0, 40.0, &["us_noaa_nadcon5_nad27_nad83_1986_conus.tif"]),
        // OSGB36 to WGS 84 (9), from geographic and projected coordinates, and to ETRS89 (2).
        ("EPSG:4277", "EPSG:4326", -2.0, 52.6, &["uk_os_OSTN15_NTv2_OSGBtoETRS.tif"]),
        ("EPSG:27700", "EPSG:4326", 400000.0, 300000.0, &["uk_os_OSTN15_NTv2_OSGBtoETRS.tif"]),
        ("EPSG:4277", "EPSG:4258", -2.0, 52.6, &["uk_os_OSTN15_NTv2_OSGBtoETRS.tif"]),
        // CH1903 to ETRS89 (2), CHENyx06; to WGS 84 (2), a Helmert transformation without grid.
        ("EPSG:4149", "EPSG:4258", 7.44, 46.95, &["ch_swisstopo_CHENyx06_ETRS.tif"]),
        ("EPSG:4149", "EPSG:4326", 7.44, 46.95, &[]),
        // NGVD29 height (m) to NAVD88 height (2), VERTCON central (its grid `vertconc.94` in
        // PROJ's database), with NAD27 to NAD83 (7): PROJ's first operation whose area holds the
        // point (`TransformerGroup` with the network on, so that every grid counts as found).
        ("EPSG:4267+7968", "EPSG:4269+5703", -100.0, 40.0, &["us_noaa_nadcon5_nad27_nad83_1986_conus.tif", "us_noaa_vertconc.tif"]),
        // Auckland 1946 height to NZVD2016 height (2), whose grid is in NZGD2000 coordinates:
        // with NZGD49 to NZGD2000 (3) to get there, though the horizontal CRS stays NZGD49.
        ("EPSG:4272+5759", "EPSG:4272+7839", 174.76, -36.85, &["nz_linz_auckht1946-nzvd2016.tif", "nz_linz_nzgd2kgrid0005.tif"]),
        // One datum: no transformation.
        ("EPSG:4326", "EPSG:32633", 15.0, 50.0, &[]),
        // To NAD27 from geocentric WGS 84 coordinates of (-100°, 40°) (PROJ's EPSG:4326 to
        // EPSG:4978): the inverse of NAD27 to WGS 84 (48), as from geographic coordinates.
        ("EPSG:4978", "EPSG:4267", -849609.758609763, -4818376.377773341, &["us_noaa_conus.tif", "us_noaa_kshpgn.tif"]),
    ] {
        let z = if source == "EPSG:4978" { 4077985.572200376 } else { 0.0 };
        let uses = grids(source, target, x, y, z);
        assert_eq!(names(&uses), expected, "{source} to {target} at ({x}, {y})");
        for g in &uses {
            let url = format!("https://cdn.proj.org/{}", g.name);
            assert_eq!((g.url.as_deref(), g.direct_download, g.open_license), (Some(url.as_str()), true, true), "{g:?}");
            assert!(!g.available && !g.optional, "{g:?}");
        }
    }
}

/// A grid a PROJ string names by its name before PROJ 7 or its producer's goes by its name on the
/// CDN: `conus` is `us_noaa_conus.tif`. One PROJ does not distribute has the producer's URL and
/// licence (PROJ's database: `rdtrans2008.gsb` from salsa.debian.org, not open), one PROJ's
/// database does not know none, and an optional one (`@`) is marked so.
#[test]
fn a_grid_is_named_and_located_as_in_proj_database() {
    let uses = grids("+proj=longlat +ellps=clrk66 +nadgrids=conus", "EPSG:4326", -100.0, 40.0, 0.0);
    assert_eq!(
        uses,
        [GridUse {
            name: "us_noaa_conus.tif".into(),
            url: Some("https://cdn.proj.org/us_noaa_conus.tif".into()),
            direct_download: true,
            open_license: true,
            available: false,
            optional: false,
        }]
    );
    // A producer's name (PROJ's `original_grid_name`), which PROJ reports the grid by but does
    // not open it by under its CDN name: kept, with the CDN's URL, so that a grid registered
    // under it is found.
    let uses = grids("+proj=longlat +ellps=clrk66 +nadgrids=conus.las", "EPSG:4326", -100.0, 40.0, 0.0);
    assert_eq!((uses[0].name.as_str(), uses[0].url.as_deref()), ("conus.las", Some("https://cdn.proj.org/us_noaa_conus.tif")));
    // A grid named twice, once optional: required.
    let uses = grids("+proj=longlat +ellps=clrk66 +nadgrids=@conus,conus", "EPSG:4326", -100.0, 40.0, 0.0);
    assert_eq!((names(&uses), uses[0].optional), (vec!["us_noaa_conus.tif"], false));
    let uses = grids("+proj=longlat +ellps=bessel +nadgrids=rdtrans2008.gsb", "EPSG:4326", 5.0, 52.0, 0.0);
    assert_eq!(names(&uses), ["rdtrans2008.gsb"]);
    assert_eq!(
        (uses[0].url.as_deref(), uses[0].direct_download, uses[0].open_license),
        (Some("https://salsa.debian.org/debian-gis-team/proj-rdnap/raw/upstream/2008/rdtrans2008.gsb"), true, false)
    );
    let uses = grids("+proj=longlat +ellps=GRS80 +nadgrids=@my_own_grid.gsb,null", "EPSG:4326", 5.0, 52.0, 0.0);
    assert_eq!(
        uses,
        [GridUse {
            name: "my_own_grid.gsb".into(),
            url: None,
            direct_download: false,
            open_license: false,
            available: false,
            optional: true,
        }]
    );
}

/// With `ONLY_BEST=YES`, a point whose best operation needs a missing grid fails naming the grid.
#[test]
fn a_missing_grid_error_names_the_grid() {
    terrio_crs::set_grid_search_paths(Some(Vec::new()));
    terrio_crs::set_network_enabled(Some(false));
    let opts = TransformOptions { only_best: true, ..Default::default() };
    let t = Transformer::with_options(&SpatialRef::from_epsg(4277).unwrap(), &SpatialRef::from_epsg(4326).unwrap(), &opts).unwrap();
    match t.transform(-2.0, 52.6) {
        Err(Error::MissingGrid(m)) => assert!(m.contains("(uk_os_OSTN15_NTv2_OSGBtoETRS.tif)"), "{m}"),
        other => panic!("{other:?}"),
    }
}

/// Each point's error names the grids its own best operation needs, not an earlier point's.
#[test]
fn each_point_names_its_own_missing_grid() {
    terrio_crs::set_grid_search_paths(Some(Vec::new()));
    terrio_crs::set_network_enabled(Some(false));
    let opts = TransformOptions { only_best: true, ..Default::default() };
    let t = Transformer::with_options(&SpatialRef::from_epsg(4267).unwrap(), &SpatialRef::from_epsg(4326).unwrap(), &opts).unwrap();
    for (x, y, grid) in [(-100.0, 50.0, "(ca_nrc_ntv2_0.tif)"), (-150.0, 62.0, "(us_noaa_alaska.tif)")] {
        match t.transform(x, y) {
            Err(Error::MissingGrid(m)) => assert!(m.contains(grid), "({x}, {y}): {m}"),
            other => panic!("{other:?}"),
        }
    }
}

/// A route through WGS 84 that has no transformation on one side (a CRS of an ellipsoid alone)
/// applies none on either (the ballpark), so the other side's grid is not needed.
#[test]
fn a_ballpark_route_needs_no_grid() {
    let uses = grids("+proj=longlat +ellps=clrk66 +nadgrids=conus", "+proj=longlat +ellps=intl", -100.0, 40.0, 0.0);
    assert_eq!(uses, []);
}
