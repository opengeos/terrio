//! Sharing a transformer between threads, its inverse, and transforming slices of points.

use std::sync::Arc;
use terrio_crs::{SpatialRef, TransformOptions, Transformer};

fn epsg(code: u32) -> SpatialRef {
    SpatialRef::from_epsg(code).unwrap()
}

#[test]
fn transformer_is_send_and_sync() {
    fn shared<T: Send + Sync>() {}
    shared::<Transformer>();
}

#[test]
fn shared_transformer_gives_the_same_points_on_every_thread() {
    let t = Arc::new(Transformer::new(&epsg(4326), &epsg(32632)).unwrap());
    let expected = t.transform(9.0, 47.0).unwrap();
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let t = Arc::clone(&t);
            std::thread::spawn(move || t.transform(9.0, 47.0).unwrap())
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap(), expected);
    }
}

#[test]
fn inverse_matches_a_transformer_built_for_the_other_direction() {
    // A datum change (CH1903+ to WGS 84), so the inverse has to pick its own operation.
    let forward = Transformer::new(&epsg(2056), &epsg(4326)).unwrap();
    let inverse = forward.inverse().unwrap();
    let built = Transformer::new(&epsg(4326), &epsg(2056)).unwrap();
    let (lon, lat) = forward.transform(2_600_000.0, 1_200_000.0).unwrap();
    assert_eq!(inverse.transform(lon, lat).unwrap(), built.transform(lon, lat).unwrap());
    let (e, n) = inverse.transform(lon, lat).unwrap();
    // The operations of the two directions don't undo each other exactly: about a millimetre.
    assert!((e - 2_600_000.0).abs() < 1e-2 && (n - 1_200_000.0).abs() < 1e-2, "round trip: ({e}, {n})");
}

#[test]
fn inverse_swaps_the_coordinate_epochs() {
    // ITRF2014 (dynamic) to GDA2020 (static): the source epoch applies; in the inverse the target
    // epoch applies, and it must be the same year.
    let opts = TransformOptions { source_coordinate_epoch: Some(2010.0), ..Default::default() };
    let forward = Transformer::with_options(&epsg(7912), &epsg(7843), &opts).unwrap();
    let built = Transformer::with_options(
        &epsg(7843),
        &epsg(7912),
        &TransformOptions { target_coordinate_epoch: Some(2010.0), ..Default::default() },
    )
    .unwrap();
    let inverse = forward.inverse().unwrap();
    let p = (135.0, -25.0, 10.0);
    assert_eq!(inverse.transform_3d(p.0, p.1, p.2).unwrap(), built.transform_3d(p.0, p.1, p.2).unwrap());
}

#[test]
fn inverse_keeps_the_reverse_degree_division() {
    let forward = Transformer::new(&epsg(32632), &epsg(4326)).unwrap().with_reverse_degree_division();
    let inverse = forward.inverse().unwrap();
    let divided = Transformer::new(&epsg(4326), &epsg(32632)).unwrap().with_reverse_degree_division();
    let plain = Transformer::new(&epsg(4326), &epsg(32632)).unwrap();
    let mut differs = false;
    for i in 0..200 {
        let (lon, lat) = (6.0 + f64::from(i) * 0.0317, 45.0 + f64::from(i) * 0.0213);
        let p = inverse.transform(lon, lat).unwrap();
        assert_eq!(p, divided.transform(lon, lat).unwrap());
        differs |= p != plain.transform(lon, lat).unwrap();
    }
    // Otherwise the points could not tell whether the inverse kept it.
    assert!(differs, "no point rounds differently with the reverse degree division");
}

#[test]
fn inverse_keeps_the_options() {
    // With PROMOTE_TO_3D, the heights of 2D CRSs go through the datum shift.
    let opts = TransformOptions { promote_to_3d: true, ..Default::default() };
    let inverse = Transformer::with_options(&epsg(2056), &epsg(4326), &opts).unwrap().inverse().unwrap();
    let built = Transformer::with_options(&epsg(4326), &epsg(2056), &opts).unwrap();
    let plain = Transformer::new(&epsg(4326), &epsg(2056)).unwrap();
    let p = inverse.transform_3d(7.4386, 46.9511, 500.0).unwrap();
    assert_eq!(p, built.transform_3d(7.4386, 46.9511, 500.0).unwrap());
    assert_ne!(p.2, plain.transform_3d(7.4386, 46.9511, 500.0).unwrap().2);
}

static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn note_warning(_: String) {
    WARNED.store(true, std::sync::atomic::Ordering::SeqCst);
}

