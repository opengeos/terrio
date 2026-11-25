// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Edge values of PROJ strings and points, as PROJ 9.9.0 reads and transforms them
//! (`PROJ_NETWORK=OFF`, no grids, `always_xy`): longitudes beyond 10 radians (#1012 item 23),
//! conversion parameters within 1e-8 of an integer and Krovak's `+alpha` (item 39), the
//! ellipsoidal `cea` inverse past the pole line (item 40), polar Lambert azimuthal equal-area's
//! axes and a vertical CRS's body (item 41), and `+over` (item 42).

use terrio_crs::{AxisOrder, SpatialRef, TransformOptions, Transformer};

fn offline() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
}

fn transformer(src: &str, dst: &str) -> Transformer {
    Transformer::new(&SpatialRef::parse(src).unwrap(), &SpatialRef::parse(dst).unwrap()).unwrap()
}

#[test]
fn longitudes_beyond_ten_radians_are_refused_into_a_shift_or_projection() {
    offline();
    // PROJ's `fwd_prepare()`: 718° into OSGB36 to WGS 84 (6) or a projection fails.
    assert!(transformer("EPSG:4277", "EPSG:4326").transform(718.0, 52.0).is_err());
    assert!(transformer("EPSG:4326", "EPSG:32633").transform(718.0, 52.0).is_err());
    // Through a null transformation, or a ballpark, PROJ keeps it (718°: terrio the same point at
    // -2°), and 572° (under 10 radians) is transformed.
    for (src, dst) in [("EPSG:4326", "EPSG:4258"), ("EPSG:4326", "EPSG:4979"), ("+proj=longlat +ellps=GRS80", "EPSG:4326")] {
        let (x, y) = transformer(src, dst).transform(718.0, 52.0).unwrap();
        assert!((x - -2.0).abs() < 1e-9 && (y - 52.0).abs() < 1e-9, "{src} -> {dst}: {x} {y}");
    }
    let (x, _) = transformer("EPSG:4326", "EPSG:4277").transform(572.0, 52.0).unwrap();
    assert!((x - -148.0).abs() < 1e-3, "{x}");
    // Through the heights' paths too (heights promoted to 3D), and reported through a null
    // transformation as without the limit.
    let options = TransformOptions { promote_to_3d: true, ..Default::default() };
    let promoted =
        Transformer::with_options(&SpatialRef::from_epsg(4326).unwrap(), &SpatialRef::from_epsg(3857).unwrap(), &options).unwrap();
    assert!(promoted.transform_3d(600.0, 40.0, 0.0).is_err());
    let null = transformer("EPSG:4326", "EPSG:4258");
    assert_eq!(null.operation_at(718.0, 52.0, 0.0).unwrap(), null.operation_at(-2.0, 52.0, 0.0).unwrap());
}

#[test]
fn conversion_parameters_near_an_integer_are_that_integer() {
    offline();
    // PROJ 9.9.0 stores `+lat_0=45.000000001` as 45: (9°, 45°) is at y = 0, not -0.000111 m.
    let (_, y) = transformer("EPSG:4326", "+proj=tmerc +lat_0=45.000000001 +lon_0=9 +ellps=GRS80").transform(9.0, 45.0).unwrap();
    assert_eq!(y, 0.0);
    let (x, y) = transformer("EPSG:4326", "+proj=tmerc +lat_0=45 +lon_0=9 +k=0.99999999999 +ellps=GRS80").transform(10.0, 46.0).unwrap();
    assert!((x - 77463.16783681384).abs() < 1e-9 && (y - 111627.84201447366).abs() < 1e-9, "{x} {y}");
}

#[test]
fn krovak_refuses_another_cone_axis() {
    offline();
    // PROJ 9.9.0 reads `+alpha` as the co-latitude of the cone axis, which its Krovak fixes.
    let (x, y) = transformer("EPSG:4326", "+proj=krovak +alpha=30.2881397527778 +ellps=bessel").transform(15.0, 50.0).unwrap();
    assert!((x - -703105.6899367268).abs() < 1e-3 && (y - -1058219.6008260108).abs() < 1e-3, "{x} {y}");
    for s in ["+proj=krovak +alpha=31 +ellps=bessel", "+proj=mod_krovak +alpha=31 +ellps=bessel", "+proj=krovak +alpha=nan +ellps=bessel"] {
        let crs = SpatialRef::parse(s);
        assert!(crs.is_err() || Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &crs.unwrap()).is_err(), "{s}");
    }
}

