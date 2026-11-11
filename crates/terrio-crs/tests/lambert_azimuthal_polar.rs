//! The polar aspect of the Lambert azimuthal equal-area projection. In IOGP's Guidance Note 7-2
//! (Lambert Azimuthal Equal Area, EPSG method 9820), its reverse on an ellipsoid is
//! β' = ±asin(1 − ρ²/(a² qP)), with qP = 1 − ((1 − e²)/(2e)) ln((1 − e)/(1 + e)): the whole
//! ellipsoid projects onto the disk ρ ≤ a √(2 qP), whose rim is the opposite pole, and a sphere of
//! radius R (e = 0, qP = 2) onto the disk ρ ≤ 2R. Coordinates beyond the rim, and non-finite ones,
//! are no point of the ellipsoid or the sphere and fail to transform; on the ellipsoid, every
//! point of the disk inverts, up to the rim. Each CRS is checked from the longitude and latitude
//! CRS on its own datum or figure, so that no datum transformation is involved.

use terrio_crs::{SpatialRef, Transformer};

/// A Lambert azimuthal equal-area CRS, the longitude and latitude CRS on the same datum, its false
/// origin, the latitude of the pole opposite its centre, the radius of its disk in metres, and
/// whether its figure is an ellipsoid.
struct Case {
    name: String,
    crs: SpatialRef,
    geographic: SpatialRef,
    origin: (f64, f64),
    opposite: f64,
    rim: f64,
    ellipsoid: bool,
}

/// The radius of the disk, a √(2 qP), on an ellipsoid of semi-major axis `a` and inverse
/// flattening `rf`, or 2a on a sphere of radius `a` (`rf` 0).
fn rim(a: f64, rf: f64) -> f64 {
    if rf == 0.0 {
        return 2.0 * a;
    }
    let f = 1.0 / rf;
    let e = (f * (2.0 - f)).sqrt();
    let qp = 1.0 - (1.0 - e * e) / (2.0 * e) * ((1.0 - e) / (1.0 + e)).ln();
    a * (2.0 * qp).sqrt()
}

const WGS84: (f64, f64) = (6_378_137.0, 298.257_223_563);

fn epsg(code: u32, opposite: f64, (a, rf): (f64, f64)) -> Case {
    let crs = SpatialRef::from_epsg(code).unwrap();
    let geographic = SpatialRef::from_epsg(crs.geographic_code().unwrap()).unwrap();
    Case { name: format!("EPSG:{code}"), crs, geographic, origin: (0.0, 0.0), opposite, rim: rim(a, rf), ellipsoid: rf != 0.0 }
}

fn proj(definition: &str, figure: &str, origin: (f64, f64), opposite: f64, (a, rf): (f64, f64)) -> Case {
    Case {
        name: format!("{definition} {figure}"),
        crs: SpatialRef::from_proj4(&format!("{definition} {figure} +units=m")).unwrap(),
        geographic: SpatialRef::from_proj4(&format!("+proj=longlat {figure}")).unwrap(),
        origin,
        opposite,
        rim: rim(a, rf),
        ellipsoid: rf != 0.0,
    }
}

/// The polar CRSs: on WGS 84, the North Pole LAEA (EPSG:3571 to 3576), the NSIDC EASE-Grid North
/// and South (EPSG:3973, 3974) and the EASE-Grid 2.0 North and South (EPSG:6931, 6932); PROJ
/// strings on other ellipsoids, north and south, one with a false origin; and on a sphere, the
/// NSIDC EASE-Grid North and South (EPSG:3408, 3409).
fn polar() -> Vec<Case> {
    let mut cases: Vec<Case> = (3571..=3576).map(|code| epsg(code, -90.0, WGS84)).collect();
    cases.extend([epsg(3973, -90.0, WGS84), epsg(3974, 90.0, WGS84), epsg(6931, -90.0, WGS84), epsg(6932, 90.0, WGS84)]);
    cases.push(proj("+proj=laea +lat_0=90 +lon_0=-45", "+ellps=GRS80", (0.0, 0.0), -90.0, (6_378_137.0, 298.257_222_101)));
    cases.push(proj("+proj=laea +lat_0=-90 +lon_0=40 +x_0=1000 +y_0=-2000", "+ellps=intl", (1000.0, -2000.0), 90.0, (6_378_388.0, 297.0)));
    cases.extend([epsg(3408, -90.0, (6_371_228.0, 0.0)), epsg(3409, 90.0, (6_371_228.0, 0.0))]);
    cases
}

/// Points at `rho` metres from the false origin, in eight directions.
fn around(c: &Case, rho: f64) -> Vec<(f64, f64)> {
    (0..8)
        .map(|i| {
            let t = (i as f64 * 45.0 + 10.0).to_radians();
            (c.origin.0 + rho * t.sin(), c.origin.1 + rho * t.cos())
        })
        .collect()
}

