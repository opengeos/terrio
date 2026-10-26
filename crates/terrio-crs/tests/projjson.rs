//! PROJJSON subset parsing compared with frozen independent GDAL coordinates.
use terrio_crs::{SpatialRef, Transformer};
fn offline() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        terrio_crs::set_grid_search_paths(Some(Vec::new()));
        terrio_crs::set_network_enabled(Some(false));
    });
}

#[test]
fn gdal_definition_and_coordinate_oracles() {
    offline();
    let reference: serde_json::Value = serde_json::from_str(include_str!("data/projjson_reference.json")).unwrap();
    for case in reference["cases"].as_array().unwrap() {
        let src = SpatialRef::parse(&case["src"].to_string()).unwrap_or_else(|e| panic!("{} source: {e}", case["name"]));
        let dst = SpatialRef::parse(&case["dst"].to_string()).unwrap_or_else(|e| panic!("{} target: {e}", case["name"]));
        let mut transforms = vec![
            ("JSON", Transformer::new(&src, &dst).unwrap()),
            (
                "reopened JSON WKT",
                Transformer::new(&SpatialRef::parse(&src.to_wkt().unwrap()).unwrap(), &SpatialRef::parse(&dst.to_wkt().unwrap()).unwrap())
                    .unwrap(),
            ),
        ];
        for (label, source, target) in [("WKT", "src_wkt", "dst_wkt"), ("PROJ", "src_proj", "dst_proj")] {
            if let (Some(source), Some(target)) = (case[source].as_str(), case[target].as_str()) {
                transforms
                    .push((label, Transformer::new(&SpatialRef::parse(source).unwrap(), &SpatialRef::parse(target).unwrap()).unwrap()));
            }
        }
        for (label, t) in transforms {
            for (p, expected) in case["points"].as_array().unwrap().iter().zip(case["expected"].as_array().unwrap()) {
                let actual = t.transform(p[0].as_f64().unwrap(), p[1].as_f64().unwrap()).unwrap();
                for (i, value) in [actual.0, actual.1].iter().enumerate() {
                    assert!(
                        (value - expected[i].as_f64().unwrap()).abs() <= case["tolerance"][i].as_f64().unwrap(),
                        "{} ({label}) {p}: {actual:?}, expected {expected}",
                        case["name"]
                    );
                }
            }
        }
    }
}

