//! Deleted Shapefile DBF slots, compared with GDAL 3.13.3 semantics.
use terrio::{Feature, FieldDef, FieldType, FieldValue, Geometry, Layer};
#[test]
fn deleted_slots_keep_public_feature_ids() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("deleted.shp");
    let mut layer = Layer::new("deleted");
    layer.add_field(FieldDef::new("id", FieldType::Integer));
    for i in 0..6 {
        layer.push(Feature {
            fid: i,
            geometry: Some(Geometry::point(i as f64, 0.0)),
            attributes: vec![FieldValue::Integer(9007199254740993 + i as i64)],
        });
    }
    let path = path.to_str().unwrap();
    terrio::write_vector(&[layer.clone()], path, &Default::default()).unwrap();
    let dbf_path = std::path::Path::new(path).with_extension("dbf");
    let original = std::fs::read(&dbf_path).unwrap();
    let header = u16::from_le_bytes([original[8], original[9]]) as usize;
    let width = u16::from_le_bytes([original[10], original[11]]) as usize;
    for deleted in [vec![0, 2, 3, 5], vec![0, 1, 2, 3, 4, 5], vec![]] {
        let mut dbf = original.clone();
        for &i in &deleted {
            dbf[header + i * width] = b'*';
        }
        std::fs::write(&dbf_path, dbf).unwrap();
        let data = terrio::open_vector(path, &Default::default()).unwrap();
        let actual = &data.layers[0].features;
        let expected: Vec<_> = layer.features.iter().filter(|f| !deleted.contains(&(f.fid as usize))).collect();
        assert_eq!(actual.len(), expected.len());
        for (got, want) in actual.iter().zip(expected) {
            assert_eq!(got.fid, want.fid);
            assert_eq!(got.geometry, want.geometry);
            assert_eq!(got.attributes, want.attributes);
        }
    }
}
