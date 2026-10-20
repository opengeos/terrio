//! The geostationary satellite view (`+proj=geos`) against PROJ, for both sweep axes: GOES-16
//! (`+sweep=x`, GRS80) and Meteosat SEVIRI (`+sweep=y`, its own ellipsoid). The reference points
//! are pyproj's (PROJ 9.7.1), from the geographic CRS on the same ellipsoid, so that no datum
//! transformation is involved.

use terrio::crs::{SpatialRef, Transformer};

const GOES: &str = "+proj=geos +h=35786023 +lon_0=-75 +sweep=x +ellps=GRS80 +units=m +no_defs";
const GOES_GEOG: &str = "+proj=longlat +ellps=GRS80 +no_defs";

const SEVIRI: &str = "+proj=geos +h=35785831 +lon_0=0 +sweep=y +a=6378169 +b=6356583.8 +units=m +no_defs";
const SEVIRI_GEOG: &str = "+proj=longlat +a=6378169 +b=6356583.8 +no_defs";

/// Longitude and latitude in degrees and PROJ's x and y in metres, from nadir to near the limb.
const GOES_POINTS: &[(f64, f64, f64, f64)] = &[
    (-75.0, 0.0, 0.0, 0.0),
    (-74.0, 0.5, 111305.36574654478, 55284.532964953985),
    (-70.0, 5.0, 552934.1056737453, 551396.2299694433),
    (-90.0, 0.0, -1639670.198596629, 0.0),
    (-75.0, -15.0, 0.0, -1629136.0730140982),
    (-55.0, 35.0, 1709204.1237692675, 3483923.2637939486),
    (-105.0, -40.0, -2291893.2531235996, -3833062.4133029096),
    (-30.0, 10.0, 4192877.523456901, 1043479.831017525),
    (-135.0, 20.0, -4709774.753098331, 1978596.4061921614),
    (-5.0, -5.0, 5303340.1016310975, -494092.038764708),
    (-65.0, 75.0, 251009.50191380794, 5378906.753128008),
    (-115.0, -65.0, -1534075.7246537933, -5104219.254082671),
    (3.0, 0.0, 5425023.24926228, 0.0),
    (-20.0, 55.0, 2655764.883775899, 4620477.366198244),
];

const SEVIRI_POINTS: &[(f64, f64, f64, f64)] = &[
    (0.0, 0.0, 0.0, 0.0),
    (1.0, 0.5, 111306.05726802953, 55281.05726193307),
    (5.0, 5.0, 553002.6586090655, 551298.5599665294),
    (-15.0, 0.0, -1639678.2986153536, 0.0),
    (0.0, -15.0, 0.0, -1629045.541578382),
    (20.0, 35.0, 1717351.513809403, 3479775.726167576),
    (-30.0, -40.0, -2305110.3781973496, -3825005.6867915546),
    (45.0, 10.0, 4194667.166002093, 1036264.0035360178),
    (-60.0, 20.0, -4716936.884713916, 1961360.1333948574),
    (70.0, -5.0, 5303859.66907667, -488647.12599109),
    (10.0, 75.0, 253880.5203321382, 5378621.102131265),
    (-40.0, -65.0, -1549842.1156801214, -5099336.579691021),
    (78.0, 0.0, 5425042.746693887, 0.0),
    (55.0, 55.0, 2678042.931082768, 4607519.426421587),
];

/// Points the satellite cannot see, relative to the sub-satellite longitude: PROJ refuses them.
const HIDDEN: &[(f64, f64)] = &[(85.0, 0.0), (0.0, 85.0), (-120.0, 10.0), (180.0, 0.0)];

/// Coordinates off the Earth's disk: PROJ refuses them.
const OFF_DISK: &[(f64, f64)] = &[(5.6e6, 0.0), (0.0, 5.6e6), (4.2e6, 4.2e6)];

fn check(geos: &str, geog: &str, lon0: f64, points: &[(f64, f64, f64, f64)]) {
    let geos = SpatialRef::from_proj4(geos).unwrap();
    let geog = SpatialRef::from_proj4(geog).unwrap();
    let fwd = Transformer::new(&geog, &geos).unwrap();
    let inv = Transformer::new(&geos, &geog).unwrap();
    for &(lon, lat, x, y) in points {
        let (tx, ty) = fwd.transform(lon, lat).unwrap();
        assert!((tx - x).abs() < 1e-3 && (ty - y).abs() < 1e-3, "forward ({lon}, {lat}): ({tx}, {ty}), PROJ ({x}, {y})");
        let (tlon, tlat) = inv.transform(x, y).unwrap();
        assert!((tlon - lon).abs() < 1e-8 && (tlat - lat).abs() < 1e-8, "inverse ({x}, {y}): ({tlon}, {tlat}), PROJ ({lon}, {lat})");
    }
    for &(dlon, lat) in HIDDEN {
        let r = fwd.transform(lon0 + dlon, lat);
        assert!(r.is_err() || !r.unwrap().0.is_finite(), "forward of hidden ({}, {lat}) succeeded", lon0 + dlon);
    }
    for &(x, y) in OFF_DISK {
        let r = inv.transform(x, y);
        assert!(r.is_err() || !r.unwrap().0.is_finite(), "inverse of off-disk ({x}, {y}) succeeded");
    }
}

#[test]
fn goes16_sweep_x_matches_proj() {
    check(GOES, GOES_GEOG, -75.0, GOES_POINTS);
}

#[test]
fn seviri_sweep_y_matches_proj() {
    check(SEVIRI, SEVIRI_GEOG, 0.0, SEVIRI_POINTS);
}