#[test]
fn the_ellipsoidal_cea_inverse_fails_past_the_pole_line() {
    offline();
    // PROJ 9.9.0: latitude NaN for (1e6, 7.4e6), where terrio gave 90°; 83.879° for 7.3e6.
    for crs in ["+proj=cea +lat_ts=30 +ellps=WGS84", "ESRI:54017"] {
        assert!(transformer(crs, "EPSG:4326").transform(1_000_000.0, 7_400_000.0).is_err(), "{crs}");
    }
    // The pole itself round-trips, also where rounding puts it a hair past the pole line, which
    // PROJ 9.9.0's plain `asin` refuses (NaN at lat_ts 3.9, 4.0, 4.2, 4.6): within the inverse's
    // precision there (PROJ: 89.9999991500401 at 4.3, as terrio).
    for lat_ts in [3.9, 4.0, 4.2, 4.3, 4.6, 30.0] {
        let cea = format!("+proj=cea +lat_ts={lat_ts} +ellps=WGS84");
        let (x, y) = transformer("EPSG:4326", &cea).transform(10.0, 90.0).unwrap();
        let (_, lat) = transformer(&cea, "EPSG:4326").transform(x, y).unwrap();
        assert!((lat - 90.0).abs() < 1e-5, "{cea}: {lat}");
    }
    let (x, y) = transformer("+proj=cea +lat_ts=30 +ellps=WGS84", "EPSG:4326").transform(1_000_000.0, 7_300_000.0).unwrap();
    assert!((x - 10.364167811212809).abs() < 1e-9 && (y - 83.87912142978298).abs() < 1e-9, "{x} {y}");
}

#[test]
fn polar_laea_has_polar_axes_and_a_vertical_crs_no_body() {
    // PROJ 9.9.0: south/south axes at the north pole, north/north at the south pole.
    for s in ["+proj=laea +lat_0=90 +datum=WGS84", "+proj=laea +lat_0=-90 +datum=WGS84", "EPSG:3571"] {
        assert_eq!(SpatialRef::parse(s).unwrap().axis_order(), AxisOrder::Other, "{s}");
    }
    assert_eq!(SpatialRef::parse("+proj=laea +lat_0=45 +datum=WGS84").unwrap().axis_order(), AxisOrder::EastNorth);
    assert_eq!(SpatialRef::from_epsg(5773).unwrap().body(), None);
    assert_eq!(SpatialRef::from_epsg(4326).unwrap().body(), Some("Earth"));
}

#[test]
fn over_keeps_longitudes_past_the_antimeridian() {
    offline();
    // PROJ 9.9.0 with `+R=1`: Eckert IV's inverse at (3, 0.5), refused without `+over`.
    let (lon, lat) = transformer("+proj=eck4 +R=1 +over", "+proj=longlat +R=1 +over").transform(3.0, 0.5).unwrap();
    assert!((lon - 211.3370913548048).abs() < 1e-9 && (lat - 24.653312377915537).abs() < 1e-9, "{lon} {lat}");
    assert!(transformer("+proj=eck4 +R=1", "+proj=longlat +R=1").transform(3.0, 0.5).is_err());
    let (x, y) = transformer("+proj=longlat +R=1 +over", "+proj=eck4 +R=1 +over").transform(lon, lat).unwrap();
    assert!((x - 3.0).abs() < 1e-9 && (y - 0.5).abs() < 1e-9, "{x} {y}");
    // Van der Grinten at 200°: the other side's branch, and wrapped to -160° without `+over`.
    let (x, y) = transformer("+proj=longlat +R=1 +over", "+proj=vandg +R=1 +over").transform(200.0, 30.0).unwrap();
    assert!((x - 3.4231721123155623).abs() < 1e-9 && (y - 0.6494850352424497).abs() < 1e-9, "{x} {y}");
    let (x, y) = transformer("+proj=longlat +R=1", "+proj=vandg +R=1").transform(200.0, 30.0).unwrap();
    assert!((x - -2.733271159987305).abs() < 1e-9 && (y - 0.609416500169409).abs() < 1e-9, "{x} {y}");
}
