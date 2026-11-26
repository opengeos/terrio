//! The coordinate operation a transformer uses for a point, against PROJ 9.9's first candidate
//! (`projinfo -s A -t B --spatial-test intersects`) without grids. PROJ also names the
//! conversions (`Inverse of British National Grid + ...`); terrio names the transformations.

use terrio_crs::{CoordinateOperation, SpatialRef, TransformOptions, Transformer};

fn no_grids() {
    terrio_crs::set_grid_search_paths(Some(Vec::new()));
    terrio_crs::set_network_enabled(Some(false));
}

fn operation(src: &str, dst: &str, x: f64, y: f64, z: f64) -> Option<CoordinateOperation> {
    let t = Transformer::new(&SpatialRef::parse(src).unwrap(), &SpatialRef::parse(dst).unwrap()).unwrap();
    t.operation_at(x, y, z).unwrap()
}

#[test]
fn reports_the_transformation_used() {
    no_grids();
    let op = operation("EPSG:27700", "EPSG:4326", 530000.0, 180000.0, 0.0).unwrap();
    assert_eq!((op.name.as_str(), op.accuracy, op.ballpark), ("OSGB36 to WGS 84 (6)", Some(2.0), false));
    assert!(op.grids.is_empty());
    let op = operation("EPSG:4326", "EPSG:27700", -0.13, 51.5, 0.0).unwrap();
    assert_eq!((op.name.as_str(), op.accuracy), ("Inverse of OSGB36 to WGS 84 (6)", Some(2.0)));
    let op = operation("EPSG:31467", "EPSG:4326", 3500000.0, 5500000.0, 0.0).unwrap();
    assert_eq!((op.name.as_str(), op.accuracy), ("DHDN to WGS 84 (2)", Some(3.0)));
}

#[test]
fn reports_ballparks_and_no_operation() {
    no_grids();
    let op = operation("EPSG:4610", "EPSG:4326", 110.0, 35.0, 0.0).unwrap();
    // Named as PROJ 9.9.0 names it, from the geographic CRSs (#1012 item 54).
    assert_eq!(
        op,
        CoordinateOperation {
            name: "Ballpark geographic offset from Xian 1980 to WGS 84".into(),
            accuracy: None,
            grids: Vec::new(),
            ballpark: true
        }
    );
    // EGM96's grid is missing: heights are kept, by PROJ 9.9.0's ballpark of that name.
    let op = operation("EPSG:4326+5773", "EPSG:4979", 153.0, -27.0, 0.0).unwrap();
    let name = "Transformation from EGM96 height to WGS 84 (ballpark vertical transformation, without ellipsoid height to vertical height correction)";
    assert_eq!((op.name.as_str(), op.ballpark), (name, true));
    // One datum: no transformation.
    assert_eq!(operation("EPSG:4326", "EPSG:3857", 10.0, 45.0, 0.0), None);
    assert_eq!(operation("EPSG:4326", "EPSG:4326", 10.0, 45.0, 0.0), None);
    // A +towgs84 definition has no name or accuracy.
    let op = operation("+proj=longlat +ellps=intl +towgs84=-87,-98,-121", "EPSG:4326", 5.0, 5.0, 0.0).unwrap();
    assert_eq!((op.name.as_str(), op.accuracy, op.ballpark), ("Transformation to WGS 84", None, false));
}

#[test]
fn fails_as_the_transformation_fails() {
    no_grids();
    let (src, dst) = (SpatialRef::parse("EPSG:4326+5773").unwrap(), SpatialRef::parse("EPSG:4979").unwrap());
    let opts = TransformOptions { only_best: true, ..Default::default() };
    let t = Transformer::with_options(&src, &dst, &opts).unwrap();
    assert_eq!(t.transform_3d(153.0, -27.0, 0.0).is_err(), t.operation_at(153.0, -27.0, 0.0).is_err());
    let t = Transformer::new(&SpatialRef::parse("EPSG:4326").unwrap(), &SpatialRef::parse("EPSG:3857").unwrap()).unwrap();
    assert!(t.operation_at(0.0, 95.0, 0.0).is_err());
}
