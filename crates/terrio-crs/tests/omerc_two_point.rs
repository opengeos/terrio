use terrio_crs::{SpatialRef, Transformer};

#[test]
fn pinned_two_point_oblique_mercator_fixture() {
    let data: serde_json::Value = serde_json::from_str(include_str!("data/omerc_two_point_benchmark.json")).unwrap();
    assert_eq!(data["environment"]["proj_version"], "9.9.0");
    for case in data["fixtures"].as_array().unwrap() {
        assert_eq!(case["id"], "method-omerc-2pt");
        let source = SpatialRef::parse(case["src"].as_str().unwrap()).unwrap();
        let target = SpatialRef::parse(case["dst"].as_str().unwrap()).unwrap();
        let (x, y) = (case["point"][0].as_f64().unwrap(), case["point"][1].as_f64().unwrap());
        let actual = Transformer::new(&source, &target).unwrap().transform(x, y).unwrap();
        assert!((actual.0 - case["oracle"]["fwd"][0].as_f64().unwrap()).abs() < 0.001);
        assert!((actual.1 - case["oracle"]["fwd"][1].as_f64().unwrap()).abs() < 0.001);
        let inverse = Transformer::new(&target, &source).unwrap().transform(actual.0, actual.1).unwrap();
        assert!((inverse.0 - case["oracle"]["inv"][0].as_f64().unwrap()).abs() < 1e-8);
        assert!((inverse.1 - case["oracle"]["inv"][1].as_f64().unwrap()).abs() < 1e-8);
        // GDAL's WKT 1 of the fixture, and the one terrio writes, which names the method as GDAL does.
        let wkt1 = case["oracle"]["dst_export"]["wkt1_gdal_bound"].as_str().unwrap();
        let written = target.to_wkt().unwrap();
        assert!(written.contains("PROJECTION[\"Hotine_Oblique_Mercator_Two_Point_Natural_Origin\"]"), "{written}");
        assert!(!written.contains("EXTENSION"), "{written}");
        for wkt in [wkt1, written.as_str()] {
            let reopened = SpatialRef::parse(wkt).unwrap();
            let again = Transformer::new(&source, &reopened).unwrap().transform(x, y).unwrap();
            assert!((again.0 - actual.0).abs() < 0.001 && (again.1 - actual.1).abs() < 0.001, "{wkt}");
        }
    }
}

fn angular_error(actual: (f64, f64), expected: &serde_json::Value) -> f64 {
    let lon = expected[0].as_f64().unwrap();
    let lat = expected[1].as_f64().unwrap();
    let dlon = (actual.0 - lon + 180.0).rem_euclid(360.0) - 180.0;
    (dlon * lat.to_radians().cos()).abs().max((actual.1 - lat).abs())
}

#[test]
fn figures_lines_units_and_domains_match_frozen_proj() {
    let data: serde_json::Value = serde_json::from_str(include_str!("data/omerc_two_point_reference.json")).unwrap();
    assert_eq!(data["environment"]["proj"], "9.8.1");
    for case in data["cases"].as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let source = SpatialRef::parse(case["source"].as_str().unwrap()).unwrap();
        let target = SpatialRef::parse(case["target"].as_str().unwrap()).unwrap();
        let forward = Transformer::new(&source, &target).unwrap();
        let inverse = Transformer::new(&target, &source).unwrap();
        // The same CRS as GDAL writes it: WKT 2, WKT 1 and ESRI's WKT 1.
        let encodings: Vec<Transformer> = ["wkt2", "wkt1", "wkt1_esri"]
            .iter()
            .map(|k| Transformer::new(&source, &SpatialRef::parse(case[k].as_str().unwrap()).unwrap()).unwrap())
            .collect();
        let factor = case["metres_per_unit"].as_f64().unwrap();
        for sample in case["samples"].as_array().unwrap() {
            let point = (sample[0].as_f64().unwrap(), sample[1].as_f64().unwrap());
            let actual = forward.transform(point.0, point.1);
            if sample[2].is_null() {
                assert!(actual.is_err(), "{name}: {point:?} is outside the projection");
                continue;
            }
            let actual = actual.unwrap_or_else(|e| panic!("{name} {point:?}: {e}"));
            let expected = (sample[2][0].as_f64().unwrap(), sample[2][1].as_f64().unwrap());
            let error = (actual.0 - expected.0).abs().max((actual.1 - expected.1).abs()) * factor;
            assert!(error <= 0.001, "{name} {point:?}: forward error {error} m");
            for (encoding, t) in ["WKT2", "WKT1", "ESRI WKT1"].iter().zip(&encodings) {
                let got = t.transform(point.0, point.1).unwrap();
                let error = (got.0 - expected.0).abs().max((got.1 - expected.1).abs()) * factor;
                assert!(error <= 0.001, "{name} {encoding} {point:?}: forward error {error} m");
            }
            let back = inverse.transform(expected.0, expected.1);
            if sample[3].is_null() {
                assert!(back.is_err(), "{name}: inverse must refuse {expected:?}");
            } else {
                let back = back.unwrap_or_else(|e| panic!("{name} inverse {point:?}: {e}"));
                let error = angular_error(back, &sample[3]);
                assert!(error <= 1e-8, "{name} {point:?}: inverse error {error} degrees");
            }
        }
        for sample in case["inverse_samples"].as_array().unwrap() {
            let point = (sample[0][0].as_f64().unwrap(), sample[0][1].as_f64().unwrap());
            let actual = inverse.transform(point.0, point.1);
            if sample[1].is_null() {
                assert!(actual.is_err(), "{name}: inverse domain {point:?}");
            } else {
                let actual = actual.unwrap_or_else(|e| panic!("{name} inverse domain {point:?}: {e}"));
                assert!(angular_error(actual, &sample[1]) <= 1e-8, "{name}: inverse {point:?}");
            }
        }
    }
}

#[test]
fn two_point_oblique_mercator_rejects_invalid_parameters() {
    let data: serde_json::Value = serde_json::from_str(include_str!("data/omerc_two_point_reference.json")).unwrap();
    // PROJ 9.8.1 refuses each of these (poles, equal latitudes, a first point on the equator, a
    // polar origin, no points at all).
    for case in data["invalid"].as_array().unwrap() {
        assert_eq!(case["proj_refuses"], true);
        let definition = format!("{} +datum=WGS84", case["definition"].as_str().unwrap());
        assert!(SpatialRef::parse(&definition).is_err(), "{definition}");
    }
    // Values that aren't numbers are refused too, rather than read as PROJ reads them (0).
    for definition in [
        "+proj=omerc +lat_1=bad +lon_1=0 +lat_2=40 +lon_2=10",
        "+proj=omerc +lat_1=40 +lon_1=0 +lat_2=45 +lon_2=NaN",
        "+proj=omerc +alpha=bad +lat_1=40 +lon_1=0 +lat_2=45 +lon_2=10",
    ] {
        assert!(SpatialRef::parse(definition).is_err(), "{definition}");
    }
}
