use terrio_crs::{SpatialRef, Transformer};

#[test]
fn pinned_quadrilateralized_spherical_cube_fixture() {
    let data: serde_json::Value = serde_json::from_str(include_str!("data/qsc_benchmark.json")).unwrap();
    assert_eq!(data["environment"]["proj_version"], "9.9.0");
    for case in data["fixtures"].as_array().unwrap() {
        assert_eq!(case["id"], "method-qsc");
        let source = SpatialRef::parse(case["src"].as_str().unwrap()).unwrap();
        let target = SpatialRef::parse(case["dst"].as_str().unwrap()).unwrap();
        let (x, y) = (case["point"][0].as_f64().unwrap(), case["point"][1].as_f64().unwrap());
        let actual = Transformer::new(&source, &target).unwrap().transform(x, y).unwrap();
        assert!((actual.0 - case["oracle"]["fwd"][0].as_f64().unwrap()).abs() < 0.001);
        assert!((actual.1 - case["oracle"]["fwd"][1].as_f64().unwrap()).abs() < 0.001);
        let inverse = Transformer::new(&target, &source).unwrap().transform(actual.0, actual.1).unwrap();
        assert!((inverse.0 - case["oracle"]["inv"][0].as_f64().unwrap()).abs() < 1e-8);
        assert!((inverse.1 - case["oracle"]["inv"][1].as_f64().unwrap()).abs() < 1e-8);
        let wkt1 = case["oracle"]["dst_export"]["wkt1_gdal_bound"].as_str().unwrap();
        let written = target.to_wkt().unwrap();
        assert!(written.contains("PROJECTION[\"Quadrilateralized_Spherical_Cube\"]"), "{written}");
        for wkt in [wkt1, written.as_str()] {
            let reopened = SpatialRef::parse(wkt).unwrap();
            let again = Transformer::new(&source, &reopened).unwrap().transform(x, y).unwrap();
            assert!((again.0 - actual.0).abs() < 0.001 && (again.1 - actual.1).abs() < 0.001, "{wkt}");
        }
        // PROJ has no EPSG method for it, so PROJJSON export refuses it.
        assert!(target.to_projjson().is_err());
    }
}

fn angular_error(actual: (f64, f64), expected: &serde_json::Value) -> f64 {
    let lon = expected[0].as_f64().unwrap();
    let lat = expected[1].as_f64().unwrap();
    let dlon = (actual.0 - lon + 180.0).rem_euclid(360.0) - 180.0;
    (dlon * lat.to_radians().cos()).abs().max((actual.1 - lat).abs())
}

#[test]
fn every_face_figure_and_unit_matches_frozen_proj() {
    let data: serde_json::Value = serde_json::from_str(include_str!("data/qsc_reference.json")).unwrap();
    assert_eq!(data["environment"]["proj"], "9.8.1");
    for case in data["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let source = SpatialRef::parse(case["source"].as_str().unwrap()).unwrap();
        let factor = case["metres_per_unit"].as_f64().unwrap();
        // terrio's own WKT1, written from the PROJ string, is checked as the others, without its
        // PROJ4 extension so that the parameters themselves are read.
        let written = SpatialRef::parse(case["target"].as_str().unwrap()).unwrap().to_wkt().unwrap();
        let written = match written.find(",EXTENSION[\"PROJ4\"") {
            Some(i) => format!("{}]", &written[..i]),
            None => written,
        };
        for encoding in ["target", "wkt1", "wkt1_esri", "wkt2", "written"] {
            let text = if encoding == "written" { written.as_str() } else { case[encoding].as_str().unwrap() };
            let target = SpatialRef::parse(text).unwrap_or_else(|e| panic!("{name} {encoding}: {e}"));
            let forward = Transformer::new(&source, &target).unwrap();
            let inverse = Transformer::new(&target, &source).unwrap();
            let antipodal: Vec<usize> =
                case["antipodal_samples"].as_array().unwrap().iter().map(|i| i.as_u64().unwrap() as usize).collect();
            for (i, sample) in case["samples"].as_array().unwrap().iter().enumerate() {
                let point = (sample[0].as_f64().unwrap(), sample[1].as_f64().unwrap());
                let actual = forward.transform(point.0, point.1);
                if sample[2].is_null() {
                    assert!(actual.is_err(), "{name} {encoding}: forward must refuse {point:?}, as PROJ does: {actual:?}");
                    continue;
                }
                let expected = (sample[2][0].as_f64().unwrap(), sample[2][1].as_f64().unwrap());
                let actual = actual.unwrap_or_else(|e| panic!("{name} {encoding} {point:?}: {e}"));
                let error = (actual.0 - expected.0).abs().max((actual.1 - expected.1).abs()) * factor;
                assert!(error <= 0.001, "{name} {encoding} {point:?}: forward error {error} m");
                let actual = inverse.transform(expected.0, expected.1);
                if sample[3].is_null() {
                    assert!(actual.is_err(), "{name} {encoding}: inverse must refuse {expected:?}, as PROJ does: {actual:?}");
                } else {
                    let actual = actual.unwrap_or_else(|e| panic!("{name} {encoding} inverse {point:?}: {e}"));
                    let error = angular_error(actual, &sample[3]);
                    // At the antipode of the face centre, the inverse goes through acos near 180
                    // degrees, so the last bit of the northing (a WKT's rounded US survey foot, or
                    // another libm) moves it by about 1e-6 degrees: there the point is checked by
                    // its projection and within 1e-5 degrees, elsewhere against PROJ's inverse.
                    if antipodal.contains(&i) {
                        let again = forward.transform(actual.0, actual.1).unwrap();
                        let residual = (again.0 - expected.0).abs().max((again.1 - expected.1).abs()) * factor;
                        assert!(residual <= 0.001 && error <= 1e-5, "{name} {encoding} {point:?}: inverse {residual} m, {error} degrees");
                    } else {
                        assert!(error <= 1e-8, "{name} {encoding} {point:?}: inverse error {error} degrees");
                    }
                }
            }
            for sample in case["inverse_samples"].as_array().unwrap() {
                let point = (sample[0][0].as_f64().unwrap(), sample[0][1].as_f64().unwrap());
                let actual = inverse.transform(point.0, point.1);
                if sample[1].is_null() {
                    assert!(actual.is_err(), "{name} {encoding}: inverse domain {point:?}: {actual:?}");
                } else {
                    let actual = actual.unwrap_or_else(|e| panic!("{name} {encoding} inverse domain {point:?}: {e}"));
                    assert!(angular_error(actual, &sample[1]) <= 1e-8, "{name} {encoding}: inverse {point:?}");
                }
            }
        }
    }
}
