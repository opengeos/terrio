//! Datum shift grids (NTv2, GeoTIFF), geoid models (GeoTIFF, GTX), compound CRSs and
//! time-dependent transformations against GDAL 3.12 and PROJ 9.7, with the grids of
//! `scripts/proj_grids.txt` and PROJ's legacy grids in the `grids` and `grids_legacy` directories
//! of `TERRIO_TEST_DATA` (see `scripts/fetch_test_data.sh`).
//!
//! The expected values are GDAL's (`osr.CoordinateTransformation` in traditional GIS order, PROJ
//! searching the same grid directories, network off). `scripts/compare_crs_grids_with_gdal.py`
//! compares many more points.

use terrio_crs::{SpatialRef, TransformOptions, Transformer};

/// Points the grid directories at the test data's, or `None` when the grids are missing.
fn grids() -> Option<()> {
    let dir = std::path::PathBuf::from(std::env::var_os("TERRIO_TEST_DATA")?);
    let (grids, legacy) = (dir.join("grids"), dir.join("grids_legacy"));
    if !grids.join("us_noaa_conus.tif").exists() || !legacy.join("BETA2007.gsb").exists() {
        eprintln!("the grids are missing: run scripts/fetch_test_data.sh");
        return None;
    }
    terrio_crs::set_grid_search_paths(Some(vec![grids, legacy]));
    terrio_crs::set_network_enabled(Some(false));
    Some(())
}

fn crs(s: &str) -> SpatialRef {
    SpatialRef::parse(s).unwrap()
}

/// Checks a transformation of a point against GDAL's, within `tol` (in the target's units for
/// x and y, metres for heights).
fn check(src: &str, dst: &str, p: (f64, f64, f64), expected: (f64, f64, f64), tol: f64) {
    let t = Transformer::new(&crs(src), &crs(dst)).unwrap();
    let (x, y, z) = t.transform_3d(p.0, p.1, p.2).unwrap();
    let ok = (x - expected.0).abs() <= tol && (y - expected.1).abs() <= tol && (z - expected.2).abs() <= 1e-6;
    assert!(ok, "{src} to {dst} {p:?}: {x} {y} {z}, GDAL {expected:?}");
}

#[test]
fn grids_match_gdal() {
    if grids().is_none() {
        return;
    }
    // 1e-10 degrees is about 0.01 mm.
    let deg = 1e-10;
    // NADCON (us_noaa_conus.tif), forward and inverse (iterated), Canada's NTv2 with its
    // subgrids (ca_nrc_ntv2_0.tif), and Alaska across the antimeridian.
    check("EPSG:4267", "EPSG:4269", (-100.0, 40.0, 0.0), (-100.0004055982497, 40.00000580229538, 0.0), deg);
    check("EPSG:4269", "EPSG:4267", (-100.0, 40.0, 0.0), (-99.9995944017503, 39.99999419770462, 0.0), deg);
    check("EPSG:4267", "EPSG:4269", (-75.7, 45.4, 0.0), (-75.6996753966207, 45.40004365152022, 0.0), deg);
    check("EPSG:4267", "EPSG:4269", (-175.0, 60.0, 0.0), (-175.00263596137367, 59.99894596801863, 0.0), deg);
    // BETA2007 (Germany) and OSTN15 (Great Britain), converted from NTv2.
    check("EPSG:4314", "EPSG:4258", (10.0, 50.0, 0.0), (9.998811455578892, 49.998857302765124, 0.0), deg);
    check("EPSG:4277", "EPSG:4258", (-1.5, 52.0, 0.0), (-1.5014540246693986, 52.0004423172289, 0.0), deg);
    // NADCON5 (PROJ's gridshift, biquadratic) and France's geocentric translations
    // (xyzgridshift), both ways.
    check("EPSG:4152", "EPSG:4759", (-100.0, 40.0, 0.0), (-100.00000017415677, 40.0000000992429, 0.0), deg);
    check("EPSG:4759", "EPSG:4152", (-100.0, 40.0, 0.0), (-99.99999982584323, 39.9999999007571, 0.0), deg);
    check("EPSG:4275", "EPSG:4171", (2.0, 47.0, 0.0), (1.9992837754445716, 46.9999418187531, 0.0), deg);
    check("EPSG:4171", "EPSG:4275", (2.0, 47.0, 0.0), (2.0007162061389456, 47.00005817458301, 0.0), deg);
    // A projected CRS on NAD27 to UTM on WGS 84, through the grids GDAL picks (millimetres).
    check("EPSG:26744", "EPSG:32611", (1840000.0, 640000.0, 0.0), (273402.1191350536, 4107905.0740628922, 0.0), 1e-3);
}