#[test]
fn inverse_keeps_the_warning_handler() {
    // No datum shift is known for a bare International 1924 ellipsoid: a ballpark, which warns.
    let other = SpatialRef::from_proj4("+proj=longlat +ellps=intl +no_defs").unwrap();
    let inverse = Transformer::new(&epsg(4326), &other).unwrap().with_warning_handler(note_warning).inverse().unwrap();
    inverse.transform(10.0, 50.0).unwrap();
    inverse.flush_warnings();
    assert!(WARNED.load(std::sync::atomic::Ordering::SeqCst));
}

#[test]
fn slice_matches_point_by_point() {
    let t = Transformer::new(&epsg(4326), &epsg(2056)).unwrap();
    let points: Vec<(f64, f64)> = (0..50).map(|i| (6.0 + f64::from(i) * 0.08, 45.9 + f64::from(i) * 0.07)).collect();
    let mut slice = points.clone();
    t.transform_slice(&mut slice).unwrap();
    for (p, q) in points.iter().zip(&slice) {
        assert_eq!(t.transform(p.0, p.1).unwrap(), *q);
    }
}

#[test]
fn slice_between_identical_crs_is_left_as_it_is() {
    let t = Transformer::new(&epsg(32632), &epsg(32632)).unwrap();
    let mut xy = vec![(500_000.0, 5_000_000.0), (f64::MAX, -1.0)];
    t.transform_slice(&mut xy).unwrap();
    assert_eq!(xy, vec![(500_000.0, 5_000_000.0), (f64::MAX, -1.0)]);
    assert_eq!(t.transform_slice_or_nan(&mut xy).unwrap(), 0);
}

#[test]
fn slice_between_identical_geocentric_crs_still_transforms() {
    // Geocentric endpoints go through geographic coordinates, so `transform` rejects NaN even
    // between identical CRSs, and the slices must too.
    let t = Transformer::new(&epsg(4978), &epsg(4978)).unwrap();
    let mut xy = vec![(f64::NAN, 0.0)];
    assert!(t.transform(f64::NAN, 0.0).is_err());
    assert!(t.transform_slice(&mut xy).is_err());
    assert_eq!(t.transform_slice_or_nan(&mut xy).unwrap(), 1);
}

#[test]
fn slice_or_nan_returns_refusals_under_the_strict_options() {
    // With ONLY_BEST=YES, WGS 84 to the British National Grid needs the OSTN15 grid, which the
    // tests run without: a refusal, not a point outside the domain, so the error is returned.
    let opts = TransformOptions { only_best: true, ..Default::default() };
    let t = Transformer::with_options(&epsg(4326), &epsg(27700), &opts).unwrap();
    let mut xy = vec![(-1.5, 52.5)];
    assert!(matches!(t.transform_slice_or_nan(&mut xy), Err(terrio_crs::Error::MissingGrid(_))));
}

/// WGS 84 to an orthographic view centred on (0, 0): the far side of the Earth (170°E on the
/// equator) has no point in it.
fn orthographic() -> Transformer {
    Transformer::new(&epsg(4326), &SpatialRef::from_proj4("+proj=ortho +lat_0=0 +lon_0=0 +datum=WGS84").unwrap()).unwrap()
}

#[test]
fn slice_stops_at_the_first_failure() {
    let t = orthographic();
    let mut xy = vec![(10.0, 10.0), (170.0, 0.0), (20.0, 20.0)];
    assert!(t.transform_slice(&mut xy).is_err());
    assert_eq!(xy[0], t.transform(10.0, 10.0).unwrap());
    assert_eq!(xy[1..], [(170.0, 0.0), (20.0, 20.0)]);
}

#[test]
fn slice_or_nan_marks_failures_and_goes_on() {
    let t = orthographic();
    let mut xy = vec![(10.0, 10.0), (170.0, 0.0), (20.0, 20.0)];
    assert_eq!(t.transform_slice_or_nan(&mut xy).unwrap(), 1);
    assert_eq!(xy[0], t.transform(10.0, 10.0).unwrap());
    assert!(xy[1].0.is_nan() && xy[1].1.is_nan());
    assert_eq!(xy[2], t.transform(20.0, 20.0).unwrap());
}

