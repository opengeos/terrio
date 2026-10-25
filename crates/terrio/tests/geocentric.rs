//! Shared Terrio surface preserves all Cartesian ordinates and measures.
use terrio::crs::SpatialRef;
use wbvector::{Coord, Feature, Geometry, Layer};

#[test]
fn vector_reprojection_retains_xyz_and_measure() {
    let mut layer = Layer::new("positions");
    layer.set_crs_epsg(Some(4979));
    layer.push(Feature {
        fid: 7,
        geometry: Some(Geometry::Point(Coord { x: 147.0, y: -42.0, z: Some(100.0), m: Some(42.0) })),
        attributes: vec![],
    });
    let output = terrio::vector::reproject_layer(&layer, &SpatialRef::from_epsg(4978).unwrap()).unwrap();
    let Geometry::Point(point) = output.features[0].geometry.as_ref().unwrap() else { panic!() };
    assert!((point.x + 3981230.6461390946).abs() < 0.001);
    assert!((point.y - 2585441.4118733606).abs() < 0.001);
    assert!((point.z.unwrap() + 4245670.74916175).abs() < 0.001);
    assert_eq!(point.m, Some(42.0));
    let back = terrio::vector::reproject_layer(&output, &SpatialRef::from_epsg(4979).unwrap()).unwrap();
    let Geometry::Point(p) = back.features[0].geometry.as_ref().unwrap() else { panic!() };
    assert!((p.x - 147.0).abs() < 1e-8 && (p.y + 42.0).abs() < 1e-8 && (p.z.unwrap() - 100.0).abs() < 0.001);
}
