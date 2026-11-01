// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! CityJSON 1.1/2.0 specification regressions; GDAL 3.13.3 has no CityJSON driver.

use serde_json::{Value, json};
use terrio::formats::VectorDriver;
use wbvector::{FieldValue, Geometry};

fn document(kind: &str, boundaries: Value) -> Value {
    json!({
        "type":"CityJSON", "version":"2.0",
        "transform":{"scale":[0.5,2,3],"translate":[100,200,300]},
        "metadata":{"referenceSystem":"https://www.opengis.net/def/crs/EPSG/0/7415"},
        "vertices":[[0,0,0],[0,2,0],[0,2,2],[0,0,2],[0,0.5,0.5],[0,1,0.5],[0,1,1]],
        "CityObjects":{
            "building":{"type":"Building", "children":["part"]},
            "part":{"type":"BuildingPart", "parents":["building"],
                "attributes":{"height":6,"name":"α & β"},
                "geometry":[{"type":kind,"lod":"2.2","boundaries":boundaries}]}
        }
    })
}

fn read(doc: &Value, name: &str) -> terrio::VectorDataset {
    terrio::memory::open_vector_bytes(serde_json::to_vec(doc).unwrap(), name, &Default::default()).unwrap()
}

#[test]
fn flat_surfaces_keep_vertical_xyz_holes_identity_and_crs() {
    for version in ["1.1", "2.0"] {
        let mut doc = document("MultiSurface", json!([[[0, 1, 2, 3], [4, 5, 6]]]));
        doc["version"] = version.into();
        for name in ["model.city.json", "model.json", "model.bin", "model.cityjson"] {
            let ds = read(&doc, name);
            assert_eq!(ds.driver, VectorDriver::CityJSON);
            let layer = &ds.layers[0];
            assert_eq!(layer.features.len(), 2);
            assert_eq!(layer.crs_epsg(), Some(7415));
            let part = layer.features.iter().find(|f| f.attributes[0] == FieldValue::Text("part".into())).unwrap();
            assert_eq!(part.attributes[1], FieldValue::Text("BuildingPart".into()));
            assert_eq!(part.attributes[2], FieldValue::Text("2.2".into()));
            assert_eq!(part.attributes[3], FieldValue::Text("[\"building\"]".into()));
            let Geometry::Polygon { exterior, interiors } = part.geometry.as_ref().unwrap() else { panic!("not a polygon") };
            assert_eq!(exterior.len(), 4);
            assert_eq!(interiors.len(), 1);
            let c = &exterior.coords()[2];
            assert_eq!((c.x, c.y, c.z), (100.0, 204.0, Some(306.0)));
            assert_eq!(interiors[0].coords()[0].z, Some(301.5));
            assert!(layer.features.iter().any(|f| f.geometry.is_none()));
        }
    }
}

#[test]
fn boundary_nesting_flattens_all_solid_shells() {
    let face = json!([[0, 1, 2, 3]]);
    for (kind, boundaries, count) in [
        ("CompositeSurface", json!([face.clone()]), 1),
        ("Solid", json!([[face.clone()], [face.clone()]]), 2),
        ("MultiSolid", json!([[[face.clone()]], [[face.clone()]]]), 2),
        ("CompositeSolid", json!([[[face.clone()]], [[face.clone()]]]), 2),
        ("MultiPoint", json!([0, 1, 2]), 3),
        ("MultiLineString", json!([[0, 1], [2, 3]]), 2),
    ] {
        let ds = read(&document(kind, boundaries), "city.city.json");
        assert_eq!(ds.layers[0].features.iter().filter(|f| f.geometry.is_some()).count(), count, "{kind}");
    }
}

#[test]
fn geometry_template_uses_unquantized_vertices_then_world_anchor() {
    let mut doc = document("GeometryInstance", json!([1]));
    doc["CityObjects"]["part"]["geometry"][0]["template"] = 0.into();
    doc["CityObjects"]["part"]["geometry"][0]["transformationMatrix"] = json!([2, 0, 0, 10, 0, 3, 0, 20, 0, 0, 4, 30, 0, 0, 0, 1]);
    doc["geometry-templates"] = json!({
        "vertices-templates":[[1,2,3]],
        "templates":[{"type":"MultiPoint","lod":"1","boundaries":[0]}]
    });
    let ds = read(&doc, "city.city.json");
    let feature = ds.layers[0].features.iter().find(|f| f.geometry.is_some()).unwrap();
    let Geometry::Point(c) = feature.geometry.as_ref().unwrap() else { panic!("not a point") };
    assert_eq!((c.x, c.y, c.z), (112.0, 230.0, Some(342.0)));
}

#[test]
fn malformed_references_and_unsupported_versions_fail() {
    for doc in [
        document("MultiSurface", json!([[[0, 1, 999]]])),
        document("MultiSurface", json!([[[0, 1, -1]]])),
        document("Solid", json!([[[0, 1, 2]]])),
        document("SomethingElse", json!([])),
        document("MultiPoint", json!([0.5])),
    ] {
        assert!(terrio::memory::open_vector_bytes(serde_json::to_vec(&doc).unwrap(), "x.city.json", &Default::default()).is_err());
    }
    let mut doc = document("MultiPoint", json!([0]));
    doc["version"] = "1.0".into();
    assert!(terrio::memory::open_vector_bytes(serde_json::to_vec(&doc).unwrap(), "x.city.json", &Default::default()).is_err());
}

