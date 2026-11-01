// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! ESRIJSON regressions, against the behavior of GDAL 3.13.3's ESRIJSON driver.

use terrio::formats::VectorDriver;
use wbvector::{FieldValue, Geometry};

const PAGE: &str = r#"{
  "objectIdFieldName": "OBJECTID",
  "geometryType": "esriGeometryPoint",
  "spatialReference": {"wkid": 4326, "latestWkid": 4326},
  "fields": [
    {"name": "OBJECTID", "type": "esriFieldTypeOID"},
    {"name": "NAME", "type": "esriFieldTypeString", "length": 32}
  ],
  "features": [
    {"attributes": {"OBJECTID": 11, "NAME": "Bogota"}, "geometry": {"x": -74.08, "y": 4.6}},
    {"attributes": {"OBJECTID": 12, "NAME": "Tunja"}, "geometry": {"x": -73.36, "y": 5.53}}
  ],
  "exceededTransferLimit": false
}"#;

fn check(ds: &terrio::VectorDataset) {
    assert_eq!(ds.driver, VectorDriver::ESRIJSON);
    let l = &ds.layers[0];
    assert_eq!(l.features.len(), 2);
    assert_eq!(l.crs_epsg(), Some(4326));
    assert_eq!(l.features[1].fid, 12);
    assert_eq!(l.features[1].attributes[1], FieldValue::Text("Tunja".into()));
    assert!(matches!(l.features[0].geometry, Some(Geometry::Point(ref c)) if (c.x, c.y) == (-74.08, 4.6)));
}

#[test]
fn recognized_by_content_from_files_and_memory() {
    let dir = tempfile::tempdir().unwrap();
    // A query result saved as `.json`, and one downloaded under the URL's last component.
    for name in ["cities.json", "query"] {
        let path = dir.path().join(name);
        std::fs::write(&path, PAGE).unwrap();
        check(&terrio::open_vector(path.to_str().unwrap(), &Default::default()).unwrap());
        check(&terrio::memory::open_vector_bytes(PAGE.as_bytes().to_vec(), name, &Default::default()).unwrap());
    }
    assert_eq!(terrio::inspection::identify_bytes(PAGE.as_bytes(), "cities.json").unwrap().driver, "ESRIJSON");
}

#[test]
fn geojson_stays_geojson_and_esrijson_is_not_written() {
    let geojson = r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"a":1},"geometry":{"type":"Point","coordinates":[1,2]}}]}"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plain.json");
    std::fs::write(&path, geojson).unwrap();
    assert_eq!(terrio::open_vector(path.to_str().unwrap(), &Default::default()).unwrap().driver, VectorDriver::GeoJSON);
    assert_eq!(VectorDriver::for_extension("out.json"), Some(VectorDriver::GeoJSON));
    assert!(!VectorDriver::ESRIJSON.info().write);
    assert_eq!(VectorDriver::from_name("esrijson"), Some(VectorDriver::ESRIJSON));
}

#[test]
fn geojson_with_esri_looking_properties_stays_geojson() {
    let props = r#""properties":{"spatialReference":{"wkid":4326},"fieldAliases":{"a":"a"}}"#;
    let point = r#""geometry":{"type":"Point","coordinates":[1,2]}"#;
    let feature = format!(r#"{{"type":"Feature",{props},{point}}}"#);
    // Sorted keys put the root `type` last, here past the bytes that identify a format.
    let padding = "x".repeat(7000);
    let sorted = format!(
        r#"{{"features":[{{"geometry":{{"coordinates":[1,2],"type":"Point"}},"properties":{{"fieldAliases":"{padding}"}},"type":"Feature"}}],"type":"FeatureCollection"}}"#
    );
    let dir = tempfile::tempdir().unwrap();
    for (doc, name) in [(&feature, "feature.json"), (&feature, "feature.geojson"), (&sorted, "sorted.json")] {
        let path = dir.path().join(name);
        std::fs::write(&path, doc).unwrap();
        let ds = terrio::open_vector(path.to_str().unwrap(), &Default::default()).unwrap();
        assert_eq!(ds.driver, VectorDriver::GeoJSON, "{name}");
        assert_eq!(ds.layers[0].features.len(), 1, "{name}");
        let ds = terrio::memory::open_vector_bytes(doc.clone().into_bytes(), name, &Default::default()).unwrap();
        assert_eq!(ds.driver, VectorDriver::GeoJSON, "{name} in memory");
    }
    // Without an extension, the identification sees only the first bytes; the reader, the whole.
    let path = dir.path().join("sorted");
    std::fs::write(&path, &sorted).unwrap();
    let ds = terrio::open_vector(path.to_str().unwrap(), &Default::default()).unwrap();
    assert_eq!((ds.driver, ds.layers[0].features.len()), (VectorDriver::GeoJSON, 1));
}

#[test]
fn a_geojson_name_is_not_read_as_esrijson() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cities.geojson");
    std::fs::write(&path, PAGE).unwrap();
    // Read by the GeoJSON driver, which may refuse it, not by this one.
    assert!(!terrio::open_vector(path.to_str().unwrap(), &Default::default()).is_ok_and(|ds| ds.driver == VectorDriver::ESRIJSON));
}

#[test]
fn a_service_description_is_an_error_not_an_empty_layer() {
    let doc = r#"{"id":0,"type":"Feature Layer","geometryType":"esriGeometryPoint","fields":[{"name":"A","type":"esriFieldTypeString"}]}"#;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("layer.json");
    std::fs::write(&path, doc).unwrap();
    let e = terrio::open_vector(path.to_str().unwrap(), &Default::default()).unwrap_err().to_string();
    assert!(e.contains("no features"), "{e}");
}

#[test]
fn a_service_error_is_an_error() {
    let bytes = br#"{"error":{"code":498,"message":"Invalid token."}}"#.to_vec();
    let e = terrio::memory::open_vector_bytes(bytes, "query", &Default::default()).unwrap_err().to_string();
    assert!(e.contains("Invalid token."), "{e}");
}

#[test]
fn the_prefix_names_the_driver_for_a_local_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cities.json");
    std::fs::write(&path, PAGE).unwrap();
    check(&terrio::open_vector(&format!("esrijson:{}", path.display()), &Default::default()).unwrap());
}
