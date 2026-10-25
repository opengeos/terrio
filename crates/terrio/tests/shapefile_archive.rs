//! Filesystem-free public archive writer, native/WASM read-back and native member parity.
use std::io::{Cursor, Read};
use terrio::memory::{open_vector_bytes, write_shapefile_zip_bytes, write_vector_bytes};
use terrio::{FieldValue, VectorWriteOptions};

fn source(geometry: &str) -> terrio::VectorDataset {
    let json = format!(
        r#"{{"type":"FeatureCollection","features":[
      {{"type":"Feature","properties":{{"name":"Zürich","id":12345678901,"real":2.5,"flag":true,"date":"2020-02-29"}},"geometry":{geometry}}},
      {{"type":"Feature","properties":{{"name":null,"id":null,"real":null,"flag":null,"date":null}},"geometry":null}}
    ]}}"#
    );
    open_vector_bytes(json.into_bytes(), "source.geojson", &Default::default()).unwrap()
}

#[test]
fn shapefile_zip_public_round_trips() {
    for geometry in [
        r#"{"type":"Point","coordinates":[8.5,47.4]}"#,
        r#"{"type":"Point","coordinates":[8.5,47.4,123.5]}"#,
        r#"{"type":"LineString","coordinates":[[0,0],[2,3],[5,1]]}"#,
        r#"{"type":"Polygon","coordinates":[[[0,0],[0,5],[5,5],[5,0],[0,0]],[[1,1],[2,1],[2,2],[1,2],[1,1]]]}"#,
    ] {
        let input = source(geometry);
        let bytes = write_vector_bytes(&input.layers, "places.shp.zip", &Default::default()).unwrap();
        assert!(bytes.starts_with(b"PK"));
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes.clone())).unwrap();
        let mut names: Vec<_> = zip.file_names().map(|n| n.unwrap().to_string()).collect();
        names.sort();
        assert_eq!(names, ["places.cpg", "places.dbf", "places.prj", "places.shp", "places.shx"]);
        assert_eq!(zip.by_name("places.cpg").unwrap().size(), 5);
        let back = open_vector_bytes(bytes, "places.shp.zip", &Default::default()).unwrap();
        let layer = &back.layers[0];
        assert_eq!(layer.name, "places");
        assert_eq!(layer.crs_epsg(), Some(4326));
        assert_eq!(layer.features.len(), 2);
        assert_eq!(layer.features[0].geometry, input.layers[0].features[0].geometry);
        assert_eq!(layer.features[0].attributes, input.layers[0].features[0].attributes);
        assert!(layer.features[1].geometry.is_none());
        assert!(layer.features[1].attributes.iter().all(|v| *v == FieldValue::Null));
        assert_eq!(
            write_shapefile_zip_bytes(&input.layers[0], "places").unwrap(),
            write_vector_bytes(&input.layers, "places.shp.zip", &Default::default()).unwrap()
        );
        // The same encoder's disk members provide an independent native path comparison.
        #[cfg(not(target_family = "wasm"))]
        {
            let dir = tempfile::tempdir().unwrap();
            terrio::write_vector(&input.layers, dir.path().join("places.shp").to_str().unwrap(), &Default::default()).unwrap();
            for name in names {
                let mut contents = Vec::new();
                zip.by_name(&name).unwrap().read_to_end(&mut contents).unwrap();
                assert_eq!(contents, std::fs::read(dir.path().join(name)).unwrap());
            }
        }
    }
}

