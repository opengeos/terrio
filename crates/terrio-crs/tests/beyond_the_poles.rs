//! A latitude beyond a pole (or a projected coordinate given as degrees) is not folded back across
//! the pole to a valid place. As in PROJ, the geodetic-to-geocentric step of a datum shift
//! refuses it, and an operation whose area is the whole world (the ballpark) holds every point.
//!
//! The values are PROJ 9.8.1's (pyproj 3.8.0, `Transformer.from_crs(src, dst, always_xy=True)`,
//! `transform(x, y, errcheck=True)`, a refusal or an infinite result as `None`), with only
//! `proj.db` and `proj.ini` and `PROJ_NETWORK=OFF`, so no grids. Tolerance: 1e-9 for degrees,
//! 1 mm for metres. terrio wraps the output longitude into [-180, 180] where PROJ's ballpark
//! keeps it as given, so longitudes are compared modulo 360°.

use terrio_crs::{SpatialRef, Transformer};

const BESSEL_TOWGS84: &str = "+proj=longlat +ellps=bessel +towgs84=598.1,73.7,418.2,0.202,0.045,-2.455,6.7";

/// (source, target, point, PROJ's result).
#[rustfmt::skip]
const CASES: &[(&str, &str, (f64, f64), Option<(f64, f64)>)] = &[
    // Helmert transformations whose areas exclude the point, then the ballpark: no shift.
    ("+proj=longlat +datum=OSGB36", "EPSG:4326", (10.0, 95.0), Some((10.0, 95.0))),
    ("+proj=longlat +datum=OSGB36", "EPSG:4326", (10.0, -90.5), Some((10.0, -90.5))),
    ("+proj=longlat +datum=OSGB36", "EPSG:4326", (451219.33, 5411550.2), Some((451219.33, 5411550.2))),
    ("+proj=longlat +datum=OSGB36", "EPSG:4326", (10.0, 90.0), Some((10.0, 90.0))),
    ("+proj=longlat +datum=OSGB36", "EPSG:4326", (-1.0, 52.0), Some((-1.0015262257828021, 52.00043957445139))),
    ("EPSG:4277", "EPSG:4326", (10.0, 95.0), Some((10.0, 95.0))),
    ("EPSG:4277", "EPSG:4326", (451219.33, 5411550.2), Some((451219.33, 5411550.2))),
    ("EPSG:4267", "EPSG:4326", (10.0, 95.0), Some((10.0, 95.0))),
    ("EPSG:4267", "EPSG:4326", (10.0, -90.5), Some((10.0, -90.5))),
    ("EPSG:4258", "EPSG:4326", (10.0, 95.0), Some((10.0, 95.0))),
    ("EPSG:4326", "EPSG:4258", (451219.33, 5411550.2), Some((451219.33, 5411550.2))),
    // A `+towgs84` Helmert is the only operation: refused beyond the poles.
    (BESSEL_TOWGS84, "EPSG:4326", (10.0, 95.0), None),
    (BESSEL_TOWGS84, "EPSG:4326", (10.0, -90.5), None),
    (BESSEL_TOWGS84, "EPSG:4326", (451219.33, 5411550.2), None),
    (BESSEL_TOWGS84, "EPSG:4326", (10.0, 90.0), Some((6.421905717473288, 89.9945987051041))),
    (BESSEL_TOWGS84, "EPSG:4326", (-1.0, 52.0), Some((-0.999527977118939, 51.9986535183326))),
    // Geocentric and projected targets refuse them too.
    ("EPSG:4326", "EPSG:4978", (10.0, 95.0), None),
    ("EPSG:4326", "EPSG:4978", (10.0, -90.5), None),
    ("EPSG:4326", "EPSG:4978", (-1.0, 52.0), Some((3934361.153136928, -68674.52937906208))),
    ("EPSG:4326", "EPSG:32632", (10.0, 95.0), None),
    ("EPSG:4326", "EPSG:32632", (451219.33, 5411550.2), None),
];

#[test]
fn latitudes_beyond_the_poles_are_not_folded_back() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    for &(src, dst, (x, y), expected) in CASES {
        let t = Transformer::new(&SpatialRef::parse(src).unwrap(), &SpatialRef::parse(dst).unwrap()).unwrap();
        let got = t.transform(x, y).ok().filter(|(u, v)| u.is_finite() && v.is_finite());
        let case = format!("{src} -> {dst} ({x}, {y}): {got:?}, PROJ {expected:?}");
        match (got, expected) {
            (None, None) => {}
            (Some((u, v)), Some((eu, ev))) => {
                let geographic = dst != "EPSG:4978" && dst != "EPSG:32632";
                if geographic {
                    let du = (u - eu).rem_euclid(360.0);
                    assert!(du.min(360.0 - du) < 1e-9 && (v - ev).abs() < 1e-9, "{case}");
                } else {
                    assert!((u - eu).abs() < 1e-3 && (v - ev).abs() < 1e-3, "{case}");
                }
            }
            _ => panic!("{case}"),
        }
    }
}