#[test]
fn solid_semantics_follow_shell_nesting_and_null_classifications() {
    let mut doc = document("Solid", json!([[[[0, 1, 2, 3]], [[0, 1, 2, 3]]], [[[0, 1, 2, 3]]]]));
    doc["CityObjects"]["part"]["geometry"][0]["semantics"] = json!({
        "surfaces":[{"type":"WallSurface","name":"east"},{"type":"RoofSurface"}],
        "values":[[0,null],[1]]
    });
    doc["CityObjects"]["part"]["geometry"].as_array_mut().unwrap().push(json!({"type":"MultiSurface","lod":"3","boundaries":[]}));
    let ds = read(&doc, "city.city.json");
    let features: Vec<_> = ds.layers[0].features.iter().filter(|f| f.geometry.is_some()).collect();
    assert_eq!(features.len(), 3);
    assert_eq!(features[0].attributes[7], FieldValue::Text("{\"type\":\"WallSurface\",\"name\":\"east\"}".into()));
    assert_eq!(features[1].attributes[7], FieldValue::Null);
    assert_eq!(features[2].attributes[7], FieldValue::Text("{\"type\":\"RoofSurface\"}".into()));
    let empty = ds.layers[0].features.iter().find(|f| f.attributes[2] == FieldValue::Text("3".into())).unwrap();
    assert!(empty.geometry.is_none());
    assert_eq!(empty.attributes[7], FieldValue::Null);
    for values in [json!([[99, null], [1]]), json!([[0], [1]]), json!([0, null, 1])] {
        doc["CityObjects"]["part"]["geometry"][0]["semantics"]["values"] = values;
        assert!(terrio::memory::open_vector_bytes(serde_json::to_vec(&doc).unwrap(), "x.city.json", &Default::default()).is_err());
    }
}

#[test]
fn detection_looks_at_root_type_and_read_only_driver_rejects_writes() {
    let bytes = serde_json::to_vec(&document("MultiPoint", json!([0]))).unwrap();
    assert_eq!(terrio::inspection::identify_bytes(&bytes, "x.json").unwrap().driver, "CityJSON");
    assert_eq!(VectorDriver::for_extension("X.CITY.JSON"), Some(VectorDriver::CityJSON));
    assert!(!VectorDriver::CityJSON.info().write);
    let dir = tempfile::tempdir().unwrap();
    let ds = read(&document("MultiPoint", json!([0])), "x.city.json");
    let path = dir.path().join("out.city.json");
    assert!(terrio::write_vector(&ds.layers, path.to_str().unwrap(), &Default::default()).is_err());
    assert!(!path.exists());
    let ordinary = br#"{"type":"FeatureCollection","features":[],"note":{"type":"CityJSON"}}"#;
    assert_eq!(terrio::inspection::identify_bytes(ordinary, "x.json").unwrap().driver, "GeoJSON");
}

#[test]
fn generic_json_with_root_type_after_large_payload_is_not_geojson() {
    let mut doc = document("MultiPoint", json!([0]));
    let mut fields = doc.as_object_mut().unwrap().clone();
    let kind = fields.remove("type").unwrap();
    fields.insert("padding".into(), Value::String("x".repeat(20000)));
    fields.insert("type".into(), kind);
    let bytes = serde_json::to_vec(&fields).unwrap();
    let identified = terrio::formats::identify_bytes("late.city.json", &bytes);
    assert_eq!(identified.by_name, ["CITYJSON"]);
    assert_eq!(identified.by_content, None);
    // serde_json preserves insertion order in this workspace.
    let ds = read(&Value::Object(fields), "late-type.json");
    assert_eq!(ds.driver, VectorDriver::CityJSON);
    assert_eq!(ds.layers[0].features.len(), 2);
    assert_eq!(ds.layers[0].crs_epsg(), Some(7415));
}

#[test]
fn degenerate_rings_preserve_other_faces_semantics_and_geometry_diagnostics() {
    let mut doc = document("MultiSurface", json!([[[0, 0, 0]], [[0, 1, 2, 3], [0, 0, 0]], [[0, 3, 2, 1]]]));
    doc["CityObjects"]["part"]["geometry"][0]["semantics"] = json!({
        "surfaces":[{"type":"GroundSurface"},{"type":"WallSurface"},{"type":"RoofSurface"}],
        "values":[0,1,2]
    });
    for version in ["1.1", "2.0"] {
        doc["version"] = version.into();
        let ds = read(&doc, "degenerate.city.json");
        let features: Vec<_> = ds.layers[0].features.iter().filter(|f| f.geometry.is_some()).collect();
        assert_eq!(features.len(), 2);
        assert_eq!(features[0].attributes[7], FieldValue::Text("{\"type\":\"WallSurface\"}".into()));
        assert_eq!(features[1].attributes[7], FieldValue::Text("{\"type\":\"RoofSurface\"}".into()));
        assert!(features.iter().all(|f| f.attributes[8] == FieldValue::Integer(2) && f.attributes[9] == FieldValue::Integer(1)));
        let Geometry::Polygon { exterior, interiors } = features[0].geometry.as_ref().unwrap() else { panic!() };
        assert_eq!(exterior.len(), 4);
        assert!(interiors.is_empty());
    }
    doc.as_object_mut().unwrap().remove("transform");
    assert!(
        terrio::memory::open_vector_bytes(serde_json::to_vec(&doc).unwrap(), "missing.city.json", &Default::default())
            .unwrap_err()
            .to_string()
            .contains("transform")
    );
}
