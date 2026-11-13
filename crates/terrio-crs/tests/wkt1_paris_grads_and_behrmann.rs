//! WKT 1 quirks PROJ reads: Paris's longitude written in degrees in a CRS in grads, and ESRI's
//! Behrmann projection. The WKTs are PROJ 9.8.1's (pyproj 3.8.0, `CRS.to_wkt("WKT1_ESRI")` and
//! `"WKT1_GDAL"` without `AUTHORITY`) with their CRS and datum names replaced, so that terrio
//! reads them rather than identifying their codes. Reference values are PROJ's
//! `Transformer.from_crs(source, crs, always_xy=True)`, network off, no grids; GDAL 3.10.3's
//! `gdaltransform -s_srs <source> -t_srs <wkt>` (on PROJ 9.6.0) gives the same, to its 15
//! printed digits.

use terrio_crs::{SpatialRef, Transformer};

/// Transforms `(x, y)` from the CRS of the PROJ string `source` to the CRS of the WKT `wkt`.
fn transform(source: &str, wkt: &str, (x, y): (f64, f64)) -> (f64, f64) {
    let crs = SpatialRef::from_wkt(wkt);
    assert_eq!((crs.epsg, crs.esri), (None, None), "identified: {wkt}");
    Transformer::new(&SpatialRef::from_proj4(source).unwrap(), &crs).unwrap().transform(x, y).unwrap()
}

const PARIS_ESRI: &str = r#"PROJCS["custom",GEOGCS["custom",DATUM["custom",SPHEROID["Clarke_1880_IGN",6378249.2,293.466021293627]],PRIMEM["Paris",2.33722917],UNIT["Grad",0.0157079632679489]],PROJECTION["Lambert_Conformal_Conic"],PARAMETER["False_Easting",600000.0],PARAMETER["False_Northing",2200000.0],PARAMETER["Central_Meridian",0.0],PARAMETER["Standard_Parallel_1",52.0],PARAMETER["Scale_Factor",0.99987742],PARAMETER["Latitude_Of_Origin",52.0],UNIT["Meter",1.0]]"#;
const PARIS_GDAL: &str = r#"PROJCS["custom",GEOGCS["custom",DATUM["custom",SPHEROID["Clarke 1880 (IGN)",6378249.2,293.466021293627]],PRIMEM["Paris",2.33722917],UNIT["grad",0.0157079632679489]],PROJECTION["Lambert_Conformal_Conic_1SP"],PARAMETER["latitude_of_origin",52],PARAMETER["central_meridian",0],PARAMETER["scale_factor",0.99987742],PARAMETER["false_easting",600000],PARAMETER["false_northing",2200000],UNIT["metre",1],AXIS["Easting",EAST],AXIS["Northing",NORTH]]"#;
const PARIS_GEOGRAPHIC: &str = r#"GEOGCS["custom",DATUM["custom",SPHEROID["Clarke 1880 (IGN)",6378249.2,293.466021293627]],PRIMEM["Paris",2.33722917],UNIT["grad",0.0157079632679489]]"#;

#[test]
fn paris_in_degrees_in_a_crs_in_grads_is_read_as_proj_reads_it() {
    // GDAL and ESRI write Paris's longitude in degrees (2.33722917) whatever the CRS's angular
    // unit; PROJ reads it so. Read in grads it is 2.10° east, 0.23° off.
    let clarke = "+proj=longlat +ellps=clrk80ign";
    for wkt in [PARIS_ESRI, PARIS_GDAL] {
        let (x, y) = transform(clarke, wkt, (2.35, 48.85));
        // PROJ converts with the WKT's rounded size of a grad, terrio with the exact one.
        assert!((x - 600937.8045896705).abs() < 1e-3 && (y - 2427953.79931374).abs() < 1e-3, "({x}, {y}) {wkt}");
    }
    let (lon, lat) = transform(clarke, PARIS_GEOGRAPHIC, (2.35, 48.85));
    assert!((lon - 0.014189814814814933).abs() < 1e-8 && (lat - 54.27777777777778).abs() < 1e-9, "({lon}, {lat})");
}

#[test]
fn another_prime_meridian_in_a_crs_in_grads_is_in_grads() {
    // Only Paris's longitude in degrees is GDAL's: another one is in the CRS's unit, as in PROJ
    // (`+pm=2.25`, from 2.5 grads).
    let wkt = PARIS_GEOGRAPHIC.replace(r#"PRIMEM["Paris",2.33722917]"#, r#"PRIMEM["custom",2.5]"#);
    let (lon, _) = transform("+proj=longlat +ellps=clrk80ign", &wkt, (2.35, 48.85));
    assert!((lon - 0.11111111111111126).abs() < 1e-9, "{lon}");
    // PROJ matches the name exactly: `paris` is in grads (2.10°), 0.23° east of Paris.
    let lower = PARIS_GEOGRAPHIC.replace(r#"PRIMEM["Paris""#, r#"PRIMEM["paris""#);
    let (lon, _) = transform("+proj=longlat +ellps=clrk80ign", &lower, (2.35, 48.85));
    assert!((lon - (2.35 - 2.33722917 * 0.9) / 0.9).abs() < 1e-8, "{lon}");
}

#[test]
fn esri_behrmann_is_cylindrical_equal_area() {
    // ESRI:54017's Behrmann, true at 30°, and EASE-Grid 2.0 (EPSG:6933) as ESRI writes it, with
    // its standard parallel: PROJ's `+proj=cea +lat_ts=30`.
    let behrmann = r#"PROJCS["custom",GEOGCS["custom",DATUM["custom",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Behrmann"],PARAMETER["False_Easting",0.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",0.0],UNIT["Meter",1.0]]"#;
    let ease = behrmann.replace(r#"UNIT["Meter""#, r#"PARAMETER["Standard_Parallel_1",30.0],UNIT["Meter""#);
    for wkt in [behrmann, ease.as_str()] {
        let (x, y) = transform("+proj=longlat +ellps=WGS84", wkt, (10.0, 45.0));
        assert!((x - 964862.802508965).abs() < 1e-6 && (y - 5180102.328839251).abs() < 1e-6, "({x}, {y}) {wkt}");
    }
    // The central meridian and false easting apply: PROJ's `+lon_0=10 +x_0=500`.
    let shifted = behrmann
        .replace(r#"PARAMETER["False_Easting",0.0]"#, r#"PARAMETER["False_Easting",500.0]"#)
        .replace(r#"PARAMETER["Central_Meridian",0.0]"#, r#"PARAMETER["Central_Meridian",10.0]"#);
    let (x, y) = transform("+proj=longlat +ellps=WGS84", &shifted, (20.0, 45.0));
    assert!((x - 965362.802508965).abs() < 1e-6 && (y - 5180102.328839251).abs() < 1e-6, "({x}, {y})");
    // Another standard parallel is kept: PROJ's `+proj=cea +lat_ts=45`.
    let at_45 = behrmann.replace(r#"UNIT["Meter""#, r#"PARAMETER["Standard_Parallel_1",45.0],UNIT["Meter""#);
    let (x, y) = transform("+proj=longlat +ellps=WGS84", &at_45, (10.0, 45.0));
    assert!((x - AT_45.0).abs() < 1e-6 && (y - AT_45.1).abs() < 1e-6, "({x}, {y})");
}

/// PROJ's `+proj=cea +lat_ts=45` coordinates of (10°, 45°) on WGS 84.
const AT_45: (f64, f64) = (788468.3509397812, 6338983.732612473);
