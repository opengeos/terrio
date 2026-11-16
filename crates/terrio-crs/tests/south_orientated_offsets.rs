//! South-orientated Transverse Mercator (EPSG method 9808) subtracts its false easting and
//! northing: W = FE - x, S = FN - y (EPSG Guidance Note 7-2), where x and y are Transverse
//! Mercator's easting and northing without offsets. PROJ 9.8.1 reads such a CRS but refuses to
//! transform it or write it as a PROJ string when the offsets are not zero (opengeos/terrio#1012,
//! item 18); terrio applies the method, a documented difference, and writes no PROJ string.
//!
//! Reference: the same CRS without offsets, which terrio places as PROJ does
//! (`+proj=tmerc +axis=wsu`), with the offsets subtracted. Tolerance: 1 mm.

use terrio_crs::{SpatialRef, Transformer};

fn esri(fe: f64, fn_: f64) -> String {
    format!(
        r#"PROJCS["Lo19",GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator_South_Orientated"],PARAMETER["False_Easting",{fe}],PARAMETER["False_Northing",{fn_}],PARAMETER["Central_Meridian",19.0],PARAMETER["Scale_Factor",1.0],PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]"#
    )
}

fn wkt2(fe: f64, fn_: f64) -> String {
    let deg = r#"ANGLEUNIT["degree",0.0174532925199433]"#;
    format!(
        r#"PROJCRS["Lo19",BASEGEOGCRS["WGS 84",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,{deg}]],CONVERSION["c",METHOD["Transverse Mercator (South Orientated)",ID["EPSG",9808]],PARAMETER["Latitude of natural origin",0,{deg},ID["EPSG",8801]],PARAMETER["Longitude of natural origin",19,{deg},ID["EPSG",8802]],PARAMETER["Scale factor at natural origin",1,SCALEUNIT["unity",1],ID["EPSG",8805]],PARAMETER["False easting",{fe},LENGTHUNIT["metre",1],ID["EPSG",8806]],PARAMETER["False northing",{fn_},LENGTHUNIT["metre",1],ID["EPSG",8807]]],CS[Cartesian,2],AXIS["westing (Y)",west,ORDER[1],LENGTHUNIT["metre",1]],AXIS["southing (X)",south,ORDER[2],LENGTHUNIT["metre",1]]]"#
    )
}

#[test]
fn south_orientated_transverse_mercator_subtracts_its_offsets() {
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let (fe, fn_) = (1000.0, 2000.0);
    let points = [(19.0, 0.0), (18.5, -33.9), (20.2, -30.1)];
    for (form, build) in [("WKT 1", esri as fn(f64, f64) -> String), ("WKT 2", wkt2)] {
        let plain = Transformer::new(&wgs84, &SpatialRef::from_wkt(&build(0.0, 0.0))).unwrap();
        let crs = SpatialRef::from_wkt(&build(fe, fn_));
        let t = Transformer::new(&wgs84, &crs).unwrap();
        for (lon, lat) in points {
            let (x, y) = plain.transform(lon, lat).unwrap();
            let got = t.transform(lon, lat).unwrap();
            let want = (fe + x, fn_ + y);
            assert!((got.0 - want.0).abs() < 1e-3 && (got.1 - want.1).abs() < 1e-3, "{form} ({lon}, {lat}): {got:?}, {want:?}");
        }
        // The natural origin is (FE, FN).
        let origin = t.transform(19.0, 0.0).unwrap();
        assert!((origin.0 - fe).abs() < 1e-6 && (origin.1 - fn_).abs() < 1e-6, "{form}: {origin:?}");
        assert_eq!(crs.to_proj4(), None, "{form}");
        assert!(SpatialRef::from_wkt(&build(0.0, 0.0)).to_proj4().is_some_and(|p| p.contains("+axis=wsu")), "{form}");
        // Written as WKT 1 and as GeoKeys, it reads back in place. A WKT 2 is written as WKT 1
        // through its PROJ string, which it has none of: no strict WKT 1 rather than a wrong one.
        let back = |c: &SpatialRef| Transformer::new(&wgs84, c).unwrap().transform(18.5, -33.9).unwrap();
        let want = back(&crs);
        match crs.to_wkt1_strict() {
            Some(w) => {
                let got = back(&SpatialRef::from_wkt(&w));
                assert!((got.0 - want.0).abs() < 1e-3 && (got.1 - want.1).abs() < 1e-3, "{form}, WKT 1: {got:?}, {want:?}");
            }
            None => assert_eq!(form, "WKT 2"),
        }
        let keys = crs.to_geokeys().unwrap().encode().unwrap();
        let read = SpatialRef::from_geokeys(&keys.directory, &keys.doubles, keys.ascii.as_bytes()).unwrap();
        let got = back(&read);
        assert!((got.0 - want.0).abs() < 1e-3 && (got.1 - want.1).abs() < 1e-3, "{form}, GeoKeys: {got:?}, {want:?}");
    }
}
