//! Datum shift grids (NTv2, GeoTIFF), geoid models (GeoTIFF, GTX), compound CRSs and
//! time-dependent transformations against GDAL 3.12 and PROJ 9.7, with the grids of
//! `scripts/proj_grids.txt` and PROJ's legacy grids in the `grids` and `grids_legacy` directories
//! of `TERRIO_TEST_DATA` (see `scripts/fetch_test_data.sh`).
//!
//! The expected values are GDAL's (`osr.CoordinateTransformation` in traditional GIS order, PROJ
//! searching the same grid directories, network off). `scripts/compare_crs_grids_with_gdal.py`
//! compares many more points.

use terrio::crs::{SpatialRef, TransformOptions, Transformer};

/// Points the grid directories at the test data's, or `None` when the grids are missing.
fn grids() -> Option<()> {
    let dir = std::path::PathBuf::from(std::env::var_os("TERRIO_TEST_DATA")?);
    let (grids, legacy) = (dir.join("grids"), dir.join("grids_legacy"));
    if !grids.join("us_noaa_conus.tif").exists() || !legacy.join("BETA2007.gsb").exists() {
        eprintln!("the grids are missing: run scripts/fetch_test_data.sh");
        return None;
    }
    terrio::crs::set_grid_search_paths(Some(vec![grids, legacy]));
    terrio::crs::set_network_enabled(Some(false));
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

#[test]
fn coordinate_epochs_match_gdal() {
    // Time-dependent Helmert transformations need no grid.
    let opts = |s: Option<f64>, t: Option<f64>| TransformOptions { source_coordinate_epoch: s, target_coordinate_epoch: t };
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
