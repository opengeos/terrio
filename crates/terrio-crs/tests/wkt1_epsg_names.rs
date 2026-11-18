//! A WKT 1 projected CRS whose parameters have EPSG's names (`Latitude of false origin`,
//! `Longitude of projection centre`, `Angle from Rectified to Skew Grid`, `Latitude of standard
//! parallel`) is read as PROJ 9.9.0 reads it, not without them; one PROJ refuses is refused, as is
//! GDAL's `Polar_Stereographic` without `latitude_of_origin`. A `PROJCS` with two `UNIT` nodes is
//! in the first, as PROJ takes it (opengeos/terrio#1012, items 25 and 31).
//!
//! `data/wkt1_epsg_names.json` (`scripts/gen_wkt1_epsg_names.py`, pyproj 3.8.0, `PROJ_NETWORK=OFF`)
//! holds PROJ's point from WGS 84 (`always_xy`), `null` where PROJ refuses. Tolerance: 1 mm.

use serde_json::Value;
use terrio_crs::{SpatialRef, Transformer};

#[test]
fn epsg_parameter_names_in_wkt1_are_read_as_proj_reads_them() {
    let data: Value = serde_json::from_str(include_str!("data/wkt1_epsg_names.json")).unwrap();
    let cases = data["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 12);
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let crs = SpatialRef::from_wkt(case["wkt"].as_str().unwrap());
        let (lon, lat) = (case["point"][0].as_f64().unwrap(), case["point"][1].as_f64().unwrap());
        let got = Transformer::new(&wgs84, &crs).and_then(|t| t.transform(lon, lat));
        match case["proj"].as_array() {
            Some(p) => {
                let (x, y) = got.unwrap_or_else(|e| panic!("{name}: {e}"));
                let (px, py) = (p[0].as_f64().unwrap(), p[1].as_f64().unwrap());
                assert!((x - px).abs() < 1e-3 && (y - py).abs() < 1e-3, "{name}: ({x}, {y}), PROJ ({px}, {py})");
            }
            None => assert!(got.is_err(), "{name}: {got:?}"),
        }
    }
}

/// A two-point Hotine oblique Mercator with a malformed latitude under an EPSG name is refused, not
/// read with a zero default.
#[test]
fn a_malformed_epsg_named_latitude_of_two_point_hotine_is_refused() {
    for name in ["Latitude of false origin", "Latitude of projection centre"] {
        let wkt = format!(
            r#"PROJCS["x",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Hotine_Oblique_Mercator_Two_Point_Natural_Origin"],PARAMETER["{name}",NaN],PARAMETER["latitude_of_point_1",30],PARAMETER["longitude_of_point_1",0],PARAMETER["latitude_of_point_2",50],PARAMETER["longitude_of_point_2",20],PARAMETER["scale_factor",1],PARAMETER["false_easting",0],PARAMETER["false_northing",0],UNIT["metre",1]]"#
        );
        let wgs84 = SpatialRef::from_epsg(4326).unwrap();
        let place = |wkt: &str| Transformer::new(&wgs84, &SpatialRef::from_wkt(wkt)).and_then(|t| t.transform(10.0, 40.0));
        assert!(place(&wkt.replace("NaN", "40")).is_ok(), "{name}");
        assert!(place(&wkt).is_err(), "{name}");
    }
}

/// GDAL's `Polar_Stereographic` with a `latitude_of_origin` of NaN, or under EPSG's variant B a
/// standard parallel of NaN, is refused, not read at the north pole; with or without the
/// `wbprojection-fallback` feature, whose parser would otherwise read it.
#[test]
fn a_polar_stereographic_latitude_of_nan_is_refused() {
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let place = |wkt: &str| Transformer::new(&wgs84, &SpatialRef::from_wkt(wkt)).and_then(|t| t.transform(0.0, -75.0));
    let gdal = r#"PROJCS["x",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Polar_Stereographic"],PARAMETER["latitude_of_origin",-71],PARAMETER["central_meridian",0],PARAMETER["scale_factor",1],PARAMETER["false_easting",0],PARAMETER["false_northing",0],UNIT["metre",1]]"#;
    assert!(place(gdal).is_ok());
    assert!(place(&gdal.replace("-71", "NaN")).is_err());
    let variant_b = gdal
        .replace("Polar_Stereographic", "Polar Stereographic (variant B)")
        .replace("latitude_of_origin", "Latitude of standard parallel");
    assert!(place(&variant_b).is_ok());
    assert!(place(&variant_b.replace("-71", "NaN")).is_err());
    // Not a number at all, which PROJ 9.9.0 refuses too.
    assert!(place(&variant_b.replace("-71", r#""abc""#)).is_err());
    // Without its latitude of origin.
    assert!(place(&gdal.replace(r#"PARAMETER["latitude_of_origin",-71],"#, "")).is_err());
}