#[test]
fn projjson_identity_and_custom_parameters_keep_wkt_grid_operations() {
    const ISOLATED_GRID: &str = "TERRIO_TEST_PROJJSON_NADCON_GRID";
    let frozen_reference = if let Some(directory) = std::env::var_os(ISOLATED_GRID) {
        terrio_crs::set_grid_search_paths(Some(vec![directory.into()]));
        terrio_crs::set_network_enabled(Some(false));
        true
    } else {
        if grids().is_none() {
            return;
        }
        // The frozen GDAL oracle uses only classic NADCON. Also test the complete
        // installed grid set below, whose registry may prefer newer NADCON5 operations.
        let data = std::path::PathBuf::from(std::env::var_os("TERRIO_TEST_DATA").unwrap());
        let isolated = tempfile::tempdir().unwrap();
        std::fs::copy(data.join("grids/us_noaa_conus.tif"), isolated.path().join("us_noaa_conus.tif")).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "projjson_identity_and_custom_parameters_keep_wkt_grid_operations", "--nocapture"])
            .env(ISOLATED_GRID, isolated.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated NADCON oracle: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        false
    };
    // PROJJSON v0.7 / GDAL 3.12.1: NAD27 UTM uses the real NADCON grid to NAD83.
    let original = SpatialRef::from_epsg(26711).unwrap();
    let mut definition: serde_json::Value = serde_json::from_str(&original.to_projjson().unwrap()).unwrap();
    let target = SpatialRef::from_epsg(4269).unwrap();
    let point = (500000.0, 4500000.0);
    let registered = Transformer::new(&original, &target).unwrap().transform(point.0, point.1).unwrap();
    let mut outcomes = Vec::new();
    for (offset, expected) in [(500000.0, (-117.00093611319762, 40.6526705145774)), (500100.0, (-117.00211897236916, 40.65267049927753))] {
        for parameter in definition["conversion"]["parameters"].as_array_mut().unwrap() {
            if parameter["id"]["code"] == 8806 {
                parameter["value"] = offset.into();
            }
        }
        let parsed = SpatialRef::from_projjson(&definition.to_string()).unwrap();
        assert_eq!(parsed.epsg, (offset == 500000.0).then_some(26711));
        let wkt_only = SpatialRef { wkt: parsed.wkt.clone(), ..Default::default() };
        let actual = Transformer::new(&parsed, &target).unwrap().transform(point.0, point.1).unwrap();
        let from_wkt = Transformer::new(&wkt_only, &target).unwrap().transform(point.0, point.1).unwrap();
        assert!((actual.0 - from_wkt.0).abs() < 1e-10 && (actual.1 - from_wkt.1).abs() < 1e-10);
        if frozen_reference {
            assert!((actual.0 - expected.0).abs() < 1e-10 && (actual.1 - expected.1).abs() < 1e-10, "GDAL's NADCON result: {actual:?}");
        }
        outcomes.push(actual);
    }
    assert_eq!(outcomes[0], registered);
    assert!((outcomes[1].0 - outcomes[0].0).abs() > 1e-3, "the supplied false easting still changes the output");
    // At UTM's central meridian a NAD27-only inverse gives exactly -117 degrees.
    assert!((outcomes[0].0 + 117.0).abs() > 1e-5, "the installed NADCON grid must actually affect the result");
}