#[test]
fn pinned_british_national_grid_fixture() {
    offline();
    let fixture: serde_json::Value = serde_json::from_str(include_str!("data/projjson_benchmark.json")).unwrap();
    assert_eq!(fixture["environment"]["proj_version"], "9.9.0");
    let case = &fixture["fixture"];
    let t = Transformer::new(
        &SpatialRef::parse(case["src"].as_str().unwrap()).unwrap(),
        &SpatialRef::parse(case["dst"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    let result = t.transform(case["point"][0].as_f64().unwrap(), case["point"][1].as_f64().unwrap()).unwrap();
    for (i, value) in [result.0, result.1].iter().enumerate() {
        assert!((value - case["oracle"]["fwd"][i].as_f64().unwrap()).abs() < 0.001);
    }
}

#[test]
fn unsupported_or_malformed_objects_are_refused() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!("data/projjson_reference.json")).unwrap();
    let valid = fixture["cases"][0]["dst"].clone();
    for broken in
        ["{}", "{", r#"{"type":"VerticalCRS","id":{"authority":"EPSG","code":27700}}"#, r#"{"type":"GeographicCRS","name":"unknown"}"#]
    {
        assert!(SpatialRef::parse(broken).is_err(), "{broken}");
    }
    let mutate = |path: &[&str], value: serde_json::Value| {
        let mut o = valid.clone();
        let mut at = &mut o;
        for key in path {
            at = &mut at[*key];
        }
        *at = value;
        assert!(SpatialRef::parse(&o.to_string()).is_err(), "{o}");
    };
    mutate(&["type"], "EngineeringCRS".into());
    mutate(&["$schema"], "https://proj.org/schemas/v0.99/projjson.schema.json".into());
    mutate(&["deformation_models"], serde_json::json!([]));
    let mut bad = valid.clone();
    bad["conversion"]["method"].as_object_mut().unwrap().remove("id");
    bad["conversion"]["method"]["name"] = "unknown method".into();
    assert!(SpatialRef::parse(&bad.to_string()).is_err());
    let mut bad = valid.clone();
    bad["conversion"]["parameters"][0]["unit"] = "metre".into();
    assert!(SpatialRef::parse(&bad.to_string()).is_err());
    bad = valid.clone();
    bad["coordinate_system"]["axis"][0]["unit"] = serde_json::json!({"type":"LinearUnit", "name":"bad", "conversion_factor":0});
    assert!(SpatialRef::parse(&bad.to_string()).is_err());
    bad = valid.clone();
    bad["coordinate_system"]["axis"].as_array_mut().unwrap().pop();
    assert!(SpatialRef::parse(&bad.to_string()).is_err());
}

#[test]
fn authority_id_does_not_replace_supplied_conversion_parameters() {
    offline();
    let reference: serde_json::Value = serde_json::from_str(include_str!("data/projjson_reference.json")).unwrap();
    let mut definition = reference["cases"][0]["dst"].clone();
    let source = SpatialRef::parse("OGC:CRS84").unwrap();
    let original = SpatialRef::parse(&definition.to_string()).unwrap();
    definition["conversion"]["parameters"][3]["value"] = 400100.0.into();
    let modified = SpatialRef::parse(&definition.to_string()).unwrap();
    let a = Transformer::new(&source, &original).unwrap().transform(-1.5, 52.5).unwrap();
    let b = Transformer::new(&source, &modified).unwrap().transform(-1.5, 52.5).unwrap();
    assert!((b.0 - a.0 - 100.0).abs() < 1e-8);
    assert_eq!(a.1, b.1);
    let reopened = SpatialRef::parse(&modified.to_wkt().unwrap()).unwrap();
    let c = Transformer::new(&source, &reopened).unwrap().transform(-1.5, 52.5).unwrap();
    assert!((c.0 - b.0).abs() < 1e-8, "reopened WKT must retain modified false easting");
    assert!((c.1 - b.1).abs() < 1e-8);
}

#[cfg(not(target_family = "wasm"))]
#[test]
fn reads_projjson_from_a_file() {
    offline();
    let reference: serde_json::Value = serde_json::from_str(include_str!("data/projjson_reference.json")).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crs.projjson");
    std::fs::write(&path, reference["cases"][0]["dst"].to_string()).unwrap();
    let crs = SpatialRef::parse(path.to_str().unwrap()).unwrap();
    assert!(!crs.is_geographic());
    assert!(crs.to_wkt().unwrap().starts_with("PROJCRS["));
}

#[test]
fn bound_targets_must_be_geographic_wgs84_on_its_actual_ellipsoid() {
    offline();
    let reference: serde_json::Value = serde_json::from_str(include_str!("data/projjson_reference.json")).unwrap();
    let bound =
        reference["cases"].as_array().unwrap().iter().find(|c| c["name"] == "Bound geographic translation with IDs").unwrap()["src"]
            .clone();
    let mut nested = bound.clone();
    nested["source_crs"] = bound.clone();
    assert!(SpatialRef::parse(&nested.to_string()).is_err());
    let mut inner = bound.clone();
    inner["source_crs"] = bound["target_crs"].clone();
    for p in inner["transformation"]["parameters"].as_array_mut().unwrap() {
        p["value"] = 0.into();
    }
    nested = bound.clone();
    nested["target_crs"] = inner;
    assert!(SpatialRef::parse(&nested.to_string()).is_err());
    let mut wrong_ellipsoid = bound.clone();
    let target = &mut wrong_ellipsoid["target_crs"];
    let datum = if target.get("datum").is_some() { &mut target["datum"] } else { &mut target["datum_ensemble"] };
    datum["ellipsoid"]["semi_major_axis"] = 6370000.0.into();
    assert!(SpatialRef::parse(&wrong_ellipsoid.to_string()).is_err());
}

#[test]
fn parameter_aliases_cannot_duplicate_or_contradict_identities() {
    let reference: serde_json::Value = serde_json::from_str(include_str!("data/projjson_reference.json")).unwrap();
    let valid = &reference["cases"][0]["dst"];
    let mut duplicate = valid.clone();
    let mut parameter = duplicate["conversion"]["parameters"][0].clone();
    parameter["name"] = "Latitude of origin".into();
    parameter["value"] = 50.into();
    duplicate["conversion"]["parameters"].as_array_mut().unwrap().push(parameter);
    assert!(SpatialRef::parse(&duplicate.to_string()).is_err());
    let mut conflict = valid.clone();
    conflict["conversion"]["parameters"][0]["id"]["code"] = 8802.into();
    assert!(SpatialRef::parse(&conflict.to_string()).is_err());
}

#[test]
fn epsg_method_id_is_found_among_alternate_identifiers() {
    offline();
    let reference: serde_json::Value = serde_json::from_str(include_str!("data/projjson_reference.json")).unwrap();
    let mut definition = reference["cases"][0]["dst"].clone();
    let source = SpatialRef::parse("OGC:CRS84").unwrap();
    let original = SpatialRef::parse(&definition.to_string()).unwrap();
    let method = &mut definition["conversion"]["method"];
    method.as_object_mut().unwrap().remove("id");
    method["name"] = "Local method alias".into();
    method["ids"] = serde_json::json!([{"authority":"LOCAL","code":"TM"},{"authority":"EPSG","code":9807}]);
    let alternate = SpatialRef::parse(&definition.to_string()).unwrap();
    assert_eq!(
        Transformer::new(&source, &original).unwrap().transform(-1.5, 52.5).unwrap(),
        Transformer::new(&source, &alternate).unwrap().transform(-1.5, 52.5).unwrap()
    );
}

#[test]
fn projected_base_uses_wkt2_angular_unit_without_axis_nodes() {
    let reference: serde_json::Value = serde_json::from_str(include_str!("data/projjson_reference.json")).unwrap();
    let wkt = SpatialRef::parse(&reference["cases"][0]["dst"].to_string()).unwrap().to_wkt().unwrap();
    // Only the projected CS declares axes; BASEGEOGCRS declares its angular unit.
    assert_eq!(wkt.matches("AXIS[").count(), 2);
    assert!(wkt.contains("ANGLEUNIT["));
}

#[test]
fn bound_parameters_convert_to_wkt2_implicit_units() {
    offline();
    let reference: serde_json::Value = serde_json::from_str(include_str!("data/projjson_reference.json")).unwrap();
    let case = reference["cases"].as_array().unwrap().iter().find(|c| c["name"] == "Bound projected seven parameters with IDs").unwrap();
    let mut source = case["src"].clone();
    for (i, parameter) in source["transformation"]["parameters"].as_array_mut().unwrap().iter_mut().enumerate() {
        let (kind, factor) = match i {
            0..=2 => ("LinearUnit", 0.3048),
            3..=5 => ("AngularUnit", std::f64::consts::PI / 180.0),
            _ => ("ScaleUnit", 1.0),
        };
        let original = parameter["value"].as_f64().unwrap();
        parameter["value"] = match i {
            0..=2 => original / factor,
            3..=5 => original / 3600.0,
            _ => original * 1e-6,
        }
        .into();
        parameter["unit"] = serde_json::json!({"type":kind, "name":"custom unit", "conversion_factor":factor});
    }
    let src = SpatialRef::from_projjson(&source.to_string()).unwrap();
    let dst = SpatialRef::from_projjson(&case["dst"].to_string()).unwrap();
    let wkt = src.to_wkt().unwrap();
    let operation = wkt.split("ABRIDGEDTRANSFORMATION").nth(1).unwrap();
    assert!(!operation.contains("UNIT["));
    assert!(operation.contains("0.9999795106"));
    let transform = Transformer::new(&src, &dst).unwrap();
    for (point, expected) in case["points"].as_array().unwrap().iter().zip(case["expected"].as_array().unwrap()) {
        let result = transform.transform(point[0].as_f64().unwrap(), point[1].as_f64().unwrap()).unwrap();
        assert!((result.0 - expected[0].as_f64().unwrap()).abs() < 1e-8);
        assert!((result.1 - expected[1].as_f64().unwrap()).abs() < 1e-8);
    }
}

#[test]
fn bare_prime_meridian_and_ellipsoid_lengths_use_degrees_and_metres() {
    offline();
    let reference: serde_json::Value = serde_json::from_str(include_str!("data/projjson_reference.json")).unwrap();
    let case = reference["cases"].as_array().unwrap().iter().find(|c| c["name"] == "Paris prime meridian and grads with IDs").unwrap();
    let mut source = case["src"].clone();
    let datum = &mut source["datum"];
    let longitude = &datum["prime_meridian"]["longitude"];
    let degrees =
        longitude["value"].as_f64().unwrap() * longitude["unit"]["conversion_factor"].as_f64().unwrap() * 180.0 / std::f64::consts::PI;
    datum["prime_meridian"]["longitude"] = degrees.into();
    let a = datum["ellipsoid"]["semi_major_axis"].as_f64().unwrap();
    datum["ellipsoid"]["semi_major_axis"] = serde_json::json!({"value":a, "unit":"metre"});
    let src = SpatialRef::from_projjson(&source.to_string()).unwrap();
    let dst = SpatialRef::from_projjson(&case["dst"].to_string()).unwrap();
    let t = Transformer::new(&src, &dst).unwrap();
    for (p, expected) in case["points"].as_array().unwrap().iter().zip(case["expected"].as_array().unwrap()) {
        let actual = t.transform(p[0].as_f64().unwrap(), p[1].as_f64().unwrap()).unwrap();
        assert!((actual.0 - expected[0].as_f64().unwrap()).abs() <= 1e-8);
        assert!((actual.1 - expected[1].as_f64().unwrap()).abs() <= 1e-8);
    }
}

#[test]
fn numeric_identifier_versions_are_supported() {
    let reference: serde_json::Value = serde_json::from_str(include_str!("data/projjson_reference.json")).unwrap();
    let mut definition = reference["cases"][0]["dst"].clone();
    definition["id"]["version"] = 2015.into();
    definition["conversion"]["method"]["id"]["version"] = 1.25.into();
    let parsed = SpatialRef::from_projjson(&definition.to_string()).unwrap();
    assert!(parsed.to_wkt().unwrap().contains("9807,\"1.25\""));
    for invalid in [serde_json::Value::Null, true.into(), serde_json::json!([])] {
        definition["id"]["version"] = invalid;
        assert!(SpatialRef::from_projjson(&definition.to_string()).is_err());
    }
}

#[test]
fn bound_datum_and_method_identities_cannot_be_masked() {
    let reference: serde_json::Value = serde_json::from_str(include_str!("data/projjson_reference.json")).unwrap();
    let bound =
        reference["cases"].as_array().unwrap().iter().find(|c| c["name"] == "Bound projected seven parameters with IDs").unwrap()["src"]
            .clone();
    for (name, code) in [
        ("Coordinate Frame rotation (geog2D domain)", 9606),
        ("Position Vector transformation (geog2D domain)", 9607),
        ("Geocentric translations (geog2D domain)", 9606),
    ] {
        let mut broken = bound.clone();
        broken["transformation"]["method"]["name"] = name.into();
        broken["transformation"]["method"]["id"]["code"] = code.into();
        assert!(SpatialRef::from_projjson(&broken.to_string()).is_err());
    }
    for datum in [
        serde_json::json!({"name":"Other datum"}),
        serde_json::json!({"name":"World Geodetic System 1984", "id":{"authority":"EPSG", "code":6258}}),
    ] {
        let mut broken = bound.clone();
        let target = &mut broken["target_crs"];
        let mut other = target["datum"].clone();
        other["name"] = datum["name"].clone();
        other.as_object_mut().unwrap().remove("id");
        if let Some(id) = datum.get("id") {
            other["id"] = id.clone();
        }
        target["datum"] = other;
        assert!(SpatialRef::from_projjson(&broken.to_string()).is_err());
    }
}

#[test]
fn bound_target_wgs84_datum_id_supports_a_local_name() {
    offline();
    let reference: serde_json::Value = serde_json::from_str(include_str!("data/projjson_reference.json")).unwrap();
    let case = reference["cases"].as_array().unwrap().iter().find(|c| c["name"] == "Bound geographic translation with IDs").unwrap();
    let mut src = case["src"].clone();
    let target = &mut src["target_crs"];
    target.as_object_mut().unwrap().remove("id");
    target["datum"]["name"] = "Local WGS84 label".into();
    target["datum"]["id"] = serde_json::json!({"authority":"EPSG", "code":6326});
    let t = Transformer::new(
        &SpatialRef::from_projjson(&src.to_string()).unwrap(),
        &SpatialRef::from_projjson(&case["dst"].to_string()).unwrap(),
    )
    .unwrap();
    for (p, expected) in case["points"].as_array().unwrap().iter().zip(case["expected"].as_array().unwrap()) {
        let result = t.transform(p[0].as_f64().unwrap(), p[1].as_f64().unwrap()).unwrap();
        assert!((result.0 - expected[0].as_f64().unwrap()).abs() < 1e-8);
        assert!((result.1 - expected[1].as_f64().unwrap()).abs() < 1e-8);
    }
}

#[test]
fn standalone_geographic_root_id_does_not_replace_its_datum() {
    offline();
    let reference: serde_json::Value = serde_json::from_str(include_str!("data/projjson_reference.json")).unwrap();
    let mut definition = reference["cases"][0]["dst"]["base_crs"].clone();
    definition["type"] = "GeographicCRS".into();
    definition["id"] = serde_json::json!({"authority":"EPSG", "code":4326});
    let src = SpatialRef::from_projjson(&definition.to_string()).unwrap();
    let dst = SpatialRef::from_epsg(4326).unwrap();
    let actual = Transformer::new(&src, &dst).unwrap().transform(-1.5, 52.5).unwrap();
    let expected = Transformer::new(&SpatialRef::from_epsg(4277).unwrap(), &dst).unwrap().transform(-1.5, 52.5).unwrap();
    assert!((actual.0 - expected.0).abs() < 1e-8);
    assert!((actual.1 - expected.1).abs() < 1e-8);
    assert!((actual.0 + 1.5).abs() > 1e-4);
}
