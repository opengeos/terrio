//! EPSG's null transformations to WGS 84 are reported by their names, as PROJ 9.9.0 reports
//! them, not as ballparks: they are transformations, with an accuracy, that move no point.

use terrio_crs::{SpatialRef, Transformer};

/// PROJ 9.9.0 (pyproj 3.8.0, network off): `Transformer.from_crs(CRS, "EPSG:4326")` at a point of
/// each CRS uses EPSG's null transformation ("ETRS89 to WGS 84 (1)", "NZGD2000 to WGS 84 (1)",
/// "REGVEN to WGS 84 (1)", and "ETRS89-SWE [SWEREF 99] to WGS 84 (1)", as PROJ 9.9.0 names it
/// since EPSG v13.102), and its inverse the other way. RustGIS
/// reads `ballpark` as "no transformation to WGS 84".
#[test]
fn null_transformations_are_named_not_ballparks() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    for (code, x, y, name) in [
        (3006, 609_924.0, 6_877_630.0, "ETRS89-SWE [SWEREF 99] to WGS 84 (1)"),
        (4258, 10.0, 50.0, "ETRS89 to WGS 84 (1)"),
        (4167, 174.0, -41.0, "NZGD2000 to WGS 84 (1)"),
        (4189, -66.0, 8.0, "REGVEN to WGS 84 (1)"),
    ] {
        let crs = SpatialRef::from_epsg(code).unwrap();
        let forward = Transformer::new(&crs, &wgs84).unwrap().operation_at(x, y, 0.0).unwrap().unwrap();
        assert_eq!((forward.name.as_str(), forward.ballpark, forward.accuracy), (name, false, Some(1.0)), "EPSG:{code}");
        let (lon, lat) = Transformer::new(&crs, &wgs84).unwrap().transform(x, y).unwrap();
        let back = Transformer::new(&wgs84, &crs).unwrap().operation_at(lon, lat, 0.0).unwrap().unwrap();
        assert_eq!((back.name, back.ballpark), (format!("Inverse of {name}"), false), "EPSG:{code}");
    }
    // A concatenation of null transformations, which PROJ names by its steps: ETRF89 (EPSG:9059)
    // to WGS 84 at (10.955, 58.995).
    let etrf89 = SpatialRef::from_epsg(9059).unwrap();
    let op = Transformer::new(&etrf89, &wgs84).unwrap().operation_at(10.955, 58.995, 0.0).unwrap().unwrap();
    assert_eq!((op.name.as_str(), op.ballpark), ("Inverse of ETRS89 to ETRF89 + ETRS89 to WGS 84 (1)", false));
}

/// The same for a WKT 2 projected CRS without a root ID whose base CRS has one (SWEREF99 TM as
/// PROJ writes it, without its `ID["EPSG",3006]` and `USAGE`): PROJ gives "Inverse of SWEREF99 to
/// WGS 84 (1) + SWEREF99 TM" from WGS 84 (terrio leaves the projection unnamed). Reported by
/// RustGIS.
#[test]
fn a_wkt2_projected_crs_on_a_null_transformation_datum_is_not_a_ballpark() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let wkt = r#"PROJCRS["SWEREF99 TM",BASEGEOGCRS["SWEREF99",DATUM["SWEREF99",ELLIPSOID["GRS 1980",6378137,298.257222101,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]],ID["EPSG",4619]],CONVERSION["SWEREF99 TM",METHOD["Transverse Mercator",ID["EPSG",9807]],PARAMETER["Latitude of natural origin",0,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8801]],PARAMETER["Longitude of natural origin",15,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8802]],PARAMETER["Scale factor at natural origin",0.9996,SCALEUNIT["unity",1],ID["EPSG",8805]],PARAMETER["False easting",500000,LENGTHUNIT["metre",1],ID["EPSG",8806]],PARAMETER["False northing",0,LENGTHUNIT["metre",1],ID["EPSG",8807]]],CS[Cartesian,2],AXIS["northing (N)",north,ORDER[1],LENGTHUNIT["metre",1]],AXIS["easting (E)",east,ORDER[2],LENGTHUNIT["metre",1]]]"#;
    let (crs, wgs84) = (SpatialRef::from_wkt(wkt), SpatialRef::from_epsg(4326).unwrap());
    let t = Transformer::new(&crs, &wgs84).unwrap();
    let op = t.operation_at(609_924.0, 6_877_630.0, 0.0).unwrap().unwrap();
    assert_eq!((op.name.as_str(), op.ballpark), ("ETRS89-SWE [SWEREF 99] to WGS 84 (1)", false));
    // From WGS 84, the inverse; terrio leaves the projection unnamed.
    let (lon, lat) = t.transform(609_924.0, 6_877_630.0).unwrap();
    let back = Transformer::new(&wgs84, &crs).unwrap().operation_at(lon, lat, 0.0).unwrap().unwrap();
    assert_eq!((back.name.as_str(), back.ballpark), ("Inverse of ETRS89-SWE [SWEREF 99] to WGS 84 (1)", false));
}