/// A grid read from many threads at once (its blocks decoded by whichever thread reaches them
/// first) gives the values it gives read from one.
#[test]
fn grids_read_from_many_threads_match_one() {
    if grids().is_none() {
        return;
    }
    // NAD27 to NAD83 through NADCON (us_noaa_conus.tif), over much of its extent.
    let t = Transformer::new(&crs("EPSG:4267"), &crs("EPSG:4269")).unwrap();
    let points: Vec<(f64, f64)> = (0..4000).map(|i| (-124.0 + (i % 80) as f64 * 0.7, 25.0 + (i / 80) as f64 * 0.45)).collect();
    let fresh = Transformer::new(&crs("EPSG:4267"), &crs("EPSG:4269")).unwrap();
    let parallel: Vec<Vec<_>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..8)
            .map(|k| {
                let (fresh, points) = (&fresh, &points);
                s.spawn(move || points.iter().skip(k).step_by(8).map(|&(x, y)| fresh.transform(x, y).ok()).collect())
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for (k, results) in parallel.iter().enumerate() {
        for (j, got) in results.iter().enumerate() {
            let (x, y) = points[k + 8 * j];
            assert_eq!(*got, t.transform(x, y).ok(), "({x}, {y})");
        }
    }
}

#[test]
fn legacy_formats_match_gdal() {
    if grids().is_none() {
        return;
    }
    // An NTv2 file and a GTX geoid, by PROJ string.
    check(
        "+proj=longlat +ellps=bessel +nadgrids=BETA2007.gsb",
        "EPSG:4326",
        (10.0, 50.0, 0.0),
        (9.998811455578892, 49.998857302765124, 0.0),
        1e-10,
    );
    check(
        "+proj=longlat +datum=WGS84 +geoidgrids=egm96_15.gtx +vunits=m",
        "EPSG:4979",
        (-100.0, 40.0, 100.0),
        (-100.0, 40.0, 74.9475040435791),
        1e-10,
    );
}

#[test]
fn geoid_models_match_gdal() {
    if grids().is_none() {
        return;
    }
    // EGM96 heights, GEOID18 (NAVD88) and OSGM15 (ODN, with OSTN15 for the horizontal part).
    check("EPSG:4979", "EPSG:4326+5773", (-100.0, 40.0, 100.0), (-100.0, 40.0, 125.0524959564209), 1e-10);
    check("EPSG:4326+5773", "EPSG:4979", (-100.0, 40.0, 100.0), (-100.0, 40.0, 74.9475040435791), 1e-10);
    check("EPSG:6319", "EPSG:6318+5703", (-100.0, 40.0, 100.0), (-100.0, 40.0, 124.55540084840055), 1e-10);
    check("EPSG:4937", "EPSG:7405", (-1.5, 52.0, 100.0), (434422.0266244352, 233623.23218370532, 51.66169718609122), 1e-4);
    check("EPSG:7405", "EPSG:4937", (434422.0, 233623.0, 100.0), (-1.500000411014412, 51.9999979142056, 148.33829991027235), 1e-9);
    // GDAL keeps heights when the target has none.
    let t = Transformer::new(&crs("EPSG:4326+5773"), &crs("EPSG:4326")).unwrap();
    assert_eq!(t.transform_3d(-100.0, 40.0, 100.0).unwrap(), (-100.0, 40.0, 100.0));
}

/// Needs the test data: without it the checks return early. The grid-free refusals and the
/// heights kept without a grid are checked in `refusal_kinds.rs`.
#[test]
fn geoid_models_of_the_vertical_datum_match_proj() {
    if grids().is_none() {
        return;
    }
    // PROJ 9.9.0 (pyproj 3.8.0, always_xy, us_noaa_g2018u0.tif): vertical CRSs of NAVD88 without
    // geoid models of their own (feet, US survey feet, depths) use NAVD88 height's GEOID18, the
    // unit and direction converted. 100 m at 30 N, 97 W is 73.40740013115874 m above the
    // NAD83(2011) ellipsoid.
    let ftus = 0.3048006096012192;
    let h = 73.40740013115874;
    let run =
        |src: &str, dst: &str, p: (f64, f64, f64)| Transformer::new(&crs(src), &crs(dst)).unwrap().transform_3d(p.0, p.1, p.2).unwrap();
    for (src, z) in [
        ("EPSG:6318+5703", 100.0),
        ("EPSG:6318+6360", 100.0 / ftus),
        ("EPSG:6318+8228", 100.0 / 0.3048),
        ("EPSG:6318+6357", -100.0),
        ("EPSG:6318+6358", -100.0 / ftus),
    ] {
        let (x, y, r) = run(src, "EPSG:6319", (-97.0, 30.0, z));
        assert!((x + 97.0).abs() < 1e-12 && (y - 30.0).abs() < 1e-12 && (r - h).abs() < 1e-8, "{src} to EPSG:6319: {x} {y} {r}, PROJ {h}");
        // The same as NAVD88 height in metres to a WGS 84 3D CRS too.
        let a = run(src, "EPSG:4979", (-97.0, 30.0, z));
        let b = run("EPSG:6318+5703", "EPSG:4979", (-97.0, 30.0, 100.0));
        assert!((a.2 - b.2).abs() < 1e-8, "{src} to EPSG:4979: {a:?}, from NAVD88 height {b:?}");
    }
    // The inverse, to US survey feet and feet: PROJ's 328.08333333333337 ftUS and 328.0839895013123 ft.
    for (dst, expected) in [("EPSG:6318+6360", 328.08333333333337), ("EPSG:6318+8228", 328.0839895013123), ("EPSG:6318+5703", 100.0)] {
        let (_, _, z) = run("EPSG:6319", dst, (-97.0, 30.0, h));
        assert!((z - expected).abs() < 1e-8, "EPSG:6319 to {dst}: {z}, PROJ {expected}");
    }
    // A Texas state plane CRS in US survey feet with NAVD88 height (ftUS), as lidar is delivered.
    check("EPSG:6588+6360", "EPSG:6319", (3000000.0, 13800000.0, 300.0), (-95.7518511397452, 29.654931459584517, 64.03956291621587), 1e-10);
    // The operation is GEOID18, named after NAVD88 height as PROJ names it.
    let t = Transformer::new(&crs("EPSG:6318+6360"), &crs("EPSG:6319")).unwrap();
    let op = t.operation_at(-97.0, 30.0, 100.0 / ftus).unwrap().unwrap();
    assert!(op.grids.iter().any(|g| g == "us_noaa_g2018u0.tif") && !op.ballpark, "{op:?}");
    assert!(op.name.contains("NAVD88 height"), "{op:?}");
    // Another datum: COH88 2025 (NAVD88) height (ftUS) (EPSG:10919) uses COH88 2025 (NAVD88)
    // height's model (EPSG:10918, us_noaa_g2018u0.tif on CSRN epoch 2025.0). PROJ 9.9.0 at
    // 121.5 W, 38.5 N: 328.0833 ftUS (100 m) is 69.06119918824336 m above NAD83(2011), as from
    // 100 m in EPSG:10918 (69.06119918824339), and back.
    let h = 69.06119918824336;
    for (src, z) in [("EPSG:6318+10918", 100.0), ("EPSG:6318+10919", 100.0 / ftus)] {
        let (_, _, r) = run(src, "EPSG:6319", (-121.5, 38.5, z));
        assert!((r - h).abs() < 1e-8, "{src} to EPSG:6319: {r}, PROJ {h}");
    }
    let (_, _, z) = run("EPSG:6319", "EPSG:6318+10919", (-121.5, 38.5, h));
    assert!((z - 100.0 / ftus).abs() < 1e-8, "EPSG:6319 to EPSG:6318+10919: {z}, PROJ {}", 100.0 / ftus);
    let t = Transformer::new(&crs("EPSG:6318+10919"), &crs("EPSG:6319")).unwrap();
    let op = t.operation_at(-121.5, 38.5, 100.0 / ftus).unwrap().unwrap();
    assert!(op.grids.iter().any(|g| g == "us_noaa_g2018u0.tif") && op.name.contains("COH88 2025 (NAVD88) height"), "{op:?}");
}

#[test]
fn coordinate_epochs_match_gdal() {
    // Time-dependent Helmert transformations need no grid.
    let opts =
        |s: Option<f64>, t: Option<f64>| TransformOptions { source_coordinate_epoch: s, target_coordinate_epoch: t, ..Default::default() };
    let run = |src: &str, dst: &str, o: TransformOptions, p: (f64, f64, f64)| {
        Transformer::with_options(&crs(src), &crs(dst), &o).unwrap().transform_3d(p.0, p.1, p.2).unwrap()
    };
    let close = |a: (f64, f64, f64), b: (f64, f64, f64)| (a.0 - b.0).abs() < 1e-11 && (a.1 - b.1).abs() < 1e-11 && (a.2 - b.2).abs() < 1e-6;
    // ITRF2014 at epoch 2010.0 to GDA2020 (a plate motion model): the source epoch is used.
    let p = run("EPSG:9000", "EPSG:7844", opts(Some(2010.0), None), (135.0, -25.0, 0.0));
    assert!(close(p, (135.0000030617912, -24.999994698869124, -0.0015074610710144043)), "{p:?}");
    // Without an epoch, the parameters at the transformation's reference epoch.
    let p = run("EPSG:9000", "EPSG:7844", opts(None, None), (135.0, -25.0, 0.0));
    assert!(close(p, (135.0, -24.999999999999996, 0.0)), "{p:?}");
    // Between dynamic CRSs, both epochs given: the source's.
    let p = run("EPSG:9000", "EPSG:9990", opts(Some(2010.0), Some(2010.0)), (10.0, 50.0, 0.0));
    assert!(close(p, (10.000000002103551, 49.99999998763506, 0.0032980116084218025)), "{p:?}");
    // An explicit time (cs2cs "50 10 0 2020").
    let t = Transformer::new(&crs("EPSG:9000"), &crs("EPSG:9990")).unwrap();
    let p = t.transform_4d(10.0, 50.0, 0.0, 2020.0).unwrap();
    assert!(close(p, (10.000000015839479, 49.99999997488123, 0.001877542585135)), "{p:?}");
}
