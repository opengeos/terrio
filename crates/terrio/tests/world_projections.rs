//! The world projections terrio ports from PROJ's spherical ones (Robinson, the three Winkels,
//! Gall, Gauss-Schreiber transverse Mercator, Foucaut sinusoidal, McBryde-Thomas flat-polar
//! sinusoidal, Putniņš P4′, Murdoch II and Tissot) against PROJ, on a sphere and on WGS 84. The
//! reference points are pyproj's (PROJ 9.7.1), from `scripts/gen_world_projection_points.py`,
//! from the geographic CRS on the same figure, so that no datum transformation is involved:
//! forward within 1 mm, and the inverse of PROJ's projected points within 1e-9°.

use std::path::Path;

use terrio::crs::{SpatialRef, Transformer};

/// A projected CRS and its points: longitude, latitude, PROJ's x and y (infinite where PROJ
/// refuses the point) and PROJ's inverse of that x and y.
struct Case {
    crs: String,
    points: Vec<[f64; 6]>,
}

fn cases() -> Vec<Case> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/world_projections.tsv");
    let text = std::fs::read_to_string(path).unwrap();
    let mut cases: Vec<Case> = Vec::new();
    for line in text.lines() {
        if let Some(crs) = line.strip_prefix("# +proj=") {
            cases.push(Case { crs: format!("+proj={crs}"), points: Vec::new() });
            continue;
        }
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let v: Vec<f64> = line.split('\t').map(|c| c.parse().unwrap()).collect();
        let inv = if v.len() == 6 { [v[4], v[5]] } else { [v[0], v[1]] };
        cases.last_mut().unwrap().points.push([v[0], v[1], v[2], v[3], inv[0], inv[1]]);
    }
    cases
}

/// The geographic CRS on the figure of a projected CRS.
fn geog(crs: &str) -> &'static str {
    if crs.contains("+R=6371000") { "+proj=longlat +R=6371000 +no_defs" } else { "+proj=longlat +datum=WGS84 +no_defs" }
}

/// The difference between two longitudes in degrees, across the antimeridian.
fn dlon(a: f64, b: f64) -> f64 {
    ((a - b + 180.0).rem_euclid(360.0) - 180.0).abs()
}

fn check(case: &Case) -> usize {
    let proj = SpatialRef::from_proj4(&case.crs).unwrap();
    let geog = SpatialRef::from_proj4(geog(&case.crs)).unwrap();
    let fwd = Transformer::new(&geog, &proj).unwrap();
    let inv = Transformer::new(&proj, &geog).unwrap();
    let crs = &case.crs;
    for &[lon, lat, x, y, ilon, ilat] in &case.points {
        if !x.is_finite() {
            let r = fwd.transform(lon, lat);
            assert!(r.is_err() || !r.unwrap().0.is_finite(), "{crs}: forward of ({lon}, {lat}), which PROJ refuses, succeeded");
            continue;
        }
        let (tx, ty) = fwd.transform(lon, lat).unwrap_or_else(|e| panic!("{crs}: forward of ({lon}, {lat}): {e}"));
        assert!((tx - x).abs() < 1e-3 && (ty - y).abs() < 1e-3, "{crs}: forward ({lon}, {lat}): ({tx}, {ty}), PROJ ({x}, {y})");
        match inv.transform(x, y) {
            Ok((tlon, tlat)) if ilon.is_finite() => {
                // At a pole every longitude is the pole's, and the latitude, an arcsine of about
                // ±1 for some, moves by 1e-6° with the last bit of y.
                // PROJ's result, or the point itself: an iterative inverse terrio runs to a tighter
                // tolerance than PROJ (Winkel II) lands nearer the point.
                let pole = lat.abs() == 90.0;
                let near = |lo: f64, la: f64| (pole || dlon(tlon, lo) < 1e-9) && (tlat - la).abs() < if pole { 1e-5 } else { 1e-9 };
                assert!(near(ilon, ilat) || near(lon, lat), "{crs}: inverse ({x}, {y}): ({tlon}, {tlat}), PROJ ({ilon}, {ilat})");
            }
            // PROJ refuses some points on the antimeridian, by rounding: terrio may too, or find
            // the point.
            Ok((tlon, tlat)) => {
                assert!(
                    dlon(tlon, lon) < 1e-9 && (tlat - lat).abs() < 1e-9,
                    "{crs}: inverse ({x}, {y}): ({tlon}, {tlat}), point ({lon}, {lat})"
                )
            }
            Err(e) => assert!(!ilon.is_finite(), "{crs}: inverse ({x}, {y}): {e}, PROJ ({ilon}, {ilat})"),
        }
    }
    case.points.len()
}

/// Every projection of the table, by its `+proj` name.
fn check_all(name: &str) {
    let prefix = format!("+proj={name} ");
    let mut n = 0;
    let mut figures = 0;
    for case in cases().iter().filter(|c| c.crs.starts_with(&prefix)) {
        n += check(case);
        figures += 1;
    }
    assert!(figures >= 2 && n > 200, "{name}: only {n} points in {figures} CRSs");
}

#[test]
fn robinson_matches_proj() {
    check_all("robin");
}

#[test]
fn robinson_refuses_points_past_the_poles() {
    let robin = SpatialRef::from_proj4("+proj=robin +R=6371000 +units=m +no_defs").unwrap();
    let geog = SpatialRef::from_proj4("+proj=longlat +R=6371000 +no_defs").unwrap();
    let inv = Transformer::new(&robin, &geog).unwrap();
    // |y| / (1.3523 R) past 1.000001.
    for y in [1.3523 * 6371000.0 * 1.00001, -1.3523 * 6371000.0 * 1.0001] {
        let r = inv.transform(0.0, y);
        assert!(r.is_err() || !r.unwrap().0.is_finite(), "inverse of (0, {y}) succeeded");
    }
    // Between 1 and 1.000001: the pole.
    let (_, lat) = inv.transform(0.0, 1.3523 * 6371000.0 * 1.0000005).unwrap();
    assert_eq!(lat, 90.0);
}

#[test]
fn winkel_tripel_matches_proj() {
    check_all("wintri");
}

#[test]
fn winkel_i_matches_proj() {
    check_all("wink1");
}

#[test]
fn winkel_ii_matches_proj() {
    check_all("wink2");
}

#[test]
fn gall_stereographic_matches_proj() {
    check_all("gall");
}

#[test]
fn gauss_schreiber_transverse_mercator_matches_proj() {
    check_all("gstmerc");
}

#[test]
fn foucaut_sinusoidal_matches_proj() {
    check_all("fouc_s");
}

#[test]
fn mcbryde_thomas_flat_polar_sinusoidal_matches_proj() {
    check_all("mbtfps");
}

#[test]
fn putnins_p4p_matches_proj() {
    check_all("putp4p");
}

#[test]
fn murdoch_ii_matches_proj() {
    check_all("murd2");
}

#[test]
fn tissot_matches_proj() {
    check_all("tissot");
}

#[test]
fn invalid_parameters_are_refused() {
    for crs in [
        "+proj=murd2 +lat_1=20 +R=6371000",
        "+proj=tissot +lat_2=60 +R=6371000",
        "+proj=tissot +lat_1=20 +lat_2=20 +R=6371000",
        "+proj=murd2 +lat_1=-20 +lat_2=20 +R=6371000",
        "+proj=fouc_s +n=1.5 +R=6371000",
        "+proj=fouc_s +n=-0.1 +R=6371000",
    ] {
        assert!(SpatialRef::from_proj4(crs).is_err(), "{crs} was accepted");
    }
}