// ---------------------------------------------------------------------------------------------
// Against PROJ. The references are PROJ 9.8.1's (pyproj 3.8.0, EPSG v12.029), with the network
// off and no grids in its PROJ directory, as terrio's tests run, printed with 12 decimals:
//
//   from pyproj import Transformer as T
//   T.from_crs(src, dst, always_xy=True).transform(x, y)            # 2D cases
//   T.from_crs(src, dst, always_xy=True).transform(x, y, z, 2010.0)  # with a coordinate epoch
//
// for EPSG:4326 <-> EPSG:32632, EPSG:2056 <-> EPSG:4326, EPSG:4326 -> "+proj=ortho +lat_0=0
// +lon_0=0 +datum=WGS84 +type=crs" (PROJ returns inf for 170°E), and EPSG:7912 <-> EPSG:7843.
//
// Tolerances: 1e-6 m and 1e-10 degrees (about 10 µm), well above the 12 printed decimals.

const METRES: f64 = 1e-6;
const DEGREES: f64 = 1e-10;

fn assert_close(actual: &[(f64, f64)], expected: &[(f64, f64)], tolerance: f64) {
    assert_eq!(actual.len(), expected.len());
    for (a, e) in actual.iter().zip(expected) {
        assert!((a.0 - e.0).abs() <= tolerance && (a.1 - e.1).abs() <= tolerance, "{a:?} != PROJ's {e:?} within {tolerance}");
    }
}

#[test]
fn slice_and_inverse_match_proj_for_utm() {
    let forward = Transformer::new(&epsg(4326), &epsg(32632)).unwrap();
    let mut xy = vec![(9.0, 47.0), (7.5, 45.9), (12.25, 48.5)];
    forward.transform_slice(&mut xy).unwrap();
    assert_close(
        &xy,
        &[(500000.0, 5205164.110152201), (383642.866045562, 5084030.81391101), (740067.9243624624, 5376977.581150721)],
        METRES,
    );
    let mut ll = vec![(500000.0, 5205000.0), (300000.0, 5100000.0)];
    forward.inverse().unwrap().transform_slice(&mut ll).unwrap();
    assert_close(&ll, &[(9.0, 46.998523211288), (6.415934879615, 46.024353395823)], DEGREES);
}

#[test]
fn slice_and_inverse_match_proj_for_a_datum_shift() {
    // CH1903+ / LV95 to WGS 84: PROJ, without grids, uses "CH1903+ to WGS 84 (1)", a Helmert
    // shift.
    let forward = Transformer::new(&epsg(2056), &epsg(4326)).unwrap();
    let mut ll = vec![(2600000.0, 1200000.0), (2700000.0, 1250000.0), (2550000.0, 1150000.0)];
    forward.transform_slice(&mut ll).unwrap();
    assert_close(&ll, &[(7.438632420872, 46.951082771871), (8.763207927152, 47.393232219252), (6.787305918873, 46.499437371830)], DEGREES);
    let mut en = vec![(7.4386, 46.9511), (8.5, 47.4), (7.438632420872, 46.951082771871)];
    forward.inverse().unwrap().transform_slice(&mut en).unwrap();
    // The last is the round trip of (2600000, 1200000): PROJ's ends the same millimetre away.
    assert_close(
        &en,
        &[(2599997.532515652, 1200001.9164401442), (2680120.1817609807, 1250450.279328116), (2600000.000562197, 1200000.0011456148)],
        METRES,
    );
}

#[test]
fn slice_or_nan_fails_where_proj_fails() {
    // PROJ returns inf for 170°E: the far side of the orthographic view.
    let mut xy = vec![(10.0, 10.0), (170.0, 0.0), (20.0, 20.0)];
    assert_eq!(orthographic().transform_slice_or_nan(&mut xy).unwrap(), 1);
    assert_close(&[xy[0], xy[2]], &[(1090835.7691960428, 1100248.5477353616), (2050696.8193774875, 2167696.7878287574)], METRES);
    assert!(xy[1].0.is_nan() && xy[1].1.is_nan());
}

#[test]
fn inverse_with_coordinate_epochs_matches_proj() {
    // ITRF2014 at epoch 2010.0 to GDA2020 and back: the plate motion model, by the epoch.
    let opts = TransformOptions { source_coordinate_epoch: Some(2010.0), ..Default::default() };
    let forward = Transformer::with_options(&epsg(7912), &epsg(7843), &opts).unwrap();
    let (x, y, z) = forward.transform_3d(135.0, -25.0, 10.0).unwrap();
    assert_close(&[(x, y)], &[(135.000003061791, -24.999994698869)], DEGREES);
    assert!((z - 9.998492538929).abs() <= METRES, "height {z}");
    let (x, y, z) = forward.inverse().unwrap().transform_3d(135.0, -25.0, 10.0).unwrap();
    assert_close(&[(x, y)], &[(134.999996938209, -25.000005301131)], DEGREES);
    assert!((z - 10.001507529989).abs() <= METRES, "height {z}");
}