#[test]
fn shapefile_archive_refuses_implicit_or_lossy_output() {
    let mut input = source(r#"{"type":"Point","coordinates":[1,2]}"#);
    assert!(write_vector_bytes(&input.layers, "p.shp", &Default::default()).unwrap_err().to_string().contains(".shp.zip"));
    assert!(write_vector_bytes(&[], "p.shp.zip", &Default::default()).is_err());
    assert!(write_vector_bytes(&[input.layers[0].clone(), input.layers[0].clone()], "p.shp.zip", &Default::default()).is_err());
    assert!(write_shapefile_zip_bytes(&input.layers[0], "../escape").is_err());
    let options = VectorWriteOptions { driver: Some(terrio::formats::VectorDriver::Shapefile), ..Default::default() };
    assert!(write_vector_bytes(&input.layers, "p.zip", &options).is_ok());
    let name = input.layers[0].schema.fields().iter().position(|f| f.name == "name").unwrap();
    for text in [format!("https://example.org/{}", "x".repeat(254)), "é".repeat(128), "trailing ".into(), "".into()] {
        input.layers[0].features[0].attributes[name] = FieldValue::Text(text);
        assert!(write_vector_bytes(&input.layers, "p.shp.zip", &Default::default()).is_err());
    }
    // An ordinary asterisk is text, not DBF's numeric null marker.
    input.layers[0].features[0].attributes[name] = FieldValue::Text("*".into());
    let bytes = write_vector_bytes(&input.layers, "p.shp.zip", &Default::default()).unwrap();
    let back = open_vector_bytes(bytes, "p.zip", &Default::default()).unwrap();
    assert_eq!(back.layers[0].features[0].attributes[name], FieldValue::Text("*".into()));
}

#[test]
fn text_boundary_and_unsupported_geometry() {
    let mut input = source(r#"{"type":"Point","coordinates":[1,2]}"#);
    let name = input.layers[0].schema.fields().iter().position(|f| f.name == "name").unwrap();
    input.layers[0].features[0].attributes[name] = FieldValue::Text("é".repeat(127));
    let bytes = write_vector_bytes(&input.layers, "p.shp.zip", &Default::default()).unwrap();
    let back = open_vector_bytes(bytes, "p.zip", &Default::default()).unwrap();
    assert_eq!(back.layers[0].features[0].attributes[name], FieldValue::Text("é".repeat(127)));
    input.layers[0].features[0].geometry = Some(terrio::Geometry::Point(wbvector::Coord { x: 1.0, y: 2.0, z: None, m: Some(3.0) }));
    assert!(write_vector_bytes(&input.layers, "p.shp.zip", &Default::default()).unwrap_err().to_string().contains("M coordinates"));
    input.layers[0].features[0].geometry = Some(terrio::Geometry::GeometryCollection(vec![]));
    assert!(write_vector_bytes(&input.layers, "p.shp.zip", &Default::default()).is_err());
}

#[test]
fn unrepresentable_fields_and_mixed_geometry_are_errors() {
    let mut input = source(r#"{"type":"Point","coordinates":[1,2]}"#);
    let id = input.layers[0].schema.fields().iter().position(|f| f.name == "id").unwrap();
    input.layers[0].features[0].attributes[id] = FieldValue::Integer(i64::MAX);
    assert!(write_vector_bytes(&input.layers, "p.shp.zip", &Default::default()).unwrap_err().to_string().contains("18 decimal columns"));
    input.layers[0].features[0].attributes[id] = FieldValue::Integer(1);
    input.layers[0].features[1].geometry =
        Some(terrio::Geometry::LineString(vec![wbvector::Coord::xy(0.0, 0.0), wbvector::Coord::xy(1.0, 1.0)]));
    assert!(write_vector_bytes(&input.layers, "p.shp.zip", &Default::default()).unwrap_err().to_string().contains("mixes types"));
    input.layers[0].features[1].geometry = None;
    input.layers[0].add_field(terrio::FieldDef::new("blob", terrio::FieldType::Blob));
    assert!(write_vector_bytes(&input.layers, "p.shp.zip", &Default::default()).unwrap_err().to_string().contains("scalars only"));
}

#[test]
fn z_only_records_omit_optional_measure_blocks() {
    for geometry in [
        r#"{"type":"Point","coordinates":[1,2,3]}"#,
        r#"{"type":"MultiPoint","coordinates":[[1,2,3],[4,5,6]]}"#,
        r#"{"type":"LineString","coordinates":[[0,0,1],[2,3,2],[5,1,3]]}"#,
        r#"{"type":"Polygon","coordinates":[[[0,0,1],[0,5,2],[5,5,3],[5,0,4],[0,0,1]]]}"#,
    ] {
        let input = source(geometry);
        let bytes = write_vector_bytes(&input.layers, "z.shp.zip", &Default::default()).unwrap();
        let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        let mut shp = Vec::new();
        zip.by_name("z.shp").unwrap().read_to_end(&mut shp).unwrap();
        let f64_at = |at| f64::from_le_bytes(shp[at..at + 8].try_into().unwrap());
        assert_eq!((f64_at(84), f64_at(92)), (0.0, 0.0), "file header M bounds");
        let length = u32::from_be_bytes(shp[104..108].try_into().unwrap()) as usize * 2;
        let record = &shp[108..108 + length];
        let u32_at = |at| u32::from_le_bytes(record[at..at + 4].try_into().unwrap()) as usize;
        let expected_length = match u32_at(0) {
            11 => 28,
            18 => 56 + 24 * u32_at(36),
            13 | 15 => 60 + 4 * u32_at(36) + 24 * u32_at(40),
            st => panic!("unexpected shape type {st}"),
        };
        // GDAL omits optional M ranges/arrays for Z-only data, rather than
        // emitting no-data sentinels as if they were actual measure bounds.
        assert_eq!(length, expected_length);
    }
}

#[test]
fn real_fields_reject_overflow_and_document_fixed_decimal_rounding() {
    let mut input = source(r#"{"type":"Point","coordinates":[1,2]}"#);
    let field = input.layers[0].schema.fields().iter().position(|f| f.name == "real").unwrap();
    for x in [1e300, -1e300, 1e8, -1e7, f64::MAX] {
        input.layers[0].features[0].attributes[field] = FieldValue::Float(x);
        let err = write_vector_bytes(&input.layers, "p.shp.zip", &Default::default()).unwrap_err();
        assert!(err.to_string().contains("24 columns at 15 decimal places"), "{err}");
    }
    for (x, expected) in [(12345678.5, 12345678.5), (-1234567.5, -1234567.5), (1e-300, 0.0), (-1e-300, -0.0)] {
        input.layers[0].features[0].attributes[field] = FieldValue::Float(x);
        let bytes = write_vector_bytes(&input.layers, "p.shp.zip", &Default::default()).unwrap();
        let back = open_vector_bytes(bytes, "p.zip", &Default::default()).unwrap();
        assert_eq!(back.layers[0].features[0].attributes[field], FieldValue::Float(expected));
    }
}