/// Infinite and NaN coordinates, alone and with a finite one.
fn non_finite(c: &Case) -> [(f64, f64); 5] {
    let (x0, y0) = c.origin;
    [(f64::INFINITY, f64::INFINITY), (f64::INFINITY, y0), (x0, f64::NEG_INFINITY), (f64::NAN, f64::NAN), (x0, f64::NAN)]
}

fn assert_refused(c: &Case, points: &[(f64, f64)]) {
    let inv = Transformer::new(&c.crs, &c.geographic).unwrap();
    for &(x, y) in points {
        let r = inv.transform(x, y);
        let rho = (x - c.origin.0).hypot(y - c.origin.1);
        assert!(r.is_err(), "{}: ({x}, {y}), {rho} m from the false origin, went to {r:?}", c.name);
    }
}

/// Coordinates beyond the rim fail to transform, from 0.13 µm beyond it (1e-14 of its radius) to
/// 1,000 km beyond it; so do infinite and NaN coordinates.
#[test]
fn polar_lambert_azimuthal_equal_area_refuses_coordinates_beyond_its_disk() {
    for c in polar() {
        let mut points = around(&c, c.rim * (1.0 + 1e-14));
        points.extend(around(&c, c.rim + 1_000_000.0));
        points.extend(non_finite(&c));
        assert_refused(&c, &points);
    }
}

/// On the ellipsoid, every point of the disk inverts, up to the rim: points from 0.1° from the
/// pole the CRS is centred on to 0.1° from the opposite pole, every 10° of longitude, come back
/// to within 1e-9° of themselves, and the projections of points 1e-7° and 1e-8° from the opposite
/// pole, from each degree of longitude, which lie on the rim to within the rounding of their
/// coordinates, come back to that pole. On the ellipsoid and the sphere, coordinates 1.3 cm within
/// the rim (1e-9 of its radius) invert to within 0.01° of the opposite pole.
#[test]
fn polar_lambert_azimuthal_equal_area_inverts_every_point_of_its_disk_on_the_ellipsoid() {
    for c in polar() {
        let fwd = Transformer::new(&c.geographic, &c.crs).unwrap();
        let inv = Transformer::new(&c.crs, &c.geographic).unwrap();
        for (x, y) in around(&c, c.rim * (1.0 - 1e-9)) {
            let (lon, lat) = inv.transform(x, y).unwrap_or_else(|e| panic!("{}: ({x}, {y}) failed: {e}", c.name));
            assert!(lon.is_finite() && (lat - c.opposite).abs() < 0.01, "{}: ({x}, {y}) went to ({lon}, {lat})", c.name);
        }
        if !c.ellipsoid {
            continue;
        }
        let towards = c.opposite.signum();
        for lon in (0..36).map(|l| l as f64 * 10.0 - 175.0) {
            for lat in [-89.9, -60.0, -30.0, 0.0, 30.0, 60.0, 89.9].map(|t| t * towards) {
                let (x, y) = fwd.transform(lon, lat).unwrap();
                let (back_lon, back_lat) = inv.transform(x, y).unwrap();
                assert!(
                    (back_lon - lon).abs() < 1e-9 && (back_lat - lat).abs() < 1e-9,
                    "{}: ({lon}, {lat}) came back as ({back_lon}, {back_lat})",
                    c.name
                );
            }
        }
        for near in [1e-7, 1e-8].map(|d| c.opposite - towards * d) {
            for lon in (0..360).map(|l| l as f64 - 180.0) {
                let (x, y) = fwd.transform(lon, near).unwrap_or_else(|e| panic!("{}: ({lon}, {near}) failed: {e}", c.name));
                let back = inv.transform(x, y);
                assert!(
                    back.as_ref().is_ok_and(|(back_lon, back_lat)| back_lon.is_finite() && (back_lat - c.opposite).abs() < 1e-3),
                    "{}: ({x}, {y}), from ({lon}, {near}), went to {back:?}",
                    c.name
                );
            }
        }
    }
}

/// The oblique and equatorial aspects refuse coordinates beyond their disk, and non-finite ones:
/// ETRS89-extended / LAEA Europe (EPSG:3035), and the equatorial aspect on WGS 84.
#[test]
fn oblique_and_equatorial_lambert_azimuthal_equal_area_refuse_coordinates_beyond_their_disk() {
    let mut europe = epsg(3035, 0.0, (6_378_137.0, 298.257_222_101));
    europe.origin = (4_321_000.0, 3_210_000.0);
    let equator = proj("+proj=laea +lat_0=0 +lon_0=0", "+datum=WGS84", (0.0, 0.0), 0.0, WGS84);
    for c in [europe, equator] {
        let mut points = around(&c, c.rim + 1_000_000.0);
        points.extend(non_finite(&c));
        assert_refused(&c, &points);
    }
}
