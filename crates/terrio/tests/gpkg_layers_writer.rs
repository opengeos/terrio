//! Public multi-layer streaming API; output semantics follow GDAL 3.13.3 GeoPackage.
#![cfg(feature = "sqlite")]

use terrio::stream::{GpkgLayersWriter, schema_of};
use terrio::{Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer};

#[test]
fn interleaved_layers_and_empty_tables_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("layers.gpkg");
    let schema = |name: &str, kind| {
        let mut l = Layer::new(name).with_epsg(4326).with_geom_type(kind);
        l.add_field(FieldDef::new("n", FieldType::Integer));
        l
    };
    let points = schema("points", GeometryType::Point);
    let lines = schema("lines", GeometryType::LineString);
    let empty = schema("empty", GeometryType::Polygon);
    let mut writer = GpkgLayersWriter::create(path.to_str().unwrap(), &[points.clone(), lines.clone(), empty]).unwrap();
    let chunk = |schema: &Layer, n, geometry| {
        let mut c = schema_of(schema);
        c.push(Feature { fid: 99, geometry: Some(geometry), attributes: vec![FieldValue::Integer(n)] });
        c
    };
    assert!(writer.write(3, &points).is_err());
    let mut bad = points.clone();
    bad.add_field(FieldDef::new("extra", FieldType::Text));
    assert!(writer.write(0, &bad).is_err());
    writer.write(0, &chunk(&points, 1, Geometry::point(30.0, -2.0))).unwrap();
    writer
        .write(1, &chunk(&lines, 2, Geometry::LineString(vec![wbvector::Coord::xy(30.0, -2.0), wbvector::Coord::xy(30.1, -2.0)])))
        .unwrap();
    writer.write(0, &chunk(&points, 5_000_000_000, Geometry::point(30.2, -2.0))).unwrap();
    writer.finish().unwrap();
    let layers = terrio::open_vector(path.to_str().unwrap(), &Default::default()).unwrap().layers;
    assert_eq!(layers.iter().map(|l| (l.name.as_str(), l.features.len())).collect::<Vec<_>>(), [("points", 2), ("lines", 1), ("empty", 0)]);
    assert_eq!(layers[0].features[1].attributes, [FieldValue::Integer(5_000_000_000)]);
    assert_eq!(layers[0].features.iter().map(|f| f.fid).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(layers[0].features[0].geometry, Some(Geometry::point(30.0, -2.0)));
}

#[test]
fn unsupported_schemas_do_not_truncate_existing_output() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("existing.gpkg");
    std::fs::write(&path, b"existing").unwrap();
    let mut fid = Layer::new("points").with_epsg(4326);
    fid.add_field(FieldDef::new("FID", FieldType::Integer));
    for schemas in [
        vec![fid],
        vec![Layer::new("unknown")],
        vec![Layer::new("same").with_epsg(4326); 2],
        vec![Layer::new("gpkg_contents").with_epsg(4326)],
        vec![Layer::new("GPKG_CONTENTS").with_epsg(4326)],
        vec![Layer::new("sqlite_sequence").with_epsg(4326)],
    ] {
        assert!(GpkgLayersWriter::create(path.to_str().unwrap(), &schemas).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"existing");
    }
}
